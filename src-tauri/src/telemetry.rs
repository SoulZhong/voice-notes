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
use tauri::AppHandle;

/// PostHog Project API Key。写进客户端的公开写入端点,不是机密(与前端同一个值,
/// 见 src/lib/analytics.ts)。空串 = 整体停用,便于本地开发与测试。
pub const PROJECT_KEY: &str = "phc_qgqdrtaowrPfMPzmD9b7e9JSUPRc3RY3oGAeeKtAAV7E";

/// 区域 host。必须与项目注册区域一致,选错连不上。
pub const HOST: &str = "https://us.i.posthog.com";

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

/// 全部遥测事件(首批 6 个,见设计文档事件清单)。
pub enum Event {
    AppStarted,
    RecordingStarted { source: RecordSource },
    RecordingStopped { duration_ms: u64 },
    NoteRefined { provider: Provider },
    NoteExported { format: ExportFormat },
    McpToolUsed { op: McpOp },
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
        }
    }
}

/// 唯一上报入口。失败静默——上报绝不影响主流程。
///
/// distinct_id 由前端生成并持久化,经 set_distinct_id 传入(见该函数说明)。
/// 拿到之前发生的事件(如启动早期)以 personless 形态上报,**绝不在此自造 id**:
/// 两边各生成一个会把同一个人算成两个人,漏斗与留存全部失真。
pub fn track(_app: &AppHandle, event: Event) {
    if PROJECT_KEY.is_empty() {
        return;
    }
    let (name, props) = event.payload();
    // id 未到位时必须用 new_anon(真正的 personless),不能用字面量 id:
    // AppStarted 发生在 webview 调 set_analytics_id 之前,若都记成同一个字面量,
    // 每台机器的启动会被算成同一个人,独立用户数与激活漏斗全废(codex review 发现)。
    let mut ev = match current_id() {
        Some(id) => posthog_rs::Event::new(name, &id),
        None => posthog_rs::Event::new_anon(name),
    };
    if let Some(Value::Object(map)) = props {
        for (k, v) in map {
            // 属性值只可能来自各枚举 as_str 与 duration_bucket(见 payload),
            // 这里不做二次校验,由 payload_shape_locked 在编译期之外守住形状。
            let _ = ev.insert_prop(k, v);
        }
    }
    posthog_rs::capture(ev);
}

/// 前端持久化的匿名 id。未设时用 personless 占位——见 track 的说明。
static DISTINCT_ID: std::sync::OnceLock<std::sync::RwLock<String>> = std::sync::OnceLock::new();

fn slot() -> &'static std::sync::RwLock<String> {
    DISTINCT_ID.get_or_init(|| std::sync::RwLock::new(String::new()))
}

fn current_id() -> Option<String> {
    let v = slot().read().map(|g| g.clone()).unwrap_or_default();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// 由前端在初始化后调用一次(命令壳 set_analytics_id)。幂等,后到的覆盖先到的。
pub fn set_distinct_id(id: &str) {
    if let Ok(mut g) = slot().write() {
        *g = id.to_string();
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
        // 出口统一脱敏。**panic hook 由 SDK 安装,它直接序列化 panic 载荷,
        // 不经 report_error、也就不经 redact**——panic 消息里常有 home 路径、
        // 文件名、上游错误文本(codex review 发现)。放在这里是唯一能覆盖所有
        // 发送路径的位置:手工上报、自动 panic、将来新增的任何 capture 都过它。
        .before_send(redact_event)
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
    if PROJECT_KEY.is_empty() {
        return;
    }
    posthog_rs::flush();
}

/// before_send 钩子:对异常类事件的文本字段做脱敏。
///
/// 只改属性值,**绝不丢事件**——丢了就看不见异常,与上报的目的相悖。
/// 钩子内不得 panic(SDK 会捕获并丢弃该事件),故全程用安全取值。
fn redact_event(mut ev: posthog_rs::Event) -> Option<posthog_rs::Event> {
    // 现代错误追踪用 $exception_list;标量字段兼容旧看板,两种都覆盖。
    if let Some(list) = ev.properties().get("$exception_list").cloned() {
        let cleaned = redact_exception_list(list);
        let _ = ev.insert_prop("$exception_list", cleaned);
    }
    // panic 事件还带 $exception_panic_file 这类独立的路径属性,
    // 以及栈帧里的 filename——只脱 type/value 会把它们漏出去(codex review 第二轮)。
    for key in ["$exception_panic_file", "$exception_message", "$exception_type", "$exception_source"] {
        let Some(v) = ev.properties().get(key).cloned() else { continue };
        if let Some(text) = v.as_str() {
            let _ = ev.insert_prop(key, crate::redact::redact(text));
        }
    }
    Some(ev)
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
    if PROJECT_KEY.is_empty() {
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let out = redact_event(ev).expect("绝不能丢事件——丢了就看不见异常");
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

        let out = redact_event(ev).expect("不得丢事件");
        let dumped = serde_json::to_string(out.properties()).unwrap();
        assert!(!dumped.contains("张伟"), "栈帧与 panic_file 里的路径都必须脱掉: {dumped}");
    }

    #[test]
    fn before_send不认识的结构原样放行而非丢弃() {
        let mut ev = posthog_rs::Event::new_anon("vn_page_view");
        ev.insert_prop("path", "/notes").unwrap();
        let out = redact_event(ev).expect("普通事件不得被丢弃");
        assert_eq!(out.properties().get("path").unwrap(), "/notes");
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
