//! 产品分析(后端侧)——Rust 这半的唯一上报入口。
//!
//! 分工:后端埋「事情真的发生了」(开录成功、转写完成、AI 整理完成),经 MCP/UDS
//! 触发的同样计入;前端埋「用户看到了、点了什么」,见 src/lib/analytics.ts。
//!
//! 隐私红线:后端事件属性只允许固定枚举与数值桶。禁止上报会议内容、笔记标题、
//! 说话人名、文件路径、API key、模型接入点 ID、任何自由文本。事件与属性用枚举
//! 建模,从类型上杜绝自由字符串进属性;新增事件必须扩 Event 枚举,
//! payload_shape_locked 会逼改测试、强制走一次红线审视。
//! 上报失败静默(SDK 内部批量+重试),绝不影响主流程。
//!
//! 设计:docs/superpowers/specs/2026-08-17-posthog-analytics-and-error-tracking-design.md
//! 可行性实测:docs/superpowers/research/2026-08-17-posthog-tauri-spike.md

use serde_json::{json, Value};
use std::path::Path;
use tauri::AppHandle;

/// PostHog Project API Key。写进客户端的公开写入端点,不是机密(与前端同一个值,
/// 见 src/lib/analytics.ts)。空串 = 整体停用,便于本地开发与测试。
pub const PROJECT_KEY: &str = "phc_qgqdrtaowrPfMPzmD9b7e9JSUPRc3RY3oGAeeKtAAV7E";

/// 区域 host。必须与项目注册区域一致,选错连不上。
pub const HOST: &str = "https://us.i.posthog.com";

// ---------------------------------------------------------------------------
// 环境属性
// ---------------------------------------------------------------------------

/// 每个事件都要带的环境属性。**唯一注入点是 [`before_send`]**,不是各调用点——
/// 那里覆盖不到 SDK 自己发的 panic 事件,而"哪个版本、哪个系统崩的"恰恰是崩溃
/// 最该回答的问题。
///
/// 为什么自己算 `$os`/`$os_version` 而不是让 SDK 算:posthog-rs 确实会用 os_info
/// 注入这两项,但它挂在 `build_events_at` 上(v1_capture.rs),而同一个 crate 的
/// `Pipeline::send_batch` 那条路径只跑 `apply_capture_defaults` + `before_send`
/// ——注入点散落在多条路径上,靠它等于把口径押在实现细节上。更要紧的是前端:
/// posthog-js 的 `$os_version` 从 UA 正则解析,而 WKWebView 的 UA 冻结在
/// `Mac OS X 10_15_7`、WebView2 冻结在 `Windows NT 10.0`(Win11 也报 10.0)。
/// 两端各信各的,同一台机器会在看板上劈成两个系统版本。所以两端**共用这一份值**:
/// 前端经 `app_env` 命令取走,在它自己的 before_send 里盖掉 UA 那份谎报。
fn env_props() -> &'static serde_json::Map<String, Value> {
    static P: std::sync::OnceLock<serde_json::Map<String, Value>> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let env = EnvSnapshot::current();
        let mut m = serde_json::Map::new();
        m.insert("$app_version".into(), json!(env.app_version));
        m.insert("$os".into(), json!(env.os));
        m.insert("$os_version".into(), json!(env.os_version));
        m.insert("app_arch".into(), json!(env.arch));
        m.insert("app_locale".into(), json!(env.locale));
        m.insert("app_is_debug".into(), json!(env.is_debug));
        m
    })
}

/// 环境快照。前端经 `app_env` 命令取同一份,保证两端口径一致。
#[derive(Debug, Clone, serde::Serialize)]
pub struct EnvSnapshot {
    pub app_version: String,
    pub os: String,
    pub os_version: String,
    pub arch: String,
    pub locale: String,
    pub is_debug: bool,
}

impl EnvSnapshot {
    pub fn current() -> Self {
        let info = os_info::get();
        Self {
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            os: canonical_os(info.os_type()),
            os_version: normalize_os_version(&info.version().to_string()),
            arch: std::env::consts::ARCH.to_string(),
            locale: normalize_locale(sys_locale::get_locale().as_deref()),
            is_debug: cfg!(debug_assertions),
        }
    }
}

/// 系统名收敛成固定几档。**不直接用 os_info 的 Display**:它给的是 "Mac OS",
/// 而 posthog-js 给的是 "Mac OS X" —— 两端不统一,同一台机器在看板上会劈成两行。
///
/// 只列 macOS/Windows/Linux 是刻意的:本应用只发这两个平台,Ubuntu/Debian 这些
/// `os_info::Type` 落进 `other` 是开发机,不需要在看板上占一档(codex review 二轮 P2)。
fn canonical_os(t: os_info::Type) -> String {
    match t {
        os_info::Type::Macos => "macOS",
        os_info::Type::Windows => "Windows",
        os_info::Type::Linux => "Linux",
        _ => "other",
    }
    .to_string()
}

/// 系统版本只留 `数字.数字[.数字]`。os_info 在取不到时会给 "Unknown",
/// 某些发行版还会带上代号与空格——放任它进属性等于给一个低基数维度开了自由文本口子。
fn normalize_os_version(raw: &str) -> String {
    let v: String = raw
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let v = v.trim_matches('.');
    if v.is_empty() {
        "unknown".to_string()
    } else {
        v.chars().take(16).collect()
    }
}

/// 语言标签必须**整体符合 BCP-47 形态**,否则一律 unknown。
///
/// 此前是"过滤掉非法字符"——那是清洗不是校验:`ALICE_PRIVATE` 会被洗成
/// `ALICE-PRIVATE` 照样出站,等于给一个低基数维度留了自由文本口子
/// (codex review 二轮 P2)。系统 locale 虽然来自选择器,但可被 `defaults write`
/// 改成任意串,属性维度不接受这种输入。
fn normalize_locale(raw: Option<&str>) -> String {
    const UNKNOWN: &str = "unknown";
    let Some(raw) = raw else { return UNKNOWN.to_string() };
    let tag = raw.replace('_', "-");
    if tag.len() > 20 {
        return UNKNOWN.to_string();
    }
    let mut parts = tag.split('-');
    // 主语言子标签:2-3 个字母(zh / en / yue)。
    let ok_primary = parts
        .next()
        .is_some_and(|p| (2..=3).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphabetic()));
    // 其余子标签:2-8 位字母数字(Hans / CN / 419 / valencia)。
    let ok_rest = parts.all(|p| {
        (2..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric())
    });
    if ok_primary && ok_rest {
        tag
    } else {
        UNKNOWN.to_string()
    }
}

/// 版本号形态的属性值。**新类型而不是裸 String**:红线是"属性只允许固定枚举与数值桶",
/// 版本号是这条红线上唯一的例外(低基数、非内容),用类型把它与自由文本隔开——
/// 构造函数是唯一入口,形态不合一律 `unknown`。
#[derive(Debug, PartialEq, Clone)]
pub struct SafeVersion(String);

impl SafeVersion {
    pub fn parse(raw: &str) -> Self {
        let ok = !raw.is_empty()
            && raw.len() <= 32
            && raw
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
        Self(if ok { raw.to_string() } else { "unknown".to_string() })
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

/// 旧供应商已下线(2026-08-18)。事件与属性建模、隐私红线、防回归测试全部保留——
/// 它们与具体供应商无关,是下一个上报后端接入时直接复用的资产。
/// 录制源类别。由设置推断而非实际启动结果:遥测只要低基数类别,不追精确。
///
/// `Mic`/`System` 两档目前无构造点(`RecordSource::from_settings` 随 record_system_only
/// 字段一起被三删一藏移除,唯一调用点固定按 `Both` 上报,见 lib.rs 的 do_start_recording/
/// do_resume_note_recording)。不删这两个变体——它们仍是遥测面板已有的历史类别定义,
/// Task 3 若把必备源判定做成真逃生舱,细分录制源类别时会重新用上;`#[allow(dead_code)]`
/// 消掉噪音而不是删掉这份枚举契约。
#[derive(Debug, PartialEq)]
pub enum RecordSource {
    #[allow(dead_code)]
    Mic,
    #[allow(dead_code)]
    System,
    Both,
}

impl RecordSource {
    fn as_str(&self) -> &'static str {
        match self {
            RecordSource::Mic => "mic",
            RecordSource::System => "system",
            RecordSource::Both => "both",
        }
    }
}

/// Aing provider 类别。预设 base_url 前缀与前端 REFINE_PRESETS
/// (src/routes/ai/+page.svelte)对齐;匹配不上一律 custom,绝不报原始 URL。
#[derive(Debug, PartialEq)]
pub enum Provider {
    Deepseek,
    Qwen,
    Doubao,
    Kimi,
    Openai,
    Agent,
    Custom,
}

impl Provider {
    pub fn classify(refine_provider: &str, base_url: &str) -> Self {
        if refine_provider == "agent" {
            return Provider::Agent;
        }
        let u = base_url.trim();
        if u.starts_with("https://api.deepseek.com") {
            Provider::Deepseek
        } else if u.starts_with("https://dashscope.aliyuncs.com") {
            Provider::Qwen
        } else if u.starts_with("https://ark.cn-beijing.volces.com") {
            Provider::Doubao
        } else if u.starts_with("https://api.moonshot.cn") {
            Provider::Kimi
        } else if u.starts_with("https://api.openai.com") {
            Provider::Openai
        } else {
            Provider::Custom
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Provider::Deepseek => "deepseek",
            Provider::Qwen => "qwen",
            Provider::Doubao => "doubao",
            Provider::Kimi => "kimi",
            Provider::Openai => "openai",
            Provider::Agent => "agent",
            Provider::Custom => "custom",
        }
    }
}

/// 导出格式。命令层收到的是字符串,先 parse 成枚举再进属性。
#[derive(Debug, PartialEq)]
pub enum ExportFormat {
    Md,
    Txt,
}

impl ExportFormat {
    pub fn parse(format: &str) -> Option<Self> {
        match format {
            "md" => Some(ExportFormat::Md),
            "txt" => Some(ExportFormat::Txt),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            ExportFormat::Md => "md",
            ExportFormat::Txt => "txt",
        }
    }
}

/// MCP 控制面 op(经 GUI 进程 UDS 分发的 6 个)。查询类工具跑在独立
/// stdio 子进程、无 GUI 上下文,首批不统计。
#[derive(Debug, PartialEq)]
pub enum McpOp {
    Status,
    Live,
    Start,
    Stop,
    Pause,
    Resume,
}

impl McpOp {
    pub fn parse(op: &str) -> Option<Self> {
        match op {
            "status" => Some(McpOp::Status),
            "live" => Some(McpOp::Live),
            "start" => Some(McpOp::Start),
            "stop" => Some(McpOp::Stop),
            "pause" => Some(McpOp::Pause),
            "resume" => Some(McpOp::Resume),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            McpOp::Status => "status",
            McpOp::Live => "live",
            McpOp::Start => "start",
            McpOp::Stop => "stop",
            McpOp::Pause => "pause",
            McpOp::Resume => "resume",
        }
    }
}

/// 识别方式。设计文档三条漏斗里"首次拿到转写"这一步的分档依据:本地与云端的
/// 失败形态完全不同,合成一档等于看不出是哪条链路把人挡住了。
#[derive(Debug, PartialEq)]
pub enum AsrEngine {
    Local,
    Cloud,
}

impl AsrEngine {
    /// 由设置的 asr_mode 分类。未知值按 local——与 settings 的兜底语义一致。
    pub fn classify(asr_mode: &str) -> Self {
        if asr_mode == "cloud" {
            AsrEngine::Cloud
        } else {
            AsrEngine::Local
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            AsrEngine::Local => "local",
            AsrEngine::Cloud => "cloud",
        }
    }
}

/// 系统授权项。漏斗 1 的第二步——首启之后最典型的流失点就卡在这里,
/// 没有这个事件就只能看到"装了没开录",看不出是被授权挡住还是自己没兴趣。
#[derive(Debug, PartialEq)]
pub enum PermissionKind {
    /// 屏幕录制(macOS 采系统声音的前置授权)。
    Screen,
}

impl PermissionKind {
    fn as_str(&self) -> &'static str {
        match self {
            PermissionKind::Screen => "screen",
        }
    }
}

/// 全部遥测事件。前 6 个是首批(Aptabase 时代沿用),其余按设计文档的三条漏斗
/// 与"崩了但看不见"补齐。
pub enum Event {
    AppStarted,
    RecordingStarted { source: RecordSource },
    RecordingStopped { duration_ms: u64 },
    NoteRefined { provider: Provider },
    NoteExported { format: ExportFormat },
    McpToolUsed { op: McpOp },
    /// 一场录制结束时是否真拿到了转写。漏斗 1 的"首次拿到转写"。
    TranscriptReady { engine: AsrEngine, empty: bool },
    /// 授权检查/申请的结局。漏斗 1 的"授权"。
    PermissionChecked { kind: PermissionKind, granted: bool },
    /// AI 执行体配置成功(从未配置变成已配置)。漏斗 3 的"配置成功"——
    /// 设计文档写明这一步预期流失率最高,是本期最想验证的假设。
    AiConfigured { provider: Provider },
    /// 版本变化后的首次启动。有了它才能把"某版本开始出错"与"多少人已升上来"对上。
    AppUpdated { from_version: SafeVersion },
    /// 上次运行没有干净退出。**这是本模块唯一能看见硬崩溃的途径**:
    /// panic hook 只覆盖 Rust panic,SIGSEGV/SIGABRT(whisper C++ 层、CoreAudio)、
    /// OOM 被杀、强制退出都不走它,且队列里没来得及 flush 的事件会一起消失。
    AppUncleanExit { version: SafeVersion },
}

/// 时长桶:精确时长不上报,只报 4 档。
fn duration_bucket(ms: u64) -> &'static str {
    let min = ms / 60_000;
    if min < 5 {
        "<5m"
    } else if min < 30 {
        "5-30m"
    } else if min < 60 {
        "30-60m"
    } else {
        ">1h"
    }
}

impl Event {
    /// (事件名, 属性)。属性值只可能来自各枚举 as_str 与 duration_bucket
    /// ——隐私红线的类型化落实,测试锁形状防回归。
    fn payload(&self) -> (&'static str, Option<Value>) {
        match self {
            Event::AppStarted => ("app_started", None),
            Event::RecordingStarted { source } => {
                ("recording_started", Some(json!({ "source": source.as_str() })))
            }
            Event::RecordingStopped { duration_ms } => (
                "recording_stopped",
                Some(json!({ "duration_bucket": duration_bucket(*duration_ms) })),
            ),
            Event::NoteRefined { provider } => {
                ("note_refined", Some(json!({ "provider": provider.as_str() })))
            }
            Event::NoteExported { format } => {
                ("note_exported", Some(json!({ "format": format.as_str() })))
            }
            Event::McpToolUsed { op } => {
                ("mcp_tool_used", Some(json!({ "tool": op.as_str() })))
            }
            Event::TranscriptReady { engine, empty } => (
                "transcript_ready",
                Some(json!({ "engine": engine.as_str(), "empty": empty })),
            ),
            Event::PermissionChecked { kind, granted } => (
                "permission_checked",
                Some(json!({ "kind": kind.as_str(), "granted": granted })),
            ),
            Event::AiConfigured { provider } => {
                ("ai_configured", Some(json!({ "provider": provider.as_str() })))
            }
            Event::AppUpdated { from_version } => (
                "app_updated",
                Some(json!({ "from_version": from_version.as_str() })),
            ),
            Event::AppUncleanExit { version } => (
                "app_unclean_exit",
                Some(json!({ "version": version.as_str() })),
            ),
        }
    }
}

/// 唯一上报入口。失败静默——上报绝不影响主流程。
///
/// distinct_id 由前端生成并持久化,经 set_distinct_id 传入(见该函数说明)。
/// **拿到之前发生的事件先压队,等 id 到位再补发**,绝不在此自造 id:
/// 两边各生成一个会把同一个人算成两个人,漏斗与留存全部失真。
pub fn track(_app: &AppHandle, event: Event) {
    if !enabled() {
        return;
    }
    let (name, props) = event.payload();
    // 判定"有没有 id"与"入队"必须在同一把锁下完成:分成两步的话,读到"无 id"之后、
    // 入队之前,另一线程可能刚好把 id 设上并排空了当时还空的队列——这条事件就会一直
    // 滞留到退出,最后以 personless 发出(codex review P2#5)。
    let mut guard = match outbox().lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    match guard.id.clone() {
        Some(id) => {
            drop(guard); // 绝不持锁调 SDK
            emit(name, Some(&id), props);
        }
        // id 还没到:压队,等 set_distinct_id 补发。
        //
        // 为什么不像以前那样直接 personless 发出去:app_started 固定发生在 setup()
        // 里、早于 webview 调 set_analytics_id,`Event::new_anon` 每次生成一个随机
        // distinct_id 并关掉 person profile——于是**每一次启动都是一个全新的匿名人**,
        // 留存、DAU、以及设计文档漏斗 1 的第一步"首启"全部无从算起。压队是唯一
        // 既不自造 id、又能让启动事件落到正确身份上的办法。
        None => {
            // **有界**:上限之后直接丢弃而不是无限攒——前端若始终起不来(webview 崩了、
            // 白屏),这个队列永远等不到补发,不能让它跟着进程一起长。
            if guard.pending.len() < PENDING_CAP {
                guard.pending.push((name, props));
            }
        }
    }
}

/// 把一条事件真正交给 SDK。属性值只可能来自各枚举 as_str 与 duration_bucket
/// (见 payload),这里不做二次校验,由 payload_shape_locked 守住形状。
/// 环境属性不在这里加——统一由 before_send 注入,那里才覆盖得到 SDK 自发的 panic。
///
/// id 为 None 时用 `new_anon`(真正的 personless),不能用字面量 id——每台机器都记成
/// 同一个字面量的话,独立用户数与激活漏斗全废(codex review 发现)。
fn emit(name: &'static str, id: Option<&str>, props: Option<Value>) {
    let mut ev = match id {
        Some(id) => posthog_rs::Event::new(name, id),
        None => posthog_rs::Event::new_anon(name),
    };
    if let Some(Value::Object(map)) = props {
        for (k, v) in map {
            let _ = ev.insert_prop(k, v);
        }
    }
    posthog_rs::capture(ev);
}

/// 身份与压队同一把锁。**不拆成两把**——理由见 track 里的注释。
const PENDING_CAP: usize = 64;

#[derive(Default)]
struct Outbox {
    /// 前端持久化的匿名 id。None = 还没到,事件压 pending。
    id: Option<String>,
    pending: Vec<(&'static str, Option<Value>)>,
}

static OUTBOX: std::sync::OnceLock<std::sync::Mutex<Outbox>> = std::sync::OnceLock::new();

fn outbox() -> &'static std::sync::Mutex<Outbox> {
    OUTBOX.get_or_init(|| std::sync::Mutex::new(Outbox::default()))
}

fn current_id() -> Option<String> {
    outbox().lock().ok().and_then(|o| o.id.clone())
}

/// 排空压着的事件。`id` 为 None 表示等不到了(进程要退出),此时以 personless 发出
/// ——身份不全好过整条没有。取出与发送分开,绝不持锁调 SDK。
fn drain_pending(id: Option<&str>) {
    let items = match outbox().lock() {
        Ok(mut o) => std::mem::take(&mut o.pending),
        Err(_) => return,
    };
    for (name, props) in items {
        emit(name, id, props);
    }
}

/// 由前端在初始化后调用一次(命令壳 set_analytics_id)。幂等,后到的覆盖先到的。
/// 设完立刻补发压队的早期事件——设 id 与取队列在同一把锁下,不留漏排窗口。
pub fn set_distinct_id(id: &str) {
    if id.is_empty() {
        return;
    }
    let items = match outbox().lock() {
        Ok(mut o) => {
            o.id = Some(id.to_string());
            std::mem::take(&mut o.pending)
        }
        Err(_) => return,
    };
    for (name, props) in items {
        emit(name, Some(id), props);
    }
}

// ---------------------------------------------------------------------------
// 上报总开关
// ---------------------------------------------------------------------------

/// 用户可关。**默认关**,由设置在启动读到 `telemetry_enabled` 后打开。
///
/// 为什么默认关而不是默认开:`init()` 在 `run()` 开头就装好了 panic hook,而设置要到
/// `setup()` 里才读得到(app_data_dir 得等 AppHandle)。默认开的话,这中间几毫秒里发生
/// 的 panic 会照发不误——对一个明确关掉了上报的用户,那就是直接违背承诺。代价是
/// 打开上报的用户会丢掉这个窗口内的 panic;两边都错的时候,选择错在不发的一侧
/// (codex review P1#1)。
static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 上报是否放行。key 为空(本地开发/测试)也算关。
fn enabled() -> bool {
    !PROJECT_KEY.is_empty() && ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// 当前值(供前端 init 前查询)。只看用户设置,不看 key 是否配置——
/// 前端有它自己的 key 判定。
pub fn is_enabled() -> bool {
    ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// 由设置驱动。**关掉时同时清空压队**——那些事件是在"当时还开着"的前提下攒的,
/// 用户既然关了,就不该在下一次 id 到位时补发出去。
pub fn set_enabled(on: bool) {
    ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
    if !on {
        if let Ok(mut o) = outbox().lock() {
            o.pending.clear();
        }
    }
}

/// 进程启动时初始化 SDK 并装 panic hook。失败只记日志,绝不影响启动。
pub fn init() {
    if PROJECT_KEY.is_empty() {
        return;
    }
    // capture_panics 默认 false(crate 源码 error_tracking.rs 的 Default impl),
    // is_server 默认 true——桌面客户端两个都必须显式设。spike 记录的第五个
    // 「默认值倒在不上报那一侧」的例子。
    let et = match posthog_rs::ErrorTrackingOptionsBuilder::default()
        .capture_panics(true)
        .capture_stacktrace(true)
        .build()
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("telemetry: 错误追踪选项构建失败(已跳过): {e}");
            return;
        }
    };
    let opts = match posthog_rs::ClientOptionsBuilder::default()
        .api_key(PROJECT_KEY.to_string())
        .host(HOST.to_string())
        .is_server(false)
        .error_tracking(et)
        // 出口统一处理:总开关、异常限流、环境属性、脱敏。**panic hook 由 SDK 安装,
        // 它直接序列化 panic 载荷,不经 report_error、也就不经 redact**——panic 消息里
        // 常有 home 路径、文件名、上游错误文本(codex review 发现)。放在这里是唯一能
        // 覆盖所有发送路径的位置:手工上报、自动 panic、将来新增的任何 capture 都过它。
        .before_send(before_send)
        .build()
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("telemetry: 客户端选项构建失败(已跳过): {e}");
            return;
        }
    };
    if let Err(e) = posthog_rs::init_global(opts) {
        eprintln!("telemetry: 初始化失败(已跳过,不影响主流程): {e}");
    }
}

/// 退出前排空上报队列。同步版 flush(关掉了 async-client 特性),不阻塞太久:
/// 失败也无所谓——上报绝不能拖住退出。
pub fn flush_on_exit() {
    if !enabled() {
        return;
    }
    // 还压着的早期事件:等不到 id 了,以 personless 发出去。身份不全好过整条没有。
    drain_pending(None);
    posthog_rs::flush();
}

/// before_send 钩子:全部出站事件的唯一关卡。四件事,顺序固定——
/// 总开关 → 异常限流 → 环境属性 → 脱敏。
///
/// 前两步会**丢事件**(返回 None),后两步只改属性值、绝不丢——丢了就看不见异常,
/// 与上报的目的相悖。钩子内不得 panic(SDK 会捕获并丢弃该事件),故全程用安全取值。
fn before_send(mut ev: posthog_rs::Event) -> Option<posthog_rs::Event> {
    // ① 总开关。放在这里而不是只放调用点:SDK 自装的 panic hook 不经我们的任何
    //    函数,只有这里拦得住。用户关了遥测,崩溃也不该发出去。
    if !enabled() {
        return None;
    }
    // ② 异常限流。设计文档「限流与额度」明写:异常按 fingerprint 每会话限流,
    //    否则断流风暴那类高频错误一场会议就能打满月额度。
    if ev.event_name() == "$exception" && !exception_allowed(&ev) {
        return None;
    }
    // ③ 环境属性。见 env_props 的说明:唯一注入点,panic 事件同样要带上版本。
    for (k, v) in env_props() {
        let _ = ev.insert_prop(k.clone(), v.clone());
    }
    // ④ 脱敏。
    Some(redact_event(ev))
}

/// 同 fingerprint 每进程最多几条,以及全进程总上限。**两道都要**:前者挡住单条链路
/// 刷屏,后者挡住"很多种错各刷几条"的合力——2026-08-13 的断流风暴就是后一种形态。
const EXCEPTION_CAP_PER_KIND: u32 = 5;
const EXCEPTION_CAP_TOTAL: u32 = 50;

/// 额度的滚动窗口。**不能只按进程算**:本应用常驻托盘、一开好几天,进程级计数攒满
/// 50 条之后剩下的整个生命周期都发不出任何异常,等于攒够一次就永久失明
/// (codex review 三轮 P2)。设计文档写的是"每会话",一小时的滚动窗口是它可实现的
/// 近似:最坏情形单机 50 条/小时,量级仍远低于免费档 10 万/月。
const EXCEPTION_WINDOW_SECS: u64 = 3600;

struct ExceptionBudget {
    per_kind: std::collections::HashMap<String, u32>,
    total: u32,
    window_start: std::time::Instant,
}

static EXCEPTION_COUNTS: std::sync::OnceLock<std::sync::Mutex<ExceptionBudget>> =
    std::sync::OnceLock::new();

/// 这条异常还能不能发。key 取 fingerprint(report_error 一律带),缺了退回 type,
/// 再缺就归到一个兜底桶——**绝不因为取不到 key 就放行**,那等于没有限流。
fn exception_allowed(ev: &posthog_rs::Event) -> bool {
    let key = exception_key(ev);
    let cell = EXCEPTION_COUNTS.get_or_init(|| {
        std::sync::Mutex::new(ExceptionBudget {
            per_kind: std::collections::HashMap::new(),
            total: 0,
            window_start: std::time::Instant::now(),
        })
    });
    let Ok(mut g) = cell.lock() else { return true };
    let elapsed = g.window_start.elapsed().as_secs();
    let ExceptionBudget { per_kind, total, window_start } = &mut *g;
    if roll_window(per_kind, total, elapsed) {
        *window_start = std::time::Instant::now();
    }
    allow_once(&key, per_kind, total)
}

/// 窗口到点就清零,返回是否真的清了(调用方据此重置窗口起点)。
/// 抽成纯函数是为了能测——`Instant` 在测试里造不出"一小时前"。
fn roll_window(
    per_kind: &mut std::collections::HashMap<String, u32>,
    total: &mut u32,
    elapsed_secs: u64,
) -> bool {
    if elapsed_secs < EXCEPTION_WINDOW_SECS {
        return false;
    }
    per_kind.clear();
    *total = 0;
    true
}

/// 限流分桶键。fingerprint 优先(report_error 一律带),缺了退回 type
/// (SDK 自装的 panic hook 发的事件只有它),再缺就归到兜底桶——
/// **绝不因为取不到 key 就放行**,那等于没有限流。
fn exception_key(ev: &posthog_rs::Event) -> String {
    ev.properties()
        .get("$exception_fingerprint")
        .or_else(|| ev.properties().get("$exception_type"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string()
}

/// 限流的纯函数体。计数器由调用方持有,**测试直接测这一层**——全局计数器是进程级的,
/// 让测试去动它会互相污染,一个并行跑的用例能把另一个的额度吃掉。
fn allow_once(
    key: &str,
    per_kind: &mut std::collections::HashMap<String, u32>,
    total: &mut u32,
) -> bool {
    if *total >= EXCEPTION_CAP_TOTAL {
        return false;
    }
    let n = per_kind.entry(key.to_string()).or_insert(0);
    if *n >= EXCEPTION_CAP_PER_KIND {
        return false;
    }
    *n += 1;
    *total += 1;
    true
}

/// 对异常类事件的文本字段做脱敏。
fn redact_event(mut ev: posthog_rs::Event) -> posthog_rs::Event {
    // 现代错误追踪用 $exception_list;标量字段兼容旧看板,两种都覆盖。
    if let Some(list) = ev.properties().get("$exception_list").cloned() {
        let cleaned = redact_exception_list(list);
        let _ = ev.insert_prop("$exception_list", cleaned);
    }
    // panic 事件还带 $exception_panic_file 这类独立的路径属性,
    // 以及栈帧里的 filename——只脱 type/value 会把它们漏出去(codex review 第二轮)。
    // $exception_stack_trace_raw 是 TS 侧已覆盖、Rust 侧漏掉的那一个——同一个清单
    // 在两端各写一份,漂移就是这么发生的(codex review 二轮 P1#4)。
    for key in [
        "$exception_panic_file",
        "$exception_message",
        "$exception_type",
        "$exception_source",
        "$exception_stack_trace_raw",
    ] {
        let Some(v) = ev.properties().get(key).cloned() else { continue };
        if let Some(text) = v.as_str() {
            let _ = ev.insert_prop(key, crate::redact::redact(text));
        }
    }
    ev
}

/// 逐条脱 $exception_list 里的 type/value。结构不认识就原样返回——
/// 宁可放过结构变化,也不能因为解析失败把事件丢掉。
fn redact_exception_list(list: Value) -> Value {
    let Value::Array(items) = list else { return list };
    Value::Array(
        items
            .into_iter()
            .map(|mut item| {
                if let Value::Object(map) = &mut item {
                    for key in ["value", "type"] {
                        if let Some(Value::String(text)) = map.get(key) {
                            let safe = crate::redact::redact(text);
                            map.insert(key.to_string(), Value::String(safe));
                        }
                    }
                    // 栈帧的 filename 是文件系统路径,同样在禁止上传之列。
                    if let Some(Value::Object(st)) = map.get_mut("stacktrace") {
                        if let Some(Value::Array(frames)) = st.get_mut("frames") {
                            for f in frames.iter_mut() {
                                let Value::Object(fm) = f else { continue };
                                for key in ["filename", "abs_path", "module"] {
                                    if let Some(Value::String(text)) = fm.get(key) {
                                        let safe = crate::redact::redact(text);
                                        fm.insert(key.to_string(), Value::String(safe));
                                    }
                                }
                            }
                        }
                    }
                }
                item
            })
            .collect(),
    )
}

/// 显式上报一次失败(崩溃由 panic hook 全覆盖,这里管「出错了但没崩」的关键链路)。
///
/// 消息一律过 [`crate::redact`]:spike 实测异常载荷会原样带出家目录里的姓名与
/// notes 路径里的会议标题。kind 是固定枚举,便于在 PostHog 侧按链路分组。
pub fn report_error(kind: ErrorKind, detail: &str) {
    if !enabled() {
        return;
    }
    let safe = crate::redact::redact(detail);
    let mut ev = match current_id() {
        Some(id) => posthog_rs::Event::new("$exception", &id),
        None => posthog_rs::Event::new_anon("$exception"),
    };
    // 现代 PostHog 错误追踪按 $exception_list 建 issue 与定位;只发标量字段的话
    // 事件可能只显示成一条普通事件,拿不到承诺的"出错位置"(codex review 发现)。
    // 标量字段一并保留,兼容旧看板。
    let _ = ev.insert_prop(
        "$exception_list",
        serde_json::json!([{ "type": kind.as_str(), "value": safe, "mechanism": { "handled": true } }]),
    );
    let _ = ev.insert_prop("$exception_type", kind.as_str());
    let _ = ev.insert_prop("$exception_message", safe);
    // fingerprint 按 kind 分组:spike 发现 PostHog 会把语义无关的裸 Error 并进
    // 同一个 issue,不给 fingerprint 就无法按链路看、也无法按 fingerprint 限流。
    let _ = ev.insert_prop("$exception_fingerprint", kind.as_str());
    posthog_rs::capture(ev);
}

/// 关键失败链路。只列「出了错但应用还活着」的那些——崩溃走 panic hook。
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ErrorKind {
    RecordingStart,
    RecordingStop,
    AsrEngine,
    AiPipeline,
    NoteWrite,
    McpDispatch,
    RefineStaleHeal,
    // —— 设计文档「上报面」列了 12 处标记点,以下四处是补齐的那批 ——
    /// 采集断连自愈:重建成功(recovered)与放弃(lost)都算。同 RefineStaleHeal 的道理
    /// ——兜住不等于没发生,它触发一次就说明这台机器上的采集链断过。
    CaptureRebuild,
    /// 停录收尾时采集线程 join 超时被放弃(issue #182):线程泄漏换全应用不冻结。
    /// 一响就说明这台机器的采集线程曾卡死在设备调用里(实测:蓝牙麦断流风暴)。
    CaptureTeardown,
    /// 模型加载失败(ASR/声纹)。与 AsrEngine 分开:加载失败是装机期问题,
    /// 引擎异常是运行期问题,合成一档会把两类完全不同的故障混进同一个 issue。
    ModelLoad,
    /// AI 结果写回笔记失败(apply_refined_texts / apply_aing_graph)。
    /// 与 AiPipeline 分开:算出来了但没存住,和根本没算出来,是两回事。
    AiApplyWrite,
    /// 数据迁移失败(换数据目录)。用户点了迁移却没迁成,本机日志之外无人知晓。
    Migration,
    /// 一键更新失败。**由前端上报**(updater 走 JS API),经 report_frontend_error 进来。
    UpdateFailed,
}

impl ErrorKind {
    fn as_str(&self) -> &'static str {
        match self {
            ErrorKind::RecordingStart => "recording_start",
            ErrorKind::RecordingStop => "recording_stop",
            ErrorKind::AsrEngine => "asr_engine",
            ErrorKind::AiPipeline => "ai_pipeline",
            ErrorKind::NoteWrite => "note_write",
            ErrorKind::McpDispatch => "mcp_dispatch",
            ErrorKind::RefineStaleHeal => "refine_stale_heal",
            ErrorKind::CaptureRebuild => "capture_rebuild",
            ErrorKind::CaptureTeardown => "capture_teardown",
            ErrorKind::ModelLoad => "model_load",
            ErrorKind::AiApplyWrite => "ai_apply_write",
            ErrorKind::Migration => "migration",
            ErrorKind::UpdateFailed => "update_failed",
        }
    }

    /// 前端上报时用的解析口。**白名单**:前端传进来的字符串不进属性,先落回枚举,
    /// 认不出就整条丢弃——否则自由文本从这个口子绕过了全部红线。
    pub fn parse(kind: &str) -> Option<Self> {
        let all = [
            ErrorKind::RecordingStart,
            ErrorKind::RecordingStop,
            ErrorKind::AsrEngine,
            ErrorKind::AiPipeline,
            ErrorKind::NoteWrite,
            ErrorKind::McpDispatch,
            ErrorKind::RefineStaleHeal,
            ErrorKind::CaptureRebuild,
            ErrorKind::ModelLoad,
            ErrorKind::AiApplyWrite,
            ErrorKind::Migration,
            ErrorKind::UpdateFailed,
        ];
        all.into_iter().find(|k| k.as_str() == kind)
    }
}

// ---------------------------------------------------------------------------
// 会话标记:硬崩溃与版本变化
// ---------------------------------------------------------------------------

/// 上一次运行留下的痕迹。
///
/// **为什么需要它**:panic hook 只覆盖 Rust panic。whisper C++ 层的 SIGSEGV/SIGABRT、
/// CoreAudio 崩溃、被系统 OOM 杀掉、用户强制退出——都不走 panic hook,而且队列里
/// 还没 flush 的事件会跟着一起消失。也就是说最严重的那类故障恰恰是唯一上报不到的。
/// 落一个"运行中"标记、下次启动回头看,是把这块盲区照亮的最省事办法。
#[derive(Debug, PartialEq, Default)]
pub struct BootState {
    /// 上次运行没留下干净退出的记号。
    pub unclean_exit: bool,
    /// 上次运行的版本(用于给 unclean_exit 定位到具体版本)。
    pub last_version: Option<String>,
    /// 上次运行的版本与本次不同——说明这次是升级后首启。
    pub updated_from: Option<String>,
}

const SESSION_FILE: &str = "telemetry_session.json";

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct SessionMark {
    running: bool,
    version: String,
    /// 落这个标记的进程。**只有 pid 对得上才允许清掉**——本仓库没有单实例守卫,
    /// 第二个实例正常退出时若把标记清掉,第一个实例随后真崩了就再也报不出来
    /// (codex review P2#6)。
    #[serde(default)]
    pid: u32,
}

/// 启动时调用一次:读走上次的痕迹,并落下本次的"运行中"标记。
/// 读写失败一律当作"没有痕迹"——这是观测设施,绝不能因为它挡住启动。
pub fn open_session(app_data: &Path) -> BootState {
    let path = app_data.join(SESSION_FILE);
    let prev: SessionMark = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let cur = env!("CARGO_PKG_VERSION");
    let last = (!prev.version.is_empty()).then(|| prev.version.clone());
    let state = BootState {
        unclean_exit: prev.running,
        updated_from: last.clone().filter(|v| v != cur),
        last_version: last,
    };
    write_mark(&path, true);
    state
}

/// 正常退出路径调用。**必须与 flush_on_exit 同一处**:漏在别的分支上,
/// 那条分支的每次退出都会被下次启动误报成崩溃。
///
/// 只在标记确实是自己落的时候才清:多开时第二个实例正常退出,不该把第一个实例
/// 那份"运行中"抹掉(codex review P2#6)。反向的误报(B 启动时把仍在跑的 A 记成崩溃)
/// 无法只靠一个文件判掉,需要跨平台的进程存活探测——已知未修,记在这里:
/// macOS 由 LaunchServices 天然单实例,Windows 多开会多出一条 app_unclean_exit,
/// 是看板噪音而非用户可见问题。
pub fn close_session(app_data: &Path) {
    let path = app_data.join(SESSION_FILE);
    let owner = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<SessionMark>(&t).ok())
        .map(|m| m.pid);
    // 标记不存在/读不出来时照常清:那是首次运行或文件坏了,不清反而会误报。
    if matches!(owner, Some(pid) if pid != std::process::id()) {
        return;
    }
    write_mark(&path, false);
}

fn write_mark(path: &Path, running: bool) {
    let mark = SessionMark {
        running,
        version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string(&mark) {
        let _ = std::fs::write(path, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ENABLED 是进程级的,而 cargo test 默认并行:动它的用例与读它的用例必须串行,
    /// 否则「关掉遥测」那条会把同时在跑的 before_send 用例一起关掉。
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn gate() -> std::sync::MutexGuard<'static, ()> {
        GATE.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn duration_bucket_boundaries() {
        assert_eq!(duration_bucket(0), "<5m");
        assert_eq!(duration_bucket(5 * 60_000 - 1), "<5m");
        assert_eq!(duration_bucket(5 * 60_000), "5-30m");
        assert_eq!(duration_bucket(30 * 60_000), "30-60m");
        assert_eq!(duration_bucket(60 * 60_000), ">1h");
        assert_eq!(duration_bucket(3 * 60 * 60_000), ">1h");
    }

    #[test]
    fn provider_classify_presets() {
        assert_eq!(Provider::classify("agent", "https://api.openai.com/v1"), Provider::Agent);
        assert_eq!(Provider::classify("openai", "https://api.deepseek.com/v1"), Provider::Deepseek);
        assert_eq!(
            Provider::classify("openai", "https://dashscope.aliyuncs.com/compatible-mode/v1"),
            Provider::Qwen
        );
        assert_eq!(
            Provider::classify("openai", "https://ark.cn-beijing.volces.com/api/v3"),
            Provider::Doubao
        );
        assert_eq!(Provider::classify("openai", "https://api.moonshot.cn/v1"), Provider::Kimi);
        assert_eq!(Provider::classify("openai", "https://api.openai.com/v1"), Provider::Openai);
        // 自定义/未知 base_url 绝不透出原文
        assert_eq!(Provider::classify("openai", "https://my-private-gw.example.com"), Provider::Custom);
        assert_eq!(Provider::classify("openai", ""), Provider::Custom);
    }

    #[test]
    fn mcp_op_parse_known_only() {
        assert_eq!(McpOp::parse("start"), Some(McpOp::Start));
        assert_eq!(McpOp::parse("live"), Some(McpOp::Live));
        assert_eq!(McpOp::parse("drop_table"), None);
    }

    #[test]
    fn export_format_parse_known_only() {
        assert_eq!(ExportFormat::parse("md"), Some(ExportFormat::Md));
        assert_eq!(ExportFormat::parse("txt"), Some(ExportFormat::Txt));
        assert_eq!(ExportFormat::parse("../etc/passwd"), None);
    }

    /// 锁全部事件的序列化形状:事件名、属性键、属性值均为受控枚举输出。
    /// 若有人往属性里塞新字段/自由文本,此测试必须跟着改——强制走一次红线审视。
    /// before_send 是 panic 载荷的唯一防线:SDK 装的 panic hook 直接序列化载荷,
    /// 不经 report_error 也就不经 redact。这条钉死它确实在出口生效。
    #[test]
    fn before_send脱掉异常载荷里的内容() {
        let mut ev = posthog_rs::Event::new_anon("$exception");
        ev.insert_prop(
            "$exception_list",
            json!([{ "type": "Error", "value": "写入 /Users/张伟/notes/季度复盘会.json 失败" }]),
        )
        .unwrap();
        ev.insert_prop("$exception_message", "panic at /Users/张伟/x.rs").unwrap();

        let out = redact_event(ev);
        let dumped = serde_json::to_string(out.properties()).unwrap();
        assert!(!dumped.contains("张伟"), "姓名必须脱掉: {dumped}");
        assert!(!dumped.contains("季度复盘会"), "会议标题必须脱掉: {dumped}");
    }

    /// panic 事件的路径不只在 value 里:栈帧的 filename 与 $exception_panic_file
    /// 都是独立的路径属性(codex review 第二轮发现)。
    #[test]
    fn before_send连栈帧路径一起脱掉() {
        let mut ev = posthog_rs::Event::new_anon("$exception");
        ev.insert_prop(
            "$exception_list",
            json!([{
                "type": "panic",
                "value": "boom",
                "stacktrace": { "frames": [{ "filename": "/Users/张伟/voice-notes/src/x.rs" }] }
            }]),
        )
        .unwrap();
        ev.insert_prop("$exception_panic_file", "/Users/张伟/voice-notes/src/y.rs").unwrap();

        let out = redact_event(ev);
        let dumped = serde_json::to_string(out.properties()).unwrap();
        assert!(!dumped.contains("张伟"), "栈帧与 panic_file 里的路径都必须脱掉: {dumped}");
    }

    #[test]
    fn before_send不认识的结构原样放行而非丢弃() {
        let _g = gate();
        set_enabled(true);
        let mut ev = posthog_rs::Event::new_anon("vn_page_view");
        ev.insert_prop("path", "/notes").unwrap();
        let out = before_send(ev).expect("普通事件不得被丢弃");
        assert_eq!(out.properties().get("path").unwrap(), "/notes");
    }

    /// 环境属性必须由 before_send 注入,而不是各调用点——SDK 自装的 panic hook
    /// 不经我们任何函数,只有这里覆盖得到。少了版本,"哪个版本开始崩"就无从回答。
    #[test]
    fn before_send给普通事件盖上环境属性() {
        let _g = gate();
        set_enabled(true);
        let out = before_send(posthog_rs::Event::new_anon("app_started")).expect("不得丢弃");
        let p = out.properties();
        assert_eq!(p.get("$app_version").unwrap(), env!("CARGO_PKG_VERSION"));
        assert!(p.contains_key("$os"), "缺 $os: {p:?}");
        assert!(p.contains_key("$os_version"), "缺 $os_version: {p:?}");
        assert_eq!(p.get("app_arch").unwrap(), std::env::consts::ARCH);
        assert!(p.contains_key("app_locale"), "缺 app_locale: {p:?}");
        assert_eq!(p.get("app_is_debug").unwrap(), &json!(cfg!(debug_assertions)));
    }

    /// panic 事件同样要带版本——它是"哪个版本开始崩"这个问题的主要数据源。
    #[test]
    fn before_send给panic事件也盖上版本() {
        let _g = gate();
        set_enabled(true);
        let mut ev = posthog_rs::Event::new_anon("$exception");
        ev.insert_prop("$exception_fingerprint", "panic_env_probe").unwrap();
        let out = before_send(ev).expect("首条不得被限流");
        assert_eq!(out.properties().get("$app_version").unwrap(), env!("CARGO_PKG_VERSION"));
    }

    /// 设计文档「限流与额度」:异常按 fingerprint 每会话限流,否则断流风暴那类
    /// 高频错误一场会议就能打满月额度。此前只有一句注释,没有实现。
    #[test]
    fn 异常按fingerprint限流() {
        let mut per = std::collections::HashMap::new();
        let mut total = 0;
        for i in 0..EXCEPTION_CAP_PER_KIND {
            assert!(allow_once("asr_engine", &mut per, &mut total), "前 {i} 条应放行");
        }
        assert!(!allow_once("asr_engine", &mut per, &mut total), "同 fingerprint 超额必须丢");
        assert!(allow_once("note_write", &mut per, &mut total), "别的 fingerprint 不受连累");
    }

    /// 单条链路刷不满,很多种错各刷几条却能合力打满——2026-08-13 断流风暴就是这形态。
    /// 常驻托盘一开好几天:进程级计数攒满就永久失明,窗口到点必须清零。
    #[test]
    fn 异常额度按窗口滚动而不是攒一辈子() {
        let mut per = std::collections::HashMap::new();
        let mut total = 0;
        for _ in 0..EXCEPTION_CAP_PER_KIND {
            allow_once("asr_engine", &mut per, &mut total);
        }
        assert!(!allow_once("asr_engine", &mut per, &mut total), "窗口内超额必须丢");
        assert!(!roll_window(&mut per, &mut total, EXCEPTION_WINDOW_SECS - 1), "没到点不清");
        assert!(roll_window(&mut per, &mut total, EXCEPTION_WINDOW_SECS), "到点必须清");
        assert!(allow_once("asr_engine", &mut per, &mut total), "新窗口重新放行");
    }

    #[test]
    fn 异常还有一道全局总闸() {
        let mut per = std::collections::HashMap::new();
        let mut total = 0;
        for i in 0..EXCEPTION_CAP_TOTAL {
            assert!(allow_once(&format!("kind-{i}"), &mut per, &mut total), "第 {i} 条应放行");
        }
        assert!(!allow_once("brand-new-kind", &mut per, &mut total), "总闸到顶后一律丢");
    }

    /// 取不到 key 不能当放行处理——那等于没有限流。SDK 自装的 panic hook 发出的事件
    /// 没有我们的 fingerprint,退回 $exception_type 才拦得住反复 panic 的循环。
    #[test]
    fn 限流key优先fingerprint再type最后兜底桶() {
        let mut ev = posthog_rs::Event::new_anon("$exception");
        ev.insert_prop("$exception_type", "panic").unwrap();
        assert_eq!(exception_key(&ev), "panic");
        ev.insert_prop("$exception_fingerprint", "asr_engine").unwrap();
        assert_eq!(exception_key(&ev), "asr_engine");
        let bare = posthog_rs::Event::new_anon("$exception");
        assert_eq!(exception_key(&bare), "unknown", "取不到 key 也必须落进一个桶,不能放行");
    }

    #[test]
    fn 版本号形态之外一律unknown() {
        assert_eq!(SafeVersion::parse("0.12.0").as_str(), "0.12.0");
        assert_eq!(SafeVersion::parse("1.0.0-beta.1").as_str(), "1.0.0-beta.1");
        assert_eq!(SafeVersion::parse("").as_str(), "unknown");
        // 自由文本绝不能借版本号这个口子进属性
        assert_eq!(SafeVersion::parse("季度复盘会").as_str(), "unknown");
        assert_eq!(SafeVersion::parse(&"9".repeat(64)).as_str(), "unknown");
    }

    #[test]
    fn 系统版本与语言标签都收敛成低基数值() {
        assert_eq!(normalize_os_version("15.6.1"), "15.6.1");
        assert_eq!(normalize_os_version("Unknown"), "unknown");
        assert_eq!(normalize_os_version("22.04 (Jammy Jellyfish)"), "22.04");
        assert_eq!(normalize_locale(Some("zh_CN")), "zh-CN");
        assert_eq!(normalize_locale(Some("en-US")), "en-US");
        assert_eq!(normalize_locale(Some("zh-Hans-CN")), "zh-Hans-CN");
        assert_eq!(normalize_locale(None), "unknown");
        assert_eq!(normalize_locale(Some("!!!")), "unknown");
        // 校验而非清洗:洗一洗就放行等于给低基数维度留了自由文本口子
        assert_eq!(normalize_locale(Some("ALICE_PRIVATE")), "unknown");
        assert_eq!(normalize_locale(Some("a")), "unknown");
    }

    /// 系统名两端必须给同一个串,否则同一台机器在看板上劈成两行。
    #[test]
    fn 系统名收敛成固定几档() {
        assert_eq!(canonical_os(os_info::Type::Macos), "macOS");
        assert_eq!(canonical_os(os_info::Type::Windows), "Windows");
        assert_eq!(canonical_os(os_info::Type::Ubuntu), "other");
    }

    /// 硬崩溃是本模块唯一看不见的故障类型,靠"运行中"标记回头看补上。
    #[test]
    fn 上次没干净退出会被下次启动看见() {
        let tmp = tempfile::tempdir().unwrap();
        // 首次启动:没有痕迹
        let first = open_session(tmp.path());
        assert!(!first.unclean_exit, "首次启动不该报崩溃: {first:?}");
        assert_eq!(first.updated_from, None);
        // 没调 close_session 就再启动一次 = 上次崩了
        let second = open_session(tmp.path());
        assert!(second.unclean_exit, "上次没干净退出必须被看见");
        assert_eq!(second.last_version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
        // 干净退出后再启动就不该再报
        close_session(tmp.path());
        let third = open_session(tmp.path());
        assert!(!third.unclean_exit, "干净退出后不得误报崩溃");
    }

    #[test]
    fn 版本变化在下次启动被识别为升级() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(SESSION_FILE),
            r#"{"running":false,"version":"0.0.1"}"#,
        )
        .unwrap();
        let boot = open_session(tmp.path());
        assert_eq!(boot.updated_from.as_deref(), Some("0.0.1"));
        assert!(!boot.unclean_exit);
        // 同版本再启动不该被当成升级
        close_session(tmp.path());
        assert_eq!(open_session(tmp.path()).updated_from, None);
    }

    /// 坏文件/无文件都不能挡住启动——这是观测设施,不是主流程。
    /// 多开时第二个实例正常退出,不该把第一个实例那份"运行中"抹掉。
    #[test]
    fn 别的进程落的运行中标记不会被清掉() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(SESSION_FILE);
        std::fs::write(
            &path,
            format!(r#"{{"running":true,"version":"0.12.0","pid":{}}}"#, std::process::id() + 1),
        )
        .unwrap();
        close_session(tmp.path());
        assert!(open_session(tmp.path()).unclean_exit, "别人的运行中标记必须留着");
    }

    #[test]
    fn 会话标记坏掉时静默当作没有痕迹() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(SESSION_FILE), "{ 这不是 json").unwrap();
        let boot = open_session(tmp.path());
        assert_eq!(boot, BootState { unclean_exit: false, last_version: None, updated_from: None });
    }

    /// 关掉遥测时压队必须清空:那些事件是在"当时还开着"的前提下攒的。
    #[test]
    fn 关掉遥测会清空压队() {
        let _g = gate();
        set_enabled(true);
        outbox().lock().unwrap().pending.push(("app_started", None));
        set_enabled(false);
        assert!(outbox().lock().unwrap().pending.is_empty(), "关掉后压队必须清空");
        set_enabled(true); // 复原,别影响同进程里别的用例
    }

    /// 总开关默认关:panic hook 在 run() 开头就装好,而设置要到 setup() 才读得到。
    /// 默认开的话,那几毫秒里的 panic 会绕过一个明确关掉了上报的用户的意愿
    /// (codex review P1#1)。
    #[test]
    fn 总开关默认关且拦得住panic事件() {
        let _g = gate();
        set_enabled(false);
        let mut ev = posthog_rs::Event::new_anon("$exception");
        ev.insert_prop("$exception_fingerprint", "gate_probe").unwrap();
        assert!(before_send(ev).is_none(), "关掉后连 panic 事件也必须拦下");
        assert!(before_send(posthog_rs::Event::new_anon("app_started")).is_none());
        set_enabled(true);
    }

    /// 身份与压队同一把锁:设 id 与取队列必须原子,否则读到"无 id"之后入队的事件
    /// 会漏排(codex review P2#5)。
    #[test]
    fn 设id会把压队一并取走() {
        let _g = gate();
        set_enabled(true);
        {
            let mut o = outbox().lock().unwrap();
            o.id = None;
            o.pending.clear();
            o.pending.push(("app_started", None));
        }
        set_distinct_id("test-id-drain");
        let o = outbox().lock().unwrap();
        assert!(o.pending.is_empty(), "设 id 必须把压队一并取走");
        assert_eq!(o.id.as_deref(), Some("test-id-drain"));
        drop(o);
        // 复原:别把这个 id 留给同进程里别的用例
        outbox().lock().unwrap().id = None;
    }

    #[test]
    fn 前端错误类别走白名单() {
        assert_eq!(ErrorKind::parse("update_failed"), Some(ErrorKind::UpdateFailed));
        assert_eq!(ErrorKind::parse("migration"), Some(ErrorKind::Migration));
        // 认不出的一律拒绝——否则自由文本从这个口子绕过全部红线
        assert_eq!(ErrorKind::parse("写入 /Users/张伟/x 失败"), None);
        assert_eq!(ErrorKind::parse(""), None);
    }

    #[test]
    fn payload_shape_locked() {
        let cases: Vec<(Event, &str, Option<Value>)> = vec![
            (Event::AppStarted, "app_started", None),
            (
                Event::RecordingStarted { source: RecordSource::Both },
                "recording_started",
                Some(json!({ "source": "both" })),
            ),
            (
                Event::RecordingStopped { duration_ms: 10 * 60_000 },
                "recording_stopped",
                Some(json!({ "duration_bucket": "5-30m" })),
            ),
            (
                Event::NoteRefined { provider: Provider::Doubao },
                "note_refined",
                Some(json!({ "provider": "doubao" })),
            ),
            (
                Event::NoteExported { format: ExportFormat::Md },
                "note_exported",
                Some(json!({ "format": "md" })),
            ),
            (
                Event::McpToolUsed { op: McpOp::Stop },
                "mcp_tool_used",
                Some(json!({ "tool": "stop" })),
            ),
            (
                Event::TranscriptReady { engine: AsrEngine::Cloud, empty: false },
                "transcript_ready",
                Some(json!({ "engine": "cloud", "empty": false })),
            ),
            (
                Event::PermissionChecked { kind: PermissionKind::Screen, granted: true },
                "permission_checked",
                Some(json!({ "kind": "screen", "granted": true })),
            ),
            (
                Event::AiConfigured { provider: Provider::Kimi },
                "ai_configured",
                Some(json!({ "provider": "kimi" })),
            ),
            (
                Event::AppUpdated { from_version: SafeVersion::parse("0.11.0") },
                "app_updated",
                Some(json!({ "from_version": "0.11.0" })),
            ),
            (
                // 自由文本走版本号这个口子进属性 → 必须落成 unknown
                Event::AppUncleanExit { version: SafeVersion::parse("季度复盘会") },
                "app_unclean_exit",
                Some(json!({ "version": "unknown" })),
            ),
        ];
        for (event, name, props) in cases {
            let (n, p) = event.payload();
            assert_eq!(n, name);
            assert_eq!(p, props);
        }
    }

    // 原 gate_blocks_without_key 已随供应商删除:它测的是「没配 App-Key 就不上报」,
    // 而 App-Key 是 Aptabase 的概念。上报门这一层要等新后端接入时按其形态重建。
}
