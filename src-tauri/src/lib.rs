mod audio;
mod feedback;
mod logging;
pub mod pipeline;
pub mod asr;
mod ipc;
pub mod models;
mod session;
mod settings;
mod shortcuts;
mod store;
mod i18n;
mod player;
mod player_align;
mod player_gate;
mod tray;
mod update;
pub mod diar;
mod ailog;
mod refine;
mod retranscribe;
mod graph;
pub mod mcp;
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
    embedder_cache: Arc<Mutex<Option<Box<dyn diar::SpeakerEmbedder>>>>,
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
            graph_scheduler: graph::index::RebuildScheduler::default(),
            relation_backfill_running: Arc::new(AtomicBool::new(false)),
            relation_backfill_cancel: Arc::new(AtomicBool::new(false)),
            relation_backfill_run_id: Arc::new(Mutex::new(None)),
            retranscribing: Arc::new(Mutex::new(None)),
            retranscribe_last: Arc::new(Mutex::new(None)),
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
    let Ok(root) = data_root(app) else {
        eprintln!("声纹库路径不可用，本场开录跳过种子注入（不影响录制）");
        return Vec::new();
    };
    let vp = store::VoiceprintStore::new(root).load();
    // 嵌入模型标签与当前选型不一致(切换后台重建尚未完成的窗口)时不注入种子:
    // 不同模型的向量空间不可混比,错认比不认糟。此门禁足以杜绝一切跨空间比较——
    // 种子被跳过后,本场新簇只在新空间内互比;既有人物无种子即不会被命中回写。
    let cur = app
        .path()
        .app_data_dir()
        .map(|d| settings::load(&d).speaker_model)
        .unwrap_or_default();
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
/// recognizer_cache 与 embedder_cache 策略完全一致，故共用一个泛型实现。
fn stash_model<T: ?Sized>(cache: &Arc<Mutex<Option<Box<T>>>>, m: Option<Box<T>>) {
    if let Some(m) = m {
        *cache.lock().unwrap() = Some(m);
    }
}

/// HTTP(OpenAI 兼容)Aing 配置是否齐备（开关开、provider 非 agent、三项均非空）：
/// 抽成纯函数供 spawn_refine 判定与单测，避免把「要不要发起网络请求」这条判断逻辑
/// 埋进整个后台线程闭包里难以单独验证。provider 值未知(手改 settings.json)时按
/// openai 对待——那是默认执行体,坏值不该让 Aing 整个哑掉。
fn refine_llm_ready(s: &settings::Settings) -> bool {
    s.refine_enabled
        && s.refine_provider != "agent"
        && !s.refine_base_url.is_empty()
        && !s.refine_model.is_empty()
        && !s.refine_api_key.is_empty()
}

/// Agent(本机 CLI 经 MCP 读写回)Aing 是否应当尝试。bin 探测留到运行时——探测结果
/// 随用户装/卸 CLI 变化,不该在这里静态判定;解析失败由 agent 分支落 failed 并留日志。
fn refine_agent_ready(s: &settings::Settings) -> bool {
    s.refine_enabled && s.refine_provider == "agent"
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
    lc.report(lifecycle::machine::Msg::RefineProgress {
        note_id: note_id.clone(),
        stage: "all".into(),
        state: "running".into(),
    });
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
    if retranscribing_blocks_refine(&state.retranscribing, &note_id) {
        eprintln!(
            "refine({note_id}): 与重转写占槽发生竞态,放弃本次 Aing(见 spawn_refine Fix 2 注释)"
        );
        // 把上面刚插入的 Aing 集清干净,与 worker 正常收尾同款消息序(先 all/failed
        // 再 RefineFinished),前端/轮询方看到的事件形状不因走了这条放弃路径而变化。
        lc.report(lifecycle::machine::Msg::RefineProgress {
            note_id: note_id.clone(),
            stage: "all".into(),
            state: "failed".into(),
        });
        lc.report(lifecycle::machine::Msg::RefineFinished { note_id });
        return;
    }
    std::thread::spawn(move || {
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
            lc.report(lifecycle::machine::Msg::RefineProgress {
                note_id: note_id.clone(),
                stage: stage.into(),
                state: st.into(),
            });
        };
        let enqueued = std::cell::Cell::new(false);
        // 全局串行闸:在起 ORT 线程池的重活之前排队,同一时刻只放一篇过。守卫在 catch_unwind
        // 之前取、随线程体自然释放——被捕获的 panic 不经此守卫展开,不会毒化(仍加 poison 兜底)。
        // 多篇同时点会各自先发一条 "all/running"(显示「Aing 中…」),但实际串行等锁逐篇跑。
        let _aing_gate = AING_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let result: std::thread::Result<anyhow::Result<()>> =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // (第一条 "all/running" 已在 spawn 前由入口同步发出,见上)
                let root = notes_dir(&app)?;
                let dir = root.join(&note_id);
                // 与 get_note 同款只读加载：全部 segments（已按 get_note 语义过滤空白 +
                // 排序）+ speakers 表。
                let note = store::NoteStore::new(root).load(&note_id)?;
                let mut embedder = match diar::SherpaEmbedder::new(&speaker_model_path(&app)) {
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
                let mut http_refine_handled = false;
                if refine_agent_ready(&s) {
                    telemetry::track(
                        &app,
                        telemetry::Event::NoteRefined {
                            provider: telemetry::Provider::classify(&s.refine_provider, &s.refine_base_url),
                        },
                    );
                    report("llm", "running");
                    let resolved = refine::agent::AgentKind::from_key(&s.refine_agent)
                        .and_then(|k| refine::agent::resolve_bin(k, &s.refine_agent_bin).map(|b| (k, b)));
                    match resolved {
                        Some((kind, bin)) => {
                            if let Err(e) = refine::agent::run_refine(
                                &dir,
                                &note_id,
                                kind,
                                &bin,
                                &s.refine_agent_model,
                                log_ctx.as_ref(),
                            ) {
                                eprintln!("refine: agent Aing 失败: {e}");
                            }
                            // Agent 经 MCP 写的是盘上文件:重载同步内存 doc(成功时
                            // llm=done + 修订文本;失败时盘上仍是 off,下面统一降级)。
                            if let Some(d) = store::load_refined(&dir) {
                                doc = d;
                            }
                        }
                        None => eprintln!(
                            "refine: 未找到 {} 的 CLI(可在 AI 页指定可执行文件路径),Agent Aing 跳过",
                            s.refine_agent
                        ),
                    }
                    // 与 run_llm 的 F4 同一语义:本轮没落成 done 就是 failed,盘上与
                    // 事件保持一致,不把「off」留给 UI 当作"没配置"误读。
                    if doc.stages.llm != "done" {
                        doc.stages.llm = "failed".into();
                        if let Err(e) = store::write_refined_atomic(&dir, &doc) {
                            eprintln!("refine: agent 失败态落盘失败: {e}");
                        }
                    }
                } else if refine_llm_ready(&s) {
                    http_refine_handled = true;
                    telemetry::track(
                        &app,
                        telemetry::Event::NoteRefined {
                            provider: telemetry::Provider::classify(&s.refine_provider, &s.refine_base_url),
                        },
                    );
                    report("llm", "running");
                    let cfg = refine::llm::LlmConfig {
                        base_url: s.refine_base_url.clone(),
                        model: s.refine_model.clone(),
                        api_key: s.refine_api_key.clone(),
                    };
                    let write_result = refine::run_llm(
                        &dir,
                        &mut doc,
                        &cfg,
                        &s.refine_model,
                        log_ctx.as_ref(),
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
                    }
                }
                report("llm", &doc.stages.llm);
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
                if (refine_agent_ready(&s) || refine_llm_ready(&s))
                    && store::writer::is_default_title(&note.meta.title)
                {
                    // 标题跟随 Aing 执行体:Agent 模式一发一收(无 MCP、无工具),
                    // HTTP 模式走原 chat completions。两边同一长度守卫、同样失败即放弃。
                    let title = if refine_agent_ready(&s) {
                        refine::agent::AgentKind::from_key(&s.refine_agent)
                            .and_then(|k| refine::agent::resolve_bin(k, &s.refine_agent_bin).map(|b| (k, b)))
                            .ok_or_else(|| anyhow::anyhow!("Agent CLI 不可用"))
                            .and_then(|(kind, bin)| {
                                refine::agent::gen_title(
                                    kind,
                                    &bin,
                                    &s.refine_agent_model,
                                    &doc.paragraphs,
                                    log_ctx.as_ref(),
                                )
                            })
                    } else {
                        let cfg = refine::llm::LlmConfig {
                            base_url: s.refine_base_url.clone(),
                            model: s.refine_model.clone(),
                            api_key: s.refine_api_key.clone(),
                        };
                        refine::llm::gen_title(&cfg, &doc.paragraphs, log_ctx.as_ref())
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
        match &result {
            Ok(Ok(())) => report("all", "done"),
            Ok(Err(e)) => {
                eprintln!("refine({note_id}): 管线失败: {e}");
                report("all", "failed");
            }
            Err(_) => {
                eprintln!("refine({note_id}): 管线 panic");
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
        // 原 refining.remove 的时机(收尾事件与兜底转码之后):把该 id 移出内核
        // Aing 集。按 id 移除,并发 Aing 的其它笔记不受波及(与旧 set.remove 一致)。
        lc.report(lifecycle::machine::Msg::RefineFinished { note_id });
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

fn qwen3_dir() -> PathBuf {
    models::root().join(models::QWEN3_DIR)
}

/// 识别器唯一实例化点：按选型造对应识别器，装进 trait 对象。preload 与 spawn_session
/// 槽空兜底都经此，杜绝两处各写一份 new 而漏掉某一选型。
/// provider 经 settings.asr_provider 覆盖(实验字段,默认 None = CPU)。
fn new_recognizer(asr_model: &str, provider: Option<String>) -> anyhow::Result<Box<dyn asr::Recognizer>> {
    if asr_model == settings::ASR_WHISPER {
        Ok(Box::new(asr::whisper::WhisperRecognizer::new(&whisper_dir(), provider)?) as Box<dyn asr::Recognizer>)
    } else if asr_model == settings::ASR_PARAFORMER {
        Ok(Box::new(asr::paraformer::ParaformerRecognizer::new(&paraformer_dir(), provider)?) as Box<dyn asr::Recognizer>)
    } else if asr_model == settings::ASR_QWEN3 {
        // 热词暂传 None:等术语库(改进计划 Phase 5)落地后从设置注入。
        Ok(Box::new(asr::qwen3::Qwen3Recognizer::new(&qwen3_dir(), provider, None)?) as Box<dyn asr::Recognizer>)
    } else {
        Ok(Box::new(asr::sense_voice::SenseVoiceRecognizer::new(&sense_voice_dir(), provider)?) as Box<dyn asr::Recognizer>)
    }
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
    models::root().join(models::speaker_model_file(&model))
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
fn source_display(s: Source) -> String {
    match s {
        Source::Mic => tr!("麦克风", "Microphone"),
        Source::System => tr!("系统声音", "System audio"),
    }
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

/// Aing↔重转写占槽后互查闭环的 A 侧（spawn_refine）判定：给定重转写槽当前值与
/// 本次 Aing 的 note_id，判断是否命中"同一笔记正被重转写占槽"（true=须清刚插入
/// 的 Aing 集并放弃本次 Aing，不 spawn 工作线程）。纯函数，从 spawn_refine 里的
/// 判据原样抽出——完整互斥证明见 spawn_refine/do_retranscribe 两处 Fix 2 注释
/// （codex 第三轮）。真实竞态涉及 lifecycle actor 信箱 + 后台线程交错，无法在
/// 单元测试里驱动出货真价实的并发窗口；这个纯函数至少把"槽命中同 note_id 时
/// 该让步"这条判据本身纳入测试，而不是只靠现场审读代码。
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
    embedder_cache: Arc<Mutex<Option<Box<dyn diar::SpeakerEmbedder>>>>,
    transcode: Arc<store::transcode::TranscodeQueue>,
    retranscribing: Arc<Mutex<Option<(String, String)>>>,
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
            // P1 影子回报:仅当前代的启动失败走到这里(过期线程已提前 return),
            // 通知 actor 内核回到 Idle。后台线程投递,不等待(见 actor.rs 死锁注记②)。
            app.state::<lifecycle::LifecycleHandle>().report(lifecycle::machine::Msg::SessionFailed);
        };

        // 0) 一次性读设置：record_system_only / keep_audio / language_filter /
        // keep_output_volume / mix_track / 识别方式与云端凭证 同源同快照（避免多次
        // load 读到并发写入的不同代）。app_data_dir 不可用时整体回落 Settings::default
        // （仅系统声=否、保留音频=是、语言过滤=开、保持外放音量=否、混音成品轨=否、
        // 识别方式=本地），绝不因读设置失败改变现状行为。位置提到取模型之前：识别
        // 方式决定要不要取常驻识别器。language_filter 在下方 start_session 处消费。
        let cfg = app.path().app_data_dir().map(|d| settings::load(&d)).unwrap_or_default();
        let (record_system_only, keep_audio, language_filter, keep_output_volume, mix_track) = (
            cfg.record_system_only,
            cfg.keep_audio,
            cfg.language_filter,
            cfg.keep_output_volume,
            cfg.mix_track,
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
                None => match new_recognizer(&current_asr(&app), current_asr_provider(&app)) {
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
        let embedder = embedder_cache.lock().unwrap().take();
        // 声纹模型是否就绪 → 决定前端是否显示「说话人区分不可用」降级横幅。
        let diarization = if embedder.is_some() { "on" } else { "unavailable" }.to_string();

        // 2) 构建源（各自 VAD）。默认建麦克风（必备）+ 系统声音（可降级）；
        // record_system_only 时刻意不建麦克风（跳过 VPIO/mic VAD），源列表只剩 System。
        let vad_path = models::root().join("silero_vad.onnx");
        let mut sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> = Vec::new();
        // 每源健康计数(FrameTap 写、pipeline_health 读),随 ActiveSession 存活一场。
        let mut session_health: Vec<(Source, Arc<SourceHealth>)> = Vec::new();
        // 两源首个真实帧共享一个单调时钟原点。谁先到谁把 0 点钉住,后到源把偏移
        // 写进 SourceHealth,供 mixed sink 在 16k 时间轴上插入准确的前导静音。
        let timeline_origin = Arc::new(OnceLock::new());
        if !record_system_only {
            let mic_seg = match new_silero(&vad_path) {
                Ok(s) => s,
                Err(e) => {
                    stash_model(&recognizer_cache, recognizer);
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
            // 采集栈:TappedCapture(ResilientCapture(真实采集))。
            //  - Resilient:流错误/失联时工厂重建采集,复用同一帧通道,worker 无感;
            //  - Tap:健康统计 + 断流期按墙钟补零(时间轴不塌,双轨对齐不断裂),
            //    其失联通知(>3s 无帧)踢 Resilient 重启——覆盖 VPIO 这类未接
            //    错误回调的后端,与 cpal 的 CaptureEvent 快路径互补。
            #[cfg(target_os = "macos")]
            let mic_factory: audio::resilient::CaptureFactory = if keep_output_volume {
                Box::new(|| {
                    let (etx, erx) = crossbeam_channel::unbounded();
                    (
                        Box::new(audio::microphone::Microphone::with_events(etx))
                            as Box<dyn AudioCapture>,
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
            let mic_resilient = audio::resilient::ResilientCapture::new(mic_factory, {
                let app = app.clone();
                let health = mic_health.clone();
                let app2 = app.clone();
                audio::resilient::ResilientNotify {
                    on_recovered: Some(Box::new(move || {
                        health.restarts.fetch_add(1, Ordering::Relaxed);
                        let _ = app.emit(
                            "source_health",
                            ipc::SourceHealthEvent {
                                source: "mic".into(),
                                state: "recovered".into(),
                            },
                        );
                    })),
                    on_lost: Some(Box::new(move || {
                        let _ = app2.emit(
                            "source_health",
                            ipc::SourceHealthEvent { source: "mic".into(), state: "lost".into() },
                        );
                    })),
                }
            });
            let mic_kicker = mic_resilient.kicker();
            let mic_notify = TapNotify {
                on_stall: Some(Box::new(move || {
                    eprintln!("麦克风采集失联(>3s 无帧):静音填充维持时间轴,触发自愈重启");
                    let _ = mic_kicker.try_send(());
                })),
                on_recover: Some(Box::new(|| eprintln!("麦克风采集恢复,静音填充结束"))),
            };
            let mic: Box<dyn AudioCapture> = Box::new(TappedCapture::new_with_timeline_origin(
                Box::new(mic_resilient),
                Source::Mic,
                TapPolicy::mic(),
                mic_health.clone(),
                mic_notify,
                timeline_origin.clone(),
            ));
            session_health.push((Source::Mic, mic_health));
            sources.push((Source::Mic, mic, mic_seg));
        }
        // record_system_only 且非 macOS/Windows(如 Linux):System 源不存在,源列表
        // 将为空,start_session 会因无源可启动返回 Err、开录失败——正是 required
        // 守卫要兜住的场景。macOS 走 SCK、Windows 走 WASAPI loopback(下方两块)。

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
                    let sys_resilient =
                        audio::resilient::ResilientCapture::new(sys_factory, {
                            let app = app.clone();
                            let health = sys_health.clone();
                            let app2 = app.clone();
                            audio::resilient::ResilientNotify {
                                on_recovered: Some(Box::new(move || {
                                    health.restarts.fetch_add(1, Ordering::Relaxed);
                                    let _ = app.emit(
                                        "source_health",
                                        ipc::SourceHealthEvent {
                                            source: "system".into(),
                                            state: "recovered".into(),
                                        },
                                    );
                                })),
                                on_lost: Some(Box::new(move || {
                                    let _ = app2.emit(
                                        "source_health",
                                        ipc::SourceHealthEvent {
                                            source: "system".into(),
                                            state: "lost".into(),
                                        },
                                    );
                                })),
                            }
                        });
                    let sys_kicker = sys_resilient.kicker();
                    let sys_notify = TapNotify {
                        on_stall: Some(Box::new(move || {
                            eprintln!("系统声音采集失联(>5s 无帧):静音填充维持时间轴,触发自愈重启");
                            let _ = sys_kicker.try_send(());
                        })),
                        on_recover: Some(Box::new(|| eprintln!("系统声音采集恢复"))),
                    };
                    let sys: Box<dyn AudioCapture> =
                        Box::new(TappedCapture::new_with_timeline_origin(
                            Box::new(sys_resilient),
                            Source::System,
                            TapPolicy::system_sck(),
                            sys_health.clone(),
                            sys_notify,
                            timeline_origin.clone(),
                        ));
                    session_health.push((Source::System, sys_health));
                    sources.push((Source::System, sys, sys_seg));
                }
                Err(e) => {
                    // 系统声音 VAD 构建失败非致命：不发 error 状态（避免闪烁），
                    // 静默跳过该源；classify_system 会因 System 既不在 active 也不在
                    // failed 里而归类为 "unavailable"，UI 仍会显示降级横幅。
                    eprintln!("系统声音 VAD 构建失败，降级为仅麦克风: {e}");
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
                    let sys_resilient =
                        audio::resilient::ResilientCapture::new(sys_factory, {
                            let app = app.clone();
                            let health = sys_health.clone();
                            let app2 = app.clone();
                            audio::resilient::ResilientNotify {
                                on_recovered: Some(Box::new(move || {
                                    health.restarts.fetch_add(1, Ordering::Relaxed);
                                    let _ = app.emit(
                                        "source_health",
                                        ipc::SourceHealthEvent {
                                            source: "system".into(),
                                            state: "recovered".into(),
                                        },
                                    );
                                })),
                                on_lost: Some(Box::new(move || {
                                    let _ = app2.emit(
                                        "source_health",
                                        ipc::SourceHealthEvent {
                                            source: "system".into(),
                                            state: "lost".into(),
                                        },
                                    );
                                })),
                            }
                        });
                    // 环回静默是常态(policy stall_after=None,tap 不判失联),
                    // 自愈只由 cpal 错误事件驱动,kicker 不接。
                    let sys: Box<dyn AudioCapture> =
                        Box::new(TappedCapture::new_with_timeline_origin(
                            Box::new(sys_resilient),
                            Source::System,
                            TapPolicy::system_loopback(),
                            sys_health.clone(),
                            TapNotify::none(),
                            timeline_origin.clone(),
                        ));
                    session_health.push((Source::System, sys_health));
                    sources.push((Source::System, sys, sys_seg));
                }
                Err(e) => {
                    eprintln!("系统声音 VAD 构建失败，降级为仅麦克风: {e}");
                }
            }
        }

        // 软件回声消除(WebRTC AEC3):「保持外放音量」下 VPIO 不启动,改由本模块以
        // system 采集流为远端参考,把外放回声从 mic 波形里消掉——mic 路只剩本人声音,
        // 文本级回声去重链降级为兜底。仅 mic+system 双源齐备才有意义;初始化失败
        // 降级为无 AEC(行为同引入前),绝不挡录制。VPIO 模式(默认)不叠加软件 AEC。
        // Windows 恒尝试:该平台无 VPIO 可选,软件 AEC 是唯一声学消回声路径
        // (当前为 stub,构造返回 Err → 走下方降级日志,文本级回声去重兜底)。
        let mut aec_roles: Vec<(Source, audio::aec::AecRole)> = Vec::new();
        if (keep_output_volume || cfg!(windows))
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
        let writer = match notes_dir(&app).and_then(|d| match &target {
            NoteTarget::New => store::writer::NoteWriter::create(&d, chrono::Local::now()),
            NoteTarget::Resume(id) => store::writer::NoteWriter::resume(&d, id),
        }) {
            Ok(w) => w,
            Err(e) => {
                stash_model(&recognizer_cache, recognizer);
                stash_model(&embedder_cache, embedder);
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
        let seeds = load_voiceprint_seeds(&app);
        let mut registry =
            crate::diar::registry::SpeakerRegistry::with_seeds(&registry_snap, &seeds);
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
        //
        // 录制期产物装配移交 pipeline::recording_sink:mix_track 开启时多落一条
        // mixed.wav(方案 B)。keep_audio=false 时不调 build_sinks、直接给空 Vec,
        // 关闭音频保留不该产生任何轨(含成品轨),行为与此前完全一致。build_sinks
        // 没有 keep_audio 参数,短路必须留在这层调用方。
        //
        // sources 在此处只是"配置期建了 capture 对象"的源(可能活跃),真正的
        // capture.start() 在 spawn_session 里才跑,启动失败的源其 sink 随 worker 一起
        // 丢弃(session.rs)——mix_track 装配时无法区分"确实活跃"与"仅配置存在",
        // 这个缺口由 MixedSink 的队列/窗口守卫与收尾 seen 检查兜住(一源不喂料就
        // 放弃成品轨,不影响两条源轨),此处不为此调整启动顺序。
        let (audio_sinks, audio_joins, audio_activity) = if keep_audio {
            let srcs: Vec<Source> = sources.iter().map(|(s, _, _)| *s).collect();
            let w = pipeline::recording_sink::build_sinks_with_first_offsets(
                &note_dir,
                base_ms,
                &srcs,
                &session_health,
                mix_track,
            );
            (w.sinks, w.joins, w.activity)
        } else {
            (Vec::new(), Vec::new(), Vec::new())
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
        // 保留外语真实发言。值在上方与 record_system_only/keep_audio 同一次 settings
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
                            match store.upsert_from_session(&snaps, &chrono::Local::now().to_rfc3339()) {
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
                // AbortSession → 带源名 fail);system_only 下 required=[System],改由
                // System 缺失触发同一条拆除路径。
                if let Some(&missing) = required_sources(record_system_only)
                    .iter()
                    .find(|s| !start.active.contains(s))
                {
                    let (r, e) = start.handle.stop(); // 先排干可能已产生的其它源 finals
                    stash_model(&recognizer_cache, r);
                    stash_model(&embedder_cache, e);
                    // 排干的 finals 已作为 Pipeline 消息入队(worker 已 join,happens-before
                    // 本条投递),abort 恒在它们之后执行——内容先落盘再按 abort 语义收尾。
                    // note_id 携带本会话身份(P2 对账加固):actor 侧核对与槽内是否一致。
                    lc.report(lifecycle::machine::Msg::AbortSession { note_id: note_id.clone() });
                    let name = source_display(missing);
                    let err = start.failed.iter()
                        .find(|(s, _)| *s == missing)
                        .map(|(_, msg)| {
                            format!(
                                "error: {}",
                                tr!("{name}未能启动: {msg}", "{name} failed to start: {msg}")
                            )
                        })
                        .unwrap_or_else(|| {
                            format!("error: {}", tr!("{name}未能启动", "{name} failed to start"))
                        });
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
                    started: std::time::Instant::now(),
                    base_ms,
                    paused_at: None,
                    paused_accum: std::time::Duration::ZERO,
                    audio_joins,
                    audio_activity,
                    health: session_health,
                    note_dir: note_dir.clone(),
                });
                drop(running_guard);
                let _ = app.emit(
                    "status",
                    ipc::StatusEvent { state: "recording".into(), system_audio, note_id: note_id.clone(), diarization, elapsed_ms: base_ms },
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
                stash_model(&embedder_cache, se.embedder);
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
        NoteTarget::New,
    );
    if result.is_ok() {
        if let Ok(dir) = app.path().app_data_dir() {
            let source =
                telemetry::RecordSource::from_settings(settings::load(&dir).record_system_only);
            telemetry::track(app, telemetry::Event::RecordingStarted { source });
        }
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
        NoteTarget::Resume(note_id),
    );
    if result.is_ok() {
        if let Ok(dir) = app.path().app_data_dir() {
            let source =
                telemetry::RecordSource::from_settings(settings::load(&dir).record_system_only);
            telemetry::track(app, telemetry::Event::RecordingStarted { source });
        }
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
    let (returned, embedder) = s.handle.stop(); // 排干 finals：所有 append 消息在此全部入队
    stash_model(&state.recognizer_cache, returned);
    stash_model(&state.embedder_cache, embedder);
    // 分段 worker 已 join → audio sink 已 drop → 写盘线程排干后自退,join 保证
    // finalize 前 WAV 头已收尾(正常情况下队列近空,瞬时完成)。
    for j in s.audio_joins {
        let _ = j.join();
    }
    // 只覆盖本场 writer 真正成功追加过的源。配置过但启动失败、活跃却无帧、以及
    // keep_audio=false 的续录都保留旧 sync,不会拿旧 WAV 配本场零计数造假。
    persist_track_sync(&s.note_dir, s.base_ms, wall_ms, &s.health, &s.audio_activity);
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
        ipc::StatusEvent { state: "stopped".into(), system_audio: String::new(), note_id, diarization: String::new(), elapsed_ms: 0 },
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
        }
    };
    let _ = app.emit("status", ev);
    Ok(())
}

#[tauri::command]
fn pause_recording(app: AppHandle) -> Result<(), String> {
    // 薄壳(P1 改道):经 lifecycle actor 信箱串行执行,执行体仍是 do_pause_recording。
    app.state::<lifecycle::LifecycleHandle>().command(lifecycle::Cmd::Pause)
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
        }
    };
    let _ = app.emit("status", ev);
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

/// 读取已落盘的 Aing 结果（refined.json）；从未 Aing 过 / Aing 在前置阶段就失败到没能落盘
/// 时返回 None，前端据此回落展示原始 segments。
/// 关联了库人物的段落做只读 join：展示名跟随声纹库现名（会议搭子里改名 → 历史修订稿
/// 跟着变），person_id 归一到 merge 后的 winner。只影响返回值，不落盘。
#[tauri::command]
fn get_refined(app: AppHandle, id: String) -> Result<Option<store::RefinedDoc>, String> {
    store::validate_note_id(&id).map_err(|e| e.to_string())?;
    let dir = notes_dir(&app).map_err(|e| e.to_string())?.join(&id);
    Ok(store::load_refined_for_display(&dir).map(|mut doc| {
        if doc.paragraphs.iter().any(|p| p.person_id.is_some()) {
            if let Ok(root) = data_root(&app) {
                let vp = store::VoiceprintStore::new(root).load();
                store::join_library_names(&mut doc, &vp);
            }
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
pub(crate) fn do_retranscribe(app: &AppHandle, id: &str, input: &str) -> Result<(), String> {
    store::validate_note_id(id).map_err(|e| e.to_string())?;
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
    spawn_retranscribe(app.clone(), id.to_string(), input == "mixed");
    Ok(())
}

/// 重转写后台线程:catch_unwind 兜 panic,独立识别器/嵌入器实例(不碰常驻缓存槽——
/// 那两槽是录制会话的常驻资源,重转写是离线一次性任务,混用会让二者互相饿死对方)。
/// 事件不经 lifecycle actor 直发(见 ipc::RetranscribeEvent 注释):重转写与录制会话
/// 全局互斥,不存在与管线事件的排序耦合,直发省一层转发不丢语义。
fn spawn_retranscribe(app: tauri::AppHandle, note_id: String, mixed: bool) {
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
            let dir = notes_dir(&app).map_err(|e| e.to_string())?.join(&note_id);
            // NoteLock 在 worker 内 acquire 且持有全程(run() 要求调用方贯穿持锁)。
            // 文案不写"或转码中":转码走队列不持 NoteLock,与此锁失败的实际成因无关
            // (会撞这把锁的只有录制/编辑写手柄);写成"转码中"是与事实不符的误导。
            let lock = store::notelock::NoteLock::acquire(&dir)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| tr!(
                    "笔记正被占用(录制/编辑中),稍后再试",
                    "The note is busy (recording or being edited); try again later"
                ))?;
            // 独立识别器实例:不碰常驻 recognizer_cache 槽(那是录制会话的);
            // 恒用本地识别器,云端协议是录制期流式,不适配离线整轨(spec 已知限制 5)。
            let mut recognizer = new_recognizer(&current_asr(&app), current_asr_provider(&app))
                .map_err(|e| tr!("识别器加载失败(本地模型未下载?): {e}", "Failed to load recognizer: {e}", e = e))?;
            let mut embedder: Option<Box<dyn diar::SpeakerEmbedder>> =
                match diar::SherpaEmbedder::new(&speaker_model_path(&app)) {
                    Ok(e) => Some(Box::new(e)),
                    Err(e) => {
                        eprintln!("重转写:声纹模型不可用,归属降级为纯继承: {e}");
                        None
                    }
                };
            let seeds = load_voiceprint_seeds(&app);
            let vad_path = models::root().join("silero_vad.onnx");
            let factory: retranscribe::input::SegmenterFactory =
                Box::new(move || new_silero(&vad_path));
            let mut input: Box<dyn retranscribe::input::TranscribeInput> = if mixed {
                Box::new(retranscribe::input::MixedInput::new(dir.clone(), factory))
            } else {
                Box::new(retranscribe::input::DualTrackInput::new(dir.clone(), factory))
            };
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
            retranscribe::run(&dir, &lock, input.as_mut(), recognizer.as_mut(),
                &mut embedder, seeds, mixed, language_filter, &mut progress)
                .map_err(|e| e.to_string())
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
fn retranscribe_note(app: AppHandle, id: String, input: String) -> Result<(), String> {
    do_retranscribe(&app, &id, &input)
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
    if request.provider != settings.refine_provider {
        return Err(tr!(
            "补建 provider 与当前配置不一致:请求 {requested},当前 {current}",
            "Backfill provider does not match the current configuration: requested {requested}, current {current}",
            requested = request.provider,
            current = settings.refine_provider
        ));
    }
    Ok(())
}

fn relation_executor(
    settings: &settings::Settings,
) -> anyhow::Result<Box<dyn refine::backfill::RelationExecutor>> {
    match settings.refine_provider.as_str() {
        "openai" => Ok(Box::new(refine::llm::HttpRelationExecutor::new(
            refine::llm::LlmConfig {
                base_url: settings.refine_base_url.clone(),
                model: settings.refine_model.clone(),
                api_key: settings.refine_api_key.clone(),
            },
        )?)),
        "agent" => {
            let kind = refine::agent::AgentKind::from_key(&settings.refine_agent)
                .ok_or_else(|| {
                    anyhow::anyhow!(tr!(
                        "未知 Agent: {agent}",
                        "Unknown agent: {agent}",
                        agent = settings.refine_agent
                    ))
                })?;
            Ok(Box::new(refine::agent::AgentRelationExecutor::new(
                kind,
                &settings.refine_agent_bin,
                &settings.refine_agent_model,
            )?))
        }
        provider => anyhow::bail!(tr!(
            "未知关系补建 provider: {provider}",
            "Unknown relation backfill provider: {provider}"
        )),
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

/// 修订稿说话人改名，并同步声纹库（会议搭子）：该说话人已关联库人物时，库中人名一并
/// 更新——所有历史与未来会议随之显示新名；未关联的只改本篇修订稿。Aing 中拒绝（管线
/// 随后整写 refined.json 会吞掉本次编辑），录制中拒绝（speakers.json 由 writer 独占）。
#[tauri::command]
fn rename_refined_speaker(
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
    // Aing 中拒绝:改读 lifecycle 内核 Aing 态(原 AppState.refining 集合已删)。
    if app.state::<lifecycle::LifecycleHandle>().is_refining(&note_id) {
        return Err(tr!(
            "该笔记正在 Aing 中，稍后再改",
            "This note is being refined by AI; try again later"
        ));
    }
    reject_if_active(&state, &note_id)?;
    store::validate_note_id(&note_id).map_err(|e| e.to_string())?;
    let root = notes_dir(&app).map_err(|e| e.to_string())?;
    let person_id = store::rename_refined_speaker(&root.join(&note_id), &speaker_id, name)
        .map_err(|e| e.to_string())?;
    // 降级修订稿沿用 S* 标签(重聚类 skipped/failed):speakers.json 同名条目一并改,
    // 原始逐字稿视图不与修订稿打架。R* 标签不存在于 speakers.json,不碰。
    if speaker_id.starts_with('S') {
        if let Err(e) = store::NoteStore::new(root).rename_speaker(&note_id, &speaker_id, name) {
            eprintln!("修订稿改名已生效,但同步 speakers.json 失败({speaker_id}): {e}");
        }
    }
    // 同步会议搭子:人已被删除/合并成悬空引用时静默跳过——本地改名已生效,不回滚。
    if let Some(pid) = person_id {
        let graph_root = data_root(&app).map_err(|e| e.to_string())?;
        let vp_store = store::VoiceprintStore::new(graph_root.clone());
        let vp = vp_store.load();
        if let Some(resolved) = store::VoiceprintStore::resolve(&vp, &pid).map(str::to_string) {
            match vp_store.rename(&resolved, name) {
                Ok(()) => {
                    queue_person_graph_rebuild(&app, graph_root, &tr!("人物改名", "Person rename"))?
                }
                Err(e) => eprintln!("修订稿改名已生效,但同步声纹库失败({pid}): {e}"),
            }
        }
    }
    Ok(())
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
) -> Result<(), String> {
    reject_if_active(&state, &note_id)?;
    let vp = open_voiceprint_store(&app)?.load();
    let Some(resolved) = store::VoiceprintStore::resolve(&vp, &person_id).map(str::to_string) else {
        return Err(tr!(
            "声纹库中没有该人物: {person_id}",
            "No such person in the voiceprint library: {person_id}"
        ));
    };
    app.state::<lifecycle::LifecycleHandle>().request(lifecycle::machine::Msg::EditNote {
        op: lifecycle::machine::EditOp::AssignPerson {
            id: note_id,
            speaker_id,
            person_id: resolved,
        },
    })
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
    rename_entity_with_rebuild(root, id, new_name, |root| {
        queue_person_graph_rebuild(&app, root, &tr!("人物改名", "Person rename"))
    })
}

/// 把修订稿说话人关联到声纹库人物（会议搭子选人）：段落写入 person_id 并采用库中
/// 现名。此后对该说话人的改名会同步进库；库里改名也会经 get_refined join 反映回来。
#[tauri::command]
fn assign_refined_person(
    app: AppHandle,
    note_id: String,
    speaker_id: String,
    person_id: String,
) -> Result<(), String> {
    // Aing 中拒绝:改读 lifecycle 内核 Aing 态(原 AppState.refining 集合已删)。
    if app.state::<lifecycle::LifecycleHandle>().is_refining(&note_id) {
        return Err(tr!(
            "该笔记正在 Aing 中，稍后再改",
            "This note is being refined by AI; try again later"
        ));
    }
    store::validate_note_id(&note_id).map_err(|e| e.to_string())?;
    let vp = open_voiceprint_store(&app)?.load();
    let Some(resolved) = store::VoiceprintStore::resolve(&vp, &person_id).map(str::to_string) else {
        return Err(tr!(
            "声纹库中没有该人物: {person_id}",
            "No such person in the voiceprint library: {person_id}"
        ));
    };
    let name = vp.people.get(&resolved).map(|p| p.name.clone()).unwrap_or_default();
    let dir = notes_dir(&app).map_err(|e| e.to_string())?.join(&note_id);
    store::assign_refined_person(&dir, &speaker_id, &resolved, &name).map_err(|e| e.to_string())
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

/// 段落编辑共用 guard：活动会话笔记一律拒绝（与 rename_note 同模式）。
fn reject_if_active(state: &State<AppState>, note_id: &str) -> Result<(), String> {
    if state.session.lock().unwrap().as_ref().map(|s| s.note_id == note_id).unwrap_or(false) {
        return Err(tr!("录制中的笔记不能编辑", "A note being recorded cannot be edited"));
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
            if doc.paragraphs.iter().any(|p| p.person_id.is_some()) {
                if let Ok(root) = data_root(&app) {
                    let vp = store::VoiceprintStore::new(root).load();
                    store::join_library_names(&mut doc, &vp);
                }
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
    emb: &mut Option<diar::SherpaEmbedder>,
) -> Result<String, String> {
    let root = data_root(app).map_err(|e| e.to_string())?;
    let store = store::VoiceprintStore::new(root.clone());
    let loser_had_samples = !store.sample_paths_existing(loser).is_empty();
    let overflow = store.sample_paths_existing(loser).len()
        + store.sample_paths_existing(winner).len()
        > store::MAX_SAMPLES;
    if overflow && emb.is_none() {
        match diar::SherpaEmbedder::new(&speaker_model_path(app)) {
            Ok(e) => *emb = Some(e),
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
    let journal_id = store
        .merge_journaled(
            loser,
            winner,
            emb.as_mut().map(|e| e as &mut dyn diar::SpeakerEmbedder),
            origin,
            similarity,
            &now,
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
        let root = data_root(&app).map_err(|e| e.to_string())?;
        queue_person_graph_rebuild(&app, root, &tr!("人物合并", "Person merge"))?;
        Ok(journal_id)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 录制中拒绝：理由同 merge_person。
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
        store::VoiceprintStore::new(root.clone())
            .undo_merge(&journal_id)
            .map_err(|e| e.to_string())?;
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
        let pid = store::VoiceprintStore::new(root.clone())
            .restore_merged_person(&journal_id)
            .map_err(|e| e.to_string())?;
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
        // 云端模式不预载本机识别器:识别在厂商侧,加载一份几 GB 的模型纯属白占内存
        // (开录也不会取用它)。声纹嵌入器照常预载——声纹是本机能力,与识别在哪跑无关。
        let cloud_mode = app
            .path()
            .app_data_dir()
            .map(|d| settings::load(&d).asr_mode == settings::ASR_MODE_CLOUD)
            .unwrap_or(false);
        let mut slot = cache.lock().unwrap();
        if slot.is_none() && !cloud_mode {
            match new_recognizer(&asr_model, current_asr_provider(&app)) {
                Ok(r) => *slot = Some(r),
                Err(e) => eprintln!("识别器预载失败（将在开录时现场加载）: {e}"),
            }
        }
        let mut eslot = embedder_cache.lock().unwrap();
        if eslot.is_none() {
            match diar::SherpaEmbedder::new(&speaker_model_path(&app)) {
                Ok(e) => *eslot = Some(Box::new(e) as Box<dyn diar::SpeakerEmbedder>),
                Err(e) => eprintln!("声纹模型预载失败（说话人区分将不可用）: {e}"),
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
        let mirror_prefix = s.mirror_prefix.clone();
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
                let mirror_prefix = mirror_prefix.as_str();
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
                        match download_one(a, root, mirror_enabled, mirror_prefix, cancel, &emit) {
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
        let app2 = app.clone();
        let cache = state.embedder_cache.clone();
        std::thread::spawn(move || {
            let tag = app2
                .path()
                .app_data_dir()
                .map(|d| settings::load(&d).speaker_model)
                .unwrap_or_default();
            match diar::SherpaEmbedder::new(&speaker_model_path(&app2)) {
                Ok(mut e) => {
                    match data_root(&app2).map(store::VoiceprintStore::new) {
                        Ok(vps) => match vps.rebuild_for_model(&tag, &mut e) {
                            Ok(n) => eprintln!("声纹库已按 {tag} 重建({n} 人有样本可建)"),
                            Err(err) => eprintln!("声纹库重建失败(种子注入将持续跳过): {err}"),
                        },
                        Err(err) => eprintln!("声纹库路径不可用,未重建: {err}"),
                    }
                    // 新模型嵌入器顺手入常驻槽,下一场开录直接可用。
                    stash_model(&cache, Some(Box::new(e) as Box<dyn diar::SpeakerEmbedder>));
                }
                Err(err) => {
                    eprintln!("声纹模型加载失败(模型未下载?),库未重建、录制不自动认人: {err}");
                }
            }
        });
    }
    if tray_changed {
        tray::apply_enabled(&app);
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
    migrate_guard(&state.running, &state.download_running, &state.retranscribing)?;
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
    migrate_guard(&state.running, &state.download_running, &state.retranscribing)?;
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
#[tauri::command]
fn purge_audio(app: AppHandle, state: State<AppState>, older_than_days: Option<u32>) -> Result<u64, String> {
    migrate_guard(&state.running, &state.download_running, &state.retranscribing)?;
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
/// 当前默认输出是否蓝牙:录制页在「保持外放音量」开启时预警蓝牙外放
/// (蓝牙延迟超出软件 AEC 的延迟估计范围,回声消除失效,见 audio::default_output_is_bluetooth)。
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

/// 清除本应用在「屏幕录制」里的 TCC 授权记录(tccutil reset)。修复授权残留:
/// 换签名后(如 v0.1.x ad-hoc → 稳定证书)旧条目的 csreq 与新二进制不匹配,系统
/// 设置里开关看似已开、实际 SCShareableContent 始终被拒,且拨动开关/重启均无效
/// (2026-07-10 实锤:一个 bundle id 下积了 3 条残留)。清除后由前端引导重新授权。
#[tauri::command]
fn reset_screen_capture_permission(app: tauri::AppHandle) -> bool {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        false
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("/usr/bin/tccutil")
            .args(["reset", "ScreenCapture", &app.config().identifier])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
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

pub fn run() {
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
    // 遥测插件:未配 App-Key 时不注册,track 亦会短路,双保险。
    // host 恒传:自托管 key(A-SH-)必需;托管云 key(A-EU-/A-US-)由插件忽略。
    // 插件内部用裸 tokio::spawn 起批量发送循环、reqwest 发请求、退出时 block_on 冲队列,
    // 都要求线程环境里有 Tokio reactor;GUI 主线程本身不在 runtime 里(真机启动即 panic:
    // "there is no reactor running"),这里 enter Tauri 自带 runtime 的 handle 兜底——
    // guard 与 handle 须存活到 builder.run()(插件 setup/退出 flush 都在其中执行),
    // 故绑定在 run() 作用域而非 if 内。EnterGuard 只设线程本地标记,不影响主线程其余逻辑。
    let telemetry_rt = tauri::async_runtime::handle();
    let _telemetry_reactor_guard = telemetry_rt.inner().enter();
    let builder = if telemetry::APP_KEY.is_empty() {
        builder
    } else {
        builder.plugin(
            tauri_plugin_aptabase::Builder::new(telemetry::APP_KEY)
                .with_options(tauri_plugin_aptabase::InitOptions {
                    host: Some(telemetry::APTABASE_HOST.into()),
                    flush_interval: None,
                })
                .build(),
        )
    };
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
            // 一次性迁移:把存量旧默认镜像前缀抬到新默认(见 settings::migrate_mirror_prefix)。
            // 必须先于本函数后续的 settings::load,使其读到迁移后的值。
            if let Some(dir) = &app_data {
                let _ = settings::migrate_mirror_prefix(dir);
            }
            let s = app_data.as_ref().map(|d| settings::load(d)).unwrap_or_default();
            // UI 语言:必须先于托盘构建等任何用户可见文案产生处(tr! 读此全局)。
            i18n::set_lang(&s.ui_lang);
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
            list_notes,
            get_note,
            refine_note,
            get_refined,
            retranscribe_note,
            retranscribe_status,
            mixed_input_status,
            save_refined,
            preview_relation_backfill,
            start_relation_backfill,
            cancel_relation_backfill,
            retry_relation_backfill_index,
            rename_refined_speaker,
            assign_refined_person,
            assign_note_speaker_person,
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
            rename_speaker,
            edit_segment,
            delete_segment,
            set_segment_speaker,
            pipeline_health,
            screen_capture_permission,
            request_screen_capture_permission,
            reset_screen_capture_permission,
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
            rename_person,
            merge_person,
            delete_person,
            delete_person_sample,
            suggest_person_merges,
            apply_confident_merges,
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
            player::player_stop
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        // macOS 点击 dock 图标触发 Reopen:关窗被托盘拦截成 hide 后,dock 是用户最
        // 本能的召回手势——之前只有托盘图标能唤回,dock 点击石沉大海(实测用户以为
        // 程序卡死)。召回语义与托盘 show 菜单项一致:show + set_focus。
        // Reopen 是 macOS 独有变体(其余平台该枚举没有此成员,匹配都编不过),整块 cfg。
        .run(|app, event| {
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

    /// 续录时只有本场 writer 真正追加过样本的源才能更新 sync。否则启动失败或
    /// keep_audio=false 会拿旧 WAV 配上本场零计数,覆盖上一场可信记录。
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
        let rt = Arc::new(Mutex::new(None));
        assert!(migrate_guard(&running, &dl, &rt).is_err(), "录制中拒绝");
        assert!(!dl.load(Ordering::SeqCst), "拒绝后复位互斥位");
        // download_running 已 true(下载/另一迁移在跑）→ 拒。
        let running = Arc::new(Mutex::new(false));
        let dl = Arc::new(AtomicBool::new(true));
        let rt = Arc::new(Mutex::new(None));
        assert!(migrate_guard(&running, &dl, &rt).is_err(), "下载/迁移进行中拒绝");
        // 都空闲 → 过,并已抢下互斥位(swap 置 true)。
        let running = Arc::new(Mutex::new(false));
        let dl = Arc::new(AtomicBool::new(false));
        let rt = Arc::new(Mutex::new(None));
        assert!(migrate_guard(&running, &dl, &rt).is_ok(), "空闲放行");
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
        assert!(migrate_guard(&running, &dl, &rt).is_err(), "重转写占槽时迁移必拒");
        assert!(!dl.load(Ordering::SeqCst), "拒绝后必须复位互斥位");
        // 槽清空后(worker 跑完)同一互斥位可以再次放行。
        *rt.lock().unwrap() = None;
        assert!(migrate_guard(&running, &dl, &rt).is_ok(), "槽清空后迁移应放行");
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

        let mut s3 = s.clone();
        s3.refine_provider = "agent".into();
        assert!(!refine_llm_ready(&s3), "provider=agent 时不走 HTTP,即使三项齐全");
        s3.refine_provider = "bogus".into();
        assert!(refine_llm_ready(&s3), "未知 provider 按默认 openai 对待,Aing 不哑掉");
    }

    #[test]
    fn refine_agent_ready_follows_switch_and_provider() {
        use super::refine_agent_ready;
        let mut s = crate::settings::Settings::default();
        assert!(!refine_agent_ready(&s), "默认 provider=openai → 不走 Agent");
        s.refine_provider = "agent".into();
        assert!(!refine_agent_ready(&s), "总开关未开 → 不走");
        s.refine_enabled = true;
        assert!(refine_agent_ready(&s), "开关开 + provider=agent → 尝试(bin 探测留给运行时)");
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
