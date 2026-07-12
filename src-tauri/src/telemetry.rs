//! 匿名使用统计(遥测)薄封装——全应用唯一上报入口。
//!
//! 隐私红线:事件属性只允许固定枚举与数值桶。禁止上报会议内容、笔记标题、
//! 说话人名、文件路径、API key、模型接入点 ID、任何自由文本。事件与属性
//! 用枚举建模,从类型上杜绝自由字符串进属性;新增事件必须扩 Event 枚举。
//! 上报失败静默(插件内部批量缓冲+重试),绝不影响主流程。
//! 设计:docs/superpowers/specs/2026-07-12-voice-notes-telemetry-design.md

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

/// Aptabase App-Key。空串 = 遥测整体停用(插件不注册、track 短路),
/// 代码可先合入,拿到 key 后填入即激活。形如 "A-EU-xxxxxxxxxx"。
pub const APP_KEY: &str = "";

/// 录制源类别。由设置推断而非实际启动结果:遥测只要低基数类别,不追精确。
#[derive(Debug, PartialEq)]
pub enum RecordSource {
    Mic,
    System,
    Both,
}

impl RecordSource {
    /// 仅录系统声 → system;否则 macOS 双源 both、其他平台 mic。
    pub fn from_settings(record_system_only: bool) -> Self {
        if record_system_only {
            RecordSource::System
        } else if cfg!(target_os = "macos") {
            RecordSource::Both
        } else {
            RecordSource::Mic
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            RecordSource::Mic => "mic",
            RecordSource::System => "system",
            RecordSource::Both => "both",
        }
    }
}

/// 精修 provider 类别。预设 base_url 前缀与前端 REFINE_PRESETS
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

/// 上报门:key 未配置或用户关闭开关 → 不发。app_data_dir 拿不到 → 按关闭处理;
/// settings.json 缺失/损坏 → settings::load 回退默认值(telemetry_enabled=true),
/// 等同新装默认开——opt-out 语义下这是预期行为,不是异常路径上报。
fn gate(app_key: &str, telemetry_enabled: bool) -> bool {
    !app_key.is_empty() && telemetry_enabled
}

/// 唯一上报入口。每次现读设置(与 spawn_session/spawn_refine 同哲学,
/// 事件稀疏、读盘便宜),开关翻转即时生效、无需重启。
pub fn track(app: &AppHandle, event: Event) {
    // key 未配置时提前短路:不必为注定丢弃的事件读盘(MCP 轮询频繁)。
    if APP_KEY.is_empty() {
        return;
    }
    let enabled = app
        .path()
        .app_data_dir()
        .map(|d| crate::settings::load(&d).telemetry_enabled)
        .unwrap_or(false);
    if !gate(APP_KEY, enabled) {
        return;
    }
    use tauri_plugin_aptabase::EventTracker;
    let (name, props) = event.payload();
    let _ = app.track_event(name, props); // 上报失败静默(设计约束)
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

    #[test]
    fn gate_blocks_without_key_or_consent() {
        assert!(!gate("", true)); // 未配 key:整体停用
        assert!(!gate("A-EU-123", false)); // 用户关闭
        assert!(!gate("", false));
        assert!(gate("A-EU-123", true));
    }
}
