mod audio;
mod logging;
pub mod pipeline;
pub mod asr;
mod ipc;
pub mod models;
mod session;
mod settings;
mod shortcuts;
mod store;
mod tray;
pub mod diar;
mod refine;

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
    /// 音频写盘线程句柄:stop 时在 handle.stop()(join 分段 worker → sink drop →
    /// 通道关闭)之后 join,保证 finalize 前 WAV 头已收尾。其余提前放弃路径不 join,
    /// 线程随通道关闭自行退出(Drop 收尾)。
    audio_joins: Vec<std::thread::JoinHandle<()>>,
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
    /// 全局串行转码队列。自带独立叶子锁：绝不在持有 running/generation/session_slot
    /// 任一把锁时调它的阻塞方法（cancel_and_wait 等 in-flight）。停录入队、启动回溯
    /// 扫描入队、续录前 cancel_and_wait 都从队列这一把锁出入，与上述锁序完全解耦。
    transcode: Arc<store::transcode::TranscodeQueue>,
    /// 正在精修的 note id 集，防重入：停止钩子与手动重跑（refine_note）共用同一把锁；
    /// 自带独立叶子锁，与上面几把锁完全解耦（spawn_refine 只在极短语句内持有）。
    refining: Arc<Mutex<std::collections::HashSet<String>>>,
}

// 手工 Default（而非 derive）：TranscodeQueue::new() 返回 Arc<Self>，且这样每个字段
// 怎么来的一目了然。
impl Default for AppState {
    fn default() -> Self {
        Self {
            running: Arc::new(Mutex::new(false)),
            generation: Arc::new(Mutex::new(0)),
            session: Arc::new(Mutex::new(None)),
            recognizer_cache: Arc::new(Mutex::new(None)),
            embedder_cache: Arc::new(Mutex::new(None)),
            download_running: Arc::new(AtomicBool::new(false)),
            download_cancel: Arc::new(AtomicBool::new(false)),
            transcode: store::transcode::TranscodeQueue::new(),
            refining: Arc::new(Mutex::new(std::collections::HashSet::new())),
        }
    }
}

/// 数据根目录：app_data_dir 读 settings.json（自举指针，永远在 app_data_dir，不随
/// data_dir 漂移）→ resolve_data_root 得到用户配置的落盘根，未配置则回落 app_data_dir。
/// 笔记/声纹等所有内容都挂这个根；settings 读写命令仍走 app_data_dir。
fn data_root(app: &AppHandle) -> anyhow::Result<PathBuf> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|e| anyhow::anyhow!("app_data_dir 不可用: {e}"))?;
    let s = settings::load(&app_data);
    Ok(settings::resolve_data_root(&app_data, &s))
}

/// notes 根目录（不存在则创建），挂在 data_root 下。
fn notes_dir(app: &AppHandle) -> anyhow::Result<PathBuf> {
    let dir = data_root(app)?.join("notes");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// 启动回溯扫描的入队判定（抽成纯函数便于单测）：meta.json 可解析为 NoteMeta 且
/// state=="complete"（已中断的 recording 态留给续录，不转码）且目录下存在 >44 字节的
/// `*.wav`（44 字节是纯 WAV 头，>44 才有真实样本可压）。任一不满足即不入队。
fn should_enqueue_transcode(note_dir: &std::path::Path) -> bool {
    let Ok(meta_str) = std::fs::read_to_string(note_dir.join("meta.json")) else {
        return false;
    };
    let Ok(meta) = serde_json::from_str::<store::NoteMeta>(&meta_str) else {
        return false; // 损坏 meta 跳过，不入队
    };
    if meta.state != "complete" {
        return false;
    }
    let Ok(rd) = std::fs::read_dir(note_dir) else {
        return false;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("wav") {
            if let Ok(m) = std::fs::metadata(&path) {
                if m.len() > 44 {
                    return true;
                }
            }
        }
    }
    false
}

/// 声纹库种子导出：app_data_dir/voiceprints.json → 每个"有效"人物（经 resolve 校验，
/// 排除已被合并掉/悬空的引用）的每个信道质心各生成一个 SeedCluster，供本场开录时
/// SpeakerRegistry::with_seeds 优先命中，免得同一人在新会话里从零建簇。
/// 库路径不可用/加载损坏 → 一律降级为空种子（load 本身已对损坏文件降级，这里只再兜
/// app_data_dir 解析失败一层）：声纹库是增值功能，绝不能因为它挡住录制。
fn load_voiceprint_seeds(app: &AppHandle) -> Vec<crate::diar::registry::SeedCluster> {
    let Ok(root) = data_root(app) else {
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

/// LLM 精修配置是否齐备（四项均非空/非关闭才可跑）：抽成纯函数供 spawn_refine 判定与单测，
/// 避免把「要不要发起网络请求」这条判断逻辑埋进整个后台线程闭包里难以单独验证。
fn refine_llm_ready(s: &settings::Settings) -> bool {
    s.refine_enabled
        && !s.refine_base_url.is_empty()
        && !s.refine_model.is_empty()
        && !s.refine_api_key.is_empty()
}

/// resume_recording 是否应因该笔记正在精修而拒绝：抽成纯函数供命令层判定与单测。
///
/// 背景（F1 音频丢失窗口）：精修完成后才把该目录 `transcode.enqueue`（见 spawn_refine），
/// 而 `resume_recording` 会先 `transcode.cancel_and_wait` 再开始向 mic.wav 追加写。若精修
/// 仍在跑（尚未 enqueue），cancel_and_wait 看到空队列直接放行，续录写入照常开始；随后精修
/// 收尾时才 enqueue，转码 worker 对着"活跃在追加"的 WAV 编码并 `remove_file`，续录段音频
/// 永久丢失。故续录入口必须挡在最前面：该 id 仍在 `state.refining` 集中就直接拒绝。
fn resume_blocked_by_refining(refining: &std::collections::HashSet<String>, id: &str) -> bool {
    refining.contains(id)
}

/// 会后精修：后台线程跑 filter+recluster（读 WAV）→ 视 `enqueue_transcode_after_local`
/// 移交转码 → 视配置可选 LLM。全程 catch_unwind，任何一步失败/panic 只留日志与
/// "failed" 事件，绝不影响已落盘的 segments/speakers——refined.json 是纯增值产物。
///
/// 转码入队保证：`enqueue_transcode_after_local` 为真时，`state.transcode.enqueue`
/// 在本函数返回前必然被调用至少一次（多次调用因 TranscodeQueue::enqueue 按目录去重
/// 而完全无害）。正常路径下 run_local 本身不返回 Result（内部已把嵌入/重聚类失败降级
/// 编码进 stages 里），因此唯一可能"来不及入队就退出"的窗口，是入队那行代码之前的
/// dir/note 解析（notes_dir 不可用、NoteStore::load 失败）或任意一步 panic；这些情形
/// 由 catch_unwind 之后的兜底分支统一补一次 enqueue（用 `enqueued` 标记避免语义混淆，
/// 但即使漏标也不会重复造成问题，因为 enqueue 本身幂等）。
fn spawn_refine(app: tauri::AppHandle, note_id: String, enqueue_transcode_after_local: bool) {
    let state: tauri::State<AppState> = app.state();
    {
        let mut set = state.refining.lock().unwrap();
        if !set.insert(note_id.clone()) {
            // 已在精修中：命令层（refine_note）已提前拒绝重复手动触发，这里只是双保险，
            // 静默跳过——原本那次精修仍会走完自己的转码承诺。
            return;
        }
    }
    let refining = state.refining.clone();
    let transcode = state.transcode.clone();
    let session = state.session.clone();
    std::thread::spawn(move || {
        // F1 修复(b):若此刻活跃会话正是本 note_id,说明 resume 已经抢在精修完成前重开
        // 录制、正在向 mic.wav 追加写——此刻 enqueue 会让转码 worker 编码+删除一份正在
        // 被写入的 WAV,续录段音频永久丢失。锁只取 note_id 立即释放,不跨 enqueue 调用
        // 持有。跳过不等于丢转码:续录自身在其最终停止时会重新走一遍精修+转码移交。
        let is_resumed_by_active_session = |note_id: &str| -> bool {
            session.lock().unwrap().as_ref().map(|s| s.note_id == note_id).unwrap_or(false)
        };
        let emit = |stage: &str, st: &str| {
            let _ = app.emit(
                "refine",
                ipc::RefineEvent { note_id: note_id.clone(), stage: stage.into(), state: st.into() },
            );
        };
        let enqueued = std::cell::Cell::new(false);
        let result: std::thread::Result<anyhow::Result<()>> =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                emit("all", "running");
                let root = notes_dir(&app)?;
                let dir = root.join(&note_id);
                // 与 get_note 同款只读加载：全部 segments（已按 get_note 语义过滤空白 +
                // 排序）+ speakers 表。
                let note = store::NoteStore::new(root).load(&note_id)?;
                let mut embedder = match diar::SherpaEmbedder::new(&speaker_model_path()) {
                    Ok(e) => Some(e),
                    Err(e) => {
                        eprintln!("refine: 声纹模型不可用，跳过重聚类: {e}");
                        None
                    }
                };
                let seeds = load_voiceprint_seeds(&app);
                let mut doc = refine::run_local(
                    &dir,
                    &note.segments,
                    &note.speakers,
                    embedder.as_mut().map(|e| e as &mut dyn diar::SpeakerEmbedder),
                    &seeds,
                    &chrono::Local::now().to_rfc3339(),
                );
                emit("filter", &doc.stages.filter);
                emit("recluster", &doc.stages.recluster);
                if enqueue_transcode_after_local {
                    if is_resumed_by_active_session(&note_id) {
                        eprintln!(
                            "refine({note_id}): 续录已在本笔记上重新开始,跳过本轮转码入队(续录停止时会再次入队)。"
                        );
                    } else {
                        // 本地两段已读完 WAV：此刻移交转码最早也最安全（不再有人读原始 WAV）。
                        transcode.enqueue(dir.clone());
                    }
                    enqueued.set(true);
                }
                let s = match app.path().app_data_dir() {
                    Ok(d) => settings::load(&d),
                    Err(_) => settings::Settings::default(),
                };
                if refine_llm_ready(&s) {
                    emit("llm", "running");
                    let cfg = refine::llm::LlmConfig {
                        base_url: s.refine_base_url.clone(),
                        model: s.refine_model.clone(),
                        api_key: s.refine_api_key.clone(),
                    };
                    if let Err(e) = refine::run_llm(&dir, &mut doc, &cfg, &s.refine_model) {
                        eprintln!("refine: llm 落盘失败: {e}");
                    }
                }
                emit("llm", &doc.stages.llm);
                anyhow::Ok(())
            }));
        match &result {
            Ok(Ok(())) => emit("all", "done"),
            Ok(Err(e)) => {
                eprintln!("refine({note_id}): 管线失败: {e}");
                emit("all", "failed");
            }
            Err(_) => {
                eprintln!("refine({note_id}): 管线 panic");
                emit("all", "failed");
            }
        }
        // 兜底：前置失败/panic 导致上面从未走到 enqueue 那一行时补一次；enqueue 幂等，
        // 重复调用（包括与正常路径里已入队的那次重复）绝对安全。
        if enqueue_transcode_after_local && !enqueued.get() {
            if is_resumed_by_active_session(&note_id) {
                eprintln!(
                    "refine({note_id}): 续录已在本笔记上重新开始,跳过兜底转码入队(续录停止时会再次入队)。"
                );
            } else {
                match notes_dir(&app) {
                    Ok(root) => transcode.enqueue(root.join(&note_id)),
                    Err(e) => {
                        eprintln!("refine({note_id}): notes_dir 不可用，转码补偿也失败，需人工核实 WAV 是否已压缩: {e}");
                    }
                }
            }
        }
        refining.lock().unwrap().remove(&note_id);
    });
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

fn whisper_dir() -> PathBuf {
    models::root().join("sherpa-onnx-whisper-base")
}

fn paraformer_dir() -> PathBuf {
    models::root().join(models::PF_DIR)
}

/// 识别器唯一实例化点：按选型造对应识别器，装进 trait 对象。preload 与 spawn_session
/// 槽空兜底都经此，杜绝两处各写一份 new 而漏掉某一选型。
fn new_recognizer(asr_model: &str) -> anyhow::Result<Box<dyn asr::Recognizer>> {
    if asr_model == settings::ASR_WHISPER {
        Ok(Box::new(asr::whisper::WhisperRecognizer::new(&whisper_dir())?) as Box<dyn asr::Recognizer>)
    } else if asr_model == settings::ASR_PARAFORMER {
        Ok(Box::new(asr::paraformer::ParaformerRecognizer::new(&paraformer_dir())?) as Box<dyn asr::Recognizer>)
    } else {
        Ok(Box::new(asr::sense_voice::SenseVoiceRecognizer::new(&sense_voice_dir())?) as Box<dyn asr::Recognizer>)
    }
}

/// 当前 ASR 选型：app_data_dir → settings.json 读 asr_model；app_data_dir 不可用时
/// 默认 sense_voice（与 settings 默认一致），绝不因读设置失败挡住录制/预载。
/// pub(crate)：托盘 build_menu 也要按当前选型算 recording_ready 决定 toggle 项禁用。
pub(crate) fn current_asr(app: &AppHandle) -> String {
    match app.path().app_data_dir() {
        Ok(d) => settings::load(&d).asr_model,
        Err(_) => settings::ASR_SENSE_VOICE.into(),
    }
}

/// 默认下载集：遍历 ARTIFACTS 保序收集「当前选型录制必需」或声纹（speaker，增值但默认装）。
/// 与旧行为等价：vad + 选中 ASR + speaker。download_models 的 None 分支用它。
fn default_download_ids(asr_model: &str) -> Vec<&'static str> {
    models::ARTIFACTS
        .iter()
        .filter(|a| models::required_now(a.id, asr_model) || a.id == "speaker")
        .map(|a| a.id)
        .collect()
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

/// 本场录制的「必备源集合」：这些源必须全部出现在 start.active 里，任一缺失即整场
/// 拆除报错（不做静默降级）。为什么随 system_only 变：
///  - 默认场景 → [Mic]：会议里本机说话人主要走麦克风，mic 是刚需；系统声音则可降级
///    （拿不到就只录 mic），故 System 不在必备集合。
///  - 仅系统声音场景 → [System]：这是「纯外放」用法（会议软件把远端声音从扬声器放出、
///    本机不对着 mic 说话）。此时刻意不建 mic 源——即便有 AEC，mic 路仍会漏进扬声器
///    回声的残渣污染转写；关掉 mic 从根上消除这条污染路径，System 随之升格为该场景下
///    唯一且必备的源。纯函数（单测覆盖），供 spawn_session 的源构建与 Fix A 守卫共用。
fn required_sources(system_only: bool) -> Vec<Source> {
    if system_only {
        vec![Source::System]
    } else {
        vec![Source::Mic]
    }
}

/// 源的中文显示名，仅用于「XX未能启动」失败文案（沿用既有文案风格）。
fn source_display(s: Source) -> &'static str {
    match s {
        Source::Mic => "麦克风",
        Source::System => "系统声音",
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
    transcode: Arc<store::transcode::TranscodeQueue>,
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
            // 加载失败且确属当前代:running 已复位，托盘同步回 idle（过期线程在上面已提前
            // return，走不到这里，不会误把托盘打回 idle）。托盘不存在则内部静默跳过。
            tray::set_recording(app, false);
            let _ = app.emit("status", ipc::StatusEvent { state: msg, system_audio: String::new(), note_id: String::new(), diarization: String::new(), elapsed_ms: 0 });
        };

        // 1) 取常驻识别器（预载中会在锁上等待）；槽空则现场加载兜底。
        let taken = recognizer_cache.lock().unwrap().take();
        let recognizer = match taken {
            Some(r) => r,
            None => match new_recognizer(&current_asr(&app)) {
                Ok(r) => r,
                Err(e) => return fail(&app, &running, &generation, my_gen, format!("error: {e}")),
            },
        };
        // 1.5) 取常驻声纹嵌入器；与 recognizer 完全对称的取用节奏（其后），但槽空
        // 时不现场加载——预载失败即降级为无声纹（说话人区分不可用），而不是在
        // 开录路径上额外背一次模型加载的延迟/失败风险。
        let embedder = embedder_cache.lock().unwrap().take();
        // 声纹模型是否就绪 → 决定前端是否显示「说话人区分不可用」降级横幅。
        let diarization = if embedder.is_some() { "on" } else { "unavailable" }.to_string();

        // 一次性读设置：record_system_only / keep_audio / language_filter /
        // keep_output_volume 同源同快照（避免多次 load 读到并发写入的不同代）。
        // app_data_dir 不可用时全部保守回落 Settings::default（仅系统声=否、保留音频=是、
        // 语言过滤=开、保持外放音量=否），绝不因读设置失败改变现状行为。
        // language_filter 在下方 start_session 处消费。
        let (record_system_only, keep_audio, language_filter, keep_output_volume) = app
            .path()
            .app_data_dir()
            .map(|d| {
                let s = settings::load(&d);
                (s.record_system_only, s.keep_audio, s.language_filter, s.keep_output_volume)
            })
            .unwrap_or((false, true, true, false));

        // 2) 构建源（各自 VAD）。默认建麦克风（必备）+ 系统声音（可降级）；
        // record_system_only 时刻意不建麦克风（跳过 VPIO/mic VAD），源列表只剩 System。
        let vad_path = models::root().join("silero_vad.onnx");
        let mut sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> = Vec::new();
        if !record_system_only {
            let mic_seg = match new_silero(&vad_path) {
                Ok(s) => s,
                Err(e) => {
                    stash_model(&recognizer_cache, Some(recognizer));
                    stash_model(&embedder_cache, embedder);
                    return fail(&app, &running, &generation, my_gen, format!("error: {e}"));
                }
            };
            // 麦克风源：macOS 默认用带 Apple AEC 的 VPIO（内部失败自动回退 cpal）；
            // 「保持外放音量」开启时改用普通 cpal 输入——VPIO(通话模式)一启动 macOS
            // 就把其它音频压低 12-16dB(ducking,Min 档配置下仍如此,系统固有行为),
            // 外放开会场景既听不清、录下的系统声轨电平也小;普通输入无 ducking,
            // 回声由下方装配的软件 AEC(WebRTC AEC3)消除,文本回声去重链保留为兜底。
            // 其他平台恒用 cpal。
            #[cfg(target_os = "macos")]
            let mic: Box<dyn AudioCapture> = if keep_output_volume {
                Box::new(audio::microphone::Microphone::new())
            } else {
                Box::new(audio::vpio::VpioMicrophone::new())
            };
            #[cfg(not(target_os = "macos"))]
            let mic: Box<dyn AudioCapture> = Box::new(audio::microphone::Microphone::new());
            sources.push((Source::Mic, mic, mic_seg));
        }
        // record_system_only 且非 macOS：System 源不存在（下方块仅 macOS 编译），
        // 源列表将为空，start_session 会因无源可启动返回 Err、开录失败——正是 required
        // 守卫要兜住的场景（本应用 macOS-only，可接受）。

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

        // 软件回声消除(WebRTC AEC3):「保持外放音量」下 VPIO 不启动,改由本模块以
        // system 采集流为远端参考,把外放回声从 mic 波形里消掉——mic 路只剩本人声音,
        // 文本级回声去重链降级为兜底。仅 mic+system 双源齐备才有意义;初始化失败
        // 降级为无 AEC(行为同引入前),绝不挡录制。VPIO 模式(默认)不叠加软件 AEC。
        let mut aec_roles: Vec<(Source, audio::aec::AecRole)> = Vec::new();
        if keep_output_volume
            && sources.iter().any(|(s, _, _)| *s == Source::Mic)
            && sources.iter().any(|(s, _, _)| *s == Source::System)
        {
            match audio::aec::new_pair(16000) {
                Ok((render, capture)) => {
                    eprintln!("软件回声消除已启用(WebRTC AEC3 + AGC2 自适应增益): system 路为参考,mic 路消回声+自动增益");
                    aec_roles.push((Source::System, audio::aec::AecRole::Render(render)));
                    aec_roles.push((Source::Mic, audio::aec::AecRole::Capture(capture)));
                }
                Err(e) => {
                    eprintln!("软件回声消除初始化失败,本场降级为无 AEC(不影响录制): {e}");
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
        // 续录前把该目录的音频解回 WAV，供本场从尾部对齐续写。必须先 cancel_and_wait：
        // 若转码 worker 此刻正把本目录的 wav 压成 m4a，解码会与它撞文件，故先摘队列 +
        // 阻塞等 in-flight 转完。锁序纪律：本调用点在加载线程、不持任何全局锁
        //（running/generation/session_slot 均未持有），符合「持全局锁时绝不调
        // cancel_and_wait 这类阻塞方法」。decode_note_to_wav 内部失败已降级打日志，无需包错。
        // 解码先行(在建 AudioTrackWriter 之前)：建档时要按既有 WAV 尾部长度对既有轨道做
        // 截断/零填充对齐，故必须先把已压缩音频解回 WAV。base_ms 本身来自 segments 时间轴
        //（下方 writer.base_ms()），与解码顺序无关。
        if let NoteTarget::Resume(_) = &target {
            let note_dir = writer.lock().unwrap().dir().to_path_buf();
            transcode.cancel_and_wait(&note_dir);
            store::transcode::decode_note_to_wav(&note_dir);
        }
        // 续录时间轴偏移：New 路径恒 0；Resume 路径 = 续录前最大 end_ms。
        // on_final 落盘/emit 前 start_ms/end_ms 均 + base_ms（partial 无时间戳，不受影响）。
        let base_ms = writer.lock().unwrap().base_ms();
        // 说话人编号/质心延续 + 库种子注入：快照（续录）优先，库中同 person 不重复注入。
        // 库加载失败降级为无种子，绝不挡录制。
        let seeds = load_voiceprint_seeds(&app);
        let mut registry = crate::diar::registry::SpeakerRegistry::with_seeds(
            &writer.lock().unwrap().registry_snapshot(),
            &seeds,
        );
        // 本场实时入库产生的 person id 集合:enroller(ASR worker 线程)写入,停止时的
        // Snapshot 分支读取,用于区分「本场新入库的陌生声音」与「种子命中的老熟人」——
        // 样本只为前者写(见 Snapshot 分支注释)。
        let live_enrolled: Arc<Mutex<std::collections::HashSet<String>>> =
            Arc::new(Mutex::new(std::collections::HashSet::new()));
        // 实时全局入库：新识别出的声纹一旦够料(≥AUTO_ENROLL_MS)当场入库领全局
        // person id(P<n>)，说话人从此刻起就有全局唯一身份，不必等停止。回调在
        // ASR worker 线程同步执行,一个新说话人只发生一次,库写失败降级为 None
        // (下条 final 自动重试),绝不影响转写主流程。库路径不可用则不装配——
        // 停止时的 Snapshot upsert 仍是兜底入库路径,行为同旧版。
        if let Ok(root) = data_root(&app) {
            let vp_store_e = store::VoiceprintStore::new(root);
            let live_enrolled_e = live_enrolled.clone();
            registry.set_enroller(
                store::AUTO_ENROLL_MS,
                Box::new(move |snap| {
                    match vp_store_e
                        .upsert_from_session(std::slice::from_ref(snap), &chrono::Local::now().to_rfc3339())
                    {
                        Ok(links) => {
                            let pid = links.get(&snap.id).cloned();
                            if let Some(pid) = &pid {
                                live_enrolled_e.lock().unwrap().insert(pid.clone());
                            }
                            pid
                        }
                        Err(e) => {
                            eprintln!("声纹实时入库失败(不影响录制,稍后自动重试): {e}");
                            None
                        }
                    }
                }),
            );
        }

        // 3) 起会话。emit 回调带 source 字符串。
        let app_f = app.clone();
        let app_p = app.clone();
        let app_d = app.clone();
        let writer_f = writer.clone();
        let writer_d = writer.clone();
        // 声纹库句柄：闭包前构造一次，供 Snapshot 分支停止时的入库回写。用 Option
        // 包裹而非兜底占位路径——app_data_dir 解析失败时彻底跳过库回写（None），
        // 而不是拿一个空/相对路径去读写，那样反而可能在意外位置产生副作用文件。
        let vp_store_d: Option<store::VoiceprintStore> = match data_root(&app) {
            Ok(root) => Some(store::VoiceprintStore::new(root)),
            Err(e) => {
                eprintln!("声纹库路径不可用，本场停止时的库回写将被跳过（不影响笔记落盘）: {e}");
                None
            }
        };
        // 音频保留:每个配置的源一个惰性轨道写入器(首帧才建档;失败只降级,不影响转写)。
        // 写盘走独立线程 + 无界通道:磁盘卡顿(Spotlight/Time Machine/外置盘)绝不
        // 反压分段 worker 与采集实时线程——增值层不许伤转写热路径。无界与 NoteWriter
        // 待写队列同哲学:内存暂存优于丢内容。base_ms 对齐语义见 AudioTrackWriter。
        // keep_audio=false 时完全跳过写盘器/写盘线程构建(两者留空 Vec):关闭音频保留,
        // 转写/声纹零影响——音频落盘是纯增值旁路,sink 仅把采集帧复制一份写 WAV,不在
        // 转写热路径上;audio_joins 空 Vec 在 stop 时 join 无害(空循环),start_session
        // 签名不变(空 sinks 即不落任何音频轨)。
        let mut audio_sinks: Vec<(Source, Box<dyn FnMut(&[f32]) + Send>)> = Vec::new();
        let mut audio_joins: Vec<std::thread::JoinHandle<()>> = Vec::new();
        if keep_audio {
            let note_dir = writer.lock().unwrap().dir().to_path_buf();
            for (source, _, _) in &sources {
                let (tx, rx) = crossbeam_channel::unbounded::<Vec<f32>>();
                let mut w = store::audio::AudioTrackWriter::new(&note_dir, source.as_str(), base_ms);
                audio_joins.push(std::thread::spawn(move || {
                    for chunk in rx.iter() {
                        w.append(&chunk);
                    }
                    // sink 随分段 worker 退出被 drop → 通道关闭 → 此处 w Drop 补头刷盘收尾。
                }));
                audio_sinks.push((
                    *source,
                    Box::new(move |s: &[f32]| {
                        let _ = tx.send(s.to_vec());
                    }) as Box<dyn FnMut(&[f32]) + Send>,
                ));
            }
        }

        let mut degraded = false;
        // language_filter:会议场景默认过滤中日韩误判幻觉段,多语会议可在设置里关闭以
        // 保留外语真实发言。值在上方与 record_system_only/keep_audio 同一次 settings
        // load 读出(读取失败已保守回落默认过滤开,与 Settings::default 一致)。
        let start = session::start_session(
            sources,
            recognizer,
            embedder,
            registry,
            std::time::Duration::from_millis(session::ECHO_HOLD_MS),
            language_filter,
            16000,
            16000,
            audio_sinks,
            aec_roles,
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
                    // sources 为空 ⇔ 未命中的库种子簇（assign 命中必 sources.insert）：
                    // 这类簇只是种子注入时铺的库人物候选，本场从未真正出现过，不该
                    // 泄漏进说话人表/chips/落盘（否则每场笔记都会囤上全库人物）。
                    let infos: Vec<_> = infos.into_iter().filter(|s| !s.sources.is_empty()).collect();
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
                            person_id: m.person_id.clone(),
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
                            person_id: m.person_id.clone(),
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
                session::DiarEvent::EchoRetract { start_ms, end_ms, text } => {
                    // 已放行的 mic 回声段被 system 定稿追认:磁盘删行 + 通知前端撤回显示。
                    // 时间戳加续录偏移,与 on_final 落盘口径一致。落盘失败仍撤 UI(显示
                    // 优先干净),磁盘差异走 storage 降级告警。
                    let start_ms = start_ms + base_ms;
                    let end_ms = end_ms + base_ms;
                    let mut w = writer_d.lock().unwrap();
                    if let Err(e) = w.retract_segment("mic", start_ms, end_ms, &text) {
                        eprintln!("回声撤回落盘失败({start_ms}-{end_ms}): {e}");
                        let _ = app_d.emit("storage", ipc::StorageEvent { state: "degraded".into() });
                    }
                    drop(w);
                    let _ = app_d.emit(
                        "final_retract",
                        ipc::RetractEvent { source: "mic".into(), start_ms, end_ms, text },
                    );
                }
                session::DiarEvent::Snapshot { snaps, samples } => {
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
                                // 声纹样本落盘:只为「本场新入库的陌生声音」写(实时入库或停止
                                // 兜底入库)。种子命中的老熟人不再追加——识别成功说明既有声纹
                                // 已覆盖这条声音,再存一份没有新信息;识别精度的提升靠质心加权
                                // 回写 + 用户把认错拆重的条目合并进来(样本/质心随合并归一)。
                                // 兜底:老人物一份样本都没有(样本功能上线前的数据/历史写失败)
                                // 时补第一份,兑现管理页"下次录到会自动补上"的承诺。
                                let sample_of = |cluster: &str| {
                                    samples.iter().find(|(id, _)| id == cluster).map(|(_, s)| s)
                                };
                                let newly = live_enrolled.lock().unwrap();
                                for snap in &snaps {
                                    let pid = snap
                                        .person
                                        .clone()
                                        .or_else(|| enrolled.get(&snap.id).cloned());
                                    let (Some(pid), Some(sample)) = (pid, sample_of(&snap.id)) else {
                                        continue;
                                    };
                                    let newly_enrolled =
                                        newly.contains(&pid) || enrolled.contains_key(&snap.id);
                                    if !newly_enrolled && !store.sample_paths_existing(&pid).is_empty() {
                                        continue; // 识别出的老熟人且已有样本:不再累积
                                    }
                                    if let Err(e) = store.append_sample(&pid, sample) {
                                        eprintln!("声纹样本写入失败({pid},不影响笔记): {e}");
                                    }
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
                // Fix A(泛化): required_sources 里的每个源都必备——任一未出现在 active
                // 就整场拆除报错(不静默降级)。默认配置 required=[Mic],与原先"mic 必备"
                // 逐字节等价(同样先 stop 排干可能已产生的其它源 finals → stash 模型 →
                // abort_or_finalize → 带源名 fail);system_only 下 required=[System],改由
                // System 缺失触发同一条拆除路径。
                if let Some(&missing) = required_sources(record_system_only)
                    .iter()
                    .find(|s| !start.active.contains(s))
                {
                    let (r, e) = start.handle.stop(); // 先排干可能已产生的其它源 finals
                    stash_model(&recognizer_cache, r);
                    stash_model(&embedder_cache, e);
                    abort_or_finalize(&writer);
                    let name = source_display(missing);
                    let err = start.failed.iter()
                        .find(|(s, _)| *s == missing)
                        .map(|(_, msg)| format!("error: {name}未能启动: {msg}"))
                        .unwrap_or_else(|| format!("error: {name}未能启动"));
                    return fail(&app, &running, &generation, my_gen, err);
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
                    audio_joins,
                });
                drop(running_guard);
                let _ = app.emit(
                    "status",
                    ipc::StatusEvent { state: "recording".into(), system_audio, note_id: note_id.clone(), diarization, elapsed_ms: base_ms },
                );
                // 会话已入槽、"recording" 已发：托盘切红点态（图标+菜单文案「停止录制」）。
                tray::set_recording(&app, true);
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

/// 开录共用实现(命令壳、快捷键共用):守卫 + spawn_session。逐语句搬自原
/// start_recording 命令体,唯一改动是 state 由 `app.state()` 取(与 `State<AppState>`
/// 注入等价)、app 因签名为 &AppHandle 而在传入 spawn_session 时 clone——逻辑零变化。
fn do_start_recording(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    // download_running 兼作迁移/下载互斥位:任一在跑都不能开录(下载中模型不完整、迁移中
    // 目录在搬)。原先仅靠模型 present 判定挡不住"下载已把文件补到位但还在收尾"的窗口。
    if state.download_running.load(Ordering::SeqCst) {
        return Err("正在迁移或下载,稍后再试".into());
    }
    if !models::recording_ready(&current_asr(app)) {
        return Err("模型缺失：请先在设置页下载所选识别模型".into());
    }
    spawn_session(
        app.clone(),
        state.running.clone(),
        state.generation.clone(),
        state.session.clone(),
        state.recognizer_cache.clone(),
        state.embedder_cache.clone(),
        state.transcode.clone(),
        NoteTarget::New,
    )
}

#[tauri::command]
fn start_recording(app: AppHandle) -> Result<(), String> {
    // 薄壳:前端 invoke("start_recording") 无参,去掉 State 参数(实现里 app.state() 取)。
    do_start_recording(&app)
}

/// 续录一场非活动（已中断或已完成）笔记：运行守卫与 start_recording 完全一致
/// （同一份 spawn_session 实现），仅 target 换成 Resume(note_id)。
#[tauri::command]
fn resume_recording(app: AppHandle, state: State<AppState>, note_id: String) -> Result<(), String> {
    // 同 start_recording:迁移/下载进行中不能开录(见该处注释)。
    if state.download_running.load(Ordering::SeqCst) {
        return Err("正在迁移或下载,稍后再试".into());
    }
    // F1 修复:该笔记正在精修中就拒绝续录,避免精修收尾时才 enqueue 的转码把续录正在
    // 追加写的 WAV 当成"已完成"编码后删除(见 resume_blocked_by_refining 文档)。
    if resume_blocked_by_refining(&state.refining.lock().unwrap(), &note_id) {
        return Err("该笔记正在精修,请稍后再试".into());
    }
    if !models::recording_ready(&current_asr(&app)) {
        return Err("模型缺失：请先在设置页下载所选识别模型".into());
    }
    spawn_session(
        app,
        state.running.clone(),
        state.generation.clone(),
        state.session.clone(),
        state.recognizer_cache.clone(),
        state.embedder_cache.clone(),
        state.transcode.clone(),
        NoteTarget::Resume(note_id),
    )
}

/// 停录共用实现(命令壳、快捷键共用)。逐语句搬自原 stop_recording 命令体,唯一改动是
/// state 由 `app.state()` 取、末尾 preload_models 的 app 因签名为 &AppHandle 而
/// clone——逻辑零变化(含全部锁序注释)。
fn do_stop_recording(app: &AppHandle) {
    let state = app.state::<AppState>();
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
        // 分段 worker 已 join → audio sink 已 drop → 写盘线程排干后自退,join 保证
        // finalize 前 WAV 头已收尾(正常情况下队列近空,瞬时完成)。
        for j in s.audio_joins {
            let _ = j.join();
        }
        note_id = s.note_id;
        let mut w = s.writer.lock().unwrap();
        let finalized = w.finalize(chrono::Local::now());
        drop(w);
        match finalized {
            Ok(()) => {
                // 仅 finalize 成功（state=complete、meta 落盘）才发起精修。精修管线接管了
                // 转码移交时机：本地两段（filter/recluster）读完 WAV 后才在 spawn_refine
                // 内部 enqueue，避免转码 worker 与精修读 WAV 打架；spawn_refine 自身对
                // 任何前置失败/panic 都兜底保证转码仍会入队一次（见其文档注释），故换掉
                // 这里的直接 enqueue 不会让 WAV 永不压缩，只是入队时点后移到精修线程内。
                spawn_refine(app.clone(), note_id.clone(), true);
            }
            Err(e) => {
                eprintln!("stop_recording: finalize 失败: {e}");
                let _ = app.emit("storage", ipc::StorageEvent { state: "degraded".into() });
            }
        }
    }
    let _ = app.emit(
        "status",
        ipc::StatusEvent { state: "stopped".into(), system_audio: String::new(), note_id, diarization: String::new(), elapsed_ms: 0 },
    );
    // 停录：托盘回 idle 态（图标+菜单文案「开始录制」）。托盘不存在则内部静默跳过。
    tray::set_recording(app, false);
    // 停录补预载：录制中下载完成的模型（预载被活跃跳过）此刻补进空槽；幂等，槽有货即跳。
    preload_models(app.clone(), state.session.clone(), state.recognizer_cache.clone(), state.embedder_cache.clone());
}

#[tauri::command]
fn stop_recording(app: AppHandle) {
    // 薄壳:前端 invoke("stop_recording") 无参,去掉 State 参数(实现里 app.state() 取)。
    do_stop_recording(&app)
}

/// 快捷键共用的录制切换:running 为真则停,否则开。开录失败只 eprintln——快捷键触发
/// 没有 UI 上下文,错误无处弹窗(设置缺失/模型未就绪等),静默进日志避免打断用户。
/// running 读取用 statement-scoped 的锁,读完即放,不与 do_* 内部锁嵌套。
pub(crate) fn toggle_recording(app: &AppHandle) {
    let running = *app.state::<AppState>().running.lock().unwrap();
    if running {
        do_stop_recording(app);
    } else if let Err(e) = do_start_recording(app) {
        eprintln!("快捷键触发开录失败(静默进日志): {e}");
    }
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

/// 手动（重）触发一次会后精修：录制中该 id 拒绝（内容未定稿，段落还在变），正在精修中
/// 也拒绝（并发跑两遍纯浪费且会互相覆盖 refined.json）。手动重跑时 m4a 早已在盘上
/// （首次精修已经移交过转码），故 `enqueue_transcode_after_local=false`，不再重复入队。
#[tauri::command]
fn refine_note(app: AppHandle, state: State<AppState>, id: String) -> Result<(), String> {
    if let Some(s) = state.session.lock().unwrap().as_ref() {
        if s.note_id == id {
            return Err("该笔记正在录制，停止后才能精修".into());
        }
    }
    if state.refining.lock().unwrap().contains(&id) {
        return Err("该笔记正在精修中".into());
    }
    spawn_refine(app, id, false);
    Ok(())
}

/// 读取已落盘的精修结果（refined.json）；从未精修过 / 精修在前置阶段就失败到没能落盘
/// 时返回 None，前端据此回落展示原始 segments。
#[tauri::command]
fn get_refined(app: AppHandle, id: String) -> Result<Option<store::RefinedDoc>, String> {
    store::validate_note_id(&id).map_err(|e| e.to_string())?;
    let dir = notes_dir(&app).map_err(|e| e.to_string())?.join(&id);
    Ok(store::load_refined(&dir))
}

/// 笔记音频轨道信息(详情页播放器用)。**纯读**:陈旧 WAV 头(硬崩残留)的修复
/// 统一放在应用启动扫描(setup)与续录 open——此前放在这里做过"非活动才修",但
/// stop 排干窗口 / 开录入槽窗口里 session 槽都是空的,check-then-act 挡不住与
/// 写盘线程并发互踩,读路径必须无副作用。
#[tauri::command]
fn note_audio_info(app: AppHandle, id: String) -> Result<Vec<store::audio::TrackInfo>, String> {
    store::validate_note_id(&id).map_err(|e| e.to_string())?;
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    let note_dir = dir.join(&id);
    if !note_dir.is_dir() {
        return Err(format!("笔记不存在: {id}"));
    }
    Ok(store::audio::list_tracks(&note_dir))
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
                    person_id: m.person_id.clone(),
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

/// 声纹库四命令共用：打开 data_root 下的 VoiceprintStore（与逐场笔记目录并列，
/// 不是 notes_dir 的子目录）。
fn open_voiceprint_store(app: &AppHandle) -> Result<store::VoiceprintStore, String> {
    data_root(app)
        .map(store::VoiceprintStore::new)
        .map_err(|e| e.to_string())
}

/// 声纹库人物列表，供管理页展示。vp.people 本就只含经 redirects 解析后的有效人
/// （merge 已把 loser 移出 people），无需再过一遍 resolve。
#[tauri::command]
fn list_people(app: AppHandle) -> Result<Vec<ipc::PersonSummary>, String> {
    let store = open_voiceprint_store(&app)?;
    let vp = store.load();
    Ok(vp
        .people
        .iter()
        .map(|(id, p)| ipc::PersonSummary {
            id: id.clone(),
            name: p.name.clone(),
            total_ms: p.total_ms,
            last_seen: p.last_seen.clone(),
            sources: p.centroids.keys().cloned().collect(),
            sample_paths: store
                .sample_paths_existing(id)
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
        })
        .collect())
}

/// 改库里人物的显示名：只影响后续会话的种子姓名与笔记侧只读 join，不涉及本场
/// registry 引用结构，录制中也允许（同 rename_speaker 的"改名不挡录制"哲学）。
#[tauri::command]
fn rename_person(app: AppHandle, id: String, name: String) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        // 未命名是系统态(空 name 触发展示端"未命名 · 最近出现…"兜底),不是一个可以
        // 被"改成"的普通名字;改回未命名无意义——清名走删除/合并，不走 rename。
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
    app: AppHandle,
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
        // 按当前选型预载（session 锁已放，此处才读设置：叶子锁纪律不变）。
        let asr_model = current_asr(&app);
        let mut slot = cache.lock().unwrap();
        if slot.is_none() {
            match new_recognizer(&asr_model) {
                Ok(r) => *slot = Some(r),
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
fn models_status(app: AppHandle) -> models::ModelsStatus {
    models::status(&current_asr(&app))
}

#[tauri::command]
fn download_models(app: AppHandle, state: State<AppState>, ids: Option<Vec<String>>) -> Result<(), String> {
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
        // 要下载的工件:显式 ids → 按 id 过滤;None → 按当前选型默认集(vad+选中 ASR+speaker)。
        // 两者都保 ARTIFACTS 原顺序(过滤而非按传入顺序),下载/进度次序稳定。
        let want: Vec<&str> = match &ids {
            Some(ids) => ids.iter().map(|s| s.as_str()).collect(),
            None => default_download_ids(&s.asr_model),
        };
        let selected: Vec<&models::Artifact> = models::ARTIFACTS
            .iter()
            .filter(|a| want.iter().any(|w| *w == a.id))
            .collect();
        // preload 需要 app,但 app 随即被 emit 闭包 move 走,先克隆一份留给补预载。
        let app_pl = app.clone();
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
        for a in selected {
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
            preload_models(app_pl, session, recognizer_cache, embedder_cache);
        }
    });
    Ok(())
}

#[tauri::command]
fn cancel_models_download(state: State<AppState>) {
    state.download_cancel.store(true, Ordering::SeqCst);
}

/// 删除单个模型工件（设置页管理用）。守卫:录制中删会与常驻槽在用实例互踩、下载中删会
/// 与写盘线程撞文件,一律拒绝。File → 删单文件;TarBz2 → 删整个 dest_dir 目录。删完清掉
/// 对应常驻槽（asr/whisper 清识别器、speaker 清嵌入器）,否则删了盘上文件、槽里旧实例
/// 还在,下次开录仍拿旧模型转写,状态与磁盘不一致。清槽是叶子锁单独持有,不与 session 锁嵌套。
#[tauri::command]
fn delete_model(_app: AppHandle, state: State<AppState>, id: String) -> Result<(), String> {
    // _app: root 走 models::root() 无需它,但保留形参与其它模型命令签名一致(Tauri 按类型注入)。
    // 查 running 而非 session 槽:开录命令同步置 running 后即返回,session 槽要数秒后才置
    // Some;查槽会在这段加载窗口误判"空闲",删掉正在被常驻实例/写盘用的模型。running
    // statement-scoped,查完即放。
    if *state.running.lock().unwrap() {
        return Err("录制中不能删除模型".into());
    }
    if state.download_running.load(Ordering::SeqCst) {
        return Err("下载进行中，稍后再试".into());
    }
    let a = models::ARTIFACTS
        .iter()
        .find(|a| a.id == id)
        .ok_or_else(|| format!("未知模型: {id}"))?;
    let root = models::root();
    match &a.kind {
        models::ArtifactKind::File => {
            let p = root.join(a.files[0].rel_path);
            if p.exists() {
                std::fs::remove_file(&p).map_err(|e| format!("删除失败: {e}"))?;
            }
        }
        models::ArtifactKind::TarBz2 { dest_dir } => {
            let p = root.join(dest_dir);
            if p.exists() {
                std::fs::remove_dir_all(&p).map_err(|e| format!("删除失败: {e}"))?;
            }
        }
    }
    // 叶子锁单独持有,不与其它锁嵌套。
    match id.as_str() {
        "asr" | "whisper" => *state.recognizer_cache.lock().unwrap() = None,
        "speaker" => *state.embedder_cache.lock().unwrap() = None,
        _ => {}
    }
    Ok(())
}

#[tauri::command]
fn get_settings(app: AppHandle) -> Result<settings::Settings, String> {
    app.path().app_data_dir().map(|d| settings::load(&d)).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_settings(app: AppHandle, state: State<AppState>, new_settings: settings::Settings) -> Result<(), String> {
    let d = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let old = settings::load(&d);
    // 存储目录（data_dir/models_dir）不走普通设置保存:改它涉及既有数据/模型的搬迁,
    // 必须经专门的迁移功能（负责移动文件 + 重设 override），这里直接拒绝防止指针漂移
    // 而数据不动导致"找不到笔记/模型"。
    if old.data_dir != new_settings.data_dir || old.models_dir != new_settings.models_dir {
        return Err("存储目录变更请使用迁移功能".into());
    }
    // ASR 选型变更:录制中切换会让常驻识别器与正在转写的会话对不上,拒绝;无会话则
    // save 后清掉旧选型的常驻识别器、按新选型重载,无需重启即可用新模型开录。
    // 查 running 而非 session 槽:同 delete_model,开录命令返回后 session 槽尚空的加载
    // 窗口里切型会与即将取用的常驻识别器对不上。statement-scoped。
    let asr_changed = old.asr_model != new_settings.asr_model;
    if asr_changed && *state.running.lock().unwrap() {
        return Err("录制中不能切换识别模型".into());
    }
    // 托盘开关是否变更(落盘后据此建/拆托盘,即时生效无需重启)。
    let tray_changed = old.tray_enabled != new_settings.tray_enabled;
    // 锁内读-改-写(update):整体取前端新值,但 data_dir/models_dir 一律保留磁盘最新值
    //(迁移专管这两指针)——防止本次写把并发迁移刚提交的目录指针覆盖回旧值,随后迁移
    // 删旧 → 笔记"凭空消失"。这正是 update 的 WRITE_LOCK 要串行掉的 load-modify-save 竞态。
    settings::update(&d, |s| {
        let data_dir = s.data_dir.clone();
        let models_dir = s.models_dir.clone();
        *s = new_settings;
        s.data_dir = data_dir;
        s.models_dir = models_dir;
    }).map_err(|e| e.to_string())?;
    if asr_changed {
        *state.recognizer_cache.lock().unwrap() = None;
        preload_models(app.clone(), state.session.clone(), state.recognizer_cache.clone(), state.embedder_cache.clone());
    }
    // 托盘开关变更 → 建/拆托盘（apply_enabled 现读设置后幂等处理）。放在 asr 之后,
    // app 已 clone 给 preload,此处直接用 &app。
    if tray_changed {
        tray::apply_enabled(&app);
    }
    Ok(())
}

/// RAII 解暂停守卫:迁移后台线程无论正常返回、提前 return 还是 panic 展开,转码队列
/// 都必然 unpause——否则一次迁移失败后转码永久静止,只能重启应用。与 ResetOnDrop
/// （复位 download_running 互斥位）配套:两者一起挂在迁移线程头部,兜住所有退出路径。
struct UnpauseOnDrop(Arc<store::transcode::TranscodeQueue>);
impl Drop for UnpauseOnDrop {
    fn drop(&mut self) {
        self.0.unpause();
    }
}

/// 迁移前置互斥守卫(两迁移命令共用):先抢下载/迁移互斥位(download_running),
/// 再查录制中(running)。判"录制中"必须查 running 而非 session 槽:spawn_session 在
/// 命令线程同步置 running=true 并即返回,而 session 槽要到加载线程数秒后才置 Some
///(续录还要先解码,窗口最宽)。若这里查 session.is_some(),那段加载窗口内发起的迁移
/// 会误判"空闲",把正在写的旧 notes 根删掉、吞掉录音。查 running 则开录命令一返回就已
/// 挡住迁移。
///
/// 「先 swap download 再查 running」的次序与 start_recording 的「先查 download 再置
/// running」是对称闭合的:migrate 抢先 swap download → start 的 download 检查必拒;
/// start 抢先置 running → 本函数的 running 检查必拒。两侧各自的 check-then-act 交错窗口
/// 被压到两条原子/加锁语句之间的微秒级(start 读到 download==false 之后、置 running
/// 之前,恰被本函数插入并放行,是残留的微秒级同时放行窗口)——记为已知取舍,个人工具
/// 可接受。running 锁 statement-scoped,查完即放,不与其它锁嵌套(遵守文件顶部锁序)。
fn migrate_guard(running: &Arc<Mutex<bool>>, download_running: &Arc<AtomicBool>) -> Result<(), String> {
    // 先抢互斥位(与 start 的 download 检查对称)。
    if download_running.swap(true, Ordering::SeqCst) {
        return Err("迁移或下载进行中".into());
    }
    // 再查录制中;拒绝时必须复位刚抢下的互斥位,否则迁移互斥位永久卡死。
    if *running.lock().unwrap() {
        download_running.store(false, Ordering::SeqCst);
        return Err("录制中不能迁移".into());
    }
    Ok(())
}

/// 数据目录迁移:把 data_root 下的笔记/声纹整树搬到 new_dir。时序是「复制→校验→
/// **写指针**→删旧」:settings.data_dir 写入是提交点,提交前任何失败都清理新目录、
/// 旧数据与旧指针完好可重试;提交后删旧只是垃圾回收,失败不算迁移失败——消灭
/// 「数据在新处、指针指旧处」的崩溃窗口。守卫只做同步检查与 spawn,搬运/
/// pause_and_wait 全在后台线程——绝不在命令线程(可能持 Tauri 内部锁)里跑阻塞搬运。
#[tauri::command]
fn migrate_data_dir(app: AppHandle, state: State<AppState>, new_dir: String) -> Result<(), String> {
    // 守卫一:目标目录必须不存在或为空(不覆盖用户既有内容),且与当前根互不包含
    //(嵌套会自拷/删旧连带删新)。旧根解析失败直接拒绝。全是只读检查,放在抢互斥位
    // 之前,失败无需复位。
    let new_path = PathBuf::from(&new_dir);
    store::migrate::dir_is_usable_target(&new_path).map_err(|e| e.to_string())?;
    let old_root = data_root(&app).map_err(|e| e.to_string())?;
    store::migrate::ensure_disjoint(&old_root, &new_path).map_err(|e| e.to_string())?;
    // 守卫二:抢迁移/下载互斥位 + 录制守卫(先 swap download 再查 running,见 migrate_guard)。
    migrate_guard(&state.running, &state.download_running)?;
    let running = state.download_running.clone();
    let transcode = state.transcode.clone();
    std::thread::spawn(move || {
        // 两道 RAII:先复位互斥位(ResetOnDrop),再 unpause(UnpauseOnDrop)。Drop 逆序:
        // 先 unpause 再复位 running,顺序无碍,关键是两者都必然发生(含 panic 展开)。
        let _reset = ResetOnDrop(running);
        // pause_and_wait 会阻塞等 in-flight 转码——只在后台线程调,命令线程绝不调。
        transcode.pause_and_wait();
        let _unpause = UnpauseOnDrop(transcode.clone());
        let _ = app.emit("migrate", ipc::MigrateEvent { kind: "data".into(), phase: "copying".into(), message: String::new() });
        let emit_err = |app: &AppHandle, msg: String| {
            let _ = app.emit("migrate", ipc::MigrateEvent { kind: "data".into(), phase: "error".into(), message: msg });
        };
        let entries: &[&str] = &["notes", "voiceprints.json", "voiceprints"];
        // 第一步:复制+校验(失败已自清新目录,旧数据未动)。
        if let Err(e) = store::migrate::copy_and_verify_entries(&old_root, &new_path, entries) {
            return emit_err(&app, format!("{e:#}"));
        }
        // 第二步(提交点):读-改-写 settings(永在 app_data_dir,不随 data_dir 漂移)。
        // 失败 → 迁移未提交:清理新目录残留(保证可原地重试),旧数据与旧指针完好。
        let saved = app.path().app_data_dir().map_err(|e| e.to_string()).and_then(|d| {
            // update 锁内 load→改→save:与并发的镜像/asr 写入串行,防指针提交被旧快照覆盖。
            settings::update(&d, |s| s.data_dir = Some(new_dir.clone()))
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        if let Err(e) = saved {
            store::migrate::cleanup_copied_entries(&new_path, entries);
            return emit_err(&app, format!("保存设置失败,迁移已回滚: {e}"));
        }
        // 自定义目录落在 asset:// 默认作用域外,放行整棵子树供详情页音频播放。
        // 失败只降级打日志(音频可能无法播放,但迁移已提交)。
        if let Err(e) = app.asset_protocol_scope().allow_directory(&new_path, true) {
            eprintln!("asset 作用域放行新 data 目录失败(音频可能无法播放): {e}");
        }
        // 第三步(提交后垃圾回收):删旧。内部失败只打日志,不影响迁移成立。
        store::migrate::remove_old_entries(&old_root, entries);
        let _ = app.emit("migrate", ipc::MigrateEvent { kind: "data".into(), phase: "done".into(), message: String::new() });
    });
    Ok(())
}

/// 模型目录迁移:同构于 migrate_data_dir(复制→校验→写指针→删旧,指针写入是提交点),
/// 搬 models::root() 顶层全部条目(含断点续传分片,整树搬最诚实),提交 = settings.models_dir
/// 保存 + models::set_models_override 重设。
#[tauri::command]
fn migrate_models_dir(app: AppHandle, state: State<AppState>, new_dir: String) -> Result<(), String> {
    // 比 data 多一道守卫:VN_MODELS 环境变量置顶于 models::root() 的解析顺序,此时改
    // settings.models_dir 也不生效,迁了等于白迁,直接拒绝并提示先移除环境变量。
    if let Ok(v) = std::env::var("VN_MODELS") {
        if !v.is_empty() {
            return Err("VN_MODELS 环境变量生效中,请先移除再迁移".into());
        }
    }
    let new_path = PathBuf::from(&new_dir);
    store::migrate::dir_is_usable_target(&new_path).map_err(|e| e.to_string())?;
    let old_root = models::root();
    // 嵌套守卫同 data:目标与当前模型根互不包含。以上皆只读检查,失败无需复位。
    store::migrate::ensure_disjoint(&old_root, &new_path).map_err(|e| e.to_string())?;
    // 抢迁移/下载互斥位 + 录制守卫(先 swap download 再查 running,见 migrate_guard)。
    migrate_guard(&state.running, &state.download_running)?;
    // 顶层条目文件名(read_dir 收集 String):不存在的旧根视作空(首次即自定义,无可搬)。
    let entries: Vec<String> = std::fs::read_dir(&old_root)
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    let running = state.download_running.clone();
    let transcode = state.transcode.clone();
    std::thread::spawn(move || {
        let _reset = ResetOnDrop(running);
        transcode.pause_and_wait();
        let _unpause = UnpauseOnDrop(transcode.clone());
        let _ = app.emit("migrate", ipc::MigrateEvent { kind: "models".into(), phase: "copying".into(), message: String::new() });
        let emit_err = |app: &AppHandle, msg: String| {
            let _ = app.emit("migrate", ipc::MigrateEvent { kind: "models".into(), phase: "error".into(), message: msg });
        };
        let entry_refs: Vec<&str> = entries.iter().map(|s| s.as_str()).collect();
        // 第一步:复制+校验(失败已自清新目录,旧模型未动)。
        if let Err(e) = store::migrate::copy_and_verify_entries(&old_root, &new_path, &entry_refs) {
            return emit_err(&app, format!("{e:#}"));
        }
        // 第二步(提交点):settings.models_dir 保存;失败清理新目录残留,旧指针完好可重试。
        let saved = app.path().app_data_dir().map_err(|e| e.to_string()).and_then(|d| {
            // update 锁内 load→改→save:与并发的镜像/asr 写入串行,防指针提交被旧快照覆盖。
            settings::update(&d, |s| s.models_dir = Some(new_dir.clone()))
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        if let Err(e) = saved {
            store::migrate::cleanup_copied_entries(&new_path, &entry_refs);
            return emit_err(&app, format!("保存设置失败,迁移已回滚: {e}"));
        }
        // 提交生效:立即重设 override,后续 models::root() 即指向新处,无需重启。
        models::set_models_override(Some(new_path.clone()));
        // 第三步(提交后垃圾回收):删旧。内部失败只打日志,不影响迁移成立。
        store::migrate::remove_old_entries(&old_root, &entry_refs);
        let _ = app.emit("migrate", ipc::MigrateEvent { kind: "models".into(), phase: "done".into(), message: String::new() });
    });
    Ok(())
}

/// 设置页「音频占用磁盘」展示:遍历 notes 根统计所有笔记的音频文件字节数。
/// 纯读操作,不需要任何守卫(不碰转码/录制状态)。
#[tauri::command]
fn audio_disk_usage(app: AppHandle) -> Result<u64, String> {
    let notes = notes_dir(&app).map_err(|e| e.to_string())?;
    Ok(store::disk::audio_usage_bytes(&notes))
}

/// 按时间清理已完成笔记的音频(保留转写文字,只删音频文件释放磁盘)。
/// 守卫改用与两迁移命令共用的 `migrate_guard`(swap `download_running` 兼作迁移/下载
/// 互斥位 + 查 `running`),而非只查 running:
///   - 若只查 running,清理会与 `migrate_data_dir` 并发——迁移复制途中的音频被清理删掉,
///     迁移的复制/校验会伪失败(明明是被并发删的,却报成迁移出错)。互斥后二者不再并发。
///   - `TranscodeQueue.paused` 是布尔而非计数:清理与迁移原先各自独立 pause/unpause,谁先
///     解除就打掉对方的暂停(clobber)。互斥闭合后两者永不并发,pause 布尔 clobber 随之消失。
/// 通过后 `pause_and_wait` 静止转码队列(防止清理途中 worker 正把某笔记的 wav 转成 m4a,
/// 清理和转码撞同一批文件),`UnpauseOnDrop` 保证无论正常返回还是提前 return 都必然解除暂停。
/// `ResetOnDrop` 复位迁移/下载互斥位:purge 是同步命令(不开后台线程),函数尾自然 drop 复位,
/// 无需照 migrate 那样挂到后台线程头部。
/// 已知取舍:命令线程会在 `pause_and_wait` 处最多阻塞等一个 in-flight 转码(秒级)才返回。
///
/// 与迁移不同:这里**不**开后台线程——遍历+删文件是百级笔记毫秒到秒级的量级,
/// 同步跑完直接返回释放字节数即可,没必要为它另起一套进度事件。
///
/// 是否为「活动笔记」用 session 槽的 note_id 比对,而非 state 参数——此时 running 已由
/// migrate_guard 确认为 false,正常不会有会话在槽里;这里仍查一次是纯防御(万一未来某处
/// 状态机出现 running=false 但 session 槽未及时清空的窗口,也不至于删正在使用的笔记的音频)。
/// 这与 `reject_if_active`(单笔记编辑命令按 note_id 拒绝活动笔记)同源:那边有具体 note_id
/// 可比对,这边是批量清理、无单一 note_id,故退化为「跳过 == session 槽笔记」的防御性比对。
#[tauri::command]
fn purge_audio(app: AppHandle, state: State<AppState>, older_than_days: Option<u32>) -> Result<u64, String> {
    migrate_guard(&state.running, &state.download_running)?;
    let _reset = ResetOnDrop(state.download_running.clone());
    state.transcode.pause_and_wait();
    let _unpause = UnpauseOnDrop(state.transcode.clone());
    // cutoff 与 meta 里的 RFC3339 字符串同源(都来自 Local::now)，可直接字符串比较。
    let cutoff = older_than_days
        .map(|d| (chrono::Local::now() - chrono::Duration::days(d as i64)).to_rfc3339());
    let active_id = state.session.lock().unwrap().as_ref().map(|s| s.note_id.clone());
    let notes = notes_dir(&app).map_err(|e| e.to_string())?;
    let Ok(rd) = std::fs::read_dir(&notes) else {
        return Ok(0);
    };
    let mut freed = 0u64;
    for entry in rd.flatten() {
        let note_dir = entry.path();
        if !note_dir.is_dir() {
            continue;
        }
        let is_active = active_id.as_deref() == note_dir.file_name().and_then(|n| n.to_str());
        if is_active || !store::disk::should_purge(&note_dir, cutoff.as_deref()) {
            continue;
        }
        freed += store::disk::purge_note_audio(&note_dir);
    }
    Ok(freed)
}

/// 设置页保存快捷键后调用:按最新设置(重)注册。失败时把 shortcut_enabled 写回 false
/// (S9 之前后端自洽的"注册失败回落关":坏快捷键不会残留开启态、下次启动反复失败),再把
/// 原始中文错误上抛给设置页提示用户。回落写盘失败不掩盖原错误,仍返回注册失败原因。
#[tauri::command]
fn apply_shortcut(app: AppHandle) -> Result<(), String> {
    if let Err(e) = shortcuts::apply_from_settings(&app) {
        if let Ok(d) = app.path().app_data_dir() {
            let _ = settings::update(&d, |s| s.shortcut_enabled = false);
        }
        return Err(e);
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
/// 屏幕录制权限预检(macOS):系统声音采集(ScreenCaptureKit)依赖该权限,未授权时
/// System 源只会在开录后静默降级为仅麦克风——录制页据此在**开录前**就给出常驻
/// 提示与授权入口,终结"录了半天发现对方声音全没进笔记"。
#[tauri::command]
fn screen_capture_permission() -> bool {
    #[cfg(target_os = "macos")]
    return unsafe { CGPreflightScreenCaptureAccess() };
    #[cfg(not(target_os = "macos"))]
    true
}

/// 触发系统授权弹窗并把本应用登记进「屏幕录制」列表。macOS 对每个 App 一生只弹
/// 一次,之后调用只返回当前状态——前端拿到 false 时应引导去系统设置手动开。
#[tauri::command]
fn request_screen_capture_permission() -> bool {
    #[cfg(target_os = "macos")]
    return unsafe { CGRequestScreenCaptureAccess() };
    #[cfg(not(target_os = "macos"))]
    true
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(shortcuts::on_shortcut)
                .build(),
        )
        .manage(AppState::default())
        .on_window_event(|window, event| {
            // 关窗即隐藏（而非退出）:仅当托盘**实际存在**时拦截关闭并隐藏主窗——托盘常驻
            // 才有"隐藏后再打开"的入口。判定按 tray_by_id 查托盘实存,而非读 settings.tray_enabled:
            // 设置只是"意图",托盘可能因创建失败而不存在;若按意图拦截,托盘建失败时关窗仍被隐藏
            // 却再无召回窗口的路径,窗口彻底消失。以托盘实存为准才保证隐藏后一定有召回入口。
            // 录制不中断是本特性核心承诺:hide 只是隐藏窗口，会话线程与录制状态完全不受影响。
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() != "main" {
                    return;
                }
                if window.app_handle().tray_by_id(tray::TRAY_ID).is_some() {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .setup(|app| {
            let handle = app.handle().clone();
            // settings.json 是自举指针,永远读写 app_data_dir(不随 data_dir 漂移)。
            let app_data = handle.path().app_data_dir().ok();
            // 最先执行:stderr/stdout 黑匣子(见 logging.rs)。后续任何 eprintln 与
            // ONNX Runtime 的错误输出都要进日志,晚一步就可能漏掉启动期报错。
            if let Some(dir) = &app_data {
                logging::redirect_stdio_to_file(dir);
            }
            let s = app_data.as_ref().map(|d| settings::load(d)).unwrap_or_default();
            // 模型目录覆盖:settings.models_dir 注入(None 也调,清除历史覆盖,幂等)。
            // 必须先于 models::root() 的任何使用。
            models::set_models_override(s.models_dir.clone().map(PathBuf::from));
            // 生产模型根目录注入（VN_MODELS / override / dev 目录优先级更高，见 models::root）。
            if let Some(dir) = &app_data {
                let models_dir = dir.join("models");
                let _ = std::fs::create_dir_all(&models_dir);
                models::init_app_root(models_dir);
            }
            models::download::sweep_tmp(&models::root());

            let st = app.state::<AppState>();
            match data_root(&handle) {
                Ok(root) => {
                    // 自定义 data_dir(非默认 app_data_dir)落在 asset:// 默认作用域之外,
                    // 详情页音频播放会被 scope 拦掉——显式放行整棵子树。失败只 eprintln
                    // 降级(自定义目录音频可能无法播放,但绝不挡启动/录制)。
                    if app_data.as_deref() != Some(root.as_path()) {
                        if let Err(e) = app.asset_protocol_scope().allow_directory(&root, true) {
                            eprintln!("asset 作用域放行 data_root 失败(自定义目录音频可能无法播放): {e}");
                        }
                    }
                    // 启动扫描 data_root/notes:①修复陈旧 WAV 头(硬崩后头尺寸落后于数据,
                    // 播放端看不到尾段);②对已 complete 且有真实 wav 的笔记入队转码(上次
                    // 没转完 / 新迁入的历史 WAV)。此刻必无录制会话,与写盘线程零竞态。
                    if let Ok(rd) = std::fs::read_dir(root.join("notes")) {
                        for e in rd.flatten() {
                            if e.path().is_dir() {
                                store::audio::repair_stale_tracks(&e.path());
                                if should_enqueue_transcode(&e.path()) {
                                    st.transcode.enqueue(e.path());
                                }
                            }
                        }
                    }
                }
                Err(e) => eprintln!("data_root 解析失败,跳过启动扫描/转码回溯(不影响录制): {e}"),
            }
            // 转码 worker 常驻:录制中让路,空闲时串行消费队列(启动回溯 + 后续停录入队)。
            // 真实转码函数外包一层完成通知:转码完成瞬间源 WAV 被删,已打开的详情页
            // 播放器引用失效(停录后立即点播放的竞态窗口)——发事件让前端重拉音轨。
            let transcode_emit = handle.clone();
            st.transcode.spawn_worker(st.running.clone(), move |dir: &std::path::Path| {
                store::transcode::transcode_note_dir(dir);
                if let Some(id) = dir.file_name().and_then(|s| s.to_str()) {
                    let _ = transcode_emit
                        .emit("transcode_done", ipc::TranscodeEvent { note_id: id.to_string() });
                }
            });

            preload_models(handle.clone(), st.session.clone(), st.recognizer_cache.clone(), st.embedder_cache.clone());
            // 依设置注册全局快捷键;坏快捷键(格式错/与系统冲突)绝不挡启动,仅 eprintln。
            // 与设置页保存路径(apply_shortcut,失败上抛并回落关)是两个消费点。
            if let Err(e) = shortcuts::apply_from_settings(&handle) {
                eprintln!("全局快捷键注册失败(不影响启动): {e}");
            }
            // 菜单栏托盘：tray_enabled 时建（内部读设置判定）。增值层，一切失败只降级。
            tray::setup(&handle);
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
            refine_note,
            get_refined,
            note_audio_info,
            rename_note,
            delete_note,
            export_note,
            rename_speaker,
            edit_segment,
            delete_segment,
            set_segment_speaker,
            screen_capture_permission,
            request_screen_capture_permission,
            models_status,
            download_models,
            cancel_models_download,
            delete_model,
            get_settings,
            set_settings,
            apply_shortcut,
            migrate_data_dir,
            migrate_models_dir,
            audio_disk_usage,
            purge_audio,
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
    fn required_sources_follow_system_only() {
        use crate::audio::Source;
        assert_eq!(super::required_sources(false), vec![Source::Mic]);
        assert_eq!(super::required_sources(true), vec![Source::System]);
    }

    #[test]
    fn should_enqueue_only_complete_notes_with_wav() {
        use super::should_enqueue_transcode;
        let tmp = tempfile::tempdir().unwrap();
        // 无 meta → 否
        assert!(!should_enqueue_transcode(tmp.path()));
        let meta = |state: &str| format!(
            r#"{{"schema_version":1,"id":"n","title":"t","started_at":"","ended_at":null,"state":"{state}"}}"#);
        std::fs::write(tmp.path().join("meta.json"), meta("recording")).unwrap();
        std::fs::write(tmp.path().join("mic.wav"), vec![0u8; 100]).unwrap();
        assert!(!should_enqueue_transcode(tmp.path()), "已中断可续录,不转码");
        std::fs::write(tmp.path().join("meta.json"), meta("complete")).unwrap();
        assert!(should_enqueue_transcode(tmp.path()));
        std::fs::remove_file(tmp.path().join("mic.wav")).unwrap();
        assert!(!should_enqueue_transcode(tmp.path()), "无 wav 无事可做");
    }

    #[test]
    fn download_selection_defaults_to_required_plus_speaker() {
        use super::default_download_ids;
        let ids = default_download_ids("sense_voice");
        assert_eq!(ids, vec!["vad", "speaker", "asr"]);
        let ids = default_download_ids("whisper");
        assert_eq!(ids, vec!["vad", "speaker", "whisper"]);
    }

    #[test]
    fn migrate_guard_rejects_recording_and_download() {
        use super::migrate_guard;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        // running=true → 拒,且必须复位刚抢下的互斥位(否则迁移互斥位永久卡死)。
        let running = Arc::new(Mutex::new(true));
        let dl = Arc::new(AtomicBool::new(false));
        assert!(migrate_guard(&running, &dl).is_err(), "录制中拒绝");
        assert!(!dl.load(Ordering::SeqCst), "拒绝后复位互斥位");
        // download_running 已 true(下载/另一迁移在跑）→ 拒。
        let running = Arc::new(Mutex::new(false));
        let dl = Arc::new(AtomicBool::new(true));
        assert!(migrate_guard(&running, &dl).is_err(), "下载/迁移进行中拒绝");
        // 都空闲 → 过,并已抢下互斥位(swap 置 true)。
        let running = Arc::new(Mutex::new(false));
        let dl = Arc::new(AtomicBool::new(false));
        assert!(migrate_guard(&running, &dl).is_ok(), "空闲放行");
        assert!(dl.load(Ordering::SeqCst), "放行后互斥位已抢占");
    }

    #[test]
    fn refine_llm_ready_requires_all_four_fields() {
        use super::refine_llm_ready;
        let base = crate::settings::Settings::default();
        assert!(!refine_llm_ready(&base), "默认全空/关闭 → 未就绪");

        let mut s = base.clone();
        s.refine_base_url = "https://api.deepseek.com".into();
        s.refine_model = "deepseek-chat".into();
        s.refine_api_key = "sk-xxx".into();
        assert!(!refine_llm_ready(&s), "四项齐全但总开关未开 → 仍未就绪");

        s.refine_enabled = true;
        assert!(refine_llm_ready(&s), "开关开且四项齐全 → 就绪");

        for field in ["base_url", "model", "api_key"] {
            let mut s2 = s.clone();
            match field {
                "base_url" => s2.refine_base_url.clear(),
                "model" => s2.refine_model.clear(),
                _ => s2.refine_api_key.clear(),
            }
            assert!(!refine_llm_ready(&s2), "{field} 为空 → 未就绪");
        }
    }

    #[test]
    fn resume_blocked_by_refining_matches_refining_set() {
        use super::resume_blocked_by_refining;
        use std::collections::HashSet;

        let mut refining: HashSet<String> = HashSet::new();
        assert!(!resume_blocked_by_refining(&refining, "note-a"), "精修集不含该 id → 放行");

        refining.insert("note-a".into());
        assert!(resume_blocked_by_refining(&refining, "note-a"), "该 id 正在精修中 → 拒绝续录");
        assert!(!resume_blocked_by_refining(&refining, "note-b"), "只挡命中的 id,不误伤其它笔记");
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
