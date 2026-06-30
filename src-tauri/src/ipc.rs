use serde::Serialize;

/// 快流临时文本，事件名 "partial"。
#[derive(Debug, Clone, Serialize)]
pub struct PartialEvent {
    pub text: String,
}

/// 录制状态，事件名 "status"。
#[derive(Debug, Clone, Serialize)]
pub struct StatusEvent {
    pub state: String, // "recording" | "stopped" | "error"
}
