use serde::Serialize;

/// 快流临时文本，事件名 "partial"。
#[derive(Debug, Clone, Serialize)]
pub struct PartialEvent {
    pub source: String, // "mic" | "system"
    pub text: String,
}

/// 录制状态，事件名 "status"。
#[derive(Debug, Clone, Serialize)]
pub struct StatusEvent {
    pub state: String, // "recording" | "stopped" | "error: .."
    /// 系统声音可用性："on" | "denied" | "unavailable"；非录制态可为空串。
    pub system_audio: String,
}

/// 一句定稿文本，事件名 "final"。
#[derive(Debug, Clone, Serialize)]
pub struct FinalEvent {
    pub source: String, // "mic" | "system"
    pub text: String,
    /// 相对会议开始的毫秒。
    pub start_ms: u64,
    pub end_ms: u64,
}
