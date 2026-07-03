mod audio;
pub mod pipeline;
pub mod asr;
mod ipc;
mod session;
mod store;
pub mod diar;

use std::sync::{Arc, Mutex};
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager, State};

use audio::{AudioCapture, Source};
use pipeline::segmenter::Segmenter;
use session::RecordingHandle;

// 锁序约定（必须在任何持锁场景下遵守）：running → generation → session_slot。
// 只有 start_recording 的加载线程会嵌套持有 running→generation（以及 running→
// generation→session_slot），且只在极短的检查/存储语句内完成；stop_recording
// 每条语句只持有一把锁，从不同时持有两把，因此不存在死锁风险。
//
// generation 协议：stop_recording 和每次新的 start_recording 都会递增
// generation。加载线程在耗时的模型/会话初始化完成后，无论是要存 session
// （成功路径）还是要清空 running（失败路径 fail()），都必须先确认自己捕获的
// my_gen 仍然等于当前 generation —— 只有仍是"当前代"时，才允许改动共享状态；
// 否则说明该线程是被后续 stop/start 抢先淘汰的过期加载，直接静默让路，避免
// 已被覆盖或已被终止的会话把自己的（过期的）结果错误地写回全局状态。

/// 一次活动录制：会话句柄 + 落盘器 + 笔记 id。
struct ActiveSession {
    handle: RecordingHandle,
    writer: Arc<Mutex<store::writer::NoteWriter>>,
    note_id: String,
    /// classify_system 的结果："on" | "denied" | "unavailable"，供重挂载时重建状态。
    system_audio: String,
    /// 说话人区分可用性："on"（声纹模型就绪）| "unavailable"（缺失，降级），供重挂载重建。
    diarization: String,
}

#[derive(Default)]
struct AppState {
    running: Arc<Mutex<bool>>,
    generation: Arc<Mutex<u64>>,
    session: Arc<Mutex<Option<ActiveSession>>>,
    /// 常驻识别器（启动预载、开录取用、停录归还）。叶子锁：绝不与上面三把锁嵌套持有；
    /// 预载线程持锁加载，使开录 take() 自然阻塞至就绪且永不双重加载。
    recognizer_cache: Arc<Mutex<Option<Box<dyn asr::Recognizer>>>>,
    /// 常驻声纹嵌入器,策略与 recognizer_cache 完全一致(叶子锁、预载持锁)。
    embedder_cache: Arc<Mutex<Option<Box<dyn diar::SpeakerEmbedder>>>>,
}

fn models_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models")
}

/// notes 根目录（不存在则创建）。
fn notes_dir(app: &AppHandle) -> anyhow::Result<PathBuf> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| anyhow::anyhow!("app_data_dir 不可用: {e}"))?
        .join("notes");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// 会话未正常存续时的笔记收尾：有内容则 finalize 保全，无内容则删掉空文件夹。
fn abort_or_finalize(writer: &Arc<Mutex<store::writer::NoteWriter>>) {
    let mut w = writer.lock().unwrap();
    if w.has_content() {
        if let Err(e) = w.finalize(chrono::Local::now()) {
            eprintln!("abort_or_finalize: finalize 失败: {e}");
        }
    } else {
        let dir = w.dir().to_path_buf();
        drop(w);
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// 归还识别器/嵌入器进常驻槽（None = 没取到、asr 线程 panic 等，不回收）。
/// recognizer_cache 与 embedder_cache 策略完全一致，故共用一个泛型实现。
fn stash_model<T: ?Sized>(cache: &Arc<Mutex<Option<Box<T>>>>, m: Option<Box<T>>) {
    if let Some(m) = m {
        *cache.lock().unwrap() = Some(m);
    }
}

fn sense_voice_dir() -> PathBuf {
    models_dir().join("sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17")
}

fn speaker_model_path() -> PathBuf {
    models_dir().join("3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx")
}

fn new_silero(vad_path: &std::path::Path) -> anyhow::Result<Box<dyn Segmenter>> {
    Ok(Box::new(pipeline::silero::SileroSegmenter::new(vad_path)?) as Box<dyn Segmenter>)
}

/// 从 failed 列表把 System 的失败归类为 "denied"（未授权）/ "unavailable"（其它）。
fn classify_system(active: &[Source], failed: &[(Source, String)]) -> String {
    if active.contains(&Source::System) {
        return "on".into();
    }
    match failed.iter().find(|(s, _)| *s == Source::System) {
        Some((_, msg)) if msg.contains("unauthorized") => "denied".into(),
        Some(_) => "unavailable".into(),
        None => "unavailable".into(),
    }
}

#[tauri::command]
fn start_recording(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    let my_gen = {
        let mut r = state.running.lock().unwrap();
        if *r {
            return Err("已在录制".into());
        }
        *r = true;
        // 锁序 running → generation：running 锁仍持有时嵌套锁 generation 并
        // 递增，捕获的 my_gen 即本次会话的"代号"，随线程一起移动。
        let mut g = state.generation.lock().unwrap();
        *g += 1;
        *g
    };
    let running = state.running.clone();
    let generation = state.generation.clone();
    let session_slot = state.session.clone();
    let recognizer_cache = state.recognizer_cache.clone();
    let embedder_cache = state.embedder_cache.clone();

    std::thread::spawn(move || {
        // fail()：加载过程中任何一步失败都会调用。必须先确认 my_gen 仍是当前代
        // 才清空 running / 发出 error —— 否则说明本线程已过期（被后续的
        // stop/start 淘汰），其失败结果不该覆盖更新的会话状态，静默丢弃即可。
        let fail = |app: &AppHandle, running: &Arc<Mutex<bool>>, generation: &Arc<Mutex<u64>>, my_gen: u64, msg: String| {
            let running_guard = running.lock().unwrap();
            let gen_guard = generation.lock().unwrap();
            if *gen_guard != my_gen {
                drop(gen_guard);
                drop(running_guard);
                eprintln!("过期加载线程的失败被忽略: {msg}");
                return;
            }
            drop(gen_guard);
            let mut running_guard = running_guard;
            *running_guard = false;
            drop(running_guard);
            let _ = app.emit("status", ipc::StatusEvent { state: msg, system_audio: String::new(), note_id: String::new(), diarization: String::new() });
        };

        // 1) 取常驻识别器（预载中会在锁上等待）；槽空则现场加载兜底。
        let taken = recognizer_cache.lock().unwrap().take();
        let recognizer = match taken {
            Some(r) => r,
            None => match asr::sense_voice::SenseVoiceRecognizer::new(&sense_voice_dir()) {
                Ok(r) => Box::new(r) as Box<dyn asr::Recognizer>,
                Err(e) => return fail(&app, &running, &generation, my_gen, format!("error: {e}")),
            },
        };
        // 1.5) 取常驻声纹嵌入器；与 recognizer 完全对称的取用节奏（其后），但槽空
        // 时不现场加载——预载失败即降级为无声纹（说话人区分不可用），而不是在
        // 开录路径上额外背一次模型加载的延迟/失败风险。
        let embedder = embedder_cache.lock().unwrap().take();
        // 声纹模型是否就绪 → 决定前端是否显示「说话人区分不可用」降级横幅。
        let diarization = if embedder.is_some() { "on" } else { "unavailable" }.to_string();

        // 2) 构建两路源（各自 VAD）。麦克风必备；系统声音失败则由 start_session 降级。
        let vad_path = models_dir().join("silero_vad.onnx");
        let mic_seg = match new_silero(&vad_path) {
            Ok(s) => s,
            Err(e) => {
                stash_model(&recognizer_cache, Some(recognizer));
                stash_model(&embedder_cache, embedder);
                return fail(&app, &running, &generation, my_gen, format!("error: {e}"));
            }
        };
        // 麦克风源：macOS 用带 Apple AEC 的 VPIO（内部失败自动回退 cpal）；其他平台用 cpal。
        #[cfg(target_os = "macos")]
        let mic: Box<dyn AudioCapture> = Box::new(audio::vpio::VpioMicrophone::new());
        #[cfg(not(target_os = "macos"))]
        let mic: Box<dyn AudioCapture> = Box::new(audio::microphone::Microphone::new());
        let mut sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> =
            vec![(Source::Mic, mic, mic_seg)];

        #[cfg(target_os = "macos")]
        {
            match new_silero(&vad_path) {
                Ok(sys_seg) => sources.push((
                    Source::System,
                    Box::new(audio::system::SystemAudioCapture::new()),
                    sys_seg,
                )),
                Err(e) => {
                    // 系统声音 VAD 构建失败非致命：不发 error 状态（避免闪烁），
                    // 静默跳过该源；classify_system 会因 System 既不在 active 也不在
                    // failed 里而归类为 "unavailable"，UI 仍会显示降级横幅。
                    eprintln!("系统声音 VAD 构建失败，降级为仅麦克风: {e}");
                }
            }
        }

        // 2.5) 创建笔记落盘器（此后任何失败路径都要 abort_or_finalize 清理）。
        let writer = match notes_dir(&app)
            .and_then(|d| store::writer::NoteWriter::create(&d, chrono::Local::now()))
        {
            Ok(w) => Arc::new(Mutex::new(w)),
            Err(e) => {
                stash_model(&recognizer_cache, Some(recognizer));
                stash_model(&embedder_cache, embedder);
                return fail(&app, &running, &generation, my_gen, format!("error: 创建笔记失败: {e}"));
            }
        };
        let note_id = writer.lock().unwrap().note_id().to_string();

        // 3) 起会话。emit 回调带 source 字符串。
        let app_f = app.clone();
        let app_p = app.clone();
        let app_d = app.clone();
        let writer_f = writer.clone();
        let writer_d = writer.clone();
        let mut degraded = false;
        let start = session::start_session(
            sources,
            recognizer,
            embedder,
            16000,
            16000,
            move |src, text, start_ms, end_ms, spk| {
                // 不丢内容优先：先落盘（失败进待写队列），再通知 UI。
                match writer_f
                    .lock()
                    .unwrap()
                    .append_final(src.as_str(), &text, start_ms, end_ms, spk.as_deref())
                {
                    Ok(()) => {
                        if degraded {
                            degraded = false;
                            let _ = app_f.emit("storage", ipc::StorageEvent { state: "ok".into() });
                        }
                    }
                    Err(e) => {
                        eprintln!("append_final 失败（段暂存内存待重试）: {e}");
                        if !degraded {
                            degraded = true;
                            let _ = app_f.emit("storage", ipc::StorageEvent { state: "degraded".into() });
                        }
                    }
                }
                let _ = app_f.emit(
                    "final",
                    ipc::FinalEvent { source: src.as_str().into(), text, start_ms, end_ms, speaker: spk },
                );
            },
            move |src, text| {
                let _ = app_p.emit(
                    "partial",
                    ipc::PartialEvent { source: src.as_str().into(), text },
                );
            },
            move |ev| match ev {
                session::DiarEvent::SpeakersChanged(infos) => {
                    let pairs: Vec<(String, Vec<String>)> = infos
                        .iter()
                        .map(|s| (s.id.clone(), s.sources.iter().cloned().collect()))
                        .collect();
                    let mut w = writer_d.lock().unwrap();
                    if let Err(e) = w.sync_speakers(&pairs) {
                        eprintln!("speakers.json 写入失败: {e}");
                    }
                    let speakers = w
                        .speakers()
                        .iter()
                        .map(|(id, m)| ipc::SpeakerEntry {
                            id: id.clone(),
                            name: m.name.clone(),
                            sources: m.sources.clone(),
                        })
                        .collect();
                    drop(w);
                    let _ = app_d.emit("speakers", ipc::SpeakersEvent { speakers, merged: None });
                }
                session::DiarEvent::Merged { loser, winner } => {
                    let mut w = writer_d.lock().unwrap();
                    // 落盘失败也照发 merged：内存/前端先统一（历史段徽章回写），
                    // 磁盘落后由 storage degraded 告警，finalize 兜底再补。
                    if let Err(e) = w.merge_speaker(&loser, &winner) {
                        eprintln!("说话人合并重写失败({loser}->{winner}): {e}");
                        let _ = app_d.emit("storage", ipc::StorageEvent { state: "degraded".into() });
                    }
                    let speakers = w
                        .speakers()
                        .iter()
                        .map(|(id, m)| ipc::SpeakerEntry {
                            id: id.clone(),
                            name: m.name.clone(),
                            sources: m.sources.clone(),
                        })
                        .collect();
                    drop(w);
                    let _ = app_d.emit(
                        "speakers",
                        ipc::SpeakersEvent {
                            speakers,
                            merged: Some(ipc::MergedPair { loser, winner }),
                        },
                    );
                }
            },
        );

        match start {
            Ok(start) => {
                // Fix A: mic is mandatory — if it failed to start, tear down and surface as error.
                if !start.active.contains(&Source::Mic) {
                    let (r, e) = start.handle.stop(); // 先排干可能已产生的 system finals
                    stash_model(&recognizer_cache, r);
                    stash_model(&embedder_cache, e);
                    abort_or_finalize(&writer);
                    let mic_err = start.failed.iter()
                        .find(|(s, _)| *s == Source::Mic)
                        .map(|(_, msg)| format!("error: 麦克风未能启动: {msg}"))
                        .unwrap_or_else(|| "error: 麦克风未能启动".into());
                    return fail(&app, &running, &generation, my_gen, mic_err);
                }
                // 停/存竞态保护：存 session、running 检查、generation 检查必须在同一把
                // running 锁内完成（锁序 running → generation → session_slot）。
                // stop_recording 和更新的 start_recording 都会递增 generation；
                // stop_recording 一律先置 running=false 再取 session，且从不同时
                // 持有两把锁，因此无论 stop/新 start 发生在加载前、加载中还是加载
                // 后，与本线程的任意交错都是安全的：
                //  - stop 先到（running=false）：这里检测到 running==false，不存
                //    session、不发 "recording"，直接把刚起好的会话原地停掉，避免
                //    孤儿会话。
                //  - 更快的 start #2 先到（running 仍为 true，但 generation 已被
                //    #2 抢先递增）：这里检测到 gen 不等于 my_gen，说明自己是过期
                //    加载（T1），同样不存 session、不发 "recording"，原地停掉，让
                //    路给 #2 稍后存入的 session——修复了"T1 的 handle 被 T2 覆盖
                //    而从未 stop()"的泄漏。
                //  - 都没发生：这里已把 session 存进 session_slot 并发出
                //    "recording"，stop_recording 随后正常取到该 session 并停止。
                let running_guard = running.lock().unwrap();
                let gen_guard = generation.lock().unwrap();
                if !*running_guard || *gen_guard != my_gen {
                    drop(gen_guard);
                    drop(running_guard);
                    let (r, e) = start.handle.stop();
                    stash_model(&recognizer_cache, r);
                    stash_model(&embedder_cache, e);
                    abort_or_finalize(&writer); // 被 stop/新 start 抢先：有内容则收尾保全（flush 失败时留 recording）
                    return;
                }
                drop(gen_guard);
                let system_audio = classify_system(&start.active, &start.failed);
                *session_slot.lock().unwrap() = Some(ActiveSession {
                    handle: start.handle,
                    writer: writer.clone(),
                    note_id: note_id.clone(),
                    system_audio: system_audio.clone(),
                    diarization: diarization.clone(),
                });
                drop(running_guard);
                let _ = app.emit(
                    "status",
                    ipc::StatusEvent { state: "recording".into(), system_audio, note_id: note_id.clone(), diarization },
                );
            }
            Err(se) => {
                stash_model(&recognizer_cache, Some(se.recognizer));
                stash_model(&embedder_cache, se.embedder);
                abort_or_finalize(&writer);
                return fail(&app, &running, &generation, my_gen, format!("error: {}", se.error));
            }
        }
    });

    Ok(())
}

#[tauri::command]
fn stop_recording(app: AppHandle, state: State<AppState>) {
    // 真停止协议：先置 running=false，再递增 generation（各自 statement-scoped
    // 锁，用完立即释放，从不同时持有两把），最后取 session 并优雅停止（停
    // capture → flush 尾段 → 排干 finals → join）。递增 generation 让任何仍在
    // 加载窗口内的旧线程（无论其 running 检查读到 true 还是 false）都会因
    // generation 不匹配而放弃存 session / 放弃清空 running，从而不会与本次
    // stop 产生孤儿会话或误清 running 的竞态。与 start_recording 加载线程的
    // 锁序一致（running → generation → session_slot），且本函数从不同时持有
    // 两把锁，所以与加载线程的任意交错都不会死锁。
    { *state.running.lock().unwrap() = false; }
    { *state.generation.lock().unwrap() += 1; }
    let sess = state.session.lock().unwrap().take();
    let mut note_id = String::new();
    if let Some(s) = sess {
        let (returned, embedder) = s.handle.stop(); // 排干 finals：所有 append 在此完成
        stash_model(&state.recognizer_cache, returned);
        stash_model(&state.embedder_cache, embedder);
        note_id = s.note_id;
        if let Err(e) = s.writer.lock().unwrap().finalize(chrono::Local::now()) {
            eprintln!("stop_recording: finalize 失败: {e}");
            let _ = app.emit("storage", ipc::StorageEvent { state: "degraded".into() });
        }
    }
    let _ = app.emit(
        "status",
        ipc::StatusEvent { state: "stopped".into(), system_audio: String::new(), note_id, diarization: String::new() },
    );
}

/// 供前端重挂载时重建录制状态(Tauri 事件非粘性)。
#[tauri::command]
fn recording_status(state: State<AppState>) -> ipc::StatusEvent {
    match state.session.lock().unwrap().as_ref() {
        Some(s) => ipc::StatusEvent {
            state: "recording".into(),
            system_audio: s.system_audio.clone(),
            note_id: s.note_id.clone(),
            diarization: s.diarization.clone(),
        },
        None => ipc::StatusEvent {
            state: "idle".into(),
            system_audio: String::new(),
            note_id: String::new(),
            diarization: String::new(),
        },
    }
}

#[tauri::command]
fn list_notes(app: AppHandle, state: State<AppState>) -> Result<Vec<store::NoteSummary>, String> {
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    let mut list = store::NoteStore::new(dir).list();
    // 正在录制的笔记在磁盘上也是 recording 态；用活动会话区分「录制中」与「已中断」。
    if let Some(active_id) = state.session.lock().unwrap().as_ref().map(|s| s.note_id.clone()) {
        for n in &mut list {
            if n.id == active_id {
                n.state = "active".into();
            }
        }
    }
    Ok(list)
}

#[tauri::command]
fn get_note(app: AppHandle, id: String) -> Result<store::Note, String> {
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    store::NoteStore::new(dir).load(&id).map_err(|e| e.to_string())
}

#[tauri::command]
fn rename_note(app: AppHandle, state: State<AppState>, id: String, title: String) -> Result<(), String> {
    if state.session.lock().unwrap().as_ref().map(|s| s.note_id == id).unwrap_or(false) {
        return Err("录制中的笔记不能改名".into());
    }
    let title = title.trim();
    if title.is_empty() {
        return Err("标题不能为空".into());
    }
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    store::NoteStore::new(dir).rename(&id, title).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_note(app: AppHandle, state: State<AppState>, id: String) -> Result<(), String> {
    if state.session.lock().unwrap().as_ref().map(|s| s.note_id == id).unwrap_or(false) {
        return Err("录制中的笔记不能删除".into());
    }
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    store::NoteStore::new(dir).delete(&id).map_err(|e| e.to_string())
}

/// 改说话人显示名：录制中的笔记也允许改。
/// 活动会话走 writer 单写者路径——在 writer 锁内改内存表并 persist_speakers 原子落盘，
/// 与 worker 线程的 sync_speakers 共用同一把锁、同一原子写，杜绝互相覆盖窗口（不再经
/// NoteStore 直写）；非活动笔记才走 NoteStore::rename_speaker 直写磁盘。
#[tauri::command]
fn rename_speaker(
    app: AppHandle,
    state: State<AppState>,
    note_id: String,
    speaker_id: String,
    name: String,
) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("名字不能为空".into());
    }
    // 活动会话：writer 锁内改名 + 落盘 + 广播，单写者不与 sync_speakers 竞争。
    if let Some(s) = state.session.lock().unwrap().as_ref() {
        if s.note_id == note_id {
            let mut w = s.writer.lock().unwrap();
            w.set_speaker_name(&speaker_id, name);
            let persisted = w.persist_speakers();
            let speakers = w
                .speakers()
                .iter()
                .map(|(id, m)| ipc::SpeakerEntry {
                    id: id.clone(),
                    name: m.name.clone(),
                    sources: m.sources.clone(),
                })
                .collect();
            drop(w);
            persisted.map_err(|e| format!("说话人改名落盘失败: {e}"))?;
            let _ = app.emit("speakers", ipc::SpeakersEvent { speakers, merged: None });
            return Ok(());
        }
    }
    // 非活动笔记：直写磁盘。
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    store::NoteStore::new(dir)
        .rename_speaker(&note_id, &speaker_id, name)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn export_note(app: AppHandle, id: String, format: String) -> Result<String, String> {
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    store::NoteStore::new(dir)
        .export(&id, &format)
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .setup(|app| {
            // 启动预载识别器：持锁加载，开录若赶上预载会在锁上等待至就绪。
            let cache = app.state::<AppState>().recognizer_cache.clone();
            let embedder_cache = app.state::<AppState>().embedder_cache.clone();
            std::thread::spawn(move || {
                let mut slot = cache.lock().unwrap();
                if slot.is_none() {
                    match asr::sense_voice::SenseVoiceRecognizer::new(&sense_voice_dir()) {
                        Ok(r) => *slot = Some(Box::new(r) as Box<dyn asr::Recognizer>),
                        Err(e) => eprintln!("识别器预载失败（将在开录时现场加载）: {e}"),
                    }
                }
                // 锁序：预载是唯一嵌套持两槽者——在仍持 recognizer 槽锁期间嵌套获取
                // embedder 槽锁，消除释放 recognizer 锁到锁定 embedder 锁之间的间隙
                // （否则开录线程可在间隙 take 到尚空的 embedder，静默无声纹）。开录
                // 线程从不同时持两槽（先 take recognizer 后 take embedder，各自即刻释放），
                // 故无死锁。
                let mut eslot = embedder_cache.lock().unwrap();
                if eslot.is_none() {
                    match diar::SherpaEmbedder::new(&speaker_model_path()) {
                        Ok(e) => *eslot = Some(Box::new(e) as Box<dyn diar::SpeakerEmbedder>),
                        Err(e) => eprintln!("声纹模型预载失败（说话人区分将不可用）: {e}"),
                    }
                }
                drop(eslot);
                drop(slot);
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_recording,
            stop_recording,
            recording_status,
            list_notes,
            get_note,
            rename_note,
            delete_note,
            export_note,
            rename_speaker
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
