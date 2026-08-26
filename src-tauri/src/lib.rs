pub mod audio; // devtools bin echo_clean_repro 复现停录清洗崩溃需要直呼 clean_wav
#[cfg(target_os = "macos")]
mod calendar;
#[cfg(not(target_os = "macos"))]
#[path = "calendar_stub.rs"]
mod calendar;
mod feedback;
mod logging;
pub mod pipeline;
pub mod asr;
mod ipc;
pub mod models;
pub mod scene; // devtools bin scene_backfill(#169)重放 SceneSensor 需要
mod session;
pub mod settings;
mod shortcuts;
pub mod store;
mod i18n;
mod player;
mod player_align;
mod player_gate;
pub mod precheck;
mod tray;
mod update;
pub mod diar;
mod ailog;
mod refine;
pub mod retranscribe;
mod graph;
pub mod mcp;
mod redact;
mod telemetry;
mod lifecycle;
mod hooks_external;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tauri::{AppHandle, Emitter, Manager, State};

use audio::{AudioCapture, Source};
use pipeline::frame_tap::{self, SourceHealth, TapNotify, TapPolicy, TappedCapture};
use pipeline::segmenter::Segmenter;
use session::RecordingHandle;

const DOWNLOAD_ATTEMPTS_PER_URL: usize = 3;
/// 同时下载的模型工件数上限。大文件占带宽,取小值折中;不做用户可配。
const MAX_CONCURRENT_DOWNLOADS: usize = 3;

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

/// 一次活动录制：会话句柄 + 笔记 id。
/// P2 起不再持 writer——落盘器所有权在 lifecycle actor 的 Owned 槽里
///（加载线程创建后经 AdoptWriter 消息移交），录制中的一切写经信箱串行。
struct ActiveSession {
    handle: RecordingHandle,
    note_id: String,
    /// classify_system 的结果："on" | "denied" | "unavailable"，供重挂载时重建状态。
    system_audio: String,
    /// 说话人区分可用性："on"（声纹模型就绪）| "unavailable"（缺失，降级），供重挂载重建。
    diarization: String,
    /// **本场开录时那一份设置快照里的声纹模型**。归还嵌入器时用它重贴标签。
    ///
    /// 为什么不能在归还时现读设置:`running` 早在 `handle.stop()` 之前就被置回 false,
    /// 停录排干期间允许 `set_settings` 切模型——现读就会把 A 建的实例标成 B,下一场
    /// 核对通过、用 A 算出整场向量却以 B 写库(codex review 实现轮 P1)。
    speaker_model: String,
    /// 本场自动改用的输入设备名(录前设备检查自动择优):默认输入是蓝牙通话麦时,
    /// 本场 cpal 采集换成的内置/有线设备。空串=没换。重挂载重建横幅用。
    input_override: String,
    /// 计时：会话入槽时刻、续录基线、暂停起点（Some=暂停中）、已累计暂停时长。
    started: std::time::Instant,
    base_ms: u64,
    paused_at: Option<std::time::Instant>,
    paused_accum: std::time::Duration,
    /// 音频写盘线程句柄:stop 时在 handle.stop()(join 分段 worker → sink drop →
    /// 通道关闭)之后 join,保证 finalize 前 WAV 头已收尾。其余提前放弃路径不 join,
    /// 线程随通道关闭自行退出(Drop 收尾)。
    audio_joins: Vec<std::thread::JoinHandle<()>>,
    /// 本场每源 writer 是否至少成功追加过一个块。与 health 的“配置过/收到原始帧”
    /// 不同,这是停录时允许覆盖 sync 的最终真值。
    audio_activity: Vec<(Source, Arc<AtomicBool>)>,
    /// 每源管线健康计数(FrameTap 写入):pipeline_health 命令随时快照,
    /// 会话拆除即随本结构丢弃——健康数据只描述"这一场",无跨场语义。
    health: Vec<(Source, Arc<SourceHealth>)>,
    /// 每源时钟漂移监视器(Task 6 接线):停录时 snapshot 落 drift_report.json,
    /// 会话拆除即随本结构丢弃——与 health 同寿命、同语义(只描述"这一场")。
    drift: Vec<(Source, Arc<pipeline::drift_monitor::DriftMonitor>)>,
    /// Task 7:mic 实测采样率(`kAudioDevicePropertyActualSampleRate`)10s 轮询线程的
    /// stop 发送端。仅持有以随会话拆除而 drop——drop 即断开会合通道,轮询线程的
    /// `recv_timeout` 立即返回 Disconnected 退出,不裸 loop+sleep 泄漏线程(与
    /// vpio.rs 的 stop 通道同款惯例)。
    /// 故意 write-only:唯一职责是被 Drop,从不读取。
    #[allow(dead_code)]
    actual_rate_stop: crossbeam_channel::Sender<()>,
    /// 笔记目录快照:停录时写墙钟-样本对账要用(该路径在 writer 移交前已确定,
    /// 见 start 处 `writer.dir()`)。
    note_dir: std::path::PathBuf,
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
    embedder_cache: Arc<Mutex<Option<Box<diar::TaggedEmbedder>>>>,
    /// 录制中发生过声纹人名变更(Qwen3 热词已过期):停录归还识别器时消费,
    /// 丢弃归还件交预载重建。录制外的变更直接清槽,不经此标记
    /// (见 refresh_qwen_hotwords_cache)。
    hotwords_dirty: Arc<AtomicBool>,
    /// 模型下载互斥位（true = 下载线程在跑）与取消信号。
    download_running: Arc<AtomicBool>,
    download_cancel: Arc<AtomicBool>,
    /// 全局串行转码队列。自带独立叶子锁：绝不在持有 running/generation/session_slot
    /// 任一把锁时调它的阻塞方法（cancel_and_wait 等 in-flight）。停录入队、启动回溯
    /// 扫描入队、续录前 cancel_and_wait 都从队列这一把锁出入，与上述锁序完全解耦。
    transcode: Arc<store::transcode::TranscodeQueue>,
    /// 语义图全量重建调度器:dirty 合并请求,running 保证至多一个 builder。
    graph_scheduler: graph::index::RebuildScheduler,
    /// 用户显式关系补建的进程级单实例闸与协作取消位；与普通 Aing 生命周期分离。
    relation_backfill_running: Arc<AtomicBool>,
    relation_backfill_cancel: Arc<AtomicBool>,
    relation_backfill_run_id: Arc<Mutex<Option<String>>>,
    // refining 集合已删(P3):Aing 态入 lifecycle 内核(machine::RefineState),
    // 防重入/续录拦截由内核裁决,Aing 中查询走 LifecycleHandle::is_refining。
    /// 重转写在跑任务:(note_id, 当前阶段)。单槽 = 全局同时只跑一个(每任务一整套
    /// ORT 管线,与 AING_GATE 同理但直接拒绝不排队——重转写是显式修复动作,静默
    /// 排队会让用户以为卡死)。守卫链:录制中拒/Aing 中拒/槽占用拒,再由 NoteLock
    /// 兜跨进程底。
    retranscribing: Arc<Mutex<Option<(String, String)>>>,
    /// 最近一次重转写的终态(跨任务保留,新任务开始不清、结束时覆盖):UDS/MCP 轮询方
    /// 靠它区分「完成」与「放弃/失败」——桌面事件是易失的,轮询面必须有可查终态。
    retranscribe_last: Arc<Mutex<Option<ipc::RetranscribeEvent>>>,
    /// 补生成成品轨在跑任务(note_id)。单槽同 retranscribing 的理由:显式修复动作,
    /// 静默排队会被当成卡死。与录制/重转写双向互斥(见 do_regenerate_mixed 守卫链)。
    mixed_regen: Arc<Mutex<Option<String>>>,
    /// 前端是否有活动播放会话(与迷你浮层同源判定)。只服务托盘菜单的「停止播放」项:
    /// 会话语义(装载不算播放)在前端,后端只有装载代次,故由前端 set_playback_active 告知。
    playback_active: Arc<AtomicBool>,
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
            hotwords_dirty: Arc::new(AtomicBool::new(false)),
            download_running: Arc::new(AtomicBool::new(false)),
            download_cancel: Arc::new(AtomicBool::new(false)),
            transcode: store::transcode::TranscodeQueue::new(),
            graph_scheduler: graph::index::RebuildScheduler::default(),
            relation_backfill_running: Arc::new(AtomicBool::new(false)),
            relation_backfill_cancel: Arc::new(AtomicBool::new(false)),
            relation_backfill_run_id: Arc::new(Mutex::new(None)),
            retranscribing: Arc::new(Mutex::new(None)),
            retranscribe_last: Arc::new(Mutex::new(None)),
            mixed_regen: Arc::new(Mutex::new(None)),
            playback_active: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// 数据根目录：app_data_dir 读 settings.json（自举指针，永远在 app_data_dir，不随
/// data_dir 漂移）→ resolve_data_root 得到用户配置的落盘根，未配置则回落 app_data_dir。
/// 笔记/声纹等所有内容都挂这个根；settings 读写命令仍走 app_data_dir。
pub(crate) fn data_root(app: &AppHandle) -> anyhow::Result<PathBuf> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|e| anyhow::anyhow!(tr!("app_data_dir 不可用: {e}", "app_data_dir unavailable: {e}")))?;
    let s = settings::load(&app_data);
    Ok(settings::resolve_data_root(&app_data, &s))
}

/// notes 根目录（不存在则创建），挂在 data_root 下。
pub(crate) fn notes_dir(app: &AppHandle) -> anyhow::Result<PathBuf> {
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
    load_voiceprint_seeds_for(app, &current_speaker_model(app))
}

/// 同上,但门禁比对的是**调用方指定的**模型标签。
///
/// 凡是"先建嵌入器、再取种子"的路径都必须用这个:两处各读一次设置的话,用户在中间
/// 切了模型就会拿 B 算出来的向量去比 A 空间的种子质心——门禁看的是新设置,已经放行了
/// (codex review 实现轮五 P1)。标签应取自建那个嵌入器时用的同一份快照。
fn load_voiceprint_seeds_for(app: &AppHandle, cur: &str) -> Vec<crate::diar::registry::SeedCluster> {
    let Ok(root) = data_root(app) else {
        eprintln!("声纹库路径不可用，本场开录跳过种子注入（不影响录制）");
        return Vec::new();
    };
    let vp = store::VoiceprintStore::new(root).load();
    // 嵌入模型标签与当前选型不一致(切换后台重建尚未完成的窗口)时不注入种子:
    // 不同模型的向量空间不可混比,错认比不认糟。此门禁足以杜绝一切跨空间比较——
    // 种子被跳过后,本场新簇只在新空间内互比;既有人物无种子即不会被命中回写。
    if vp.embedding_model != cur {
        eprintln!("声纹库模型标签({})与当前选型({cur})不一致,本场跳过种子注入(重建完成后恢复)", vp.embedding_model);
        return Vec::new();
    }
    // 种子构建下沉 store::seed_clusters(主质心 + 会话状态变体,同人多种子取 max 命中)。
    store::seed_clusters(&vp)
}

// abort_or_finalize 已随 writer 所有权迁入 lifecycle actor(actor.rs::abort_owned,
// 逐语句等价):失败路径改发 Msg::AbortSession,由 runner 对槽内 writer 执行。

/// 归还识别器/嵌入器进常驻槽（None = 没取到、asr 线程 panic 等，不回收）。
/// 会话归还嵌入器时重新贴标签。标签取自**开录时那一份设置快照**——正是用来挑选
/// 权重、也用来声明写库空间的那一份,三者同源。
fn retag(model: &str, e: Option<Box<dyn diar::SpeakerEmbedder>>) -> Option<Box<diar::TaggedEmbedder>> {
    e.map(|inner| Box::new(diar::TaggedEmbedder::new(model, inner)))
}

/// recognizer_cache 与 embedder_cache 策略完全一致，故共用一个泛型实现。
fn stash_model<T: ?Sized>(cache: &Arc<Mutex<Option<Box<T>>>>, m: Option<Box<T>>) {
    if let Some(m) = m {
        *cache.lock().unwrap() = Some(m);
    }
}

/// Aing 生效执行体(2026-08-11 执行体分层):功能开关 + 引用解析 + HTTP 就绪门,
/// None = 关闭/未配置/引用悬空/HTTP 缺项。HTTP 缺项不回落 Agent——用户显式选了
/// 哪个执行体就用哪个,缺项即未就绪(与旧世界 provider 二选一语义等价)。
/// Agent 引用即尝试,bin 探测留运行时(探测结果随装/卸 CLI 变化,不静态判定)。
fn active_refine_executor(s: &settings::Settings) -> Option<settings::ResolvedExecutor> {
    if !s.refine_enabled {
        return None;
    }
    if !settings::executor_ready(s, settings::AiFeature::Refine) {
        return None;
    }
    settings::resolve_executor(s, settings::AiFeature::Refine)
}

/// 旧判定的等价薄壳(标题生成等多处调用点沿用其名义语义)。
fn refine_llm_ready(s: &settings::Settings) -> bool {
    matches!(active_refine_executor(s), Some(settings::ResolvedExecutor::Http { .. }))
}

fn refine_agent_ready(s: &settings::Settings) -> bool {
    matches!(active_refine_executor(s), Some(settings::ResolvedExecutor::Agent { .. }))
}

/// 遥测分类的适配(classify 的旧签名是 provider+base_url 字符串对)。
fn telemetry_provider(e: &settings::ResolvedExecutor) -> telemetry::Provider {
    match e {
        settings::ResolvedExecutor::Agent { .. } => telemetry::Provider::classify("agent", ""),
        settings::ResolvedExecutor::Http { base_url, .. } => {
            telemetry::Provider::classify("openai", base_url)
        }
    }
}

/// HTTP Aing 的提交→索引交接边界：note 写失败时绝不请求 rebuild；note 已写成功而
/// scheduler 请求失败时不回滚人读真值，并把返回语义明确标成「已保存、待重试」。
fn handoff_http_refine_write(
    write_result: anyhow::Result<()>,
    request_rebuild: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    write_result?;
    request_rebuild().map_err(|error| {
        anyhow::anyhow!("Aing 已保存，但语义索引排队失败，索引待重试（将自动重试）: {error:#}")
    })
}

// resume_blocked_by_refining 纯函数已删:Aing 集入 lifecycle 内核(machine::RefineState),
// 守卫仍在 do_resume_note_recording 原位判定(顺序不变:下载→Aing→模型),判定值
// 由 actor 执行 Delegate 时从内核 Aing 集读出传入(见该函数 refining 参数注释)。

/// 会后 Aing：后台线程跑 filter+recluster（读 WAV）→ 视 `enqueue_transcode_after_local`
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
/// 全局 Aing 并发闸(限 1 篇串行)。每篇 Aing 都会起一整套 onnxruntime 线程池(重聚类
/// 嵌入)+ 本地重活;多篇并行各起一套 ORT 池互相抢核——在多核机上吵、低配机上直接卡死
/// (点 N 篇 = N 套完整管线)。串行既把 CPU/RAM 钉死上限,又通常更快(ORT 本身已跨核并行,
/// 叠第二套只增争用)。内核守卫只拦「同 note_id 重复 Aing」,跨笔记无闸,故在此加全局串行。
/// 需放宽到 N 并行,把此 Mutex 换成计数信号量即可。
static AING_GATE: Mutex<()> = Mutex::new(());

/// 重跑起跑仪式(codex 十二轮):上一轮的运行元数据(收工/写盘戳)先归档进
/// aing_runs.jsonl 再撕掉 finished_at——停摆取证靠的就是这些,不能裸清。
/// 归档发生在 update 闭包内,天然处于 NoteLock 保护下。无旧稿时 update 失败即无事。
fn archive_and_clear_finish_stamp(dir: &std::path::Path) -> anyhow::Result<()> {
    let mut last_err = None;
    // archived_at 在重试圈外定死(codex 十八轮):归档成功而后续整写失败时,下一轮
    // 重试生成的行与已写入的字节相同,末行比对即可去重,一次运行不会记成好几次。
    let archived_at = chrono::Local::now().to_rfc3339();
    for _ in 0..5 {
        match store::update_refined_for_retry(dir, |d| {
        if !d.finished_at.is_empty() || !d.written_at.is_empty() {
            let rec = serde_json::json!({
                "finished_at": d.finished_at,
                "written_at": d.written_at,
                "writer_pid": d.writer_pid,
                // 上一轮的运行标识随档保留(codex 三十二轮):新一轮整写会把稿面
                // writer_run 顶掉,归档不带它,稿与终态日志就再对不上号了。
                "writer_run": d.writer_run,
                "generated_at": d.generated_at,
                "archived_at": archived_at,
            });
            // 归档失败必须中止(codex 十四轮):闭包报错则 update 不落盘,收工戳
            // 原样保留——绝不能戳清了、档没归,把要保全的证据反手清掉。
            // 幂等去重按上一轮的四个稳定字段(codex 十八/十九轮),不含 archived_at:
            // 进程在「归档已写、撕戳未落」之间死掉,下次启动 archived_at 必然不同,
            // 按整行比对会把同一次运行再归一遍;按源字段比对跨重启也认得。
            let path = dir.join("aing_runs.jsonl");
            let rec_line = rec.to_string();
            // 扫全文件而非只看末行(codex 二十二轮):归档后整写失败会再插一条
            // failed_before_start,末行比对就认不出已归档过的同一轮了。文件小,全扫无妨。
            let dup = std::fs::read_to_string(&path)
                .ok()
                .map(|raw| {
                    raw.lines()
                        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                        .any(|v| {
                            v["finished_at"] == rec["finished_at"]
                                && v["written_at"] == rec["written_at"]
                                && v["writer_pid"] == rec["writer_pid"]
                                && v["writer_run"] == rec["writer_run"]
                                && v["generated_at"] == rec["generated_at"]
                        })
                })
                .unwrap_or(false);
            if !dup {
                // 与 append_refine_run_log 同款互斥+整行单写(codex 二十三轮)
                let _g = REFINE_RUN_LOG_LOCK
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)?;
                use std::io::Write as _;
                f.write_all(format!("{rec_line}\n").as_bytes())?;
            }
        }
            d.finished_at = String::new();
            Ok(())
        }) {
            Ok(()) => return Ok(()),
            // 「不存在或已损坏」要分家(codex 十五轮):盘上确实没有稿(首跑)才等价
            // 成功;文件在但解析不了是坏稿,必须拦住重跑,别把取证材料整写覆盖掉。
            Err(e) if e.to_string().contains("不存在") => {
                if store::aing_exists(dir) {
                    return Err(anyhow::anyhow!("盘上稿存在但已损坏,拒绝开跑以保全证据: {e}"));
                }
                return Ok(());
            }
            // 锁被短暂占用等瞬态:重试几轮再定失败(codex 十三轮 P2:失败必须拦住
            // 重跑,否则旧 finished_at 仍在,本轮停摆会被自愈误判为已收工)
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("清收工戳未知失败")))
}

/// runs 日志写入互斥(codex 二十三轮):writeln! 对 Value 是多次小写,并发 writer
/// (前任收工 vs 部分重试收工)可能把两行绞在一起毁掉 JSONL。进程内上锁 + 整行
/// 一次 write_all(O_APPEND 单次写原子);跨进程仍靠单次写兜底。
static REFINE_RUN_LOG_LOCK: Mutex<()> = Mutex::new(());

/// aing_runs.jsonl 追加一条运行事件。失败出声并返回 Err,成败由调用方决定要不要
/// 因此放弃后续动作(收工戳与成败日志的先后契约见 stamp_refine_finished)。
pub(crate) fn append_refine_run_log(
    dir: &std::path::Path,
    note_id: &str,
    rec: &serde_json::Value,
) -> std::io::Result<()> {
    let _g = REFINE_RUN_LOG_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let line = format!("{rec}\n");
    let r = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("aing_runs.jsonl"))
        .and_then(|mut f| {
            use std::io::Write as _;
            f.write_all(line.as_bytes())
        });
    if let Err(e) = &r {
        eprintln!("refine({note_id}): 运行日志写入失败: {e}");
    }
    r
}

/// 收工戳落盘(codex 十六轮):worker 有序退场时调用。
/// - outcome 一并记进 aing_runs.jsonl:重跑失败在 run_local 写盘之前时,盘上还是
///   上一轮的 llm=done 旧稿,光有 finished_at 会被读成「新近成功收工」——成败要
///   有单独的落盘证据。
/// - 落戳带重试且失败出声:静默丢戳会让自愈把下一次停摆误判成「尾段停摆」。
fn stamp_refine_finished(dir: &std::path::Path, note_id: &str, outcome: &str, my_gen: u64) {
    let at = chrono::Local::now().to_rfc3339();
    // 代次守卫(codex 二十一轮):被判停摆的前任若在替补起跑后才苏醒走到这里,
    // 盘上已是替补的中间稿——前任来盖 finished_at 会让替补此后的停摆被误判为
    // 已收工。心跳表在座代次比我新即让位:成败照记日志(标 superseded),稿不动。
    // (跨进程的双实例场景此守卫不覆盖,写盘仍由 NoteLock 串行,属已知边界。)
    if refine_beat_owner(note_id).is_some_and(|g| g > my_gen) {
        let rec = serde_json::json!({
            "event": "finished", "outcome": outcome, "at": at, "superseded": true,
        });
        let _ = append_refine_run_log(dir, note_id, &rec);
        eprintln!("refine({note_id}): 替补已在跑,前任收工戳弃盖(成败已记日志)");
        return;
    }
    // 先落成败日志,后盖收工戳(codex 二十轮):反过来的话,盖完戳、日志没写成
    // (或进程恰好死在中间),旧 llm=done 稿+新戳会被读成「新近成功收工」——这
    // 正是日志要消灭的歧义。日志写不进就不盖戳,保持「无戳=没收工」的保守可读。
    let rec = serde_json::json!({
        "event": "finished", "outcome": outcome, "at": at,
        "run": format!("{}-{my_gen}", std::process::id()),
    });
    if append_refine_run_log(dir, note_id, &rec).is_err() {
        eprintln!("refine({note_id}): 成败日志未落,收工戳弃盖(保守:无戳读作未收工)");
        return;
    }
    // 尾段停摆调解(codex 三十五轮):自愈曾把 llm 从终态改标 failed,而 identify/
    // 标题尾段不再写盘——worker 诈尸跑完以 done 收工时,稿面还挂着 failed,与
    // 事件/last_run 永久矛盾。从 runs 日志找出自愈记录里的原值,收工时调解回去。
    let heal_prev: Option<String> = if matches!(outcome, "done" | "retry_done") {
        std::fs::read_to_string(dir.join("aing_runs.jsonl"))
            .ok()
            .and_then(|raw| {
                // rec 刚追加,倒数第二条起往回找最近的 event 记录
                let last = raw
                    .lines()
                    .rev()
                    .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                    .filter(|v| v.get("event").is_some())
                    .nth(1)?;
                if last["outcome"] != "stale_heal_failed" {
                    return None;
                }
                let detail = last["detail"].as_str()?;
                if !detail.contains("尾段停摆") {
                    return None;
                }
                let prev = detail.split("(原 ").nth(1)?.trim_end_matches(')');
                Some(prev.to_string())
            })
    } else {
        None
    };
    let mut stamped = false;
    let mut last_err = None;
    for _ in 0..5 {
        match store::update_refined_for_retry(dir, |d| {
            // 锁内复验(codex 二十二轮):每轮起跑仪式都会清空 finished_at,此刻
            // 稿上已有戳 = 我离场后另一轮已完整收工(替补收工连心跳都清了,光看
            // 心跳表查不出)。前任不得覆盖,弃盖中止本次 update。
            if !d.finished_at.is_empty() {
                anyhow::bail!("稿上已有更新的收工戳,弃盖");
            }
            // 提交时再验代次(codex 二十八轮):替补在外层检查之后才起跑(部分重试
            // 不过 AING_GATE)时,它的仪式已清过戳,上一条查不出;但它起跑前必先
            // 同步注册更高代次心跳,此处能看见。
            if refine_beat_owner(note_id).is_some_and(|g| g > my_gen) {
                anyhow::bail!("替补已接手,弃盖");
            }
            if let Some(prev) = &heal_prev {
                if d.stages.llm == "failed" {
                    d.stages.llm = prev.clone(); // 调解:自愈的 failed 让位于真收工
                }
            }
            d.finished_at = at.clone();
            Ok(())
        }) {
            Ok(()) => {
                stamped = true;
                break;
            }
            Err(e) if e.to_string().contains("弃盖") => {
                let rec = serde_json::json!({
                    "event": "finished", "outcome": outcome, "at": at, "superseded": true,
                });
                let _ = append_refine_run_log(dir, note_id, &rec);
                eprintln!("refine({note_id}): 稿上已有更新收工戳,前任弃盖(成败已记日志)");
                return;
            }
            // 盘上无稿(run_local 前就失败):没有可盖戳的稿,run 日志照记
            Err(e) if e.to_string().contains("不存在") && !store::aing_exists(dir) => break,
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
    }
    if !stamped {
        if let Some(e) = last_err {
            eprintln!(
                "refine({note_id}): 收工戳落盘失败(自愈可能把下次停摆误判为尾段停摆): {e}"
            );
        }
    }
}

fn spawn_refine(app: tauri::AppHandle, note_id: String, enqueue_transcode_after_local: bool) {
    let state: tauri::State<AppState> = app.state();
    let transcode = state.transcode.clone();
    let graph_scheduler = state.graph_scheduler.clone();
    let session = state.session.clone();
    let lc = app.state::<lifecycle::LifecycleHandle>().inner().clone();
    // Aing 态置 Running 的信号(原 refining.insert 的时机)同步先行——必须在 spawn
    // 线程之前发出:自动路径(DoFinalize 直调)在 actor 线程上执行,这条自投消息
    // 排在停录 reply 之前入队,停录返回后到达的续录命令必然在它后面,内核守卫才
    // 不会因 worker 线程起步慢而漏挡(与旧世界入口同步 insert 的窗口对齐)。
    // 它同时就是旧 worker 的第一条 emit("all","running"),事件序列起点不变。
    // 心跳注册必须先于 all/running 入队(codex 三十三轮):前任退场围栏持心跳锁
    // 做「查代次+发 RefineFinished」,只有保证「注册在前、入队在后」,围栏读不到
    // 新代次才能推出替补的 running 还没入队,FIFO 上前任的 Finished 必然排在它
    // 前面,不会反过来把刚起跑的替补摘掉。
    // (也兼收 codex P2a:run_local 阶段 refine_status 即有 beat 可探。)
    let beat_gen = refine_beat_gen_next(); // 本 worker 的心跳代次(codex 四轮)
    refine_report_fenced(&lc, &note_id, beat_gen, "all", "running");
    // Fix 2(codex 第三轮,A 侧互查闭环——两处必须同步改,另一侧见 do_retranscribe
    // 里 `is_refining(id)` 占槽后复查处的同款注释)。
    //
    // 修正设计初稿的一个错误前提:LifecycleHandle::report 不是同步调用——它的文档
    // 明写"只投递不等待"(actor.rs 62-63 行),tx.send() 入队即返回,不等 actor 线程
    // 真正把 note_id 插进 Aing 集。所以不能说"上面这条 report 一返回,写就已完成"。
    // 但互斥证明不需要这个(错误的)前提,靠 actor 信箱本身的 FIFO 单消费者顺序
    // 就够:report() 与 is_refining() 共用同一个 tx channel,actor 严格按消息入队
    // 顺序串行处理(actor.rs run 循环逐条 recv,QueryRefine 也在同一循环里直答)。
    // 于是只需要「入队先后序」而不需要「处理完成」这个更强的前提：
    //   A 线程:先 send(RefineProgress "all/running")入队,程序序上紧接着才执行
    //     到这里读 retranscribing 槽——send 调用发生在这次读之前(实时上)。
    //   R 线程(do_retranscribe):先写 retranscribing 槽(`*slot = Some(...)`),
    //     之后才调用 is_refining(id)(即 send(QueryRefine)入队并阻塞等回执)。
    // 反证:若 A 与 R"双穿"（A 读槽时空、R 的 is_refining 读到 false)同时成立，
    // 由 A 侧读槽早于 R 写槽(A 才会读到空)可推出 A 的 send 早于 R 的 send（实时上，
    // 见上面两条程序序链:A_send < A_read < R_write < R_send)；而 A 的 send 更早
    // 入队，按 FIFO,actor 必先处理 A 的插入、再处理 R 的查询，R 的 is_refining
    // 就不可能读到 false——矛盾。故双穿不可能发生：R 先写必被 A 看见（A 让步不
    // spawn 线程),A 先"送单"必被 R 的复查看见（R 让步清槽拒绝)。
    if retranscribing_blocks_refine(&state.retranscribing, &note_id)
        || mixed_regen_blocks_refine(&state.mixed_regen, &note_id)
    {
        eprintln!(
            "refine({note_id}): 与重转写/补生成占槽发生竞态,放弃本次 Aing(见 spawn_refine Fix 2 注释)"
        );
        // 把上面刚插入的 Aing 集清干净,与 worker 正常收尾同款消息序(先 all/failed
        // 再 RefineFinished),前端/轮询方看到的事件形状不因走了这条放弃路径而变化。
        lc.report(lifecycle::machine::Msg::RefineProgress {
            note_id: note_id.clone(),
            stage: "all".into(),
            state: "failed".into(),
        });
        refine_beat_clear(&note_id, beat_gen); // 上面刚起的心跳同步清掉,不留无主条目
        lc.report(lifecycle::machine::Msg::RefineFinished { note_id });
        return;
    }
    std::thread::spawn(move || {
        // Aing 集条目的移除交给 RAII(见 RefineDoneOnDrop):线程无论怎么结束都必然移除,
        // 不再依赖"执行流一定走到末尾那一行"。最先声明 ⇒ 最后 drop,时机不变。
        let _refine_done = RefineDoneOnDrop { lc: lc.clone(), note_id: note_id.clone(), beat_gen };
        set_current_refine_run(beat_gen); // 本线程写盘一律以本代次署名(codex 三十三轮)
        // F1 修复(b):若此刻活跃会话正是本 note_id,说明 resume 已经抢在 Aing 完成前重开
        // 录制、正在向 mic.wav 追加写——此刻 enqueue 会让转码 worker 编码+删除一份正在
        // 被写入的 WAV,续录段音频永久丢失。锁只取 note_id 立即释放,不跨 enqueue 调用
        // 持有。跳过不等于丢转码:续录自身在其最终停止时会重新走一遍 Aing+转码移交。
        let is_resumed_by_active_session = |note_id: &str| -> bool {
            session.lock().unwrap().as_ref().map(|s| s.note_id == note_id).unwrap_or(false)
        };
        // 原 emit("refine",..) 改 report 进 lifecycle 信箱:同一 worker 串行 report +
        // 信箱 FIFO,actor 的 DoEmitRefine 以同种类/载荷/顺序对外发事件,逐位不变。
        let report = |stage: &str, st: &str| {
            // 代次围栏+心跳+入队三合一持锁(codex 二十五/三十四轮)
            refine_report_fenced(&lc, &note_id, beat_gen, stage, st);
        };
        let enqueued = std::cell::Cell::new(false);
        // 全局串行闸:在起 ORT 线程池的重活之前排队,同一时刻只放一篇过。守卫在 catch_unwind
        // 之前取、随线程体自然释放——被捕获的 panic 不经此守卫展开,不会毒化(仍加 poison 兜底)。
        // 多篇同时点会各自先发一条 "all/running"(显示「Aing 中…」),但实际串行等锁逐篇跑。
        // 排队要发心跳(codex P1,预存缺陷被 #175 退避拓宽后收口):滞留自愈按
        // RefineProgress 刷时钟,而排队者在 lock() 里睡死、一小时无声就被误杀出
        // 运行集——之后真开工时集合不再插入,编辑守卫/重复 Aing 全部失效。改
        // try_lock 轮询,每分钟补一条 all/running 心跳。代价是失去 Mutex 的排队
        // 公平性(多篇并发排队时唤醒顺序随机),Aing 并发本就罕见,可接受。
        let _aing_gate = {
            // 2s 一试拿门,60s 一报心跳(codex 二十四轮):照旧一分钟一试的话,
            // 前一篇一放门,排队的这篇还要干等最多一分钟才接上。
            let mut ticks: u32 = 0;
            loop {
                match AING_GATE.try_lock() {
                    Ok(g) => break g,
                    Err(std::sync::TryLockError::Poisoned(p)) => break p.into_inner(),
                    Err(std::sync::TryLockError::WouldBlock) => {
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        ticks += 1;
                        if ticks % 30 == 0 {
                            report("all", "running");
                        }
                    }
                }
            }
        };
        let result: std::thread::Result<anyhow::Result<()>> =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // (第一条 "all/running" 已在 spawn 前由入口同步发出,见上)
                // local_cloud 云端二遍:读 note 之前先用云端批式对整场重转写(实时
                // 本地快稿 + Aing 前云端精修覆盖 segments,2026-08-11 用户拍板)。
                // 放在 AING_GATE 内、refine 读盘之前:与本 worker 天然串行,后续
                // filter/recluster 读到的就是二遍后的文本;NoteLock 在
                // run_retranscribe_once 内取放,与 run_local 的取锁先后互不重叠。
                // 仅首次停录自动路径(enqueue_transcode_after_local)做——手动重跑
                // Aing 时 segments 可能已被用户编辑,悄悄重转写会冲掉人工修订。
                // 任何失败只降级保留实时稿,绝不挡 Aing;续录已重开则跳过(与转码
                // 入队的 F1 守卫同因:音频正被追加写)。
                if enqueue_transcode_after_local {
                    let s2 = app.path().app_data_dir().map(|d| settings::load(&d)).unwrap_or_default();
                    // 续录判别(codex 2026-08-11 P1):enqueue_transcode_after_local 在
                    // 「续录后再次停止」时同样为 true,而重转写会整篇覆盖 segments,冲掉
                    // 用户在续录前做过的文字编辑。判据用盘上 refined.json 的在场性:
                    // 每次停录的自动 Aing 都会写它,故「已有 refined.json」⇔ 本篇经历过
                    // 至少一次完整停录 ⇔ 这是续录收尾;首次停录时它必然还不存在
                    // (云端二遍先于本轮 refine 读盘执行)。录制期间持 NoteLock,编辑
                    // 不可能发生在首次停录完成之前,所以首停做二遍恒安全。
                    let resumed_note = notes_dir(&app)
                        .ok()
                        .map(|root| store::load_refined(&root.join(&note_id)).is_some())
                        .unwrap_or(true); // 判别不了按"续录"保守处理:宁可不精修,不冒覆盖编辑的险
                    if settings::cloud_second_pass_wanted(&s2) {
                        if resumed_note {
                            eprintln!("refine({note_id}): 本篇已有历史停录(或判别失败),跳过云端二遍以保护既有编辑");
                        } else if is_resumed_by_active_session(&note_id) {
                            eprintln!("refine({note_id}): 续录已重开,跳过云端二遍");
                        } else {
                            report("cloud_pass", "running");
                            // 停录收尾的 writer 锁在 spawn_refine 返回后才释放,与本
                            // worker 存在毫秒级竞窗;NoteLock 自身只重试 ~100ms,不够
                            // 覆盖调度抖动(codex P2)——这里再包一层有界重试(~3s),
                            // 「笔记正被占用」以外的错误立刻放弃不重试。
                            let mut outcome = Err(String::new());
                            // 伴生刷跳线程(codex 二十七轮):进度回调只在 decode/
                            // transcribe/attribute/commit 四个阶段边界响,云端转写
                            // 单阶段就能超一小时——不持续打点会被定时体检误杀
                            // (写失败态+放行编辑,而 worker 还在跑)。每分钟代跳
                            // 一次,二遍结束落旗自停。
                            let sp_alive =
                                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
                            // 落旗交 RAII(codex 二十八轮):二遍代码 panic 展开时手动
                            // store 走不到,刷跳线程会拿着这篇的心跳永远跳下去。
                            struct SpFlagDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);
                            impl Drop for SpFlagDrop {
                                fn drop(&mut self) {
                                    self.0.store(false, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                            let _sp_guard = SpFlagDrop(sp_alive.clone());
                            let _sp_refresher = {
                                let alive = sp_alive.clone();
                                let nid = note_id.clone();
                                std::thread::spawn(move || {
                                    // 保活设硬上限(codex 二十九轮):无进度信号的盲打点
                                    // 若无限续命,二遍真吊死时体检永远看不见。4 小时
                                    // 远超实测最长二遍(3.5h 会议约 70min),超限停跳,
                                    // 停摆监工随后接管。
                                    for i in 0.. {
                                        if !alive.load(std::sync::atomic::Ordering::Relaxed) {
                                            break;
                                        }
                                        std::thread::sleep(std::time::Duration::from_secs(60));
                                        if !alive.load(std::sync::atomic::Ordering::Relaxed) {
                                            break;
                                        }
                                        if i >= 240 {
                                            eprintln!(
                                                "refine({nid}): 二遍保活达 4h 上限,停止代跳(若真吊死,停摆监工将接管)"
                                            );
                                            break;
                                        }
                                        refine_beat_touch(&nid, beat_gen, "second_pass", "running");
                                    }
                                })
                            };
                            for attempt in 0..10 {
                                // strict=true:任一段失败整体放弃(见 retranscribe::run 注释)。
                                outcome = run_retranscribe_once(&app, &note_id, false, s2.language_filter, true, None, &mut |_| {
                                    // 二遍的解码/转写/归属进度喂进心跳表(codex 十五轮):
                                    // 长会议二遍可超一小时,不打点会被定时体检误杀
                                    refine_beat_touch(&note_id, beat_gen, "second_pass", "running");
                                });
                                match &outcome {
                                    Err(e) if e.contains("正被占用") || e.contains("busy") => {
                                        if attempt < 9 {
                                            std::thread::sleep(std::time::Duration::from_millis(300));
                                            continue;
                                        }
                                    }
                                    _ => {}
                                }
                                break;
                            }
                            // 二遍收尾落旗,刷跳线程最多再睡一觉便自停
                            sp_alive.store(false, std::sync::atomic::Ordering::Relaxed);
                            match outcome {
                                Ok(sum) => {
                                    eprintln!("云端二遍完成({note_id}): {sum:?}");
                                    report("cloud_pass", "ok");
                                }
                                Err(e) => {
                                    eprintln!("云端二遍失败({note_id}),保留实时稿: {e}");
                                    report("cloud_pass", "failed");
                                }
                            }
                        }
                    }
                }
                let root = notes_dir(&app)?;
                let dir = root.join(&note_id);
                // 新一轮起跑:归档上一轮证据并撕收工戳(codex 十一/十二轮)——
                // 停摆时自愈不能被旧 finished_at 骗成「已收工」,而旧戳本身是取证
                // 材料,清之前先落 aing_runs.jsonl。
                archive_and_clear_finish_stamp(&dir)
                    .map_err(|e| anyhow::anyhow!("重跑起跑仪式失败(旧收工戳未清,拒绝开跑): {e}"))?;
                // 与 get_note 同款只读加载：全部 segments（已按 get_note 语义过滤空白 +
                // 排序）+ speakers 表。
                let note = store::NoteStore::new(root).load(&note_id)?;
                // 标签、权重、种子门禁三者必须出自同一次设置读取(codex review 实现轮五 P1)。
                let speaker_tag = current_speaker_model(&app);
                let mut embedder = match diar::SherpaEmbedder::new(&speaker_model_path_for(&speaker_tag)) {
                    Ok(e) => Some(e),
                    Err(e) => {
                        eprintln!("refine: 声纹模型不可用，跳过重聚类: {e}");
                        None
                    }
                };
                let seeds = load_voiceprint_seeds_for(&app, &speaker_tag);
                let (mut doc, cluster_stats) = refine::run_local(
                    &dir,
                    &note.segments,
                    embedder.as_mut().map(|e| e as &mut dyn diar::SpeakerEmbedder),
                    &seeds,
                    &chrono::Local::now().to_rfc3339(),
                    &current_speaker_match(&app),
                    &speaker_tag,
                )?;
                report("filter", &doc.stages.filter);
                report("recluster", &doc.stages.recluster);
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
                // AI 日志上下文:所有对外 AI 调用(HTTP/Agent/标题)全量留痕。
                // data_root 拿不到时降级为不记录,绝不影响 Aing 本身。
                let log_ctx = data_root(&app)
                    .ok()
                    .map(|root| ailog::Ctx { data_root: root, note_id: note_id.clone() });
                // P3 日历匹配:先于 identify 执行(参会人闭集先验要进 ctx);
                // 失败/未授权/开关关都只收窄,不影响精修。
                if let Err(e) = match_and_store_calendar(&app, &note_id) {
                    eprintln!("calendar({note_id}): 匹配失败(不影响笔记): {e}");
                }
                let mut http_refine_handled = false;
                let refine_exec = active_refine_executor(&s);
                if let Some(settings::ResolvedExecutor::Agent { kind, bin, model }) = &refine_exec {
                    telemetry::track(
                        &app,
                        telemetry::Event::NoteRefined { provider: telemetry_provider(refine_exec.as_ref().unwrap()) },
                    );
                    report("llm", "running");
                    let resolved = refine::agent::AgentKind::from_key(kind)
                        .and_then(|k| refine::agent::resolve_bin(k, bin).map(|b| (k, b)));
                    match resolved {
                        Some((kind, bin)) => {
                            if let Err(e) = refine::agent::run_refine(
                                &dir,
                                &note_id,
                                kind,
                                &bin,
                                model,
                                log_ctx.as_ref(),
                            ) {
                                eprintln!("refine: agent Aing 失败: {e}");
                                telemetry::report_error(
                                    telemetry::ErrorKind::AiPipeline,
                                    &format!("agent Aing 失败: {e}"),
                                );
                            }
                            // Agent 经 MCP 写的是盘上文件:重载同步内存 doc(成功时
                            // llm=done + 修订文本;失败时盘上仍是 off,下面统一降级)。
                            if let Some(d) = store::load_refined(&dir) {
                                doc = d;
                            }
                        }
                        None => eprintln!(
                            "refine: 未找到 {kind} 的 CLI(可在 AI 页指定可执行文件路径),Agent Aing 跳过"
                        ),
                    }
                    // 与 run_llm 的 F4 同一语义:本轮没落成 done 就是 failed,盘上与
                    // 事件保持一致,不把「off」留给 UI 当作"没配置"误读。
                    if doc.stages.llm != "done" {
                        doc.stages.llm = "failed".into();
                        if let Err(e) = store::write_refined_atomic(&dir, &doc) {
                            eprintln!("refine: agent 失败态落盘失败: {e}");
                            // 算出来了但没存住,和根本没算出来是两回事——后者已有
                            // AiPipeline,前者此前完全不可见。
                            telemetry::report_error(
                                telemetry::ErrorKind::AiApplyWrite,
                                &format!("agent 失败态落盘失败: {e}"),
                            );
                        }
                    }
                } else if let Some(settings::ResolvedExecutor::Http { base_url, model, api_key }) = &refine_exec {
                    http_refine_handled = true;
                    telemetry::track(
                        &app,
                        telemetry::Event::NoteRefined { provider: telemetry_provider(refine_exec.as_ref().unwrap()) },
                    );
                    report("llm", "running");
                    let cfg = refine::llm::LlmConfig {
                        base_url: base_url.clone(),
                        model: model.clone(),
                        api_key: api_key.clone(),
                    };
                    let prompt_labels = {
                        let vp_now = store::VoiceprintStore::new(
                            data_root(&app).map_err(anyhow::Error::msg)?,
                        )
                        .load();
                        speaker_prompt_labels(&note.speakers, &vp_now)
                    };
                    let write_result = refine::run_llm(
                        &dir,
                        &mut doc,
                        &cfg,
                        model,
                        &prompt_labels,
                        log_ctx.as_ref(),
                        // 逐块进度:①重发 llm/running(滞留自愈据此判断 worker 活着,
                        // 见 lifecycle/actor.rs 的 REFINE_STALE_MS;事件幂等,内核只在
                        // stage=="all"&&state=="running" 时动集合) ②emit aing_progress
                        // 给界面画「精修中 done/total · 约剩 X 分」。
                        &|done, total, avg_ms| {
                            report("llm", "running");
                            let _ = app.emit(
                                "aing_progress",
                                AingProgress {
                                    note_id: note_id.clone(),
                                    stage: "llm".into(),
                                    done: done as u32,
                                    total: total as u32,
                                    avg_chunk_ms: avg_ms,
                                },
                            );
                        },
                    );
                    if let Err(error) = handoff_http_refine_write(write_result, || {
                        let root = data_root(&app)?;
                        let graph_events = app.clone();
                        graph_scheduler
                            .request(root, move |status| {
                                let _ = graph_events.emit("graph_index_status", status);
                            })
                            .map(|_| ())
                    }) {
                        eprintln!("refine: HTTP Aing 提交/索引交接: {error:#}");
                        telemetry::report_error(
                            telemetry::ErrorKind::AiApplyWrite,
                            &format!("HTTP Aing 提交/索引交接失败: {error:#}"),
                        );
                    }
                }
                report("llm", &doc.stages.llm);
                // identify(P2a 只读期):精修定稿后推断说话人身份,产物只落
                // identify.json + 收件箱建议卡,零自动写入。执行体分派内含
                // refine_llm_ready 门禁(用户关精修/agent provider → 静默跳过);
                // 失败仅留日志,绝不影响 Aing 结果。
                let identify_exec = match identify_executor(&s) {
                    Ok(e) => Some(e),
                    Err(reason) => {
                        eprintln!("identify({note_id}): 跳过——{reason}");
                        None
                    }
                };
                if let Some(identify_exec) = identify_exec {
                    // 心跳表专用(codex 七轮):identify/title 在末次 report("llm",..)
                    // 之后运行,不打点的话 refine_status 只见 llm/done 越来越陈旧,
                    // 健康长跑与真停摆无从区分。只碰 beat,不进 lifecycle 事件流。
                    refine_beat_touch(&note_id, beat_gen, "identify", "running");
                    let identify_result = (|| -> anyhow::Result<()> {
                        let vp = open_voiceprint_store(&app).map_err(anyhow::Error::msg)?.load();
                        let acoustic_enabled = vp.embedding_model == s.speaker_model;
                        // 日历快照在本线程开头已匹配落盘,此处重读最新 meta。
                        let cal = store::NoteStore::new(notes_dir(&app)?)
                            .load(&note_id)
                            .ok()
                            .and_then(|n| n.meta.calendar);
                        let now = chrono::Local::now().to_rfc3339();
                        // 未完成 op 无条件先恢复(不依赖开关/推断成败):崩溃在
                        // assign 后时,不先前滚,新一轮推断会因簇已关联吞掉条目。
                        recover_identify_ops(&app, &note_id);
                        // identify.json 读改写全程收进 IDENTIFY_ACT_GATE(与
                        // apply/reject/自动应用同门,消并发覆盖)。
                        let idoc = {
                            let _gate = IDENTIFY_ACT_GATE.lock().unwrap();
                            let idoc = refine::identify::run_identify(
                                &dir,
                                &note_id,
                                &doc,
                                &note.speakers,
                                &cluster_stats,
                                &vp,
                                acoustic_enabled,
                                cal.as_ref(),
                                identify_exec.as_ref(),
                                log_ctx.as_ref(),
                                &now,
                            )?;
                            refine::identify::save_identify(&dir, &idoc)?;
                            idoc
                        };
                        // P2b 自动应用(默认关):恢复已在 run_identify 前无条件
                        // 执行;此后 idoc 已是旧副本,绝不再保存它。
                        if s.identify_auto_apply {
                            let fps: Vec<String> = store::load_refined(&dir)
                                .map(|d| {
                                    refine::identify::auto_apply_targets(&idoc, &d, &note.speakers)
                                        .iter()
                                        .map(|a| a.fingerprint.clone())
                                        .collect()
                                })
                                .unwrap_or_default();
                            for fp in fps {
                                if let Err(e) = auto_apply_one(&app, &note_id, &fp) {
                                    eprintln!(
                                        "identify({note_id}): 自动应用 {fp} 未执行(留建议卡): {e}"
                                    );
                                }
                            }
                        }
                        let _ = app.emit("identify_done", note_id.clone());
                        Ok(())
                    })();
                    if let Err(e) = identify_result {
                        eprintln!("identify({note_id}): 推断失败(不影响精修): {e}");
                    }
                }
                // 图谱是纯增值产物:成功 Aing 只把全量重建标脏。scheduler 合并并发请求，
                // 从 ledger + 全部 aing.json 取快照后原子替换；失败保留旧库且不打断 Aing。
                if !http_refine_handled && doc.stages.llm == "done" {
                    match data_root(&app) {
                        Ok(root) => {
                            let graph_events = app.clone();
                            if let Err(error) = graph_scheduler.request(root, move |status| {
                                let _ = graph_events.emit("graph_index_status", status);
                            }) {
                                eprintln!("graph: Aing 后索引排队失败，已保留重试标记: {error:#}");
                            }
                        }
                        Err(e) => eprintln!("graph: data_root 不可用,跳过入图: {e}"),
                    }
                }
                // 主题标题:LLM 阶段产出可用(done/partial 都行,标题只要大意)且标题
                // 仍是默认样式(用户没手动改过)才自动替换——手动命名永远最高优先级。
                // 失败静默:标题是锦上添花,不影响 Aing 完成态。
                // 主题标题:只要 AI 执行体就绪且标题仍是默认样式就尝试——不再要求
                // LLM Aing 阶段成功(标题是独立的小调用,Aing 分块失败不代表标题也会
                // 失败;llm 失败时段落是原文,起标题足够)。手动命名永远最高优先级,
                // 失败静默保默认名。
                if refine_exec.is_some() && store::writer::is_default_title(&note.meta.title) {
                    refine_beat_touch(&note_id, beat_gen, "title", "running"); // 心跳表打点,同 identify
                    // 标题跟随 Aing 执行体:Agent 模式一发一收(无 MCP、无工具),
                    // HTTP 模式走原 chat completions。两边同一长度守卫、同样失败即放弃。
                    let title = match refine_exec.as_ref().unwrap() {
                        settings::ResolvedExecutor::Agent { kind, bin, model } => {
                            refine::agent::AgentKind::from_key(kind)
                                .and_then(|k| refine::agent::resolve_bin(k, bin).map(|b| (k, b)))
                                .ok_or_else(|| anyhow::anyhow!("Agent CLI 不可用"))
                                .and_then(|(kind, bin)| {
                                    refine::agent::gen_title(
                                        kind,
                                        &bin,
                                        model,
                                        &doc.paragraphs,
                                        log_ctx.as_ref(),
                                    )
                                })
                        }
                        settings::ResolvedExecutor::Http { base_url, model, api_key } => {
                            let cfg = refine::llm::LlmConfig {
                                base_url: base_url.clone(),
                                model: model.clone(),
                                api_key: api_key.clone(),
                            };
                            refine::llm::gen_title(&cfg, &doc.paragraphs, log_ctx.as_ref())
                        }
                    };
                    match title {
                        Ok(title) => {
                            match store::NoteStore::new(notes_dir(&app)?).rename(&note_id, &title) {
                                Ok(()) => {
                                    let _ = app.emit(
                                        "note_renamed",
                                        ipc::NoteRenamedEvent {
                                            note_id: note_id.clone(),
                                            title,
                                        },
                                    );
                                }
                                Err(e) => eprintln!("refine({note_id}): 主题标题落盘失败: {e}"),
                            }
                        }
                        Err(e) => eprintln!("refine({note_id}): 主题标题生成失败(保留默认名): {e}"),
                    }
                }
                anyhow::Ok(())
            }));
        // 收工戳(issue #173 十/十六轮):llm 终态只证明 llm 阶段写过盘,identify/
        // 标题尾段吊死时稿面与真收工无从区分。有序走到终态上报(done/failed/panic
        // 被 catch 三臂都算"worker 有序退场")才盖戳,成败记进 runs 日志。
        let stamp_finished = |outcome: &str| {
            if let Ok(root) = notes_dir(&app) {
                stamp_refine_finished(&root.join(&note_id), &note_id, outcome, beat_gen);
            }
        };
        match &result {
            Ok(Ok(())) => {
                stamp_finished("done");
                report("all", "done");
            }
            Ok(Err(e)) => {
                eprintln!("refine({note_id}): 管线失败: {e}");
                if e.to_string().contains("起跑仪式失败") {
                    // 证据保全优先(codex 十九轮):归档没写成,盘上旧收工戳必须原样
                    // 保留——盖戳会把「上一轮何时收工」的唯一记录冲掉。只记日志。
                    if let Ok(root) = notes_dir(&app) {
                        let _ = append_refine_run_log(
                            &root.join(&note_id),
                            &note_id,
                            &serde_json::json!({
                                "event": "finished",
                                "outcome": "failed_before_start",
                                "at": chrono::Local::now().to_rfc3339(),
                            }),
                        );
                    }
                } else {
                    stamp_finished("failed");
                }
                report("all", "failed");
            }
            Err(_) => {
                eprintln!("refine({note_id}): 管线 panic");
                // panic 已被 catch_unwind 吞掉,进程级 hook 仍会捕获它;
                // 这里补一条带链路 kind 的显式上报,便于按 fingerprint 分组与限流。
                telemetry::report_error(telemetry::ErrorKind::AiPipeline, "Aing 管线 panic");
                stamp_finished("panic");
                report("all", "failed");
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
        // 移出内核 Aing 集的动作由 _refine_done 的 Drop 完成(线程体结束时,即此处之后)。
        // 刻意不在这里再发一次:重复的 RefineFinished 会撞上 machine 的
        // "不在集合中"分支,平白产出 ShadowMismatch 对账噪音。
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

/// RAII:Aing 集条目必然移除。与上面 ResetOnDrop 同一教训的第二例——
/// RefineFinished 原先只手动写在 worker 线程体末尾,任何**提前退出**路径都会让
/// note_id 永久留在内核 Aing 集里,而 `is_refining` 是 delete_note_speaker 等命令的
/// 第一道守卫,后果是该笔记的删除说话人**永久失败,只能重启应用**
/// (2026-08-17 真实发生:用户点删除毫无反应,重启后同一操作立即成功)。
///
/// 守卫在线程体最先声明 ⇒ 最后 drop,时机与原先"收尾事件与兜底转码之后"一致。
/// 只覆盖**线程结束**(正常/panic/提前 return);线程若永久阻塞则 Drop 不会执行,
/// 那条路径由 actor 的滞留超时自愈兜底(见 lifecycle::actor 的 REFINE_STALE)。
struct RefineDoneOnDrop {
    lc: lifecycle::LifecycleHandle,
    note_id: String,
    beat_gen: u64,
}
impl Drop for RefineDoneOnDrop {
    fn drop(&mut self) {
        // 代次围栏(codex 二十五/三十三轮):整个「查代次+清心跳+发 RefineFinished」
        // 持心跳锁执行,与替补的注册(touch,同一把锁)互斥——查不到新代次即可断定
        // 替补的 all/running 尚未入队(注册先于入队,见 spawn_refine),本条 Finished
        // 在 FIFO 上必排它前面,不可能把刚起跑的替补移出 Aing 集。
        let mut g = REFINE_BEAT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let m = g.get_or_insert_with(Default::default);
        if m.get(&self.note_id).is_some_and(|(g0, _, _)| *g0 > self.beat_gen) {
            eprintln!(
                "refine({}): 前任 worker 退场,替补在跑,不发 RefineFinished",
                self.note_id
            );
            return;
        }
        if m.get(&self.note_id).is_some_and(|(g0, _, _)| *g0 == self.beat_gen) {
            m.remove(&self.note_id);
        }
        // 持锁发送:unbounded channel 不阻塞,actor 侧不回取本锁,无死锁环。
        self.lc.report(lifecycle::machine::Msg::RefineFinished { note_id: self.note_id.clone() });
    }
}

/// Aing 心跳表(issue #173):note_id → (最近一次 stage/state, 时刻)。由 spawn_refine
/// 的 report 咽喉更新、RefineDoneOnDrop 清理;refine_status(UDS/MCP)对外暴露——
/// 外部工具从此能区分「在跑(心跳新鲜)/收工(无条目)/真停摆(心跳陈旧)」,不再靠
/// 猜文件名与取样验尸(2026-08-26 事故:一次误诊链耗掉两小时)。
static REFINE_BEAT: Mutex<
    Option<std::collections::HashMap<String, (u64, String, std::time::Instant)>>,
> = Mutex::new(None);
/// worker 代次发号器:替补 worker 的心跳不被前任的 RAII 清理误伤(codex 四轮)。
static REFINE_BEAT_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn refine_beat_gen_next() -> u64 {
    REFINE_BEAT_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

/// 持心跳锁的「查代次+记心跳+入队上报」三合一(codex 三十四轮):与替补的注册
/// 互斥,诈尸前任的终态事件不可能在替补 all/running 之后入队(FIFO 论证同
/// RefineDoneOnDrop)。lifecycle 事件一律走这里;纯打点(identify/标题/二遍)仍用
/// refine_beat_touch。
fn refine_report_fenced(
    lc: &lifecycle::LifecycleHandle,
    note_id: &str,
    my_gen: u64,
    stage: &str,
    state_s: &str,
) {
    let mut g = REFINE_BEAT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let m = g.get_or_insert_with(Default::default);
    if m.get(note_id).is_some_and(|(g0, _, _)| *g0 > my_gen) {
        return; // 替补已注册:前任连进度都不该再报
    }
    m.insert(
        note_id.to_string(),
        (my_gen, format!("{stage}/{state_s}"), std::time::Instant::now()),
    );
    // 持锁入队:unbounded 不阻塞,actor 不回取本锁,无死锁环。
    lc.report(lifecycle::machine::Msg::RefineProgress {
        note_id: note_id.to_string(),
        stage: stage.into(),
        state: state_s.into(),
    });
}

/// 运行署名 RAII(codex 三十四轮):spawn_blocking 线程会被复用,退役时不清署名,
/// 后续无关写盘会被记到已收工的那轮头上。
struct RunTagGuard;
impl Drop for RunTagGuard {
    fn drop(&mut self) {
        CURRENT_REFINE_RUN.with(|c| *c.borrow_mut() = None);
    }
}

fn refine_beat_touch(note_id: &str, gen: u64, stage: &str, state_s: &str) {
    let mut g = REFINE_BEAT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let m = g.get_or_insert_with(Default::default);
    // 旧代次不许抢座(codex 五轮):停摆被摘的前任若诈尸继续 report,任其覆盖
    // 替补的条目,前任 Drop 时会把座位整个清掉,替补重活阶段就误报 beat=null。
    if m.get(note_id).is_some_and(|(g0, _, _)| *g0 > gen) {
        return;
    }
    m.insert(note_id.to_string(), (gen, format!("{stage}/{state_s}"), std::time::Instant::now()));
}

/// 只清本代 worker 的条目:停摆被摘的前任退场时,若心跳已被替补接管(代次不同),
/// 原样留下——否则 refine_status 会在替补重活阶段误报 beat=null。
fn refine_beat_clear(note_id: &str, gen: u64) {
    let mut g = REFINE_BEAT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(m) = g.as_mut() {
        if m.get(note_id).is_some_and(|(g0, _, _)| *g0 == gen) {
            m.remove(note_id);
        }
    }
}

thread_local! {
    /// 本线程所属 Aing 运行的标识 "pid-代次"(codex 三十/三十三轮):worker 线程
    /// 起跑时设置,写盘咽喉读它落 writer_run。查心跳表在座者会把诈尸前任的写
    /// 错记到替补头上;线程局部才是"谁写的"的真值。非 worker 线程为 None。
    static CURRENT_REFINE_RUN: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

pub(crate) fn set_current_refine_run(gen: u64) {
    CURRENT_REFINE_RUN
        .with(|c| *c.borrow_mut() = Some(format!("{}-{gen}", std::process::id())));
}

pub(crate) fn current_refine_run() -> Option<String> {
    CURRENT_REFINE_RUN.with(|c| c.borrow().clone())
}

/// 查一篇心跳条目当前属于哪一代 worker(收工戳代次守卫用,codex 二十一轮)。
fn refine_beat_owner(note_id: &str) -> Option<u64> {
    let g = REFINE_BEAT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    g.as_ref()?.get(note_id).map(|(gen, _, _)| *gen)
}

/// 查一篇的心跳:Some((stage/state, 距今 ms))。None = 无 worker 在跑。
pub(crate) fn refine_beat_of(note_id: &str) -> Option<(String, u64)> {
    let g = REFINE_BEAT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    g.as_ref()?.get(note_id).map(|(_, s, t)| (s.clone(), t.elapsed().as_millis() as u64))
}

/// 识别器唯一实例化点：按选型造对应识别器，装进 trait 对象。preload 与 spawn_session
/// 槽空兜底都经此，杜绝两处各写一份 new 而漏掉某一选型。pub:asr_bench 评测工具
/// 复用同一实例化点(bin 是独立 crate,pub(crate) 不够)。
/// provider 经 settings.asr_provider 覆盖(实验字段,默认 None = CPU)。
/// hotwords 仅 Qwen3 消费(prompt 注入偏置),其余引擎无解码级热词入口,忽略。
pub fn new_recognizer(
    asr_model: &str,
    provider: Option<String>,
    hotwords: Option<String>,
) -> anyhow::Result<Box<dyn asr::Recognizer>> {
    let dir = models::asr_model_dir(asr_model);
    if asr_model == settings::ASR_WHISPER {
        Ok(Box::new(asr::whisper::WhisperRecognizer::new(&dir, provider)?) as Box<dyn asr::Recognizer>)
    } else if asr_model == settings::ASR_PARAFORMER {
        Ok(Box::new(asr::paraformer::ParaformerRecognizer::new(&dir, provider)?) as Box<dyn asr::Recognizer>)
    } else if asr_model == settings::ASR_QWEN3 {
        Ok(Box::new(asr::qwen3::Qwen3Recognizer::new(&dir, provider, hotwords)?) as Box<dyn asr::Recognizer>)
    } else if asr_model == settings::ASR_FIRERED {
        Ok(Box::new(asr::fire_red::FireRedRecognizer::new(&dir, provider)?) as Box<dyn asr::Recognizer>)
    } else {
        Ok(Box::new(asr::sense_voice::SenseVoiceRecognizer::new(&dir, provider)?) as Box<dyn asr::Recognizer>)
    }
}

/// 热词词表上限。词表越大,偏置越被稀释,且空/静音段的幻觉风险越高
/// (sherpa-onnx #3509:热词会被整句吐出);上限内先收用户手填词,再收声纹人名。
const HOTWORDS_MAX: usize = 100;

/// 合并热词词表(纯逻辑):用户手填(逗号/中文逗号/顿号/分号/换行分隔)优先,
/// 其后并入声纹库人名;去重保序,超上限截断;空集 → None(引擎不启用偏置)。
fn merge_hotwords<I: IntoIterator<Item = String>>(user: &str, names: I) -> Option<String> {
    let mut words: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let user_words = user.split([',', '，', '、', ';', '；', '\n']).map(str::to_string);
    for w in user_words.chain(names) {
        let w = w.trim();
        if !w.is_empty() && words.len() < HOTWORDS_MAX && seen.insert(w.to_string()) {
            words.push(w.to_string());
        }
    }
    if words.is_empty() { None } else { Some(words.join(",")) }
}

#[cfg(test)]
mod hotwords_tests {
    use super::{merge_hotwords, HOTWORDS_MAX};

    #[test]
    fn merge_splits_dedupes_and_keeps_user_words_first() {
        let names = vec!["张伟".to_string(), " ".to_string(), "Alice".to_string()];
        let got = merge_hotwords("DashScope，语音笔记、Alice\n张伟;  ", names);
        // 用户词序在前;名单里的重复(Alice/张伟)与空白被去掉。
        assert_eq!(got.as_deref(), Some("DashScope,语音笔记,Alice,张伟"));
    }

    #[test]
    fn merge_empty_everything_is_none() {
        assert_eq!(merge_hotwords("", Vec::new()), None);
        assert_eq!(merge_hotwords(" ,，、\n ", vec!["".into(), "  ".into()]), None);
    }

    #[test]
    fn merge_caps_at_limit_user_words_win() {
        let user = (0..HOTWORDS_MAX).map(|i| format!("u{i}")).collect::<Vec<_>>().join(",");
        let got = merge_hotwords(&user, vec!["溢出词".to_string()]).unwrap();
        let words: Vec<&str> = got.split(',').collect();
        assert_eq!(words.len(), HOTWORDS_MAX);
        assert!(!words.contains(&"溢出词"), "超上限后声纹人名不再挤入");
        assert_eq!(words[0], "u0");
    }
}

/// 当前设置与声纹库拼出的 Qwen3 热词。设置读取失败/声纹库缺失都只降级为少词,
/// 绝不挡识别器装配。非 Qwen3 引擎调用方也可无脑传入(new_recognizer 会忽略)。
fn qwen3_hotwords(app: &AppHandle) -> Option<String> {
    let s = app.path().app_data_dir().map(|d| settings::load(&d)).unwrap_or_default();
    let names: Vec<String> = data_root(app)
        .map(|root| {
            let vp = store::VoiceprintStore::new(root).load();
            vp.people.values().map(|p| p.name.clone()).collect()
        })
        .unwrap_or_default();
    merge_hotwords(&s.asr_hotwords, names)
}

/// 声纹人名变更(改名/合并/删除/撤销合并)后失效常驻识别器:Qwen3 热词在识别器
/// 构造时快照声纹人名,常驻缓存跨场复用会让变更后仍偏置旧名直到重启或无关设置
/// 变更(codex 2026-08-11 P2)。仅 Qwen3 吃热词,其余引擎清槽纯属白重载几 GB
/// 模型,按当前选型门控。录制中(仅改名可达,合并/删除录制中被拒)清槽/预载都
/// 无效——识别器已被本场取走,preload 会跳过,停录 stash 又把旧热词件还回来;
/// 改置脏标记,由停录归还处丢弃重载(本场热词开录已定型,无法热更)。
fn refresh_qwen_hotwords_cache(app: &AppHandle) {
    if current_asr(app) != settings::ASR_QWEN3 {
        return;
    }
    let state = app.state::<AppState>();
    if state.session.lock().unwrap().is_some() {
        state.hotwords_dirty.store(true, Ordering::Relaxed);
        return;
    }
    *state.recognizer_cache.lock().unwrap() = None;
    preload_models(app.clone(), state.session.clone(), state.recognizer_cache.clone(), state.embedder_cache.clone());
}

/// 云端识别器唯一实例化点(对称 new_recognizer):按 provider 造火山/阿里适配器。
/// 凭证不齐直接 bail——开录/测试连接都走这里,错误文案单一真源,避免"连上了才发现
/// 没填 key"这种要等一次握手往返才暴露的失败。
fn make_cloud_asr(s: &settings::Settings) -> anyhow::Result<std::sync::Arc<dyn asr::cloud::CloudAsr>> {
    if !settings::cloud_creds_ok(s) {
        anyhow::bail!(tr!(
            "请先在设置中配置云端凭证",
            "Please configure the cloud credentials in Settings first"
        ));
    }
    if s.cloud_asr_provider == settings::CLOUD_ALIYUN {
        Ok(std::sync::Arc::new(asr::cloud::aliyun::AliyunAsr::new(s.dashscope_api_key.trim().to_string()))
            as std::sync::Arc<dyn asr::cloud::CloudAsr>)
    } else {
        Ok(std::sync::Arc::new(asr::cloud::volcano::VolcanoAsr::new(
            s.volc_app_key.trim().to_string(),
            s.volc_access_key.trim().to_string(),
        )) as std::sync::Arc<dyn asr::cloud::CloudAsr>)
    }
}

/// 云端厂商展示名(测试连接文案 / 日志)。
fn cloud_provider_label(provider: &str) -> String {
    if provider == settings::CLOUD_ALIYUN {
        tr!("阿里云", "Alibaba Cloud")
    } else {
        tr!("火山引擎", "Volcano Engine")
    }
}

/// 当前 provider 覆盖:settings.asr_provider 经 asr::provider_override 规整。
/// 读设置失败 → None(CPU),与 current_asr 的兜底纪律一致。
fn current_asr_provider(app: &AppHandle) -> Option<String> {
    app.path()
        .app_data_dir()
        .ok()
        .and_then(|d| asr::provider_override(&settings::load(&d).asr_provider))
}

/// 当前说话人识别方法(种子匹配策略键):读设置失败回落空串 → matcher_from_key
/// 按默认最近邻处理,与 current_asr 的兜底纪律一致。
fn current_speaker_match(app: &AppHandle) -> String {
    app.path().app_data_dir().map(|d| settings::load(&d).speaker_match).unwrap_or_default()
}

/// 当前 ASR 选型：app_data_dir → settings.json 读 asr_model；app_data_dir 不可用时
/// 默认 sense_voice（与 settings 默认一致），绝不因读设置失败挡住录制/预载。
/// 仅本模块内取用（识别器装配 / preload）；托盘就绪判定已改经 current_models_status
/// （模式感知，云端模式不看本地选型），不再直接依赖这个函数。
fn current_asr(app: &AppHandle) -> String {
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

/// 当前声纹模型文件路径(按设置选型;调用点均为低频路径,现场读一次 settings)。
fn speaker_model_path(app: &AppHandle) -> PathBuf {
    let model = app
        .path()
        .app_data_dir()
        .map(|d| settings::load(&d).speaker_model)
        .unwrap_or_default();
    speaker_model_path_for(&model)
}

/// 按**给定**模型名取权重路径。重建路径必须用它而不是 speaker_model_path:
/// 后者自己再读一次设置,于是"库标签写成 A"与"实际用哪个权重嵌入"来自两次独立读取,
/// 中间用户切一次模型就能让二者分叉——最终把 B 空间的向量写进标着 A 的库
/// (codex review 二轮 P1#1)。
fn speaker_model_path_for(model: &str) -> PathBuf {
    models::root().join(models::speaker_model_file(model))
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

/// 本场录制的「必备源集合」：硬承诺双轨（settings-overhaul spec §4）——Mic 与 System
/// 两源都必备，任一未出现在 start.active 里即整场拆除报错（不做静默降级）。
///
/// 为什么不再区分场景降级 System：会议笔记的核心承诺是「对方/外放说了什么都要被录到」，
/// System 拿不到就默默只录 mic，等于用户以为记完整了、实际漏了对方发言而不自知——这类
/// 静默降级比直接拒录更有害（2026-07-07 复盘：用户所有笔记都没有 system 轨，自己毫无
/// 察觉）。因此 System 起不来（无论是权限未授权还是设备/组件不可用）一律整场拆除，
/// 错误消息里带上权限/设备的分类，前端据此弹出授权引导卡（拒录，不降级）。
///
/// 纯函数（单测覆盖），供 spawn_session 的源构建与 Fix A 守卫共用。
fn required_sources() -> Vec<Source> {
    vec![Source::Mic, Source::System]
}

/// 源的中文显示名，仅用于「XX未能启动」失败文案（沿用既有文案风格）。
fn source_display(s: Source) -> String {
    match s {
        Source::Mic => tr!("麦克风", "Microphone"),
        Source::System => tr!("系统声音", "System audio"),
    }
}

/// Fix A 拆除路径的错误文案构造（纯函数，单测覆盖）：`missing` 是 required_sources
/// 里缺失的那个源，`failed` 是合并后的失败记录（spawn_session 里 pre_start_failed
/// ++ start.failed，装配阶段 + capture 启动阶段两段失败的并集）。返回值恒以
/// "error: " 前缀开头，供 fail() 直接作为 status 事件的 state 使用。
///
/// System 缺失时在文案末尾追加稳定分类 token（" [system_denied]" / "
/// [system_unavailable]"，判据沿用 classify_system 的既有 "unauthorized" 子串
/// 匹配）——前端 record 页据此渲染授权引导卡。Mic 缺失沿用硬承诺双轨改造前的
/// 纯文案，不带任何 token（逐字节兼容旧格式，Mic 缺失没有"打开系统设置"这类
/// 可操作的引导可给）。
fn missing_source_error(missing: Source, failed: &[(Source, String)]) -> String {
    let name = source_display(missing);
    let system_token = |msg: Option<&str>| -> &'static str {
        match msg {
            Some(m) if m.contains("unauthorized") => "system_denied",
            _ => "system_unavailable",
        }
    };
    failed
        .iter()
        .find(|(s, _)| *s == missing)
        .map(|(_, msg)| {
            let base = format!(
                "error: {}",
                tr!("{name}未能启动: {msg}", "{name} failed to start: {msg}")
            );
            if missing == Source::System {
                format!("{base} [{}]", system_token(Some(msg)))
            } else {
                base
            }
        })
        .unwrap_or_else(|| {
            let base = format!("error: {}", tr!("{name}未能启动", "{name} failed to start"));
            if missing == Source::System {
                format!("{base} [{}]", system_token(None))
            } else {
                base
            }
        })
}

/// 会话加载线程要落盘的目标笔记：New = 新建，Resume = 续录既有非活动笔记
/// （已中断或已完成均可）。spawn_session 据此分支 writer 的创建方式。
enum NoteTarget {
    New,
    Resume(String),
}

/// 录制↔重转写互斥的 S 侧（spawn_session）权威判定：running 已置 true 之后读重转写槽
/// （Dekker 写后读）。true=须回滚 running 并拒绝启动录制。纯函数，供生产路径与并发
/// 回归测试共用——测试直接驱动这两个判定函数，验证的正是生产两侧调用的同一份协议。
pub(crate) fn retranscribe_blocks_recording(slot: &Mutex<Option<(String, String)>>) -> bool {
    slot.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

/// 录制↔重转写互斥的 R 侧（do_retranscribe）权威判定：重转写槽已占之后读录音旗
/// （Dekker 写后读）。true=须清槽并拒绝启动重转写。session_active 由调用方传入
/// （session 槽覆盖 stop 早期窗口：running 已假但会话槽还没清空的那一小段时间）。
pub(crate) fn recording_blocks_retranscribe(running: &Mutex<bool>, session_active: bool) -> bool {
    *running.lock().unwrap_or_else(|e| e.into_inner()) || session_active
}

/// 补生成成品轨的占槽判定(纯函数,镜像 retranscribe_blocks_recording 形态)。
/// 供三处互斥接线共用:do_regenerate_mixed 自身的槽占用拒、do_retranscribe 的
/// 双向互查(重转写读旧 mixed 的同时补生成在原子替换它)、以及并发回归测试。
pub(crate) fn mixed_regen_busy(slot: &Mutex<Option<String>>) -> bool {
    slot.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

/// Aing↔重转写占槽后互查闭环的 A 侧（spawn_refine）判定：给定重转写槽当前值与
/// 本次 Aing 的 note_id，判断是否命中"同一笔记正被重转写占槽"（true=须清刚插入
/// 的 Aing 集并放弃本次 Aing，不 spawn 工作线程）。纯函数，从 spawn_refine 里的
/// 判据原样抽出——完整互斥证明见 spawn_refine/do_retranscribe 两处 Fix 2 注释
/// （codex 第三轮）。真实竞态涉及 lifecycle actor 信箱 + 后台线程交错，无法在
/// 单元测试里驱动出货真价实的并发窗口；这个纯函数至少把"槽命中同 note_id 时
/// 该让步"这条判据本身纳入测试，而不是只靠现场审读代码。
/// 补生成↔Aing 占槽后互查的 A 侧判定(codex 第三轮 P1,镜像 retranscribing_blocks_refine):
/// regen worker 全程持目录级 NoteLock,Aing 收尾提交 aing/refined 时要拿同一把锁,
/// 昂贵 LLM 阶段跑完才失败——同 note_id 占槽即让步。
pub(crate) fn mixed_regen_blocks_refine(slot: &Mutex<Option<String>>, note_id: &str) -> bool {
    slot.lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_deref()
        .is_some_and(|running_id| running_id == note_id)
}

pub(crate) fn retranscribing_blocks_refine(slot: &Mutex<Option<(String, String)>>, note_id: &str) -> bool {
    slot.lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|(running_id, _)| running_id == note_id)
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
    embedder_cache: Arc<Mutex<Option<Box<diar::TaggedEmbedder>>>>,
    transcode: Arc<store::transcode::TranscodeQueue>,
    retranscribing: Arc<Mutex<Option<(String, String)>>>,
    mixed_regen: Arc<Mutex<Option<String>>>,
    target: NoteTarget,
) -> Result<(), String> {
    let my_gen = {
        let mut r = running.lock().unwrap();
        if *r {
            return Err("已在录制".into()); // i18n-exempt: 前端按原文判等
        }
        *r = true;
        // 锁序 running → generation：running 锁仍持有时嵌套锁 generation 并
        // 递增，捕获的 my_gen 即本次会话的"代号"，随线程一起移动。
        let mut g = generation.lock().unwrap();
        *g += 1;
        *g
    };

    // Fix 1B(Dekker 写后读，S 侧权威判定):running 置 true 之后再读重转写槽。
    // do_start_recording/do_resume_note_recording 里的早期槽检查只是快速失败的
    // UX——若那次检查穿过加载窗口（槽在检查后、running 置位前才被占），这里补上
    // 权威判定。与 R 侧 do_retranscribe 的占槽后复查（recording_blocks_retranscribe）
    // 互为镜像：两侧各自"先写自己、再读对方"，顺序矛盾使得二者不可能同时判定通过。
    // 短锁读槽，不与 running/session 嵌套，避免和 R 侧的锁序相反而成环。
    if retranscribe_blocks_recording(&retranscribing) {
        // 回滚照 fail() 的纪律:锁 running → 嵌套锁 generation → 仅当 *g==my_gen
        // 才置回 false。置 true 到这里之间不可能有新的 start 穿进来(running=true
        // 挡着)，但 stop 可能已经把 running 置 false 并递增了 generation——过期
        // 就不回滚，以免覆盖更新的状态。
        let mut r = running.lock().unwrap();
        let g = generation.lock().unwrap();
        if *g == my_gen {
            *r = false;
        }
        drop(g);
        drop(r);
        return Err(tr!(
            "重转写进行中,完成后再录制",
            "A re-transcription is in progress; please record after it finishes"
        ));
    }
    // 补生成侧同款写后读(codex P1:此前只有 regen 单向读 running,协议没闭环——
    // regen 复查 running 之后、这里置 running 之前的窗口里两侧可双穿):regen 是
    // 写(槽)→读(running),本侧是写(running)→读(槽),顺序矛盾封死双穿。回滚纪律
    // 与上方重转写分支完全一致。
    if mixed_regen_busy(&mixed_regen) {
        let mut r = running.lock().unwrap();
        let g = generation.lock().unwrap();
        if *g == my_gen {
            *r = false;
        }
        drop(g);
        drop(r);
        return Err(tr!(
            "正在补生成成品轨,完成后再录制",
            "Mixed-track regeneration is in progress; please record after it finishes"
        ));
    }

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
            let _ = app.emit("status", ipc::StatusEvent { state: msg, system_audio: String::new(), note_id: String::new(), diarization: String::new(), elapsed_ms: 0, input_override: String::new() });
            // P1 影子回报:仅当前代的启动失败走到这里(过期线程已提前 return),
            // 通知 actor 内核回到 Idle。后台线程投递,不等待(见 actor.rs 死锁注记②)。
            app.state::<lifecycle::LifecycleHandle>().report(lifecycle::machine::Msg::SessionFailed);
        };

        // 0) 一次性读设置：language_filter / capture_path(采集路径逃生舱)/
        // audio_scheme(录制期混音) / 识别方式与云端凭证 同源同快照
        // （避免多次 load 读到并发写入的不同代）。app_data_dir 不可用时整体回落
        // Settings::default（语言过滤=开、采集路径=AEC(软件回声消除)、
        // 混音成品轨=是、识别方式=本地），绝不因读设置失败改变现状行为。位置提到取模型
        // 之前：识别方式决定要不要取常驻识别器。language_filter 在下方 start_session 处消费。
        let cfg = app.path().app_data_dir().map(|d| settings::load(&d)).unwrap_or_default();
        // 本场声纹的模型标签。**必须取自 cfg 这一份快照**,与下面取嵌入器同源——
        // 中途再读一次设置就可能与实际用来算向量的那个模型分家,于是"声明的空间"和
        // "真实的空间"不符,而库那边只认声明(见 voiceprints::space_ok)。
        let speaker_model = cfg.speaker_model.clone();
        let (language_filter, use_aec_capture, mix_track) = (
            cfg.language_filter,
            cfg.capture_path == settings::CapturePath::Aec,
            cfg.audio_scheme.mix_track(),
        );
        let cloud_mode = cfg.asr_mode == settings::ASR_MODE_CLOUD;

        // 1) 识别引擎。
        //  - 云端：按凭证造厂商适配器，完全不碰 recognizer_cache——常驻识别器留着,
        //    回切本地时零延迟可用；凭证缺失在这里就拦(不必等一次握手往返)。
        //  - 本地：取常驻识别器（预载中会在锁上等待）；槽空则现场加载兜底。
        let cloud_asr = if cloud_mode {
            match make_cloud_asr(&cfg) {
                Ok(c) => Some(c),
                Err(e) => return fail(&app, &running, &generation, my_gen, format!("error: {e}")),
            }
        } else {
            None
        };
        let recognizer: Option<Box<dyn asr::Recognizer>> = if cloud_mode {
            None
        } else {
            let taken = recognizer_cache.lock().unwrap().take();
            Some(match taken {
                Some(r) => r,
                None => match new_recognizer(&current_asr(&app), current_asr_provider(&app), qwen3_hotwords(&app)) {
                    Ok(r) => r,
                    Err(e) => {
                        return fail(&app, &running, &generation, my_gen, format!("error: {e}"))
                    }
                },
            })
        };
        // 1.5) 取常驻声纹嵌入器；与 recognizer 完全对称的取用节奏（其后），但槽空
        // 时不现场加载——预载失败即降级为无声纹（说话人区分不可用），而不是在
        // 开录路径上额外背一次模型加载的延迟/失败风险。云端模式同样取用/返还:
        // 声纹是本机能力,与识别在哪跑无关。
        // **取用即核对**:槽里那位可能是上一个选型建的(重建线程 stash 与设置变更
        // 清缓存之间存在窗口)。不符就丢弃——宁可本场没有说话人区分,也不能用错空间
        // 的嵌入器算出一整场向量,再以当前标签写进库。
        let embedder = embedder_cache.lock().unwrap().take().and_then(|te| {
            if te.model() == speaker_model {
                Some(te.into_inner())
            } else {
                eprintln!(
                    "常驻声纹嵌入器是 {} 建的,本场选型是 {speaker_model},丢弃不用",
                    te.model()
                );
                None
            }
        });
        // 声纹模型是否就绪 → 决定前端是否显示「说话人区分不可用」降级横幅。
        let diarization = if embedder.is_some() { "on" } else { "unavailable" }.to_string();

        // 2) 构建源（各自 VAD）。恒建麦克风 + 系统声音，硬承诺双轨下两源皆必备——
        // System 的 VAD 构建失败不再静默跳过（旧行为:打日志降级为仅麦克风），改记入
        // pre_start_failed,走下方 match start 处的 Fix A 拆除路径统一处理（见两块
        // cfg(target_os = "macos") / cfg(windows) 的 Err 分支）。record_system_only
        // (仅系统声)已随三删一藏移除,不再有跳过麦克风的路径。
        let vad_path = models::root().join("silero_vad.onnx");
        let mut sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> = Vec::new();
        // 源在装配阶段（sources 构建期,start_session 之前）就失败的记录——目前只有
        // System 的 VAD 构建失败会走到这里。与 start_session 返回的 SessionStart::failed
        // （capture.start() 失败,装配之后）分属两个阶段,下方 match start 处合并两者
        // 供 Fix A 的错误文案与分类判定共用（classify_system 的既有 unauthorized 判据
        // 不变，见该函数注释）。
        let mut pre_start_failed: Vec<(Source, String)> = Vec::new();
        // 每源健康计数(FrameTap 写、pipeline_health 读),随 ActiveSession 存活一场。
        let mut session_health: Vec<(Source, Arc<SourceHealth>)> = Vec::new();
        // 每源时钟漂移监视器(Task 6),随 ActiveSession 存活一场,停录落 drift_report.json。
        let mut session_drift: Vec<(Source, Arc<pipeline::drift_monitor::DriftMonitor>)> =
            Vec::new();
        // 两源首个真实帧共享一个单调时钟原点。谁先到谁把 0 点钉住,后到源把偏移
        // 写进 SourceHealth,供 mixed sink 在 16k 时间轴上插入准确的前导静音。
        let timeline_origin = Arc::new(OnceLock::new());
        // Codex review Fix 4(P2):时钟漂移传感器 E1 标定场需要 AEC 完全不介入——
        // 默认 capture_path=aec 下软件 AEC 在 mic.wav 落盘前工作,E1 标定播放的
        // click 恰是"系统回放→mic 回声",会被定向消除,xcorr 的刺激没了就测不出
        // 传感器精度;VPIO 路径同理(Apple AEC)。VOICE_NOTES_CALIBRATION=1 时旁路:
        // 强制普通 cpal 麦克风(不走 VPIO)、跳过软件 AEC 角色构建。只读一次环境变量
        // 存 bool,不进设置系统/UI——AEC 是 PR#86 定死不可配的,校准是唯一例外。
        let calibration = std::env::var("VOICE_NOTES_CALIBRATION").as_deref() == Ok("1");
        let mic_seg = match new_silero(&vad_path) {
            Ok(s) => s,
            Err(e) => {
                stash_model(&recognizer_cache, recognizer);
                stash_model(&embedder_cache, retag(&speaker_model, embedder));
                return fail(&app, &running, &generation, my_gen, format!("error: {e}"));
            }
        };
        // 麦克风源：macOS 默认用普通 cpal 输入 + 软件 AEC(采集路径逃生舱 capture_path
        // 默认 aec);vpio 档改用带 Apple AEC 的 VPIO(通话模式,内部失败自动回退
        // cpal)——VPIO 一启动 macOS 就把其它音频压低 12-16dB(ducking,Min 档配置下仍
        // 如此,系统固有行为),外放开会场景既听不清、录下的系统声轨电平也小;普通输入
        // 无 ducking,回声由下方装配的软件 AEC(WebRTC AEC3)消除,文本回声去重链保留
        // 为兜底。其他平台恒用 cpal。
        // 采集栈:TappedCapture(ResilientCapture(真实采集))。
        //  - Resilient:流错误/失联时工厂重建采集,复用同一帧通道,worker 无感;
        //  - Tap:健康统计 + 断流期按墙钟补零(时间轴不塌,双轨对齐不断裂),
        //    其失联通知(>3s 无帧)踢 Resilient 重启——覆盖 VPIO 这类未接
        //    错误回调的后端,与 cpal 的 CaptureEvent 快路径互补。
        // 标定模式(calibration)下即便 vpio 档也不许走 VPIO——同一 Codex review Fix 4
        // 理由:Apple AEC 会把标定刺激的房间回声消掉,校准是唯一例外,不进设置系统。
        // 录前设备检查自动择优(2026-08-22 设计):跟随系统默认的输入是蓝牙通话麦
        // 且存在内置/有线替代 → 本场 cpal 采集直接绑定替代设备(不改系统设置,
        // 不影响会议软件)。设置开关 auto_input_pick 可关;VPIO 逃生舱路径不覆盖
        // (它由 Apple 通话链路自行管理设备)。
        #[cfg(target_os = "macos")]
        let input_override: Option<String> = if cfg.auto_input_pick
            && (use_aec_capture || calibration)
            && audio::default_input_is_bluetooth()
        {
            let picked = audio::pick_non_bluetooth_input();
            if let Some(name) = &picked {
                eprintln!("录前择优:默认输入是蓝牙通话麦,本场改用「{name}」采集");
            }
            picked
        } else {
            None
        };
        #[cfg(target_os = "macos")]
        let input_override_for_session = input_override.clone().unwrap_or_default();
        #[cfg(not(target_os = "macos"))]
        let input_override_for_session = String::new();
        #[cfg(target_os = "macos")]
        let mic_factory: audio::resilient::CaptureFactory = if use_aec_capture || calibration {
            if calibration && !use_aec_capture {
                eprintln!("[标定模式] AEC 已停用(本场)");
            }
            Box::new(move || {
                let (etx, erx) = crossbeam_channel::unbounded();
                (
                    Box::new(audio::microphone::Microphone::with_events_and_device(
                        etx,
                        input_override.clone(),
                    )) as Box<dyn AudioCapture>,
                    erx,
                )
            })
        } else {
            Box::new(|| {
                // VPIO 无运行期错误回调:事件通道空置(发送端即弃),
                // 死亡由 Tap 帧荒检测兜底。
                let (_etx, erx) = crossbeam_channel::unbounded::<audio::CaptureEvent>();
                (Box::new(audio::vpio::VpioMicrophone::new()) as Box<dyn AudioCapture>, erx)
            })
        };
        #[cfg(not(target_os = "macos"))]
        let mic_factory: audio::resilient::CaptureFactory = Box::new(|| {
            let (etx, erx) = crossbeam_channel::unbounded();
            (
                Box::new(audio::microphone::Microphone::with_events(etx))
                    as Box<dyn AudioCapture>,
                erx,
            )
        });
        let mic_health = Arc::new(SourceHealth::default());
        // nominal_hz=0:tap 在重采样之前,mic 声明率在 start 后才可知(cpal 默认配置)
        // ——DriftMonitor 惰性初始化,以首帧声明率为准现场锁定(见 drift_monitor.rs feed)。
        let mic_drift = Arc::new(pipeline::drift_monitor::DriftMonitor::new(0));
        let mic_resilient = audio::resilient::ResilientCapture::new(mic_factory, {
            let app = app.clone();
            let health = mic_health.clone();
            let app2 = app.clone();
            audio::resilient::ResilientNotify {
                on_recovered: Some(Box::new(move || {
                    health.restarts.fetch_add(1, Ordering::Relaxed);
                    // 兜住不等于没发生:重建成功一次,就说明这台机器上的采集链断过一次。
                    telemetry::report_error(
                        telemetry::ErrorKind::CaptureRebuild,
                        "mic 采集断连后重建成功",
                    );
                    let _ = app.emit(
                        "source_health",
                        ipc::SourceHealthEvent {
                            source: "mic".into(),
                            state: "recovered".into(),
                            gap_pct: None,
                        },
                    );
                })),
                on_lost: Some(Box::new(move || {
                    // 重试耗尽 = 这一路音源本场彻底没了,是最该被看见的采集故障。
                    telemetry::report_error(
                        telemetry::ErrorKind::CaptureRebuild,
                        "mic 采集断连重试耗尽,本场放弃该源",
                    );
                    let _ = app2.emit(
                        "source_health",
                        ipc::SourceHealthEvent {
                            source: "mic".into(),
                            state: "lost".into(),
                            gap_pct: None,
                        },
                    );
                })),
            }
        });
        let mic_kicker = mic_resilient.kicker();
        let storm_app = app.clone();
        let mic_notify = TapNotify {
            on_stall: Some(Box::new(move || {
                eprintln!("麦克风采集失联(>3s 无帧):静音填充维持时间轴,触发自愈重启");
                let _ = mic_kicker.try_send(());
            })),
            on_recover: Some(Box::new(|| eprintln!("麦克风采集恢复,静音填充结束"))),
            // 高频短断流:设备还活着(不触发 on_stall),但内容在持续丢。
            // 录制中就告诉用户,别等会后才靠耳朵发现掉字。
            on_gap_storm: Some(Box::new(move |ratio| {
                let _ = storm_app.emit(
                    "source_health",
                    ipc::SourceHealthEvent {
                        source: "mic".into(),
                        state: if ratio.is_some() { "gap_storm" } else { "gap_storm_over" }.into(),
                        gap_pct: ratio.map(|r| (r * 100.0).round() as u32),
                    },
                );
            })),
        };
        let mic: Box<dyn AudioCapture> = Box::new(
            TappedCapture::new_with_timeline_origin(
                Box::new(mic_resilient),
                Source::Mic,
                TapPolicy::mic(),
                mic_health.clone(),
                mic_notify,
                timeline_origin.clone(),
            )
            .with_drift(mic_drift.clone()),
        );
        session_health.push((Source::Mic, mic_health));
        session_drift.push((Source::Mic, mic_drift.clone()));
        // Task 7:实测采样率旁证——10s 一次查询默认输入设备的 ActualSampleRate,喂给
        // mic DriftMonitor,与 DLL 频率估计互为旁证。一期铁律:只测不动数据,查询
        // 失败(非 macOS/无权限/设备异常)一律返回 None,静默跳过,不影响录音。
        // stop_tx 存进 ActiveSession,随会话拆除 drop 触发线程退出(见该字段注释)。
        let (actual_rate_stop_tx, actual_rate_stop_rx) = crossbeam_channel::bounded::<()>(0);
        // 非 macOS 上 actual_hz_of 恒为 None,起这条线程等于让每场录音白白空转
        // 一个 10s 轮询到底(issue #100 条 7);整段 cfg 门控掉。
        #[cfg(target_os = "macos")]
        {
            let mic_drift = mic_drift.clone();
            // 设备归属(issue #100 条 2 + Codex review P1)。旁证只有在能确定"测的
            // 就是本场录音那只设备"时才有意义,而 cpal 不暴露流所绑定的
            // AudioDeviceID,我们只能查系统默认输入。两种误判都真实存在:
            //   · 每轮重解析 → 用户中途改默认输入,就测到了与本场无关的设备;
            //   · 起点解析一次后钉死 → ResilientCapture 断连自愈会重开默认设备,
            //     可能换成另一只物理麦,而我们仍指着旧的。
            // 既然无法确定归属,就不猜:逐轮解析,一旦设备中途变过,直接把旁证与
            // 设备名作废(报告里为 None),而不是发布一个可能张冠李戴的数字。
            // 一期铁律是只测不动数据——测不准时闭嘴,比报错数强。
            std::thread::spawn(move || {
                let mut pinned: Option<u32> = None;
                let mut invalidated = false;
                loop {
                    match actual_rate_stop_rx.recv_timeout(std::time::Duration::from_secs(10)) {
                        // 超时(10s 到)= 轮询一次;stop 发送端已断开 = 会话已结束,退出。
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                            let Some(dev) = audio::actual_rate::default_input_device_id() else {
                                continue;
                            };
                            match pinned {
                                None => {
                                    pinned = Some(dev);
                                    if let Some(name) = audio::actual_rate::device_name(dev) {
                                        mic_drift.set_device_name(name);
                                    }
                                }
                                Some(p) if p != dev => {
                                    // 设备换了:此前测得的旁证属于另一只设备,作废。
                                    invalidated = true;
                                    mic_drift.invalidate_device_evidence();
                                }
                                _ => {}
                            }
                            if invalidated {
                                continue;
                            }
                            if let Some(hz) = audio::actual_rate::actual_hz_of(dev) {
                                mic_drift.set_actual_rate_hz(hz);
                            }
                        }
                        _ => break,
                    }
                }
            });
        }
        #[cfg(not(target_os = "macos"))]
        let _ = &actual_rate_stop_rx;
        sources.push((Source::Mic, mic, mic_seg));

        #[cfg(target_os = "macos")]
        {
            match new_silero(&vad_path) {
                Ok(sys_seg) => {
                    // SCK 无运行期错误回调:自愈全靠 Tap 帧荒(5s)踢重启。
                    let sys_factory: audio::resilient::CaptureFactory = Box::new(|| {
                        let (_etx, erx) = crossbeam_channel::unbounded::<audio::CaptureEvent>();
                        (
                            Box::new(audio::system::SystemAudioCapture::new())
                                as Box<dyn AudioCapture>,
                            erx,
                        )
                    });
                    let sys_health = Arc::new(SourceHealth::default());
                    // 同 mic:声明率待 start 后可知,惰性初始化(见上方 mic_drift 注释)。
                    let sys_drift = Arc::new(pipeline::drift_monitor::DriftMonitor::new(0));
                    let sys_resilient =
                        audio::resilient::ResilientCapture::new(sys_factory, {
                            let app = app.clone();
                            let health = sys_health.clone();
                            let app2 = app.clone();
                            audio::resilient::ResilientNotify {
                                on_recovered: Some(Box::new(move || {
                                    health.restarts.fetch_add(1, Ordering::Relaxed);
                                    // 兜住不等于没发生:重建成功一次,就说明这台机器上的采集链断过一次。
                                    telemetry::report_error(
                                        telemetry::ErrorKind::CaptureRebuild,
                                        "system 采集断连后重建成功",
                                    );
                                    let _ = app.emit(
                                        "source_health",
                                        ipc::SourceHealthEvent {
                                            source: "system".into(),
                                            state: "recovered".into(),
                                            gap_pct: None,
                                        },
                                    );
                                })),
                                on_lost: Some(Box::new(move || {
                                    // 重试耗尽 = 这一路音源本场彻底没了,是最该被看见的采集故障。
                                    telemetry::report_error(
                                        telemetry::ErrorKind::CaptureRebuild,
                                        "system 采集断连重试耗尽,本场放弃该源",
                                    );
                                    let _ = app2.emit(
                                        "source_health",
                                        ipc::SourceHealthEvent {
                                            source: "system".into(),
                                            state: "lost".into(),
                                            gap_pct: None,
                                        },
                                    );
                                })),
                            }
                        });
                    let sys_kicker = sys_resilient.kicker();
                    let sys_storm_app = app.clone();
                    let sys_notify = TapNotify {
                        on_stall: Some(Box::new(move || {
                            eprintln!("系统声音采集失联(>5s 无帧):静音填充维持时间轴,触发自愈重启");
                            let _ = sys_kicker.try_send(());
                        })),
                        on_recover: Some(Box::new(|| eprintln!("系统声音采集恢复"))),
                        // system 轨同样接:实测健康场次它恒为 0 断流,真叫起来
                        // 说明是系统采集侧出了事,同样该在录制中看见。
                        on_gap_storm: Some(Box::new(move |ratio| {
                            let _ = sys_storm_app.emit(
                                "source_health",
                                ipc::SourceHealthEvent {
                                    source: "system".into(),
                                    state: if ratio.is_some() { "gap_storm" } else { "gap_storm_over" }.into(),
                                    gap_pct: ratio.map(|r| (r * 100.0).round() as u32),
                                },
                            );
                        })),
                    };
                    let sys: Box<dyn AudioCapture> = Box::new(
                        TappedCapture::new_with_timeline_origin(
                            Box::new(sys_resilient),
                            Source::System,
                            TapPolicy::system_sck(),
                            sys_health.clone(),
                            sys_notify,
                            timeline_origin.clone(),
                        )
                        .with_drift(sys_drift.clone()),
                    );
                    session_health.push((Source::System, sys_health));
                    session_drift.push((Source::System, sys_drift));
                    sources.push((Source::System, sys, sys_seg));
                }
                Err(e) => {
                    // 硬承诺双轨：System 是必备源，VAD 构建失败不再静默跳过——记入
                    // pre_start_failed，走下方 Fix A 拆除路径（整场报错，不留仅 mic
                    // 的半场笔记）。VAD 构建失败非权限问题，classify_system 的既有
                    // "unauthorized" 判据不会命中，恒归类 unavailable（设备/组件问题）。
                    eprintln!("系统声音 VAD 构建失败: {e}");
                    pre_start_failed.push((Source::System, e.to_string()));
                }
            }
        }

        // Windows:系统声音走 WASAPI loopback(对默认输出设备建环回流)。无授权
        // 概念,失败即 unavailable 降级。静默期不回调由 TapPolicy::system_loopback
        // 的 250ms 补零维持时间轴;设备切换/流错误经 CaptureEvent 触发自愈重启,
        // 重启重新解析默认输出设备,天然跟随用户换设备。
        #[cfg(windows)]
        {
            match new_silero(&vad_path) {
                Ok(sys_seg) => {
                    let sys_factory: audio::resilient::CaptureFactory = Box::new(|| {
                        let (etx, erx) = crossbeam_channel::unbounded();
                        (
                            Box::new(audio::loopback::LoopbackCapture::with_events(etx))
                                as Box<dyn AudioCapture>,
                            erx,
                        )
                    });
                    let sys_health = Arc::new(SourceHealth::default());
                    // 同 mic:声明率待 start 后可知,惰性初始化(见上方 mic_drift 注释)。
                    let sys_drift = Arc::new(pipeline::drift_monitor::DriftMonitor::new(0));
                    let sys_resilient =
                        audio::resilient::ResilientCapture::new(sys_factory, {
                            let app = app.clone();
                            let health = sys_health.clone();
                            let app2 = app.clone();
                            audio::resilient::ResilientNotify {
                                on_recovered: Some(Box::new(move || {
                                    health.restarts.fetch_add(1, Ordering::Relaxed);
                                    // 兜住不等于没发生:重建成功一次,就说明这台机器上的采集链断过一次。
                                    telemetry::report_error(
                                        telemetry::ErrorKind::CaptureRebuild,
                                        "system 采集断连后重建成功",
                                    );
                                    let _ = app.emit(
                                        "source_health",
                                        ipc::SourceHealthEvent {
                                            source: "system".into(),
                                            state: "recovered".into(),
                                            gap_pct: None,
                                        },
                                    );
                                })),
                                on_lost: Some(Box::new(move || {
                                    // 重试耗尽 = 这一路音源本场彻底没了,是最该被看见的采集故障。
                                    telemetry::report_error(
                                        telemetry::ErrorKind::CaptureRebuild,
                                        "system 采集断连重试耗尽,本场放弃该源",
                                    );
                                    let _ = app2.emit(
                                        "source_health",
                                        ipc::SourceHealthEvent {
                                            source: "system".into(),
                                            state: "lost".into(),
                                            gap_pct: None,
                                        },
                                    );
                                })),
                            }
                        });
                    // 环回静默是常态(policy stall_after=None,tap 不判失联),
                    // 自愈只由 cpal 错误事件驱动,kicker 不接。
                    let sys: Box<dyn AudioCapture> = Box::new(
                        TappedCapture::new_with_timeline_origin(
                            Box::new(sys_resilient),
                            Source::System,
                            TapPolicy::system_loopback(),
                            sys_health.clone(),
                            TapNotify::none(),
                            timeline_origin.clone(),
                        )
                        .with_drift(sys_drift.clone()),
                    );
                    session_health.push((Source::System, sys_health));
                    session_drift.push((Source::System, sys_drift));
                    sources.push((Source::System, sys, sys_seg));
                }
                Err(e) => {
                    // 同上（macOS 分支注释）：硬承诺双轨下不再静默跳过，记入
                    // pre_start_failed 走 Fix A 拆除路径；Windows 无授权概念，恒 unavailable。
                    eprintln!("系统声音 VAD 构建失败: {e}");
                    pre_start_failed.push((Source::System, e.to_string()));
                }
            }
        }

        // 软件回声消除(WebRTC AEC3):capture_path=aec(默认)下 VPIO 不启动,改由本模块以
        // system 采集流为远端参考,把外放回声从 mic 波形里消掉——mic 路只剩本人声音,
        // 文本级回声去重链降级为兜底。仅 mic+system 双源齐备才有意义;初始化失败
        // 降级为无 AEC(行为同引入前),绝不挡录制。capture_path=vpio 逃生舱不叠加软件 AEC。
        // Windows 恒尝试:该平台无 VPIO 可选,软件 AEC 是唯一声学消回声路径
        // (当前为 stub,构造返回 Err → 走下方降级日志,文本级回声去重兜底)。
        let mut aec_roles: Vec<(Source, audio::aec::AecRole)> = Vec::new();
        if calibration {
            eprintln!("[标定模式] AEC 已停用(本场)");
        }
        // Codex review Fix 4(P2):标定模式下跳过软件 AEC 角色构建(aec_roles 留空)
        // ——原因见上方 calibration 声明处注释。
        if (use_aec_capture || cfg!(windows))
            && !calibration
            && sources.iter().any(|(s, _, _)| *s == Source::Mic)
            && sources.iter().any(|(s, _, _)| *s == Source::System)
        {
            // 二期:实时预对齐——蓝牙外放延迟(实测可漂至 1200ms)远超 AEC3 内置
            // 估计范围,由 AlignState 滑窗实测扣压参考;初值按当前输出设备给,
            // 之后实测接管。探测失败按非蓝牙(0ms),等同现状。
            let initial_predelay_ms = if audio::default_output_is_bluetooth() { 450 } else { 0 };
            match audio::aec::new_aligned_pair(16000, initial_predelay_ms) {
                Ok((render, capture, _align)) => {
                    eprintln!(
                        "软件回声消除已启用(WebRTC AEC3 + AGC2 + NS + 实时预对齐 初值{initial_predelay_ms}ms): system 路为参考,mic 路消回声"
                    );
                    aec_roles.push((Source::System, audio::aec::AecRole::Render(render)));
                    aec_roles.push((Source::Mic, audio::aec::AecRole::Capture(capture)));
                }
                Err(e) => {
                    eprintln!("软件回声消除初始化失败,本场降级为无 AEC(不影响录制): {e}");
                }
            }
        }
        let soft_aec_on = aec_roles.iter().any(|(_, r)| matches!(r, audio::aec::AecRole::Capture(_)));

        // 2.5) 创建/续录笔记落盘器（此后任何失败路径都要发 AbortSession 清理）。
        // 续录先握手再取锁:转码 worker 现在持锁覆盖整个转码窗口(见下方 worker 内
        // NoteLock::try_exclusive 处注释),若不先 cancel_and_wait 就直接调
        // NoteWriter::resume 去抢同一把 flock,在途转码会让 resume 拿锁失败,把「转码中」
        // 误判成「另一实例在录制/编辑」而拒绝续录。此处在加载线程、未持任何全局锁
        // （running/generation/session_slot 均未持有），符合「持全局锁时绝不调
        // cancel_and_wait 这类阻塞方法」的锁序纪律。notes_dir 解析失败就跳过握手，
        // 交给下面的 create/resume 走正常报错路径。
        if let NoteTarget::Resume(id) = &target {
            if let Ok(d) = notes_dir(&app) {
                transcode.cancel_and_wait(&d.join(id));
            }
        }
        // New → NoteWriter::create；Resume → NoteWriter::resume（meta 损坏/id 不存在 → Err）。
        let mut writer = match notes_dir(&app).and_then(|d| match &target {
            NoteTarget::New => store::writer::NoteWriter::create(&d, chrono::Local::now()),
            NoteTarget::Resume(id) => store::writer::NoteWriter::resume(&d, id),
        }) {
            Ok(w) => w,
            Err(e) => {
                stash_model(&recognizer_cache, recognizer);
                stash_model(&embedder_cache, retag(&speaker_model, embedder));
                let msg = match &target {
                    NoteTarget::New => {
                        format!("error: {}", tr!("创建笔记失败: {e}", "Failed to create note: {e}"))
                    }
                    NoteTarget::Resume(_) => format!(
                        "error: {}",
                        tr!("续录笔记失败: {e}", "Failed to resume note recording: {e}")
                    ),
                };
                return fail(&app, &running, &generation, my_gen, msg);
            }
        };
        // 引擎身份落盘(显性化):本场实际用于转写的引擎,云端记 "cloud:厂商"。
        // 2026-08-14 教训:选型与实际生效可能不一致,不落盘事后无从对证。
        // 身份必须向识别器实例本人要(engine_id),不能回头再读一次设置——本场用的
        // 可能是开录前预载的常驻实例,用户在预载与开录之间改过选型时,设置里的值
        // 与真正在跑的实例对不上,恰好毁掉本字段的取证价值(Codex review P2)。
        // 云端无本机实例,但 cfg 是本次开录的同一份快照,与 make_cloud_asr 同源。
        // 写失败只打日志——诊断信息,不挡开录。
        let engine = match recognizer.as_ref() {
            Some(r) => r.engine_id().to_string(),
            None => format!("cloud:{}", cfg.cloud_asr_provider),
        };
        if let Err(e) = writer.set_asr_engine(&engine) {
            eprintln!("引擎身份写入失败(不影响录制): {e}");
        }
        // —— 移交前一次性读完全部元信息(note_id/dir/base_ms/registry 快照):writer
        // 即将整体移交 lifecycle actor(单写者),此后本线程不得再持它的任何引用,
        // 一切写经信箱。——
        let note_id = writer.note_id().to_string();
        let note_dir = writer.dir().to_path_buf();
        // 续录前把该目录的音频解回 WAV，供本场从尾部对齐续写。必须先 cancel_and_wait：
        // 若转码 worker 此刻正把本目录的 wav 压成 m4a，解码会与它撞文件，故先摘队列 +
        // 阻塞等 in-flight 转完。锁序纪律：本调用点在加载线程、不持任何全局锁
        //（running/generation/session_slot 均未持有），符合「持全局锁时绝不调
        // cancel_and_wait 这类阻塞方法」。decode_note_to_wav 内部失败已降级打日志，无需包错。
        // 解码先行(在建 AudioTrackWriter 之前)：建档时要按既有 WAV 尾部长度对既有轨道做
        // 截断/零填充对齐，故必须先把已压缩音频解回 WAV。base_ms 本身来自 segments 时间轴
        //（下方 writer.base_ms()），与解码顺序无关。
        if let NoteTarget::Resume(_) = &target {
            transcode.cancel_and_wait(&note_dir);
            store::transcode::decode_note_to_wav(&note_dir);
        }
        // 续录时间轴偏移：New 路径恒 0；Resume 路径 = 续录前最大 end_ms。
        // on_final 落盘/emit 前 start_ms/end_ms 均 + base_ms（partial 无时间戳，不受影响）。
        let base_ms = writer.base_ms();
        let registry_snap = writer.registry_snapshot();
        // 标记本笔记的 mic 轨道已启用软件 AEC（离线清洗只认这类场次）。
        if soft_aec_on {
            if let Err(e) = store::audio::set_track_soft_aec(&note_dir, "mic") {
                eprintln!("软件AEC标记写入失败(不影响录制,本场将跳过离线清洗): {e}");
            }
        }
        // writer 所有权移交 lifecycle actor:装入 runner 的 Owned 槽后,append/说话人
        // 事件/改题/改名/收尾全部在 actor 线程串行执行。失败路径不再本地清理,改发
        // AbortSession(同信箱 FIFO,恒排在本会话已入队的管线消息之后)。
        let lc = app.state::<lifecycle::LifecycleHandle>().inner().clone();
        lc.report(lifecycle::machine::Msg::AdoptWriter { writer: Box::new(writer) });
        // 说话人编号/质心延续 + 库种子注入：快照（续录）优先，库中同 person 不重复注入。
        // 库加载失败降级为无种子，绝不挡录制。
        //
        // 种子门禁比对的是**本场开录时那份 speaker_model 快照**,与校验嵌入器用的是同一个。
        // 这里若重读当前设置,开录期间切模型就会拿 A 的嵌入器去比 B 库的种子,而门禁看新
        // 设置已经放行(codex review 实现轮六 P1)。
        let seeds = load_voiceprint_seeds_for(&app, &speaker_model);
        let mut registry =
            crate::diar::registry::SpeakerRegistry::with_seeds(&registry_snap, &seeds);
        // 说话人识别方法(设置项):与本场其余配置同一快照,场中改设置不影响进行中会话。
        registry.set_matcher(crate::diar::registry::matcher_from_key(&cfg.speaker_match));
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
            let enroll_model = speaker_model.clone();
            let enroll_note = note_id.clone();
            let enroll_nstore = store::NoteStore::new(match notes_dir(&app) {
                Ok(d) => d,
                Err(_) => std::path::PathBuf::new(),
            });
            registry.set_enroller(
                store::AUTO_ENROLL_MS,
                Box::new(move |snap| {
                    // 续录场景:该簇已被标多人混杂 → 不入库(标是上一场停录后打的)。
                    if enroll_nstore.multi_speaker_ids(&enroll_note).contains(&snap.id) {
                        eprintln!("实时入库跳过:{} 已标多人混杂", snap.id);
                        return None;
                    }
                    match vp_store_e.upsert_from_session_traced(
                        std::slice::from_ref(snap),
                        &chrono::Local::now().to_rfc3339(),
                        &enroll_model,
                        &enroll_note,
                    ) {
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

        // 3) 起会话。管线回调只发消息(writer 归 actor):on_final/on_diar 的 writer
        // 触发块已逐字搬进 actor 的 run_pipeline,回调侧仅保留不触 writer 的声纹库
        // 回写(见 on_diar 闭包注释)与时间戳偏移加定。
        let app_p = app.clone();
        let lc_f = lc.clone();
        let lc_d = lc.clone();
        // Pipeline 消息携带 note_id(P2 对账加固):双加载线程重叠窗口下(start→
        // 本线程卡住数秒→stop→start),本线程迟到的管线消息可能与届时槽内的新
        // 会话不是同一笔记——actor 侧按 note_id 核对,不匹配即丢弃,不误写。
        let note_id_f = note_id.clone();
        let note_id_d = note_id.clone();
        // 声纹库句柄：闭包前构造一次，供 Snapshot 分支停止时的入库回写。用 Option
        // 包裹而非兜底占位路径——app_data_dir 解析失败时彻底跳过库回写（None），
        // 而不是拿一个空/相对路径去读写，那样反而可能在意外位置产生副作用文件。
        // 停录快照入库用的模型标签,与实时入库同一份快照(见上方 speaker_model)。
        let snapshot_model = speaker_model.clone();
        let nstore_d = store::NoteStore::new(match notes_dir(&app) {
            Ok(d) => d,
            Err(_) => std::path::PathBuf::new(),
        });
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
        // keep_audio 开关已随三删一藏移除,固定保留音频——恒装配写盘器/写盘线程。
        //
        // 录制期产物装配移交 pipeline::recording_sink:mix_track 开启时多落一条
        // mixed.wav(方案 B)。
        //
        // sources 在此处只是"配置期建了 capture 对象"的源(可能活跃),真正的
        // capture.start() 在 spawn_session 里才跑,启动失败的源其 sink 随 worker 一起
        // 丢弃(session.rs)——mix_track 装配时无法区分"确实活跃"与"仅配置存在",
        // 这个缺口由 MixedSink 的队列/窗口守卫与收尾 seen 检查兜住(一源不喂料就
        // 放弃成品轨,不影响两条源轨),此处不为此调整启动顺序。
        let (audio_sinks, audio_joins, audio_activity) = {
            let srcs: Vec<Source> = sources.iter().map(|(s, _, _)| *s).collect();
            let w = pipeline::recording_sink::build_sinks_with_first_offsets(
                &note_dir,
                base_ms,
                &srcs,
                &session_health,
                mix_track,
            );
            (w.sinks, w.joins, w.activity)
        };

        // 识别引擎装配。云端两件调用方专属的注入(见 session::AsrEngine):
        //  - backfill_segmenter:断网缺口补识的本机 VAD 切段(厂商批式接口有单请求
        //    时长上限)。每次现造一只 SileroSegmenter——sherpa 的段起点是「本实例流内
        //    绝对样本号」,复用同一只会把上一个缺口的长度算进下一个缺口的偏移。补识是
        //    低频路径(一次断连一次),多一次模型构造换偏移正确,值。构造失败退化为整段
        //    一刀:厂商若因超长报错,worker 会落 [识别失败] 占位段留痕——绝不返回空段
        //    列表,那等于把这段发声静默丢掉。
        //  - on_status:重连/补识状态 → 前端状态条(session 层保持无 UI 依赖)。
        let engine = match cloud_asr {
            Some(asr) => {
                let vad_bf = vad_path.clone();
                let app_st = app.clone();
                session::AsrEngine::Cloud {
                    asr,
                    backfill_segmenter: Box::new(move |samples: &[f32]| {
                        match pipeline::silero::SileroSegmenter::new(&vad_bf) {
                            Ok(mut seg) => {
                                seg.accept(samples);
                                seg.flush();
                                seg.take_finished()
                                    .into_iter()
                                    .map(|s| (s.start as u64, s.samples))
                                    .collect()
                            }
                            Err(e) => {
                                eprintln!("补识切段器构造失败({e});整缺口按单段送批式");
                                vec![(0u64, samples.to_vec())]
                            }
                        }
                    }),
                    on_status: Box::new(move |st| {
                        let (state, source, message) = match st {
                            session::CloudAsrStatus::Reconnecting { source, message } => {
                                ("reconnecting", source, message)
                            }
                            session::CloudAsrStatus::Recovered { source } => {
                                ("recovered", source, None)
                            }
                            session::CloudAsrStatus::Backfilling { source } => {
                                ("backfilling", source, None)
                            }
                            session::CloudAsrStatus::BackfillFailed { source } => {
                                ("backfill_failed", source, None)
                            }
                        };
                        // 厂商/本机错误原文可能很长(带 requestId 的整串 JSON);状态条只
                        // 展示一行,这里按字符(而非字节)钳制,不能把多字节字符切成半个。
                        let message = message.map(|m| m.chars().take(200).collect::<String>());
                        let _ = app_st.emit(
                            "cloud-asr-status",
                            ipc::CloudAsrStatusEvent {
                                state: state.into(),
                                source: source.as_str().into(),
                                message,
                            },
                        );
                    }),
                }
            }
            None => session::AsrEngine::Local(
                recognizer.expect("本地模式在上方恒已取到识别器(取不到即已 fail 返回)"),
            ),
        };
        // language_filter:会议场景默认过滤中日韩误判幻觉段,多语会议可在设置里关闭以
        // 保留外语真实发言。值在上方与 use_aec_capture/mix_track 同一次 settings
        // load 读出(读取失败已保守回落默认过滤开,与 Settings::default 一致)。
        let start = session::start_session(
            sources,
            engine,
            embedder,
            registry,
            std::time::Duration::from_millis(session::ECHO_HOLD_MS),
            language_filter,
            16000,
            16000,
            audio_sinks,
            aec_roles,
            move |src, text, start_ms, end_ms, spk, rms| {
                // P2:定稿段转成消息入信箱(unbounded send 不阻塞,不反压 ASR 热路径),
                // 落盘/降级翻转/emit 由 actor 串行执行(run_pipeline,块逐字搬移)。
                // 续录偏移在此处加定:消息里恒为落盘口径的绝对时间轴,runner 不再加。
                let start_ms = start_ms + base_ms;
                let end_ms = end_ms + base_ms;
                lc_f.report(lifecycle::machine::Msg::Pipeline {
                    note_id: note_id_f.clone(),
                    op: lifecycle::machine::PipelineOp::Final {
                        source: src.as_str().into(),
                        text,
                        start_ms,
                        end_ms,
                        speaker: spk,
                        rms,
                    },
                });
            },
            move |src, text| {
                let _ = app_p.emit(
                    "partial",
                    ipc::PartialEvent { source: src.as_str().into(), text },
                );
            },
            move |ev| {
                // P2 拆分决策:触 writer 的四分支块(SpeakersChanged/Merged/EchoRetract/
                // Snapshot 的 store_centroids)逐字搬进 actor 的 run_pipeline,经消息串行;
                // 不触 writer 的声纹库回写/样本落盘(vp_store_d/live_enrolled 只在此消费)
                // 留在本回调线程原地执行——库自带 VP_LOCK 全局互斥,不依赖 writer 锁。
                let ev = match ev {
                    session::DiarEvent::EchoRetract { start_ms, end_ms, text } => {
                        // 时间戳加续录偏移,与 on_final 同口径在发送侧加定:消息里恒为
                        // 落盘口径的绝对时间轴,runner 侧不再二次加 base_ms。
                        session::DiarEvent::EchoRetract {
                            start_ms: start_ms + base_ms,
                            end_ms: end_ms + base_ms,
                            text,
                        }
                    }
                    session::DiarEvent::SuppressedFinal {
                        source,
                        text,
                        start_ms,
                        end_ms,
                        rms,
                        reason,
                    } => session::DiarEvent::SuppressedFinal {
                        source,
                        text,
                        start_ms: start_ms + base_ms,
                        end_ms: end_ms + base_ms,
                        rms,
                        reason,
                    },
                    session::DiarEvent::Snapshot { mut snaps, samples } => {
                        // 库回写/够料入库（spec:person 簇加权回写；无主簇 ≥10s 入库为未命名人）。
                        // 失败只降级打日志:库是增值层,绝不影响笔记落盘。Snapshot 在 worker
                        // join 前送达(入队),故恒先于停录自投的 Finalize 被 actor 处理,
                        // person_id 随 finalize 落盘。
                        if let Some(store) = &vp_store_d {
                            // 「多人混杂」簇不入库不写样本(打标是事后行为,续录/重转写
                            // 后同一 S 再攒出快照时这里兜住——codex 实现轮一 P1①)。
                            let multi = nstore_d.multi_speaker_ids(&note_id_d);
                            if !multi.is_empty() {
                                snaps.retain(|sn| {
                                    let keep = !multi.contains(&sn.id);
                                    if !keep {
                                        eprintln!("声纹入库跳过:{} 已标多人混杂", sn.id);
                                    }
                                    keep
                                });
                            }
                            match store.upsert_from_session_traced(
                                &snaps,
                                &chrono::Local::now().to_rfc3339(),
                                &snapshot_model,
                                &note_id_d,
                            ) {
                                Ok(enrolled) => {
                                    // 原 set_speaker_person(cluster, person) 循环改为把新关联
                                    // 注进 snaps[].person 随消息走:runner 的 store_centroids
                                    // 落表时一并写 person_id,终态逐位等价(enrolled 只含
                                    // person 原为 None 的新入库簇)。
                                    for snap in &mut snaps {
                                        if let Some(pid) = enrolled.get(&snap.id) {
                                            snap.person = Some(pid.clone());
                                        }
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
                                        // 收口版:resolve/隔离/老熟人检查、WAL 溯源、写文件同一临界区。
                                        // 旧代码在这里用未 resolve 的 pid 查"有没有样本",笔记持
                                        // loser id 时会误判无样本、把样本重复写到 winner(设计轮二 P1②)。
                                        if let Err(e) = store.append_session_sample(
                                            &pid,
                                            sample,
                                            &note_id_d,
                                            &snap.id,
                                            newly_enrolled,
                                        ) {
                                            eprintln!("声纹样本写入失败({pid},不影响笔记): {e}");
                                        }
                                    }
                                }
                                Err(e) => eprintln!("声纹库回写失败(不影响笔记): {e}"),
                            }
                        }
                        // samples 已在上方消费完,不随消息复运(嵌入样本可达 MB 级)。
                        session::DiarEvent::Snapshot { snaps, samples: Vec::new() }
                    }
                    other => other,
                };
                lc_d.report(lifecycle::machine::Msg::Pipeline {
                    note_id: note_id_d.clone(),
                    op: lifecycle::machine::PipelineOp::Diar(ev),
                });
            },
            {
                let app_l = app.clone();
                Some(std::sync::Arc::new(move |source: crate::audio::Source, rms: f32| {
                    let _ = app_l.emit("level", ipc::LevelEvent { source: source.as_str().into(), rms });
                }) as std::sync::Arc<dyn Fn(crate::audio::Source, f32) + Send + Sync>)
            },
        );

        match start {
            Ok(start) => {
                // Fix A(泛化): required_sources 里的每个源都必备——任一未出现在 active
                // 就整场拆除报错(不静默降级)。硬承诺双轨下 required=[Mic, System]
                // （先 stop 排干可能已产生的其它源 finals → join audio_joins → stash
                // 模型 → AbortSession → 带源名+分类 fail)。
                if let Some(&missing) = required_sources()
                    .iter()
                    .find(|s| !start.active.contains(s))
                {
                    // 装配阶段失败(pre_start_failed,VAD 构建等)与 start_session 内
                    // capture 启动阶段失败(start.failed)分属两个阶段,合并起来供
                    // missing_source_error 查找——否则 VAD 构建失败的 System 既不在
                    // active 也不在 start.failed 里,分类会漏判。真缺源才需要这份
                    // 合并,克隆放在本分支内,不给源齐备的正常路径背这两次 clone。
                    let all_failed: Vec<(Source, String)> = pre_start_failed
                        .iter()
                        .cloned()
                        .chain(start.failed.iter().cloned())
                        .collect();
                    let (r, e) = start.handle.stop(); // 先排干可能已产生的其它源 finals
                    stash_model(&recognizer_cache, r);
                    stash_model(&embedder_cache, retag(&speaker_model, e));
                    // 镜像正常停止路径(do_stop_teardown 里 `for j in s.audio_joins`
                    // 那段):分段 worker 已随 handle.stop() join → audio sink 已 drop →
                    // 写盘线程排干无界队列后自退,这里 join 等它们真正退出,确保文件
                    // 句柄已关闭。硬承诺双轨把这条拆除路径推成"mic 已在满速写盘、
                    // System 却起不来"的常见触发点——不 join 会在 Windows 上让后续
                    // remove_dir_all(如删除/清理该笔记目录)撞上仍打开的句柄,留下
                    // 删不掉的孤儿目录。
                    for j in audio_joins {
                        let _ = j.join();
                    }
                    // 排干的 finals 已作为 Pipeline 消息入队(worker 已 join,happens-before
                    // 本条投递),abort 恒在它们之后执行——内容先落盘再按 abort 语义收尾。
                    // note_id 携带本会话身份(P2 对账加固):actor 侧核对与槽内是否一致。
                    lc.report(lifecycle::machine::Msg::AbortSession { note_id: note_id.clone() });
                    let err = missing_source_error(missing, &all_failed);
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
                    stash_model(&embedder_cache, retag(&speaker_model, e));
                    // 镜像上方 Fix A 拆除路径与正常停止路径(do_stop_teardown 里
                    // `for j in s.audio_joins` 那段)的 join:分段 worker 已随
                    // handle.stop() join → audio sink 已 drop → 写盘线程排干无界队列后
                    // 自退,这里 join 等它们真正退出,确保文件句柄已关闭——这条兄弟拆除
                    // 路径同样可能撞上 Windows 上删不掉的孤儿目录。audio_joins 此刻仍属
                    // 本函数所有(直到下方成功路径才移交进 ActiveSession),可安全消费,
                    // 于是本函数内两条 abort 路径与正常停止路径三处共守同一 join 不变式。
                    for j in audio_joins {
                        let _ = j.join();
                    }
                    // 被 stop/新 start(/resume) 抢先:经信箱 abort——有内容则收尾保全
                    // (flush 失败时留 recording)。排干的 finals 先于本条入队,不丢内容。
                    // note_id 携带本会话身份(P2 对账加固):actor 侧核对与槽内是否一致。
                    lc.report(lifecycle::machine::Msg::AbortSession { note_id: note_id.clone() });
                    return;
                }
                drop(gen_guard);
                let system_audio = classify_system(&start.active, &start.failed);
                // P1 影子回报用:在入槽块前克隆,入槽/emit 各自 clone 不受影响。
                let note_id_for_report = note_id.clone();
                *session_slot.lock().unwrap() = Some(ActiveSession {
                    handle: start.handle,
                    note_id: note_id.clone(),
                    system_audio: system_audio.clone(),
                    diarization: diarization.clone(),
                    speaker_model: speaker_model.clone(),
                    input_override: input_override_for_session.clone(),
                    started: std::time::Instant::now(),
                    base_ms,
                    paused_at: None,
                    paused_accum: std::time::Duration::ZERO,
                    audio_joins,
                    audio_activity,
                    health: session_health,
                    drift: session_drift,
                    actual_rate_stop: actual_rate_stop_tx,
                    note_dir: note_dir.clone(),
                });
                drop(running_guard);
                let _ = app.emit(
                    "status",
                    ipc::StatusEvent { state: "recording".into(), system_audio, note_id: note_id.clone(), diarization, elapsed_ms: base_ms, input_override: input_override_for_session.clone() },
                );
                // P1 影子回报:会话已真实入槽并广播 recording,通知 actor 内核演进。
                // 本回报来自后台加载线程,只投递不等待(见 actor.rs 死锁注记②)。
                // 托盘红点态(图标+菜单文案「停止录制」)不再在此直调:actor 内核收到
                // 本回报后 Starting→Recording 迁移落地,TrayHook 经 hook 总线驱动
                // (P3 consumers.rs)。翻转时点从「emit 后紧邻」变为「actor 处理完
                // 本条消息后」,同为毫秒级异步投递,不可感知。
                app.state::<lifecycle::LifecycleHandle>()
                    .report(lifecycle::machine::Msg::SessionStarted { note_id: note_id_for_report });
            }
            Err(se) => {
                stash_model(&recognizer_cache, se.recognizer);
                stash_model(&embedder_cache, retag(&speaker_model, se.embedder));
                // 会话未能启动:经信箱 abort(此路径无 worker,不存在在途管线消息)。
                // note_id 携带本会话身份(P2 对账加固):actor 侧核对与槽内是否一致。
                lc.report(lifecycle::machine::Msg::AbortSession { note_id: note_id.clone() });
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
        return Err(tr!("正在迁移或下载,稍后再试", "Migration or download in progress; try again later").into());
    }
    // 全局互斥于重转写(不限本篇,双向对称——do_retranscribe 侧对称判断 running/session
    // 是否存在来拒绝重转写)。这里只是快速失败的 UX——权威判定(Dekker 写后读)在
    // spawn_session 内、running 置 true 之后再读一次同一把槽,堵掉本次检查与 running
    // 置位之间的加载窗口。
    if state.retranscribing.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
        return Err(tr!(
            "重转写进行中,完成后再录制",
            "A re-transcription is in progress; please record after it finishes"
        ));
    }
    // 模式感知就绪判定(与设置页/托盘同一份):本地看所选模型齐不齐,云端看 vad + 凭证。
    if !current_models_status(app).recording_ready {
        return Err(recording_not_ready_msg(app));
    }
    let result = spawn_session(
        app.clone(),
        state.running.clone(),
        state.generation.clone(),
        state.session.clone(),
        state.recognizer_cache.clone(),
        state.embedder_cache.clone(),
        state.transcode.clone(),
        state.retranscribing.clone(),
        state.mixed_regen.clone(),
        NoteTarget::New,
    );
    if result.is_ok() {
        // record_system_only 已随三删一藏移除,不再有"仅系统声"录制形态可推断源类别；
        // Task 3(硬承诺双轨)落地后 Mic+System 是必备源集合,能走到这里(result.is_ok())
        // 就意味着两源皆已启动——固定按 Both 上报不再是近似,而是准确值。
        telemetry::track(app, telemetry::Event::RecordingStarted { source: telemetry::RecordSource::Both });
    }
    result
}

#[tauri::command]
fn start_recording(app: AppHandle) -> Result<(), String> {
    // 薄壳(P1 改道):经 lifecycle actor 信箱串行执行,执行体仍是 do_start_recording。
    app.state::<lifecycle::LifecycleHandle>()
        .command(lifecycle::Cmd::Start { resume_id: None })
}

/// 续录一场非活动（已中断或已完成）笔记的共用实现：运行守卫与 do_start_recording
/// 完全一致（同一份 spawn_session 实现），仅 target 换成 Resume(note_id)。逐语句搬自
/// 原 resume_recording 命令体,唯一改动是 state 由 `app.state()` 取(与 `State<AppState>`
/// 注入等价)、app 因签名为 &AppHandle 而在传入 spawn_session 时 clone——逻辑零变化。
///
/// refining(P3):该笔记是否正在 Aing,由 actor 执行 Delegate 时从内核 Aing 集读出
/// 传入(本函数在 actor 线程上运行,数据源即内核、同一消息处理内快照一致)。守卫
/// 留在此处而非内核抢答,是为逐位还原旧判定顺序:下载→Aing→模型,谁先判谁先报。
fn do_resume_note_recording(app: &AppHandle, note_id: String, refining: bool) -> Result<(), String> {
    let state = app.state::<AppState>();
    // 同 start_recording:迁移/下载进行中不能开录(见该处注释)。
    if state.download_running.load(Ordering::SeqCst) {
        return Err(tr!("正在迁移或下载,稍后再试", "Migration or download in progress; try again later").into());
    }
    // F1 修复:该笔记正在 Aing 中就拒绝续录——Aing 完成后才 transcode.enqueue,而续录
    // 先 cancel_and_wait 再向 mic.wav 追加写;若放行,Aing 收尾时才入队的转码会把
    // 「活跃在追加」的 WAV 编码后删除,续录段音频永久丢失。
    if refining {
        return Err(tr!(
            "该笔记正在 Aing,请稍后再试",
            "This note is being refined by AI; try again later"
        ));
    }
    // 全局互斥于重转写(不限本篇——升级自"仅同笔记"的旧检查):重转写与实时 ASR 各起
    // 一套 ORT 管线,叠跑抢核,这条互斥与 do_start_recording 一样是双向对称的
    // (do_retranscribe 侧对称判断 running/session 是否存在来拒绝重转写)。同笔记场景下
    // 还叠加一层理由:重转写持 NoteLock 全程,续录也要写 mic.wav,放行会在
    // NoteWriter::resume 拿锁失败才报错——此处提前拒绝把错误提到「点续录就说清」;
    // 即便这里漏检,NoteLock 兜底仍在(resume 会因锁失败拒绝)。这里只是快速失败的
    // UX——权威判定(Dekker 写后读)在 spawn_session 内、running 置 true 之后。
    if state.retranscribing.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
        return Err(tr!(
            "重转写进行中,完成后再录制",
            "A re-transcription is in progress; please record after it finishes"
        ));
    }
    // 模式感知就绪判定(与设置页/托盘同一份):本地看所选模型齐不齐,云端看 vad + 凭证。
    if !current_models_status(app).recording_ready {
        return Err(recording_not_ready_msg(app));
    }
    let result = spawn_session(
        app.clone(),
        state.running.clone(),
        state.generation.clone(),
        state.session.clone(),
        state.recognizer_cache.clone(),
        state.embedder_cache.clone(),
        state.transcode.clone(),
        state.retranscribing.clone(),
        state.mixed_regen.clone(),
        NoteTarget::Resume(note_id),
    );
    if result.is_ok() {
        // record_system_only 已随三删一藏移除,不再有"仅系统声"录制形态可推断源类别；
        // Task 3(硬承诺双轨)落地后 Mic+System 是必备源集合,能走到这里(result.is_ok())
        // 就意味着两源皆已启动——固定按 Both 上报不再是近似,而是准确值。
        telemetry::track(app, telemetry::Event::RecordingStarted { source: telemetry::RecordSource::Both });
    }
    result
}

#[tauri::command]
fn resume_recording(app: AppHandle, note_id: String) -> Result<(), String> {
    // 薄壳(P1 改道):经 lifecycle actor 信箱串行执行,执行体仍是 do_resume_note_recording。
    app.state::<lifecycle::LifecycleHandle>()
        .command(lifecycle::Cmd::Start { resume_id: Some(note_id) })
}

fn persist_track_sync(
    note_dir: &std::path::Path,
    base_ms: u64,
    wall_ms: u64,
    health: &[(Source, Arc<SourceHealth>)],
    activity: &[(Source, Arc<AtomicBool>)],
) {
    for (source, health) in health {
        // [诊断插桩 2026-08-07] 首帧偏移落日志:分解 drift_ms 里的起流差成分,
        // 供实时 AEC 失效调查区分"起流差"与"持续时钟漂移"。
        eprintln!(
            "[对账诊断] {} 首帧偏移(相对最早源) {}ms",
            source.as_str(),
            health.first_frame_offset_16k() / 16
        );
        let wrote_current_audio = activity
            .iter()
            .find(|(candidate, _)| candidate == source)
            .map(|(_, wrote)| wrote.load(Ordering::Acquire))
            .unwrap_or(false);
        if !wrote_current_audio {
            continue;
        }

        let h = health.snapshot(*source);
        // 轨时长必须量 WAV,不能拿 h.samples 换算:后者是设备原生率、交错多声道的原始
        // 计数,且在暂停闸之前累加。调用方已 join writer,文件长度是终值。
        let Some(track_ms) = store::audio::session_track_ms(note_dir, source.as_str(), base_ms)
        else {
            continue;
        };
        let info = store::audio::SyncInfo {
            wall_ms,
            samples: h.samples,
            track_ms,
            drift_ms: store::audio::drift_ms(track_ms, wall_ms),
            silence_ms: h.silence_ms,
            gaps: h.gaps,
            rate_fixes: h.rate_fixes,
            hw_gaps: h.hw_gaps,
            hw_holes: h.hw_holes,
            hw_gap_ms: h.hw_gap_ms,
            cap_dropped_samples: h.cap_dropped_samples,
            cap_queue_hw: h.cap_queue_hw,
            send_wait_ms: h.send_wait_ms,
            send_wait_max_ms: h.send_wait_max_ms,
            first_frame_offset_ms: Some(health.first_frame_offset_16k() / 16),
        };
        if let Err(e) = store::audio::set_track_sync(note_dir, source.as_str(), info) {
            eprintln!("对账写入失败({}): {e}", source.as_str());
        }
    }
}

/// 停录 teardown(P2 上半,原 do_stop_recording 的拆除段逐语句搬移):running 复位、
/// generation 递增、取会话、时长埋点、handle.stop 排干、模型归还、音频写盘线程 join。
/// finalize 不在这里——writer 归 lifecycle actor,由调用方(actor 的 Cmd::Stop 特化
/// 分支)在本函数返回后自投 Finalize{note_id}:该消息排在排干期间入队的全部管线消息
/// 之后,「先落盘后收尾」由信箱 FIFO 保证。返回 None=本就无会话(空停)。
pub(crate) fn do_stop_teardown(app: &AppHandle) -> Option<String> {
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
    let s = sess?;
    // 埋点先取时长:下面 s.handle.stop() 起会逐字段搬空 s,搬空后不能再整体借用取
    // elapsed_ms(&self)(partial move 借用检查会拒绝),故须在任何字段搬走之前算好。
    // 续录笔记 elapsed_ms 含 base_ms(历史累计)——上报的是笔记累计时长而非本次会话时长,看板解读以此为准。
    telemetry::track(app, telemetry::Event::RecordingStopped { duration_ms: s.elapsed_ms() });
    // 对账用的墙钟必须在这里取,不能等到下面 join 完再取:handle.stop() 要 join ASR 线程
    // (等尾段识别跑完,云端还可能叠上重连/补识往返),audio_joins 要排干无界写盘队列
    // (它存在的理由正是"磁盘可能卡顿数秒")——这段拆解耗时既无上界也未被测量,算进
    // wall_ms 就是给 drift_ms 加一段大到能翻转符号的负偏置,让 SyncInfo 文档里那份
    // 偏置清单(启动窗等,量级 ≤ 数百 ms)失效,首次冒烟的读数会被解释反。
    // 与之相对,track_ms 必须留在 join 之后取——那时 WAV 头才收尾,文件长度才是终值。
    // base_ms 传 0:对账描述"这一场",不含历史累计。
    let wall_ms = active_elapsed_ms(
        s.started.elapsed(),
        s.paused_accum,
        s.paused_at.map(|p| p.elapsed()),
        0,
    );
    // 标签取自**本场开录时**那一份快照,不是现读设置:running 早在 stop() 之前就置回
    // false,排干期间允许切模型,现读会把 A 建的实例标成 B(codex review 实现轮 P1)。
    let session_model = s.speaker_model.clone();
    let (returned, embedder) = s.handle.stop(); // 排干 finals：所有 append 消息在此全部入队
    stash_model(&state.recognizer_cache, returned);
    stash_model(&state.embedder_cache, retag(&session_model, embedder));
    // 本场录制中发生过声纹改名:归还件的 Qwen3 热词已过期,丢弃,停录收尾的
    // preload 会按新名单重建(见 refresh_qwen_hotwords_cache 的脏标记分支)。
    if state.hotwords_dirty.swap(false, Ordering::Relaxed) {
        *state.recognizer_cache.lock().unwrap() = None;
    }
    // 分段 worker 已 join → audio sink 已 drop → 写盘线程排干后自退,join 保证
    // finalize 前 WAV 头已收尾(正常情况下队列近空,瞬时完成)。
    for j in s.audio_joins {
        let _ = j.join();
    }
    // 只覆盖本场 writer 真正成功追加过的源。配置过但启动失败、以及活跃却无帧的
    // 续录都保留旧 sync,不会拿旧 WAV 配本场零计数造假。
    persist_track_sync(&s.note_dir, s.base_ms, wall_ms, &s.health, &s.audio_activity);
    // 时钟漂移传感器一期:只测不动数据,报告只写 note_dir、不进 telemetry;
    // 失败/异常都只 eprintln,绝不影响停录主流程(旁路纪律)。
    match pipeline::drift_monitor::persist_report(&s.note_dir, &s.drift) {
        Ok(anomalies) => {
            for a in anomalies {
                eprintln!("[drift] 异常: {a}");
            }
        }
        Err(e) => eprintln!("[drift] 报告写入失败: {e}"),
    }
    // 诊断档案(2026-08-23 数据积累):从盘上产物汇总 diagnostics.json,后续开发
    // 先分析再设计。延时 3s:scene.json 经 actor 信箱异步落盘,给它让路;纯观测,
    // 失败只打日志(旁路纪律同 drift)。
    {
        let dir = s.note_dir.clone();
        let cap = store::diagnostics::CaptureMeta {
            capture_path: {
                let p = app
                    .path()
                    .app_data_dir()
                    .map(|d| settings::load(&d).capture_path)
                    .unwrap_or_default();
                format!("{p:?}").to_lowercase()
            },
            input_override: s.input_override.clone(),
            speaker_model: session_model.clone(),
            erle_last_db: audio::aec::latest_erle_db(),
        };
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(3));
            let now = chrono::Local::now().to_rfc3339();
            if let Err(e) = store::diagnostics::compute_and_save(&dir, cap, &now) {
                eprintln!("[diag] 诊断档案写入失败(忽略): {e}");
            }
        });
    }
    Some(s.note_id)
}

/// 停录尾段(P2 下半,原 do_stop_recording 的收尾段逐语句搬移):emit stopped、补预载。
/// 有会话路径由 actor 的 DoFinalize 执行器在 finalize 之后调用(stopped 恒在
/// finalize 之后,与旧实现顺序一致);空停路径由 actor 的 Cmd::Stop 分支直接调用
/// (note_id 空串,与旧实现「无会话也发 stopped」一致)。
/// 托盘回 idle 态不再在此直调:有会话路径随 DoFinalize 前的状态迁移
/// (Recording/Stopping→Idle)经 hook 总线驱动(P3 consumers.rs::TrayHook);
/// 空停路径本就没有真实迁移(从未进过 Recording,托盘本来就是 idle 态),
/// 故无需补触发。
pub(crate) fn do_stop_tail(app: &AppHandle, note_id: String) {
    let state = app.state::<AppState>();
    let _ = app.emit(
        "status",
        ipc::StatusEvent { state: "stopped".into(), system_audio: String::new(), note_id, diarization: String::new(), elapsed_ms: 0, input_override: String::new() },
    );
    // 停录补预载：录制中下载完成的模型（预载被活跃跳过）此刻补进空槽；幂等，槽有货即跳。
    preload_models(app.clone(), state.session.clone(), state.recognizer_cache.clone(), state.embedder_cache.clone());
}

#[tauri::command]
async fn stop_recording(app: AppHandle) -> Result<(), String> {
    // 停录会排干分段/ASR worker、收尾 WAV 并 finalize 笔记，这些都是有意保留的
    // 阻塞操作。同步命令会占住 Tauri IPC 执行路径，在 Windows 上表现为整个 WebView
    // 无法重绘。把完整的 durable shutdown 移到阻塞线程池：数据完整性和 reply=已落盘
    // 的语义不变，但 UI 事件循环始终可响应。
    let lifecycle = app.state::<lifecycle::LifecycleHandle>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        lifecycle.command(lifecycle::Cmd::Stop)
    })
    .await
    .map_err(|e| tr!("停止录制后台任务异常: {e}", "Stop-recording background task failed: {e}"))?
}

/// 快捷键共用的录制切换:running 为真则停,否则开。开录失败只 eprintln——快捷键触发
/// 没有 UI 上下文,错误无处弹窗(设置缺失/模型未就绪等),静默进日志避免打断用户。
/// running 读取用 statement-scoped 的锁,读完即放,不与 do_* 内部锁嵌套。
pub(crate) fn toggle_recording(app: &AppHandle) {
    let running = *app.state::<AppState>().running.lock().unwrap();
    let lc = app.state::<lifecycle::LifecycleHandle>();
    if running {
        // P1 改道:经 actor 串行(委托 do_stop_recording,恒 Ok);Err 仅 actor 退出时出现。
        // 停录是持续数秒的 durable shutdown,而托盘菜单/全局快捷键回调跑在事件循环线程,
        // 同步等待会冻结 WebView(与 stop_recording 命令同因):丢阻塞线程池,错误照旧进日志。
        let lc = lc.inner().clone();
        tauri::async_runtime::spawn_blocking(move || {
            if let Err(e) = lc.command(lifecycle::Cmd::Stop) {
                eprintln!("快捷键触发停录失败(静默进日志): {e}");
            }
        });
    } else if let Err(e) = lc.command(lifecycle::Cmd::Start { resume_id: None }) {
        eprintln!("快捷键触发开录失败(静默进日志): {e}");
    }
}

/// 当前系统麦克风模式("standard" / "wide_spectrum" / "voice_isolation" / "unknown")。
/// 录制页据此告警:语音突显会把非人声削成绝对零(见 audio::mic_mode 文档)。
/// 只报状态不拦录制——它是提示,不是门禁;读不到一律 "unknown",UI 不提示。
#[tauri::command]
fn mic_mode() -> &'static str {
    crate::audio::mic_mode::active().as_str()
}

/// 开录前风险清单(空 = 可以直接开录)。前端两个开录按钮据此弹一次确认。
///
/// 为什么由后端组合而不是前端调两次:拦截点要的是"有没有风险"这**一个**判断,
/// 把组合放这里,将来别的入口读同一份真值,不各自拼一遍。判定本身是纯函数
/// (`precheck::record_risks`),这里只负责把两个系统查询读出来喂给它。
///
/// 注意它与 `mic_mode` 的分工:那个只报状态供横幅用,这个是开录路径上的门。
/// 「提示不是门禁」的旧约定被推翻了一半——现在拦一次,但用户始终能选择继续
/// (依据见 docs/superpowers/specs/2026-08-17-precord-risk-gate-design.md)。
#[tauri::command]
fn precheck_recording(app: AppHandle) -> Vec<precheck::RecordRisk> {
    let bt = crate::audio::default_input_is_bluetooth();
    // 自动择优接管时蓝牙不再弹窗(开录时会真的换设备并出横幅);关了开关或没有
    // 替代设备,风险如实照弹。
    let auto = app
        .path()
        .app_data_dir()
        .map(|d| settings::load(&d).auto_input_pick)
        .unwrap_or(true);
    let bt_effective = bt && !(auto && crate::audio::pick_non_bluetooth_input().is_some());
    precheck::record_risks(crate::audio::mic_mode::active(), bt_effective)
}

/// 前端播放会话开/关的告知(与迷你浮层同源判定):托盘据此增删「停止播放」项。
/// 为什么由前端说:「有没有在播」的产品语义是会话——进笔记页就自动装载内核,拿后端的
/// 装载态判会让只看过没播过的笔记也在托盘冒出「停止播放」。
/// 只在真变化时重建菜单:前端 effect 可能因会话对象换引用(改名/重装)重发同一个值。
#[tauri::command]
fn set_playback_active(app: AppHandle, state: State<AppState>, active: bool) {
    if state.playback_active.swap(active, Ordering::SeqCst) != active {
        tray::refresh_menu(&app);
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
            input_override: s.input_override.clone(),
        },
        None => ipc::StatusEvent {
            state: "idle".into(),
            system_audio: String::new(),
            note_id: String::new(),
            diarization: String::new(),
            elapsed_ms: 0,
            input_override: String::new(),
        },
    }
}

/// 暂停共用实现(命令壳、UDS 桥共用)。逐语句搬自原 pause_recording 命令体,唯一改动是
/// state 由 `app.state()` 取(与 `State<AppState>` 注入等价)——逻辑零变化。
fn do_pause_recording(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let ev = {
        let mut slot = state.session.lock().unwrap();
        let Some(s) = slot.as_mut() else {
            return Err(tr!("没有正在进行的录制", "No recording in progress"));
        };
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
            input_override: s.input_override.clone(),
        }
    };
    let _ = app.emit("status", ev);
    // 托盘菜单要把「暂停录制」换成「恢复录制」。lifecycle 的托盘钩子对
    // (Recording, Recording) 这类转移一律返回 None(不为暂停翻转重建菜单),
    // 所以这里显式刷一次。会话锁已在上面的块结束时释放,与 refresh_menu 读的
    // running 锁不嵌套。
    tray::refresh_menu(app);
    // 图标动画同理:边沿在暂停路上不存在,必须在这里显式停(set_anim_paused 注释)。
    tray::set_anim_paused(app, true);
    Ok(())
}

#[tauri::command]
fn pause_recording(app: AppHandle) -> Result<(), String> {
    // 薄壳(P1 改道):经 lifecycle actor 信箱串行执行,执行体仍是 do_pause_recording。
    app.state::<lifecycle::LifecycleHandle>().command(lifecycle::Cmd::Pause)
}

/// 「就此结束」:把已中断笔记免续录收尾(2026-08-26 用户实报:为收尾假录一两秒不合理)。
///
/// 守卫最小集,与停止尾巴同权:
/// - 该笔记正被活动会话占用(续录已重开)→ 拒绝;**别的笔记在录不挡**——Aing/转码
///   本就允许与新录制并行,重叠由 AING_GATE(Aing 全局串行)与 spawn_refine 内的
///   F1 守卫(入队前复查活跃会话)兜底。
/// - 幂等由 store 层保证:非 recording 态返回 false,这里就不再补跑尾巴。
/// - 与续录的竞争:两入口都不经内核仲裁,窗口极窄且两个结局都自洽——续录先到则
///   本命令的会话守卫拒绝;本命令先写 complete 而用户随后续录,writer 的
///   open_resume 会把 state 改回 recording,F1 守卫让转码不碰追加中的 WAV。
///
/// 声纹**不补**(见 NoteStore::finalize_interrupted 注释):实时入库在录制期间已
/// 发生,停止时的加权回写消费内存会话态,随崩溃丢失——不从快照伪造。
#[tauri::command]
fn finalize_interrupted_note(app: AppHandle, state: State<AppState>, note_id: String) -> Result<(), String> {
    let occupied = state
        .session
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.note_id == note_id)
        .unwrap_or(false);
    if occupied {
        return Err(tr!("该笔记正在录制中,请先停止", "This note is currently recording; stop it first"));
    }
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    let changed = store::NoteStore::new(dir)
        .finalize_interrupted(&note_id)
        .map_err(|e| e.to_string())?;
    if changed {
        // 与首次停录同一条尾巴:云端二遍(若配)→ 会后 Aing → 转码入队。
        spawn_refine(app, note_id, true);
    }
    Ok(())
}

/// 续录共用实现(命令壳、UDS 桥共用)。逐语句搬自原 unpause_recording 命令体,唯一改动是
/// state 由 `app.state()` 取(与 `State<AppState>` 注入等价)——逻辑零变化。
fn do_resume_recording(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let ev = {
        let mut slot = state.session.lock().unwrap();
        let Some(s) = slot.as_mut() else {
            return Err(tr!("没有正在进行的录制", "No recording in progress"));
        };
        let Some(p) = s.paused_at.take() else { return Ok(()) }; // 未暂停：幂等
        s.paused_accum += p.elapsed();
        s.handle.set_paused(false);
        ipc::StatusEvent {
            state: "recording".into(),
            system_audio: s.system_audio.clone(),
            note_id: s.note_id.clone(),
            diarization: s.diarization.clone(),
            elapsed_ms: s.elapsed_ms(),
            input_override: s.input_override.clone(),
        }
    };
    let _ = app.emit("status", ev);
    tray::refresh_menu(app); // 同 do_pause_recording:把「恢复录制」换回「暂停录制」
    tray::set_anim_paused(app, false); // 恢复抖动:边沿在暂停路上不存在,显式起
    Ok(())
}

#[tauri::command]
fn unpause_recording(app: AppHandle) -> Result<(), String> {
    // 薄壳(P1 改道):经 lifecycle actor 信箱串行执行,执行体仍是 do_resume_recording。
    app.state::<lifecycle::LifecycleHandle>().command(lifecycle::Cmd::Unpause)
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

/// 手动（重）触发一次会后 Aing：录制中该 id 拒绝（内容未定稿，段落还在变），正在 Aing 中
/// 也拒绝（并发跑两遍纯浪费且会互相覆盖 refined.json）。两条守卫已入 lifecycle 内核
/// （Msg::RefineRequest 裁决，文案逐字不变）；通过则内核置 Running 并以 DoSpawnRefine
/// 调回 spawn_refine——手动重跑时 m4a 早已在盘上（首次 Aing 已经移交过转码），故
/// enqueue_transcode 恒 false，不再重复入队。
/// Aing 逐块进度事件(界面画「精修中 3/8 · 约剩 4 分」)。avg_chunk_ms 后端算,
/// 前端只乘剩余块数;total=0 表示不可分块的阶段。best-effort,发失败不影响流水线。
#[derive(Clone, serde::Serialize)]
struct AingProgress {
    note_id: String,
    stage: String, // "llm" | "llm_retry"
    done: u32,
    total: u32,
    avg_chunk_ms: u64,
}

/// 只重试 Aing 失败的段落(不重发已成功的块,token 不重花;2026-08-20 设计)。
/// 与整篇 Aing 同一套 lifecycle 契约:RefineProgress(all,running) 注册(编辑被拒、
/// 前端 aiState=running)→ 逐块心跳 → all/done|failed → RefineFinished。
/// 仅 HTTP 执行体(Agent 不分块,没有失败列表);只补文本润色,不补实体/关系(设计取舍)。
#[tauri::command]
async fn retry_failed_refine(app: AppHandle, id: String) -> Result<(), String> {
    store::validate_note_id(&id).map_err(|e| e.to_string())?;
    if let Some((rid, _)) = app.state::<AppState>().retranscribing.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        if rid == id {
            return Err(tr!("该笔记正在重转写中", "This note is being re-transcribed"));
        }
    }
    let lc = app.state::<lifecycle::LifecycleHandle>().inner().clone();
    if lc.is_refining(&id) {
        return Err(tr!("该笔记正在 Aing 中", "This note is being refined"));
    }
    let s = app
        .path()
        .app_data_dir()
        .map(|d| settings::load(&d))
        .map_err(|e| e.to_string())?;
    let Some(settings::ResolvedExecutor::Http { base_url, model, api_key }) = active_refine_executor(&s) else {
        return Err(tr!(
            "当前执行体不支持部分重试(仅 HTTP)",
            "Partial retry needs the HTTP executor"
        ));
    };
    let dir = notes_dir(&app).map_err(|e| e.to_string())?.join(&id);
    // 失败列表先验(不注册就能拒绝的错误尽早拒):空/缺失 = 旧产物或无失败。
    {
        let doc = store::load_refined(&dir)
            .ok_or_else(|| tr!("该笔记尚无修订稿", "This note has no refined doc yet"))?;
        if doc.llm_failed_paragraphs.is_empty() {
            return Err(tr!("没有待重试的失败段落", "No failed paragraphs to retry"));
        }
    }
    let note_id = id.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let beat_gen = refine_beat_gen_next(); // 本 worker 的心跳代次(codex 四轮)
        set_current_refine_run(beat_gen); // 本线程写盘一律以本代次署名(codex 三十三轮)
        let _run_tag = RunTagGuard; // spawn_blocking 线程复用,退出必清署名(codex 三十四轮)
        // 收尾(清心跳+RefineFinished)交 RAII(codex 八轮):polish/回调 panic 展开时
        // 手动清理会被跳过,心跳条目从此常驻,refine_status 永远报一个不存在的 worker。
        let _refine_done =
            RefineDoneOnDrop { lc: lc.clone(), note_id: note_id.clone(), beat_gen };
        let report = |stage: &str, state: &str| {
            // 代次围栏+心跳+入队三合一持锁,同 spawn_refine(codex 三十四轮)
            refine_report_fenced(&lc, &note_id, beat_gen, stage, state);
        };
        report("all", "running"); // 注册:编辑从此被拒,与整篇 Aing 同一守卫
        // 起跑归档上一轮证据并撕收工戳,理由同 spawn_refine(codex 十一/十二轮);
        // 失败必须拦住重跑(codex 十三轮 P2),报终态走人,RAII 哨兵收尾
        if let Err(e) = archive_and_clear_finish_stamp(&dir) {
            eprintln!("部分重试({note_id}):旧收工戳未清,拒绝开跑: {e}");
            // 与整篇路径同款(codex 二十一轮):这次未遂的重试也要进 runs 日志,
            // 否则重启后 status 只剩上一轮的旧记录,这次失败像没发生过。
            let _ = append_refine_run_log(
                &dir,
                &note_id,
                &serde_json::json!({
                    "event": "finished",
                    "outcome": "failed_before_start",
                    "at": chrono::Local::now().to_rfc3339(),
                }),
            );
            report("all", "failed");
            return;
        }
        let run = || -> anyhow::Result<()> {
            let doc = store::load_refined(&dir)
                .ok_or_else(|| anyhow::anyhow!("修订稿加载失败"))?;
            // 越界防御性剔除(理论上只有整写后才会,而整写清列表)。
            let mut failed: Vec<usize> = doc
                .llm_failed_paragraphs
                .iter()
                .copied()
                .filter(|&i| i < doc.paragraphs.len())
                .collect();
            failed.sort_unstable();
            failed.dedup();
            anyhow::ensure!(!failed.is_empty(), "没有待重试的失败段落");
            let old_texts: Vec<String> =
                failed.iter().map(|&i| doc.paragraphs[i].text.clone()).collect();
            let mut subset: Vec<store::RefinedParagraph> =
                failed.iter().map(|&i| doc.paragraphs[i].clone()).collect();
            let cfg = refine::llm::LlmConfig {
                base_url: base_url.clone(),
                model: model.clone(),
                api_key: api_key.clone(),
            };
            let log_ctx = data_root(&app)
                .ok()
                .map(|root| ailog::Ctx { data_root: root, note_id: note_id.clone() });
            let prompt_labels = {
                let note = store::NoteStore::new(notes_dir(&app).map_err(anyhow::Error::msg)?)
                    .load(&note_id)?;
                let vp_now =
                    store::VoiceprintStore::new(data_root(&app).map_err(anyhow::Error::msg)?).load();
                speaker_prompt_labels(&note.speakers, &vp_now)
            };
            let (outcome, _ents, _rels) = refine::llm::polish(
                &cfg,
                &mut subset,
                &prompt_labels,
                log_ctx.as_ref(),
                // 取舍(设计明说):补跑块以空术语表起步;实体/关系不补。
                &|done, total, avg_ms| {
                    report("llm", "running");
                    let _ = app.emit(
                        "aing_progress",
                        AingProgress {
                            note_id: note_id.clone(),
                            stage: "llm_retry".into(),
                            done: done as u32,
                            total: total as u32,
                            avg_chunk_ms: avg_ms,
                        },
                    );
                },
            );
            // 子集内仍失败的位置 → 原下标。
            let still_failed_pos: std::collections::BTreeSet<usize> = match outcome {
                refine::llm::LlmOutcome::Partial(v) => v.into_iter().collect(),
                refine::llm::LlmOutcome::Failed => (0..subset.len()).collect(),
                _ => Default::default(),
            };
            store::update_refined_for_retry(&dir, |doc| {
                let mut remaining: Vec<usize> = Vec::new();
                for (pos, &idx) in failed.iter().enumerate() {
                    if still_failed_pos.contains(&pos) {
                        remaining.push(idx);
                        continue;
                    }
                    match doc.paragraphs.get_mut(idx) {
                        // 逐段 CAS:注册前的在途保存可能已改过文本——改过就尊重用户,
                        // 该段按仍失败保留(不覆盖、不谎称已修)。
                        Some(p) if p.text == old_texts[pos] => p.text = subset[pos].text.clone(),
                        Some(_) => {
                            eprintln!("部分重试({note_id}):段 {idx} 文本已被编辑,跳过写回");
                            remaining.push(idx);
                        }
                        None => eprintln!("部分重试({note_id}):段 {idx} 已不存在,移出列表"),
                    }
                }
                remaining.sort_unstable();
                remaining.dedup();
                doc.stages.llm = if remaining.is_empty() { "done".into() } else { "partial".into() };
                doc.llm_failed_paragraphs = remaining;
                Ok(())
            })?;
            Ok(())
        };
        match run() {
            Ok(()) => {
                stamp_refine_finished(&dir, &note_id, "retry_done", beat_gen);
                report("all", "done");
            }
            Err(e) => {
                eprintln!("部分重试失败({note_id}): {e}");
                stamp_refine_finished(&dir, &note_id, "retry_failed", beat_gen);
                report("all", "failed");
            }
        }
        // 清心跳与 RefineFinished 由函数头部的 RefineDoneOnDrop 在退出时统一处理
    });
    Ok(())
}

#[tauri::command]
fn refine_note(app: AppHandle, id: String) -> Result<(), String> {
    // 重转写中该笔记拒绝 Aing:重转写持 NoteLock,refine 的 run_local 提交时也会
    // 因锁失败——这里提前拒绝只是把错误从「跑完才失败」提到「点下去就说清」。
    if let Some((rid, _)) = app.state::<AppState>().retranscribing.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        if rid == id {
            return Err(tr!("该笔记正在重转写中", "This note is being re-transcribed"));
        }
    }
    app.state::<lifecycle::LifecycleHandle>()
        .request(lifecycle::machine::Msg::RefineRequest { note_id: id })
}

/// 这篇笔记的 Aing 是否正在跑。前端进页时补问一次:running 事件是易失的,进页晚了
/// 就再也收不到,只看事件会把"正在跑"误判成"没在跑"——而 run_local 一开始就把
/// stages.llm 落成 "off",误判会让「这场没做 AI 整理」的提示在整理途中冒出来
/// (Codex P2)。async:is_refining 要等 actor 回执,不能占着主线程等。
#[tauri::command]
async fn note_refining(app: AppHandle, id: String) -> Result<bool, String> {
    store::validate_note_id(&id).map_err(|e| e.to_string())?;
    Ok(app.state::<lifecycle::LifecycleHandle>().is_refining(&id))
}

/// 读取已落盘的 Aing 结果（refined.json）；从未 Aing 过 / Aing 在前置阶段就失败到没能落盘
/// 时返回 None，前端据此回落展示原始 segments。
/// 关联了库人物的段落做只读 join：展示名跟随声纹库现名（会议搭子里改名 → 历史修订稿
/// 跟着变），person_id 归一到 merge 后的 winner。只影响返回值，不落盘。
#[tauri::command]
fn get_refined(app: AppHandle, id: String) -> Result<Option<store::RefinedDoc>, String> {
    store::validate_note_id(&id).map_err(|e| e.to_string())?;
    let root = notes_dir(&app).map_err(|e| e.to_string())?;
    let dir = root.join(&id);
    Ok(store::load_refined_for_display(&dir).map(|mut doc| {
        // 一波说话人 join:身份现查 note.speakers(旧 R 键文档按源段多数票映射回 S)。
        if let (Ok(note), Ok(droot)) = (store::NoteStore::new(root.clone()).load(&id), data_root(&app)) {
            let vp = store::VoiceprintStore::new(droot).load();
            store::join_note_identities(&mut doc, &note.speakers, &note.segments, &vp);
        }
        doc
    }))
}

/// 重转写守卫与启动(tauri command 与 UDS op 共用;spec §提交与安全网)。
/// 守卫顺序(不可颠倒):迁移/下载中拒 → 录制中拒 → Aing 中拒 → 完成态校验 → 转码中拒 →
/// mixed 输入校验 → 槽占用拒 → 占槽后录制中复拒。迁移/下载中拒(Fix 1,codex 第二轮)
/// 排最前,因为它和录制中拒同属"这场重转写现在动不动得了"的资格判定,且比录制检查
/// 更早失败代价更低。中间三条是"这场重转写值不值得跑"的资格判定,转码中拒
/// 是"盘上这个源此刻会不会被转码 worker 改写"的资源校验,mixed 校验是"这个输入源
/// 可不可信",槽占用判是因为它改变了共享状态(占槽)——只有前面全过才允许触碰它。
/// 最后一步(占槽后复拒)是 Fix 1A 补的 Dekker 写后读权威判定:开头那次"录制中拒"只是
/// 快速失败的 UX,真正堵死与 spawn_session 竞态的是这一步。
/// `engine`:本次重转写强制使用的本地识别引擎(如 "firered"),None = 按设置决策。
/// 为什么要覆盖而不是让调用方改设置(Codex P1):①云端模式下重转写会走云端批式,
/// 改 asr_model 根本不生效;②改设置会清识别器缓存并异步预载,与紧接着启动的重转写
/// 各建一份 1.2G 模型,峰值内存翻倍。覆盖只作用于这一次任务,不动用户的默认选择。
pub(crate) fn do_retranscribe(
    app: &AppHandle,
    id: &str,
    input: &str,
    engine: Option<String>,
) -> Result<(), String> {
    store::validate_note_id(id).map_err(|e| e.to_string())?;
    if let Some(e) = engine.as_deref() {
        let known = [
            settings::ASR_SENSE_VOICE,
            settings::ASR_WHISPER,
            settings::ASR_PARAFORMER,
            settings::ASR_QWEN3,
            settings::ASR_FIRERED,
        ];
        if !known.contains(&e) {
            return Err(tr!("未知识别引擎: {e}", "Unknown ASR engine: {e}", e = e));
        }
    }
    if input != "dual" && input != "mixed" {
        return Err(tr!("未知重转写来源: {input}", "Unknown retranscribe input: {input}", input = input));
    }
    let state: tauri::State<AppState> = app.state();
    // Fix 1(codex 第二轮):download_running 兼作迁移/下载互斥位,与 do_start_recording
    // 同款判据同文案。迁移会搬 data_root 并删旧目录,若此刻放行重转写,worker 全程持有
    // 的路径可能被搬走/删掉,提交时 rename 失败甚至"成功"但已写进被丢弃的旧目录。
    // 反向对称检查见 migrate_guard(它读 state.retranscribing 槽拒绝迁移)。这里只是
    // 快速失败的 UX:本检查到下方占槽之间隔着多次盘 IO(load/audio meta),迁移完全可能
    // 在这个毫秒级窗口内 swap download_running 并在槽还空着时穿过 guard——权威判定
    // (写后读)在占槽成功之后再做一次(见下方 Fix 1B)。
    if state.download_running.load(Ordering::SeqCst) {
        return Err(tr!("正在迁移或下载,稍后再试", "Migration or download in progress; try again later"));
    }
    // 全局互斥于录制(不限本篇):重转写与实时 ASR 各起一套 ORT 管线,叠跑抢核;
    // 且省去"另一篇在录、本篇重转写"的时序矩阵——修复动作等一等没有代价。running 是
    // 录制侧最早置位的旗子,session 覆盖 stop 早期窗口(running 已假但会话槽还没清空
    // 的那一小段时间)——两者任一为真都算"录制中"。这里只是快速失败的 UX;权威判定
    // (Dekker 写后读)在下方占槽成功之后再做一次同款检查。
    // session 读必须是独立语句:若写进 recording_blocks_retranscribe 的实参里,
    // MutexGuard 临时值会存活到整个调用表达式结束——函数内部锁 running 时 session
    // 锁仍被持有,形成 session→running 锁序,与 spawn_session 加载线程的
    // running→session_slot(全库锁序纪律,见 do_stop_teardown 注释)成 ABBA 环。
    let session_active = state.session.lock().unwrap().is_some();
    if recording_blocks_retranscribe(&state.running, session_active) {
        return Err(tr!("录制中不能重转写,请先停止录制", "Cannot re-transcribe while recording"));
    }
    if app.state::<lifecycle::LifecycleHandle>().is_refining(id) {
        return Err(tr!("该笔记正在 Aing 中", "This note is being refined"));
    }
    let dir = notes_dir(app).map_err(|e| e.to_string())?.join(id);
    let note = store::NoteStore::new(notes_dir(app).map_err(|e| e.to_string())?)
        .load(id).map_err(|e| e.to_string())?;
    if note.meta.state != "complete" {
        return Err(tr!("笔记未完成,不能重转写", "Only completed notes can be re-transcribed"));
    }
    // 转码互斥:转码 worker 会把该目录的 wav 编码后删除,若此刻正 pending/in-flight,
    // 与重转写离线读盘并发有踩踏窗口。不能用 cancel_and_wait 顶替这条检查:那会把
    // pending 转码项摘掉,wav 就此永不转码——只读查询 + 直接拒绝才是正确的互斥手段。
    if state.transcode.is_busy(&dir) {
        return Err(tr!("该笔记正在转码,稍后再试", "This note is being transcoded; try again later"));
    }
    // 补生成互斥(二期):它会原子替换 mixed.wav 并改写 audio.json 的 mixed 条目,
    // 与重转写(尤其 input=mixed 时读该轨)并发即读到新旧混合状态。快速失败 UX,
    // 权威判定在占槽后(下方 Dekker 段)。
    if mixed_regen_busy(&state.mixed_regen) {
        return Err(tr!("正在补生成成品轨,稍后再试", "Mixed-track regeneration in progress; try again later"));
    }
    if input == "mixed" {
        let meta = store::audio::load_audio_meta(&dir);
        if let Some(reason) = retranscribe::input::mixed_untrusted(&meta) {
            return Err(reason);
        }
    }
    {
        let mut slot = state.retranscribing.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((running, _)) = slot.as_ref() {
            return Err(tr!(
                "已有重转写任务在进行({running}),请等它完成",
                "A re-transcription task is already running ({running})", running = running
            ));
        }
        *slot = Some((id.to_string(), "decode".into()));
    }
    // Fix 1A(Dekker 写后读，R 侧权威判定):槽已占之后再读一次 running/session
    // ——不与槽锁嵌套持有(上面的槽锁已在此前的花括号作用域结束时释放),避免和
    // S 侧(spawn_session)的锁序相反而成环。若此刻仍判定"录制中",说明 do_start_recording
    // /do_resume_note_recording 的早期检查穿过了本次占槽与它们置位 running 之间的窗口
    // ——必须清槽退让。与 S 侧互为镜像:两侧各自"先写自己、再读对方"，顺序矛盾使得
    // 二者不可能同时判定通过（同时穿关）。
    // session 读同样独立成句(锁序理由同上方快速失败检查处的注释)。
    let session_active = state.session.lock().unwrap().is_some();
    if recording_blocks_retranscribe(&state.running, session_active) {
        *state.retranscribing.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return Err(tr!("录制中不能重转写,请先停止录制", "Cannot re-transcribe while recording"));
    }
    // Fix 1B(迁移侧同款写后读):占槽之后复查 download_running。迁移是
    // write(download_running)→read(槽),本侧是 write(槽)→read(download_running),
    // 两侧不可能同时穿关(最坏双拒,无害)。缺这一读时,迁移在上方快速失败检查
    // 与占槽之间的盘 IO 窗口里穿过 guard 即双跑——正是本检查封死的交错。
    if state.download_running.load(Ordering::SeqCst) {
        *state.retranscribing.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return Err(tr!("正在迁移或下载,稍后再试", "Migration or download in progress; try again later"));
    }
    // 补生成侧同款写后读(对侧在 do_regenerate_mixed 占槽后读本槽;两侧先写自己
    // 再读对方,最坏双拒无害):
    if mixed_regen_busy(&state.mixed_regen) {
        *state.retranscribing.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return Err(tr!("正在补生成成品轨,稍后再试", "Mixed-track regeneration in progress; try again later"));
    }
    // Fix 2(codex 第三轮,R 侧互查闭环——两处必须同步改,另一侧见 spawn_refine 里
    // 占槽复查那一段的同款注释):占槽之后复查 `is_refining(id)`。开头那次
    // "Aing 中拒"(本函数上方 `is_refining(id)` 检查)只是快速失败的 UX——它与本次
    // 占槽之间隔着多次盘 IO(load note/audio meta/transcode.is_busy),
    // `refine_note` 命令壳在 kernel 插入 Aing 集之前查重转写槽的那次检查完全可能
    // 落在这个窗口里穿过去。权威判定同 Fix 1A/1B 一样是写后读:占槽(写)已经完成,
    // 此刻再读一次 Aing 集才是真正堵死并发的那一步。
    //
    // 互斥证明与 spawn_refine 侧完全对称(完整推演见该函数 Fix 2 注释;那边同时
    // 记录了一处对设计初稿前提的订正——LifecycleHandle::report 是"只投递不等待"
    // 的异步调用,不是同步调用,证明改靠 actor 信箱 FIFO 单消费者的入队顺序,
    // 不依赖"report 一返回 Aing 集就已插入完成"这个不成立的前提):
    //   R: 写(占槽,上面 `*slot = Some(...)`)→ 读(is_refining(id),就是这里,
    //      即 send(QueryRefine)入队并阻塞等回执)
    //   A: send(RefineProgress "all/running")入队 → 读(重转写槽,spawn_refine 里
    //      thread::spawn 之前那次复查)
    // 若 R 的写先发生(早于 A 的 send 入队),A 那边必读到 R 已占的槽 → A 让步、
    // 不 spawn 工作线程。若 A 的 send 先入队(早于 R 写槽这一实时点之前就已发生,
    // 从而必早于 R 的 send 入队),按 FIFO actor 必先处理 A 的插入、再处理 R 的
    // 查询,R 这里必读到 true → R 让步、清槽、拒绝。两侧不可能同时读到"对方还
    // 没写"——双穿不可能发生。
    if app.state::<lifecycle::LifecycleHandle>().is_refining(id) {
        *state.retranscribing.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return Err(tr!("该笔记正在 Aing 中", "This note is being refined"));
    }
    spawn_retranscribe(app.clone(), id.to_string(), input == "mixed", engine);
    Ok(())
}

/// 重转写后台线程:catch_unwind 兜 panic,独立识别器/嵌入器实例(不碰常驻缓存槽——
/// 那两槽是录制会话的常驻资源,重转写是离线一次性任务,混用会让二者互相饿死对方)。
/// 事件不经 lifecycle actor 直发(见 ipc::RetranscribeEvent 注释):重转写与录制会话
/// 全局互斥,不存在与管线事件的排序耦合,直发省一层转发不丢语义。
/// 一次离线重转写的公共装配:目录 → NoteLock → 识别器 → 嵌入器 → 种子 → 输入 → run。
/// spawn_retranscribe(手动/UI)与 Aing 前置云端二遍(spawn_refine,local_cloud)共用,
/// 不各配一份漂移。识别器按设置决策:local_cloud 且凭证齐 → 云端批式(手动重转写
/// 与自动二遍同引擎,结果口径一致);否则本地引擎(不碰常驻 recognizer_cache 槽——
/// 那是录制会话的常驻资源,离线任务混用会互相饿死对方)。
#[allow(clippy::too_many_arguments)]
fn run_retranscribe_once(
    app: &tauri::AppHandle,
    note_id: &str,
    mixed: bool,
    language_filter: bool,
    strict: bool,
    engine: Option<String>,
    progress: &mut dyn FnMut(&str),
) -> Result<retranscribe::Summary, String> {
    let dir = notes_dir(app).map_err(|e| e.to_string())?.join(note_id);
    // NoteLock 在本函数内 acquire 且持有全程(run() 要求调用方贯穿持锁)。
    // 文案不写"或转码中":转码走队列不持 NoteLock,与此锁失败的实际成因无关
    // (会撞这把锁的只有录制/编辑写手柄);写成"转码中"是与事实不符的误导。
    let lock = store::notelock::NoteLock::acquire(&dir)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| tr!(
            "笔记正被占用(录制/编辑中),稍后再试",
            "The note is busy (recording or being edited); try again later"
        ))?;
    let s = app.path().app_data_dir().map(|d| settings::load(&d)).unwrap_or_default();
    // 显式指定引擎时一律走本地那一支:调用方要的就是"用这个引擎再解一遍",
    // 云端二遍会把这个意图吃掉(Codex P1)。
    // 云端识别器没有 engine_id(trait 默认值是 "unknown"),身份得从建它时用的那份
    // 设置快照里取,写成与实时链路同款的 "cloud:厂商"(Codex P2)。写错了这篇就不再
    // 被认成"云端转的",反而会被建议换 FireRed。
    let mut cloud_id: Option<String> = None;
    let mut recognizer: Box<dyn asr::Recognizer> = if let Some(e) = engine.as_deref() {
        new_recognizer(e, current_asr_provider(app), qwen3_hotwords(app))
            .map_err(|err| tr!("识别器加载失败(本地模型未下载?): {e}", "Failed to load recognizer: {e}", e = err))?
    } else if settings::cloud_second_pass_wanted(&s) {
        let cloud = make_cloud_asr(&s).map_err(|e| e.to_string())?;
        cloud_id = Some(format!("cloud:{}", s.cloud_asr_provider));
        Box::new(asr::cloud::BatchRecognizer::new(cloud))
    } else {
        new_recognizer(&current_asr(app), current_asr_provider(app), qwen3_hotwords(app))
            .map_err(|e| tr!("识别器加载失败(本地模型未下载?): {e}", "Failed to load recognizer: {e}", e = e))?
    };
    // 标签、权重、种子门禁同源(codex review 实现轮五 P1)。
    let speaker_tag = current_speaker_model(app);
    let mut embedder: Option<Box<dyn diar::SpeakerEmbedder>> =
        match diar::SherpaEmbedder::new(&speaker_model_path_for(&speaker_tag)) {
            Ok(e) => Some(Box::new(e)),
            Err(e) => {
                eprintln!("重转写:声纹模型不可用,归属降级为纯继承: {e}");
                None
            }
        };
    let seeds = load_voiceprint_seeds_for(app, &speaker_tag);
    let vad_path = models::root().join("silero_vad.onnx");
    let factory: retranscribe::input::SegmenterFactory = Box::new(move || new_silero(&vad_path));
    let mut input: Box<dyn retranscribe::input::TranscribeInput> = if mixed {
        Box::new(retranscribe::input::MixedInput::new(dir.clone(), factory))
    } else {
        Box::new(retranscribe::input::DualTrackInput::new(dir.clone(), factory))
    };
    // 身份向识别器实例本人要(与实时链路同款,见 spawn_refine 处注释):这一遍到底
    // 是谁转的,决定了「疑似识别失败,换引擎重转写」还要不要再提示这篇。
    let engine_id = cloud_id.unwrap_or_else(|| recognizer.engine_id().to_string());
    let out = retranscribe::run(&dir, &lock, input.as_mut(), recognizer.as_mut(),
        &mut embedder, seeds, mixed, language_filter, strict, progress,
        &current_speaker_match(app))
        .map_err(|e| e.to_string())?;
    // 成功提交之后再改 meta:失败时不能留下"已经是 FireRed 转的"这种假账
    // (Codex P2)。落盘失败只记日志——正文已经换过了,不该因为一行元数据回滚。
    if let Err(e) = store::set_note_asr_engine(&dir, &engine_id) {
        eprintln!("重转写:asr_engine 落盘失败(不影响正文): {e}");
    }
    Ok(out)
}

fn spawn_retranscribe(app: tauri::AppHandle, note_id: String, mixed: bool, engine: Option<String>) {
    let slot = app.state::<AppState>().retranscribing.clone();
    let last = app.state::<AppState>().retranscribe_last.clone();
    // language_filter:与实时链路(0) 一次性读设置同款途径同源同快照——不读到并发写入的
    // 半新半旧状态。关闭时重转写不得替用户悄悄丢外语段(Fix 1,codex 第三轮):旧代码
    // 无条件调 is_foreign_final,会把用户已显式关闭过滤后保留下来的多语内容冲掉。
    let language_filter = app.path().app_data_dir().map(|d| settings::load(&d)).unwrap_or_default().language_filter;
    std::thread::spawn(move || {
        let emit = |stage: &str, state: &str, message: Option<String>, summary: Option<retranscribe::Summary>| {
            let ev = ipc::RetranscribeEvent {
                note_id: note_id.clone(), stage: stage.into(), state: state.into(), message, summary,
            };
            // 只有终态(ok/error)落 last;中途的 "running" 不覆盖上一次任务的终态——
            // 轮询方靠 last 区分"完成"与"放弃/失败",不该被进行中的噪声冲掉。
            if state != "running" {
                // poison 只可能因锁内 panic 产生,槽是纯数据,中毒后继续写最后一次值好过永久卡死。
                *last.lock().unwrap_or_else(|e| e.into_inner()) = Some(ev.clone());
            }
            let _ = app.emit("retranscribe", ev);
        };
        emit("all", "running", None, None);
        let body = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<retranscribe::Summary, String> {
            let slot2 = slot.clone();
            let note_id2 = note_id.clone();
            let app2 = app.clone();
            let mut progress = move |stage: &str| {
                // poison 只可能因锁内 panic 产生,槽是纯数据(note_id+stage 字符串),
                // 中毒后继续用最后一次写入的值远好过让整条重转写进度上报永久卡死。
                if let Some(s) = slot2.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    s.1 = stage.to_string();
                }
                let _ = app2.emit("retranscribe", ipc::RetranscribeEvent {
                    note_id: note_id2.clone(), stage: stage.into(), state: "running".into(),
                    message: None, summary: None,
                });
            };
            // 手动路径宽容(strict=false):失败段落占位,用户看 summary 自行决定。
            run_retranscribe_once(&app, &note_id, mixed, language_filter, false, engine.clone(), &mut progress)
        }));
        match body {
            Ok(Ok(summary)) => {
                eprintln!("重转写完成({note_id}): {summary:?}");
                emit("all", "ok", None, Some(summary));
            }
            Ok(Err(e)) => {
                eprintln!("重转写失败({note_id}): {e}");
                emit("all", "error", Some(e), None);
            }
            Err(_) => {
                eprintln!("重转写 panic({note_id})");
                emit("all", "error", Some(tr!("内部错误(见日志)", "Internal error (see logs)")), None);
            }
        }
        // Fix 3:清槽移到终态 emit 之后（而非 match 之前）。轮询契约是"看到
        // running=false（即槽空）时,last 必须已经是本次任务的终态"——三个 match 分支
        // 各自只做锁写 last + emit,没有 panic 面（catch_unwind 已经兜过内部 body 的
        // panic，这里三条分支本身不会再 panic），所以槽必达清空,不会因为提前 return
        // 或异常路径漏清。poison 只可能因锁内 panic 产生,槽是纯数据,中毒后继续清槽
        // 好过永久卡死。
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    });
}

#[tauri::command]
fn retranscribe_note(
    app: AppHandle,
    id: String,
    input: String,
    engine: Option<String>,
) -> Result<(), String> {
    do_retranscribe(&app, &id, &input, engine)
}

#[derive(serde::Serialize)]
struct RetranscribeStatus {
    note_id: String,
    stage: String,
}

#[tauri::command]
fn retranscribe_status(state: State<AppState>) -> Option<RetranscribeStatus> {
    // poison 只可能因锁内 panic 产生,槽是纯数据,中毒后继续读最后写入值好过永久卡死。
    state.retranscribing.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        .map(|(note_id, stage)| RetranscribeStatus { note_id: note_id.clone(), stage: stage.clone() })
}

/// 成品轨入口可用性:None = 可用;Some(原因) = 置灰并提示。
#[tauri::command]
fn mixed_input_status(app: AppHandle, id: String) -> Result<Option<String>, String> {
    store::validate_note_id(&id).map_err(|e| e.to_string())?;
    let dir = notes_dir(&app).map_err(|e| e.to_string())?.join(&id);
    Ok(retranscribe::input::mixed_untrusted(&store::audio::load_audio_meta(&dir)))
}

/// 离线补生成成品轨的守卫与启动(二期,spec §离线补生成)。守卫链照抄
/// do_retranscribe 的顺序纪律(迁移/下载 → 录制快拒 → 重转写槽拒 → 双轨可用 →
/// 转码中拒 → 占槽 → Dekker 写后读复查),差异两点并各有理由:
/// - 不查 Aing:补生成只读源轨、只写 mixed.wav 与 audio.json(META_LOCK 内),
///   与 Aing 的精修文本写面零交集;
/// - 不要求 note complete:补生成不碰 segments,残局笔记(崩溃遗留)双轨俱在时
///   照样能修——这正是"历史录音直接成为回归集"的目标场景。
/// 录制侧不反查本槽:全局录制被本侧拒之门外后,录制期间新发起的补生成也进不来;
/// 同一笔记的写冲突由 worker 全程持有的 NoteLock 兜底,别的笔记录制与本任务无盘面交集。
pub(crate) fn do_regenerate_mixed(app: &AppHandle, id: &str) -> Result<(), String> {
    store::validate_note_id(id).map_err(|e| e.to_string())?;
    let state: tauri::State<AppState> = app.state();
    if state.download_running.load(Ordering::SeqCst) {
        return Err(tr!("正在迁移或下载,稍后再试", "Migration or download in progress; try again later"));
    }
    // session 读独立成句:锁序纪律同 do_retranscribe(ABBA 环,见那边注释)。
    let session_active = state.session.lock().unwrap().is_some();
    if recording_blocks_retranscribe(&state.running, session_active) {
        return Err(tr!("录制中不能补生成成品轨,请先停止录制", "Cannot regenerate the mixed track while recording"));
    }
    // Aing 互斥(codex 第三轮 P1):worker 全程持 NoteLock,Aing 收尾提交要拿同一把
    // 锁——放行会让昂贵 LLM 阶段跑完才失败。快拒 + 占槽后复查(下方),反向由
    // spawn_refine 的占槽后复查闭环(FIFO actor 论证同重转写侧 Fix 2)。
    if app.state::<lifecycle::LifecycleHandle>().is_refining(id) {
        return Err(tr!("该笔记正在 Aing 中,稍后再试", "This note is being refined by AI; try again later"));
    }
    if retranscribe_blocks_recording(&state.retranscribing) {
        return Err(tr!("重转写进行中,稍后再试", "Re-transcription in progress; try again later"));
    }
    let dir = notes_dir(app).map_err(|e| e.to_string())?.join(id);
    let meta = store::audio::load_audio_meta(&dir);
    for src in ["mic", "system"] {
        let present = dir.join(format!("{src}.wav")).is_file() || dir.join(format!("{src}.m4a")).is_file();
        if !present {
            let _ = meta; // meta 仅用于未来扩展校验;文件在场性是硬判据
            return Err(tr!(
                "需要 mic 与 system 双轨才能补生成成品轨(缺 {src})",
                "Regenerating requires both mic and system tracks (missing {src})", src = src
            ));
        }
    }
    // 转码互斥理由同 do_retranscribe:worker 会把 wav 编码后删除,离线读盘有踩踏窗口。
    if state.transcode.is_busy(&dir) {
        return Err(tr!("该笔记正在转码,稍后再试", "This note is being transcoded; try again later"));
    }
    {
        let mut slot = state.mixed_regen.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(running) = slot.as_ref() {
            return Err(tr!(
                "已有补生成任务在进行({running}),请等它完成",
                "A mixed-track regeneration is already running ({running})", running = running
            ));
        }
        *slot = Some(id.to_string());
    }
    // Dekker 写后读(占槽之后复查对侧;两侧都是"先写自己、再读对方",不可能同时穿关):
    let session_active = state.session.lock().unwrap().is_some();
    if recording_blocks_retranscribe(&state.running, session_active)
        || state.download_running.load(Ordering::SeqCst)
        || retranscribe_blocks_recording(&state.retranscribing)
        || app.state::<lifecycle::LifecycleHandle>().is_refining(id)
    {
        *state.mixed_regen.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return Err(tr!("状态刚发生变化(录制/迁移/重转写),稍后再试", "State just changed (recording/migration/re-transcription); try again later"));
    }
    spawn_mixed_regen(app.clone(), id.to_string());
    Ok(())
}

/// 补生成后台线程:catch_unwind 兜 panic;NoteLock 全程持有;终态 emit 之后清槽
/// (Fix 3 同款顺序:槽空时事件必已送达)。
fn spawn_mixed_regen(app: tauri::AppHandle, note_id: String) {
    let slot = app.state::<AppState>().mixed_regen.clone();
    let transcode = app.state::<AppState>().transcode.clone();
    std::thread::spawn(move || {
        let emit = |stage: &str, state: &str, message: Option<String>| {
            let _ = app.emit("mixed_regen", ipc::MixedRegenEvent {
                note_id: note_id.clone(), stage: stage.into(), state: state.into(), message,
            });
        };
        emit("mix", "running", None);
        let body = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), String> {
            let dir = notes_dir(&app).map_err(|e| e.to_string())?.join(&note_id);
            let _lock = store::notelock::NoteLock::acquire(&dir)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| tr!(
                    "笔记正被占用(录制/编辑中),稍后再试",
                    "The note is busy (recording or being edited); try again later"
                ))?;
            store::mix_regen::regen_note_dir(&dir).map_err(|e| e.to_string())?;
            // 转码守卫(codex 第二轮 P1):transcode_note_dir 是目录粒度,会把目录里
            // 所有 WAV——包括中断笔记仍在盘的源轨——清洗、有损转码并删除,违反补生成
            // 「源轨只读」契约,还会转码启动扫描刻意排除的未完成笔记。只有源轨已是
            // 转码形态(wav 不在盘)时才入队:此时目录里唯一的 WAV 就是新 mixed。
            // 否则成品轨保持 WAV(多占些磁盘;时长读数走 mix.track_ms,波形已算)。
            let sources_transcoded =
                !dir.join("mic.wav").is_file() && !dir.join("system.wav").is_file();
            if sources_transcoded {
                transcode.enqueue(dir);
            } else {
                eprintln!("补生成:源轨仍是 WAV,跳过目录转码(避免整目录清洗/删源),mixed 保持 WAV");
            }
            Ok(())
        }));
        match body {
            Ok(Ok(())) => emit("finish", "ok", None),
            Ok(Err(e)) => {
                eprintln!("补生成失败({note_id}): {e}");
                emit("finish", "error", Some(e));
            }
            Err(_) => {
                eprintln!("补生成 panic({note_id})");
                emit("finish", "error", Some(tr!("内部错误(见日志)", "Internal error (see logs)")));
            }
        }
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    });
}

#[tauri::command]
fn regenerate_mixed(app: AppHandle, id: String) -> Result<(), String> {
    do_regenerate_mixed(&app, &id)
}

#[tauri::command]
fn mixed_regen_status(state: State<AppState>) -> Option<String> {
    state.mixed_regen.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// 回放消费侧一站式读数(二期,详情页 A/B 切换用)。
#[derive(Debug, serde::Serialize)]
pub struct MixedPlaybackInfo {
    /// None = 无成品轨(前端给「生成成品轨」动作)。
    track: Option<store::audio::TrackInfo>,
    /// Some(原因) = 有轨但不可信(置灰 + tooltip;消费前校验是 spec §错误处理的硬要求)。
    untrusted: Option<String>,
    /// 各源段落 seek 到 mixed 的修正量(ms)。空表 = 无需修正(regen 轨按 offset
    /// 定位;一期旧轨无标记,按 0 容忍偏移)。
    seek_offset_ms: std::collections::BTreeMap<String, u64>,
    /// mic 轨带离线清洗记录:A 侧比 B 侧多一级回声抑制,听感不可直比
    /// (spec §对照条件 选项 1 的判据落进 UI)。
    ab_caveat: bool,
}

/// 读数拼装抽成纯函数:命令壳只做 dir 解析与懒回填,这里的口径规则全部可单测。
fn assemble_mixed_playback(
    meta: &store::audio::AudioMeta,
    track: Option<store::audio::TrackInfo>,
) -> MixedPlaybackInfo {
    let untrusted = if track.is_some() {
        retranscribe::input::mixed_untrusted(meta)
    } else {
        None
    };
    let seek_offset_ms = meta
        .tracks
        .get(pipeline::recording_sink::MIXED_TRACK)
        .and_then(|t| t.mix.as_ref())
        .map(|m| m.seek_offset_ms.clone())
        .unwrap_or_default();
    let ab_caveat = meta.tracks.get("mic").map(|t| t.clean.is_some()).unwrap_or(false);
    MixedPlaybackInfo { track, untrusted, seek_offset_ms, ab_caveat }
}

#[tauri::command]
fn mixed_playback_info(app: AppHandle, id: String) -> Result<MixedPlaybackInfo, String> {
    store::validate_note_id(&id).map_err(|e| e.to_string())?;
    let note_dir = notes_dir(&app).map_err(|e| e.to_string())?.join(&id);
    if !note_dir.is_dir() {
        return Err(tr!("笔记不存在: {id}", "Note not found: {id}"));
    }
    let meta = store::audio::load_audio_meta(&note_dir);
    let track = store::audio::mixed_track(&note_dir);
    // 波形懒回填:note_audio_info 的回填循环遍历 list_tracks(),mixed 被过滤在外
    // (spec §存储 点名二期自行触发)。样板与那边一致:in-flight 去重、后台线程、
    // 完成发 transcode_done 复用详情页既有刷新链。仅未转码 WAV 需要——m4a 的波形
    // 在转码期已预计算。
    if let Some(t) = &track {
        if t.waveform.is_none() && t.path.ends_with(".wav") {
            static INFLIGHT: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);
            let key = format!("{id}/mixed");
            let claimed = {
                let mut g = INFLIGHT.lock().unwrap();
                g.get_or_insert_with(Default::default).insert(key.clone())
            };
            if claimed {
                let (app, note_dir, note_id) = (app.clone(), note_dir.clone(), id.clone());
                std::thread::spawn(move || {
                    match store::audio::backfill_wav_waveform(
                        &note_dir,
                        pipeline::recording_sink::MIXED_TRACK,
                    ) {
                        Ok(()) => {
                            let _ = app.emit("transcode_done", ipc::TranscodeEvent { note_id });
                        }
                        Err(e) => eprintln!("mixed 波形回填失败({note_id}),维持段落包络: {e}"),
                    }
                    INFLIGHT.lock().unwrap().as_mut().map(|s| s.remove(&key));
                });
            }
        }
    }
    Ok(assemble_mixed_playback(&meta, track))
}

fn relation_backfill_settings(app: &AppHandle) -> Result<settings::Settings, String> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| tr!("app_data_dir 不可用: {error}", "app_data_dir unavailable: {error}"))?;
    Ok(settings::load(&app_data))
}

fn ensure_requested_backfill_provider(
    request: &ipc::BackfillRequest,
    settings: &settings::Settings,
) -> Result<(), String> {
    // 对账词表沿用旧的 "openai"/"agent"(preview.provider 同源):按当前关系执行体
    // 的解析结果推导,请求与现状不一致即拒(用户在 preview 后改了执行体的 TOCTOU 守卫)。
    let current = match settings::resolve_executor(settings, settings::AiFeature::Relations) {
        Some(settings::ResolvedExecutor::Http { .. }) => "openai",
        Some(settings::ResolvedExecutor::Agent { .. }) => "agent",
        None => "",
    };
    if request.provider != current {
        return Err(tr!(
            "补建 provider 与当前配置不一致:请求 {requested},当前 {current}",
            "Backfill provider does not match the current configuration: requested {requested}, current {current}",
            requested = request.provider,
            current = current
        ));
    }
    Ok(())
}

fn relation_executor(
    settings: &settings::Settings,
) -> anyhow::Result<Box<dyn refine::backfill::RelationExecutor>> {
    match settings::resolve_executor(settings, settings::AiFeature::Relations) {
        Some(settings::ResolvedExecutor::Http { base_url, model, api_key }) => {
            Ok(Box::new(refine::llm::HttpRelationExecutor::new(refine::llm::LlmConfig {
                base_url,
                model,
                api_key,
            })?))
        }
        Some(settings::ResolvedExecutor::Agent { kind, bin, model }) => {
            let k = refine::agent::AgentKind::from_key(&kind).ok_or_else(|| {
                anyhow::anyhow!(tr!("未知 Agent: {agent}", "Unknown agent: {agent}", agent = kind))
            })?;
            Ok(Box::new(refine::agent::AgentRelationExecutor::new(k, &bin, &model)?))
        }
        None => anyhow::bail!(tr!(
            "关系分析执行体未配置(在 AI 页选择执行体)",
            "Relation analysis executor is not configured (choose one on the AI page)"
        )),
    }
}

/// identify(P2a)执行体分派:与精修同一外发授权语义——用户关闭精修即整体
/// 不跑;agent provider 走零工具面的 AgentIdentifyExecutor(Cursor 拒绝),
/// 其余沿 refine_llm_ready 的宽 provider 语义走 HTTP。返回 Err 时调用方跳过
/// 并留一行原因日志(静默吞错会让"identify 没发生"无法诊断)。
fn identify_executor(
    settings: &settings::Settings,
) -> anyhow::Result<Box<dyn refine::identify::IdentifyExecutor>> {
    anyhow::ensure!(settings.refine_enabled, "identify 需要已启用精修");
    match settings::resolve_executor(settings, settings::AiFeature::Refine) {
        Some(settings::ResolvedExecutor::Agent { kind, bin, model }) => {
            let k = refine::agent::AgentKind::from_key(&kind).ok_or_else(|| {
                anyhow::anyhow!(tr!("未知 Agent: {agent}", "Unknown agent: {agent}", agent = kind))
            })?;
            Ok(Box::new(refine::agent::AgentIdentifyExecutor::new(k, &bin, &model)?))
        }
        Some(settings::ResolvedExecutor::Http { .. }) if !refine_llm_ready(settings) => {
            anyhow::bail!("identify 需要配置齐全的 HTTP 精修")
        }
        Some(settings::ResolvedExecutor::Http { base_url, model, api_key }) => {
            Ok(Box::new(refine::llm::HttpIdentifyExecutor::new(refine::llm::LlmConfig {
                base_url,
                model,
                api_key,
            })?))
        }
        None => anyhow::bail!("identify 需要已配置的 AI 执行体"),
    }
}

fn spawn_relation_backfill_worker<Spawn, Worker, EmitFailure>(
    gate: refine::backfill::BackfillGate,
    initial: ipc::BackfillProgress,
    spawn: Spawn,
    worker: Worker,
    emit_failure: EmitFailure,
) -> Result<(), String>
where
    Spawn: FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
    Worker: FnOnce(ipc::BackfillProgress) + Send + 'static,
    EmitFailure: FnOnce(ipc::BackfillProgress),
{
    let failure_template = initial.clone();
    let job: Box<dyn FnOnce() + Send> = Box::new(move || {
        let _gate = gate;
        worker(initial);
    });
    if let Err(error) = spawn(job) {
        let mut terminal = failure_template;
        terminal.state = "failed".into();
        terminal.failed.push(ipc::BackfillFailure {
            note_id: String::new(),
            error: tr!(
                "无法启动关系补建线程:{error}",
                "Failed to start the relation backfill thread: {error}"
            ),
        });
        terminal.index_error = None;
        emit_failure(terminal);
        return Err(tr!(
            "无法启动关系补建线程:{error}",
            "Failed to start the relation backfill thread: {error}"
        ));
    }
    Ok(())
}

#[tauri::command]
fn preview_relation_backfill(
    app: AppHandle,
    note_ids: Option<Vec<String>>,
) -> Result<ipc::BackfillPreview, String> {
    let settings = relation_backfill_settings(&app)?;
    let root = data_root(&app).map_err(|error| error.to_string())?;
    refine::backfill::preview(&root, &settings, note_ids.as_deref())
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
fn start_relation_backfill(
    app: AppHandle,
    state: State<AppState>,
    request: ipc::BackfillRequest,
) -> Result<(), String> {
    let gate = refine::backfill::BackfillGate::acquire(
        Arc::clone(&state.relation_backfill_running),
        Arc::clone(&state.relation_backfill_run_id),
        &request.run_id,
    )
    .map_err(|error| error.to_string())?;
    let settings = relation_backfill_settings(&app)?;
    ensure_requested_backfill_provider(&request, &settings)?;
    let root = data_root(&app).map_err(|error| error.to_string())?;
    let approved = refine::backfill::preflight(&root, &settings, &request)
        .map_err(|error| format!("{error:#}"))?;
    let preview = &approved.preview;
    let executor = relation_executor(&settings).map_err(|error| format!("{error:#}"))?;
    if executor.provider() != preview.provider || executor.model() != preview.model {
        return Err(tr!(
            "preview 与执行 provider/model 不一致",
            "Preview and execution provider/model do not match"
        ));
    }

    state.relation_backfill_cancel.store(false, Ordering::SeqCst);
    let cancel = Arc::clone(&state.relation_backfill_cancel);
    let scheduler = state.graph_scheduler.clone();
    let note_ids = preview.note_ids.clone();
    let approved_source_hashes = approved.source_hashes;
    let run_id = request.run_id.clone();
    let events = app.clone();
    let initial = ipc::BackfillProgress {
        run_id: run_id.clone(),
        state: "running".into(),
        completed: 0,
        total: note_ids.len(),
        current_note_id: None,
        failed: vec![],
        rebuild_generation: None,
        index_error: None,
    };
    let last_progress = Arc::new(Mutex::new(initial.clone()));
    let spawn_failure_events = app.clone();
    spawn_relation_backfill_worker(
        gate,
        initial,
        |job| {
            std::thread::Builder::new()
                .name("relation-backfill".into())
                .spawn(move || job())
                .map(|_| ())
        },
        move |initial| {
            let _ = events.emit("relation_backfill_progress", initial);
            let panic_events = events.clone();
            let progress_state = Arc::clone(&last_progress);
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rebuild_events = events.clone();
                let rebuild_root = root.clone();
                refine::backfill::run_batch(
                    &run_id,
                    &root.join("notes"),
                    &note_ids,
                    &approved_source_hashes,
                    executor.as_ref(),
                    &cancel,
                    |progress| {
                        *progress_state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = progress.clone();
                        let _ = events.emit("relation_backfill_progress", progress);
                    },
                    || {
                        let graph_events = rebuild_events.clone();
                        scheduler
                            .request(rebuild_root.clone(), move |status| {
                                let _ = graph_events.emit("graph_index_status", status);
                            })
                    },
                )
            }));
            if run.is_err() {
                let terminal = refine::backfill::panic_progress(
                    last_progress
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
                    cancel.load(Ordering::SeqCst),
                );
                let _ = panic_events.emit("relation_backfill_progress", terminal);
            }
        },
        move |terminal| {
            let _ = spawn_failure_events.emit("relation_backfill_progress", terminal);
        },
    )?;
    Ok(())
}

#[tauri::command]
fn cancel_relation_backfill(state: State<AppState>, run_id: String) -> Result<(), String> {
    refine::backfill::request_cancel(
        &state.relation_backfill_run_id,
        &state.relation_backfill_cancel,
        &run_id,
    )
    .map_err(|error| error.to_string())
}

fn retry_relation_backfill_index_with(
    scheduler: &graph::index::RebuildScheduler,
    root: PathBuf,
    emit: impl Fn(graph::index::IndexStatus) + Send + Sync + 'static,
) -> Result<u64, String> {
    scheduler
        .retry_dirty(root, emit)
        .map_err(|error| {
            tr!(
                "图谱索引重试排队失败:{error:#}",
                "Failed to queue the graph index retry: {error:#}"
            )
        })
}

#[tauri::command]
fn retry_relation_backfill_index(
    app: AppHandle,
    state: State<AppState>,
) -> Result<u64, String> {
    let root = data_root(&app).map_err(|error| error.to_string())?;
    let graph_events = app.clone();
    retry_relation_backfill_index_with(&state.graph_scheduler, root, move |status| {
        let _ = graph_events.emit("graph_index_status", status);
    })
}

/// 原始稿说话人关联声纹库人物（会议搭子选人）：speakers.json 写 person_id 并清空
/// 本地改名，展示走既有只读 join 显示库中现名。录制中拒绝（speakers.json 由 writer
/// 独占）；person_id 经 resolve 归一，悬空报错。
#[tauri::command]
fn assign_note_speaker_person(
    app: AppHandle,
    state: State<AppState>,
    note_id: String,
    speaker_id: String,
    person_id: String,
    audited_seq: Option<u64>,
) -> Result<(), String> {
    reject_if_active(&state, &note_id)?;
    do_assign_note_speaker_person_with(&app, &note_id, &speaker_id, &person_id, audited_seq)
}

/// 关联的可复用本体:命令壳与 identify 建议确认(apply_identify_suggestion)共用。
/// 调用方自备录制中守卫(reject_if_active);EditNote 经 lifecycle actor 串行,
/// spawn_feedback 承担纠错回灌。一波说话人设计(2026-08-21)后这是唯一的关联写入口。
fn do_assign_note_speaker_person(
    app: &AppHandle,
    note_id: &str,
    speaker_id: &str,
    person_id: &str,
) -> Result<(), String> {
    do_assign_note_speaker_person_with(app, note_id, speaker_id, person_id, None)
}

/// 「确认才入库」版本(2026-08-22-one-click-split-design.md):拆分产物说话人
/// (split_born)关联时**不做整组批量回灌**——混杂簇是批量喂库的污染源;库写入
/// 只走 audited_seq(用户刚试听过的那一段):存为人物样本 + 单段回灌质心。
/// 没试听就关联 → 本篇生效,库零写入。普通说话人行为不变(整组 spawn_feedback)。
fn do_assign_note_speaker_person_with(
    app: &AppHandle,
    note_id: &str,
    speaker_id: &str,
    person_id: &str,
    audited_seq: Option<u64>,
) -> Result<(), String> {
    let vp = open_voiceprint_store(app)?.load();
    let Some(resolved) = store::VoiceprintStore::resolve(&vp, person_id).map(str::to_string) else {
        return Err(tr!(
            "声纹库中没有该人物: {person_id}",
            "No such person in the voiceprint library: {person_id}",
            person_id = person_id
        ));
    };
    // 纠错回灌(spec P1-2)的输入必须在写入前同步取好:指认时刻的段快照与
    // 先前关联,后台任务不再回读笔记,避免基于"稍后状态"的混合版本回灌。
    let dir = notes_dir(app).map_err(|e| e.to_string())?;
    let note = store::NoteStore::new(dir).load(note_id).map_err(|e| e.to_string())?;
    let prior = note
        .speakers
        .get(speaker_id)
        .and_then(|m| m.person_id.as_deref())
        .and_then(|pid| store::VoiceprintStore::resolve(&vp, pid))
        .map(|rid| (rid.to_string(), vp.people.get(rid).map(|p| p.name.clone()).unwrap_or_default()));
    let split_born = note.speakers.get(speaker_id).is_some_and(|m| m.split_born);
    app.state::<lifecycle::LifecycleHandle>().request(lifecycle::machine::Msg::EditNote {
        op: lifecycle::machine::EditOp::AssignPerson {
            id: note_id.to_string(),
            speaker_id: speaker_id.to_string(),
            person_id: resolved.clone(),
        },
    })?;
    if split_born {
        // 确认才入库:只有用户刚试听过的那段(audited_seq 且确属该说话人)进库。
        let audited = audited_seq.and_then(|q| {
            note.segments.iter().find(|s| s.seq == q && s.speaker.as_deref() == Some(speaker_id)).cloned()
        });
        if let Some(seg) = audited {
            spawn_confirmed_sample(app, note_id.to_string(), speaker_id.to_string(), resolved, note.segments, seg);
        }
    } else {
        spawn_feedback(
            app,
            note_id.to_string(),
            note.segments,
            feedback::SegFilter::Speakers(std::collections::BTreeSet::from([speaker_id.to_string()])),
            prior,
            resolved,
            Some(speaker_id.to_string()),
        );
    }
    Ok(())
}

/// 用户确认样本的后台落库:切出音频存样本(append_confirmed_sample,免老熟人策略)
/// + 该段单独回灌质心(reinforce_person,走普通门禁与账本)。失败只记日志——本篇
/// 关联已生效,库写入是增强不是前提。
fn spawn_confirmed_sample(
    app: &AppHandle,
    note_id: String,
    speaker_id: String,
    person_id: String,
    segments: Vec<store::SegmentRecord>,
    seg: store::SegmentRecord,
) {
    let app = app.clone();
    std::thread::spawn(move || {
        let run = || -> anyhow::Result<()> {
            let root = data_root(&app).map_err(anyhow::Error::msg)?;
            let nroot = notes_dir(&app).map_err(anyhow::Error::msg)?;
            let dir = nroot.join(&note_id);
            let vp_store = store::VoiceprintStore::new(root);
            let _fb = FEEDBACK_GATE.lock().unwrap();
            // 切音频:与 feedback 同一口径(track_pcm + offset_ms,16k f32)。
            let meta = store::audio::load_audio_meta(&dir);
            let pcm = store::transcode::track_pcm(&dir, &seg.source)?;
            let offset = meta.tracks.get(&seg.source).map(|t| t.offset_ms).unwrap_or(0);
            let start = (seg.start_ms.saturating_sub(offset) as usize).saturating_mul(16);
            let end = ((seg.end_ms.saturating_sub(offset) as usize).saturating_mul(16)).min(pcm.len());
            anyhow::ensure!(start < end, "试听段落在音轨覆盖范围之外");
            let wrote = vp_store.append_confirmed_sample(&person_id, &pcm[start..end], &note_id, &speaker_id)?;
            if !wrote {
                eprintln!("确认样本未写入(隔离/满员/空音频): {person_id}");
            }
            // 单段回灌质心(模型门禁/账本/黑名单照过)。
            let expected = current_speaker_model(&app);
            let library_model = vp_store.load().embedding_model.clone();
            let mut embedder = diar::SherpaEmbedder::new(&speaker_model_path_for(&expected))?;
            let mut needs_rebuild = false;
            let now = chrono::Local::now().to_rfc3339();
            let r = feedback::reinforce_person(
                &dir,
                &segments,
                &feedback::SegFilter::Seqs(std::collections::BTreeSet::from([seg.seq])),
                &person_id,
                &vp_store,
                &library_model,
                &expected,
                &mut embedder,
                &now,
                None,
                &mut needs_rebuild,
                false,
            )?;
            if needs_rebuild {
                let st = app.state::<AppState>();
                *st.embedder_cache.lock().unwrap() = None;
                spawn_voiceprint_rebuild(&app, st.embedder_cache.clone(), "确认样本回灌纠错后质心置空");
            }
            eprintln!("确认样本入库: {person_id} seg#{} → {r:?}", seg.seq);
            Ok(())
        };
        if let Err(e) = run() {
            eprintln!("确认样本入库失败(本篇关联不受影响): {e}");
        }
    });
}

/// 解除说话人与声纹库人物的关联,并**连带撤销**这次关联带来的声纹回灌。
///
/// 清掉 person_id 之后 name 必然还是空串(关联时就把本地名清了),显示回落到
/// 「新说话人 N」。表项与段落归属一概不动——与"删除说话人"不是一回事。
///
/// **已知不可撤销的一种**:若当初的指认走的是 `FeedbackAction::MergePrior`
/// (先前关联的是无名自动人物 → 整个人物被并进目标),那次合并是库级 journaled 操作,
/// 有自己的撤销入口(收件箱回执),不在这里连带回滚——回灌账本里根本没有它的条目,
/// 撤销会如实报 NoEntry 并落日志。硬要在这里 un-merge 会把两本账搅乱。
#[tauri::command]
fn clear_note_speaker_person(
    app: AppHandle,
    state: State<AppState>,
    note_id: String,
    speaker_id: String,
) -> Result<(), String> {
    reject_if_active(&state, &note_id)?;
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    let note = store::NoteStore::new(dir).load(&note_id).map_err(|e| e.to_string())?;
    // 撤销回灌要的两样东西必须在清空之前取:清完就查不到当初关联的是谁了。
    // person_id 取的是 load 后(经 redirects 归一)的值,与账本的比较口径一致。
    let linked = note.speakers.get(&speaker_id).and_then(|m| m.person_id.clone());
    let seqs: std::collections::BTreeSet<u64> = note
        .segments
        .iter()
        .filter(|s| s.speaker.as_deref() == Some(speaker_id.as_str()))
        .map(|s| s.seq)
        .collect();
    app.state::<lifecycle::LifecycleHandle>().request(lifecycle::machine::Msg::EditNote {
        op: lifecycle::machine::EditOp::ClearPerson {
            id: note_id.clone(),
            speaker_id: speaker_id.clone(),
        },
    })?;
    // **不连带撤销这次关联带来的声纹回灌**(2026-08-19 范围决定)。
    // 撤销要求"撤销任务"与"回灌任务"两个后台任务正确排序,而它们只隔着一把不保证
    // 顺序的门——三轮 codex review 里最难缠的几条 P1(反向执行竞态、MergePrior 漏
    // 复核、账本误撤)根源全在这里。砍掉撤销任务,这些问题不是被堵住,是不存在。
    // 代价:库里那个人多留一段本不该有的样本。见
    // docs/superpowers/specs/2026-08-19-voiceprint-model-space-design.md
    let _ = (linked, seqs);
    Ok(())
}

// ── 多人混杂:打标(quarantine_only)四命令。设计:2026-08-20-mixed-speaker-split-design.md ──

#[derive(serde::Serialize)]
struct MultiImpactPerson {
    person_id: String,
    name: String,
    /// 该簇对此人的 count 贡献(质心贡献 receipt 汇总)。**估算**,不是账目。
    cluster_count_est: u64,
    /// 此人主质心的累计 count(各信道求和),做占比分母。
    person_count_total: u64,
    /// 是否存在与该场入库同刻的会话质心。措辞只能是"该场存在",不能说"来自被标簇"
    /// (同场干净簇也可能写入,系统分不出来)。
    has_session_centroid: bool,
    total_ms: u64,
    last_seen: String,
    samples: Vec<MultiImpactSample>,
}

#[derive(serde::Serialize)]
struct MultiImpactSample {
    /// 相对声纹根的路径(voiceprints/P15-2.wav),删除 API 用它。
    path: String,
    /// 绝对路径:前端 convertFileSrc 试听用。
    audition_path: String,
    /// 有溯源且来自本篇被标簇 → 可自动删;false = 「来源未知」,只能试听勾删。
    from_marked_cluster: bool,
}

#[derive(serde::Serialize)]
struct MultiImpactReport {
    op_id: String,
    phase: String,
    persons: Vec<MultiImpactPerson>,
}

/// 打「多人混杂」标。speaker_ids 是原始稿 S 编号(修订稿 R 没有存储位,UI 先映射)。
/// 顺序:plan+隔离(同一 vp_guard,原子)→ 作废旧 identify 建议(IDENTIFY_ACT_GATE 内,
/// 失败即失败,不静默)→ 笔记侧标记(actor/NoteLock)→ marked。
/// 幂等恢复:同笔记同说话人集合已有 plan 阶段的 op → 复用续跑,不再新建
/// (codex 实现轮一 P1④⑤⑬)。
#[tauri::command]
fn mark_speaker_multi(
    app: AppHandle,
    note_id: String,
    speaker_ids: Vec<String>,
) -> Result<String, String> {
    store::validate_note_id(&note_id).map_err(|e| e.to_string())?;
    if speaker_ids.is_empty() {
        return Err(tr!("没有选择说话人", "No speaker selected"));
    }
    let root = data_root(&app).map_err(|e| e.to_string())?;
    let nroot = notes_dir(&app).map_err(|e| e.to_string())?;
    let note = store::NoteStore::new(nroot.clone()).load(&note_id).map_err(|e| e.to_string())?;
    for sid in &speaker_ids {
        if !note.speakers.contains_key(sid) {
            return Err(tr!("笔记中没有该说话人: {sid}", "No such speaker in this note: {sid}", sid = sid));
        }
    }
    let vp_store = store::VoiceprintStore::new(root.clone());
    let now = chrono::Local::now().to_rfc3339();
    let marked_seqs: std::collections::BTreeSet<u64> = note
        .segments
        .iter()
        .filter(|s| s.speaker.as_deref().is_some_and(|sp| speaker_ids.iter().any(|x| x == sp)))
        .map(|s| s.seq)
        .collect();
    let dir = nroot.join(&note_id);
    // **先持 IDENTIFY_ACT_GATE,罩住计划+隔离+作废全程**:auto_apply 持同一把门做
    // 关联+回灌,不前置的话它可以插在"计划算完(目标还是 suggested,没进 affected)"
    // 与"作废落盘"之间完成回灌——混杂段进了一个没被隔离的人(codex 实现轮二 P1②)。
    // 锁序恒 IDENTIFY_ACT_GATE → vp_guard/actor,与 auto_apply_one 同向。
    let _act_gate = IDENTIFY_ACT_GATE.lock().unwrap();
    let members = store::load_refined(&dir)
        .map(|doc| refine::identify::cluster_members_from_doc(&doc))
        .unwrap_or_default();
    // ── plan + 受影响人物 + 隔离:同一 vp_guard 内原子完成(解析与置位之间不许插入
    //    合并;plan 先于隔离落盘)。复用既有 plan 阶段的 op(上次卡在后半程)。 ──
    static SPLIT_OP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // 时间戳进 id:PID 会复用、计数器重启归零,只有 pid+计数在重启后能撞上旧 op 并
    // 覆盖它(codex 实现轮二 P1⑦);create() 的存在性检查是最后一道闸。
    let candidate_op_id = format!(
        "so-{}-{}-{}",
        chrono::Local::now().format("%Y%m%d%H%M%S%3f"),
        std::process::id(),
        SPLIT_OP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let op_id = vp_store
        .with_guard(|| {
            let mut sorted_ids = speaker_ids.clone();
            sorted_ids.sort();
            // 复用:同笔记同说话人集合、卡在 plan 的 op。
            let existing = store::split_ops::open_ops_for_note(&root, &note_id)
                .into_iter()
                .find(|o| {
                    let mut a = o.speaker_ids.clone();
                    a.sort();
                    o.phase == store::split_ops::phase::PLAN && a == sorted_ids
                });
            let vp = vp_store.load();
            let mut affected: std::collections::BTreeSet<String> = Default::default();
            for sid in &speaker_ids {
                if let Some(pid) = note.speakers.get(sid).and_then(|m| m.person_id.as_deref()) {
                    if let Some(r) = store::VoiceprintStore::resolve(&vp, pid) {
                        affected.insert(r.to_string());
                    }
                }
            }
            for r in store::sample_trace::read_centroid_receipts(&root) {
                if r.note_id == note_id && speaker_ids.iter().any(|s| s == &r.cluster_id) {
                    if let Some(p) = store::VoiceprintStore::resolve(&vp, &r.resolved_person) {
                        affected.insert(p.to_string());
                    }
                }
            }
            // identify 自动应用过、且簇成员与被标段重叠的人物:混杂段已被回灌进去,
            // 一并隔离(codex 实现轮一 P1⑬)。
            if let Some(idoc) = refine::identify::load_identify(&dir) {
                for a in &idoc.assignments {
                    if a.status != "auto_applied" && a.status != "applied" {
                        continue;
                    }
                    let overlap = members
                        .get(&a.cluster)
                        .is_some_and(|m| m.intersection(&marked_seqs).next().is_some());
                    if overlap {
                        if let Some(pid) = a.person_id.as_deref() {
                            if let Some(r) = store::VoiceprintStore::resolve(&vp, pid) {
                                affected.insert(r.to_string());
                            }
                        }
                    }
                }
            }
            let op = match existing {
                Some(mut o) => {
                    // 并集,不覆盖:上次已执行的 SetMultiSpeaker 清掉了 person_id,
                    // 这次重算会看不到那些人——缩小集合等于把已隔离的人永久遗弃
                    // (codex 实现轮二 P1①)。
                    for p in &o.affected_persons {
                        affected.insert(p.clone());
                    }
                    o.affected_persons = affected.iter().cloned().collect();
                    o.updated_at = now.clone();
                    store::split_ops::save(&root, &o)?;
                    o
                }
                None => {
                    // 撤销要恢复的人物关联快照:SetMultiSpeaker 马上会清掉 person_id,
                    // 此刻不记就没了(undo_auto_split 只动本篇表项,不触库)。
                    let mut prior_links: std::collections::BTreeMap<String, String> =
                        Default::default();
                    for sid in &speaker_ids {
                        if let Some(pid) = note.speakers.get(sid).and_then(|m| m.person_id.as_deref())
                        {
                            if let Some(r) = store::VoiceprintStore::resolve(&vp, pid) {
                                prior_links.insert(sid.clone(), r.to_string());
                            }
                        }
                    }
                    let o = store::split_ops::SplitOp {
                        op_id: candidate_op_id.clone(),
                        mode: "quarantine_only".into(),
                        note_id: note_id.clone(),
                        speaker_ids: speaker_ids.clone(),
                        affected_persons: affected.iter().cloned().collect(),
                        phase: store::split_ops::phase::PLAN.into(),
                        residual_choice: None,
                        samples_confirm_seen: false,
                        plan_groups: Vec::new(),
                        prior_links,
                        undone_at: None,
                        created_at: now.clone(),
                        updated_at: now.clone(),
                    };
                    store::split_ops::create(&root, &o)?;
                    o
                }
            };
            // 隔离置位与解析同锁:中间不可能插入合并让 id 失效。
            let mut vp = vp_store.load();
            let mut changed = false;
            for pid in &op.affected_persons {
                if let Some(p) = vp.people.get_mut(pid) {
                    if !p.voiceprint_quarantined {
                        p.voiceprint_quarantined = true;
                        changed = true;
                    }
                }
            }
            if changed {
                vp_store.save_for_split(&vp)?;
            }
            Ok(op.op_id)
        })
        .map_err(|e| e.to_string())?;
    // ── 作废旧 identify 建议:在 IDENTIFY_ACT_GATE 内,失败就失败(op 停在 plan,
    //    可重试)——静默吞掉的话旧建议还能把混杂段灌回库(codex 实现轮一 P1⑬)。 ──
    {
        refine::identify::invalidate_for_marking(&dir, &{
            store::split_ops::load(&root, &op_id)
                .map_err(|e| e.to_string())?
                .affected_persons
                .into_iter()
                .collect()
        }, &marked_seqs, &members, &now)
        .map_err(|e| e.to_string())?;
    }
    // ── 笔记侧标记(actor 持 NoteLock 串行落盘;附带清 person_id)。幂等。 ──
    let lc = app.state::<lifecycle::LifecycleHandle>();
    for sid in &speaker_ids {
        lc.request(lifecycle::machine::Msg::EditNote {
            op: lifecycle::machine::EditOp::SetMultiSpeaker {
                id: note_id.clone(),
                speaker_id: sid.clone(),
            },
        })?;
    }
    store::split_ops::advance_guarded(
        &vp_store,
        &root,
        &op_id,
        &[store::split_ops::phase::PLAN],
        store::split_ops::phase::MARKED,
        &now,
    )
    .map_err(|e| e.to_string())?;
    Ok(op_id)
}

/// 打标影响面(供确认面板):每个受影响人物的 count 占比估算、会话质心存在性、
/// 元数据残留、样本清单(标注哪些可归因)。纯读。
#[tauri::command]
fn multi_impact(app: AppHandle, op_id: String) -> Result<MultiImpactReport, String> {
    let root = data_root(&app).map_err(|e| e.to_string())?;
    let op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
    let vp = store::VoiceprintStore::new(root.clone()).load();
    let receipts = store::sample_trace::read_centroid_receipts(&root);
    let trace = store::sample_trace::load(&root);
    let mut persons = Vec::new();
    for pid in &op.affected_persons {
        let Some(p) = vp.people.get(pid) else { continue };
        let mine: Vec<_> = receipts
            .iter()
            .filter(|r| {
                r.note_id == op.note_id
                    && op.speaker_ids.iter().any(|s| s == &r.cluster_id)
                    && store::VoiceprintStore::resolve(&vp, &r.resolved_person).is_some_and(|x| x == pid.as_str())
            })
            .collect();
        let cluster_count_est: u64 = mine.iter().map(|r| r.count).sum();
        let ats: std::collections::BTreeSet<&str> = mine.iter().map(|r| r.at.as_str()).collect();
        let has_session_centroid = p
            .session_centroids
            .values()
            .flatten()
            .any(|c| ats.contains(c.seen.as_str()));
        let store_h = store::VoiceprintStore::new(root.clone());
        let samples = store_h
            .sample_paths_existing(pid)
            .iter()
            .map(|abs| {
                let rel = abs.strip_prefix(&root).unwrap_or(abs).to_string_lossy().into_owned();
                let from_marked = trace.receipts.iter().any(|r| {
                    r.path == rel
                        && r.note_id == op.note_id
                        && op.speaker_ids.iter().any(|s| s == &r.cluster_id)
                });
                MultiImpactSample {
                    path: rel,
                    audition_path: abs.to_string_lossy().into_owned(),
                    from_marked_cluster: from_marked,
                }
            })
            .collect();
        persons.push(MultiImpactPerson {
            person_id: pid.clone(),
            name: p.name.clone(),
            cluster_count_est,
            person_count_total: p.centroids.values().map(|c| c.count).sum(),
            has_session_centroid,
            total_ms: p.total_ms,
            last_seen: p.last_seen.clone(),
            samples,
        });
    }
    Ok(MultiImpactReport { op_id: op.op_id, phase: op.phase, persons })
}

/// 样本处置:自动删可归因的(receipt 校验 path+hash),用户勾选的额外删除逐份
/// hash 封禁后删。confirm_seen 落盘为"用户已确认看到信息缺口"。
#[tauri::command]
fn confirm_multi_samples(
    app: AppHandle,
    op_id: String,
    extra_delete: Vec<String>,
    confirm_seen: bool,
) -> Result<u32, String> {
    let root = data_root(&app).map_err(|e| e.to_string())?;
    let op_lock = split_op_lock(&op_id);
    let _op_guard = op_lock.lock().unwrap();
    // **锁后重读**:锁前的快照可能停在"还没选残留"的旧状态——residual 若在本请求
    // 等锁期间落了意图并跑完 baseline,拿旧快照过冻结检查再删样本,删完才在阶段
    // CAS 上失败,但删除已无法撤销(codex 实现轮四 P1①)。
    let op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
    if op.phase != store::split_ops::phase::MARKED
        && op.phase != store::split_ops::phase::SAMPLES_HANDLED
    {
        return Err(tr!("当前阶段不能处置样本: {p}", "Cannot handle samples in phase {p}", p = &op.phase));
    }
    // 意图落盘后样本集合冻结:residual 选择(尤其 baseline)以当时的样本为输入,
    // 再 purge 会让重算的幂等性失效(codex 实现轮三 P2)。
    if op.residual_choice.is_some() {
        return Err(tr!(
            "残留处置已开始,样本集合已冻结",
            "Residual handling has started; the sample set is frozen"
        ));
    }
    // 阶段落盘必须证明用户看到过信息缺口:不带确认不推进,重入也不许把已确认改回
    // 未确认(codex 实现轮一 P2)。
    if !confirm_seen {
        return Err(tr!(
            "请先确认已了解样本无法归因的说明",
            "Please confirm you understand the attribution gap first"
        ));
    }
    let vp_store = store::VoiceprintStore::new(root.clone());
    let deleted = vp_store
        .purge_marked_samples(&op, &extra_delete)
        .map_err(|e| e.to_string())?;
    let now = chrono::Local::now().to_rfc3339();
    vp_store
        .with_guard(|| {
            let mut o = store::split_ops::load(&root, &op_id)?;
            anyhow::ensure!(
                o.phase == store::split_ops::phase::MARKED
                    || o.phase == store::split_ops::phase::SAMPLES_HANDLED,
                "阶段已变: {}",
                o.phase
            );
            o.phase = store::split_ops::phase::SAMPLES_HANDLED.into();
            o.samples_confirm_seen = true;
            o.updated_at = now.clone();
            store::split_ops::save(&root, &o)
        })
        .map_err(|e| e.to_string())?;
    Ok(deleted)
}

/// 残留二选一并收尾:accept = 质心不动;baseline = 逐人重算(退回样本基线)。
/// 恢复语义(codex 实现轮一 P1⑩):residual_decided 阶段重入必须与已落盘的选择一致
/// (副作用在选择落盘**之前**执行,所以落盘值=已执行值);released 阶段重入只补 done。
/// 解除隔离排除其它未完成 op 仍持有的人物(P1⑤)。
#[tauri::command]
async fn resolve_multi_residual(
    app: AppHandle,
    op_id: String,
    choice: String,
    then_split: bool,
) -> Result<(), String> {
    if choice != "accept" && choice != "baseline" {
        return Err(tr!("未知选项: {choice}", "Unknown choice: {choice}", choice = &choice));
    }
    tauri::async_runtime::spawn_blocking(move || {
        let root = data_root(&app).map_err(|e| e.to_string())?;
        let op_lock = split_op_lock(&op_id);
        let _op_guard = op_lock.lock().unwrap();
        let op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
        let vp_store = store::VoiceprintStore::new(root.clone());
        let now = chrono::Local::now().to_rfc3339();
        use store::split_ops::phase as ph;
        match op.phase.as_str() {
            p if p == ph::SAMPLES_HANDLED => {
                // 意图先落盘,副作用后执行,阶段最后推进(codex 实现轮二 P1④)。
                // 意图 CAS 收进 guard 内(锁内重读):两个并发请求各自拿着"还没选"的
                // 旧快照时,后进 guard 的那个必须看见先者写的 choice 并被拒
                // (codex 实现轮三 P1④;op 锁已串行化,这是纵深防御)。
                vp_store
                    .with_guard(|| {
                        let mut o = store::split_ops::load(&root, &op_id)?;
                        anyhow::ensure!(o.phase == ph::SAMPLES_HANDLED, "阶段已变: {}", o.phase);
                        match &o.residual_choice {
                            Some(stored) if stored != &choice => {
                                anyhow::bail!("已选择过「{stored}」,恢复中不能改选")
                            }
                            Some(_) => {} // 同值重入:不重写
                            None => {
                                o.residual_choice = Some(choice.clone());
                                if then_split {
                                    o.mode = "split_commit".into();
                                }
                                o.updated_at = now.clone();
                                store::split_ops::save(&root, &o)?;
                            }
                        }
                        Ok(())
                    })
                    .map_err(|e| e.to_string())?;
                if choice == "baseline" {
                    // 幂等:rebuild_person_from_samples 重跑得到同一结果。
                    if let Err(e) = run_baseline_reset(&app, &vp_store, &op) {
                        consume_pending_rebuild(&app); // pending 不能没人管
                        return Err(e);
                    }
                }
                // **以落盘的 mode 为准,不信本次请求参数**(codex 实现轮四 P1②):
                // advance 的返回值就是刚落盘的 op,不再单独 load——单独 load 的瞬时
                // 失败若回退到请求参数,旧问题原样回来(轮五 P1②)。
                let advanced = store::split_ops::advance_guarded(
                    &vp_store, &root, &op_id, &[ph::SAMPLES_HANDLED], ph::RESIDUAL_DECIDED, &now,
                )
                .map_err(|e| e.to_string())?;
                if advanced.mode == "split_commit" {
                    // pending 重建**不在这里**消化:人物还隔离着,全库重建会把刚算的
                    // 基线按"隔离只清空"冲掉(codex 实现轮三 P1②)。commit/cancel
                    // 完成解除后消化。
                    return Ok(());
                }
            }
            p if p == ph::RESIDUAL_DECIDED => {
                // 重入:选择已落盘已执行,只允许同值收尾;拆分模式不从这里收尾。
                let stored = op.residual_choice.clone().unwrap_or_default();
                if stored != choice {
                    return Err(tr!(
                        "已选择过「{stored}」,恢复中不能改选",
                        "Already chose \"{stored}\"; cannot change during recovery",
                        stored = &stored
                    ));
                }
                if op.mode == "split_commit" {
                    // 落盘意图优先于请求参数(轮四 P1②)。
                    return Err(tr!("拆分模式由拆分流程收尾", "Split mode finishes via the split flow"));
                }
            }
            p if p == ph::RELEASED => {
                // 重入:公共收尾(重跑解除+done+pending+缓存+图谱,轮四 P1③)。
                return complete_released(&app, &vp_store, &root, &op_id, &now);
            }
            p => {
                return Err(tr!("先完成样本处置(当前阶段: {p})", "Handle samples first (phase: {p})", p = p));
            }
        }
        finish_and_release(&vp_store, &root, &op_id, &[ph::RESIDUAL_DECIDED], ph::RELEASED, &now)
            .map_err(|e| e.to_string())?;
        complete_released(&app, &vp_store, &root, &op_id, &now)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// baseline 重算的执行体:与全库重建共用 REBUILD_RUNNING 单飞;结束后消化
/// REBUILD_PENDING(期间若有人切模型,请求被记为 pending,没人消化的话库会长期
/// 停在旧空间——codex 实现轮一 P1⑨)。
fn run_baseline_reset(
    app: &AppHandle,
    vp_store: &store::VoiceprintStore,
    op: &store::split_ops::SplitOp,
) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    {
        // 起止交接都进 REBUILD_CTL(轮八 P1:锁外摸这些原子量就有交接窗)。
        let _ctl = REBUILD_CTL.lock().unwrap();
        if REBUILD_RUNNING.swap(true, Ordering::SeqCst) {
            return Err(tr!("声纹库重建进行中,稍后再试", "A library rebuild is running; try again later"));
        }
    }
    let r = (|| -> Result<(), String> {
        let tag = current_speaker_model(app);
        let mut e = diar::SherpaEmbedder::new(&speaker_model_path_for(&tag))
            .map_err(|e| tr!("声纹模型不可用: {e}", "Speaker model unavailable: {e}", e = e))?;
        for pid in &op.affected_persons {
            vp_store.rebuild_person_from_samples(pid, &mut e, &tag).map_err(|e| e.to_string())?;
        }
        Ok(())
    })();
    {
        let _ctl = REBUILD_CTL.lock().unwrap();
        REBUILD_RUNNING.store(false, Ordering::SeqCst);
    }
    // 注意:排队的全库重建**不在这里**消化——人物还隔离着,全库重建会把刚算的基线
    // 清空。调用方在解除隔离之后调 consume_pending_rebuild(codex 实现轮二 P1⑤);
    // 出错路径也由调用方兜(pending 不能没人管)。PENDING/标记留在原位,进程中途
    // 退出也有启动补跑兜住(标记是排队那次 spawn 在 CTL 内写的)。
    r
}

/// released 阶段的**公共收尾**(commit/residual 两侧共用,幂等):重跑解除(兜底)→
/// 补 done → 消化 pending 重建 → 刷缓存 →(拆分模式)排图谱重建。没有它,DONE 推进
/// 失败后的重入会各走各的半截路径,漏掉 pending/缓存/图谱(codex 实现轮四 P1③)。
fn complete_released(
    app: &AppHandle,
    vp_store: &store::VoiceprintStore,
    root: &std::path::Path,
    op_id: &str,
    now: &str,
) -> Result<(), String> {
    vp_store
        .with_guard(|| {
            let o = store::split_ops::load(root, op_id)?;
            release_for_op_locked(vp_store, root, &o)
        })
        .map_err(|e| e.to_string())?;
    // DONE **最后**落:它一落操作就从恢复列表消失,之前任何一步(图谱排队可失败)
    // 没做完都补不回来(codex 实现轮五 P1①)。前面各步全部幂等,重试安全。
    let op = store::split_ops::load(root, op_id).map_err(|e| e.to_string())?;
    consume_pending_rebuild(app);
    refresh_qwen_hotwords_cache(app);
    if op.mode == "split_commit" {
        queue_person_graph_rebuild(app, root.to_path_buf(), &tr!("拆分说话人", "Speaker split"))?;
    }
    store::split_ops::advance_guarded(
        vp_store,
        root,
        op_id,
        &[store::split_ops::phase::RELEASED],
        store::split_ops::phase::DONE,
        now,
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 消化 REBUILD_PENDING(若有)。与 spawn_voiceprint_rebuild 的排队协议配套:
/// pending 在 RUNNING 期间被置位,清 RUNNING 的一方负责消化。
fn consume_pending_rebuild(app: &AppHandle) {
    use std::sync::atomic::Ordering;
    // swap 进 CTL(轮八 P1);spawn 在锁外(它自己要取 CTL,不可重入)。
    // "swap 完、spawn 前"崩掉不会丢诉求:排队那次 spawn 已在 CTL 内写了落盘标记,
    // 此刻没有 runner 会清它(清标记只发生在 runner 收尾且 PENDING=false&&成功),
    // 下次启动按标记补跑。
    let go = {
        let _ctl = REBUILD_CTL.lock().unwrap();
        REBUILD_PENDING.swap(false, Ordering::SeqCst)
    };
    if go {
        let st = app.state::<AppState>();
        spawn_voiceprint_rebuild(app, st.embedder_cache.clone(), "基线重算期间排队的重建");
    }
}

/// **同一 vp_guard 内**把本 op 推进到"不再持有"的阶段并解除隔离。两件事必须原子:
/// 分开做的话,重叠的两个 op 各自在解除时看到对方"未关单"而跳过共享人物,随后又
/// 各自关单——全部完成、人物却永久隔离(codex 实现轮二 P1③)。原子化后,后关单的
/// 那个 op 一定能看到先关单者已处于非持有阶段,补上解除。
/// `from`/`to`:本 op 的阶段推进(to 必须是非持有阶段:released / cancelled)。
fn finish_and_release(
    vp_store: &store::VoiceprintStore,
    root: &std::path::Path,
    op_id: &str,
    from: &[&str],
    to: &str,
    now: &str,
) -> anyhow::Result<()> {
    vp_store.with_guard(|| {
        // **先解除、后推进**(同一 guard):顺序反过来的话,"阶段已 released、解除没写成"
        // 的崩溃窗会让独占人物永久隔离——released 重入只补 done,cancelled 连恢复列表
        // 都不进(codex 实现轮三 P1①)。现在崩在中间 → 阶段未变 → 重入重跑解除(幂等)。
        // 持有者判定读的是**别的 op** 的盘面阶段,与自身阶段无关,所以先后互换不
        // 影响轮二 P1③ 的原子性结论:后关单者仍然会看到先关单者已是非持有态。
        let op = store::split_ops::load(root, op_id)?;
        anyhow::ensure!(
            from.contains(&op.phase.as_str()),
            "阶段不符:当前 {},不能收尾到 {to}",
            op.phase
        );
        release_for_op_locked(vp_store, root, &op)?;
        store::split_ops::advance(root, op_id, from, to, now)?;
        Ok(())
    })
}

/// 解除本 op 人物的隔离(排除其它持有者)。**调用方须已持 vp_guard**。幂等。
fn release_for_op_locked(
    vp_store: &store::VoiceprintStore,
    root: &std::path::Path,
    op: &store::split_ops::SplitOp,
) -> anyhow::Result<()> {
    let held: std::collections::BTreeSet<String> = store::split_ops::open_ops_all(root)
        .into_iter()
        .filter(|o| o.op_id != op.op_id && store::split_ops::holds_quarantine(o))
        .flat_map(|o| o.affected_persons)
        .collect();
    let mut vp = vp_store.load();
    let mut changed = false;
    for pid in &op.affected_persons {
        if held.contains(pid) {
            eprintln!("解除隔离跳过:{pid} 仍被其它未完成处置持有");
            continue;
        }
        if let Some(p) = vp.people.get_mut(pid) {
            if p.voiceprint_quarantined {
                p.voiceprint_quarantined = false;
                changed = true;
            }
        }
    }
    if changed {
        vp_store.save_for_split(&vp)?;
    }
    Ok(())
}

/// 进程内按 op 串行化(commit/cancel/confirm/residual 四命令共用):同一 op 的两个
/// 命令并发交错会产生"读旧阶段 → 各做各的副作用"的竞态(codex 实现轮三 P1⑤)。
/// 客户端单进程,进程内互斥即可;跨进程不在本设计承诺内(见设计文档推迟项)。
fn split_op_lock(op_id: &str) -> std::sync::Arc<std::sync::Mutex<()>> {
    static LOCKS: std::sync::Mutex<
        std::collections::BTreeMap<String, std::sync::Arc<std::sync::Mutex<()>>>,
    > = std::sync::Mutex::new(std::collections::BTreeMap::new());
    LOCKS
        .lock()
        .unwrap()
        .entry(op_id.to_string())
        .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(())))
        .clone()
}

#[derive(serde::Serialize)]
struct SplitSuggestGroup {
    seqs: Vec<u64>,
    total_ms: u64,
    suggested: Option<(String, String, f32)>, // (person_id, name, cosine)
}

#[derive(serde::Serialize)]
struct SplitSuggestOut {
    groups: Vec<SplitSuggestGroup>,
    /// 无法判定的段(过短/嵌入失败/轨道缺失):不猜,单独一桶交给人。
    undetermined: Vec<u64>,
}

#[derive(serde::Deserialize)]
struct SplitGroupIn {
    seqs: Vec<u64>,
    dest_kind: String, // existing_speaker | person | new_speaker | keep
    dest_id: Option<String>,
}

/// 建议分组:对被标簇的段落单独跑一次拆分专用聚类(关碎片吞并;无嵌入进无法判定桶;
/// 种子给去处建议)。纯本地纯读,不写任何东西。重活(解码+逐段嵌入)在 spawn_blocking,
/// FEEDBACK_GATE 约束 ORT 并发(与回灌同一门)。
#[tauri::command]
async fn suggest_split_groups(app: AppHandle, op_id: String) -> Result<SplitSuggestOut, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let root = data_root(&app).map_err(|e| e.to_string())?;
        let op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
        let nroot = notes_dir(&app).map_err(|e| e.to_string())?;
        let dir = nroot.join(&op.note_id);
        let note = store::NoteStore::new(nroot).load(&op.note_id).map_err(|e| e.to_string())?;
        let segs: Vec<&store::SegmentRecord> = note
            .segments
            .iter()
            .filter(|s| s.speaker.as_deref().is_some_and(|sp| op.speaker_ids.iter().any(|x| x == sp)))
            .collect();
        if segs.is_empty() {
            return Err(tr!("被标说话人名下没有段落", "The marked speakers have no segments"));
        }
        let _fb = FEEDBACK_GATE.lock().unwrap();
        // 标签、权重、种子同源(同一次设置读取)。
        let tag = current_speaker_model(&app);
        let mut embedder = diar::SherpaEmbedder::new(&speaker_model_path_for(&tag))
            .map_err(|e| tr!("声纹模型不可用: {e}", "Speaker model unavailable: {e}", e = e))?;
        // 进度事件:大簇逐段嵌入要数分钟,前端横幅靠它区分「在算」与「卡死」。
        // 每 10 段发一次 + 首尾各一次,避免事件风暴。
        let note_id_ev = op.note_id.clone();
        let app_ev = app.clone();
        let total_hint = segs.len();
        let embs = refine::embed_all_with_progress(&dir, &segs, &mut embedder, &tag, &|done, total| {
            if done == 1 || done == total || done % 10 == 0 {
                let _ = app_ev.emit(
                    "auto_split_progress",
                    serde_json::json!({ "note_id": note_id_ev, "done": done, "total": total }),
                );
            }
        })
        .map_err(|e| e.to_string())?;
        let _ = total_hint;
        let inputs: Vec<refine::recluster::SegInput> = segs
            .iter()
            .map(|s| refine::recluster::SegInput {
                seq: s.seq,
                start_ms: s.start_ms,
                end_ms: s.end_ms,
                source: s.source.clone(),
                old_speaker: s.speaker.clone(),
            })
            .collect();
        let seeds = load_voiceprint_seeds_for(&app, &tag);
        let sug = refine::recluster::recluster_split(&inputs, &embs, &seeds);
        Ok(SplitSuggestOut {
            groups: sug
                .groups
                .into_iter()
                .map(|g| SplitSuggestGroup {
                    seqs: g.member_idx.iter().map(|&i| inputs[i].seq).collect(),
                    total_ms: g.total_ms,
                    suggested: g.suggested,
                })
                .collect(),
            undetermined: sug.undetermined_idx.iter().map(|&i| inputs[i].seq).collect(),
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 提交拆分:计划定稿 → 占号(reserved)→ 批量改派+修订稿同步(segments_reassigned)
/// → 受权回灌(reenrolled)→ 解除隔离(released→done)。按 op.phase 从任意中断点续跑;
/// 全部改派走显式 ID,重试不产生新编号。返回摘要(含回灌的如实结果)。
#[tauri::command]
async fn commit_split(
    app: AppHandle,
    op_id: String,
    groups: Vec<SplitGroupIn>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let root = data_root(&app).map_err(|e| e.to_string())?;
        // 同一 op 的 commit/cancel/confirm/residual 进程内串行:并发交错 = 各拿旧阶段
        // 做各的副作用(codex 实现轮三 P1⑤)。
        let op_lock = split_op_lock(&op_id);
        let _op_guard = op_lock.lock().unwrap();
        let vp_store = store::VoiceprintStore::new(root.clone());
        let mut op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
        if op.mode != "split_commit" {
            return Err(tr!("该操作不在拆分模式", "This operation is not in split mode"));
        }
        let nroot = notes_dir(&app).map_err(|e| e.to_string())?;
        let nstore = store::NoteStore::new(nroot.clone());
        let dir = nroot.join(&op.note_id);
        let now = chrono::Local::now().to_rfc3339();
        let lc = app.state::<lifecycle::LifecycleHandle>();

        // ── 阶段 1:计划定稿 + 占号(residual_decided → reserved) ──
        if op.phase == store::split_ops::phase::RESIDUAL_DECIDED {
            if groups.is_empty() {
                return Err(tr!("没有分组", "No groups"));
            }
            // 孤儿清理**先于**选号与建表读取:上次占号成功、阶段没落盘时,残留的预留
            // 项会把 max(S) 抬高,每次重试都换更大的号;放在 to_reserve 判空之后则
            // "重试计划不需要新号"时孤儿永远清不掉(codex 实现轮二 P2⑧)。
            lc.request(lifecycle::machine::Msg::EditNote {
                op: lifecycle::machine::EditOp::ReleaseReservedSpeakers {
                    id: op.note_id.clone(),
                    op_id: op.op_id.clone(),
                },
            })?;
            let note = nstore.load(&op.note_id).map_err(|e| e.to_string())?;
            let vp = vp_store.load();
            let by_seq: std::collections::BTreeMap<u64, &store::SegmentRecord> =
                note.segments.iter().map(|s| (s.seq, s)).collect();
            // seq 不重不漏地属于被标说话人。
            let mut seen_seqs: std::collections::BTreeSet<u64> = Default::default();
            for g in &groups {
                for q in &g.seqs {
                    let seg = by_seq
                        .get(q)
                        .ok_or_else(|| tr!("段不存在: {q}", "No such segment: {q}", q = q))?;
                    let sp = seg.speaker.as_deref().unwrap_or("");
                    if !op.speaker_ids.iter().any(|x| x == sp) {
                        return Err(tr!("段 {q} 不属于被标说话人", "Segment {q} is not under a marked speaker", q = q));
                    }
                    if !seen_seqs.insert(*q) {
                        return Err(tr!("段 {q} 出现在多个组", "Segment {q} appears in multiple groups", q = q));
                    }
                }
            }
            // 不漏:分组必须覆盖被标说话人名下的**全部**段(UI 的"无法判定"桶以
            // dest=keep 送来,等式才成立)。漏段静默留在混杂簇里,op 却能关单
            // (codex 实现轮二 P2⑨)。
            let all_marked: std::collections::BTreeSet<u64> = note
                .segments
                .iter()
                .filter(|sg| {
                    sg.speaker.as_deref().is_some_and(|sp| op.speaker_ids.iter().any(|x| x == sp))
                })
                .map(|sg| sg.seq)
                .collect();
            if seen_seqs != all_marked {
                let missing = all_marked.difference(&seen_seqs).count();
                return Err(tr!(
                    "分组漏了 {missing} 段(被标说话人的段必须全部指定去处,拿不准选「保持不动」)",
                    "{missing} segments missing from groups (every marked segment needs a destination; pick keep-as-is when unsure)",
                    missing = missing
                ));
            }
            // 目标解析 + 预留数量。
            let mut need_reserve = 0usize;
            for g in &groups {
                match g.dest_kind.as_str() {
                    "existing_speaker" => {
                        let sid = g.dest_id.as_deref().unwrap_or("");
                        match note.speakers.get(sid) {
                            None => {
                                return Err(tr!("目标说话人不存在: {sid}", "No such speaker: {sid}", sid = sid))
                            }
                            Some(m) => {
                                // 别的 op 的预留号不许当去向(codex 实现轮二 P1⑥)。
                                if m.reserved_by.as_deref().is_some_and(|o| o != op.op_id) {
                                    return Err(tr!(
                                        "说话人 {sid} 是另一次拆分的预留号",
                                        "Speaker {sid} is reserved by another split",
                                        sid = sid
                                    ));
                                }
                            }
                        }
                    }
                    "person" => {
                        let pid = g.dest_id.as_deref().unwrap_or("");
                        let resolved = store::VoiceprintStore::resolve(&vp, pid)
                            .ok_or_else(|| tr!("声纹库中没有该人物: {pid}", "No such person: {pid}", pid = pid))?;
                        // 隔离中的人物只允许「本 op 的 A 类认领」:别的 op 正处置中的
                        // 人物不许当去向——那会绕过它的门禁写入(codex 实现轮一 P1⑥)。
                        let q = vp.people.get(resolved).is_some_and(|p| p.voiceprint_quarantined);
                        if q && !op.affected_persons.iter().any(|a| a == resolved) {
                            return Err(tr!(
                                "人物 {pid} 正被隔离处置,不能作为去向",
                                "Person {pid} is quarantined by another cleanup and cannot be a destination",
                                pid = pid
                            ));
                        }
                        // 已有关联 S 则复用,否则要一个新号。
                        let linked = note
                            .speakers
                            .iter()
                            .any(|(_, m)| m.person_id.as_deref() == Some(resolved));
                        if !linked {
                            need_reserve += 1;
                        }
                    }
                    "new_speaker" => need_reserve += 1,
                    "keep" => {}
                    other => return Err(tr!("未知去处: {other}", "Unknown destination: {other}", other = other)),
                }
            }
            let mut fresh = nstore
                .peek_next_speaker_ids(&op.note_id, need_reserve)
                .map_err(|e| e.to_string())?
                .into_iter();
            let mut plan: Vec<store::split_ops::SplitPlanGroup> = Vec::new();
            let mut to_reserve: Vec<String> = Vec::new();
            for g in &groups {
                let dest_speaker = match g.dest_kind.as_str() {
                    "existing_speaker" => g.dest_id.clone(),
                    "person" => {
                        let resolved = store::VoiceprintStore::resolve(&vp, g.dest_id.as_deref().unwrap())
                            .expect("上面已校验")
                            .to_string();
                        match note
                            .speakers
                            .iter()
                            .find(|(_, m)| m.person_id.as_deref() == Some(resolved.as_str()))
                        {
                            Some((sid, _)) => Some(sid.clone()),
                            None => {
                                let sid = fresh.next().expect("need_reserve 已计数");
                                to_reserve.push(sid.clone());
                                Some(sid)
                            }
                        }
                    }
                    "new_speaker" => {
                        let sid = fresh.next().expect("need_reserve 已计数");
                        to_reserve.push(sid.clone());
                        Some(sid)
                    }
                    _ => None, // keep
                };
                let mut seqs = g.seqs.clone();
                seqs.sort_unstable();
                let expected: Vec<String> = seqs
                    .iter()
                    .map(|q| by_seq[q].speaker.clone().unwrap_or_default())
                    .collect();
                plan.push(store::split_ops::SplitPlanGroup {
                    seqs,
                    expected_speakers: expected,
                    dest_kind: g.dest_kind.clone(),
                    dest_id: g.dest_id.clone(),
                    dest_speaker,
                });
            }
            // 计划先落盘,再占号(占号后崩溃:reserved_by 所有权 + 计划都在,可恢复可取消)。
            vp_store
                .with_guard(|| {
                    let mut o = store::split_ops::load(&root, &op_id)?;
                    o.plan_groups = plan.clone();
                    o.updated_at = now.clone();
                    store::split_ops::save(&root, &o)
                })
                .map_err(|e| e.to_string())?;
            if !to_reserve.is_empty() {
                lc.request(lifecycle::machine::Msg::EditNote {
                    op: lifecycle::machine::EditOp::ReserveSpeakers {
                        id: op.note_id.clone(),
                        speaker_ids: to_reserve,
                        op_id: op.op_id.clone(),
                    },
                })?;
            }
            op = store::split_ops::advance_guarded(
                &vp_store,
                &root,
                &op_id,
                &[store::split_ops::phase::RESIDUAL_DECIDED],
                store::split_ops::phase::RESERVED,
                &now,
            )
            .map_err(|e| e.to_string())?;
        }

        // ── 阶段 2:条件关联 → 批量改派 → 修订稿同步(reserved → segments_reassigned)。
        //    关联放**最前**:它带 CAS,用户改过关联时在任何段被动过之前干净停下
        //    (codex 实现轮三 P1③——冲突不是跳过继续,是停止计划);重入幂等(已是目标
        //    人物直接放行)。 ──
        if op.phase == store::split_ops::phase::RESERVED {
            {
                let vp = vp_store.load();
                for g in &op.plan_groups {
                    if g.dest_kind == "person" {
                        let (Some(pid), Some(sid)) = (g.dest_id.as_deref(), g.dest_speaker.as_deref()) else {
                            continue;
                        };
                        if let Some(resolved) = store::VoiceprintStore::resolve(&vp, pid) {
                            lc.request(lifecycle::machine::Msg::EditNote {
                                op: lifecycle::machine::EditOp::AssignPersonIf {
                                    id: op.note_id.clone(),
                                    speaker_id: sid.to_string(),
                                    person_id: resolved.to_string(),
                                },
                            })?;
                        }
                    }
                }
            }
            let moves: Vec<(u64, String, String)> = op
                .plan_groups
                .iter()
                .filter_map(|g| g.dest_speaker.as_ref().map(|d| (g, d)))
                .flat_map(|(g, d)| {
                    g.seqs
                        .iter()
                        .zip(&g.expected_speakers)
                        .filter(|(_, exp)| exp.as_str() != d.as_str())
                        .map(|(q, exp)| (*q, exp.clone(), d.clone()))
                        .collect::<Vec<_>>()
                })
                .collect();
            if !moves.is_empty() {
                lc.request(lifecycle::machine::Msg::EditNote {
                    op: lifecycle::machine::EditOp::SplitReassign {
                        id: op.note_id.clone(),
                        moves: moves.clone(),
                        op_id: op.op_id.clone(),
                    },
                })?;
            }
            // 修订稿同步:全组同去向原位改;跨组标 stale(一期边界)。
            // 一波说话人(2026-08-21):段落只改归属,身份显示现查 note.speakers,
            // 原 person_name 快照(codex 实现轮一 P2 的保身份逻辑)随之整体删除。
            let moved: std::collections::BTreeMap<u64, String> = op
                .plan_groups
                .iter()
                .filter_map(|g| g.dest_speaker.as_ref().map(|d| (g, d)))
                .flat_map(|(g, d)| g.seqs.iter().map(|q| (*q, d.clone())).collect::<Vec<_>>())
                .collect();
            if !moved.is_empty() && store::aing_exists(&dir) {
                match store::sync_refined_after_split(&dir, &moved) {
                    Ok(true) => eprintln!("拆分({op_id}):修订稿存在跨组段落,已标 stale 待重新 Aing"),
                    Ok(false) => {}
                    Err(e) => {
                        // 原始段已改派而修订稿没跟上:不标脏的话默认视图显示旧归属,
                        // 用户以为拆完了。降级标 stale;连 stale 都标不上就整体失败
                        // (重试安全:改派 CAS 认已完成态)——codex 实现轮一 P1⑪。
                        eprintln!("拆分({op_id}):修订稿同步失败,降级标 stale: {e}");
                        store::mark_refined_stale(&dir).map_err(|e2| {
                            format!("修订稿同步失败且标 stale 也失败,先重试: {e};{e2}")
                        })?;
                    }
                }
            }
            op = store::split_ops::advance_guarded(
                &vp_store,
                &root,
                &op_id,
                &[store::split_ops::phase::RESERVED],
                store::split_ops::phase::SEGMENTS_REASSIGNED,
                &now,
            )
            .map_err(|e| e.to_string())?;
        }

        // ── 阶段 3:受权回灌(segments_reassigned → reenrolled)。可选增强:失败如实
        //    报告,不回滚拆分、不挡收尾(设计:回灌只有方向性,不宣称修复)。 ──
        let mut enroll_notes: Vec<String> = Vec::new();
        if op.phase == store::split_ops::phase::SEGMENTS_REASSIGNED {
            let person_groups: Vec<_> =
                op.plan_groups.iter().filter(|g| g.dest_kind == "person").collect();
            if !person_groups.is_empty() {
                let _fb = FEEDBACK_GATE.lock().unwrap();
                let expected = current_speaker_model(&app);
                let library_model = vp_store.load().embedding_model.clone();
                match diar::SherpaEmbedder::new(&speaker_model_path_for(&expected)) {
                    Ok(mut embedder) => {
                        let note = nstore.load(&op.note_id).map_err(|e| e.to_string())?;
                        for g in person_groups {
                            let Some(pid) = g.dest_id.as_deref() else { continue };
                            let seqs: std::collections::BTreeSet<u64> = g.seqs.iter().copied().collect();
                            let mut needs_rebuild = false;
                            let r = feedback::reinforce_person(
                                &dir,
                                &note.segments,
                                &feedback::SegFilter::Seqs(seqs),
                                pid,
                                &vp_store,
                                &library_model,
                                &expected,
                                &mut embedder,
                                &now,
                                Some(&op.op_id),
                                &mut needs_rebuild,
                                // 只对本 op 认领的隔离人物放行;其它目标走普通门禁
                                // (codex 实现轮一 P1⑥:布尔旁路必须限定在 op 范围内)。
                                op.affected_persons.iter().any(|a| Some(a.as_str()) == g.dest_id.as_deref()),
                            );
                            if needs_rebuild {
                                let st = app.state::<AppState>();
                                *st.embedder_cache.lock().unwrap() = None;
                                spawn_voiceprint_rebuild(&app, st.embedder_cache.clone(), "拆分回灌纠错后质心置空");
                            }
                            match r {
                                Ok(feedback::ReinforceResult::Applied { .. }) => {}
                                Ok(other) => enroll_notes.push(format!("{pid}: {other:?}")),
                                Err(e) => enroll_notes.push(format!("{pid}: {e}")),
                            }
                        }
                    }
                    Err(e) => enroll_notes.push(format!("嵌入器不可用,回灌全部跳过: {e}")),
                }
            }
            op = store::split_ops::advance_guarded(
                &vp_store,
                &root,
                &op_id,
                &[store::split_ops::phase::SEGMENTS_REASSIGNED],
                store::split_ops::phase::REENROLLED,
                &now,
            )
            .map_err(|e| e.to_string())?;
        }

        // ── 阶段 4:解除隔离并收尾(推进+解除同一 guard,排除其它持有者) ──
        if op.phase == store::split_ops::phase::REENROLLED {
            finish_and_release(
                &vp_store,
                &root,
                &op_id,
                &[store::split_ops::phase::REENROLLED],
                store::split_ops::phase::RELEASED,
                &now,
            )
            .map_err(|e| e.to_string())?;
            op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
        }
        // released 的公共收尾(轮四 P1③):REENROLLED 刚推进来的、以及上次 DONE 没写成
        // 的重入,都从这里走同一条幂等路径(done+pending+缓存+图谱)。
        if op.phase == store::split_ops::phase::RELEASED {
            complete_released(&app, &vp_store, &root, &op_id, &now)?;
        }
        Ok(if enroll_notes.is_empty() { String::new() } else { enroll_notes.join("; ") })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 取消拆分(持久化路径,不是一句 CAS):residual_decided/reserved 可取消——落取消意图
/// → 清理本 op 的空预留项 → 解除隔离 → cancelled。segments_reassigned 之后只能前滚
/// (段落已改派,取消没有还原语义)。
#[tauri::command]
fn cancel_split(app: AppHandle, op_id: String) -> Result<(), String> {
    let root = data_root(&app).map_err(|e| e.to_string())?;
    let op_lock = split_op_lock(&op_id);
    let _op_guard = op_lock.lock().unwrap();
    let vp_store = store::VoiceprintStore::new(root.clone());
    let op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
    let now = chrono::Local::now().to_rfc3339();
    match op.phase.as_str() {
        p if p == store::split_ops::phase::RESIDUAL_DECIDED
            || p == store::split_ops::phase::RESERVED
            || p == store::split_ops::phase::CANCEL_REQUESTED => {}
        p if p == store::split_ops::phase::SEGMENTS_REASSIGNED
            || p == store::split_ops::phase::REENROLLED =>
        {
            return Err(tr!("段落已改派,只能继续完成拆分", "Segments already reassigned; finish the split instead"));
        }
        p => return Err(tr!("当前阶段不能取消: {p}", "Cannot cancel in phase {p}", p = p)),
    }
    store::split_ops::advance_guarded(
        &vp_store,
        &root,
        &op_id,
        &[
            store::split_ops::phase::RESIDUAL_DECIDED,
            store::split_ops::phase::RESERVED,
            store::split_ops::phase::CANCEL_REQUESTED,
        ],
        store::split_ops::phase::CANCEL_REQUESTED,
        &now,
    )
    .map_err(|e| e.to_string())?;
    let lc = app.state::<lifecycle::LifecycleHandle>();
    lc.request(lifecycle::machine::Msg::EditNote {
        op: lifecycle::machine::EditOp::ReleaseReservedSpeakers {
            id: op.note_id.clone(),
            op_id: op.op_id.clone(),
        },
    })?;
    // 取消拆分 ≠ 取消打标:隔离义务已在样本/残留阶段兑现,这里照常解除。
    // 推进 cancelled 与解除同一 guard(codex 实现轮二 P1③)。
    finish_and_release(
        &vp_store,
        &root,
        &op_id,
        &[store::split_ops::phase::CANCEL_REQUESTED],
        store::split_ops::phase::CANCELLED,
        &now,
    )
    .map_err(|e| e.to_string())?;
    consume_pending_rebuild(&app);
    Ok(())
}

/// 某笔记的未完成打标操作(UI 恢复入口)。纯读。
#[derive(serde::Serialize, Clone)]
struct AutoSplitHint {
    person_id: String,
    name: String,
    sim: f32,
}

#[derive(serde::Serialize, Clone)]
struct AutoSplitGroupOut {
    speaker_id: String,
    count: u32,
    dur_ms: u64,
    hint: Option<AutoSplitHint>,
}

#[derive(serde::Serialize)]
struct AutoSplitOut {
    op_id: String,
    /// false = 声纹听下来就是一个人(或全部判不准),没拆,一切已恢复原状。
    split: bool,
    groups: Vec<AutoSplitGroupOut>,
    /// 判不准、留在原说话人的段数。
    kept: u32,
}

/// 一键拆分(2026-08-22-one-click-split-design.md):普通用户入口——「这不是一个人?」。
/// 后台串既有阶段机,全部取默认:mark(隔离) → 样本自动清理(只删可归因到本篇被标簇
/// 的) → 残留「接受」 → 声纹分组 → 每组落新说话人,判不准的保持不动。只有一组时
/// 不硬拆:取消并恢复原状,如实报告。全程库零写入(去向全是新说话人,无回灌);
/// 入库只发生在用户此后亲自试听+认人时(见 assign_note_speaker_person 的 audited_seq)。
#[tauri::command]
async fn auto_split_speaker(
    app: AppHandle,
    state: State<'_, AppState>,
    note_id: String,
    speaker_id: String,
) -> Result<AutoSplitOut, String> {
    store::validate_note_id(&note_id).map_err(|e| e.to_string())?;
    reject_if_active(&state, &note_id)?;
    if app.state::<lifecycle::LifecycleHandle>().is_refining(&note_id) {
        return Err(tr!("该笔记正在 Aing 中,稍后再试", "This note is being refined; try again later"));
    }
    let root = data_root(&app).map_err(|e| e.to_string())?;
    // 断点续跑:同一说话人已有未完成 op(嵌入中途被重启杀掉是常态,实测一天两单)
    // 就接着跑,绝不另起炉灶——重复 mark 会叠出第二个 op,隔离悬置、账目成灾。
    use store::split_ops::phase as ph;
    let existing = store::split_ops::open_ops_for_note(&root, &note_id)
        .into_iter()
        .find(|o| o.speaker_ids == vec![speaker_id.clone()] && o.undone_at.is_none());
    // ① 打标(隔离/作废建议/清关联,记录 prior_links 快照)。PLAN 态的 op 由 mark
    //    自身复用推进。
    let op_id = match &existing {
        Some(o) if o.phase != ph::PLAN => o.op_id.clone(),
        _ => mark_speaker_multi(app.clone(), note_id.clone(), vec![speaker_id.clone()])?,
    };
    // ② 样本自动清理:零勾选=只删「可归因到本篇被标簇」的样本(receipt 证据),
    //    来源未知的一律保留——比旧流程让用户凭试听勾删更保守。
    let cur = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
    if cur.phase == ph::MARKED {
        confirm_multi_samples(app.clone(), op_id.clone(), Vec::new(), true)?;
    }
    // ③ 残留默认「接受」:零损失、立即可用,小偏差随后续录音按加权稀释。
    let cur = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
    if cur.phase == ph::SAMPLES_HANDLED {
        resolve_multi_residual(app.clone(), op_id.clone(), "accept".into(), true).await?;
    }
    let nroot = notes_dir(&app).map_err(|e| e.to_string())?;
    let nstore = store::NoteStore::new(nroot);
    // 已过计划期(占号/改派/回灌/释放中断):不重新分组,直接把既有计划跑到头。
    let cur = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
    if matches!(cur.phase.as_str(), p if p == ph::RESERVED || p == ph::SEGMENTS_REASSIGNED || p == ph::REENROLLED || p == ph::RELEASED)
    {
        let enroll_notes = commit_split(app.clone(), op_id.clone(), Vec::new()).await?;
        if !enroll_notes.is_empty() {
            eprintln!("auto_split({op_id}): {enroll_notes}");
        }
        let op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
        let out_groups: Vec<AutoSplitGroupOut> = op
            .plan_groups
            .iter()
            .filter(|pg| pg.dest_kind == "new_speaker")
            .filter_map(|pg| {
                pg.dest_speaker.clone().map(|sid| AutoSplitGroupOut {
                    speaker_id: sid,
                    count: pg.seqs.len() as u32,
                    dur_ms: 0,
                    hint: None,
                })
            })
            .collect();
        let kept = op
            .plan_groups
            .iter()
            .filter(|pg| pg.dest_kind == "keep")
            .map(|pg| pg.seqs.len() as u32)
            .sum();
        return Ok(AutoSplitOut { op_id, split: true, groups: out_groups, kept });
    }
    // ④ 声纹分组。
    let sug = suggest_split_groups(app.clone(), op_id.clone()).await?;
    // ⑤ 只有一组(或全是判不准):不硬拆。取消(解除隔离)并恢复本篇原状。
    if sug.groups.len() <= 1 {
        cancel_split(app.clone(), op_id.clone())?;
        let op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
        nstore
            .restore_after_unsplit(&note_id, &speaker_id, op.prior_links.get(&speaker_id).map(String::as_str))
            .map_err(|e| e.to_string())?;
        return Ok(AutoSplitOut { op_id, split: false, groups: Vec::new(), kept: 0 });
    }
    // ⑥ 提交:每组新说话人,判不准保持不动。
    let mut groups_in: Vec<SplitGroupIn> = sug
        .groups
        .iter()
        .map(|g| SplitGroupIn { seqs: g.seqs.clone(), dest_kind: "new_speaker".into(), dest_id: None })
        .collect();
    if !sug.undetermined.is_empty() {
        groups_in.push(SplitGroupIn {
            seqs: sug.undetermined.clone(),
            dest_kind: "keep".into(),
            dest_id: None,
        });
    }
    let enroll_notes = commit_split(app.clone(), op_id.clone(), groups_in).await?;
    if !enroll_notes.is_empty() {
        eprintln!("auto_split({op_id}): {enroll_notes}");
    }
    // ⑦ 读回新号,写声纹建议徽标(仅展示;split_born 已随预留项创建置位)。
    let op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
    let vp = store::VoiceprintStore::new(root.clone()).load();
    let mut hints: Vec<(String, String)> = Vec::new();
    let mut out_groups: Vec<AutoSplitGroupOut> = Vec::new();
    // plan_groups 与提交的 groups 同序(commit 按提交顺序落计划);逐一配对建议。
    for (i, pg) in op.plan_groups.iter().enumerate() {
        if pg.dest_kind != "new_speaker" {
            continue;
        }
        let Some(sid) = pg.dest_speaker.clone() else { continue };
        let hint = sug.groups.get(i).and_then(|g| g.suggested.as_ref()).map(|(pid, name, sim)| {
            let resolved = store::VoiceprintStore::resolve(&vp, pid).unwrap_or(pid).to_string();
            AutoSplitHint { person_id: resolved, name: name.clone(), sim: *sim }
        });
        if let Some(h) = &hint {
            hints.push((sid.clone(), h.person_id.clone()));
        }
        out_groups.push(AutoSplitGroupOut {
            speaker_id: sid,
            count: pg.seqs.len() as u32,
            dur_ms: sug.groups.get(i).map(|g| g.total_ms).unwrap_or(0),
            hint,
        });
    }
    nstore.set_speaker_hints(&note_id, &hints).map_err(|e| e.to_string())?;
    Ok(AutoSplitOut {
        op_id,
        split: true,
        groups: out_groups,
        kept: sug.undetermined.len() as u32,
    })
}

/// 一键拆分的撤销(纯笔记级——自动流对声纹库零写入,可安全逆转;唯一不还原的是
/// 已删的「可归因本篇」样本,它们本就是被污染的):段落原路搬回 → 空的新说话人
/// 删除 → 多人标记复位 → 原人物关联恢复 → 修订稿反向同步。段落被后续编辑动过
/// 则拒绝(CAS 兜底)。幂等:已撤销过的 op 直接拒。
#[tauri::command]
fn undo_auto_split(app: AppHandle, state: State<AppState>, op_id: String) -> Result<(), String> {
    let root = data_root(&app).map_err(|e| e.to_string())?;
    let op_lock = split_op_lock(&op_id);
    let _op_guard = op_lock.lock().unwrap();
    let op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
    if op.phase != store::split_ops::phase::DONE {
        return Err(tr!("该拆分未完成,不能撤销: {p}", "Split not finished; cannot undo: {p}", p = &op.phase));
    }
    if op.undone_at.is_some() {
        return Err(tr!("该拆分已撤销过", "This split was already undone"));
    }
    reject_if_active(&state, &op.note_id)?;
    if app.state::<lifecycle::LifecycleHandle>().is_refining(&op.note_id) {
        return Err(tr!("该笔记正在 Aing 中,稍后再试", "This note is being refined; try again later"));
    }
    let nroot = notes_dir(&app).map_err(|e| e.to_string())?;
    let nstore = store::NoteStore::new(nroot.clone());
    let dir = nroot.join(&op.note_id);
    // 反向搬运表:seq 现在必须仍在拆分去向上(CAS),搬回计划定稿时的原说话人。
    let mut back_moves: Vec<(u64, String, String)> = Vec::new();
    let mut created_sids: std::collections::BTreeSet<String> = Default::default();
    for pg in &op.plan_groups {
        let Some(dest) = &pg.dest_speaker else { continue };
        if pg.dest_kind == "new_speaker" {
            created_sids.insert(dest.clone());
        }
        for (q, orig) in pg.seqs.iter().zip(&pg.expected_speakers) {
            back_moves.push((*q, dest.clone(), orig.clone()));
        }
    }
    if !back_moves.is_empty() {
        nstore
            .batch_set_segment_speaker(&op.note_id, &back_moves, &op.op_id)
            .map_err(|e| {
                tr!(
                    "段落已被后续编辑改动,无法撤销: {e}",
                    "Segments were edited after the split; cannot undo: {e}",
                    e = e
                )
            })?;
    }
    // 空的新说话人删除(段已搬回,必空;删除失败不阻塞其余恢复,如实记 stderr)。
    for sid in &created_sids {
        if let Err(e) = nstore.delete_speaker(&op.note_id, sid) {
            eprintln!("undo_auto_split({op_id}): 删除新说话人 {sid} 失败(忽略): {e}");
        }
    }
    // 多人标记复位 + 原关联恢复(仅本篇表项,不触库)。
    for sid in &op.speaker_ids {
        nstore
            .restore_after_unsplit(&op.note_id, sid, op.prior_links.get(sid).map(String::as_str))
            .map_err(|e| e.to_string())?;
    }
    // 修订稿反向同步:整段同去向原位改回;跨组标 stale(与正向同一口径)。
    let moved_back: std::collections::BTreeMap<u64, String> =
        back_moves.iter().map(|(q, _, back)| (*q, back.clone())).collect();
    if !moved_back.is_empty() && store::aing_exists(&dir) {
        if let Err(e) = store::sync_refined_after_split(&dir, &moved_back) {
            if let Err(e2) = store::mark_refined_stale(&dir) {
                return Err(tr!(
                    "撤销已生效,但修订稿同步失败且无法标记过期: {e} / {e2}",
                    "Undo applied, but refined sync failed and stale-marking failed: {e} / {e2}",
                    e = e,
                    e2 = e2
                ));
            }
        }
    }
    // 落撤销标记(幂等闸)。
    let mut op2 = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
    op2.undone_at = Some(chrono::Local::now().to_rfc3339());
    op2.updated_at = op2.undone_at.clone().unwrap();
    store::split_ops::save(&root, &op2).map_err(|e| e.to_string())?;
    Ok(())
}

/// 最近一次可撤销的拆分(结果横幅关掉后的撤销入口)。纯读。
#[tauri::command]
fn latest_undoable_split(
    app: AppHandle,
    note_id: String,
) -> Result<Option<store::split_ops::SplitOp>, String> {
    store::validate_note_id(&note_id).map_err(|e| e.to_string())?;
    let root = data_root(&app).map_err(|e| e.to_string())?;
    Ok(store::split_ops::latest_undoable_for_note(&root, &note_id))
}

/// 场景判定结果(2026-08-23 一期):笔记页信息级提示用。纯读。
#[tauri::command]
fn get_scene(app: AppHandle, note_id: String) -> Result<Option<scene::SceneDoc>, String> {
    store::validate_note_id(&note_id).map_err(|e| e.to_string())?;
    let dir = notes_dir(&app).map_err(|e| e.to_string())?.join(&note_id);
    Ok(scene::load(&dir))
}

#[tauri::command]
fn list_split_ops(app: AppHandle, note_id: String) -> Result<Vec<store::split_ops::SplitOp>, String> {
    store::validate_note_id(&note_id).map_err(|e| e.to_string())?;
    let root = data_root(&app).map_err(|e| e.to_string())?;
    Ok(store::split_ops::open_ops_for_note(&root, &note_id))
}

/// 按当前选型重建声纹库:拿每个人存下的录音样本用新模型重新算质心,并把库标签
/// 改写成新选型。**这是模型切换之后声纹识别能恢复的唯一途径**。
///
/// 成功即写标签(`rebuild_for_model` 末尾无条件写),所以"库标签是否等于当前选型"
/// 就是"重建有没有成功过"的判据——也正是 heal_voiceprint_model_mismatch 的依据。
/// 同一时刻只允许一个重建在跑。**必须有**:启动自愈、设置切换、手动重建三个入口
/// 都能触发,并发跑两个的话,两条线程各拿各的模型嵌同一批样本、各自写库与写标签,
/// 最后谁后写谁赢——库标签与向量空间可能来自不同的两次运行(codex review 二轮 P1#1)。
static REBUILD_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 重建期间又来了新请求。**不能直接丢**:丢掉的往往正是最新那次切换,结果库停在
/// 旧空间、门禁继续关着,而用户以为自己已经切过去了(codex review 二轮 P1#1)。
/// 记一笔,当前这轮跑完再跑一轮(新一轮自己重新读设置,自然收敛到最新选型)。
static REBUILD_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 当前选型快照。重建全程只读这一份,不再各处重读设置。
fn current_speaker_model(app: &AppHandle) -> String {
    app.path()
        .app_data_dir()
        .map(|d| settings::load(&d).speaker_model)
        .unwrap_or_default()
}

/// 重建请求的落盘标记:REBUILD_PENDING/发起中的重建都只在内存,进程在重建线程
/// 完成前退出即丢——而"质心清空型"重建(纠错还原/拆回/退回基线)不改库标签,
/// 启动自愈永远兜不住,那些人就永久空质心(codex 混杂实现轮六 P1)。
/// 语义:有未完成的重建诉求 → 标记在;一轮重建成功跑完 → 清掉。启动时见标记补跑。
fn rebuild_marker_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    data_root(app).ok().map(|r| r.join("rebuild_pending.flag"))
}

fn set_rebuild_marker(app: &AppHandle, reason: &str) {
    if let Some(p) = rebuild_marker_path(app) {
        if let Err(e) = std::fs::write(&p, reason) {
            eprintln!("重建标记写入失败(进程退出前完不成的重建将无人补跑): {e}");
        }
    }
}

fn clear_rebuild_marker(app: &AppHandle) {
    if let Some(p) = rebuild_marker_path(app) {
        let _ = std::fs::remove_file(p);
    }
}

/// 重建调度的控制互斥:RUNNING/PENDING 的交接与落盘标记的写/清必须在同一把锁下。
/// 没有它,旧 runner 在"清 RUNNING → 查 PENDING → 清标记"的窗口里,会把**新请求
/// 刚写下的标记**当成自己的诉求删掉——进程随后退出,轮六要修的丢失原样回来
/// (codex 混杂实现轮七 P1)。锁内只做标志与文件操作,微秒级,不持锁做重活。
static REBUILD_CTL: Mutex<()> = Mutex::new(());

fn spawn_voiceprint_rebuild(
    app: &AppHandle,
    cache: std::sync::Arc<Mutex<Option<Box<diar::TaggedEmbedder>>>>,
    reason: &'static str,
) {
    use std::sync::atomic::Ordering;
    {
        let _ctl = REBUILD_CTL.lock().unwrap();
        // 先落盘再排队/起线程:反过来的话"内存标志置了、标记没写、进程退出"仍丢请求。
        set_rebuild_marker(app, reason);
        if REBUILD_RUNNING.swap(true, Ordering::SeqCst) {
            REBUILD_PENDING.store(true, Ordering::SeqCst);
            eprintln!("声纹库重建已在进行中,本次({reason})排队等当前这轮跑完");
            return;
        }
    }
    let app2 = app.clone();
    let cache2 = cache.clone();
    std::thread::spawn(move || {
        // RAII 复位**只兜 panic**(armed 模式):正常路径的 RUNNING 交接必须在
        // REBUILD_CTL 内完成。此前 drop 无条件在锁外清 RUNNING,新请求可在
        // "drop 清完 → 本线程进锁"之间成为新 runner,本线程随后在锁内再清一次,
        // 把新 runner 的单飞标志掀了——重建并发跑、新标记被误删(codex 轮八/九 P1)。
        struct Reset {
            armed: bool,
        }
        impl Drop for Reset {
            fn drop(&mut self) {
                if self.armed {
                    let _ctl = REBUILD_CTL.lock().unwrap_or_else(|e| e.into_inner());
                    REBUILD_RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
        let mut reset = Reset { armed: true };
        let ok = rebuild_once(&app2, cache, reason);
        // 收尾:**只因"期间来过新请求"重跑**,不看库标签是否仍不一致。
        // 曾经也检查过标签(想顺手兜住"跑完发现又被切了"),但那会在永久失败时变成
        // 无界循环:模型文件缺失/保存失败 → 标签永远对不上 → 每轮结束立刻再起一轮,
        // 无退避、无上限,日志与遥测一起刷屏(codex review 三轮 P1#1)。
        // 真正的切换必然经过 set_settings,那条路会把 PENDING 置上,不会漏。
        // 剩下的"卡住"情形由下次启动的自愈兜——一次启动最多跑一轮,天然有界。
        //
        // 决策在 CTL 内做(标志交接与清标记原子),重跑动作在 CTL 外发
        // (spawn_voiceprint_rebuild 自己要取 CTL,不可重入)。
        let respawn = {
            let _ctl = REBUILD_CTL.lock().unwrap();
            reset.armed = false; // 正常路径:交接在此完成,panic 兜底解除武装
            REBUILD_RUNNING.store(false, Ordering::SeqCst);
            if REBUILD_PENDING.swap(false, Ordering::SeqCst) {
                true
            } else {
                if ok {
                    // 成功且无排队:诉求已兑现,清落盘标记。失败留标记,下次启动补跑。
                    clear_rebuild_marker(&app2);
                }
                false
            }
        };
        if respawn {
            spawn_voiceprint_rebuild(&app2, cache2, "有排队请求,重跑");
        }
    });
}

/// 跑一轮重建。返回是否成功(落盘标记只在成功后清,失败留给下次启动补跑)。
fn rebuild_once(
    app2: &AppHandle,
    cache: std::sync::Arc<Mutex<Option<Box<diar::TaggedEmbedder>>>>,
    reason: &'static str,
) -> bool {
    {
        // 一次快照定死"标签"与"权重路径",两者必须同源。
        let tag = current_speaker_model(app2);
        if tag.is_empty() {
            return false;
        }
        match diar::SherpaEmbedder::new(&speaker_model_path_for(&tag)) {
            Ok(mut e) => {
                // 加载模型可能耗时;这中间用户完全可能又切了一次。此时这份嵌入器
                // 已经不是当前选型,写库与入常驻槽都会把错的东西留下来。
                if current_speaker_model(&app2) != tag {
                    eprintln!("声纹库重建({reason})中途选型已变,放弃本次结果");
                    return false;
                }
                let ok = match data_root(&app2).map(store::VoiceprintStore::new) {
                    Ok(vps) => match vps.rebuild_for_model(&tag, &mut e) {
                        Ok(n) => {
                            eprintln!("声纹库已按 {tag} 重建({reason};{n} 人有样本可建)");
                            true
                        }
                        Err(err) => {
                            eprintln!("声纹库重建失败(种子注入将持续跳过): {err}");
                            false
                        }
                    },
                    Err(err) => {
                        eprintln!("声纹库路径不可用,未重建: {err}");
                        false
                    }
                };
                // 嵌入很慢(实测约半分钟),再核一次才敢占常驻槽:塞错模型的嵌入器
                // 进去,下一场录制整场用错空间嵌入。
                if current_speaker_model(&app2) == tag {
                    // 标签就是这次重建用的 tag,与权重路径同源(见 rebuild_once 开头)。
                    stash_model(&cache, Some(Box::new(diar::TaggedEmbedder::new(&tag, Box::new(e)))));
                }
                return ok;
            }
            Err(err) => {
                eprintln!("声纹模型加载失败(模型未下载?),库未重建、录制不自动认人: {err}");
                telemetry::report_error(
                    telemetry::ErrorKind::ModelLoad,
                    // 断句是有讲究的:脱敏规则会整段丢弃连续 12 个以上中日韩字符,
                    // "换模型后声纹模型加载失败" 正好 12 个,连写就会被脱成 <TEXT>。
                    &format!("{reason}后,声纹模型加载失败,库未重建: {err}"),
                );
            }
        }
    }
    false
}

/// 启动自愈:库标签与当前选型不一致就主动重建一次。
///
/// **为什么必须有这一步**:重建原先只挂在「设置里改动模型的那一瞬间」起的一次性
/// 线程上。那次没跑成(应用当场被关、线程死了、或者用户直接改了 settings.json),
/// 就再也没有第二次机会——门禁从此永久关闭:开录不注入种子、指认不回灌、
/// identify 的声学证据全程为假,而每次启动只是如实记一行「重建完成后恢复」然后
/// 什么也不做。实测一台机器这样连续降级了 149 次启动、一个多月(2026-08-19 定位)。
///
/// 录制中跳过:重建要现场加载嵌入器逐条嵌入样本,和录制抢 ORT 线程与 CPU;
/// 这是自愈不是急救,等下次启动无妨。
fn heal_voiceprint_model_mismatch(app: &AppHandle, state: &AppState) -> bool {
    let Ok(root) = data_root(app) else { return false };
    let want = app
        .path()
        .app_data_dir()
        .map(|d| settings::load(&d).speaker_model)
        .unwrap_or_default();
    if want.is_empty() {
        return false;
    }
    let have = store::VoiceprintStore::new(root).load().embedding_model.clone();
    if have == want {
        return false;
    }
    if state.session.lock().map(|s| s.is_some()).unwrap_or(true) {
        eprintln!("声纹库标签({have})与当前选型({want})不一致,录制中暂不重建,下次启动再试");
        return false;
    }
    eprintln!("声纹库标签({have})与当前选型({want})不一致,启动自愈:开始重建");
    spawn_voiceprint_rebuild(app, state.embedder_cache.clone(), "启动自愈");
    true
}

/// 回灌互斥门:同一时刻最多一个回灌任务在嵌入。不借用 AppState.embedder_cache
/// (开录会 take 走它,回灌不能卡住开录、也不能被开录饿死),自建临时嵌入器,
/// 靠此门保证回灌侧 ORT 并发最多 +1;它同时把 track_pcm 的 m4a 临时文件竞争
/// 收敛到回灌之间自串行。
static FEEDBACK_GATE: Mutex<()> = Mutex::new(());

/// 指认成功后的纠错回灌(spec P1-2):后台 best-effort,任何失败只留日志,
/// 绝不影响指认结果。分派逻辑见 feedback::plan_action(纯函数已单测);
/// 无名先前人物走 journaled 合并(可撤销),其余走段重嵌入回灌。
fn spawn_feedback(
    app: &AppHandle,
    note_id: String,
    segs: Vec<store::SegmentRecord>,
    filter: feedback::SegFilter,
    prior: Option<(String, String)>,
    target: String,
    // 提交前复核用的原始稿说话人 id(修订稿路径传 None)。
    // **必须在真正写库之前再查一次**:关联与取消关联各起一个后台任务,
    // FEEDBACK_GATE 只保证互斥、不保证顺序——撤销先跑就会拿到 NoEntry,
    // 随后这个回灌照样落库,人物明明已经解除关联,增量却留在库里
    // (codex review 二轮 P1#3)。
    verify_speaker: Option<String>,
) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // 声明在 run 之外:纠错还原一旦清空了旧人物的质心,这件事就已经落盘了,
        // 之后 run 无论返回 Ok 还是 Err 都必须补一次重建,否则那个人永远没有声纹
        // (codex review 实现轮二 P1)。
        let mut needs_rebuild = false;
        let run = |needs_rebuild: &mut bool| -> anyhow::Result<()> {
            let vp = open_voiceprint_store(&app).map_err(anyhow::Error::msg)?;
            let now = chrono::Local::now().to_rfc3339();
            let action =
                feedback::plan_action(prior.as_ref().map(|(i, n)| (i.as_str(), n.as_str())), &target);
            // 门要先拿,复核要在门内做:门只保证互斥、不保证顺序,不在门内复核等于没核。
            // **复核覆盖所有分支**——MergePrior 会把一整个人物并进目标,是比回灌更重的
            // 库级写入,且明确不由取消关联撤销(codex review 二轮 P1#2)。
            let _gate = FEEDBACK_GATE.lock().unwrap();
            if let Some(sid) = &verify_speaker {
                let still_linked = notes_dir(&app)
                    .ok()
                    .and_then(|d| store::NoteStore::new(d).load(&note_id).ok())
                    .and_then(|n| n.speakers.get(sid).and_then(|m| m.person_id.clone()))
                    .is_some_and(|pid| pid == target);
                if !still_linked {
                    eprintln!("feedback: note={note_id} {sid} 已不再关联 {target},跳过本次回灌/合并");
                    return Ok(());
                }
            }
            match action {
                feedback::FeedbackAction::Noop => Ok(()),
                feedback::FeedbackAction::MergePrior { prior } => {
                    // 不带嵌入器:并的是库里已有的质心,本来就同空间,传库当前标签。
                    let lib_model = vp.load().embedding_model.clone();
                    let receipt =
                        vp.merge_journaled(&prior, &target, None, "feedback-assign", None, &now, &lib_model)?;
                    eprintln!("feedback: 无名先前人物 {prior} 已并入 {target}(回执 {receipt})");
                    Ok(())
                }
                feedback::FeedbackAction::Reinforce => {
                    let note_dir = notes_dir(&app)?.join(&note_id);
                    let expected = app
                        .path()
                        .app_data_dir()
                        .map(|d| settings::load(&d).speaker_model)
                        .unwrap_or_default();
                    let library_model = vp.load().embedding_model.clone();
                    // **标签与权重必须来自同一份快照**:用 speaker_model_path(&app) 会
                    // 再读一次设置,切换发生在两次读取之间时,就会用 B 的权重算、以 A 的
                    // 标签写库,而库若仍是 A,门禁会错误放行(codex review 实现轮 P1)。
                    let mut embedder = diar::SherpaEmbedder::new(&speaker_model_path_for(&expected))?;
                    let r = feedback::reinforce_person(
                        &note_dir,
                        &segs,
                        &filter,
                        &target,
                        &vp,
                        &library_model,
                        &expected,
                        &mut embedder,
                        &now,
                        None,
                        needs_rebuild,
                        false,
                    )?;
                    eprintln!("feedback: note={note_id} target={target} result={r:?}");
                    Ok(())
                }
            }
        };
        let outcome = run(&mut needs_rebuild);
        // 先无条件处理重建,再看回灌结果——顺序不能反,run 出错时也要重建。
        if needs_rebuild {
            eprintln!("feedback: 纠错还原清空了旧人物质心,排一次重建 note={note_id}");
            let state = app.state::<AppState>();
            spawn_voiceprint_rebuild(&app, state.embedder_cache.clone(), "纠错还原清空质心");
        }
        if let Err(e) = outcome {
            eprintln!("feedback: 回灌失败(不影响指认) note={note_id}: {e}");
        }
    });
}

/// 某声纹库人物出现过的会议（详情页「出现过的会议」卡）：扫各笔记 speakers.json 的
/// person_id，经 redirects 归一后比对（笔记里可能还留着已被合并的 loser 引用）。
/// 按开始时间倒序。纯读，损坏/缺失的 speakers.json 静默跳过。
#[tauri::command]
fn person_notes(app: AppHandle, person_id: String) -> Result<Vec<store::NoteSummary>, String> {
    let vp = open_voiceprint_store(&app)?.load();
    let target = store::VoiceprintStore::resolve(&vp, &person_id)
        .map(str::to_string)
        .ok_or_else(|| {
            tr!(
                "声纹库中没有该人物: {person_id}",
                "No such person in the voiceprint library: {person_id}"
            )
        })?;
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    let notes = store::NoteStore::new(dir.clone()).list(); // list 已按开始时间倒序
    Ok(notes
        .into_iter()
        .filter(|n| {
            let Ok(text) = std::fs::read_to_string(dir.join(&n.id).join("speakers.json")) else {
                return false;
            };
            let Ok(map) = serde_json::from_str::<std::collections::BTreeMap<String, store::SpeakerMeta>>(&text) else {
                return false;
            };
            map.values().any(|m| {
                m.person_id
                    .as_deref()
                    .and_then(|pid| store::VoiceprintStore::resolve(&vp, pid))
                    .map(|r| r == target)
                    .unwrap_or(false)
            })
        })
        .collect())
}

/// 相关笔记:与该笔记共享 Aing 实体的其他笔记(经知识图谱),按共享实体数降序。
/// 纯增值:图谱缺失/查询失败 → 返回空列表(前端据此隐藏该区块),绝不 Err 拖垮详情页。
#[tauri::command]
fn note_related(app: AppHandle, id: String) -> Result<Vec<ipc::RelatedNote>, String> {
    store::validate_note_id(&id).map_err(|e| e.to_string())?;
    let Ok(root) = data_root(&app) else { return Ok(vec![]) };
    let pairs = match graph::related_notes(&root, &id) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("note_related: 图谱查询失败,返回空: {e}");
            return Ok(vec![]);
        }
    };
    if pairs.is_empty() {
        return Ok(vec![]);
    }
    let notes_root = notes_dir(&app).map_err(|e| e.to_string())?;
    let summaries = store::NoteStore::new(notes_root).list();
    let by_id: std::collections::HashMap<String, &store::NoteSummary> =
        summaries.iter().map(|n| (n.id.clone(), n)).collect();
    let out = pairs
        .into_iter()
        .filter_map(|(nid, shared)| {
            by_id.get(&nid).map(|n| ipc::RelatedNote {
                id: n.id.clone(),
                title: n.title.clone(),
                started_at: n.started_at.clone(),
                shared_entities: shared,
            })
        })
        .collect();
    Ok(out)
}

/// 图谱全部实体(列表视图),按出现笔记数降序。图谱失败/空 → 空列表,不 Err。
fn graph_read_error(command: &str, error: anyhow::Error) -> String {
    eprintln!("{command}: semantic graph read failed: {error:#}");
    tr!(
        "知识图谱暂时不可用，请稍后重试",
        "The knowledge graph is temporarily unavailable; please try again later"
    )
}

fn graph_mutation_error(command: &str, error: anyhow::Error) -> String {
    eprintln!("{command}: knowledge mutation failed: {error:#}");
    tr!(
        "无法保存知识整理操作；请确认目标仍存在且整理记录完好",
        "Could not save the knowledge edit; make sure the target still exists and the edit history is intact"
    )
}

fn mark_knowledge_rebuild_queued(
    mut result: ipc::KnowledgeMutationResult,
    scheduled: anyhow::Result<u64>,
) -> Result<ipc::KnowledgeMutationResult, String> {
    let generation = scheduled.map_err(|error| {
        eprintln!("knowledge mutation committed but graph rebuild scheduling failed: {error:#}");
        tr!(
            "知识整理操作已保存，但索引排队失败；应用将在下次启动或整理时自动重试",
            "The knowledge edit was saved, but indexing could not be queued; the app will retry automatically on next launch or edit"
        )
    })?;
    result.rebuild_state = "queued".into();
    result.rebuild_generation = Some(generation);
    Ok(result)
}

fn queue_knowledge_rebuild(
    app: &AppHandle,
    root: PathBuf,
    result: ipc::KnowledgeMutationResult,
) -> Result<ipc::KnowledgeMutationResult, String> {
    let graph_events = app.clone();
    let scheduled = app
        .state::<AppState>()
        .graph_scheduler
        .request(root, move |status| {
            let _ = graph_events.emit("graph_index_status", status);
        });
    mark_knowledge_rebuild_queued(result, scheduled)
}

fn mark_person_graph_rebuild_queued(
    action: &str,
    scheduled: anyhow::Result<u64>,
) -> Result<(), String> {
    if let Err(error) = scheduled {
        eprintln!("{action} committed but graph rebuild scheduling failed: {error:#}");
        return Err(tr!(
            "{action}已保存，但索引待重试；应用将在下次启动或整理时自动重试",
            "{action} was saved, but indexing is pending retry; the app will retry automatically on next launch or edit"
        ));
    }
    Ok(())
}

fn queue_person_graph_rebuild_with(
    scheduler: &graph::index::RebuildScheduler,
    root: PathBuf,
    action: &str,
    emit: impl Fn(graph::index::IndexStatus) + Send + Sync + 'static,
) -> Result<(), String> {
    mark_person_graph_rebuild_queued(action, scheduler.request(root, emit))
}

fn queue_person_graph_rebuild(
    app: &AppHandle,
    root: PathBuf,
    action: &str,
) -> Result<(), String> {
    let graph_events = app.clone();
    queue_person_graph_rebuild_with(
        &app.state::<AppState>().graph_scheduler,
        root,
        action,
        move |status| {
            let _ = graph_events.emit("graph_index_status", status);
        },
    )
}

#[tauri::command]
fn semantic_graph(
    app: AppHandle,
    filter: graph::query::GraphFilter,
) -> Result<ipc::SemanticGraphData, String> {
    let root = data_root(&app).map_err(|error| graph_read_error("semantic_graph", error))?;
    graph::query::semantic_graph(&root, &filter)
        .map_err(|error| graph_read_error("semantic_graph", error))
}

#[tauri::command]
fn semantic_entity_detail(
    app: AppHandle,
    entity_id: String,
    filter: graph::query::GraphFilter,
) -> Result<Option<ipc::SemanticEntityDetail>, String> {
    let root =
        data_root(&app).map_err(|error| graph_read_error("semantic_entity_detail", error))?;
    graph::query::semantic_entity_detail(&root, &entity_id, &filter)
        .map_err(|error| graph_read_error("semantic_entity_detail", error))
}

#[tauri::command]
fn relation_detail(
    app: AppHandle,
    relation_id: String,
) -> Result<Option<ipc::RelationDetail>, String> {
    let root = data_root(&app).map_err(|error| graph_read_error("relation_detail", error))?;
    graph::query::relation_detail(&root, &relation_id)
        .map_err(|error| graph_read_error("relation_detail", error))
}

#[tauri::command]
fn pending_review(
    app: AppHandle,
    filter: graph::query::GraphFilter,
) -> Result<Vec<ipc::PendingReviewItem>, String> {
    let root = data_root(&app).map_err(|error| graph_read_error("pending_review", error))?;
    graph::query::pending_review(&root, &filter)
        .map_err(|error| graph_read_error("pending_review", error))
}

#[tauri::command]
fn entity_mentions(app: AppHandle, entity_id: String) -> Result<Vec<ipc::MentionEvidence>, String> {
    let root = data_root(&app).map_err(|error| graph_read_error("entity_mentions", error))?;
    graph::query::entity_mentions(&root, &entity_id)
        .map_err(|error| graph_read_error("entity_mentions", error))
}

#[tauri::command]
fn shortest_path(
    app: AppHandle,
    start: String,
    end: String,
    filter: graph::query::GraphFilter,
) -> Result<Option<ipc::KnowledgePath>, String> {
    let root = data_root(&app).map_err(|error| graph_read_error("shortest_path", error))?;
    graph::path::shortest_path(&root, &start, &end, &filter)
        .map_err(|error| graph_read_error("shortest_path", error))
}

#[tauri::command]
fn apply_knowledge_operation(
    app: AppHandle,
    operation: ipc::KnowledgeOperationInput,
) -> Result<ipc::KnowledgeMutationResult, String> {
    let root = data_root(&app)
        .map_err(|error| graph_mutation_error("apply_knowledge_operation", error))?;
    let result = graph::query::apply_operation(&root, &operation)
        .map_err(|error| graph_mutation_error("apply_knowledge_operation", error))?;
    // `overrides::update` has returned here, so its cross-process ledger lock is released before
    // the scheduler can sample or rebuild.
    queue_knowledge_rebuild(&app, root, result)
}

#[tauri::command]
fn split_entity(
    app: AppHandle,
    request: ipc::SplitEntityRequest,
) -> Result<ipc::KnowledgeMutationResult, String> {
    let root = data_root(&app).map_err(|error| graph_mutation_error("split_entity", error))?;
    let result = graph::query::split_operation(&root, &request)
        .map_err(|error| graph_mutation_error("split_entity", error))?;
    queue_knowledge_rebuild(&app, root, result)
}

#[tauri::command]
fn merge_entities(
    app: AppHandle,
    source_id: String,
    target_id: String,
) -> Result<ipc::KnowledgeMutationResult, String> {
    let root = data_root(&app).map_err(|error| graph_mutation_error("merge_entities", error))?;
    let result = graph::query::merge_operation(&root, &source_id, &target_id)
        .map_err(|error| graph_mutation_error("merge_entities", error))?;
    queue_knowledge_rebuild(&app, root, result)
}

#[tauri::command]
fn undo_knowledge_operation(
    app: AppHandle,
    operation_id: String,
) -> Result<ipc::KnowledgeMutationResult, String> {
    let root = data_root(&app)
        .map_err(|error| graph_mutation_error("undo_knowledge_operation", error))?;
    let result = graph::query::undo_operation(&root, &operation_id)
        .map_err(|error| graph_mutation_error("undo_knowledge_operation", error))?;
    queue_knowledge_rebuild(&app, root, result)
}

#[tauri::command]
fn graph_entities(app: AppHandle) -> Result<Vec<ipc::EntitySummary>, String> {
    let Ok(root) = data_root(&app) else { return Ok(vec![]) };
    let rows = match graph::list_entities(&root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("graph_entities: 查询失败,返回空: {e}");
            return Ok(vec![]);
        }
    };
    Ok(rows
        .into_iter()
        .map(|r| ipc::EntitySummary {
            id: r.id,
            kind: r.kind,
            name: r.name,
            aliases: r.aliases,
            is_person: r.is_person,
            note_count: r.note_count,
            mention_total: r.mention_total,
        })
        .collect())
}

/// 力导图数据:节点(全部实体)+ 共现边。任一子查询失败 → 该部分空,整体不 Err。
#[tauri::command]
fn graph_data(app: AppHandle) -> Result<ipc::GraphData, String> {
    let Ok(root) = data_root(&app) else { return Ok(ipc::GraphData { nodes: vec![], edges: vec![] }) };
    let nodes = graph::list_entities(&root)
        .unwrap_or_else(|e| {
            eprintln!("graph_data: 实体查询失败,返回空: {e}");
            vec![]
        })
        .into_iter()
        .map(|r| ipc::EntitySummary {
            id: r.id,
            kind: r.kind,
            name: r.name,
            aliases: r.aliases,
            is_person: r.is_person,
            note_count: r.note_count,
            mention_total: r.mention_total,
        })
        .collect();
    let edges = graph::cooccurrence_edges(&root)
        .unwrap_or_else(|e| {
            eprintln!("graph_data: 共现边查询失败,返回空: {e}");
            vec![]
        })
        .into_iter()
        .map(|(a, b, weight)| ipc::EdgeRow { a, b, weight })
        .collect();
    Ok(ipc::GraphData { nodes, edges })
}

/// 文章视角力导图:节点=笔记(name=标题,note_count 字段复用为「该笔记含的实体数」
/// 驱动节点大小),边=两篇笔记共享的不同实体数。实体视角(graph_data)的对偶。
/// 任一子查询失败 → 该部分空,整体不 Err。没标题的笔记(已删/找不到)跳过。
#[tauri::command]
fn note_graph_data(app: AppHandle) -> Result<ipc::GraphData, String> {
    let Ok(root) = data_root(&app) else { return Ok(ipc::GraphData { nodes: vec![], edges: vec![] }) };
    let raw_nodes = graph::note_nodes(&root).unwrap_or_else(|e| {
        eprintln!("note_graph_data: 笔记节点查询失败,返回空: {e}");
        vec![]
    });
    let Ok(notes_root) = notes_dir(&app) else { return Ok(ipc::GraphData { nodes: vec![], edges: vec![] }) };
    let summaries = store::NoteStore::new(notes_root).list();
    let by_id: std::collections::HashMap<String, &store::NoteSummary> =
        summaries.iter().map(|n| (n.id.clone(), n)).collect();
    let nodes: Vec<ipc::EntitySummary> = raw_nodes
        .into_iter()
        .filter_map(|(nid, ecount, mtotal)| {
            by_id.get(&nid).map(|n| ipc::EntitySummary {
                id: n.id.clone(),
                kind: "note".into(),
                name: n.title.clone(),
                aliases: vec![],
                is_person: false,
                note_count: ecount,   // 复用为节点大小信号(该笔记含的实体数)
                mention_total: mtotal,
            })
        })
        .collect();
    // 边两端都得是有标题的笔记(跳过被过滤掉的节点),否则力导图会有指向不存在节点的悬空边。
    let live: std::collections::HashSet<String> = nodes.iter().map(|n| n.id.clone()).collect();
    let edges = graph::note_shared_edges(&root)
        .unwrap_or_else(|e| {
            eprintln!("note_graph_data: 笔记共享边查询失败,返回空: {e}");
            vec![]
        })
        .into_iter()
        .filter(|(a, b, _)| live.contains(a) && live.contains(b))
        .map(|(a, b, weight)| ipc::EdgeRow { a, b, weight })
        .collect();
    Ok(ipc::GraphData { nodes, edges })
}

/// 点击图谱共现边时按需读取连接原因。perspective=note 返回共用实体；
/// perspective=entity 返回同时提到两个实体的笔记标题。
#[tauri::command]
fn graph_edge_detail(
    app: AppHandle,
    a: String,
    b: String,
    perspective: String,
) -> Result<ipc::GraphEdgeDetail, String> {
    let root = data_root(&app).map_err(|error| graph_read_error("graph_edge_detail", error))?;
    let items = match perspective.as_str() {
        "note" => graph::shared_entities_for_notes(&root, &a, &b)
            .map_err(|error| graph_read_error("graph_edge_detail", error))?
            .into_iter()
            .map(|(id, kind, name)| ipc::GraphEdgeDetailItem {
                id,
                name,
                kind: Some(kind),
                started_at: None,
                duration_secs: None,
            })
            .collect(),
        "entity" => {
            let semantic_note_ids = graph::query::shared_notes_for_entities(&root, &a, &b)
                .unwrap_or_default();
            let note_ids = if semantic_note_ids.is_empty() {
                graph::shared_notes_for_entities(&root, &a, &b)
                    .map_err(|error| graph_read_error("graph_edge_detail", error))?
            } else {
                semantic_note_ids
            };
            let by_id: std::collections::HashMap<String, store::NoteSummary> = notes_dir(&app)
                .map(|notes_root| {
                    store::NoteStore::new(notes_root)
                        .list()
                        .into_iter()
                        .map(|note| (note.id.clone(), note))
                        .collect()
                })
                .unwrap_or_default();
            note_ids
                .into_iter()
                .filter_map(|id| {
                    by_id.get(&id).map(|note| ipc::GraphEdgeDetailItem {
                        id: note.id.clone(),
                        name: note.title.clone(),
                        kind: None,
                        started_at: Some(note.started_at.clone()),
                        duration_secs: note.duration_secs,
                    })
                })
                .collect()
        }
        _ => return Err(tr!("未知的图谱视角", "Unknown graph view")),
    };
    Ok(ipc::GraphEdgeDetail { items })
}

/// 单个实体详情(右侧面板)。实体不存在/图谱失败 → None,不 Err。
#[tauri::command]
fn entity_detail(app: AppHandle, id: String) -> Result<Option<ipc::EntityDetail>, String> {
    let Ok(root) = data_root(&app) else { return Ok(None) };
    let detail = match graph::entity_detail(&root, &id) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("entity_detail: 查询失败,返回 None: {e}");
            return Ok(None);
        }
    };
    let Some(d) = detail else { return Ok(None) };
    // 联查笔记标题(NoteStore.list);查不到标题的笔记跳过
    let by_id: std::collections::HashMap<String, store::NoteSummary> = match notes_dir(&app) {
        Ok(nr) => store::NoteStore::new(nr).list().into_iter().map(|n| (n.id.clone(), n)).collect(),
        Err(_) => std::collections::HashMap::new(),
    };
    let notes = d
        .notes
        .into_iter()
        .filter_map(|(nid, cnt)| {
            by_id.get(&nid).map(|n| ipc::EntityNoteRef {
                id: n.id.clone(),
                title: n.title.clone(),
                started_at: n.started_at.clone(),
                mention_count: cnt,
            })
        })
        .collect();
    let related = d
        .related
        .into_iter()
        .map(|r| ipc::RelatedEntity { id: r.id, kind: r.kind, name: r.name, shared_notes: r.shared_notes })
        .collect();
    Ok(Some(ipc::EntityDetail {
        id: d.row.id,
        kind: d.row.kind,
        name: d.row.name,
        aliases: d.row.aliases,
        is_person: d.row.is_person,
        note_count: d.row.note_count,
        mention_total: d.row.mention_total,
        notes,
        related,
    }))
}

/// 笔记页高亮点击导航:该笔记局部实体 → 全局 id(+是否人)。失败/无实体 → 空。
#[tauri::command]
fn note_entity_links(app: AppHandle, id: String) -> Result<Vec<ipc::EntityLink>, String> {
    store::validate_note_id(&id).map_err(|e| e.to_string())?;
    let Ok(root) = data_root(&app) else { return Ok(vec![]) };
    match graph::resolve_local_ids(&root, &id) {
        Ok(v) => Ok(v
            .into_iter()
            .map(|(local_id, global_id, is_person)| ipc::EntityLink { local_id, global_id, is_person })
            .collect()),
        Err(e) => {
            eprintln!("note_entity_links: 解析失败,返回空: {e}");
            Ok(vec![])
        }
    }
}

/// 改实体显示名。很多录音提取的名字不对(ASR 同音异写),这是纠错入口——与查询类命令不同,
/// 这是写操作,失败要如实报给用户(不能静默降级)。人实体委托声纹库改名(id 不变);非人
/// 实体 id 随名字重算,撞已存在实体自动合并。
fn rename_entity_with_rebuild(
    root: PathBuf,
    id: String,
    new_name: String,
    queue_person_rebuild: impl FnOnce(PathBuf) -> Result<(), String>,
) -> Result<ipc::RenameEntityResult, String> {
    let is_person = !id.starts_with("e:");
    let outcome = graph::rename_entity(&root, &id, &new_name).map_err(|e| e.to_string())?;
    if is_person {
        queue_person_rebuild(root)?;
    }
    Ok(ipc::RenameEntityResult {
        new_id: outcome.new_id,
        merged: outcome.merged,
    })
}

#[tauri::command]
fn rename_entity(app: AppHandle, id: String, new_name: String) -> Result<ipc::RenameEntityResult, String> {
    let root = data_root(&app).map_err(|e| e.to_string())?;
    let is_person = !id.starts_with("e:");
    let result = rename_entity_with_rebuild(root, id, new_name, |root| {
        queue_person_graph_rebuild(&app, root, &tr!("人物改名", "Person rename"))
    });
    // 人实体改名落进声纹库,刷新 Qwen 热词缓存。放在 ? 之前:重建排队失败时改名
    // 可能已落库(rebuild 是 rename 之后的独立一步),宁可多刷一次不可漏刷。
    if is_person {
        refresh_qwen_hotwords_cache(&app);
    }
    result
}

/// P3 自动日历匹配(停止后/backfill 共用):开关开、已授权、无快照且未被清除
/// 才写。持锁复查——查询期间用户手动改选/清除则放弃。返回是否写入。
fn match_and_store_calendar(app: &AppHandle, note_id: &str) -> anyhow::Result<bool> {
    let s = app
        .path()
        .app_data_dir()
        .map(|d| settings::load(&d))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if !s.calendar_match_enabled {
        return Ok(false);
    }
    if calendar::permission_status() != calendar::Permission::Full {
        return Ok(false);
    }
    let root = notes_dir(app)?;
    let note = store::NoteStore::new(root.clone()).load(note_id)?;
    if note.meta.calendar.is_some() || note.meta.calendar_cleared {
        return Ok(false);
    }
    let Some((start_ms, end_ms)) = calendar::note_window_ms(&note.meta, &note.segments) else {
        return Ok(false);
    };
    let events = calendar::events_between(start_ms - 60_000, end_ms + 60_000)?;
    let Some(ev) = calendar::best_match(&events, start_ms, end_ms) else {
        return Ok(false);
    };
    let now = chrono::Local::now().to_rfc3339();
    let snap = calendar::snapshot_of(ev, "auto", &now);
    store::NoteStore::new(root).update_calendar(note_id, |meta| {
        if meta.calendar.is_some() || meta.calendar_cleared {
            return false; // 持锁复查:自动匹配绝不覆盖用户决定
        }
        meta.calendar = Some(snap.clone());
        true
    })
}

/// backfill 并发门:授权成功后的自动回填与用户手动回填不并跑。
static CALENDAR_BACKFILL_GATE: Mutex<()> = Mutex::new(());

/// 全库回填:一次拉取覆盖全部候选笔记的时间窗,内存逐笔记匹配(不逐笔记查
/// EventKit)。返回写入数。
fn backfill_calendar_impl(app: &AppHandle) -> anyhow::Result<u32> {
    let _gate = CALENDAR_BACKFILL_GATE.lock().unwrap();
    let s = app
        .path()
        .app_data_dir()
        .map(|d| settings::load(&d))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if !s.calendar_match_enabled || calendar::permission_status() != calendar::Permission::Full {
        return Ok(0);
    }
    let root = notes_dir(app)?;
    let store_ = store::NoteStore::new(root.clone());
    // 候选:完成态、无快照、未清除;窗口按全体候选的最早开始/最晚结束一次拉取。
    let mut windows: Vec<(String, i64, i64)> = Vec::new();
    for n in store_.list() {
        if n.state != "complete" {
            continue;
        }
        let Ok(note) = store_.load(&n.id) else { continue };
        if note.meta.calendar.is_some() || note.meta.calendar_cleared {
            continue;
        }
        if let Some((a, b)) = calendar::note_window_ms(&note.meta, &note.segments) {
            windows.push((n.id, a, b));
        }
    }
    if windows.is_empty() {
        return Ok(0);
    }
    let lo = windows.iter().map(|(_, a, _)| *a).min().unwrap() - 60_000;
    let hi = windows.iter().map(|(_, _, b)| *b).max().unwrap() + 60_000;
    let events = calendar::events_between(lo, hi)?;
    let mut written = 0u32;
    let now = chrono::Local::now().to_rfc3339();
    for (id, a, b) in windows {
        let Some(ev) = calendar::best_match(&events, a, b) else { continue };
        let snap = calendar::snapshot_of(ev, "auto", &now);
        let ok = store::NoteStore::new(root.clone()).update_calendar(&id, |meta| {
            if meta.calendar.is_some() || meta.calendar_cleared {
                return false;
            }
            meta.calendar = Some(snap.clone());
            true
        })?;
        if ok {
            written += 1;
        }
    }
    Ok(written)
}

/// 日历授权态(前端设置页/详情页查询;非 macOS 恒 unavailable → 整块隐藏)。
#[tauri::command]
fn calendar_permission() -> String {
    match calendar::permission_status() {
        calendar::Permission::Full => "full",
        calendar::Permission::WriteOnly => "write_only",
        calendar::Permission::Denied => "denied",
        calendar::Permission::NotDetermined => "not_determined",
        calendar::Permission::Unavailable => "unavailable",
    }
    .into()
}

/// 发起系统日历授权(只能由设置页说明卡「继续」触发)。授权成功后对历史笔记
/// 做一次 best-effort 回填。
#[tauri::command]
async fn request_calendar_permission(app: AppHandle) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let outcome = calendar::request_permission();
        if outcome == calendar::AuthOutcome::Granted {
            if let Err(e) = backfill_calendar_impl(&app) {
                eprintln!("calendar: 授权后回填失败(忽略): {e}");
            }
        }
        Ok(match outcome {
            calendar::AuthOutcome::Granted => "granted",
            calendar::AuthOutcome::Denied => "denied",
            calendar::AuthOutcome::Insufficient => "insufficient",
            calendar::AuthOutcome::Error => "error",
            calendar::AuthOutcome::Timeout => "timeout",
        }
        .into())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 详情页改选候选:笔记当天(前后各扩 2h)的非全天事件,按重叠降序;
/// 零重叠也列出(延迟开录/错记场景)。
#[tauri::command]
async fn list_calendar_candidates(
    app: AppHandle,
    id: String,
) -> Result<Vec<ipc::CalendarCandidate>, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<Vec<ipc::CalendarCandidate>, String> {
        store::validate_note_id(&id).map_err(|e| e.to_string())?;
        let root = notes_dir(&app).map_err(|e| e.to_string())?;
        let note = store::NoteStore::new(root).load(&id).map_err(|e| e.to_string())?;
        let Some((start_ms, end_ms)) = calendar::note_window_ms(&note.meta, &note.segments) else {
            return Ok(vec![]);
        };
        // 当天窗:起点回拨到当日 00:00 再扩 2h,终点同理,覆盖跨午夜。
        let day_ms = 86_400_000i64;
        let lo = (start_ms / day_ms) * day_ms - 2 * 3_600_000;
        let hi = ((end_ms / day_ms) + 1) * day_ms + 2 * 3_600_000;
        let events = calendar::events_between(lo, hi).map_err(|e| e.to_string())?;
        let mut out: Vec<ipc::CalendarCandidate> = events
            .iter()
            .filter(|e| !e.all_day)
            .map(|e| ipc::CalendarCandidate {
                event_id: e.event_id.clone(),
                title: e.title.clone(),
                start_ms: e.start_ms,
                end_ms: e.end_ms,
                attendee_n: e.attendees.len(),
                overlap_ms: calendar::overlap_ms(start_ms, end_ms, e.start_ms, e.end_ms),
            })
            .collect();
        out.sort_by(|a, b| b.overlap_ms.cmp(&a.overlap_ms).then(a.start_ms.cmp(&b.start_ms)));
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 手动改选/清除:Some(event_id) 从候选窗重取该事件快照(match_kind=manual,
/// 复位 tombstone);None 清除并立 tombstone(自动匹配/回填永不再绑)。
#[tauri::command]
async fn set_note_calendar_event(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    event_id: Option<String>,
) -> Result<(), String> {
    store::validate_note_id(&id).map_err(|e| e.to_string())?;
    reject_if_active(&state, &id)?;
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let root = notes_dir(&app).map_err(|e| e.to_string())?;
        let store_ = store::NoteStore::new(root);
        match event_id {
            None => {
                store_
                    .update_calendar(&id, |meta| {
                        meta.calendar = None;
                        meta.calendar_cleared = true;
                        true
                    })
                    .map_err(|e| e.to_string())?;
                Ok(())
            }
            Some(eid) => {
                let note = store_.load(&id).map_err(|e| e.to_string())?;
                let Some((start_ms, end_ms)) = calendar::note_window_ms(&note.meta, &note.segments)
                else {
                    return Err(tr!("笔记时间信息不完整", "Note time info incomplete"));
                };
                let day_ms = 86_400_000i64;
                let lo = (start_ms / day_ms) * day_ms - 2 * 3_600_000;
                let hi = ((end_ms / day_ms) + 1) * day_ms + 2 * 3_600_000;
                let events = calendar::events_between(lo, hi).map_err(|e| e.to_string())?;
                let Some(ev) = events.iter().find(|e| e.event_id == eid) else {
                    return Err(tr!(
                        "该日程已不在候选窗口内(可能被移动或删除)",
                        "Event no longer in the candidate window (moved or deleted)"
                    ));
                };
                let now = chrono::Local::now().to_rfc3339();
                let snap = calendar::snapshot_of(ev, "manual", &now);
                store_
                    .update_calendar(&id, |meta| {
                        meta.calendar = Some(snap.clone());
                        meta.calendar_cleared = false;
                        true
                    })
                    .map_err(|e| e.to_string())?;
                Ok(())
            }
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 手动全库回填(设置页入口可后续加;当前供授权成功后自动调用与 devtools)。
#[tauri::command]
async fn backfill_calendar_matches(app: AppHandle) -> Result<u32, String> {
    tauri::async_runtime::spawn_blocking(move || backfill_calendar_impl(&app).map_err(|e| e.to_string()))
        .await
        .map_err(|e| e.to_string())?
}

/// 手动触发/重试 identify(P2a):从现稿重建簇成员与簇质心(逐段重嵌入,复用
/// P1 feedback 纯逻辑核),走管线同一 run_identify。门禁同管线(refine_llm_ready
/// 内含于 identify_executor);持 FEEDBACK_GATE 全程——嵌入并发与 track_pcm
/// 临时文件竞争都收敛于此门。
#[tauri::command]
async fn identify_note(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<(), String> {
    store::validate_note_id(&id).map_err(|e| e.to_string())?;
    if app.state::<lifecycle::LifecycleHandle>().is_refining(&id) {
        return Err(tr!(
            "该笔记正在 Aing 中,稍后再试",
            "This note is being refined; try again later"
        ));
    }
    reject_if_active(&state, &id)?;
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let run = || -> anyhow::Result<()> {
            let s = app2
                .path()
                .app_data_dir()
                .map(|d| settings::load(&d))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let executor = identify_executor(&s)?;
            let root = notes_dir(&app2)?;
            let dir = root.join(&id);
            let doc = store::load_refined(&dir)
                .ok_or_else(|| anyhow::anyhow!(tr!("该笔记尚无精修稿,先运行 Aing", "No refined doc yet; run Aing first")))?;
            let note = store::NoteStore::new(root).load(&id)?;
            let vp = open_voiceprint_store(&app2).map_err(anyhow::Error::msg)?.load();
            let acoustic_enabled = vp.embedding_model == s.speaker_model;
            // FEEDBACK_GATE 只护住嵌入重建段——auto_apply_one 的同步回灌也要取它,
            // 锁不收窄会自锁(锁序恒 IDENTIFY_ACT_GATE → FEEDBACK_GATE)。
            let gate = FEEDBACK_GATE.lock().unwrap();

            // 簇质心重建:每簇成员段逐段重嵌入(与管线路径的 recluster 统计等价口径)。
            let members = refine::identify::cluster_members_from_doc(&doc);
            let mut stats: Vec<refine::recluster::ClusterStat> = Vec::new();
            if acoustic_enabled {
                let meta = store::audio::load_audio_meta(&dir);
                let mut pcm_by_source: std::collections::BTreeMap<String, (u64, Vec<f32>)> =
                    Default::default();
                for source in note
                    .segments
                    .iter()
                    .map(|sg| sg.source.as_str())
                    .collect::<std::collections::BTreeSet<_>>()
                {
                    match store::transcode::track_pcm(&dir, source) {
                        Ok(pcm) => {
                            let off = meta.tracks.get(source).map(|t| t.offset_ms).unwrap_or(0);
                            pcm_by_source.insert(source.to_string(), (off, pcm));
                        }
                        Err(e) => eprintln!("identify({id}): 音轨 {source} 解码失败,该轨无声学: {e}"),
                    }
                }
                // 权重取自决定 acoustic_enabled 的那份设置快照(s),不重读:重读的话
                // 中途切模型会拿 B 的向量去比快照里 A 的质心(codex review 实现轮五 P1)。
                let mut embedder = diar::SherpaEmbedder::new(&speaker_model_path_for(&s.speaker_model))?;
                for (speaker, seqs) in &members {
                    let segs: Vec<&store::SegmentRecord> =
                        note.segments.iter().filter(|sg| seqs.contains(&sg.seq)).collect();
                    let sstats = feedback::build_source_stats(&segs, &pcm_by_source, &mut embedder);
                    let mut centroids = std::collections::BTreeMap::new();
                    let mut source_ms = std::collections::BTreeMap::new();
                    let mut total = 0u64;
                    for (src, st) in sstats {
                        total += st.total_ms;
                        source_ms.insert(src.clone(), st.total_ms);
                        centroids.insert(src, st.centroid);
                    }
                    stats.push(refine::recluster::ClusterStat {
                        speaker: speaker.clone(),
                        centroids,
                        total_ms: total,
                        source_ms,
                        core_seqs: seqs.iter().copied().collect(),
                        seed: None,
                    });
                }
            }

            drop(gate); // 嵌入完毕即释放:后续自动应用要按 ACT→FEEDBACK 锁序再取

            let log_ctx = data_root(&app2)
                .ok()
                .map(|r| ailog::Ctx { data_root: r, note_id: id.clone() });
            let now = chrono::Local::now().to_rfc3339();
            recover_identify_ops(&app2, &id); // 无条件前置(理由同管线路径)
            let idoc = {
                let _gate = IDENTIFY_ACT_GATE.lock().unwrap();
                let idoc = refine::identify::run_identify(
                    &dir,
                    &id,
                    &doc,
                    &note.speakers,
                    &stats,
                    &vp,
                    acoustic_enabled,
                    note.meta.calendar.as_ref(),
                    executor.as_ref(),
                    log_ctx.as_ref(),
                    &now,
                )?;
                refine::identify::save_identify(&dir, &idoc)?;
                idoc
            };
            if s.identify_auto_apply {
                let fps: Vec<String> = store::load_refined(&dir)
                    .map(|d| {
                        refine::identify::auto_apply_targets(&idoc, &d, &note.speakers)
                            .iter()
                            .map(|a| a.fingerprint.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                for fp in fps {
                    if let Err(e) = auto_apply_one(&app2, &id, &fp) {
                        eprintln!("identify({id}): 自动应用 {fp} 未执行(留建议卡): {e}");
                    }
                }
            }
            let _ = app2.emit("identify_done", id.clone());
            Ok(())
        };
        run().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// identify 建议动作的命令级串行门:apply/reject 都是对同一 identify.json 的
/// 读改写,UI 双击/并发确认全收敛于此(list 只读不受影响)。
static IDENTIFY_ACT_GATE: Mutex<()> = Mutex::new(());

/// 一波说话人:LLM 提示词的说话人标签(id → 显示名)。段落不携带身份,标签由
/// 调用方现查 speakers.json(关联者跟库中现名,否则本地名;无名不进表,由
/// llm::speaker_label 退回裸 id)。
fn speaker_prompt_labels(
    speakers: &std::collections::BTreeMap<String, store::SpeakerMeta>,
    vp: &store::Voiceprints,
) -> std::collections::BTreeMap<String, String> {
    speakers
        .iter()
        .filter_map(|(sid, m)| {
            let name = m
                .person_id
                .as_deref()
                .and_then(|pid| store::VoiceprintStore::resolve(vp, pid))
                .and_then(|rid| vp.people.get(rid))
                .map(|p| p.name.clone())
                .filter(|n| !n.is_empty())
                .or_else(|| Some(m.name.clone()).filter(|n| !n.is_empty()))?;
            Some((sid.clone(), name))
        })
        .collect()
}

/// P2b 操作 id:进程号 + 计数 + 时间戳,不求密码学强度,只求全局不重。
fn identify_op_id() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "iop-{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        chrono::Utc::now().timestamp_millis()
    )
}

/// P2b 自动应用单条(意向日志护航,调用方须已确认 identify_auto_apply 开):
/// 写序 pending→assign→assigned→同步回灌→reinforced→status→done,每步落盘,
/// 崩溃由 recover_identify_ops 前滚。不做 is_refining 守卫——只在 Aing 线程/
/// identify_note 线程内调用,彼时无并发编辑。锁序:IDENTIFY_ACT_GATE →
/// FEEDBACK_GATE,绝不反向。
fn auto_apply_one(app: &AppHandle, note_id: &str, fingerprint: &str) -> anyhow::Result<()> {
    let _gate = IDENTIFY_ACT_GATE.lock().unwrap();
    let root = notes_dir(app)?;
    let dir = root.join(note_id);
    let mut idoc = refine::identify::load_identify(&dir)
        .ok_or_else(|| anyhow::anyhow!("identify.json 缺失"))?;
    let doc = store::load_refined(&dir).ok_or_else(|| anyhow::anyhow!("精修稿缺失"))?;
    // 资格在锁内复核(auto_apply_targets 含"说话人当前无关联无手填名"的全部前置;
    // 一波说话人后前置查 speakers.json,须在锁内取新鲜表)。
    let fresh = store::NoteStore::new(root.clone()).load(note_id)?;
    let eligible = refine::identify::auto_apply_targets(&idoc, &doc, &fresh.speakers)
        .iter()
        .any(|a| a.fingerprint == fingerprint);
    anyhow::ensure!(eligible, "条目已不满足自动应用前置");
    let a = idoc
        .assignments
        .iter()
        .find(|a| a.fingerprint == fingerprint)
        .expect("eligible 已含存在性")
        .clone();
    let target = a.person_id.clone().expect("eligible 已含库内人前置");
    let vp_store = open_voiceprint_store(app).map_err(anyhow::Error::msg)?;
    let vp = vp_store.load();
    let Some(resolved) = store::VoiceprintStore::resolve(&vp, &target).map(str::to_string) else {
        anyhow::bail!("目标人物已不在库");
    };
    let name = vp.people.get(&resolved).map(|p| p.name.clone()).unwrap_or_default();
    let members = refine::identify::cluster_members_from_doc(&doc);
    let (speaker, seqs) = members
        .iter()
        .find(|(_, sq)| refine::identify::cluster_fingerprint(sq) == fingerprint)
        .map(|(sp, sq)| (sp.clone(), sq.clone()))
        .ok_or_else(|| anyhow::anyhow!("指纹已不对应任何簇"))?;

    // ① 意向记录先落盘(pending)。
    let now = chrono::Local::now().to_rfc3339();
    let op_id = identify_op_id();
    let mut ops = refine::identify::load_ops(&dir);
    ops.ops.push(refine::identify::IdentifyOp {
        op_id: op_id.clone(),
        fingerprint: fingerprint.to_string(),
        cluster: speaker.clone(),
        seqs: seqs.iter().copied().collect(),
        target_person: resolved.clone(),
        target_name: name.clone(),
        quote: a.evidence.first().map(|e| e.quote.clone()).unwrap_or_default(),
        quote_type: a.evidence.first().map(|e| e.r#type.clone()).unwrap_or_default(),
        created_at: now.clone(),
        stage: "pending".into(),
        acknowledged: false,
        reinforce_skipped: None,
        undo_stage: None,
        non_revertible: None,
    });
    refine::identify::save_ops(&dir, &ops)?;
    let set_stage = |dir: &std::path::Path, op_id: &str, stage: &str, skipped: Option<String>| {
        let mut ops = refine::identify::load_ops(dir);
        if let Some(op) = ops.ops.iter_mut().find(|o| o.op_id == op_id) {
            op.stage = stage.into();
            if skipped.is_some() {
                op.reinforce_skipped = skipped;
            }
        }
        let _ = refine::identify::save_ops(dir, &ops);
    };

    // ② 关联:一波说话人——写 speakers.json,CAS「当前未关联才写」(store 层自取
    // NoteLock;与资格复核间若被用户抢先关联,这里原子拒绝)。
    store::NoteStore::new(root.clone()).assign_speaker_person_if(note_id, &speaker, &resolved)?;
    set_stage(&dir, &op_id, "assigned", None);

    // ③ 同步回灌(自动路径绝不异步——否则撤销后后台污染)。
    let skipped = {
        let _fb = FEEDBACK_GATE.lock().unwrap();
        let note = store::NoteStore::new(root.clone()).load(note_id)?;
        let expected = app
            .path()
            .app_data_dir()
            .map(|d| settings::load(&d).speaker_model)
            .unwrap_or_default();
        let library_model = vp.embedding_model.clone();
        // 标签与权重同源(同 spawn_feedback 那处):不能再读一次设置,否则可能用 B 的
        // 权重算、以 A 的标签写库(codex review 实现轮 P1)。
        match diar::SherpaEmbedder::new(&speaker_model_path_for(&expected)) {
            Ok(mut embedder) => {
                let mut needs_rebuild = false;
                let r = feedback::reinforce_person(
                    &dir,
                    &note.segments,
                    &feedback::SegFilter::Seqs(seqs.clone()),
                    &resolved,
                    &vp_store,
                    &library_model,
                    &expected,
                    &mut embedder,
                    &now,
                    Some(&op_id),
                    &mut needs_rebuild,
                    false,
                );
                // 先无条件处理重建,再判回灌结果:纠错还原一旦清空了旧人物的质心就已经
                // 落盘,回灌本身跳过或出错都不改变"必须重建"这件事
                // (codex review 实现轮二 P1)。此处已出 vp_guard。
                if needs_rebuild {
                    let st = app.state::<AppState>();
                    *st.embedder_cache.lock().unwrap() = None;
                    spawn_voiceprint_rebuild(app, st.embedder_cache.clone(), "纠错还原后质心置空");
                }
                match r {
                    Ok(feedback::ReinforceResult::Applied { .. }) => None,
                    Ok(other) => Some(format!("{other:?}")),
                    Err(e) => Some(format!("回灌失败: {e}")),
                }
            }
            Err(e) => Some(format!("嵌入器不可用: {e}")),
        }
    };
    set_stage(&dir, &op_id, "reinforced", skipped);

    // ④ 状态落盘 + done。
    refine::identify::mark_transition(&mut idoc, fingerprint, &["suggested"], "auto_applied", &now)?;
    refine::identify::save_identify(&dir, &idoc)?;
    set_stage(&dir, &op_id, "done", None);
    eprintln!("identify({note_id}): 已自动认出 {speaker} = {name}(op {op_id})");
    Ok(())
}

/// P2b 崩溃恢复:未完成 op 前滚(pending 且未见关联=放弃;assigned 起=补回灌/补状态)。
/// 在自动应用循环前、IDENTIFY_ACT_GATE 内由调用方间接串行(本函数自取锁)。
fn recover_identify_ops(app: &AppHandle, note_id: &str) {
    let run = || -> anyhow::Result<()> {
        let _gate = IDENTIFY_ACT_GATE.lock().unwrap();
        let root = notes_dir(app)?;
        let dir = root.join(note_id);
        let mut ops = refine::identify::load_ops(&dir);
        let pending: Vec<String> = ops
            .ops
            .iter()
            .filter(|o| {
                (o.stage != "done" && o.undo_stage.is_none())
                    || matches!(o.undo_stage.as_deref(), Some("undo_pending") | Some("link_cleared"))
            })
            .map(|o| o.op_id.clone())
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        let doc = store::load_refined(&dir).ok_or_else(|| anyhow::anyhow!("精修稿缺失"))?;
        let mut idoc = refine::identify::load_identify(&dir)
            .ok_or_else(|| anyhow::anyhow!("identify.json 缺失"))?;
        for op_id in pending {
            let Some(op) = ops.ops.iter_mut().find(|o| o.op_id == op_id) else { continue };
            // 撤销中途崩溃的恢复:undo_pending=实质动作未发生,回退到可撤状态;
            // link_cleared=关联已清,前滚完成质心还原与拒绝键(与撤销命令 ②③ 同)。
            match op.undo_stage.as_deref() {
                Some("undo_pending") => {
                    op.undo_stage = None;
                    continue;
                }
                Some("link_cleared") => {
                    let seqs: std::collections::BTreeSet<u64> =
                        op.seqs.iter().copied().collect();
                    if op.reinforce_skipped.is_none() {
                        if let Ok(vp_store) = open_voiceprint_store(app) {
                            match feedback::undo_reinforce_op(
                                &dir,
                                &seqs,
                                &op.target_person,
                                &op.op_id,
                                &vp_store,
                            ) {
                                Ok(feedback::UndoOutcome::Restored) => {}
                                // 快照来自另一个模型空间:质心已置空,必须**当场**排重建。
                                // 早先这里只记日志,理由写的是"下次启动自愈会兜住"——
                                // 那是错的:restore_feedback 不改库标签,标签恒相等,
                                // 自愈的判据永远不成立(codex review 实现轮 P1)。
                                Ok(feedback::UndoOutcome::RestoredNeedsRebuild) => {
                                    eprintln!("identify 恢复:回灌快照来自另一空间,质心已置空,排重建");
                                    let st = app.state::<AppState>();
                                    *st.embedder_cache.lock().unwrap() = None;
                                    spawn_voiceprint_rebuild(
                                        app,
                                        st.embedder_cache.clone(),
                                        "identify 恢复后质心置空",
                                    );
                                }
                                Ok(feedback::UndoOutcome::NoEntry) => {
                                    op.non_revertible
                                        .get_or_insert("ledger-lost(账本缺失,污染未回滚)".into());
                                }
                                Ok(feedback::UndoOutcome::NotRevertible(r)) => {
                                    op.non_revertible.get_or_insert(r.into());
                                }
                                Err(e) => {
                                    op.non_revertible.get_or_insert(format!("restore-error: {e}"));
                                }
                            }
                        }
                    }
                    if refine::identify::mark_rejected(&mut idoc, &op.fingerprint, &op.created_at)
                        .is_err()
                    {
                        // 条目已被新一轮吞掉:拒绝键直接落(同目标不再建议)。
                        idoc.rejected.insert(
                            refine::identify::rejected_key(&op.fingerprint, &op.target_person),
                            op.created_at.clone(),
                        );
                    }
                    op.undo_stage = Some("undone".into());
                    continue;
                }
                _ => {}
            }
            let seqs: std::collections::BTreeSet<u64> = op.seqs.iter().copied().collect();
            let linked_to_target = refine::identify::cluster_members_from_doc(&doc)
                .iter()
                .find(|(_, sq)| **sq == seqs)
                .map(|(sp, _)| {
                    doc.paragraphs
                        .iter()
                        .filter(|p| &p.speaker == sp)
                        .all(|p| p.person_id.as_deref() == Some(op.target_person.as_str()))
                })
                .unwrap_or(false);
            if op.stage == "pending" && !linked_to_target {
                // assign 未发生:放弃,建议卡还在,无痕。
                op.stage = "aborted".into();
                op.undo_stage = Some("undone".into());
                continue;
            }
            // assigned/reinforced(或 pending 但已见关联):前滚补状态;回灌交给
            // 幂等账本(同 scope 已有条目则 reinforce 已发生;缺则标 skipped——
            // 崩溃点在回灌中途时宁可少灌,绝不重复加权)。
            if op.stage == "pending" || op.stage == "assigned" {
                op.reinforce_skipped
                    .get_or_insert("crash-before-reinforce(未回灌,宁缺勿重)".into());
                op.stage = "reinforced".into();
            }
            if refine::identify::mark_transition(
                &mut idoc,
                &op.fingerprint,
                &["suggested"],
                "auto_applied",
                &op.created_at,
            )
            .is_err()
                && !idoc.assignments.iter().any(|a| a.fingerprint == op.fingerprint)
            {
                // 条目已被新一轮吞掉:按 op 记录合成回执条目,撤销入口不丢。
                idoc.assignments.push(refine::identify::IdentifyAssignment {
                    fingerprint: op.fingerprint.clone(),
                    cluster: op.cluster.clone(),
                    person_id: Some(op.target_person.clone()),
                    new_name: None,
                    tier: refine::identify::Tier::High,
                    llm_confidence: "high".into(),
                    acoustic: None,
                    acoustic_z: None,
                    evidence: vec![],
                    status: "auto_applied".into(),
                    decided_at: Some(op.created_at.clone()),
                });
            }
            op.stage = "done".into();
        }
        refine::identify::save_identify(&dir, &idoc)?;
        refine::identify::save_ops(&dir, &ops)?;
        Ok(())
    };
    if let Err(e) = run() {
        eprintln!("identify({note_id}): op 恢复失败(忽略): {e}");
    }
}

/// 收件箱身份建议列表:扫各笔记 identify.json,只收 status=suggested 且「新鲜」
/// (source_hash 与现稿一致、指纹仍对应某簇、该簇当前无关联、目标人仍在库)。
#[tauri::command]
fn list_identify_suggestions(app: AppHandle) -> Result<Vec<ipc::IdentifySuggestion>, String> {
    let root = notes_dir(&app).map_err(|e| e.to_string())?;
    let vp = open_voiceprint_store(&app)?.load();
    let notes = store::NoteStore::new(root.clone()).list();
    let mut out: Vec<ipc::IdentifySuggestion> = Vec::new();
    for n in notes {
        let dir = root.join(&n.id);
        let Some(idoc) = refine::identify::load_identify(&dir) else { continue };
        if idoc.assignments.iter().all(|a| a.status != "suggested") {
            continue;
        }
        let Some(doc) = store::load_refined(&dir) else { continue };
        if store::source_hash(&doc.paragraphs) != idoc.source_hash {
            continue; // 稿已被精修/编辑,证据锚点不可信:等下轮 identify 重建
        }
        let members = refine::identify::cluster_members_from_doc(&doc);
        let fp_to_speaker: std::collections::BTreeMap<String, String> = members
            .iter()
            .map(|(sp, seqs)| (refine::identify::cluster_fingerprint(seqs), sp.clone()))
            .collect();
        let linked: std::collections::BTreeSet<&str> = doc
            .paragraphs
            .iter()
            .filter(|p| p.person_id.is_some())
            .map(|p| p.speaker.as_str())
            .collect();
        for a in idoc.assignments.iter().filter(|a| a.status == "suggested") {
            let Some(speaker) = fp_to_speaker.get(&a.fingerprint) else { continue };
            if linked.contains(speaker.as_str()) {
                continue; // 用户已手动关联,不再打扰
            }
            let (person_id, person_name, is_new) = match (&a.person_id, &a.new_name) {
                (Some(pid), _) => {
                    let Some(rid) = store::VoiceprintStore::resolve(&vp, pid) else { continue };
                    let name = vp.people.get(rid).map(|p| p.name.clone()).unwrap_or_default();
                    if name.trim().is_empty() {
                        continue;
                    }
                    (Some(rid.to_string()), name, false)
                }
                (None, Some(nn)) => (None, nn.clone(), true),
                _ => continue,
            };
            let ev = a.evidence.first();
            out.push(ipc::IdentifySuggestion {
                note_id: n.id.clone(),
                note_title: n.title.clone(),
                cluster: speaker.clone(),
                fingerprint: a.fingerprint.clone(),
                person_id,
                person_name,
                is_new,
                tier: match a.tier {
                    refine::identify::Tier::High => "high",
                    refine::identify::Tier::Medium => "medium",
                    refine::identify::Tier::Low => "low",
                }
                .into(),
                quote: ev.map(|e| e.quote.clone()).unwrap_or_default(),
                evidence_type: ev.map(|e| e.r#type.clone()).unwrap_or_default(),
                generated_at: idoc.generated_at.clone(),
                status: "suggested".into(),
                op_id: None,
                revertible: true,
            });
        }
    }
    out.sort_by(|a, b| b.generated_at.cmp(&a.generated_at));
    out.truncate(50);

    // P2b 自动回执:渲染自意向日志(永续可见,不受新鲜度与 50 条上限约束——
    // 撤销入口不能因稿变化/淘汰而消失);revertible=簇仍可按指纹定位且关联仍是
    // 自动目标(否则冲突态只留「好」)。
    let mut receipts: Vec<ipc::IdentifySuggestion> = Vec::new();
    for n in store::NoteStore::new(root.clone()).list() {
        let dir = root.join(&n.id);
        let ops = refine::identify::load_ops(&dir);
        let pending: Vec<_> = ops
            .ops
            .iter()
            .filter(|o| o.stage == "done" && !o.acknowledged && o.undo_stage.is_none())
            .collect();
        if pending.is_empty() {
            continue;
        }
        let doc = store::load_refined(&dir);
        for op in pending {
            let revertible = doc
                .as_ref()
                .map(|d| {
                    let seqs: std::collections::BTreeSet<u64> = op.seqs.iter().copied().collect();
                    refine::identify::cluster_members_from_doc(d)
                        .iter()
                        .find(|(_, sq)| **sq == seqs)
                        .map(|(sp, _)| {
                            d.paragraphs
                                .iter()
                                .filter(|p| &p.speaker == sp)
                                .all(|p| p.person_id.as_deref() == Some(op.target_person.as_str()))
                        })
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            let name = store::VoiceprintStore::resolve(&vp, &op.target_person)
                .and_then(|rid| vp.people.get(rid))
                .map(|p| p.name.clone())
                .filter(|nm| !nm.trim().is_empty())
                .unwrap_or_else(|| op.target_name.clone());
            receipts.push(ipc::IdentifySuggestion {
                note_id: n.id.clone(),
                note_title: n.title.clone(),
                cluster: op.cluster.clone(),
                fingerprint: op.fingerprint.clone(),
                person_id: Some(op.target_person.clone()),
                person_name: name,
                is_new: false,
                tier: "high".into(),
                quote: op.quote.clone(),
                evidence_type: if op.quote_type.is_empty() { "self_intro".into() } else { op.quote_type.clone() },
                generated_at: op.created_at.clone(),
                status: "auto_applied".into(),
                op_id: Some(op.op_id.clone()),
                revertible,
            });
        }
    }
    receipts.sort_by(|a, b| b.generated_at.cmp(&a.generated_at));
    receipts.extend(out);
    Ok(receipts)
}

/// P2b 回执「好」:确认自动认人(回执消失);identify.json 状态 auto_applied→applied
/// best-effort(稿重生成后条目可能不在,确认动作以 op 记录为准)。
#[tauri::command]
fn acknowledge_identify(app: AppHandle, note_id: String, op_id: String) -> Result<(), String> {
    store::validate_note_id(&note_id).map_err(|e| e.to_string())?;
    let _gate = IDENTIFY_ACT_GATE.lock().unwrap();
    let dir = notes_dir(&app).map_err(|e| e.to_string())?.join(&note_id);
    let mut ops = refine::identify::load_ops(&dir);
    let op = ops
        .ops
        .iter_mut()
        .find(|o| o.op_id == op_id && o.stage == "done" && o.undo_stage.is_none())
        .ok_or_else(|| tr!("回执不存在或已处理", "Receipt missing or already handled"))?;
    op.acknowledged = true;
    let fp = op.fingerprint.clone();
    refine::identify::save_ops(&dir, &ops).map_err(|e| e.to_string())?;
    if let Some(mut idoc) = refine::identify::load_identify(&dir) {
        let now = chrono::Local::now().to_rfc3339();
        if refine::identify::mark_transition(&mut idoc, &fp, &["auto_applied"], "applied", &now).is_ok() {
            let _ = refine::identify::save_identify(&dir, &idoc);
        }
    }
    Ok(())
}

/// P2b 回执「撤销」:CAS 解除关联 + 按 op 对账还原质心 + 拒绝键。返回质心是否
/// 还原(false=已被后续写动过,关联已解除但声纹保留,前端如实提示)。
#[tauri::command]
async fn undo_identify_apply(
    app: AppHandle,
    state: State<'_, AppState>,
    note_id: String,
    op_id: String,
) -> Result<bool, String> {
    store::validate_note_id(&note_id).map_err(|e| e.to_string())?;
    if app.state::<lifecycle::LifecycleHandle>().is_refining(&note_id) {
        return Err(tr!("该笔记正在 Aing 中,稍后再试", "This note is being refined; try again later"));
    }
    reject_if_active(&state, &note_id)?;
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<bool, String> {
        let _gate = IDENTIFY_ACT_GATE.lock().unwrap();
        let root = notes_dir(&app2).map_err(|e| e.to_string())?;
        let dir = root.join(&note_id);
        let mut ops = refine::identify::load_ops(&dir);
        let idx = ops
            .ops
            .iter()
            .position(|o| o.op_id == op_id && o.stage == "done" && !o.acknowledged)
            .ok_or_else(|| tr!("回执不存在或已处理", "Receipt missing or already handled"))?;
        let (seqs, target, fp, reinforced) = {
            let op = &mut ops.ops[idx];
            if op.undo_stage.as_deref() == Some("undone") {
                return Ok(op.non_revertible.is_none()); // 幂等重入
            }
            op.undo_stage = Some("undo_pending".into());
            (
                op.seqs.iter().copied().collect::<std::collections::BTreeSet<u64>>(),
                op.target_person.clone(),
                op.fingerprint.clone(),
                op.reinforce_skipped.is_none(),
            )
        };
        refine::identify::save_ops(&dir, &ops).map_err(|e| e.to_string())?;

        // ① 定位当前簇并 CAS 解除关联(用户已手改则拒绝并回退 undo 状态)。
        let doc = store::load_refined(&dir)
            .ok_or_else(|| tr!("精修稿缺失", "Refined doc missing"))?;
        let speaker = refine::identify::cluster_members_from_doc(&doc)
            .iter()
            .find(|(_, sq)| **sq == seqs)
            .map(|(sp, _)| sp.clone());
        let Some(speaker) = speaker else {
            ops.ops[idx].undo_stage = None;
            ops.ops[idx].non_revertible = Some("clusters-changed".into());
            let _ = refine::identify::save_ops(&dir, &ops);
            return Err(tr!(
                "说话人分组已变化,无法自动撤销(可手动改指认)",
                "Speaker clusters changed; cannot auto-undo (reassign manually)"
            ));
        };
        if let Err(e) =
            store::NoteStore::new(root.clone()).clear_speaker_person_if(&note_id, &speaker, &target)
        {
            ops.ops[idx].undo_stage = None;
            let _ = refine::identify::save_ops(&dir, &ops);
            return Err(e.to_string());
        }
        ops.ops[idx].undo_stage = Some("link_cleared".into());
        refine::identify::save_ops(&dir, &ops).map_err(|e| e.to_string())?;

        // ② 质心还原(按 op 对账;不可还原如实记录,绝不错撤后续人工作业)。
        // FEEDBACK_GATE:与人工回灌对同一账本/人物快照的读改写互斥
        // (锁序恒 IDENTIFY_ACT_GATE → FEEDBACK_GATE)。
        let vp_store = open_voiceprint_store(&app2)?;
        let restored = {
            let _fb = FEEDBACK_GATE.lock().unwrap();
            match feedback::undo_reinforce_op(&dir, &seqs, &target, &op_id, &vp_store) {
                Ok(feedback::UndoOutcome::Restored) => true,
                Ok(feedback::UndoOutcome::RestoredNeedsRebuild) => {
                    // 撤销成功,但质心因跨空间被置空 → 排一次重建把这个人从样本长回来。
                    // 放在门内是安全的:spawn_voiceprint_rebuild 只是起线程,不取 vp_guard。
                    let st = app.state::<AppState>();
                    *st.embedder_cache.lock().unwrap() = None;
                    spawn_voiceprint_rebuild(&app, st.embedder_cache.clone(), "撤销回灌后质心置空");
                    true
                }
                Ok(feedback::UndoOutcome::NoEntry) if !reinforced => true, // 未曾回灌=无污染
                Ok(feedback::UndoOutcome::NoEntry) => {
                    // op 声称回灌过而账本无条:账丢了,污染无法回滚,如实报。
                    ops.ops[idx].non_revertible = Some("ledger-lost".into());
                    false
                }
                Ok(feedback::UndoOutcome::NotRevertible(reason)) => {
                    ops.ops[idx].non_revertible = Some(reason.into());
                    false
                }
                Err(e) => {
                    ops.ops[idx].non_revertible = Some(format!("restore-error: {e}"));
                    false
                }
            }
        };

        // ③ 状态与拒绝键(同目标不再建议)+ undone。
        if let Some(mut idoc) = refine::identify::load_identify(&dir) {
            let now = chrono::Local::now().to_rfc3339();
            if refine::identify::mark_rejected(&mut idoc, &fp, &now).is_err() {
                // 条目已被新一轮吞掉:拒绝键直接落,同目标不再建议。
                idoc.rejected
                    .insert(refine::identify::rejected_key(&fp, &target), now.clone());
            }
            let _ = refine::identify::save_identify(&dir, &idoc);
        }
        ops.ops[idx].undo_stage = Some("undone".into());
        refine::identify::save_ops(&dir, &ops).map_err(|e| e.to_string())?;
        Ok(restored)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 确认身份建议:锁外走 do_assign_refined_person(内部自取 NoteLock,不嵌套);
/// 新面孔先建档,assign 失败补偿删除空档案;成功后回写 status=applied。
#[tauri::command]
async fn apply_identify_suggestion(
    app: AppHandle,
    note_id: String,
    fingerprint: String,
) -> Result<(), String> {
    store::validate_note_id(&note_id).map_err(|e| e.to_string())?;
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let _gate = IDENTIFY_ACT_GATE.lock().unwrap();
        let dir = notes_dir(&app2).map_err(|e| e.to_string())?.join(&note_id);
        let mut idoc = refine::identify::load_identify(&dir)
            .ok_or_else(|| tr!("建议已失效", "Suggestion no longer valid"))?;
        let a = idoc
            .assignments
            .iter()
            .find(|a| a.fingerprint == fingerprint && a.status == "suggested")
            .ok_or_else(|| tr!("建议已失效", "Suggestion no longer valid"))?
            .clone();
        // 指纹复核:现稿仍有该成员集的簇。R 号可以变(重聚类重编号),成员集不能变。
        let doc = store::load_refined(&dir)
            .ok_or_else(|| tr!("精修稿缺失", "Refined doc missing"))?;
        let members = refine::identify::cluster_members_from_doc(&doc);
        let speaker = members
            .iter()
            .find(|(_, seqs)| refine::identify::cluster_fingerprint(seqs) == fingerprint)
            .map(|(sp, _)| sp.clone());
        let now = chrono::Local::now().to_rfc3339();
        let Some(speaker) = speaker else {
            let _ = refine::identify::mark_rejected(&mut idoc, &fingerprint, &now);
            let _ = refine::identify::save_identify(&dir, &idoc);
            return Err(tr!(
                "建议已过期(说话人分组已变化)",
                "Suggestion expired (speaker clusters changed)"
            ));
        };
        let vp_store = open_voiceprint_store(&app2)?;
        let (target, created) = match (&a.person_id, &a.new_name) {
            (Some(pid), _) => (pid.clone(), false),
            (None, Some(nn)) => (vp_store.create_person(nn, &now).map_err(|e| e.to_string())?, true),
            _ => return Err(tr!("建议数据异常", "Corrupt suggestion")),
        };
        // 录制中拒绝(speakers.json 由 writer 独占;与手动关联命令同守卫)。
        if let Err(e) = reject_if_active(&app2.state::<AppState>(), &note_id)
            .and_then(|_| do_assign_note_speaker_person(&app2, &note_id, &speaker, &target))
        {
            if created {
                let _ = vp_store.delete_person_if_empty(&target);
            }
            return Err(e);
        }
        // P3:参会人邮箱记录——三重唯一性防线(目标人名与某参会人名精确相等、
        // 该名在参会人中唯一、该名在库中唯一)防同名污染;残余风险=确认本身指错。
        if let Ok(note) = store::NoteStore::new(notes_dir(&app2).map_err(|e| e.to_string())?).load(&note_id) {
            if let Some(cal) = &note.meta.calendar {
                let vp_now = vp_store.load();
                if let Some(target_name) =
                    store::VoiceprintStore::resolve(&vp_now, &target).and_then(|rid| vp_now.people.get(rid)).map(|p| p.name.clone())
                {
                    let hits: Vec<_> = cal
                        .attendees
                        .iter()
                        .filter(|a| !a.email.is_empty() && a.name == target_name)
                        .collect();
                    let library_unique =
                        vp_now.people.values().filter(|p| p.name == target_name).count() == 1;
                    if hits.len() == 1 && library_unique && !target_name.trim().is_empty() {
                        if let Err(e) = vp_store.add_person_email(&target, &hits[0].email) {
                            eprintln!("calendar: 记录参会人邮箱失败(忽略): {e}");
                        }
                    }
                }
            }
        }
        refine::identify::mark_applied(&mut idoc, &fingerprint, &now).map_err(|e| e.to_string())?;
        refine::identify::save_identify(&dir, &idoc).map_err(|e| e.to_string())?;
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 拒绝身份建议:status=rejected + 拒绝表记「指纹|目标」——同目标永不再建议,
/// 其它候选不受影响。走后端真值,不用前端 dismissed 字符串名单。
#[tauri::command]
fn reject_identify_suggestion(
    app: AppHandle,
    note_id: String,
    fingerprint: String,
) -> Result<(), String> {
    store::validate_note_id(&note_id).map_err(|e| e.to_string())?;
    let _gate = IDENTIFY_ACT_GATE.lock().unwrap();
    let dir = notes_dir(&app).map_err(|e| e.to_string())?.join(&note_id);
    let mut idoc = refine::identify::load_identify(&dir)
        .ok_or_else(|| tr!("建议已失效", "Suggestion no longer valid"))?;
    let now = chrono::Local::now().to_rfc3339();
    refine::identify::mark_rejected(&mut idoc, &fingerprint, &now).map_err(|e| e.to_string())?;
    refine::identify::save_identify(&dir, &idoc).map_err(|e| e.to_string())?;
    Ok(())
}

/// 笔记页 WYSIWYG 整篇保存精修稿。守卫与 rename_refined_speaker 同套:Aing 中拒绝
/// (管线随后整写会吞掉编辑),录制中拒绝;revision 乐观并发在 store 层校验。
#[tauri::command]
fn save_refined(
    app: AppHandle,
    state: State<AppState>,
    note_id: String,
    revision: u64,
    paragraphs: Vec<store::ParagraphPayload>,
) -> Result<u64, String> {
    if app.state::<lifecycle::LifecycleHandle>().is_refining(&note_id) {
        return Err(tr!(
            "该笔记正在 Aing 中，稍后再存",
            "This note is being refined by AI; save again later"
        ));
    }
    reject_if_active(&state, &note_id)?;
    store::validate_note_id(&note_id).map_err(|e| e.to_string())?;
    let root = notes_dir(&app).map_err(|e| e.to_string())?;
    store::save_refined_paragraphs(&root.join(&note_id), revision, &paragraphs)
        .map_err(|e| e.to_string())
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
        return Err(tr!("笔记不存在: {id}", "Note not found: {id}"));
    }
    let tracks = store::audio::list_tracks(&note_dir);
    // 波形懒回填(读路径本身仍纯读,重活在后台线程):无预计算波形的轨道算一次写回
    // audio.json,完成发 transcode_done 让详情页重拉音轨(复用停录转码的刷新链)。
    // 两类来源都走这里:①波形功能上线前转码的 m4a(解码后桶化);②未转码 WAV
    // (中断笔记/转码失败降级,直接流式扫)——后者曾在 list_tracks 里同步现算,长会议
    // 数百 MB 全扫是切换卡顿主因,移到此处后台。in-flight 集合防同一轨并发重复回填。
    for t in &tracks {
        if t.waveform.is_some() {
            continue;
        }
        let is_m4a = t.path.ends_with(".m4a");
        let is_wav = t.path.ends_with(".wav");
        if !is_m4a && !is_wav {
            continue;
        }
        static INFLIGHT: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);
        let key = format!("{id}/{}", t.source);
        {
            let mut g = INFLIGHT.lock().unwrap();
            let set = g.get_or_insert_with(Default::default);
            if !set.insert(key.clone()) {
                continue;
            }
        }
        let (app, note_dir, source, note_id) =
            (app.clone(), note_dir.clone(), t.source.clone(), id.clone());
        std::thread::spawn(move || {
            let res = if is_m4a {
                store::transcode::backfill_waveform(&note_dir, &source)
            } else {
                store::audio::backfill_wav_waveform(&note_dir, &source)
            };
            match res {
                Ok(()) => {
                    let _ = app.emit("transcode_done", ipc::TranscodeEvent { note_id });
                }
                Err(e) => eprintln!("波形回填失败({note_id}/{source}),维持段落包络: {e}"),
            }
            INFLIGHT.lock().unwrap().as_mut().map(|s| s.remove(&key));
        });
    }
    Ok(tracks)
}

#[tauri::command]
fn rename_note(app: AppHandle, state: State<AppState>, id: String, title: String) -> Result<(), String> {
    if state.session.lock().unwrap().as_ref().map(|s| s.note_id == id).unwrap_or(false) {
        return Err(tr!("录制中的笔记不能改名", "A note being recorded cannot be renamed"));
    }
    let title = title.trim();
    if title.is_empty() {
        return Err(tr!("标题不能为空", "Title cannot be empty"));
    }
    // 非活动编辑经 actor 串行执行(取代 NoteStore 直写,见 lifecycle/actor.rs run_edit)。
    app.state::<lifecycle::LifecycleHandle>().request(lifecycle::machine::Msg::EditNote {
        op: lifecycle::machine::EditOp::Rename { id, title: title.to_string() },
    })
}

#[tauri::command]
fn delete_note(app: AppHandle, state: State<AppState>, id: String) -> Result<(), String> {
    if state.session.lock().unwrap().as_ref().map(|s| s.note_id == id).unwrap_or(false) {
        return Err(tr!("录制中的笔记不能删除", "A note being recorded cannot be deleted"));
    }
    app.state::<lifecycle::LifecycleHandle>().request(lifecycle::machine::Msg::EditNote {
        op: lifecycle::machine::EditOp::Delete { id },
    })
}

/// 改说话人显示名：录制中的笔记也允许改。
/// 活动会话经 lifecycle 信箱走 writer 单写者路径(P2:writer 归 actor)——改内存表、
/// persist_speakers 原子落盘、广播都在 actor 线程串行执行,与管线事件同线程,天然
/// 杜绝互相覆盖窗口(不再经 NoteStore 直写);非活动笔记才走 NoteStore 直写磁盘。
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
        return Err(tr!("名字不能为空", "Name cannot be empty"));
    }
    // 活动判定读 session 槽(与旧实现一致;槽与 actor 的 writer 槽同源于同一会话)。
    // statement-scoped 取值:request() 阻塞等 actor,而 actor 的执行体可能要取
    // session 锁——持锁等待会成环(见 actor.rs 死锁注记③)。判定与执行之间恰逢
    // 停录的竞态窗口由执行器按槽内 note_id 对账兜底报错。
    let active = state
        .session
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.note_id == note_id)
        .unwrap_or(false);
    if active {
        return app.state::<lifecycle::LifecycleHandle>().request(
            lifecycle::machine::Msg::RenameActiveSpeaker { note_id, speaker_id, name: name.into() },
        );
    }
    // 非活动笔记：经 actor 串行执行(取代 NoteStore 直写)。
    app.state::<lifecycle::LifecycleHandle>().request(lifecycle::machine::Msg::EditNote {
        op: lifecycle::machine::EditOp::RenameSpeaker {
            id: note_id,
            speaker_id,
            name: name.to_string(),
        },
    })
}

/// 删除笔记内说话人(原始逐字稿 chips):表项移除,名下段落回到未标注。
/// 只动本笔记,不碰人物库。录制中拒绝(与段落编辑同模式,不做活动会话变体:
/// 删除是低频清理动作,录完再删没有代价);Aing 中拒绝(管线随后整写会引用
/// speakers 的产物)。
#[tauri::command]
fn delete_note_speaker(
    app: AppHandle,
    state: State<AppState>,
    note_id: String,
    speaker_id: String,
) -> Result<(), String> {
    if app.state::<lifecycle::LifecycleHandle>().is_refining(&note_id) {
        return Err(tr!("该笔记正在 Aing 中，稍后再删", "This note is being refined by AI; try again later"));
    }
    reject_if_active(&state, &note_id)?;
    app.state::<lifecycle::LifecycleHandle>().request(lifecycle::machine::Msg::EditNote {
        op: lifecycle::machine::EditOp::DeleteSpeaker { id: note_id, speaker_id },
    })
}

/// 段落编辑共用 guard：活动会话笔记一律拒绝（与 rename_note 同模式）。
fn reject_if_active(state: &State<AppState>, note_id: &str) -> Result<(), String> {
    if state.session.lock().unwrap().as_ref().map(|s| s.note_id == note_id).unwrap_or(false) {
        return Err(tr!("录制中的笔记不能编辑", "A note being recorded cannot be edited"));
    }
    Ok(())
}

/// 活动会话判定：与 rename_speaker 同款 statement-scoped 取值——request() 阻塞等
/// actor,而 actor 的执行体可能要取 session 锁,持锁等 reply 会成环(见 actor.rs
/// 死锁注记③)。判定与执行之间的停录竞态由执行器按槽内 note_id 对账兜底报错。
fn is_active_note(state: &State<AppState>, note_id: &str) -> bool {
    state.session.lock().unwrap().as_ref().map(|s| s.note_id == note_id).unwrap_or(false)
}

/// 改段文本：活动笔记走 lifecycle 信箱的 writer 单写者路径(与定稿追加同线程串行),
/// 非活动笔记走既有 EditNote 冷路径直改磁盘。前端调用方无感,同一个命令。
#[tauri::command]
fn edit_segment(
    app: AppHandle,
    state: State<AppState>,
    note_id: String,
    seq: u64,
    expected_text: String,
    new_text: String,
) -> Result<(), String> {
    if is_active_note(&state, &note_id) {
        return app.state::<lifecycle::LifecycleHandle>().request(
            lifecycle::machine::Msg::EditActiveSegment { note_id, seq, expected_text, new_text },
        );
    }
    app.state::<lifecycle::LifecycleHandle>().request(lifecycle::machine::Msg::EditNote {
        op: lifecycle::machine::EditOp::EditText { id: note_id, seq, expected_text, new_text },
    })
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
    app.state::<lifecycle::LifecycleHandle>().request(lifecycle::machine::Msg::EditNote {
        op: lifecycle::machine::EditOp::DeleteSegment { id: note_id, seq, expected_text },
    })
}

/// 批量改派段落说话人(2026-08-22):同目标一次一批,逐段 expected_text CAS,任一
/// 失配整体失败零写入;"new" 整批共享一个新号。录制中一律拒(批量场景本就来自
/// 事后修正;live 路径的单段语义不外推)。
/// 批量删段(2026-08-23 同源双路清洗):录制中拒;逐段 CAS 任一失配整体失败。
#[tauri::command]
fn delete_segments(
    app: AppHandle,
    state: State<AppState>,
    note_id: String,
    moves: Vec<(u64, String)>,
) -> Result<(), String> {
    reject_if_active(&state, &note_id)?;
    app.state::<lifecycle::LifecycleHandle>().request(lifecycle::machine::Msg::EditNote {
        op: lifecycle::machine::EditOp::DeleteSegments { id: note_id, moves },
    })
}

#[tauri::command]
fn set_segments_speaker(
    app: AppHandle,
    state: State<AppState>,
    note_id: String,
    moves: Vec<(u64, String)>,
    speaker_id: String,
) -> Result<String, String> {
    reject_if_active(&state, &note_id)?;
    let first = moves.first().map(|(q, _)| *q);
    app.state::<lifecycle::LifecycleHandle>().request(lifecycle::machine::Msg::EditNote {
        op: lifecycle::machine::EditOp::SetSegmentsSpeaker {
            id: note_id.clone(),
            moves,
            speaker_id,
        },
    })?;
    // 与单段版同款:actor 回执收窄为 ()、终值(尤其 "new" 分配的号)靠重查取回。
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    let note = store::NoteStore::new(dir).load(&note_id).map_err(|e| e.to_string())?;
    first
        .and_then(|q| note.segments.iter().find(|s| s.seq == q))
        .and_then(|s| s.speaker.clone())
        .ok_or_else(|| tr!("改派已生效但重查失败", "Applied, but re-query failed"))
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
    if is_active_note(&state, &note_id) {
        // 录制中不开放新建说话人:"new" 分配的 id 会与 diar 注册表的 S-id 空间撞车
        // (writer 侧亦有同款拒绝,此处先拒是为给出面向用户的双语文案)。
        if speaker_id == "new" {
            return Err(tr!(
                "录制中不能新建说话人，请先停止录制",
                "Cannot create a new speaker while recording"
            ));
        }
        app.state::<lifecycle::LifecycleHandle>().request(
            lifecycle::machine::Msg::SetActiveSegmentSpeaker {
                note_id,
                seq,
                expected_text,
                speaker_id: speaker_id.clone(),
            },
        )?;
        // live 路径不分配新 id,终值即入参(冷路径靠重查取回新分配的 id)。
        return Ok(speaker_id);
    }
    app.state::<lifecycle::LifecycleHandle>().request(lifecycle::machine::Msg::EditNote {
        op: lifecycle::machine::EditOp::SetSegmentSpeaker {
            id: note_id.clone(),
            seq,
            expected_text,
            speaker_id,
        },
    })?;
    // DoEdit 的回执统一收窄成 Result<(),String>(与其余六个编辑操作同形状,见
    // actor.rs run_edit 注释),新分配的说话人 id 靠这次重查取回——actor 已把
    // 写入落盘完成才回执 Ok,重查读到的必是刚写入的最终值,不构成竞态。
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    let note = store::NoteStore::new(dir).load(&note_id).map_err(|e| e.to_string())?;
    note.segments
        .iter()
        .find(|s| s.seq == seq)
        .and_then(|s| s.speaker.clone())
        .ok_or_else(|| {
            tr!(
                "说话人写入后重查未命中该段",
                "The segment was not found when re-reading after the speaker was written"
            )
        })
}

/// 导出笔记到用户选定路径(前端保存对话框拿到 dest)。prefer_refined=真且修订稿
/// 在盘时导修订稿(所见即所得:用户看着哪个视图点导出就得到哪个),否则导原始
/// 逐字稿;修订稿导出前与 get_refined 同款只读 join,库中现名(会议搭子改名)
/// 一并带出。dest 由系统保存对话框产生,不做路径白名单(用户显式选择即授权,
/// 桌面 webview 为本地可信代码);export_to 内有兜底守卫:须绝对路径、不得落在
/// 笔记数据目录内,写入为原子替换。
#[tauri::command]
fn export_note(app: AppHandle, id: String, format: String, prefer_refined: bool, dest: String) -> Result<String, String> {
    store::validate_note_id(&id).map_err(|e| e.to_string())?;
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    let refined = if prefer_refined {
        store::load_refined_for_display(&dir.join(&id)).map(|mut doc| {
            if let (Ok(note), Ok(root)) = (store::NoteStore::new(dir.clone()).load(&id), data_root(&app)) {
                let vp = store::VoiceprintStore::new(root).load();
                store::join_note_identities(&mut doc, &note.speakers, &note.segments, &vp);
            }
            doc
        })
    } else {
        None
    };
    let dest_path = std::path::PathBuf::from(&dest);
    let result = store::NoteStore::new(dir)
        .export_to(&id, &format, refined.as_ref(), &dest_path)
        .map(|_| dest)
        .map_err(|e| e.to_string());
    if result.is_ok() {
        if let Some(fmt) = telemetry::ExportFormat::parse(&format) {
            telemetry::track(&app, telemetry::Event::NoteExported { format: fmt });
        }
    }
    result
}

/// 导出成品轨音频到用户选定路径(前端保存对话框拿到 dest)。守卫在 store 层
/// (export_audio_to):绝对路径、不落数据目录内、tmp+rename 原子替换。
#[tauri::command]
fn export_note_audio(app: AppHandle, id: String, dest: String) -> Result<String, String> {
    store::validate_note_id(&id).map_err(|e| e.to_string())?;
    let dir = notes_dir(&app).map_err(|e| e.to_string())?;
    store::NoteStore::new(dir)
        .export_audio_to(&id, std::path::Path::new(&dest))
        .map(|_| dest)
        .map_err(|e| e.to_string())
}

/// 在系统文件管理器中打开该笔记的存储目录。走 Rust 侧 opener,
/// 同 open_models_dir 先例:能直接打开目录本身,不依赖前端路径白名单。
#[tauri::command]
fn open_note_dir(app: AppHandle, id: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    store::validate_note_id(&id).map_err(|e| e.to_string())?;
    let dir = notes_dir(&app).map_err(|e| e.to_string())?.join(&id);
    if !dir.is_dir() {
        return Err(tr!(
            "笔记目录不存在: {path}",
            "Note directory does not exist: {path}",
            path = dir.display()
        ));
    }
    app.opener()
        .open_path(dir.to_string_lossy().into_owned(), None::<&str>)
        .map_err(|e| tr!("打开目录失败: {e}", "Failed to open the directory: {e}"))
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
/// 按 last_seen 降序返回（BTreeMap 原生是 P1,P10,P2… 字典序，对用户毫无意义）——
/// 侧栏索引、选人面板、合并菜单三处同源，排序统一放这里。
#[tauri::command]
fn list_people(app: AppHandle) -> Result<Vec<ipc::PersonSummary>, String> {
    let store = open_voiceprint_store(&app)?;
    let vp = store.load();
    let mut people: Vec<ipc::PersonSummary> = vp
        .people
        .iter()
        .map(|(id, p)| {
            let sample_paths = store.sample_paths_existing(id);
            // 样本录制日期 = 文件 mtime(停止录制时写入,≈该场会议时间);取不到给空串。
            let sample_dates = sample_paths
                .iter()
                .map(|p| {
                    std::fs::metadata(p)
                        .and_then(|m| m.modified())
                        .map(|t| chrono::DateTime::<chrono::Local>::from(t).to_rfc3339())
                        .unwrap_or_default()
                })
                .collect();
            ipc::PersonSummary {
                id: id.clone(),
                name: p.name.clone(),
                total_ms: p.total_ms,
                last_seen: p.last_seen.clone(),
                sources: p.centroids.keys().cloned().collect(),
                sample_paths: sample_paths.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
                sample_dates,
            }
        })
        .collect();
    people.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
    Ok(people)
}

/// 库内「无录音样本」的人数——设置页切换声纹模型前的确认提示用:这些人切换后
/// 质心会被 rebuild_for_model 清空（新模型向量空间不可比），重建完成前无法自动
/// 认出（名字与历史笔记不受影响）。只读查询，录制中也可用。
#[tauri::command]
fn count_people_without_samples(app: AppHandle) -> Result<usize, String> {
    Ok(open_voiceprint_store(&app)?.count_people_without_samples())
}

/// 声纹库**实际**所处的模型空间。设置页那个分段控件显示的是**设置值**,而重建失败时
/// 两者会长期不一致——界面显示 ERes2NetV2、库里其实还是 CAM++,声纹识别全程停用而
/// 用户一无所知(2026-08-19 定位到一台机器这样过了一个多月)。有了这个查询,
/// 设置页才能如实说出"库现在是什么"。
#[tauri::command]
fn voiceprint_library_model(app: AppHandle) -> Result<String, String> {
    Ok(open_voiceprint_store(&app)?.load().embedding_model.clone())
}

/// 手动发起一次声纹库重建。启动自愈已经会自动做这件事,这个入口是给
/// "自愈失败了想立刻重试"与"想知道现在到底在不在重建"的场景兜底。
/// 录制中拒绝:重建要现场加载嵌入器逐条嵌入样本,和录制抢 ORT 线程与 CPU。
#[tauri::command]
fn rebuild_voiceprint_library(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    if state.session.lock().map(|s| s.is_some()).unwrap_or(true) {
        return Err(tr!(
            "录制中不能重建声纹库,请先停止录制",
            "Cannot rebuild the voiceprint library while recording"
        ));
    }
    *state.embedder_cache.lock().unwrap() = None;
    spawn_voiceprint_rebuild(&app, state.embedder_cache.clone(), "手动重建");
    Ok(())
}

/// 整理·再辨认：未命名人物与库中其他人比对声纹质心，可归属者给出合并建议。
/// 纯推荐不落任何修改——确认合并由前端走既有 merge_person（含录制中拒绝等守卫）。
#[tauri::command]
fn suggest_person_merges(app: AppHandle) -> Result<Vec<ipc::PersonMergeSuggestion>, String> {
    let vp = open_voiceprint_store(&app)?.load();
    Ok(store::suggest_merges(&vp)
        .into_iter()
        .map(|s| ipc::PersonMergeSuggestion {
            loser_name: vp.people.get(&s.loser).map(|p| p.name.clone()).unwrap_or_default(),
            winner_name: vp.people.get(&s.winner).map(|p| p.name.clone()).unwrap_or_default(),
            loser: s.loser,
            winner: s.winner,
            similarity: s.similarity,
            source: s.source,
            salience: s.salience,
        })
        .collect())
}

/// 删除声纹库人物的一份录音样本（详情页试听区,录坏/混音的样本可单独删）。
/// 样本不参与识别（认人靠质心），删除不影响准确率;路径归属校验在 store 层。
#[tauri::command]
fn delete_person_sample(app: AppHandle, id: String, path: String) -> Result<(), String> {
    open_voiceprint_store(&app)?
        .delete_sample(&id, std::path::Path::new(&path))
        .map_err(|e| e.to_string())
}

/// 改库里人物的显示名：只影响后续会话的种子姓名与笔记侧只读 join，不涉及本场
/// registry 引用结构，录制中也允许（同 rename_speaker 的"改名不挡录制"哲学）。
#[tauri::command]
fn rename_person(app: AppHandle, id: String, name: String) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        // 未命名是系统态(空 name 触发展示端"未命名 · 最近出现…"兜底),不是一个可以
        // 被"改成"的普通名字;改回未命名无意义——清名走删除/合并，不走 rename。
        return Err(tr!("名字不能为空", "Name cannot be empty"));
    }
    let root = data_root(&app).map_err(|e| e.to_string())?;
    store::VoiceprintStore::new(root.clone())
        .rename(&id, name)
        .map_err(|e| e.to_string())?;
    refresh_qwen_hotwords_cache(&app);
    queue_person_graph_rebuild(&app, root, &tr!("人物改名", "Person rename"))
}

/// 从 person 出现过的最近一条笔记的音频里截取其发言(≤ 试听样本上限)。
/// 合并兜底用:loser 没有既存样本文件(样本功能上线前的老数据/历史写失败)时,
/// 把"被并入的那个声音"物化成 winner 的可试听样本——否则合并后试听列表
/// 无从体现新并入的声音(2026-07-08 用户反馈)。
/// 新→旧扫最近 MAX_NOTES 条;任何失败返回 None(样本是纯增值,不挡合并)。
fn cut_person_sample_from_notes(notes_root: &std::path::Path, person: &str) -> Option<Vec<f32>> {
    const MAX_NOTES: usize = 30;
    let mut ids: Vec<String> = std::fs::read_dir(notes_root)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect();
    ids.sort_unstable_by(|a, b| b.cmp(a)); // id 即时间戳,倒序=新在前
    let ns = store::NoteStore::new(notes_root.to_path_buf());
    for id in ids.into_iter().take(MAX_NOTES) {
        let Ok(note) = ns.load(&id) else { continue };
        // 该 person 关联的本地 speaker id 集(speakers.json 存的是入库时的 pid,原样匹配)。
        let spk_ids: std::collections::HashSet<&String> = note
            .speakers
            .iter()
            .filter(|(_, m)| m.person_id.as_deref() == Some(person))
            .map(|(k, _)| k)
            .collect();
        if spk_ids.is_empty() {
            continue;
        }
        // 按信道分组取段(时长最长优先),选发言最多的信道解一次码。
        let mut by_source: std::collections::BTreeMap<&str, Vec<&store::SegmentRecord>> =
            Default::default();
        for s in &note.segments {
            if s.speaker.as_ref().map(|x| spk_ids.contains(x)).unwrap_or(false) {
                by_source.entry(s.source.as_str()).or_default().push(s);
            }
        }
        let (source, mut segs) = by_source
            .into_iter()
            .max_by_key(|(_, v)| v.iter().map(|s| s.end_ms - s.start_ms).sum::<u64>())?;
        let note_dir = notes_root.join(&id);
        let Ok(pcm) = store::transcode::track_pcm(&note_dir, source) else { continue };
        let offset_ms =
            store::audio::load_audio_meta(&note_dir).tracks.get(source).map(|t| t.offset_ms).unwrap_or(0);
        segs.sort_unstable_by_key(|s| std::cmp::Reverse(s.end_ms - s.start_ms));
        let cap = session::SPEAKER_SAMPLE_CAP;
        let mut out: Vec<f32> = Vec::with_capacity(cap);
        for s in segs {
            if out.len() >= cap {
                break;
            }
            let a = ((s.start_ms.saturating_sub(offset_ms)) * 16) as usize;
            let b = (((s.end_ms.saturating_sub(offset_ms)) * 16) as usize).min(pcm.len());
            if a >= b {
                continue;
            }
            let take = (b - a).min(cap - out.len());
            out.extend_from_slice(&pcm[a..a + take]);
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    None
}

/// merge_person 与 apply_confident_merges 共享的合并主体:落日志→合并→loser 无
/// 样本时从笔记音频兜底截样(walk 同旧 merge_person),返回 journal id。不含录制
/// 中检查与图谱重建(调用方各自处理/批量做)。`emb` 是调用方持有的惰性单例:样本超限
/// 首次才加载声纹模型,同批后续条目复用同一实例(批量合并不再逐条秒级加载模型)。
fn do_merge_person(
    app: &AppHandle,
    loser: &str,
    winner: &str,
    origin: &str,
    similarity: Option<f32>,
    // **带标签的实例**:合并要写质心,标签必须与算这些质心的那份权重同源。此前是
    // 加载时读一次设置、几秒后落库时再读一次当空间标签,期间切完模型的话,旧嵌入器
    // 算出的向量会顶着新标签通过门禁,并据此永久删掉超额样本(codex review 实现轮四 P2)。
    emb: &mut Option<diar::TaggedEmbedder>,
) -> Result<String, String> {
    let root = data_root(app).map_err(|e| e.to_string())?;
    let store = store::VoiceprintStore::new(root.clone());
    let loser_had_samples = !store.sample_paths_existing(loser).is_empty();
    let overflow = store.sample_paths_existing(loser).len()
        + store.sample_paths_existing(winner).len()
        > store::MAX_SAMPLES;
    if overflow && emb.is_none() {
        // 标签与权重出自同一次设置读取,之后一路带着走。
        let tag = current_speaker_model(app);
        match diar::SherpaEmbedder::new(&speaker_model_path_for(&tag)) {
            Ok(e) => *emb = Some(diar::TaggedEmbedder::new(tag, Box::new(e))),
            // 加载失败不缓存"已尝试":同批后续超限条目会重试加载——模型损坏是罕见态,重试成本可接受,不为它引入毒化标记。
            Err(e) => eprintln!("合并样本挑选:声纹模型不可用,退回按序保留: {e}"),
        }
    }
    // 模型加载耗秒级:落库前最后一查,「合并中开录」的种子错配窗口收到毫秒级。
    // apply 逐条调用本函数,等价获得逐条重查。
    if app.state::<AppState>().session.lock().unwrap().is_some() {
        return Err(tr!("录制中不能合并说话人", "Cannot merge speakers while recording"));
    }
    let now = chrono::Local::now().to_rfc3339();
    // 先取标签再借出嵌入器(借用检查:后者是可变借用)。
    // 有嵌入器 → 标签取自它(要写新质心,必须同源)。没有嵌入器 → 这次不算任何向量,
    // merge_journaled 的契约要求传**库当前标签**;传设置标签的话,换模型重建还没跑完
    // 的窗口里,一次不超样本上限的普通合并会被门禁误拒(codex review 实现轮五 P2)。
    let model_tag = emb
        .as_ref()
        .map(|e| e.model().to_string())
        .unwrap_or_else(|| store.load().embedding_model.clone());
    let journal_id = store
        .merge_journaled(
            loser,
            winner,
            emb.as_mut().map(|e| e as &mut dyn diar::SpeakerEmbedder),
            origin,
            similarity,
            &now,
            // 标签取自嵌入器自身。没有嵌入器时不写质心,标签取当前选型即可
            // (merge_journaled 内部仍会与库比对)。
            &model_tag,
        )
        .map_err(|e| e.to_string())?;
    if !loser_had_samples {
        match notes_dir(app) {
            Ok(nroot) => match cut_person_sample_from_notes(&nroot, loser) {
                Some(sample) => {
                    // 兜底样本走 for_merge 变体:不触发日志失效(是合并动作的一部分)。
                    if let Err(e) = store.append_sample_for_merge(winner, &sample) {
                        eprintln!("合并兜底样本写入失败({loser}->{winner},不影响合并): {e}");
                    }
                    // 回执卡左栏"合并时的原声":同一段兜底截声也落进本次合并日志的
                    // loser 快照副本,不然左栏永远"无可试听的快照"。
                    store.write_journal_cut_sample(&journal_id, loser, &sample);
                }
                None => eprintln!("合并兜底:未能从笔记音频截到 {loser} 的样本(可能无笔记/无音频)"),
            },
            Err(e) => eprintln!("合并兜底样本跳过(notes_dir 不可用): {e}"),
        }
    }
    Ok(journal_id)
}

/// 录制中拒绝合并/删除:开录时种子已按当前库结构注入本场 registry,此刻改动库的
/// 引用关系会让"是谁"混乱——比改名危险得多,故禁止。返回合并日志 id(前端撤销条用)。
#[tauri::command]
async fn merge_person(
    app: AppHandle,
    state: State<'_, AppState>,
    loser: String,
    winner: String,
) -> Result<String, String> {
    if state.session.lock().unwrap().is_some() {
        return Err(tr!("录制中不能合并说话人", "Cannot merge speakers while recording"));
    }
    // 重活(样本超限时现场加载声纹模型+逐份嵌入挑选、loser 无样本时扫笔记截声)
    // 走 spawn_blocking,别冻主线程——同步命令在 Tauri v2 里跑在主线程,这些秒级
    // 活会冻结整个 WebView。
    tauri::async_runtime::spawn_blocking(move || {
        let mut emb = None;
        // 异步化后检查与重活之间有秒级窗口(模型加载),落库前再查一次,把
        // 「合并中开录」的种子错配窗口缩到微秒级。
        if app.state::<AppState>().session.lock().unwrap().is_some() {
            return Err(tr!("录制中不能合并说话人", "Cannot merge speakers while recording"));
        }
        let journal_id = do_merge_person(&app, &loser, &winner, "manual", None, &mut emb)?;
        refresh_qwen_hotwords_cache(&app);
        let root = data_root(&app).map_err(|e| e.to_string())?;
        queue_person_graph_rebuild(&app, root, &tr!("人物合并", "Person merge"))?;
        Ok(journal_id)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 录制中拒绝：理由同 merge_person。
/// 按样本重建人物声纹(2026-08-23 污染修复):删掉坏样本后一键从剩余样本重算
/// 质心,历史回灌污染整体清除(样本即真相)。复用拆分 baseline 的同一 store 例程;
/// REBUILD_RUNNING 互斥全库重建;隔离中的人物拒绝(拆分流程自会处理)。
#[tauri::command]
async fn rebuild_person_voiceprint(app: AppHandle, id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let root = data_root(&app).map_err(|e| e.to_string())?;
        let vp_store = store::VoiceprintStore::new(root);
        let vp = vp_store.load();
        let Some(resolved) = store::VoiceprintStore::resolve(&vp, &id).map(str::to_string) else {
            return Err(tr!("声纹库中没有该人物: {id}", "No such person: {id}", id = id));
        };
        if vp.people.get(&resolved).is_some_and(|p| p.voiceprint_quarantined) {
            return Err(tr!(
                "该人物正被拆分流程隔离,请先完成或撤销拆分",
                "Person is quarantined by a split; finish or undo it first"
            ));
        }
        {
            let _ctl = REBUILD_CTL.lock().unwrap();
            if REBUILD_RUNNING.swap(true, Ordering::SeqCst) {
                return Err(tr!("声纹库重建进行中,稍后再试", "A library rebuild is running; try again later"));
            }
        }
        let r = (|| -> Result<(), String> {
            let tag = current_speaker_model(&app);
            let mut e = diar::SherpaEmbedder::new(&speaker_model_path_for(&tag))
                .map_err(|e| tr!("声纹模型不可用: {e}", "Speaker model unavailable: {e}", e = e))?;
            vp_store.rebuild_person_from_samples(&resolved, &mut e, &tag).map_err(|e| e.to_string())
        })();
        {
            let _ctl = REBUILD_CTL.lock().unwrap();
            REBUILD_RUNNING.store(false, Ordering::SeqCst);
        }
        // 嵌入器缓存作废:质心已换血,种子须按新库重取。
        if r.is_ok() {
            let st = app.state::<AppState>();
            *st.embedder_cache.lock().unwrap() = None;
        }
        r
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn delete_person(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<(), String> {
    if state.session.lock().unwrap().is_some() {
        return Err(tr!("录制中不能删除说话人", "Cannot delete a speaker while recording"));
    }
    // 重活(store 删除+索引重建排队)走 spawn_blocking,别冻主线程。
    tauri::async_runtime::spawn_blocking(move || {
        // 异步化后检查与重活之间有秒级窗口(模型加载),落库前再查一次,把
        // 「合并中开录」的种子错配窗口缩到微秒级。
        if app.state::<AppState>().session.lock().unwrap().is_some() {
            return Err(tr!("录制中不能删除说话人", "Cannot delete a speaker while recording"));
        }
        let root = data_root(&app).map_err(|e| e.to_string())?;
        store::VoiceprintStore::new(root.clone())
            .delete(&id)
            .map_err(|e| e.to_string())?;
        refresh_qwen_hotwords_cache(&app);
        queue_person_graph_rebuild(&app, root, &tr!("人物删除", "Person deletion"))
    })
    .await
    .map_err(|e| e.to_string())?
}

fn receipt_of(journal: &store::MergeJournal, e: &store::MergeJournalEntry) -> ipc::MergeReceipt {
    ipc::MergeReceipt {
        journal_id: e.id.clone(),
        time: e.time.clone(),
        origin: e.origin.clone(),
        loser: e.loser.clone(),
        loser_name: e.loser_name.clone(),
        winner: e.winner.clone(),
        winner_name: e.winner_name.clone(),
        similarity: e.similarity,
        loser_sample_paths: journal
            .sample_copies(&e.id, "loser")
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        winner_sample_paths: journal
            .sample_copies(&e.id, "winner")
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        invalid_reason: e.invalid_reason.clone(),
    }
}

/// 整理·自动归并:strong 档且 loser 未命名且不在拒绝名单的建议逐条落日志后合并,
/// 其余留给人工。录制中不动库(建议仍只读算出返回,审阅流此时只读浏览)。单条
/// 失败不挡整批:该条降级人工,eprintln 留痕。
#[tauri::command]
async fn apply_confident_merges(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ipc::ConfidentMergeOutcome, String> {
    // 录制中检查须在进 spawn_blocking 前同步做完(State 不能进闭包);结果以 bool
    // 搬进闭包,闭包内按 live 分支——录制中仍要"只读算建议"，不落库。
    let live = state.session.lock().unwrap().is_some();
    // 重活(建议计算+样本超限时现场加载声纹模型逐条嵌入挑选、loser 无样本时扫笔记
    // 截声)走 spawn_blocking,别冻主线程。
    tauri::async_runtime::spawn_blocking(move || {
        let root = data_root(&app).map_err(|e| e.to_string())?;
        let store = store::VoiceprintStore::new(root.clone());
        let vp = store.load();
        let sugs = store::suggest_merges(&vp);
        let to_ipc = |s: &store::MergeSuggestion| ipc::PersonMergeSuggestion {
            loser_name: vp.people.get(&s.loser).map(|p| p.name.clone()).unwrap_or_default(),
            winner_name: vp.people.get(&s.winner).map(|p| p.name.clone()).unwrap_or_default(),
            loser: s.loser.clone(),
            winner: s.winner.clone(),
            similarity: s.similarity,
            source: s.source.clone(),
            salience: s.salience,
        };
        if live {
            return Ok(ipc::ConfidentMergeOutcome {
                applied: vec![],
                remaining: sugs.iter().map(to_ipc).collect(),
            });
        }
        // 异步化后 live 快照与此处之间有秒级窗口(建议计算等重活),进"落库分支"前
        // 再查一次;命中则和 live 分支同样只读返回 remaining,不报错——自动归并是
        // 后台增值行为,不该在用户开录时弹出错误。
        if app.state::<AppState>().session.lock().unwrap().is_some() {
            return Ok(ipc::ConfidentMergeOutcome {
                applied: vec![],
                remaining: sugs.iter().map(to_ipc).collect(),
            });
        }
        let journal = store::MergeJournal::new(root.clone());
        let deny = journal.auto_denylist();
        let (autos, manual) = store::confident_picks(&vp, sugs, &deny);
        let mut remaining: Vec<ipc::PersonMergeSuggestion> = manual.iter().map(to_ipc).collect();
        let mut applied = Vec::new();
        let mut merged: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut emb = None;
        for s in autos {
            // 撤销/拆回的 deny_auto 可能落在本轮快照之后:每条落库前重读名单,
            // 用户刚撤销的 pair 不许被同一轮自动归并打回(重读是小文件,廉价)。
            let deny_now = journal.auto_denylist();
            let pair = format!("{}>{}", s.loser, s.winner);
            let rev = format!("{}>{}", s.winner, s.loser);
            if deny_now.iter().any(|d| d == &pair || d == &rev) {
                remaining.push(to_ipc(&s));
                continue;
            }
            match do_merge_person(&app, &s.loser, &s.winner, "auto", Some(s.similarity), &mut emb) {
                Ok(jid) => {
                    merged.insert(s.loser.clone());
                    match journal.entry(&jid) {
                        Ok(e) => applied.push(receipt_of(&journal, &e)),
                        Err(err) => {
                            eprintln!("自动归并回执读取失败({jid}): {err}");
                            // 合并已发生,不能从响应里消失:按建议数据合成兜底回执
                            // (time 空串;list_merge_receipts 仍是真值源)。方法只要 id,
                            // 条目读不回也能列副本。
                            applied.push(ipc::MergeReceipt {
                                journal_id: jid.clone(),
                                time: String::new(),
                                origin: "auto".into(),
                                loser: s.loser.clone(),
                                loser_name: vp.people.get(&s.loser).map(|p| p.name.clone()).unwrap_or_default(),
                                winner: s.winner.clone(),
                                winner_name: vp.people.get(&s.winner).map(|p| p.name.clone()).unwrap_or_default(),
                                similarity: Some(s.similarity),
                                loser_sample_paths: journal
                                    .sample_copies(&jid, "loser")
                                    .iter()
                                    .map(|p| p.to_string_lossy().into_owned())
                                    .collect(),
                                winner_sample_paths: journal
                                    .sample_copies(&jid, "winner")
                                    .iter()
                                    .map(|p| p.to_string_lossy().into_owned())
                                    .collect(),
                                invalid_reason: None,
                            });
                        }
                    }
                }
                Err(err) => {
                    eprintln!("自动归并失败({}->{}),留给人工: {err}", s.loser, s.winner);
                    remaining.push(to_ipc(&s));
                }
            }
        }
        // 本轮已被自动合并吃掉的 id 不能再出现在人工建议里(loser 已消失,点了必错)。
        remaining.retain(|s| !merged.contains(&s.loser) && !merged.contains(&s.winner));
        if !applied.is_empty() {
            queue_person_graph_rebuild(&app, root, &tr!("自动归并", "Automatic merge"))?;
        }
        Ok(ipc::ConfidentMergeOutcome { applied, remaining })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 撤销一次合并(按日志条目)。录制中拒绝:理由同 merge_person。
#[tauri::command]
async fn undo_merge(app: AppHandle, state: State<'_, AppState>, journal_id: String) -> Result<(), String> {
    if state.session.lock().unwrap().is_some() {
        return Err(tr!("录制中不能撤销合并", "Cannot undo a merge while recording"));
    }
    // 重活(store 改写+样本副本清理+索引重建排队)走 spawn_blocking,别冻主线程。
    tauri::async_runtime::spawn_blocking(move || {
        // 异步化后检查与重活之间有秒级窗口(模型加载),落库前再查一次,把
        // 「合并中开录」的种子错配窗口缩到微秒级。
        if app.state::<AppState>().session.lock().unwrap().is_some() {
            return Err(tr!("录制中不能撤销合并", "Cannot undo a merge while recording"));
        }
        let root = data_root(&app).map_err(|e| e.to_string())?;
        let mut needs_rebuild = false;
        let r = store::VoiceprintStore::new(root.clone()).undo_merge(&journal_id, &mut needs_rebuild);
        // 质心因跨空间被置空 → 排一次重建。这里已在 vp_guard 之外(store 方法已返回)。
        // **先无条件处理,再判 r**:清空已经落盘,撤销后半程(样本、删条目)失败也不能
        // 让它跟着 Err 一起丢(codex review 实现轮三 P1)。
        if needs_rebuild {
            let st = app.state::<AppState>();
            *st.embedder_cache.lock().unwrap() = None;
            spawn_voiceprint_rebuild(&app, st.embedder_cache.clone(), "撤销合并后质心置空");
        }
        r.map_err(|e| e.to_string())?;
        refresh_qwen_hotwords_cache(&app);
        queue_person_graph_rebuild(&app, root, &tr!("撤销合并", "Merge undo"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 失效回执「拆回独立说话人」:按快照重建被并入方。录制中拒绝:理由同 merge_person。
#[tauri::command]
async fn restore_merged_person(
    app: AppHandle,
    state: State<'_, AppState>,
    journal_id: String,
) -> Result<String, String> {
    if state.session.lock().unwrap().is_some() {
        return Err(tr!("录制中不能拆回说话人", "Cannot split a speaker back out while recording"));
    }
    // 重活(按快照重建+文件拷贝+索引重建排队)走 spawn_blocking,别冻主线程。
    tauri::async_runtime::spawn_blocking(move || {
        // 异步化后检查与重活之间有秒级窗口(模型加载),落库前再查一次,把
        // 「合并中开录」的种子错配窗口缩到微秒级。
        if app.state::<AppState>().session.lock().unwrap().is_some() {
            return Err(tr!("录制中不能拆回说话人", "Cannot split a speaker back out while recording"));
        }
        let root = data_root(&app).map_err(|e| e.to_string())?;
        let mut needs_rebuild = false;
        let r = store::VoiceprintStore::new(root.clone())
            .restore_merged_person(&journal_id, &mut needs_rebuild);
        // 质心被清空了(快照来自另一个模型空间)→ 现在必须排一次重建,把这个人从
        // 样本重新长出来。**放在这里而不是 store 里**:store 那边还持着 vp_guard,
        // 重建自己也要取它。走 spawn_voiceprint_rebuild(空闲即启动、忙则排队),
        // 不能直接置 REBUILD_PENDING——那不是队列,空闲时置位不会启动任何东西。
        // **先无条件处理,再判 r**:同 undo_merge——清空已经落盘,后半程失败也不能
        // 让重建需求跟着 Err 丢掉(codex review 实现轮三 P1)。
        if needs_rebuild {
            let st = app.state::<AppState>();
            *st.embedder_cache.lock().unwrap() = None;
            spawn_voiceprint_rebuild(&app, st.embedder_cache.clone(), "拆回后质心置空");
        }
        let pid = r.map_err(|e| e.to_string())?;
        refresh_qwen_hotwords_cache(&app);
        queue_person_graph_rebuild(&app, root, &tr!("拆回说话人", "Speaker split-back"))?;
        Ok(pid)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 回执卡「好」:确认自动归并,条目(连同样本副本)删除。
#[tauri::command]
async fn acknowledge_merge(app: AppHandle, journal_id: String) -> Result<(), String> {
    // 重活(样本副本文件删除)走 spawn_blocking,别冻主线程。
    tauri::async_runtime::spawn_blocking(move || {
        let root = data_root(&app).map_err(|e| e.to_string())?;
        store::VoiceprintStore::new(root).acknowledge_merge(&journal_id).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 未确认的合并回执(审阅流回执卡数据;含已失效的——卡上撤销钮变灰注明原因)。
/// manual 条目生来已确认,天然不在其中。
#[tauri::command]
fn list_merge_receipts(app: AppHandle) -> Result<Vec<ipc::MergeReceipt>, String> {
    let root = data_root(&app).map_err(|e| e.to_string())?;
    let journal = store::MergeJournal::new(root);
    Ok(journal
        .entries()
        .iter()
        .filter(|e| !e.acknowledged)
        .map(|e| receipt_of(&journal, e))
        .collect())
}

/// 整理条目人工处置(忽略/保留)落盘:重启后不再出现。键格式由前端定义。
#[tauri::command]
fn dismiss_tidy_item(app: AppHandle, key: String) -> Result<(), String> {
    let root = data_root(&app).map_err(|e| e.to_string())?;
    store::MergeJournal::new(root).dismiss_item(&key);
    Ok(())
}

#[tauri::command]
fn list_dismissed_tidy_items(app: AppHandle) -> Result<Vec<String>, String> {
    let root = data_root(&app).map_err(|e| e.to_string())?;
    Ok(store::MergeJournal::new(root).dismissed_items())
}

// —— MCP 注册(设置页/欢迎页消费;registry 真值源是各 Agent 配置文件) ——

#[derive(serde::Serialize)]
struct RegisterOutcome {
    key: String,
    ok: bool,
    error: Option<String>,
}

/// 启动自愈修复的条目数,设置页读一次并展示提示条。AtomicU32 而非事件:setup 时
/// 前端尚未挂监听,事件会丢。
static MCP_HEALED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[tauri::command]
async fn mcp_agents_status() -> Result<Vec<mcp::registry::AgentStatus>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        Ok(mcp::registry::Registry::new().map_err(|e| e.to_string())?.status())
    })
    .await
    .map_err(|e| tr!("Agent 状态后台任务异常: {e}", "Agent status background task failed: {e}"))?
}

#[tauri::command]
fn mcp_register(agents: Vec<String>) -> Result<Vec<RegisterOutcome>, String> {
    let reg = mcp::registry::Registry::new().map_err(|e| e.to_string())?;
    Ok(agents
        .into_iter()
        .map(|key| match reg.register(&key) {
            Ok(()) => RegisterOutcome { key, ok: true, error: None },
            Err(e) => RegisterOutcome { key, ok: false, error: Some(e.to_string()) },
        })
        .collect())
}

#[tauri::command]
fn mcp_unregister(agent: String) -> Result<(), String> {
    mcp::registry::Registry::new().map_err(|e| e.to_string())?.unregister(&agent).map_err(|e| e.to_string())
}

#[tauri::command]
async fn mcp_manual_snippet() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(|| {
        Ok(mcp::registry::Registry::new().map_err(|e| e.to_string())?.entry_snippet_json())
    })
    .await
    .map_err(|e| tr!("MCP 配置读取后台任务异常: {e}", "MCP config read background task failed: {e}"))?
}

#[tauri::command]
fn mcp_healed_count() -> u32 {
    MCP_HEALED.swap(0, Ordering::SeqCst) // 读即清:提示只出一次
}

fn skill_state_str(state: mcp::skill::SkillState) -> &'static str {
    use mcp::skill::SkillState::*;
    match state {
        NotInstalled => "not_installed",
        Current => "current",
        Stale => "stale",
        Unmanaged => "unmanaged",
    }
}

#[tauri::command]
async fn mcp_skill_status() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(|| {
        Ok(skill_state_str(mcp::skill::status().map_err(|e| e.to_string())?).into())
    })
    .await
    .map_err(|e| tr!("Skill 状态后台任务异常: {e}", "Skill status background task failed: {e}"))?
}

#[tauri::command]
fn mcp_skill_install() -> Result<(), String> {
    mcp::skill::install().map_err(|e| e.to_string())
}

#[tauri::command]
fn mcp_skill_uninstall() -> Result<(), String> {
    mcp::skill::uninstall().map_err(|e| e.to_string())
}

/// `/ai` 页的静态能力清单(MCP 工具 + CLI 命令),纯数据、不依赖 App 运行状态。
#[tauri::command]
fn mcp_capabilities() -> serde_json::Value {
    mcp::server::catalog()
}

/// 四家 Agent CLI 的本机探测结果(key → 解析到的可执行路径或 null),供 /ai 页
/// Agent Aing 模式展示「已检测到/未检测到」。探测只做文件存在性检查,毫秒级。
#[tauri::command]
async fn refine_agents_probe() -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(|| {
        refine::agent::probe_all()
            .into_iter()
            .map(|(k, p)| (k.to_string(), serde_json::json!(p)))
            .collect::<serde_json::Map<_, _>>()
            .into()
    })
    .await
    .map_err(|e| tr!("Agent 探测后台任务异常: {e}", "Agent probe background task failed: {e}"))
}

/// AI 调用日志查询(倒序分页,过滤条件见 ailog::Filter)。
#[tauri::command]
async fn ai_logs_query(app: AppHandle, filter: ailog::Filter) -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let root = data_root(&app).map_err(|e| e.to_string())?;
        Ok(ailog::query(&root, &filter))
    })
    .await
    .map_err(|e| tr!("AI 日志查询后台任务异常: {e}", "AI log query background task failed: {e}"))?
}

/// AI 调用日志全量导出为 JSONL,返回文件路径(写 ai_logs/ 目录,与笔记导出同一
/// 「写数据目录、把路径给用户」约定)。
#[tauri::command]
async fn ai_logs_export(app: AppHandle) -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let root = data_root(&app).map_err(|e| e.to_string())?;
        let (path, count) = ailog::export_jsonl(&root, None).map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "path": path.to_string_lossy(), "count": count }))
    })
    .await
    .map_err(|e| tr!("AI 日志导出后台任务异常: {e}", "AI log export background task failed: {e}"))?
}

/// 在访达中打开 AI 日志目录(macOS `open`;目录不存在先建,空目录也可打开)。
#[tauri::command]
fn ai_logs_open_dir(app: AppHandle) -> Result<String, String> {
    let root = data_root(&app).map_err(|e| e.to_string())?;
    let dir = ailog::log_dir(&root);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(windows)]
    let mut command = std::process::Command::new("explorer.exe");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = std::process::Command::new("xdg-open");
    command.arg(&dir).spawn().map_err(|e| e.to_string())?;
    Ok(dir.to_string_lossy().into_owned())
}

#[derive(serde::Serialize)]
struct SkillRead {
    content: String,
    state: String,
}

#[tauri::command]
fn mcp_skill_read() -> Result<SkillRead, String> {
    let (content, state) = mcp::skill::read().map_err(|e| e.to_string())?;
    Ok(SkillRead { content, state: skill_state_str(state).into() })
}

/// 保存 = 编辑即接管:落盘后受管标记已被剥离,状态自然变 Unmanaged。
#[tauri::command]
fn mcp_skill_save(content: String) -> Result<(), String> {
    mcp::skill::save(&content).map_err(|e| e.to_string())
}

/// 后台预载识别器与声纹嵌入器进常驻槽（幂等：槽已有则跳过）。
/// 锁序：预载是唯一嵌套持两槽者——持 recognizer 槽锁期间嵌套获取 embedder 槽锁，
/// 消除间隙内开录线程 take 到空 embedder 的静默降级（详见原 setup 注释）。
fn preload_models(
    app: AppHandle,
    session: Arc<Mutex<Option<ActiveSession>>>,
    cache: Arc<Mutex<Option<Box<dyn asr::Recognizer>>>>,
    embedder_cache: Arc<Mutex<Option<Box<diar::TaggedEmbedder>>>>,
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
        // 云端模式不预载本机识别器:识别在厂商侧,加载一份几 GB 的模型纯属白占内存
        // (开录也不会取用它)。声纹嵌入器照常预载——声纹是本机能力,与识别在哪跑无关。
        let cloud_mode = app
            .path()
            .app_data_dir()
            .map(|d| settings::load(&d).asr_mode == settings::ASR_MODE_CLOUD)
            .unwrap_or(false);
        let mut slot = cache.lock().unwrap();
        if slot.is_none() && !cloud_mode {
            match new_recognizer(&asr_model, current_asr_provider(&app), qwen3_hotwords(&app)) {
                Ok(r) => *slot = Some(r),
                Err(e) => {
                    eprintln!("识别器预载失败（将在开录时现场加载）: {e}");
                    // 与 AsrEngine 分开:加载失败是装机期问题(模型没下全/文件损坏),
                    // 引擎异常是运行期问题,合成一档会把两类故障混进同一个 issue。
                    telemetry::report_error(
                        telemetry::ErrorKind::ModelLoad,
                        &format!("识别器预载失败: {e}"),
                    );
                }
            }
        }
        let mut eslot = embedder_cache.lock().unwrap();
        if eslot.is_none() {
            // 标签与权重路径必须出自**同一次**设置读取。分两次读的话,用户在两次之间
            // 切了模型就会造出"A 权重、B 标签"的实例;它被 B 会话取走后,写侧门禁看
            // 标签放行,A 空间的向量就进了 B 库(codex review 实现轮二 P1)。
            let tag = current_speaker_model(&app);
            match diar::SherpaEmbedder::new(&speaker_model_path_for(&tag)) {
                Ok(e) => *eslot = Some(Box::new(diar::TaggedEmbedder::new(tag, Box::new(e)))),
                Err(e) => {
                    eprintln!("声纹模型预载失败（说话人区分将不可用）: {e}");
                    telemetry::report_error(
                        telemetry::ErrorKind::ModelLoad,
                        &format!("声纹模型预载失败: {e}"),
                    );
                }
            }
        }
        drop(eslot);
        drop(slot);
    });
}

/// 当前设置下的模型就绪快照(模式感知)。设置页、托盘菜单、开录/续录守卫共用同一份
/// 判定——云端模式下"就绪"= vad 在 + 凭证齐(本地大模型全不必需),本地模式与旧行为
/// 逐位等价。三处若各写一份必然分叉:例如托盘按本地缺件把「开始录制」灰掉,而云端
/// 用户根本不需要那些件。app_data_dir 不可用 → Settings::default(本地模式),与
/// current_asr 的兜底纪律一致。
pub(crate) fn current_models_status(app: &AppHandle) -> models::ModelsStatus {
    let s = app.path().app_data_dir().map(|d| settings::load(&d)).unwrap_or_default();
    models::status_for(
        &s.asr_model,
        s.asr_mode == settings::ASR_MODE_CLOUD,
        settings::cloud_creds_ok(&s),
    )
}

/// 开录被就绪判定挡下时的文案。云端模式下"缺件"多半是凭证没填(vad 随包下过),
/// 沿用本地文案会让人跑去下载页找一个根本不需要的模型。
pub(crate) fn recording_not_ready_msg(app: &AppHandle) -> String {
    let s = app.path().app_data_dir().map(|d| settings::load(&d)).unwrap_or_default();
    if s.asr_mode == settings::ASR_MODE_CLOUD && !settings::cloud_creds_ok(&s) {
        tr!(
            "请先在设置中配置云端凭证",
            "Please configure the cloud credentials in Settings first"
        )
    } else {
        tr!(
            "模型缺失：请先在设置页下载所选识别模型",
            "Model missing: please download the selected recognition model in Settings"
        )
    }
}

#[tauri::command]
fn models_status(app: AppHandle) -> models::ModelsStatus {
    current_models_status(&app)
}

/// 在系统文件管理器中打开模型存储目录(设置页「语音模型」区路径点击)。
/// 走 Rust 侧 opener:能直接打开目录本身,且不依赖前端 opener 权限的路径白名单。
#[tauri::command]
fn open_models_dir(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let dir = models::root();
    if !dir.is_dir() {
        return Err(tr!(
            "模型目录不存在: {path}",
            "Model directory does not exist: {path}",
            path = dir.display()
        ));
    }
    app.opener()
        .open_path(dir.to_string_lossy().into_owned(), None::<&str>)
        .map_err(|e| tr!("打开目录失败: {e}", "Failed to open the directory: {e}"))
}

/// 下载单个工件:按 download_urls 的候选顺序尝试。代理候选各试 1 次(死代理快速跳过,
/// 压回退延迟),原站(候选列表最后一项,== a.url)给足 DOWNLOAD_ATTEMPTS_PER_URL 次。
/// 返回 Err(msg):msg=="cancelled" 表示被取消,其余为可展示错误文案。
fn download_one(
    a: &models::Artifact,
    root: &std::path::Path,
    mirror_enabled: bool,
    mirror_prefix: &str,
    cancel: &std::sync::atomic::AtomicBool,
    emit: &(impl Fn(&str, &str, u64, u64, &str) + 'static),
) -> Result<(), String> {
    let urls = models::download::download_urls(a.url, mirror_enabled, mirror_prefix);
    let mut last_err: Option<String> = None;
    for url in &urls {
        // 原站(无前缀,恒等于 a.url)多重试;代理候选各 1 次快速跳过。
        let attempts = if url == a.url { DOWNLOAD_ATTEMPTS_PER_URL } else { 1 };
        for attempt in 1..=attempts {
            match models::download::download_artifact(a, root, url, cancel, emit) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let msg = e.to_string();
                    if msg == "cancelled" {
                        return Err("cancelled".into());
                    }
                    let retryable = models::download::retryable_download_error(&msg);
                    last_err = Some(format!("{url}: {msg}"));
                    if !retryable || attempt == attempts {
                        break; // 换下一个候选 URL
                    }
                }
            }
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("cancelled".into());
            }
        }
    }
    Err(last_err.unwrap_or_else(|| tr!("下载失败", "Download failed")))
}

#[tauri::command]
fn download_models(app: AppHandle, state: State<AppState>, ids: Option<Vec<String>>) -> Result<(), String> {
    if state.download_running.swap(true, Ordering::SeqCst) {
        return Err("下载已在进行中".into()); // i18n-exempt: 前端按原文判等
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
        // preload 需要 app,但 app 随即被 worker 闭包 clone 走,先克隆留给补预载与 done 事件。
        let app_pl = app.clone();
        let app_done = app.clone();
        let mirror_enabled = s.mirror_enabled;
        let items: Vec<&models::Artifact> = selected; // ARTIFACTS 原顺序,进度/展示稳定
        let next = std::sync::atomic::AtomicUsize::new(0);
        let all_ok = std::sync::atomic::AtomicBool::new(true);
        let worker_count = items.len().min(MAX_CONCURRENT_DOWNLOADS).max(1);
        // scope:worker 借用 items/next/all_ok/cancel/root,块结束自动 join,无需 Arc。
        std::thread::scope(|scope| {
            for _ in 0..worker_count {
                let app_w = app.clone();
                let cancel = &cancel;
                let next = &next;
                let all_ok = &all_ok;
                let root = &root;
                let items = &items;
                scope.spawn(move || {
                    let emit = move |id: &str, phase: &str, received: u64, total: u64, message: &str| {
                        let _ = app_w.emit(
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
                    loop {
                        if cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        let i = next.fetch_add(1, Ordering::SeqCst);
                        if i >= items.len() {
                            break;
                        }
                        let a = items[i];
                        if models::artifact_present(root, a) {
                            continue;
                        }
                        match download_one(a, root, mirror_enabled, settings::MIRROR_PREFIX, cancel, &emit) {
                            Ok(()) => {}
                            Err(msg) if msg == "cancelled" => {
                                emit(a.id, "cancelled", 0, 0, "cancelled");
                                all_ok.store(false, Ordering::SeqCst);
                                break; // 取消:本 worker 停止取新工件
                            }
                            Err(msg) => {
                                // 失败隔离:标记整体失败,但继续下载其余工件(不再连带中断)。
                                emit(a.id, "error", 0, 0, &msg);
                                all_ok.store(false, Ordering::SeqCst);
                            }
                        }
                    }
                });
            }
        });
        drop(guard); // 复位先于 done 事件,保持"收到 done 即可再次下载"的时序
        if all_ok.load(Ordering::SeqCst) {
            let _ = app_done.emit(
                "model_download",
                ipc::ModelDownloadEvent {
                    artifact: "all".into(),
                    phase: "done".into(),
                    received_bytes: 0,
                    total_bytes: 0,
                    message: String::new(),
                },
            );
            // 补齐后立即预载,无需重启即可开录。
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
        return Err(tr!("录制中不能删除模型", "Cannot delete a model while recording"));
    }
    if state.download_running.load(Ordering::SeqCst) {
        return Err(tr!("下载进行中，稍后再试", "A download is in progress; try again later"));
    }
    let a = models::ARTIFACTS
        .iter()
        .find(|a| a.id == id)
        .ok_or_else(|| tr!("未知模型: {id}", "Unknown model: {id}"))?;
    let root = models::root();
    match &a.kind {
        models::ArtifactKind::File => {
            let p = root.join(a.files[0].rel_path);
            if p.exists() {
                std::fs::remove_file(&p).map_err(|e| tr!("删除失败: {e}", "Delete failed: {e}"))?;
            }
        }
        models::ArtifactKind::TarBz2 { dest_dir } => {
            let p = root.join(dest_dir);
            if p.exists() {
                std::fs::remove_dir_all(&p).map_err(|e| tr!("删除失败: {e}", "Delete failed: {e}"))?;
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

/// 测试连接等 Closed 的上限。握手本身是同步的(open_stream 返回即已鉴权),这里等的
/// 只是「推完静音 → finish → 厂商回关闭」的一个往返;5s 足够,超时即判网络异常。
const CLOUD_TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 设置页/欢迎页「测试连接」:直接使用表单当前值造适配器 → 开流(同步握手,鉴权/
/// 网络问题在此暴露) → 推 200ms 静音 → finish → 等 Closed。表单值作为命令参数
/// 传入,不依赖输入框 blur 先异步写 settings.json,也不会为了测试而持久化无效凭证。
/// error=None 即通,Some 原样透出厂商说法(「鉴权失败」这类文案由适配层给,这里不猜)。
///
/// 阻塞至多「握手 ≤7s(阿里云 CONNECT_TIMEOUT 6s + 1s 缓冲) + 等 Closed ≤ CLOUD_TEST_TIMEOUT
/// 5s」,worst case 逼近 12s:走 spawn_blocking 别占 IPC 线程,同 test_refine_llm 惯例——
/// 用户主动触发的一次点击,阻塞线程池里的一条工作线程可接受,前端一次 invoke 就拿到结论。
#[tauri::command]
async fn test_cloud_asr(
    state: State<'_, AppState>,
    provider: String,
    volc_app_key: String,
    volc_access_key: String,
    dashscope_api_key: String,
) -> Result<String, String> {
    // 录制中再开一条厂商流会挤占并发额度(多数厂商按账号限并发路数),拒绝而非静默抢占。
    if *state.running.lock().unwrap() {
        return Err(tr!(
            "录制中不能测试云端连接",
            "Cannot test the cloud connection while recording"
        ));
    }
    tauri::async_runtime::spawn_blocking(move || {
        let s = settings::Settings {
            cloud_asr_provider: provider,
            volc_app_key,
            volc_access_key,
            dashscope_api_key,
            ..Default::default()
        };
        let cloud = make_cloud_asr(&s).map_err(|e| e.to_string())?;
        let mut stream = cloud
            .open_stream()
            .map_err(|e| tr!("连接失败: {e}", "Connection failed: {e}"))?;
        // 200ms 静音:有些厂商在收到首个音频包前不会走完会话建立,空推一段最接近真实录制。
        (stream.push)(&vec![0.0f32; 16000 / 5])
            .map_err(|e| tr!("推流失败: {e}", "Failed to push audio: {e}"))?;
        (stream.finish)().map_err(|e| tr!("收尾失败: {e}", "Failed to finish the stream: {e}"))?;
        let deadline = std::time::Instant::now() + CLOUD_TEST_TIMEOUT;
        loop {
            let left = deadline
                .checked_duration_since(std::time::Instant::now())
                .ok_or_else(|| {
                    tr!(
                        "连接超时:请检查网络或凭证",
                        "Connection timed out: check your network or credentials"
                    )
                })?;
            match stream.events.recv_timeout(left) {
                Ok(asr::cloud::CloudEvent::Closed { error: None }) => {
                    return Ok(tr!(
                        "连接成功({label})",
                        "Connected successfully ({label})",
                        label = cloud_provider_label(&s.cloud_asr_provider)
                    ))
                }
                Ok(asr::cloud::CloudEvent::Closed { error: Some(e) }) => return Err(e),
                // 中途的预览/定稿(静音也可能吐空定稿)不是结论,继续等关闭。
                Ok(_) => continue,
                // 通道断开却没给 Closed:适配层线程异常退出,当失败报。
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    return Err(tr!(
                        "连接异常中断:请检查网络或凭证",
                        "Connection dropped unexpectedly: check your network or credentials"
                    ))
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    return Err(tr!(
                        "连接超时:请检查网络或凭证",
                        "Connection timed out: check your network or credentials"
                    ))
                }
            }
        }
    })
    .await
    .map_err(|e| tr!("执行线程失败: {e}", "Worker thread failed: {e}"))?
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
        return Err(tr!(
            "存储目录变更请使用迁移功能",
            "Use the migration feature to change the storage directory"
        ));
    }
    // ASR 选型变更:录制中切换会让常驻识别器与正在转写的会话对不上,拒绝;无会话则
    // save 后清掉旧选型的常驻识别器、按新选型重载,无需重启即可用新模型开录。
    // 查 running 而非 session 槽:同 delete_model,开录命令返回后 session 槽尚空的加载
    // 窗口里切型会与即将取用的常驻识别器对不上。statement-scoped。
    let asr_changed = old.asr_model != new_settings.asr_model;
    if asr_changed && *state.running.lock().unwrap() {
        return Err(tr!(
            "录制中不能切换识别模型",
            "Cannot switch the recognition model while recording"
        ));
    }
    // 热词变更(codex 2026-08-11 P2):常驻识别器在预载时已把 hotwords 焊进 prompt,
    // 只改设置不清缓存的话,下一场录音仍拿旧词表。与 asr_changed 同路径:清槽+重预载。
    // 录制中同样拒绝——停录 stash 会把持旧词表的会话识别器塞回缓存槽,清槽白清
    // (前端热词输入本就 recording.isLive 时禁用,这里是后端兜底)。
    let hotwords_changed = old.asr_hotwords != new_settings.asr_hotwords;
    if hotwords_changed && *state.running.lock().unwrap() {
        return Err(tr!(
            "录制中不能修改热词",
            "Cannot change hotwords while recording"
        ));
    }
    // 识别方式(本地/云端)与云端凭证变更:与 ASR 选型同理,录制中改会让正在跑的会话与
    // 设置对不上(云端流已按旧凭证握手、本地 worker 已持旧识别器),拒绝。凭证也在内:
    // 改 key 不会重开流,却会让下一次重连/补识用上另一套账号,静默分裂到两个厂商账号下。
    let mode_changed = old.asr_mode != new_settings.asr_mode
        || old.cloud_asr_provider != new_settings.cloud_asr_provider;
    let creds_changed = old.volc_app_key != new_settings.volc_app_key
        || old.volc_access_key != new_settings.volc_access_key
        || old.dashscope_api_key != new_settings.dashscope_api_key;
    if (mode_changed || creds_changed) && *state.running.lock().unwrap() {
        return Err(tr!(
            "录制中不能切换识别方式",
            "Cannot switch the recognition mode while recording"
        ));
    }
    // 声纹模型切换:录制中拒绝(与 ASR 同理);保存后清旧嵌入器缓存,并起后台线程
    // 用新模型从录音样本重建整库质心(不同模型空间不可混用)。重建期间录制可用,
    // 只是种子注入被门禁跳过(不自动认人),完成后自动恢复。
    let speaker_changed = old.speaker_model != new_settings.speaker_model;
    if speaker_changed && *state.running.lock().unwrap() {
        return Err(tr!(
            "录制中不能切换声纹模型",
            "Cannot switch the voiceprint model while recording"
        ));
    }
    // 托盘开关是否变更(落盘后据此建/拆托盘,即时生效无需重启)。
    let tray_changed = old.tray_enabled != new_settings.tray_enabled;
    // UI 语言变更:落盘后切全局语言并重建托盘菜单文案(new_settings 即将 move 进闭包,先取值)。
    let lang_changed = old.ui_lang != new_settings.ui_lang;
    let new_ui_lang = new_settings.ui_lang.clone();
    // 上报总开关(new_settings 即将 move,先取值)。
    let telemetry_on = new_settings.telemetry_enabled;
    let new_speaker_model = new_settings.speaker_model.clone();
    // AI 从"未配置"变成"已配置"——设计文档漏斗 3 的关键一步,它预期流失率最高,
    // 是本期最想验证的假设。只在跨越那一次上报,之后每次保存设置都不再重复计数。
    let ai_newly_configured = active_refine_executor(&old)
        .is_none()
        .then(|| active_refine_executor(&new_settings))
        .flatten()
        .map(|e| telemetry_provider(&e));
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
    if asr_changed || hotwords_changed {
        *state.recognizer_cache.lock().unwrap() = None;
        preload_models(app.clone(), state.session.clone(), state.recognizer_cache.clone(), state.embedder_cache.clone());
    } else if mode_changed {
        // 识别方式切换(本地↔云端 / 换厂商):无条件清掉常驻识别器——
        //  - 切到云端:那份几 GB 的模型此后没人取用,留着白占内存,清掉即释放;
        //  - 切回本地:预载按新设置重载(preload 内部已按模式判定是否真的加载),
        //    与 asr_changed 后的节奏一致,无需重启即可开录。
        // 换厂商不涉及本机识别器,清空是空操作(槽本就为空),沿用同一条路径不分叉。
        *state.recognizer_cache.lock().unwrap() = None;
        preload_models(app.clone(), state.session.clone(), state.recognizer_cache.clone(), state.embedder_cache.clone());
    }
    // 托盘开关变更 → 建/拆托盘（apply_enabled 现读设置后幂等处理）。放在 asr 之后,
    // app 已 clone 给 preload,此处直接用 &app。
    if speaker_changed {
        *state.embedder_cache.lock().unwrap() = None; // 旧模型常驻嵌入器作废
        // 切回库本来所在的空间**不需要重建**:库里的向量就是这个模型算的,标签也已经
        // 对上,重建纯属白跑半分钟,期间还把常驻槽空着(用户紧接着开会就整场没有说话人
        // 区分)。设置页那句「切回 X 无需重建」正是这么承诺的,后端必须真的这么做
        // (codex review 二轮 P2)。
        let lib_model = data_root(&app)
            .map(|r| store::VoiceprintStore::new(r).load().embedding_model.clone())
            .unwrap_or_default();
        if lib_model == new_speaker_model {
            eprintln!("声纹库已在 {new_speaker_model} 空间,切回无需重建");
            preload_models(app.clone(), state.session.clone(), state.recognizer_cache.clone(), state.embedder_cache.clone());
        } else {
            spawn_voiceprint_rebuild(&app, state.embedder_cache.clone(), "换模型");
        }
    }
    if tray_changed {
        tray::apply_enabled(&app);
    }
    // 总开关放在落盘之后:先保证用户的选择已经存住,再让它生效。关掉的那一刻
    // set_enabled 会连压队一起清空(见 telemetry::set_enabled)。
    telemetry::set_enabled(telemetry_on);
    if let Some(provider) = ai_newly_configured {
        telemetry::track(&app, telemetry::Event::AiConfigured { provider });
    }
    if lang_changed {
        i18n::set_lang(&new_ui_lang);
        // 菜单标签按新语言重建(set_recording 即整体重建路径);running statement-scoped。
        let running = *state.running.lock().unwrap();
        tray::set_recording(&app, running);
    }
    Ok(())
}

/// 钩子配置读取(独立 hooks.json,不掺和 settings)。用 load_checked 而非
/// load:损坏时必须如实回 Err,让 Sidebar 的 hooksError 横幅、编辑页的
/// loadError 点亮;同时编辑页 save 流程(先 listHooks 读旧配置、改、再
/// saveHooks 整表写回)会因这里抛错而在第一步就中止,不会拿着「损坏当空表」
/// 的假象把用户手编但只是格式有误的原文件静默覆盖。
#[tauri::command]
fn list_hooks(app: AppHandle) -> Result<Vec<hooks_external::HookCfg>, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(hooks_external::load_checked(&dir)?.hooks)
}

/// 整表覆盖保存:前端是唯一写者,配置量小,不做逐条 CRUD。
#[tauri::command]
fn save_hooks(app: AppHandle, hooks: Vec<hooks_external::HookCfg>) -> Result<(), String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    hooks_external::save(&dir, &hooks_external::HooksFile { hooks }).map_err(|e| e.to_string())
}

/// 配置页「测试」:同步执行体最长 10s,走 spawn_blocking 别占 IPC 线程。
#[tauri::command]
async fn test_hook(cfg: hooks_external::HookCfg) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || hooks_external::test_run(&cfg))
        .await
        .map_err(|e| tr!("执行线程失败: {e}", "Worker thread failed: {e}"))?
}

/// 配置页「测试连接」:发一条最小 chat/completions 验证大模型 Aing 配置。
#[tauri::command]
async fn test_refine_llm(base_url: String, model: String, api_key: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        refine::llm::probe(&refine::llm::LlmConfig { base_url, model, api_key })
    })
    .await
    .map_err(|e| tr!("执行线程失败: {e}", "Worker thread failed: {e}"))?
}

/// 配置页「测试运行」:用配好的 Agent CLI 跑一句极短提示验证可用。
#[tauri::command]
async fn test_refine_agent(provider: String, bin: String, model: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || refine::agent::probe_run(&provider, &bin, &model))
        .await
        .map_err(|e| tr!("执行线程失败: {e}", "Worker thread failed: {e}"))?
}

/// 设置页「测试」镜像:经镜像前缀探一个已知资源验证可达。
#[tauri::command]
async fn test_mirror(prefix: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || models::download::probe_mirror(&prefix))
        .await
        .map_err(|e| tr!("执行线程失败: {e}", "Worker thread failed: {e}"))?
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
///
/// Fix 1(codex 第二轮,双向互锁的迁移侧):额外查 `retranscribing` 槽——重转写 worker
/// 从占槽(do_retranscribe 末尾)到清槽(spawn_retranscribe 线程收尾)全程持有该槽,
/// 期间它正在离线读盘上的音轨、终态时还要写 segments/speakers。若此刻放行迁移,
/// 会把 worker 正在读写的旧路径搬走甚至删掉。与 do_retranscribe 侧新增的
/// download_running 检查互为镜像:那边是"重转写起跑前拒绝迁移中"，这里是
/// "迁移起跑前拒绝重转写中"——两侧互查对方状态,worker 存续期内迁移必被拒。
fn migrate_guard(
    running: &Arc<Mutex<bool>>,
    download_running: &Arc<AtomicBool>,
    retranscribing: &Arc<Mutex<Option<(String, String)>>>,
    mixed_regen: &Arc<Mutex<Option<String>>>,
) -> Result<(), String> {
    // 先抢互斥位(与 start 的 download 检查对称)。
    if download_running.swap(true, Ordering::SeqCst) {
        return Err(tr!("迁移或下载进行中", "A migration or download is in progress"));
    }
    // 再查录制中;拒绝时必须复位刚抢下的互斥位,否则迁移互斥位永久卡死。
    if *running.lock().unwrap() {
        download_running.store(false, Ordering::SeqCst);
        return Err(tr!("录制中不能迁移", "Cannot migrate while recording"));
    }
    // 再查重转写槽;同样必须复位互斥位。poison 只可能因锁内 panic 产生,槽是纯数据,
    // 中毒后继续读最后一次写入的值好过让迁移永久拒绝。
    if retranscribing.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
        download_running.store(false, Ordering::SeqCst);
        return Err(tr!(
            "重转写进行中,完成后再迁移",
            "A re-transcription is in progress; please migrate after it finishes"
        ));
    }
    // 补生成侧(codex 第二轮 P1):regen worker 从占槽到清槽全程 mmap 读源轨并写
    // 笔记目录,迁移此刻搬根会把映射中的文件移走/删掉。与 do_regenerate_mixed 的
    // download_running 复查互为镜像,写后读闭环同上方重转写侧。
    if mixed_regen_busy(mixed_regen) {
        download_running.store(false, Ordering::SeqCst);
        return Err(tr!(
            "正在补生成成品轨,完成后再迁移",
            "Mixed-track regeneration is in progress; please migrate after it finishes"
        ));
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
    migrate_guard(&state.running, &state.download_running, &state.retranscribing, &state.mixed_regen)?;
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
            // 用户点了迁移却没迁成:本机日志之外无人知晓,而这是最容易让人以为
            // "笔记丢了"的一类故障。
            telemetry::report_error(telemetry::ErrorKind::Migration, &msg);
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
            return emit_err(
                &app,
                tr!(
                    "保存设置失败,迁移已回滚: {e}",
                    "Failed to save settings; the migration was rolled back: {e}"
                ),
            );
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
            return Err(tr!(
                "VN_MODELS 环境变量生效中,请先移除再迁移",
                "The VN_MODELS environment variable is in effect; remove it before migrating"
            ));
        }
    }
    let new_path = PathBuf::from(&new_dir);
    store::migrate::dir_is_usable_target(&new_path).map_err(|e| e.to_string())?;
    let old_root = models::root();
    // 嵌套守卫同 data:目标与当前模型根互不包含。以上皆只读检查,失败无需复位。
    store::migrate::ensure_disjoint(&old_root, &new_path).map_err(|e| e.to_string())?;
    // 抢迁移/下载互斥位 + 录制守卫(先 swap download 再查 running,见 migrate_guard)。
    migrate_guard(&state.running, &state.download_running, &state.retranscribing, &state.mixed_regen)?;
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
            return emit_err(
                &app,
                tr!(
                    "保存设置失败,迁移已回滚: {e}",
                    "Failed to save settings; the migration was rolled back: {e}"
                ),
            );
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
///
/// 清理本体,`purge_audio` 命令与启动期音频保留期自动清理(见 setup 内
/// `tauri::async_runtime::spawn`)共用同一实现,防两处漂移。`older_than_days` 为
/// `None` 时清理全部已完成笔记的音频(命令原语义,前端传 `null` 触发);`Some(d)`
/// 时只清理 `d` 天前的。活动笔记豁免(上文两段注释所述的 session 槽比对 + `migrate_guard`
/// 的 running 检查)对两个调用方一体生效——自动清理绝不会碰正在录制的笔记。
/// 以上豁免只覆盖本进程的 `AppState`:另一个共享同一数据目录的应用实例仍可能正在
/// 转写/补生成/续录某个笔记(全程持 `NoteLock`)。因此逐笔记清理时还要非阻塞探锁
/// (`store::disk::purge_note_audio_if_unlocked`),拿不到就跳过该笔记,不计入 freed——
/// 与启动扫描、转码 worker 的探锁语义一致。
fn purge_audio_older_than(app: &AppHandle, older_than_days: Option<u32>) -> Result<u64, String> {
    let state = app.state::<AppState>();
    migrate_guard(&state.running, &state.download_running, &state.retranscribing, &state.mixed_regen)?;
    let _reset = ResetOnDrop(state.download_running.clone());
    state.transcode.pause_and_wait();
    let _unpause = UnpauseOnDrop(state.transcode.clone());
    // cutoff 与 meta 里的 RFC3339 字符串同源(都来自 Local::now)，可直接字符串比较。
    let cutoff = older_than_days
        .map(|d| (chrono::Local::now() - chrono::Duration::days(d as i64)).to_rfc3339());
    let active_id = state.session.lock().unwrap().as_ref().map(|s| s.note_id.clone());
    let notes = notes_dir(app).map_err(|e| e.to_string())?;
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
        if is_active {
            continue;
        }
        // 跨进程互斥探锁 + should_purge 判定 + 实际清理三者一体,见
        // store::disk::purge_note_audio_if_unlocked 顶部注释:本进程的活动笔记豁免(上面
        // 的 is_active)只覆盖本进程,另一实例可能正持 NoteLock 在转写/补生成/续录同一
        // 笔记,该函数内部非阻塞探锁,拿不到就整笔记跳过(不计入 freed),不与它相撞。
        freed += store::disk::purge_note_audio_if_unlocked(&note_dir, cutoff.as_deref());
    }
    Ok(freed)
}

#[tauri::command]
fn purge_audio(app: AppHandle, older_than_days: Option<u32>) -> Result<u64, String> {
    purge_audio_older_than(&app, older_than_days)
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
/// 当前默认输出是否蓝牙:录制页据此预警蓝牙外放(蓝牙延迟超出软件 AEC 的延迟估计
/// 范围,回声消除失效)。与 capture_path(aec/vpio)设置无关,不按设置项门控——
/// 见 audio::default_output_is_bluetooth。
#[tauri::command]
fn output_is_bluetooth() -> bool {
    audio::default_output_is_bluetooth()
}

/// 每源管线健康快照(借鉴 meetily BufferStats 的可观测性设计):录制中返回各源
/// 帧数/样本数/断流次数/填充静音时长/重启次数,未录制返回空表。用途:用户报
/// "少了半句话"时可即时判断是设备断流(gaps/silence_ms>0)还是别的环节问题,
/// 不再靠猜。也是断连自愈的观测面。
#[tauri::command]
fn pipeline_health(state: State<AppState>) -> Vec<frame_tap::HealthSnapshot> {
    state
        .session
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.health.iter().map(|(src, h)| h.snapshot(*src)).collect())
        .unwrap_or_default()
}

/// 屏幕录制权限预检(macOS):系统声音采集(ScreenCaptureKit)依赖该权限。硬承诺
/// 双轨下未授权时 System 源在开录后会被 Fix A 拆除整场(不再是静默降级为仅麦克风)——
/// 录制页据此在**开录前**就给出常驻提示与授权入口,把"根本录不了"提前到点开录之前,
/// 而不是等用户点了开始才被拒录打断。
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
fn request_screen_capture_permission(app: AppHandle) -> bool {
    // 漏斗 1 的"授权"这一步。埋在"申请"而不是"预检"上:预检每次进录制页都会跑,
    // 计进去等于把一个用户算许多次;申请是用户主动做的一次动作,恰好是流失点本身。
    #[cfg(target_os = "macos")]
    let granted = unsafe { CGRequestScreenCaptureAccess() };
    #[cfg(not(target_os = "macos"))]
    let granted = true;
    telemetry::track(
        &app,
        telemetry::Event::PermissionChecked {
            kind: telemetry::PermissionKind::Screen,
            granted,
        },
    );
    granted
}

/// 依次执行全部捕获权限清理；任一项失败时仍继续，最终统一报告结果。
#[cfg(any(target_os = "macos", test))]
fn reset_capture_permissions_with(
    identifier: &str,
    mut reset: impl FnMut(&str, &str) -> bool,
) -> bool {
    let mut all_ok = true;
    for service in ["ScreenCapture", "AudioCapture"] {
        // 不短路：即使一项失败，也要尝试清理另一项，避免留下新的半修复状态。
        all_ok &= reset(service, identifier);
    }
    all_ok
}

/// 清除本应用在「屏幕录制」里的 TCC 授权记录(tccutil reset)。修复授权残留:
/// 换签名后(如 v0.1.x ad-hoc → 稳定证书)旧条目的 csreq 与新二进制不匹配,系统
/// 设置里开关看似已开、实际 SCShareableContent 始终被拒,且拨动开关/重启均无效
/// (2026-07-10 实锤:一个 bundle id 下积了 3 条残留)。清除后由前端引导重新授权。
///
/// 增强(2026-08-11,macOS 26.6.1 实锤):新版系统还会为系统声音维护独立的
/// AudioCapture 条目；两项任一残留旧 csreq 都会让双轨录制失败，因此必须一起清除。
#[tauri::command]
fn reset_screen_capture_permission(app: tauri::AppHandle) -> bool {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        false
    }
    #[cfg(target_os = "macos")]
    {
        reset_capture_permissions_with(&app.config().identifier, |service, identifier| {
            std::process::Command::new("/usr/bin/tccutil")
                .args(["reset", service, identifier])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
    }
}

/// 打开系统设置的屏幕录制隐私页(硬承诺双轨的授权引导:录制页 system_denied 引导卡
/// 「打开系统设置」按钮走这个命令)。opener 用法同 open_models_dir 先例。Windows 无
/// 对应的隐私页/URL scheme,该平台的引导卡走 unavailable 文案(无按钮),命令本身
/// 仍做平台分支返回 Err 兜底,避免误按下静默失败。
#[tauri::command]
fn open_screen_capture_settings(app: AppHandle) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use tauri_plugin_opener::OpenerExt;
        app.opener()
            .open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture", None::<&str>)
            .map_err(|e| tr!("打开系统设置失败: {e}", "Failed to open System Settings: {e}"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        Err(tr!(
            "本平台暂不支持自动跳转系统设置，请手动前往系统隐私设置授权",
            "Automatic navigation to System Settings isn't supported on this platform yet; please open your system's privacy settings manually"
        ))
    }
}

/// 解析 `osascript -e 'input volume of (get volume settings)'` 的 stdout(0..100)。
/// trim 后按十进制解析,越界截到 100,空/非数字 → None。
fn parse_input_volume(stdout: &str) -> Option<u8> {
    let v: u32 = stdout.trim().parse().ok()?;
    Some(v.min(100) as u8)
}

/// 读取 macOS 系统输入音量(0..100)。非 macOS / 读取失败 → None。录制页据此在普通
/// 麦克风模式下预警"输入音量被会议软件拉低,会录得很轻"。
#[tauri::command]
fn input_volume() -> Option<u8> {
    #[cfg(not(target_os = "macos"))]
    return None;
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("osascript")
            .args(["-e", "input volume of (get volume settings)"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        parse_input_volume(&String::from_utf8_lossy(&out.stdout))
    }
}

/// 设置 macOS 系统输入音量(0..100)。成功返回 true。非 macOS → false。
#[tauri::command]
fn set_input_volume(v: u8) -> bool {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = v;
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        let v = v.min(100);
        std::process::Command::new("osascript")
            .args(["-e", &format!("set volume input volume {v}")])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

#[cfg(debug_assertions)]
#[derive(serde::Serialize)]
struct SemanticGraphDebugFixture {
    session_id: String,
    graph: ipc::SemanticGraphData,
    path: ipc::KnowledgePath,
}

#[cfg(debug_assertions)]
fn semantic_graph_debug_sessions(
) -> &'static Mutex<std::collections::HashMap<String, PathBuf>> {
    static SESSIONS: std::sync::OnceLock<
        Mutex<std::collections::HashMap<String, PathBuf>>,
    > = std::sync::OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

#[cfg(debug_assertions)]
fn semantic_graph_debug_fixture_root(session_id: &str) -> Option<PathBuf> {
    semantic_graph_debug_sessions()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(session_id)
        .cloned()
}

#[cfg(debug_assertions)]
fn remove_semantic_graph_debug_fixture(session_id: &str) -> Result<(), String> {
    let root = semantic_graph_debug_sessions()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(session_id);
    let Some(root) = root else {
        return Ok(());
    };
    let owned_temp_root = root.parent() == Some(std::env::temp_dir().as_path())
        && root.file_name().is_some_and(|name| {
            name.to_string_lossy()
                .starts_with("aing-semantic-fixture-")
        });
    if !owned_temp_root {
        return Err("拒绝删除非调试夹具临时目录".to_string());
    }
    match std::fs::remove_dir_all(&root) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("删除隔离调试夹具失败:{error}")),
    }
}

/// Debug-only desktop harness. It always creates a new OS-temp child itself;
/// callers cannot provide a path, so the configured library can never be
/// selected or overwritten by the fixture importer.
#[cfg(debug_assertions)]
#[tauri::command]
fn semantic_graph_debug_fixture() -> Result<SemanticGraphDebugFixture, String> {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let session_id = store::stable_id(
        "fixture_session_",
        &[
            std::process::id().to_string(),
            nanos.to_string(),
            sequence.to_string(),
        ],
    );
    let fixture_root = std::env::temp_dir().join(format!("aing-semantic-fixture-{session_id}"));
    graph::large_fixture::import_semantic_graph_large_fixture(&fixture_root)
        .map_err(|error| format!("创建隔离语义图夹具失败:{error:#}"))?;
    let filter = graph::query::GraphFilter {
        entity_kinds: Vec::new(),
        predicate_types: Vec::new(),
        from: None,
        to: None,
        include_history: true,
        include_cooccurrence: true,
    };
    let graph = graph::query::semantic_graph(&fixture_root, &filter)
        .map_err(|error| format!("读取隔离语义图夹具失败:{error:#}"))?;
    let path = graph::path::shortest_path(&fixture_root, "kg_0000", "kg_0017", &filter)
        .map_err(|error| format!("读取隔离夹具路径失败:{error:#}"))?
        .ok_or_else(|| "隔离夹具没有预期路径".to_string())?;
    let stale_roots = {
        let mut sessions = semantic_graph_debug_sessions()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stale_roots = sessions.drain().map(|(_, root)| root).collect::<Vec<_>>();
        sessions.insert(session_id.clone(), fixture_root);
        stale_roots
    };
    for stale_root in stale_roots {
        let _ = std::fs::remove_dir_all(stale_root);
    }
    Ok(SemanticGraphDebugFixture {
        session_id,
        graph,
        path,
    })
}

/// Debug-only evidence loader. The client can only present an opaque session
/// token minted above; it can never select a filesystem path or the configured
/// user library.
#[cfg(debug_assertions)]
#[tauri::command]
fn semantic_graph_debug_relation_detail(
    session_id: String,
    relation_id: String,
) -> Result<Option<ipc::RelationDetail>, String> {
    let fixture_root = semantic_graph_debug_fixture_root(&session_id)
        .ok_or_else(|| "隔离调试夹具会话不存在或已过期".to_string())?;
    graph::query::relation_detail(&fixture_root, &relation_id)
        .map_err(|error| format!("读取隔离夹具关系证据失败:{error:#}"))
}

/// Debug-only cleanup. The client supplies only the opaque token; the server
/// resolves and validates its own mapped temp root before deleting anything.
#[cfg(debug_assertions)]
#[tauri::command]
fn semantic_graph_debug_release(session_id: String) -> Result<(), String> {
    remove_semantic_graph_debug_fixture(&session_id)
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// 前端把它持久化的匿名 id 交给后端,使两端事件归到同一个人。
///
/// 方向是单向的:**只由前端生成,后端一律接收、绝不自造**。两边各生成一个会把
/// 同一个人算成两个人,漏斗与留存全部失真。
#[tauri::command]
fn set_analytics_id(id: String) {
    telemetry::set_distinct_id(&id);
}

/// 环境快照给前端。**两端必须用同一份值**:posthog-js 的 `$os_version` 从 UA 正则
/// 解析,而 WKWebView 的 UA 冻结在 `Mac OS X 10_15_7`、WebView2 冻结在
/// `Windows NT 10.0`(Win11 也报 10.0)——不盖掉的话同一台机器在看板上会劈成两个
/// 系统版本,而本应用的采集行为恰恰按 macOS 大版本分叉(ScreenCaptureKit/CATap/授权)。
#[tauri::command]
fn app_env() -> telemetry::EnvSnapshot {
    telemetry::EnvSnapshot::current()
}

/// 上报总开关的当前值。前端 init 前问一次:关掉时 posthog-js 根本不 init,
/// 而不是 init 之后再 opt-out——后者仍会加载录制器、仍受远端配置摆布。
#[tauri::command]
fn telemetry_enabled() -> bool {
    telemetry::is_enabled()
}

/// 前端上报一次失败。**kind 走白名单枚举**,认不出就整条丢弃——否则自由文本从这个
/// 口子绕过了后端的全部隐私红线。detail 与后端同一条脱敏路径(report_error 内部)。
///
/// 存在的理由:一键更新走的是 tauri-plugin-updater 的 JS API,失败只发生在前端;
/// 而"更新装不上"正是那种本机日志之外无人知晓、又直接卡住所有后续修复的故障。
#[tauri::command]
fn report_frontend_error(kind: String, detail: String) {
    let Some(kind) = telemetry::ErrorKind::parse(&kind) else {
        return;
    };
    telemetry::report_error(kind, &detail);
}

pub fn run() {
    // 尽早初始化:panic hook 越早装覆盖面越大(它由 posthog-rs 的 capture_panics
    // 安装,能捕获被 catch_unwind 吞掉的 panic——那些「出事了但应用硬撑着不崩」
    // 的场景恰恰最该被看见)。失败只记日志,绝不影响启动。
    telemetry::init();
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        // 一键更新:updater 查/下/装(签名校验,密钥见 tauri.conf plugins.updater),
        // process 提供装完后的 relaunch。
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(shortcuts::on_shortcut)
                .build(),
        );
    // 遥测供应商(Aptabase)已下线:插件注册、以及为它兜底 Tokio reactor 的
    // EnterGuard 一并移除(那段兜底是插件内部裸 tokio::spawn 的需要,供应商没了
    // 就没有存在理由)。telemetry 模块与 6 个埋点调用点保留,见 telemetry.rs。
    builder
        .manage(AppState::default())
        .manage(player::PlayerHandle::default())
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
            // 生命周期 actor:命令面(五命令/toggle/UDS/tray)经其信箱串行执行(P1 绞杀者)。
            // 必须在任何命令可达之前 manage——setup 先于 webview 加载/UDS server/托盘构建。
            app.manage(lifecycle::spawn(handle.clone()));
            // settings.json 是自举指针,永远读写 app_data_dir(不随 data_dir 漂移)。
            let app_data = handle.path().app_data_dir().ok();
            // 最先执行:stderr/stdout 黑匣子(见 logging.rs)。后续任何 eprintln 与
            // ONNX Runtime 的错误输出都要进日志,晚一步就可能漏掉启动期报错。
            if let Some(dir) = &app_data {
                logging::redirect_stdio_to_file(dir);
            }
            // 会话标记紧跟着 app_data 落,越早越好:它但凡晚一步,settings 探测/自愈、
            // 插件构建、模型与后台服务这些启动期步骤里的 SIGSEGV/OOM 就会留着上次的
            // 干净标记,下次启动什么也报不出来——而启动期崩溃恰恰是最难在别人机器上
            // 复现的一类(codex review P2#7、二轮 P2)。事件仍在本闭包末尾发,
            // 那时 track 才有意义。
            let boot = app_data.as_ref().map(|d| telemetry::open_session(d));
            // 镜像前缀已随三删一藏改为编译期常量(settings::MIRROR_PREFIX),不再有
            // mirror_prefix 字段可迁移——一次性 migrate_mirror_prefix 启动调用随之删除。
            //
            // 启动一次性自愈(2026-08-10 review Important:堵尸检累积回归 → 同日二审 Important:
            // 堵双尸检回归):先前的写法是先 `load(d)` 拿 `s`,再 `if needs_heal { update(d, |_|{}) }`
            // ——`load` 对坏文件会当场写一具 `settings.json.corrupt-*` 尸体,而 `update` 内部
            // 又会重新 `load` 同一份坏文件,再写第二具尸体,一次启动堵出两具。改为:先探测
            // `needs_heal`(纯 `from_str`,不写盘,见 settings::needs_heal 文档),只有它为 true
            // 时才走一次 `load→save` round-trip(`update`),并直接拿 `update` 返回的落盘快照
            // 当 `s` 用——那一次 `load` 就是产出尸检文件的唯一一次,不再另起一次探测性 load。
            // 探测为 false(全新安装/已是干净新格式)则直接 `load`,全程不落盘也不产生尸体。
            // `update` 失败(权限/IO 等极端情况)保底退回纯 `load`,行为不劣于旧代码。
            let s = match &app_data {
                Some(dir) if settings::needs_heal(dir) => settings::update(dir, |_| {})
                    .unwrap_or_else(|_| settings::load(dir)),
                Some(dir) => settings::load(dir),
                None => settings::Settings::default(),
            };
            // UI 语言:必须先于托盘构建等任何用户可见文案产生处(tr! 读此全局)。
            i18n::set_lang(&s.ui_lang);
            // 上报总开关:settings 一读到就同步。放这么早是因为 telemetry::init() 在
            // run() 开头就装好了 panic hook——从这一刻起到这里之间发生的 panic 仍会上报
            // (那个窗口只有几毫秒,且此时连设置都还没读到,没有更早的判据可用)。
            telemetry::set_enabled(s.telemetry_enabled);
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
            player::clean_playback_cache(&handle); // 回收超期的回放解码缓存(可再生)

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
                    // 没转完 / 新迁入的历史 WAV)。本进程此刻必无录制会话,但同一数据目录可能
                    // 被另一实例正在录制——那种目录绝不能被当孤儿去修头/入队,故逐目录先探锁。
                    if let Ok(rd) = std::fs::read_dir(root.join("notes")) {
                        for e in rd.flatten() {
                            if e.path().is_dir() {
                                // 探锁并把它绑定为存活值(不是探完即放):repair_stale_tracks
                                // 直接改写 WAV 头,若锁在探完那一刻就释放，另一实例可能在
                                // repair 与 enqueue 之间的窗口期开始续录并发写同一 WAV，
                                // 修头操作与它的写入相撞。锁必须覆盖到 repair 完成之后，
                                // enqueue 只是入队(不动这个目录的文件)，之前 drop 即可。
                                let _probe = match store::notelock::NoteLock::try_exclusive(&e.path()) {
                                    Ok(Some(probe)) => probe,
                                    _ => continue,
                                };
                                store::audio::repair_stale_tracks(&e.path());
                                drop(_probe);
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
                // 转码会编码后删除源 WAV——必须独占持锁到转码结束,防止与
                // (本进程续录 cancel_and_wait 之外的)另一实例的活动会话相撞。
                let lock = match store::notelock::NoteLock::try_exclusive(dir) {
                    Ok(Some(l)) => l,
                    // 拿不到锁=该目录有活会话(含另一实例)在用;此次转码任务作废,
                    // 但队列语义幂等按目录去重——续录结束后 Aing 路径会重新入队,不丢。
                    _ => return,
                };
                store::transcode::transcode_note_dir(dir);
                drop(lock); // 锁只需护住"转码+删 WAV"窗口,完成通知不必持锁。
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
            // MCP 注册路径自愈:App 被移动/换装后,各 Agent 配置里的 command 指向旧路径,
            // Agent spawn 会失败。启动时静默改正;开发态二进制(target/)在 heal 内部跳过。
            std::thread::spawn(|| {
                if let Ok(reg) = mcp::registry::Registry::new() {
                    if let Ok(n) = reg.heal() {
                        if n > 0 {
                            MCP_HEALED.store(n, Ordering::SeqCst);
                        }
                    }
                }
                // Skill 同步:受管且过期(应用升级后)静默重写为当前版本。
                let _ = crate::mcp::skill::heal();
            });
            // 图谱存量兜底:启动只标脏一次。与 Aing 完成请求共用同一 scheduler，
            // 因而不会重叠 builder，也不会在 worker 退出窗口丢掉最后一次请求。
            if let Ok(root) = data_root(&handle) {
                let graph_events = handle.clone();
                if let Err(error) = st.graph_scheduler.request(root, move |status| {
                    let _ = graph_events.emit("graph_index_status", status);
                }) {
                    eprintln!("graph: 启动索引排队失败，已保留重试标记: {error:#}");
                }
            }
            // UDS listener:MCP stdio 进程的活能力后端(状态/实时/控制)。
            mcp::uds::spawn_listener(handle.clone());
            // 音频自动保留期(spec §5):到期笔记仅清音频轨,转写/精修稿永留。启动后台跑一次,
            // 失败仅打日志(容量治理是增值层,绝不挡启动)。录制中笔记天然不在清理范围——
            // 复用 purge_audio 命令的同一份实现 purge_audio_older_than,其活动笔记豁免
            // (session 槽比对 + migrate_guard 的 running 检查)对自动路径同样生效。
            if let Some(days) = s.audio_retention.days() {
                let app2 = handle.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    match purge_audio_older_than(&app2, Some(days)) {
                        Ok(freed) => eprintln!("音频保留期清理完成: 释放 {freed} 字节(>{days} 天)"),
                        Err(e) => eprintln!("音频保留期清理失败(不影响使用): {e}"),
                    }
                });
            }
            // 上次运行的痕迹(标记已在本闭包开头读走并重落,见那里的说明):硬崩溃
            //(SIGSEGV/OOM/强退,panic hook 覆盖不到)与升级首启。必须在 AppStarted
            // 之前发——它们描述的是上一次运行,时间上排在本次启动之前。
            if let Some(boot) = &boot {
                if boot.unclean_exit {
                    telemetry::track(
                        &handle,
                        telemetry::Event::AppUncleanExit {
                            version: telemetry::SafeVersion::parse(
                                boot.last_version.as_deref().unwrap_or(""),
                            ),
                        },
                    );
                }
                if let Some(from) = &boot.updated_from {
                    telemetry::track(
                        &handle,
                        telemetry::Event::AppUpdated {
                            from_version: telemetry::SafeVersion::parse(from),
                        },
                    );
                }
            }
            // 样本溯源 WAL 恢复:上次进程死在"intent 已落、complete 未落"之间的话,
            // 这里把在途写入按三分支收尾(转正/丢弃/记冲突)。纯文件操作,先于重建。
            if let Ok(root) = data_root(&handle) {
                let (done, dropped, conflicted) =
                    store::VoiceprintStore::new(root).recover_sample_trace();
                if done + dropped + conflicted > 0 {
                    eprintln!("样本溯源恢复:转正 {done} 丢弃 {dropped} 冲突 {conflicted}");
                }
            }
            // 声纹库标签与当前选型不一致时主动重建一次。放在启动末尾:它要起线程加载
            // 嵌入器,不该和前面那些"必须先就位"的初始化抢时间。
            // 标记快照必须在自愈**之前**取:自愈自己会写标记,后取快照必然看见它,
            // 一次启动就固定跑两轮整库嵌入(codex 混杂实现轮七 P2)。
            let stale_marker = rebuild_marker_path(&handle).is_some_and(|p| p.exists());
            let healed = heal_voiceprint_model_mismatch(&handle, &st);
            // 上次退出前有没跑完的重建诉求(落盘标记还在)→ 补跑。质心清空型重建不改
            // 库标签,标签自愈兜不住它(codex 混杂实现轮六 P1)。自愈已发起时不必再发:
            // 一轮全库重建覆盖同样的诉求。
            if stale_marker && !healed {
                eprintln!("发现未完成的重建诉求(上次退出前没跑完),补跑");
                spawn_voiceprint_rebuild(&handle, st.embedder_cache.clone(), "启动补跑上次未完成的重建");
            }
            telemetry::track(&handle, telemetry::Event::AppStarted);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_recording,
            resume_recording,
            stop_recording,
            recording_status,
            pause_recording,
            unpause_recording,
            finalize_interrupted_note,
            list_notes,
            get_note,
            refine_note,
            retry_failed_refine,
            identify_note,
            calendar_permission,
            request_calendar_permission,
            list_calendar_candidates,
            set_note_calendar_event,
            backfill_calendar_matches,
            list_identify_suggestions,
            apply_identify_suggestion,
            reject_identify_suggestion,
            acknowledge_identify,
            undo_identify_apply,
            get_refined,
            note_refining,
            retranscribe_note,
            regenerate_mixed,
            mixed_regen_status,
            mixed_playback_info,
            retranscribe_status,
            mixed_input_status,
            save_refined,
            preview_relation_backfill,
            start_relation_backfill,
            cancel_relation_backfill,
            retry_relation_backfill_index,
            assign_note_speaker_person,
            clear_note_speaker_person,
            person_notes,
            note_related,
            graph_entities,
            graph_data,
            semantic_graph,
            semantic_entity_detail,
            relation_detail,
            pending_review,
            entity_mentions,
            shortest_path,
            #[cfg(debug_assertions)]
            semantic_graph_debug_fixture,
            #[cfg(debug_assertions)]
            semantic_graph_debug_relation_detail,
            #[cfg(debug_assertions)]
            semantic_graph_debug_release,
            apply_knowledge_operation,
            split_entity,
            merge_entities,
            undo_knowledge_operation,
            note_graph_data,
            graph_edge_detail,
            entity_detail,
            note_entity_links,
            rename_entity,
            note_audio_info,
            rename_note,
            delete_note,
            export_note,
            export_note_audio,
            open_note_dir,
            rename_speaker,
            delete_note_speaker,
            edit_segment,
            delete_segment,
            set_segment_speaker,
            set_segments_speaker,
            delete_segments,
            pipeline_health,
            screen_capture_permission,
            request_screen_capture_permission,
            reset_screen_capture_permission,
            open_screen_capture_settings,
            input_volume,
            set_input_volume,
            output_is_bluetooth,
            models_status,
            open_models_dir,
            download_models,
            cancel_models_download,
            delete_model,
            get_settings,
            set_settings,
            test_cloud_asr,
            list_hooks,
            save_hooks,
            test_hook,
            test_refine_llm,
            test_refine_agent,
            test_mirror,
            apply_shortcut,
            migrate_data_dir,
            migrate_models_dir,
            audio_disk_usage,
            purge_audio,
            list_people,
            count_people_without_samples,
            voiceprint_library_model,
            rebuild_voiceprint_library,
            rename_person,
            merge_person,
            delete_person,
            rebuild_person_voiceprint,
            delete_person_sample,
            suggest_person_merges,
            apply_confident_merges,
            mark_speaker_multi,
            multi_impact,
            suggest_split_groups,
            commit_split,
            cancel_split,
            auto_split_speaker,
            undo_auto_split,
            latest_undoable_split,
            get_scene,
            confirm_multi_samples,
            resolve_multi_residual,
            list_split_ops,
            undo_merge,
            restore_merged_person,
            acknowledge_merge,
            list_merge_receipts,
            dismiss_tidy_item,
            list_dismissed_tidy_items,
            mcp_agents_status,
            mcp_register,
            mcp_unregister,
            mcp_manual_snippet,
            mcp_healed_count,
            mcp_skill_status,
            mcp_skill_install,
            mcp_skill_uninstall,
            mcp_capabilities,
            refine_agents_probe,
            ai_logs_query,
            ai_logs_export,
            ai_logs_open_dir,
            mcp_skill_read,
            mcp_skill_save,
            update::check_update,
            player::player_load,
            player::player_play,
            player::player_pause,
            player::player_seek,
            player::player_set_muted,
            player::player_stop,
            set_playback_active,
            mic_mode,
            precheck_recording,
            set_analytics_id,
            app_env,
            telemetry_enabled,
            report_frontend_error
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        // macOS 点击 dock 图标触发 Reopen:关窗被托盘拦截成 hide 后,dock 是用户最
        // 本能的召回手势——之前只有托盘图标能唤回,dock 点击石沉大海(实测用户以为
        // 程序卡死)。召回语义与托盘 show 菜单项一致:show + set_focus。
        // Reopen 是 macOS 独有变体(其余平台该枚举没有此成员,匹配都编不过),整块 cfg。
        .run(|app, event| {
            // 退出前排空上报队列。SDK 起的是带缓冲的后台 worker,全局单例的析构
            // 在进程退出时不会执行,不主动 flush 的话临退出那几秒的事件(停录、
            // 导出、刚发生的错误)会静默丢失(codex review 发现)。
            if let tauri::RunEvent::Exit = event {
                // 干净退出记号必须与 flush 同一处:漏在别的退出分支上,那条分支的每次
                // 退出都会被下次启动误报成崩溃(见 telemetry::close_session)。
                if let Ok(dir) = app.path().app_data_dir() {
                    telemetry::close_session(&dir);
                }
                telemetry::flush_on_exit();
            }
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (app, &event);
        });
}

#[cfg(test)]
mod cut_sample_tests {
    use super::cut_person_sample_from_notes;

    /// 合并兜底截样端到端:合成一条带 mic.wav 的笔记,speaker S1 关联 P9,
    /// 截出的样本应等于 S1 两段之和(长段优先)且不超上限;查无此人返回 None。
    #[test]
    fn cuts_person_speech_from_note_audio() {
        let tmp = tempfile::tempdir().unwrap();
        let note = tmp.path().join("20260101-000000");
        std::fs::create_dir_all(&note).unwrap();
        // mic.wav:10s @16k s16,样本值=下标 mod 1000(可校验切片位置)
        let n = 16000 * 10;
        let mut data = Vec::with_capacity(n * 2);
        for i in 0..n {
            data.extend_from_slice(&(((i % 1000) as i16) - 500).to_le_bytes());
        }
        let mut wav = crate::store::audio::wav_header(data.len() as u32).to_vec();
        wav.extend_from_slice(&data);
        std::fs::write(note.join("mic.wav"), &wav).unwrap();
        std::fs::write(
            note.join("speakers.json"),
            r#"{"S1":{"name":"","sources":["mic"],"person_id":"P9"},"S2":{"name":"","sources":["mic"],"person_id":"P8"}}"#,
        )
        .unwrap();
        // S1: 1000..3000ms 与 5000..6000ms;S2(别人)夹在中间不得混入
        std::fs::write(
            note.join("segments.jsonl"),
            concat!(
                r#"{"seq":1,"source":"mic","text":"a","start_ms":1000,"end_ms":3000,"speaker":"S1"}"#, "\n",
                r#"{"seq":2,"source":"mic","text":"b","start_ms":3000,"end_ms":5000,"speaker":"S2"}"#, "\n",
                r#"{"seq":3,"source":"mic","text":"c","start_ms":5000,"end_ms":6000,"speaker":"S1"}"#, "\n",
            ),
        )
        .unwrap();

        let sample = cut_person_sample_from_notes(tmp.path(), "P9").expect("应截到样本");
        assert_eq!(sample.len(), 16000 * 3, "S1 两段共 3s");
        // 长段优先:开头应是 1000ms 处的样本(值 (16000%1000)-500=... 按下标校验首值)
        let first_idx = 1000 * 16; // 1000ms → 样本下标 16000
        let expect = (((first_idx % 1000) as i16) - 500) as f32 / 32768.0;
        assert!((sample[0] - expect).abs() < 1e-3, "首样本应来自 1000ms 处");
        assert!(cut_person_sample_from_notes(tmp.path(), "P404").is_none(), "查无此人");
    }
}

#[cfg(test)]
mod cloud_asr_factory_tests {
    use super::{cloud_provider_label, make_cloud_asr};
    use crate::settings::{Settings, CLOUD_ALIYUN, CLOUD_VOLCANO};

    #[test]
    fn missing_creds_bail_with_settings_hint() {
        // 断言的是中文原文(进程默认语言):拿语言锁,免得并发的语言切换用例把它掀翻。
        let _lang = crate::i18n::test_lang_guard();
        let s = Settings { cloud_asr_provider: CLOUD_VOLCANO.into(), ..Default::default() };
        let err = make_cloud_asr(&s).err().expect("火山缺凭证应报错");
        assert!(err.to_string().contains("请先在设置中配置云端凭证"), "{err}");
        // 半套凭证(只有 app_key)同样不算齐:握手必然 401,提前拦住。
        let s = Settings {
            cloud_asr_provider: CLOUD_VOLCANO.into(),
            volc_app_key: "a".into(),
            ..Default::default()
        };
        assert!(make_cloud_asr(&s).is_err(), "火山半套凭证应报错");
        let s = Settings { cloud_asr_provider: CLOUD_ALIYUN.into(), ..Default::default() };
        let err = make_cloud_asr(&s).err().expect("阿里缺凭证应报错");
        assert!(err.to_string().contains("请先在设置中配置云端凭证"), "{err}");
    }

    #[test]
    fn builds_adapter_per_provider_when_creds_ok() {
        // 同上:cloud_provider_label 断言中文原文,须与语言切换用例互斥。
        let _lang = crate::i18n::test_lang_guard();
        let volc = Settings {
            cloud_asr_provider: CLOUD_VOLCANO.into(),
            volc_app_key: "a".into(),
            volc_access_key: "b".into(),
            ..Default::default()
        };
        assert!(make_cloud_asr(&volc).is_ok(), "火山凭证齐 → 造得出适配器");
        let ali = Settings {
            cloud_asr_provider: CLOUD_ALIYUN.into(),
            dashscope_api_key: "sk-x".into(),
            ..Default::default()
        };
        assert!(make_cloud_asr(&ali).is_ok(), "阿里凭证齐 → 造得出适配器");
        assert_eq!(cloud_provider_label(CLOUD_VOLCANO), "火山引擎");
        assert_eq!(cloud_provider_label(CLOUD_ALIYUN), "阿里云");
    }
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

    /// 续录时只有本场 writer 真正追加过样本的源才能更新 sync。否则(启动失败/活跃却
    /// 无帧写入等一般性场景)会拿旧 WAV 配上本场零计数,覆盖上一场可信记录——
    /// keep_audio 开关已随三删一藏移除(固定保留音频),这里锁的是更一般的
    /// "writer 未写入"契约,不再是某个可关闭开关的专属场景。
    #[test]
    fn sync_persistence_preserves_prior_record_when_current_writer_wrote_nothing() {
        use crate::audio::Source;
        use crate::pipeline::frame_tap::SourceHealth;
        use crate::store::audio::{load_audio_meta, set_track_sync, AudioTrackWriter, SyncInfo};
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let mut writer = AudioTrackWriter::new(dir.path(), "mic", 0);
        let _ = writer.append(&[0.1; 1600]);
        drop(writer);
        set_track_sync(
            dir.path(),
            "mic",
            SyncInfo {
                wall_ms: 111,
                samples: 222,
                track_ms: 100,
                drift_ms: -11,
                silence_ms: 3,
                gaps: 4,
                rate_fixes: 5,
                ..Default::default()
            },
        )
        .unwrap();

        let health = Arc::new(SourceHealth::default());
        let wrote = Arc::new(AtomicBool::new(false));
        super::persist_track_sync(
            dir.path(),
            100,
            999,
            &[(Source::Mic, health)],
            &[(Source::Mic, wrote)],
        );

        let sync = load_audio_meta(dir.path()).tracks["mic"].sync.clone().unwrap();
        assert_eq!(sync.wall_ms, 111);
        assert_eq!(sync.samples, 222);
        assert_eq!(sync.track_ms, 100);
        assert_eq!(sync.drift_ms, -11);
    }

    #[test]
    fn sync_persistence_updates_record_after_successful_current_write() {
        use crate::audio::Source;
        use crate::pipeline::frame_tap::SourceHealth;
        use crate::store::audio::{load_audio_meta, AudioTrackWriter};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let mut writer = AudioTrackWriter::new(dir.path(), "mic", 0);
        assert!(writer.append(&[0.1; 1600]));
        drop(writer);

        let health = Arc::new(SourceHealth::default());
        health.samples.store(1600, Ordering::Relaxed);
        let wrote = Arc::new(AtomicBool::new(true));
        super::persist_track_sync(
            dir.path(),
            0,
            100,
            &[(Source::Mic, health)],
            &[(Source::Mic, wrote)],
        );

        let sync = load_audio_meta(dir.path()).tracks["mic"].sync.clone().unwrap();
        assert_eq!(sync.wall_ms, 100);
        assert_eq!(sync.samples, 1600);
        assert_eq!(sync.track_ms, 100);
        assert_eq!(sync.drift_ms, 0);
        // 二期:首帧偏移随对账落盘(mixed seek 修正的数据来源)。默认 health 未记录
        // 首帧 → 读数为 0,但字段必须是 Some——None 专属旧数据。
        assert_eq!(sync.first_frame_offset_ms, Some(0));
    }

    /// 硬承诺双轨(Task 3,settings-overhaul spec §4):required_sources 去参数化,
    /// 恒返 [Mic, System]——两源皆必备,任一起不来即整场拆除(Fix A 守卫),不再有
    /// 「仅系统声」的降级分支可选。
    #[test]
    fn required_sources_always_requires_mic_and_system() {
        use crate::audio::Source;
        assert_eq!(super::required_sources(), vec![Source::Mic, Source::System]);
    }

    /// 新版 macOS 将系统声音单列为 AudioCapture TCC 服务；修复旧签名残留时必须
    /// 与 ScreenCapture 一起清理，而且首项失败也不能阻止第二项继续尝试。
    #[test]
    fn reset_capture_permissions_attempts_both_tcc_services() {
        let mut calls = Vec::new();
        let ok = super::reset_capture_permissions_with("com.teemo.voice-notes", |service, id| {
            calls.push((service.to_string(), id.to_string()));
            service != "ScreenCapture"
        });

        assert!(!ok);
        assert_eq!(
            calls,
            vec![
                (
                    "ScreenCapture".to_string(),
                    "com.teemo.voice-notes".to_string()
                ),
                (
                    "AudioCapture".to_string(),
                    "com.teemo.voice-notes".to_string()
                ),
            ]
        );
    }

    /// Fix A 拆除路径错误文案的三分支(硬承诺双轨,Task 3 审查修复):System 缺失
    /// 且底层失败带 "unauthorized" → system_denied token(屏幕录制权限缺失,
    /// 前端渲染可操作的授权引导卡)。语言锁到 zh,避免与其它切换语言的用例并发互踩
    /// (见 i18n::test_lang_guard 注释)。
    #[test]
    fn missing_source_error_system_unauthorized_yields_denied_token() {
        use crate::audio::Source;
        let _guard = crate::i18n::test_lang_guard();
        crate::i18n::set_lang("zh");
        let failed = vec![(Source::System, "unauthorized: 未授权屏幕录制".to_string())];
        let err = super::missing_source_error(Source::System, &failed);
        assert!(err.starts_with("error: "));
        assert!(err.contains("system_denied"), "{err}");
        assert!(!err.contains("system_unavailable"), "{err}");
        crate::i18n::set_lang("zh"); // 恢复默认,避免污染同进程其它用例
    }

    /// System 缺失但失败原因不含 "unauthorized"(如 VAD 构建失败/设备问题)→
    /// system_unavailable token,不是权限问题,前端不给「打开系统设置」按钮。
    #[test]
    fn missing_source_error_system_other_failure_yields_unavailable_token() {
        use crate::audio::Source;
        let _guard = crate::i18n::test_lang_guard();
        crate::i18n::set_lang("zh");
        let failed = vec![(Source::System, "vad 构建失败: 模型文件缺失".to_string())];
        let err = super::missing_source_error(Source::System, &failed);
        assert!(err.contains("system_unavailable"), "{err}");
        assert!(!err.contains("system_denied"), "{err}");
        crate::i18n::set_lang("zh");
    }

    /// Mic 缺失沿用硬承诺双轨改造前的纯文案,不带任何分类 token——逐字节等价旧
    /// 格式(Mic 缺失没有"打开系统设置"这类可操作引导可给,不该被误判成 System 相关)。
    #[test]
    fn missing_source_error_mic_missing_has_no_token_and_matches_legacy_format() {
        use crate::audio::Source;
        let _guard = crate::i18n::test_lang_guard();
        crate::i18n::set_lang("zh");
        let failed = vec![(Source::Mic, "设备被占用".to_string())];
        let err = super::missing_source_error(Source::Mic, &failed);
        assert_eq!(err, "error: 麦克风未能启动: 设备被占用");
        assert!(!err.contains("system_denied") && !err.contains("system_unavailable"), "{err}");
        crate::i18n::set_lang("zh");
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
        let rt = Arc::new(Mutex::new(None));
        let mr: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        assert!(migrate_guard(&running, &dl, &rt, &mr).is_err(), "录制中拒绝");
        assert!(!dl.load(Ordering::SeqCst), "拒绝后复位互斥位");
        // download_running 已 true(下载/另一迁移在跑）→ 拒。
        let running = Arc::new(Mutex::new(false));
        let dl = Arc::new(AtomicBool::new(true));
        let rt = Arc::new(Mutex::new(None));
        let mr: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        assert!(migrate_guard(&running, &dl, &rt, &mr).is_err(), "下载/迁移进行中拒绝");
        // 都空闲 → 过,并已抢下互斥位(swap 置 true)。
        let running = Arc::new(Mutex::new(false));
        let dl = Arc::new(AtomicBool::new(false));
        let rt = Arc::new(Mutex::new(None));
        let mr: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        assert!(migrate_guard(&running, &dl, &rt, &mr).is_ok(), "空闲放行");
        assert!(dl.load(Ordering::SeqCst), "放行后互斥位已抢占");
    }

    /// Fix 1(codex 第二轮):重转写槽占用时迁移必被拒,且拒绝后必须复位刚抢下的
    /// download_running 互斥位(否则迁移互斥位永久卡死,连"重转写已经跑完"之后
    /// 的下一次迁移也会被误拒)。
    #[test]
    fn migrate_guard_rejects_while_retranscribing() {
        use super::migrate_guard;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        let running = Arc::new(Mutex::new(false));
        let dl = Arc::new(AtomicBool::new(false));
        let rt = Arc::new(Mutex::new(Some(("n1".to_string(), "decode".to_string()))));
        let mr: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        assert!(migrate_guard(&running, &dl, &rt, &mr).is_err(), "重转写占槽时迁移必拒");
        assert!(!dl.load(Ordering::SeqCst), "拒绝后必须复位互斥位");
        // 槽清空后(worker 跑完)同一互斥位可以再次放行。
        *rt.lock().unwrap() = None;
        assert!(migrate_guard(&running, &dl, &rt, &mr).is_ok(), "槽清空后迁移应放行");
    }

    /// codex 第二轮 P1:补生成槽占用时迁移必拒(worker 正 mmap 读源轨/写笔记目录,
    /// 搬根会移走映射中的文件),拒绝后互斥位必须复位。
    #[test]
    fn migrate_guard_rejects_while_mixed_regen() {
        use super::migrate_guard;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        let running = Arc::new(Mutex::new(false));
        let dl = Arc::new(AtomicBool::new(false));
        let rt: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
        let mr: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(Some("n1".into())));
        assert!(migrate_guard(&running, &dl, &rt, &mr).is_err(), "补生成占槽时迁移必拒");
        assert!(!dl.load(Ordering::SeqCst), "拒绝后复位互斥位");
        *mr.lock().unwrap() = None;
        assert!(migrate_guard(&running, &dl, &rt, &mr).is_ok(), "槽清空后迁移应放行");
    }

    /// Fix 2(codex 第三轮,A↔R 占槽后互查闭环)的说明性单测:验证 A 侧判据本身——
    /// `retranscribing_blocks_refine` 命中"同 note_id 正被重转写占槽"时必须为
    /// true(spawn_refine 据此清 Aing 集、放弃、不 spawn),不同 note_id / 槽为空
    /// 时必须为 false(不误伤其它笔记的正常 Aing)。
    ///
    /// 说明(为何不是并发回归测试):真实竞态是 do_retranscribe 的"占槽"与
    /// spawn_refine 的"kernel 插入 Aing 集"这两个动作在两个线程间交错,需要
    /// AppHandle + lifecycle actor 信箱 + 后台线程才能搭台,不是能在单元测试里
    /// 稳定复现的窗口(时序依赖真实调度,搭出来的"竞态"测试要么永远命中、要么
    /// 永远不命中，测的是测试自身的调度而非生产逻辑)。互斥的完整正确性来自
    /// spawn_refine/do_retranscribe 两处 Fix 2 注释里的书面推演(两侧"先写自己、
    /// 再读对方"，靠 actor 信箱 FIFO 单消费者顺序构成时序环，双穿不可能发生)，
    /// 本测试只锁死这条推演依赖的判据函数本身不会因未来重构而跑偏。
    #[test]
    fn retranscribing_blocks_refine_matches_same_note_id_only() {
        use super::retranscribing_blocks_refine;
        use std::sync::Mutex;
        let empty: Mutex<Option<(String, String)>> = Mutex::new(None);
        assert!(!retranscribing_blocks_refine(&empty, "n1"), "槽为空,不阻挡任何 Aing");

        let occupied: Mutex<Option<(String, String)>> =
            Mutex::new(Some(("n1".to_string(), "decode".to_string())));
        assert!(retranscribing_blocks_refine(&occupied, "n1"), "同 note_id 命中,须阻挡");
        assert!(
            !retranscribing_blocks_refine(&occupied, "n2"),
            "不同笔记不受影响,各笔记的重转写/Aing 互不干扰"
        );
    }

    #[test]
    fn semantic_graph_commands_are_registered() {
        let source = include_str!("lib.rs").replace("\r\n", "\n");
        let handlers = source
            .split_once(".invoke_handler(tauri::generate_handler![")
            .expect("generate_handler block")
            .1
            .split_once("])")
            .expect("generate_handler terminator")
            .0;
        for command in [
            "semantic_graph,",
            "semantic_entity_detail,",
            "relation_detail,",
            "pending_review,",
            "entity_mentions,",
            "shortest_path,",
            "apply_knowledge_operation,",
            "split_entity,",
            "merge_entities,",
            "undo_knowledge_operation,",
            "graph_edge_detail,",
        ] {
            assert!(
                handlers.contains(command),
                "missing registered command {command}"
            );
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn semantic_graph_debug_fixture_owns_a_new_isolated_temp_root() {
        let fixture = super::semantic_graph_debug_fixture().unwrap();
        assert!(!fixture.session_id.contains(std::path::MAIN_SEPARATOR));
        let root = super::semantic_graph_debug_fixture_root(&fixture.session_id).unwrap();
        let temp = std::env::temp_dir().canonicalize().unwrap();
        let parent = root.parent().unwrap().canonicalize().unwrap();
        assert!(parent.starts_with(temp));
        assert!(root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("aing-semantic-fixture-"));
        assert_eq!(fixture.graph.nodes.len(), 1_000);
        assert_eq!(fixture.graph.semantic_edges.len(), 5_000);
        assert_eq!(fixture.path.steps[0].id, "cr_fixture_0000");

        let configured_root = tempfile::tempdir().unwrap();
        let mut configured_graph = crate::graph::large_fixture::deterministic_large_graph();
        configured_graph.relations[0].provider = Some("configured-real-library".into());
        configured_graph.relations[1].id = "configured-only-relation".into();
        crate::graph::index::rebuild_atomic(configured_root.path(), &configured_graph).unwrap();
        assert_eq!(
            crate::graph::query::relation_detail(configured_root.path(), "cr_fixture_0000")
                .unwrap()
                .unwrap()
                .provider
                .as_deref(),
            Some("configured-real-library")
        );
        assert!(crate::graph::query::relation_detail(
            configured_root.path(),
            "configured-only-relation"
        )
        .unwrap()
        .is_some());

        let isolated = super::semantic_graph_debug_relation_detail(
            fixture.session_id.clone(),
            "cr_fixture_0000".into(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(isolated.provider.as_deref(), Some("fixture"));
        assert!(super::semantic_graph_debug_relation_detail(
            fixture.session_id.clone(),
            "configured-only-relation".into(),
        )
        .unwrap()
        .is_none());
        assert!(super::semantic_graph_debug_relation_detail(
            "unknown-session".into(),
            "cr_fixture_0000".into(),
        )
        .is_err());

        let source = include_str!("lib.rs").replace("\r\n", "\n");
        assert!(source.contains("#[cfg(debug_assertions)]\n            semantic_graph_debug_fixture,"));
        assert!(source.contains(
            "#[cfg(debug_assertions)]\n            semantic_graph_debug_relation_detail,"
        ));
        assert!(source.contains(
            "#[cfg(debug_assertions)]\n            semantic_graph_debug_release,"
        ));
        assert!(source.contains("fn semantic_graph_debug_fixture()"));
        assert!(source.contains("fn semantic_graph_debug_relation_detail("));
        assert!(source.contains("fn semantic_graph_debug_release(session_id: String)"));

        let real_sentinel = configured_root.path().join("real-library-sentinel");
        std::fs::write(&real_sentinel, b"must survive debug release").unwrap();
        super::semantic_graph_debug_release(configured_root.path().display().to_string()).unwrap();
        assert!(
            real_sentinel.exists(),
            "opaque session IDs must never be treated as paths"
        );

        let replacement = super::semantic_graph_debug_fixture().unwrap();
        assert!(super::semantic_graph_debug_fixture_root(&fixture.session_id).is_none());
        assert!(!root.exists(), "replaced server-owned fixture root must be deleted");
        let replacement_root =
            super::semantic_graph_debug_fixture_root(&replacement.session_id).unwrap();
        super::semantic_graph_debug_release(replacement.session_id.clone()).unwrap();
        assert!(super::semantic_graph_debug_fixture_root(&replacement.session_id).is_none());
        assert!(!replacement_root.exists(), "released fixture root must be deleted");
        super::semantic_graph_debug_release(replacement.session_id).unwrap();
        assert!(
            real_sentinel.exists(),
            "repeated release must remain harmless to real data"
        );
    }

    #[test]
    fn relation_backfill_commands_are_registered_and_never_auto_started() {
        let source = include_str!("lib.rs");
        let handlers = source
            .split_once(".invoke_handler(tauri::generate_handler![")
            .expect("generate_handler block")
            .1
            .split_once("])")
            .expect("generate_handler terminator")
            .0;
        for command in [
            "preview_relation_backfill,",
            "start_relation_backfill,",
            "cancel_relation_backfill,",
            "retry_relation_backfill_index,",
        ] {
            assert!(
                handlers.contains(command),
                "missing registered command {command}"
            );
        }
        assert_eq!(
            source.matches("\nfn start_relation_backfill(").count(),
            1,
            "backfill must only have its user-triggered command entrypoint"
        );
        let preview_signature = source
            .split_once("\nfn preview_relation_backfill(")
            .unwrap()
            .1
            .split_once(") ->")
            .unwrap()
            .0;
        assert!(preview_signature.contains("note_ids: Option<Vec<String>>"));
        assert!(!preview_signature.contains("BackfillRequest"));
        let start_signature = source
            .split_once("\nfn start_relation_backfill(")
            .unwrap()
            .1
            .split_once('{')
            .unwrap()
            .0;
        assert!(start_signature.contains("request: ipc::BackfillRequest"));
        assert!(start_signature.contains(") -> Result<(), String>"));
        assert!(source.contains("relation_backfill_progress"));
    }

    #[test]
    fn relation_backfill_spawn_failure_emits_terminal_and_releases_gate() {
        let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let active = std::sync::Arc::new(std::sync::Mutex::new(None));
        let gate = crate::refine::backfill::BackfillGate::acquire(
            std::sync::Arc::clone(&running),
            std::sync::Arc::clone(&active),
            "run-spawn-failure",
        )
        .unwrap();
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_for_failure = std::sync::Arc::clone(&events);
        let worker_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_ran_in_job = std::sync::Arc::clone(&worker_ran);
        let initial = crate::ipc::BackfillProgress {
            run_id: "run-spawn-failure".into(),
            state: "running".into(),
            completed: 0,
            total: 2,
            current_note_id: None,
            failed: vec![],
            rebuild_generation: None,
            index_error: None,
        };

        let error = super::spawn_relation_backfill_worker(
            gate,
            initial,
            |_job| Err(std::io::Error::other("injected spawn failure")),
            move |_initial| {
                worker_ran_in_job.store(true, std::sync::atomic::Ordering::SeqCst)
            },
            move |progress| events_for_failure.lock().unwrap().push(progress),
        )
        .unwrap_err();

        assert!(error.contains("injected spawn failure"));
        assert!(!worker_ran.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            !running.load(std::sync::atomic::Ordering::SeqCst),
            "spawn Err must drop the gate"
        );
        assert!(active.lock().unwrap().is_none());
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].state, "failed");
        assert_eq!((events[0].completed, events[0].total), (0, 2));
        assert!(events[0].failed[0].error.contains("injected spawn failure"));
    }

    #[test]
    fn dirty_index_retry_returns_the_exact_generation_without_running_backfill() {
        let root = tempfile::tempdir().unwrap();
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rebuilds = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rebuilds_in_worker = std::sync::Arc::clone(&rebuilds);
        let attempts_in_spawn = std::sync::Arc::clone(&attempts);
        let scheduler = crate::graph::index::RebuildScheduler::with_rebuilder_and_spawner(
            move |_| {
                rebuilds_in_worker.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(crate::graph::index::BuildStats::default())
            },
            move |job| {
                if attempts_in_spawn.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    Err(std::io::Error::other("injected initial queue failure"))
                } else {
                    job();
                    Ok(())
                }
            },
        );
        assert!(scheduler.request(root.path().to_path_buf(), |_| {}).is_err());
        let statuses = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let statuses_in_emit = std::sync::Arc::clone(&statuses);

        let generation = super::retry_relation_backfill_index_with(
            &scheduler,
            root.path().to_path_buf(),
            move |status| statuses_in_emit.lock().unwrap().push(status),
        )
        .unwrap();

        assert_eq!(generation, 2);
        assert_eq!(
            rebuilds.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let statuses = statuses.lock().unwrap();
        assert!(statuses
            .iter()
            .any(|status| status.generation == generation && status.state == "building"));
        assert!(statuses
            .iter()
            .any(|status| status.generation == generation && status.state == "ready"));
        let source = include_str!("lib.rs");
        assert!(source.contains("retry_relation_backfill_index,"));
        let body = source
            .split_once("fn retry_relation_backfill_index(")
            .unwrap()
            .1
            .split_once("\n}")
            .unwrap()
            .0;
        assert!(!body.contains("relation_executor"));
        assert!(!body.contains("run_batch"));
    }

    #[test]
    fn failed_rebuild_request_never_reports_a_queued_mutation() {
        // 断言中文原文(进程默认语言):与语言切换用例互斥。
        let _lang = crate::i18n::test_lang_guard();
        let result = crate::ipc::KnowledgeMutationResult {
            operation_id: "op_saved".into(),
            entity_id: None,
            rebuild_state: "committed".into(),
            rebuild_generation: None,
        };
        let error = super::mark_knowledge_rebuild_queued(
            result,
            Err(anyhow::anyhow!("injected spawn failure")),
        )
        .unwrap_err();
        assert!(error.contains("操作已保存"));
        assert!(error.contains("自动重试"));
    }

    #[test]
    fn queued_knowledge_mutation_records_the_nonzero_scheduler_generation() {
        let result = crate::ipc::KnowledgeMutationResult {
            operation_id: "op_saved".into(),
            entity_id: None,
            rebuild_state: "committed".into(),
            rebuild_generation: None,
        };

        let queued = super::mark_knowledge_rebuild_queued(result, Ok(37)).unwrap();
        assert_eq!(queued.rebuild_state, "queued");
        assert_eq!(queued.rebuild_generation, Some(37));
    }

    #[test]
    fn http_refine_handoff_runs_only_after_write_and_keeps_dirty_retry_on_spawn_failure() {
        let root = tempfile::tempdir().unwrap();
        let note = root.path().join("notes").join("note-1");
        std::fs::create_dir_all(&note).unwrap();
        let saved = note.join(crate::store::AING_DOC_FILE);
        std::fs::write(&saved, b"saved-document").unwrap();
        let scheduler = crate::graph::index::RebuildScheduler::with_rebuilder_and_spawner(
            |_| Ok(crate::graph::index::BuildStats::default()),
            |_job| Err(std::io::Error::other("injected spawn failure")),
        );

        let error = super::handoff_http_refine_write(Ok(()), || {
            assert_eq!(std::fs::read(&saved).unwrap(), b"saved-document");
            scheduler
                .request(root.path().to_path_buf(), |_| {})
                .map(|_| ())
        })
        .unwrap_err();

        assert!(error.to_string().contains("Aing 已保存"));
        assert!(error.to_string().contains("索引待重试"));
        assert!(root.path().join(".graph-index-dirty").is_file());
        assert_eq!(std::fs::read(&saved).unwrap(), b"saved-document");
    }

    #[test]
    fn http_refine_handoff_does_not_schedule_after_write_failure() {
        let requested = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let requested_in_closure = requested.clone();

        let error = super::handoff_http_refine_write(
            Err(anyhow::anyhow!("injected note write failure")),
            move || {
                requested_in_closure.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("injected note write failure"));
        assert!(!requested.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn failed_person_rebuild_request_keeps_retry_marker_and_reports_saved_merge() {
        // 断言中文原文(进程默认语言):与语言切换用例互斥。
        let _lang = crate::i18n::test_lang_guard();
        let root = tempfile::tempdir().unwrap();
        let scheduler = crate::graph::index::RebuildScheduler::with_rebuilder_and_spawner(
            |_| Ok(crate::graph::index::BuildStats::default()),
            |_job| Err(std::io::Error::other("injected spawn failure")),
        );

        let error = super::queue_person_graph_rebuild_with(
            &scheduler,
            root.path().to_path_buf(),
            "人物合并",
            |_| {},
        )
        .unwrap_err();

        assert!(root.path().join(".graph-index-dirty").exists());
        assert!(error.contains("人物合并已保存"));
        assert!(error.contains("索引待重试"));
        assert!(error.contains("自动重试"));
    }

    #[test]
    fn compat_graph_failure_after_person_rename_still_requests_rebuild() {
        // 断言中文原文(进程默认语言):与语言切换用例互斥。
        let _lang = crate::i18n::test_lang_guard();
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("voiceprints.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "people": {"P1": {"name": "张三"}}
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::create_dir(root.path().join(crate::graph::GRAPH_FILE)).unwrap();
        let spawn_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let spawn_called_for_scheduler = std::sync::Arc::clone(&spawn_called);
        let scheduler = crate::graph::index::RebuildScheduler::with_rebuilder_and_spawner(
            |_| Ok(crate::graph::index::BuildStats::default()),
            move |_job| {
                spawn_called_for_scheduler.store(true, std::sync::atomic::Ordering::SeqCst);
                Err(std::io::Error::other("injected spawn failure"))
            },
        );

        let error = super::rename_entity_with_rebuild(
            root.path().to_path_buf(),
            "P1".into(),
            "张三丰".into(),
            |root| {
                super::queue_person_graph_rebuild_with(
                    &scheduler,
                    root,
                    "人物改名",
                    |_| {},
                )
            },
        )
        .unwrap_err();

        let people = crate::store::VoiceprintStore::new(root.path().to_path_buf()).load();
        assert_eq!(people.people["P1"].name, "张三丰");
        assert!(spawn_called.load(std::sync::atomic::Ordering::SeqCst));
        assert!(root.path().join(".graph-index-dirty").is_file());
        assert!(error.contains("人物改名已保存"));
        assert!(error.contains("索引待重试"));
    }

    #[test]
    fn merged_person_rebuild_runs_after_voiceprint_lock_and_updates_all_read_surfaces() {
        use crate::graph::canonical::{
            CanonicalEntity, CanonicalGraph, CanonicalRelation, RelationOrigin, RelationStatus,
        };
        use std::collections::BTreeMap;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let root = tempfile::tempdir().unwrap();
        let ledger = crate::graph::overrides::KnowledgeLedger {
            schema_version: 1,
            registry: BTreeMap::from([
                (
                    "P1".into(),
                    crate::graph::overrides::RegistryEntity {
                        kind: "person".into(),
                        name: "Loser".into(),
                        aliases: Vec::new(),
                        status: "confirmed".into(),
                    },
                ),
                (
                    "P2".into(),
                    crate::graph::overrides::RegistryEntity {
                        kind: "person".into(),
                        name: "Winner".into(),
                        aliases: Vec::new(),
                        status: "confirmed".into(),
                    },
                ),
                (
                    "kg_project".into(),
                    crate::graph::overrides::RegistryEntity {
                        kind: "project".into(),
                        name: "Project".into(),
                        aliases: Vec::new(),
                        status: "confirmed".into(),
                    },
                ),
            ]),
            legacy_ids: BTreeMap::new(),
            operations: Vec::new(),
        };
        std::fs::write(
            root.path()
                .join(crate::graph::overrides::KNOWLEDGE_FILE),
            serde_json::to_vec(&ledger).unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.path().join("voiceprints.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "people": {"P1": {"name": "Loser"}, "P2": {"name": "Winner"}}
            }))
            .unwrap(),
        )
        .unwrap();

        let voiceprints = crate::store::VoiceprintStore::new(root.path().to_path_buf());
        voiceprints.merge("P1", "P2").unwrap();

        let rebuild_count = Arc::new(AtomicUsize::new(0));
        let rebuild_count_for_worker = Arc::clone(&rebuild_count);
        let scheduler = crate::graph::index::RebuildScheduler::with_rebuilder(move |root| {
            rebuild_count_for_worker.fetch_add(1, Ordering::SeqCst);
            let voiceprints = crate::store::VoiceprintStore::new(root.to_path_buf());
            voiceprints.rename("P2", "Winner after merge")?;
            let people = voiceprints.load();
            anyhow::ensure!(
                crate::store::VoiceprintStore::resolve(&people, "P1") == Some("P2"),
                "merge redirect was not durable"
            );
            crate::graph::index::rebuild_atomic(
                root,
                &CanonicalGraph {
                    entities: BTreeMap::from([
                        (
                            "P2".into(),
                            CanonicalEntity {
                                id: "P2".into(),
                                kind: "person".into(),
                                name: people.people["P2"].name.clone(),
                                aliases: Vec::new(),
                                confirmed: true,
                            },
                        ),
                        (
                            "kg_project".into(),
                            CanonicalEntity {
                                id: "kg_project".into(),
                                kind: "project".into(),
                                name: "Project".into(),
                                aliases: Vec::new(),
                                confirmed: true,
                            },
                        ),
                    ]),
                    mentions: Vec::new(),
                    relations: vec![CanonicalRelation {
                        id: "cr_person_project".into(),
                        subject_id: "P2".into(),
                        predicate: crate::store::RelationPredicate {
                            kind: "responsible_for".into(),
                            label: None,
                        },
                        object_id: "kg_project".into(),
                        confidence: 1.0,
                        valid_from: None,
                        valid_to: None,
                        status: RelationStatus::Current,
                        origin: RelationOrigin::UserAssertion,
                        provider: None,
                        model: None,
                        note_ids: Vec::new(),
                        evidence: Vec::new(),
                    }],
                    pending: Vec::new(),
                },
            )
        });
        let (status_tx, status_rx) = std::sync::mpsc::channel();
        super::queue_person_graph_rebuild_with(
            &scheduler,
            root.path().to_path_buf(),
            "人物合并",
            move |status| {
                let _ = status_tx.send(status);
            },
        )
        .unwrap();
        loop {
            let status = status_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("person rebuild should finish without waiting on the voiceprint lock");
            assert_ne!(status.state, "error", "person rebuild failed");
            if status.state == "ready" {
                break;
            }
        }

        assert_eq!(rebuild_count.load(Ordering::SeqCst), 1);
        assert_eq!(voiceprints.load().people["P2"].name, "Winner after merge");
        let graph = crate::graph::query::semantic_graph(
            root.path(),
            &crate::graph::query::GraphFilter::default(),
        )
        .unwrap();
        assert_eq!(
            graph.nodes.iter().map(|node| node.id.as_str()).collect::<Vec<_>>(),
            ["P2", "kg_project"]
        );
        let detail = crate::graph::query::semantic_entity_detail(
            root.path(),
            "P1",
            &crate::graph::query::GraphFilter::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(detail.id, "P2");
        assert_eq!(detail.name, "Winner after merge");
        assert_eq!(detail.relations.len(), 1);
        let path = crate::graph::path::shortest_path(
            root.path(),
            "P1",
            "kg_project",
            &crate::graph::query::GraphFilter::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(path.entity_ids, ["P2", "kg_project"]);
    }

    #[test]
    fn refine_llm_ready_requires_switch_and_complete_profile() {
        use super::refine_llm_ready;
        let base = crate::settings::Settings::default();
        assert!(!refine_llm_ready(&base), "默认未配置/关闭 → 未就绪");

        let mut s = base.clone();
        s.llm_profiles.push(crate::settings::LlmProfile {
            id: "p1".into(),
            label: "DeepSeek".into(),
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-chat".into(),
            api_key: "sk-xxx".into(),
        });
        s.refine_executor = "llm:p1".into();
        assert!(!refine_llm_ready(&s), "档案齐全但总开关未开 → 仍未就绪");

        s.refine_enabled = true;
        assert!(refine_llm_ready(&s), "开关开且档案三项齐全 → 就绪");

        for field in ["base_url", "model", "api_key"] {
            let mut s2 = s.clone();
            match field {
                "base_url" => s2.llm_profiles[0].base_url.clear(),
                "model" => s2.llm_profiles[0].model.clear(),
                _ => s2.llm_profiles[0].api_key.clear(),
            }
            assert!(!refine_llm_ready(&s2), "{field} 为空 → 未就绪");
        }

        let mut s3 = s.clone();
        s3.refine_executor = "agent:claude".into();
        assert!(!refine_llm_ready(&s3), "执行体是 Agent 时不走 HTTP");
        s3.refine_executor = "llm:ghost".into();
        assert!(!refine_llm_ready(&s3), "悬空引用(档案已删)→ 未就绪");
    }

    #[test]
    fn refine_agent_ready_follows_switch_and_executor() {
        use super::refine_agent_ready;
        let mut s = crate::settings::Settings::default();
        assert!(!refine_agent_ready(&s), "默认未配置 → 不走 Agent");
        s.refine_executor = "agent:claude".into();
        assert!(!refine_agent_ready(&s), "总开关未开 → 不走");
        s.refine_enabled = true;
        assert!(refine_agent_ready(&s), "开关开 + agent 引用 → 尝试(bin 探测留给运行时)");
    }

    // resume_blocked_by_refining_matches_refining_set 已随 Aing 集入内核而删除:
    // 同一语义(按 id 查集合/不误伤其它笔记)由 lifecycle::machine 的
    // concurrent_refines_tracked_independently_by_id 与 RefineRequest 裁决表接管。

    /// 重转写摘要事件可序列化,None 字段不出现(前端契约)。
    #[test]
    fn retranscribe_event_serialization_shape() {
        let e = crate::ipc::RetranscribeEvent {
            note_id: "n1".into(), stage: "all".into(), state: "ok".into(),
            message: None,
            summary: Some(crate::retranscribe::Summary {
                old_segments: 10, new_segments: 8, seed_matched: 5,
                inherited: 2, echo_dropped: 1, failed_segments: 0,
            }),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"new_segments\":8"));
        assert!(!json.contains("message"));

        // error 形态:message Some/summary None——轮询方(uds::retranscribe_status
        // 的 last 字段)读的正是这两种终态形状,这里锁死不许漂移。
        let err_e = crate::ipc::RetranscribeEvent {
            note_id: "n1".into(), stage: "all".into(), state: "error".into(),
            message: Some("笔记正被占用".into()),
            summary: None,
        };
        let err_json = serde_json::to_string(&err_e).unwrap();
        assert!(err_json.contains("\"message\":\"笔记正被占用\""));
        assert!(!err_json.contains("summary"));
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

    /// Fix 1C:录制↔重转写互斥的并发回归测试。直接驱动生产两侧调用的同两个判定函数
    /// （retranscribe_blocks_recording / recording_blocks_retranscribe），用真实的两条
    /// 操作系统线程重演 Dekker 写后读协议 1000 轮，断言两侧不可能在同一轮都判定"通过"。
    ///
    /// 每轮由主线程用 rendezvous channel 给两条racer线程发"go"，racer各自跑一遍生产
    /// 侧的完整协议（S 侧:置 running=true → 读槽 → 命中则回滚；R 侧:占槽 → 读
    /// running/session → 命中则清槽），把"最终是否通过"回传主线程；主线程收完双方结果
    /// 后断言不同时为真，再复位 running/slot 供下一轮使用。channel 往返本身不消除
    /// 并发——两条线程在收到 go 之后到把结果送回之前是真正并行执行的，判定函数内部
    /// 的锁竞争窗口原样保留，这正是要验证的东西。
    #[test]
    fn recording_retranscribe_mutex_never_double_passes() {
        use std::sync::mpsc;
        use std::sync::{Arc, Mutex};

        let running: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let slot: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
        const ROUNDS: usize = 1000;

        // S 侧 racer:模拟 spawn_session 的 running 置位 + Dekker 权威判定。
        let (tx_go_s, rx_go_s) = mpsc::channel::<()>();
        let (tx_res_s, rx_res_s) = mpsc::channel::<bool>();
        let running_s = running.clone();
        let slot_s = slot.clone();
        let h_s = std::thread::spawn(move || {
            while rx_go_s.recv().is_ok() {
                let mut passed = {
                    let mut r = running_s.lock().unwrap();
                    if *r {
                        false
                    } else {
                        *r = true;
                        true
                    }
                };
                if passed && super::retranscribe_blocks_recording(&slot_s) {
                    // 回滚:与生产 spawn_session 同款纪律（测试里没有 generation 过期
                    // 的问题，直接复位）。
                    *running_s.lock().unwrap() = false;
                    passed = false;
                }
                let _ = tx_res_s.send(passed);
            }
        });

        // R 侧 racer:模拟 do_retranscribe 的占槽 + Dekker 权威判定。
        let (tx_go_r, rx_go_r) = mpsc::channel::<()>();
        let (tx_res_r, rx_res_r) = mpsc::channel::<bool>();
        let running_r = running.clone();
        let slot_r = slot.clone();
        let h_r = std::thread::spawn(move || {
            while rx_go_r.recv().is_ok() {
                let mut occupied = {
                    let mut s = slot_r.lock().unwrap();
                    if s.is_some() {
                        false
                    } else {
                        *s = Some(("n1".into(), "decode".into()));
                        true
                    }
                };
                if occupied && super::recording_blocks_retranscribe(&running_r, false) {
                    *slot_r.lock().unwrap() = None;
                    occupied = false;
                }
                let _ = tx_res_r.send(occupied);
            }
        });

        let mut both_passed_count = 0usize;
        for round in 0..ROUNDS {
            tx_go_s.send(()).unwrap();
            tx_go_r.send(()).unwrap();
            let passed_s = rx_res_s.recv().unwrap();
            let passed_r = rx_res_r.recv().unwrap();
            if passed_s && passed_r {
                both_passed_count += 1;
            }
            assert!(
                !(passed_s && passed_r),
                "round {round}: 录制与重转写同一轮都判定通过——互斥协议破了"
            );
            // 轮末复位，供下一轮使用。
            *running.lock().unwrap() = false;
            *slot.lock().unwrap() = None;
        }
        assert_eq!(both_passed_count, 0);

        drop(tx_go_s);
        drop(tx_go_r);
        h_s.join().unwrap();
        h_r.join().unwrap();
    }

    /// mixed_playback_info 的读数拼装:轨在+对账过 → untrusted None;seek 表从
    /// MixInfo 原样透传;mic 带 clean → ab_caveat(A 侧多一级清洗,不可直比)。
    #[test]
    fn mixed_playback_assembly_rules() {
        use crate::store::audio::{AudioMeta, CleanInfo, MixInfo, TrackInfo, TrackMeta};
        let mut meta = AudioMeta::default();
        meta.tracks.insert("mic".into(), TrackMeta {
            duration_ms: Some(1000),
            clean: Some(CleanInfo { delay_ms: 120, confidence: 0.9, segments: 3, neural: Some(false) }),
            ..Default::default()
        });
        meta.tracks.insert("mixed".into(), TrackMeta {
            mix: Some(MixInfo {
                origin: "live".into(),
                seek_offset_ms: [("system".to_string(), 120u64)].into_iter().collect(),
                track_ms: 1000,
            }),
            ..Default::default()
        });
        let track = Some(TrackInfo {
            source: "mixed".into(), path: "mixed.wav".into(),
            offset_ms: 0, duration_ms: 1000, waveform: None,
        });
        let info = super::assemble_mixed_playback(&meta, track);
        assert!(info.untrusted.is_none(), "对账一致应可信: {:?}", info.untrusted);
        assert_eq!(info.seek_offset_ms.get("system"), Some(&120));
        assert!(info.ab_caveat, "mic 带 clean 记录必须亮不可直比告警");
        // 无轨:untrusted 不给原因(前端走「生成」动作,不是「不可信」态)。
        let none = super::assemble_mixed_playback(&meta, None);
        assert!(none.track.is_none() && none.untrusted.is_none());
    }

    /// codex 第三轮 P1:regen↔Aing 互查判据——同 note_id 占槽才让步,不误伤
    /// 其它笔记的正常 Aing(镜像 retranscribing_blocks_refine 的语义)。
    #[test]
    fn mixed_regen_blocks_refine_matches_same_note_only() {
        let slot: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(Some("n1".into()));
        assert!(super::mixed_regen_blocks_refine(&slot, "n1"));
        assert!(!super::mixed_regen_blocks_refine(&slot, "n2"));
        *slot.lock().unwrap() = None;
        assert!(!super::mixed_regen_blocks_refine(&slot, "n1"));
    }

    /// 补生成占槽判定(纯函数):占了就 busy,清了就不 busy。与录制/重转写的
    /// 互斥接线依赖这一判定(双向 Dekker 写后读,证明同上方 S/R 侧)。
    #[test]
    fn mixed_regen_slot_blocks_and_clears() {
        let slot: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
        assert!(!super::mixed_regen_busy(&slot));
        *slot.lock().unwrap() = Some("n1".into());
        assert!(super::mixed_regen_busy(&slot));
        *slot.lock().unwrap() = None;
        assert!(!super::mixed_regen_busy(&slot));
    }
}

#[cfg(test)]
mod input_volume_parse_tests {
    use super::parse_input_volume;

    #[test]
    fn parses_trims_and_clamps() {
        assert_eq!(parse_input_volume("30\n"), Some(30));
        assert_eq!(parse_input_volume("100"), Some(100));
        assert_eq!(parse_input_volume("150"), Some(100)); // 越界截到 100
        assert_eq!(parse_input_volume(" 42 \n"), Some(42)); // 含空白
        assert_eq!(parse_input_volume(""), None);
        assert_eq!(parse_input_volume("abc"), None);
        assert_eq!(parse_input_volume("missing value"), None); // 无输入设备时 osascript 的输出
    }
}
