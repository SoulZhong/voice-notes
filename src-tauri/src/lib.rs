mod audio;
pub mod pipeline;
pub mod asr;
mod ipc;
pub mod models;
mod session;
mod settings;
mod store;
pub mod diar;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager, State};

use audio::{AudioCapture, Source};
use pipeline::segmenter::Segmenter;
use session::RecordingHandle;

// 锁序约定（必须在任何持锁场景下遵守）：running → generation → session_slot。
// 只有 spawn_session 的加载线程会嵌套持有 running→generation（以及 running→
// generation→session_slot），且只在极短的检查/存储语句内完成；stop_recording
// 每条语句只持有一把锁，从不同时持有两把，因此不存在死锁风险。
//
// generation 协议：stop_recording 和每次新的 spawn_session 调用（start_recording
// 与 resume_recording 均经它发起，二者的守卫逻辑完全相同）都会递增 generation。
// 加载线程在耗时的模型/会话初始化完成后，无论是要存 session（成功路径）还是要
// 清空 running（失败路径 fail()），都必须先确认自己捕获的 my_gen 仍然等于当前
// generation —— 只有仍是"当前代"时，才允许改动共享状态；否则说明该线程是被后续
// stop/start/resume 抢先淘汰的过期加载，直接静默让路，避免已被覆盖或已被终止的
// 会话把自己的（过期的）结果错误地写回全局状态。

/// 活跃时长 = 总 wall 时长 - 已累计暂停 - 当前暂停中时长，再加续录基线 base_ms。
/// checked_sub 兜底：时钟异常倒挂时饱和为 0 而非 panic。
fn active_elapsed_ms(
    total: std::time::Duration,
    paused_accum: std::time::Duration,
    current_pause: Option<std::time::Duration>,
    base_ms: u64,
) -> u64 {
    let active = total
        .checked_sub(paused_accum + current_pause.unwrap_or_default())
        .unwrap_or_default();
    base_ms + active.as_millis() as u64
}

/// 一次活动录制：会话句柄 + 落盘器 + 笔记 id。
struct ActiveSession {
    handle: RecordingHandle,
    writer: Arc<Mutex<store::writer::NoteWriter>>,
    note_id: String,
    /// classify_system 的结果："on" | "denied" | "unavailable"，供重挂载时重建状态。
    system_audio: String,
    /// 说话人区分可用性："on"（声纹模型就绪）| "unavailable"（缺失，降级），供重挂载重建。
    diarization: String,
    /// 计时：会话入槽时刻、续录基线、暂停起点（Some=暂停中）、已累计暂停时长。
    started: std::time::Instant,
    base_ms: u64,
    paused_at: Option<std::time::Instant>,
    paused_accum: std::time::Duration,
}

impl ActiveSession {
    fn elapsed_ms(&self) -> u64 {
        active_elapsed_ms(
            self.started.elapsed(),
            self.paused_accum,
            self.paused_at.map(|p| p.elapsed()),
            self.base_ms,
        )
    }
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
    /// 模型下载互斥位（true = 下载线程在跑）与取消信号。
    download_running: Arc<AtomicBool>,
    download_cancel: Arc<AtomicBool>,
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

/// 声纹库种子导出：app_data_dir/voiceprints.json → 每个"有效"人物（经 resolve 校验，
/// 排除已被合并掉/悬空的引用）的每个信道质心各生成一个 SeedCluster，供本场开录时
/// SpeakerRegistry::with_seeds 优先命中，免得同一人在新会话里从零建簇。
/// 库路径不可用/加载损坏 → 一律降级为空种子（load 本身已对损坏文件降级，这里只再兜
/// app_data_dir 解析失败一层）：声纹库是增值功能，绝不能因为它挡住录制。
fn load_voiceprint_seeds(app: &AppHandle) -> Vec<crate::diar::registry::SeedCluster> {
    let Ok(root) = app.path().app_data_dir() else {
        eprintln!("声纹库路径不可用，本场开录跳过种子注入（不影响录制）");
        return Vec::new();
    };
    let vp = store::VoiceprintStore::new(root).load();
    let mut seeds = Vec::new();
    for (id, person) in &vp.people {
        // 防御性校验：正常情况下 people 里的 key 都是当前有效引用（merge 已把 loser
        // 从 people 移除），这里用 resolve 兜底，防止任何手工损坏数据把一个实际已被
        // 重定向掉的 id 当种子注入。
        if store::VoiceprintStore::resolve(&vp, id) != Some(id.as_str()) {
            continue;
        }
        for centroid in person.centroids.values() {
            seeds.push(crate::diar::registry::SeedCluster {
                person: id.clone(),
                name: person.name.clone(),
                centroid: centroid.vec.clone(),
                count: centroid.count,
            });
        }
    }
    seeds
}

/// 会话未正常存续时的笔记收尾：有内容则 finalize 保全；无内容且是本会话新建的才删空目录；
/// 续录打开的既有笔记(即使零段)绝不删——留 recording 态(诚实显示「已中断」)。
fn abort_or_finalize(writer: &Arc<Mutex<store::writer::NoteWriter>>) {
    let mut w = writer.lock().unwrap();
    if w.has_content() {
        if let Err(e) = w.finalize(chrono::Local::now()) {
            eprintln!("abort_or_finalize: finalize 失败: {e}");
        }
    } else if w.created_this_session() {
        let dir = w.dir().to_path_buf();
        drop(w);
        let _ = std::fs::remove_dir_all(dir);
    }
    // 既有笔记零段:什么都不做,meta 留 recording,内容零损失。
}

/// 归还识别器/嵌入器进常驻槽（None = 没取到、asr 线程 panic 等，不回收）。
/// recognizer_cache 与 embedder_cache 策略完全一致，故共用一个泛型实现。
fn stash_model<T: ?Sized>(cache: &Arc<Mutex<Option<Box<T>>>>, m: Option<Box<T>>) {
    if let Some(m) = m {
        *cache.lock().unwrap() = Some(m);
    }
}

/// RAII 复位守卫:下载线程无论正常结束还是 panic 展开,download_running 都必然
/// 回 false——否则一次 panic 后"下载已在进行中"永久卡死,只能重启应用。
struct ResetOnDrop(Arc<AtomicBool>);
impl Drop for ResetOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn sense_voice_dir() -> PathBuf {
    models::root().join("sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17")
}

fn speaker_model_path() -> PathBuf {
    models::root().join("3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx")
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

/// 会话加载线程要落盘的目标笔记：New = 新建，Resume = 续录既有非活动笔记
/// （已中断或已完成均可）。spawn_session 据此分支 writer 的创建方式。
enum NoteTarget {
    New,
    Resume(String),
}

/// start_recording / resume_recording 共用的会话启动实现：running/generation
/// 守卫（拒绝重复录制、递增 generation）+ 加载线程 spawn。二者的运行守卫与
/// 竞态处理完全一致（同一份代码），仅 target 决定 writer 走 create 还是 resume。
fn spawn_session(
    app: AppHandle,
    running: Arc<Mutex<bool>>,
    generation: Arc<Mutex<u64>>,
    session_slot: Arc<Mutex<Option<ActiveSession>>>,
    recognizer_cache: Arc<Mutex<Option<Box<dyn asr::Recognizer>>>>,
    embedder_cache: Arc<Mutex<Option<Box<dyn diar::SpeakerEmbedder>>>>,
    target: NoteTarget,
) -> Result<(), String> {
    let my_gen = {
        let mut r = running.lock().unwrap();
        if *r {
            return Err("已在录制".into());
        }
        *r = true;
        // 锁序 running → generation：running 锁仍持有时嵌套锁 generation 并
        // 递增，捕获的 my_gen 即本次会话的"代号"，随线程一起移动。
        let mut g = generation.lock().unwrap();
        *g += 1;
        *g
    };

    std::thread::spawn(move || {
        // fail()：加载过程中任何一步失败都会调用。必须先确认 my_gen 仍是当前代
        // 才清空 running / 发出 error —— 否则说明本线程已过期（被后续的
        // stop/start/resume 淘汰），其失败结果不该覆盖更新的会话状态，静默丢弃即可。
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
            let _ = app.emit("status", ipc::StatusEvent { state: msg, system_audio: String::new(), note_id: String::new(), diarization: String::new(), elapsed_ms: 0 });
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
        let vad_path = models::root().join("silero_vad.onnx");
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

        // 2.5) 创建/续录笔记落盘器（此后任何失败路径都要 abort_or_finalize 清理）。
        // New → NoteWriter::create；Resume → NoteWriter::resume（meta 损坏/id 不存在 → Err）。
        let writer = match notes_dir(&app).and_then(|d| match &target {
            NoteTarget::New => store::writer::NoteWriter::create(&d, chrono::Local::now()),
            NoteTarget::Resume(id) => store::writer::NoteWriter::resume(&d, id),
        }) {
            Ok(w) => Arc::new(Mutex::new(w)),
            Err(e) => {
                stash_model(&recognizer_cache, Some(recognizer));
                stash_model(&embedder_cache, embedder);
                let msg = match &target {
                    NoteTarget::New => format!("error: 创建笔记失败: {e}"),
                    NoteTarget::Resume(_) => format!("error: 续录笔记失败: {e}"),
                };
                return fail(&app, &running, &generation, my_gen, msg);
            }
        };
        let note_id = writer.lock().unwrap().note_id().to_string();
        // 续录时间轴偏移：New 路径恒 0；Resume 路径 = 续录前最大 end_ms。
        // on_final 落盘/emit 前 start_ms/end_ms 均 + base_ms（partial 无时间戳，不受影响）。
        let base_ms = writer.lock().unwrap().base_ms();
        // 说话人编号/质心延续 + 库种子注入：快照（续录）优先，库中同 person 不重复注入。
        // 库加载失败降级为无种子，绝不挡录制。
        let seeds = load_voiceprint_seeds(&app);
        let registry = crate::diar::registry::SpeakerRegistry::with_seeds(
            &writer.lock().unwrap().registry_snapshot(),
            &seeds,
        );

        // 3) 起会话。emit 回调带 source 字符串。
        let app_f = app.clone();
        let app_p = app.clone();
        let app_d = app.clone();
        let writer_f = writer.clone();
        let writer_d = writer.clone();
        // 声纹库句柄：闭包前构造一次，供 Snapshot 分支停止时的入库回写。用 Option
        // 包裹而非兜底占位路径——app_data_dir 解析失败时彻底跳过库回写（None），
        // 而不是拿一个空/相对路径去读写，那样反而可能在意外位置产生副作用文件。
        let vp_store_d: Option<store::VoiceprintStore> = match app.path().app_data_dir() {
            Ok(root) => Some(store::VoiceprintStore::new(root)),
            Err(e) => {
                eprintln!("声纹库路径不可用，本场停止时的库回写将被跳过（不影响笔记落盘）: {e}");
                None
            }
        };
        let mut degraded = false;
        let start = session::start_session(
            sources,
            recognizer,
            embedder,
            registry,
            std::time::Duration::from_millis(session::ECHO_HOLD_MS),
            16000,
            16000,
            move |src, text, start_ms, end_ms, spk, rms| {
                let start_ms = start_ms + base_ms;
                let end_ms = end_ms + base_ms;
                // 不丢内容优先：先落盘（失败进待写队列），再通知 UI。
                match writer_f
                    .lock()
                    .unwrap()
                    .append_final(src.as_str(), &text, start_ms, end_ms, spk.as_deref(), rms)
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
                    // 种子命中显名：registry 里已关联库人物（seed 命中或续录带入）的簇，
                    // 把 person_id 同步进本场 speakers 表；本地名为空时用库名兜底（本场
                    // 手动改过名的一律保留，不被库名打回原形）。
                    for s in &infos {
                        let Some(person) = &s.person else { continue };
                        w.set_speaker_person(&s.id, person);
                        let local_name_empty =
                            w.speakers().get(&s.id).map(|m| m.name.is_empty()).unwrap_or(true);
                        if local_name_empty {
                            if let Some(name) = s.name.as_deref().filter(|n| !n.is_empty()) {
                                w.set_speaker_name(&s.id, name);
                            }
                        }
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
                session::DiarEvent::Snapshot(snaps) => {
                    let mut w = writer_d.lock().unwrap();
                    w.store_centroids(&snaps);
                    // 库回写/够料入库（spec:person 簇加权回写；无主簇 ≥10s 入库为未命名人）。
                    // 失败只降级打日志:库是增值层,绝不影响笔记落盘。Snapshot 在 worker
                    // join 前送达,故先于 stop_recording 的 finalize,person_id 随
                    // finalize 落盘。
                    if let Some(store) = &vp_store_d {
                        match store.upsert_from_session(&snaps, &chrono::Local::now().to_rfc3339()) {
                            Ok(enrolled) => {
                                for (cluster_id, person_id) in &enrolled {
                                    w.set_speaker_person(cluster_id, person_id);
                                }
                            }
                            Err(e) => eprintln!("声纹库回写失败(不影响笔记): {e}"),
                        }
                    }
                }
            },
            {
                let app_l = app.clone();
                Some(Box::new(move |rms: f32| {
                    let _ = app_l.emit("level", ipc::LevelEvent { rms });
                }) as Box<dyn Fn(f32) + Send>)
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
                // stop_recording 和更新的 spawn_session 调用（新 start 或 resume）都会
                // 递增 generation；stop_recording 一律先置 running=false 再取 session，
                // 且从不同时持有两把锁，因此无论 stop/新 start(/resume) 发生在加载前、
                // 加载中还是加载后，与本线程的任意交错都是安全的：
                //  - stop 先到（running=false）：这里检测到 running==false，不存
                //    session、不发 "recording"，直接把刚起好的会话原地停掉，避免
                //    孤儿会话。
                //  - 更快的 start/resume #2 先到（running 仍为 true，但 generation 已被
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
                    abort_or_finalize(&writer); // 被 stop/新 start(/resume) 抢先：有内容则收尾保全（flush 失败时留 recording）
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
                    started: std::time::Instant::now(),
                    base_ms,
                    paused_at: None,
                    paused_accum: std::time::Duration::ZERO,
                });
                drop(running_guard);
                let _ = app.emit(
                    "status",
                    ipc::StatusEvent { state: "recording".into(), system_audio, note_id: note_id.clone(), diarization, elapsed_ms: base_ms },
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
fn start_recording(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    if !models::recording_ready() {
        return Err("模型缺失：请先在录制页下载模型".into());
    }
    spawn_session(
        app,
        state.running.clone(),
        state.generation.clone(),
        state.session.clone(),
        state.recognizer_cache.clone(),
        state.embedder_cache.clone(),
        NoteTarget::New,
    )
}

/// 续录一场非活动（已中断或已完成）笔记：运行守卫与 start_recording 完全一致
/// （同一份 spawn_session 实现），仅 target 换成 Resume(note_id)。
#[tauri::command]
fn resume_recording(app: AppHandle, state: State<AppState>, note_id: String) -> Result<(), String> {
    if !models::recording_ready() {
        return Err("模型缺失：请先在录制页下载模型".into());
    }
    spawn_session(
        app,
        state.running.clone(),
        state.generation.clone(),
        state.session.clone(),
        state.recognizer_cache.clone(),
        state.embedder_cache.clone(),
        NoteTarget::Resume(note_id),
    )
}

#[tauri::command]
fn stop_recording(app: AppHandle, state: State<AppState>) {
    // 真停止协议：先置 running=false，再递增 generation（各自 statement-scoped
    // 锁，用完立即释放，从不同时持有两把），最后取 session 并优雅停止（停
    // capture → flush 尾段 → 排干 finals → join）。递增 generation 让任何仍在
    // 加载窗口内的旧线程（无论其 running 检查读到 true 还是 false）都会因
    // generation 不匹配而放弃存 session / 放弃清空 running，从而不会与本次
    // stop 产生孤儿会话或误清 running 的竞态。与 spawn_session 加载线程的
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
        ipc::StatusEvent { state: "stopped".into(), system_audio: String::new(), note_id, diarization: String::new(), elapsed_ms: 0 },
    );
    // 停录补预载：录制中下载完成的模型（预载被活跃跳过）此刻补进空槽；幂等，槽有货即跳。
    preload_models(state.session.clone(), state.recognizer_cache.clone(), state.embedder_cache.clone());
}

/// 供前端重挂载时重建录制状态(Tauri 事件非粘性)。
#[tauri::command]
fn recording_status(state: State<AppState>) -> ipc::StatusEvent {
    match state.session.lock().unwrap().as_ref() {
        Some(s) => ipc::StatusEvent {
            state: if s.paused_at.is_some() { "paused".into() } else { "recording".into() },
            system_audio: s.system_audio.clone(),
            note_id: s.note_id.clone(),
            diarization: s.diarization.clone(),
            elapsed_ms: s.elapsed_ms(),
        },
        None => ipc::StatusEvent {
            state: "idle".into(),
            system_audio: String::new(),
            note_id: String::new(),
            diarization: String::new(),
            elapsed_ms: 0,
        },
    }
}

#[tauri::command]
fn pause_recording(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    let ev = {
        let mut slot = state.session.lock().unwrap();
        let Some(s) = slot.as_mut() else { return Err("没有正在进行的录制".into()) };
        if s.paused_at.is_some() {
            return Ok(()); // 已暂停：幂等
        }
        s.handle.set_paused(true);
        s.paused_at = Some(std::time::Instant::now());
        ipc::StatusEvent {
            state: "paused".into(),
            system_audio: s.system_audio.clone(),
            note_id: s.note_id.clone(),
            diarization: s.diarization.clone(),
            elapsed_ms: s.elapsed_ms(),
        }
    };
    let _ = app.emit("status", ev);
    Ok(())
}

#[tauri::command]
fn unpause_recording(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    let ev = {
        let mut slot = state.session.lock().unwrap();
        let Some(s) = slot.as_mut() else { return Err("没有正在进行的录制".into()) };
        let Some(p) = s.paused_at.take() else { return Ok(()) }; // 未暂停：幂等
        s.paused_accum += p.elapsed();
        s.handle.set_paused(false);
        ipc::StatusEvent {
            state: "recording".into(),
            system_audio: s.system_audio.clone(),
            note_id: s.note_id.clone(),
            diarization: s.diarization.clone(),
            elapsed_ms: s.elapsed_ms(),
        }
    };
    let _ = app.emit("status", ev);
    Ok(())
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

/// 段落编辑共用 guard：活动会话笔记一律拒绝（与 rename_note 同模式）。
fn reject_if_active(state: &State<AppState>, note_id: &str) -> Result<(), String> {
    if state.session.lock().unwrap().as_ref().map(|s| s.note_id == note_id).unwrap_or(false) {
        return Err("录制中的笔记不能编辑".into());
    }
    Ok(())
}

#[tauri::command]
fn edit_segment(
    app: AppHandle,
    state: State<AppState>,
    note_id: String,
    seq: u64,
    expected_text: String,
    new_text: String,
) -> Result<(), String> {
    reject_if_active(&state, &note_id)?;
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    store::NoteStore::new(dir)
        .edit_segment_text(&note_id, seq, &expected_text, &new_text)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_segment(
    app: AppHandle,
    state: State<AppState>,
    note_id: String,
    seq: u64,
    expected_text: String,
) -> Result<(), String> {
    reject_if_active(&state, &note_id)?;
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    store::NoteStore::new(dir)
        .delete_segment(&note_id, seq, &expected_text)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_segment_speaker(
    app: AppHandle,
    state: State<AppState>,
    note_id: String,
    seq: u64,
    expected_text: String,
    speaker_id: String,
) -> Result<String, String> {
    reject_if_active(&state, &note_id)?;
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    store::NoteStore::new(dir)
        .set_segment_speaker(&note_id, seq, &expected_text, &speaker_id)
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

/// 声纹库四命令共用：打开 app_data_dir 根下的 VoiceprintStore（与逐场笔记目录并列，
/// 不是 notes_dir 的子目录）。
fn open_voiceprint_store(app: &AppHandle) -> Result<store::VoiceprintStore, String> {
    app.path()
        .app_data_dir()
        .map(store::VoiceprintStore::new)
        .map_err(|e| e.to_string())
}

/// 声纹库人物列表，供管理页展示。vp.people 本就只含经 redirects 解析后的有效人
/// （merge 已把 loser 移出 people），无需再过一遍 resolve。
#[tauri::command]
fn list_people(app: AppHandle) -> Result<Vec<ipc::PersonSummary>, String> {
    let vp = open_voiceprint_store(&app)?.load();
    Ok(vp
        .people
        .iter()
        .map(|(id, p)| ipc::PersonSummary {
            id: id.clone(),
            name: p.name.clone(),
            total_ms: p.total_ms,
            last_seen: p.last_seen.clone(),
            sources: p.centroids.keys().cloned().collect(),
        })
        .collect())
}

/// 改库里人物的显示名：只影响后续会话的种子姓名与笔记侧只读 join，不涉及本场
/// registry 引用结构，录制中也允许（同 rename_speaker 的"改名不挡录制"哲学）。
#[tauri::command]
fn rename_person(app: AppHandle, id: String, name: String) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("名字不能为空".into());
    }
    open_voiceprint_store(&app)?.rename(&id, name).map_err(|e| e.to_string())
}

/// 录制中拒绝合并/删除：开录时种子已经按当前库结构注入本场 registry，若此刻改
/// 动库的引用关系（合并/删除 person），本场 registry 里的种子锚点和库状态就脱节，
/// "是谁"会变得混乱——比改名危险得多，故禁止，等停止录制后再操作。
#[tauri::command]
fn merge_person(
    app: AppHandle,
    state: State<AppState>,
    loser: String,
    winner: String,
) -> Result<(), String> {
    if state.session.lock().unwrap().is_some() {
        return Err("录制中不能合并说话人".into());
    }
    open_voiceprint_store(&app)?.merge(&loser, &winner).map_err(|e| e.to_string())
}

/// 录制中拒绝：理由同 merge_person。
#[tauri::command]
fn delete_person(app: AppHandle, state: State<AppState>, id: String) -> Result<(), String> {
    if state.session.lock().unwrap().is_some() {
        return Err("录制中不能删除说话人".into());
    }
    open_voiceprint_store(&app)?.delete(&id).map_err(|e| e.to_string())
}

/// 后台预载识别器与声纹嵌入器进常驻槽（幂等：槽已有则跳过）。
/// 锁序：预载是唯一嵌套持两槽者——持 recognizer 槽锁期间嵌套获取 embedder 槽锁，
/// 消除间隙内开录线程 take 到空 embedder 的静默降级（详见原 setup 注释）。
fn preload_models(
    session: Arc<Mutex<Option<ActiveSession>>>,
    cache: Arc<Mutex<Option<Box<dyn asr::Recognizer>>>>,
    embedder_cache: Arc<Mutex<Option<Box<dyn diar::SpeakerEmbedder>>>>,
) {
    std::thread::spawn(move || {
        // 会话活跃则整体跳过：开录已 take() 空槽，此刻加载纯属双载（瞬时 2x 内存），
        // 且停录 stash 会把这份顶掉白载；停录收尾会补调预载。session 锁查完即放
        // （锁序纪律：绝不持 session 锁再拿叶子槽锁）。检查后立刻开录的窗口仍可能
        // 双载——用户级操作间隔，可忽略。
        if session.lock().unwrap().is_some() {
            eprintln!("预载跳过：录制会话进行中，停止后自动补载");
            return;
        }
        let mut slot = cache.lock().unwrap();
        if slot.is_none() {
            match asr::sense_voice::SenseVoiceRecognizer::new(&sense_voice_dir()) {
                Ok(r) => *slot = Some(Box::new(r) as Box<dyn asr::Recognizer>),
                Err(e) => eprintln!("识别器预载失败（将在开录时现场加载）: {e}"),
            }
        }
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
}

#[tauri::command]
fn models_status() -> models::ModelsStatus {
    models::status()
}

#[tauri::command]
fn download_models(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    if state.download_running.swap(true, Ordering::SeqCst) {
        return Err("下载已在进行中".into());
    }
    state.download_cancel.store(false, Ordering::SeqCst);
    let running = state.download_running.clone();
    let cancel = state.download_cancel.clone();
    let session = state.session.clone();
    let recognizer_cache = state.recognizer_cache.clone();
    let embedder_cache = state.embedder_cache.clone();
    std::thread::spawn(move || {
        // guard 而非尾部手动清位:中途任何 panic 也必然复位,不卡死后续下载。
        let guard = ResetOnDrop(running);
        let root = models::root();
        models::download::sweep_tmp(&root);
        let s = app
            .path()
            .app_data_dir()
            .map(|d| settings::load(&d))
            .unwrap_or_default();
        let emit = move |id: &str, phase: &str, received: u64, total: u64, message: &str| {
            let _ = app.emit(
                "model_download",
                ipc::ModelDownloadEvent {
                    artifact: id.into(),
                    phase: phase.into(),
                    received_bytes: received,
                    total_bytes: total,
                    message: message.into(),
                },
            );
        };
        let mut all_ok = true;
        for a in models::ARTIFACTS {
            if models::artifact_present(&root, a) {
                continue;
            }
            let url = models::download::apply_mirror(a.url, s.mirror_enabled, &s.mirror_prefix);
            if let Err(e) = models::download::download_artifact(a, &root, &url, &cancel, &emit) {
                all_ok = false;
                let msg = e.to_string();
                let phase = if msg == "cancelled" { "cancelled" } else { "error" };
                emit(a.id, phase, 0, 0, &msg);
                break;
            }
        }
        drop(guard); // 复位先于 done 事件,保持"收到 done 即可再次下载"的时序
        if all_ok {
            emit("all", "done", 0, 0, "");
            // 补齐后立即预载，无需重启即可开录。
            preload_models(session, recognizer_cache, embedder_cache);
        }
    });
    Ok(())
}

#[tauri::command]
fn cancel_models_download(state: State<AppState>) {
    state.download_cancel.store(true, Ordering::SeqCst);
}

#[tauri::command]
fn get_settings(app: AppHandle) -> Result<settings::Settings, String> {
    app.path().app_data_dir().map(|d| settings::load(&d)).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_settings(app: AppHandle, new_settings: settings::Settings) -> Result<(), String> {
    let d = app.path().app_data_dir().map_err(|e| e.to_string())?;
    settings::save(&d, &new_settings).map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .setup(|app| {
            // 生产模型根目录注入（VN_MODELS / dev 目录优先级更高，见 models::root）。
            if let Ok(dir) = app.path().app_data_dir() {
                let models_dir = dir.join("models");
                let _ = std::fs::create_dir_all(&models_dir);
                models::init_app_root(models_dir);
            }
            models::download::sweep_tmp(&models::root());
            let st = app.state::<AppState>();
            preload_models(st.session.clone(), st.recognizer_cache.clone(), st.embedder_cache.clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_recording,
            resume_recording,
            stop_recording,
            recording_status,
            pause_recording,
            unpause_recording,
            list_notes,
            get_note,
            rename_note,
            delete_note,
            export_note,
            rename_speaker,
            edit_segment,
            delete_segment,
            set_segment_speaker,
            models_status,
            download_models,
            cancel_models_download,
            get_settings,
            set_settings,
            list_people,
            rename_person,
            merge_person,
            delete_person
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::active_elapsed_ms;
    use std::time::Duration;

    #[test]
    fn active_elapsed_subtracts_pauses_and_adds_base() {
        let s = Duration::from_secs;
        assert_eq!(active_elapsed_ms(s(10), s(0), None, 0), 10_000, "无暂停");
        assert_eq!(active_elapsed_ms(s(10), s(3), None, 0), 7_000, "扣已累计暂停");
        assert_eq!(active_elapsed_ms(s(10), s(3), Some(s(2)), 0), 5_000, "再扣当前暂停");
        assert_eq!(active_elapsed_ms(s(10), s(0), None, 60_000), 70_000, "续录加 base_ms");
        assert_eq!(active_elapsed_ms(s(1), s(5), None, 0), 0, "异常倒挂饱和为 0 不 panic");
    }

    #[test]
    fn download_running_resets_even_on_panic() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let flag = Arc::new(AtomicBool::new(true));
        let g = super::ResetOnDrop(flag.clone());
        let h = std::thread::spawn(move || {
            let _g = g;
            panic!("模拟下载线程 panic");
        });
        assert!(h.join().is_err());
        assert!(!flag.load(Ordering::SeqCst), "panic 展开也必须复位标志");
    }
}
