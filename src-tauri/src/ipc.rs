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
    pub state: String, // "recording" | "stopped" | "error: .."；recording_status 查询另可返回 "idle"
    /// 系统声音可用性："on" | "denied" | "unavailable"；非录制态可为空串。
    pub system_audio: String,
    /// 本次会话的笔记 id；recording / stopped 时携带，其余为空串。
    pub note_id: String,
}

/// 一句定稿文本，事件名 "final"。
#[derive(Debug, Clone, Serialize)]
pub struct FinalEvent {
    pub source: String, // "mic" | "system"
    pub text: String,
    /// 相对该源流开始的毫秒（≈会议开始；双源起点存在毫秒级偏差，
    /// 展示用途可接受，见设计文档 §8）。
    pub start_ms: u64,
    pub end_ms: u64,
}

/// 落盘健康度，事件名 "storage"。"degraded" = 追加写失败（段暂存内存）；"ok" = 已恢复。
#[derive(Debug, Clone, Serialize)]
pub struct StorageEvent {
    pub state: String,
}
