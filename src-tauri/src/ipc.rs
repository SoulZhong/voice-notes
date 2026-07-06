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
    /// 活跃录制毫秒数（不含暂停期；续录含历史 base_ms）。仅 recording/paused 状态
    /// 有意义，其余为 0。
    pub elapsed_ms: u64,
}

/// 麦克风电平（闸前 RMS，0..1 量级），事件名 "level"，约 10Hz。
#[derive(Debug, Clone, Serialize)]
pub struct LevelEvent {
    pub rms: f32,
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
    /// 关联的全局声纹库人物 id(P<n>)：实时入库/种子命中后即有。前端以它为
    /// 说话人的全局唯一编号展示;None = 尚未够料入库(新声音的短暂过渡态)。
    pub person_id: Option<String>,
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

/// 声纹库人物摘要，供 `list_people` 返回、管理页展示。sources 取该人已有质心的信道集合
/// （"mic"/"system"），不是"当前在场"，纯粹反映库里记录过哪些信道的声纹。
#[derive(Debug, Clone, Serialize)]
pub struct PersonSummary {
    pub id: String,
    pub name: String,
    pub total_ms: u64,
    pub last_seen: String,
    pub sources: Vec<String>,
    /// 录音样本绝对路径列表(按会议逐份累积,至多 MAX_SAMPLES;合并会带入对方的样本)。
    /// 空 = 库中无样本(旧数据/写失败),前端据此决定是否显示「试听」。
    pub sample_paths: Vec<String>,
}

/// 目录迁移进度，事件名 "migrate"。kind∈{"data","models"} 标明迁的是哪条目录;
/// phase∈{"copying","done","error"};error 时 message 带原因,其余为空串。
#[derive(Debug, Clone, Serialize)]
pub struct MigrateEvent {
    pub kind: String,
    pub phase: String,
    pub message: String,
}

/// 模型下载进度，事件名 "model_download"。artifact="all" + phase="done" 表示整体完成。
/// phase: downloading | verifying | extracting | done | error | cancelled。
#[derive(Debug, Clone, Serialize)]
pub struct ModelDownloadEvent {
    pub artifact: String,
    pub phase: String,
    pub received_bytes: u64,
    pub total_bytes: u64,
    /// error 时的原因说明，其余为空串。
    pub message: String,
}

/// 会后精修进度，事件名 "refine"。stage ∈ {"filter","recluster","llm","all"}；
/// state ∈ {"running","done","failed","partial","skipped","off"}（含义随 stage 而定，
/// 语义与 store::RefineStages/RefinedDoc.stages 的字符串一致）。
#[derive(Debug, Clone, Serialize)]
pub struct RefineEvent {
    pub note_id: String,
    pub stage: String,
    pub state: String,
}
