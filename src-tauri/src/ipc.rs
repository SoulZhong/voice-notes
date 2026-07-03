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
    /// 说话人区分可用性："on"（声纹模型就绪）| "unavailable"（模型缺失，降级）| ""（非录制态）。
    pub diarization: String,
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
    /// 声纹归簇得到的说话人 id（如 "S1"）；无 embedder / 嵌入失败 / 短段则为 None。
    pub speaker: Option<String>,
}

/// 落盘健康度，事件名 "storage"。"degraded" = 追加写失败（段暂存内存）；"ok" = 已恢复。
#[derive(Debug, Clone, Serialize)]
pub struct StorageEvent {
    pub state: String,
}

/// 说话人表(全量推送),事件名 "speakers"。name 空串 = 未改名(前端按 id 兜底)。
#[derive(Debug, Clone, Serialize)]
pub struct SpeakerEntry {
    pub id: String,
    pub name: String,
    pub sources: Vec<String>,
}

/// 一次簇合并：loser 的历史段应在前端改写为 winner，使历史徽章与新段统一。
#[derive(Debug, Clone, Serialize)]
pub struct MergedPair {
    pub loser: String,
    pub winner: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpeakersEvent {
    pub speakers: Vec<SpeakerEntry>,
    /// 本次事件伴随的簇合并（仅 Merged 分支非 None）；前端据此回写已上屏历史段。
    pub merged: Option<MergedPair>,
}
