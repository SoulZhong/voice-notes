use serde::{Deserialize, Serialize};

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

/// 追溯回声撤回，事件名 "final_retract"：一条已上屏的 mic 段事后被确认为 system
/// 段的回声（system 长句晚于 mic 回声段定稿）。前端应从已显示的 finals 中移除
/// (source, start_ms, text) 精确匹配的那一行；磁盘侧由后端同步删除。
#[derive(Debug, Clone, Serialize)]
pub struct RetractEvent {
    pub source: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// 音频转码完成，事件名 "transcode_done"。停录后 WAV→m4a 转码结束(源 WAV 已删),
/// 打开中的详情页应重拉音轨——否则播放器仍握着已删除 WAV 的引用,呈现"无声播放"。
#[derive(Debug, Clone, Serialize)]
pub struct TranscodeEvent {
    pub note_id: String,
}

/// 后端自动改名(LLM 主题标题),事件名 "note_renamed":侧栏列表与详情页据此刷新
/// 标题——改名发生在后台 Aing 线程,前端不会主动重拉。
#[derive(Debug, Clone, Serialize)]
pub struct NoteRenamedEvent {
    pub note_id: String,
    pub title: String,
}

/// 落盘健康度，事件名 "storage"。"degraded" = 追加写失败（段暂存内存）；"ok" = 已恢复。
#[derive(Debug, Clone, Serialize)]
pub struct StorageEvent {
    pub state: String,
}

/// 采集源运行期健康,事件名 "source_health"。录制中某源断流自愈的结局通报:
/// "recovered" = 重启成功帧已续上;"lost" = 一轮重试耗尽本场放弃(该源时间轴
/// 由静音填充维持,另一源不受影响)。前端可据此提示"麦克风已断开/已恢复";
/// 未接监听也不影响任何现有流程。
#[derive(Debug, Clone, Serialize)]
pub struct SourceHealthEvent {
    pub source: String, // "mic" | "system"
    pub state: String,  // "recovered" | "lost"
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
    /// 与 sample_paths 一一对应的录制日期(文件 mtime,RFC3339;取不到给空串)。
    /// 样本文件在会议停止时写入,mtime≈该场会议时间,足够做「哪场的声音」标注。
    pub sample_dates: Vec<String>,
}

/// 相关笔记(笔记详情页「相关笔记」区):与当前笔记共享 Aing 实体的其他笔记 + 共享实体数。
#[derive(Debug, Clone, Serialize)]
pub struct RelatedNote {
    pub id: String,
    pub title: String,
    pub started_at: String,
    pub shared_entities: i64,
}

/// 图谱实体摘要(列表 / 力导图节点)。镜像 graph::EntityRow(后者无 Serialize)。
#[derive(Debug, Clone, Serialize)]
pub struct EntitySummary {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub is_person: bool,
    pub note_count: i64,
    pub mention_total: i64,
}

/// 力导图一条共现边(a<b,weight=共享笔记数)。
#[derive(Debug, Clone, Serialize)]
pub struct EdgeRow {
    pub a: String,
    pub b: String,
    pub weight: i64,
}

/// 力导图数据:节点(全部实体)+ 边(共现)。
#[derive(Debug, Clone, Serialize)]
pub struct GraphData {
    pub nodes: Vec<EntitySummary>,
    pub edges: Vec<EdgeRow>,
}

/// A published evidence-backed semantic relation. Co-occurrence edges deliberately use
/// `EdgeRow` instead so callers cannot accidentally present a weak link as a fact.
#[derive(Debug, Clone, Serialize)]
pub struct SemanticEdge {
    pub id: String,
    pub subject_id: String,
    pub object_id: String,
    pub predicate_type: String,
    pub predicate_label: Option<String>,
    pub status: String,
    pub confidence: f64,
    pub origin: String,
    pub evidence_count: i64,
    pub note_count: i64,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SemanticGraphData {
    pub nodes: Vec<EntitySummary>,
    pub semantic_edges: Vec<SemanticEdge>,
    pub cooccurrence_edges: Vec<EdgeRow>,
    pub degraded: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SemanticEntityDetail {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub confirmed: bool,
    pub is_person: bool,
    pub note_count: i64,
    pub mention_total: i64,
    pub relations: Vec<SemanticEdge>,
    pub degraded: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelationEvidence {
    pub id: String,
    pub note_id: String,
    pub paragraph_index: i64,
    pub start_offset: i64,
    pub end_offset: i64,
    pub quote: String,
    pub source_seqs: Vec<u64>,
    pub source_hash: String,
    pub subject_mentions: Vec<String>,
    pub object_mentions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelationDetail {
    pub relation: SemanticEdge,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub note_ids: Vec<String>,
    pub evidence: Vec<RelationEvidence>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingReviewItem {
    pub id: String,
    pub kind: String,
    pub note_id: Option<String>,
    pub relation_id: Option<String>,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct MentionEvidence {
    pub id: String,
    pub note_id: String,
    pub entity_id: String,
    pub paragraph_index: i64,
    pub start_offset: i64,
    pub end_offset: i64,
    pub quote: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct KnowledgePathStep {
    pub id: String,
    pub from_id: String,
    pub to_id: String,
    pub subject_id: String,
    pub object_id: String,
    pub predicate_type: String,
    pub predicate_label: Option<String>,
    pub direction: String,
    pub origin: String,
    pub confidence: f64,
    pub evidence_count: i64,
    pub note_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct KnowledgePath {
    pub entity_ids: Vec<String>,
    pub steps: Vec<KnowledgePathStep>,
    pub total_cost: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum KnowledgeOperationInput {
    RenameEntity {
        entity_id: String,
        name: String,
    },
    AddAlias {
        entity_id: String,
        alias: String,
    },
    RemoveAlias {
        entity_id: String,
        alias: String,
    },
    BindMention {
        mention_id: String,
        entity_id: String,
    },
    ConfirmRelation {
        relation_id: String,
    },
    EditRelation {
        relation_id: String,
        subject_id: String,
        predicate: crate::store::RelationPredicate,
        object_id: String,
        valid_from: Option<String>,
        valid_to: Option<String>,
        note: Option<String>,
    },
    SuppressRelation {
        subject_id: String,
        predicate: crate::store::RelationPredicate,
        object_id: String,
    },
    EndRelation {
        relation_id: String,
        valid_to: String,
    },
    RestoreRelation {
        operation_id: String,
    },
    CreateEntity {
        kind: String,
        name: String,
        #[serde(default)]
        aliases: Vec<String>,
    },
    CreateRelation {
        subject_id: String,
        predicate: crate::store::RelationPredicate,
        object_id: String,
        valid_from: Option<String>,
        valid_to: Option<String>,
        note: Option<String>,
        #[serde(default)]
        evidence_ids: Vec<String>,
        user_assertion: bool,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct SplitEntityRequest {
    pub entity_id: String,
    pub name: String,
    pub kind: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub mention_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct KnowledgeMutationResult {
    pub operation_id: String,
    pub entity_id: Option<String>,
    pub rebuild_state: String,
    pub rebuild_generation: Option<u64>,
}

#[cfg(test)]
mod knowledge_mutation_result_tests {
    use super::KnowledgeMutationResult;

    #[test]
    fn serialization_exposes_queued_generation_and_committed_null() {
        let queued = serde_json::to_value(KnowledgeMutationResult {
            operation_id: "op_queued".into(),
            entity_id: None,
            rebuild_state: "queued".into(),
            rebuild_generation: Some(37),
        })
        .unwrap();
        assert_eq!(queued["rebuild_generation"], 37);

        let committed = serde_json::to_value(KnowledgeMutationResult {
            operation_id: "op_committed".into(),
            entity_id: None,
            rebuild_state: "committed".into(),
            rebuild_generation: None,
        })
        .unwrap();
        assert!(committed["rebuild_generation"].is_null());
    }
}

/// 实体详情面板里「出现的笔记」一项(联查了标题)。
#[derive(Debug, Clone, Serialize)]
pub struct EntityNoteRef {
    pub id: String,
    pub title: String,
    pub started_at: String,
    pub mention_count: i64,
}

/// 实体详情面板里「相关(共现)实体」一项。
#[derive(Debug, Clone, Serialize)]
pub struct RelatedEntity {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub shared_notes: i64,
}

/// 实体详情(右侧面板)。
#[derive(Debug, Clone, Serialize)]
pub struct EntityDetail {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub is_person: bool,
    pub note_count: i64,
    pub mention_total: i64,
    pub notes: Vec<EntityNoteRef>,
    pub related: Vec<RelatedEntity>,
}

/// 笔记页高亮点击导航:局部实体 id → 全局 id(+是否人实体)。
#[derive(Debug, Clone, Serialize)]
pub struct EntityLink {
    pub local_id: String,
    pub global_id: String,
    pub is_person: bool,
}

/// 实体改名结果:new_id 是改名后的规范 id(人实体不变,非人随名字重算);
/// merged=true 表示撞上已存在的同名实体,已自动合并。
#[derive(Debug, Clone, Serialize)]
pub struct RenameEntityResult {
    pub new_id: String,
    pub merged: bool,
}

/// 整理·合并建议(suggest_person_merges 返回):把 loser 并入 winner 的推荐,
/// 相似度是共有信道质心余弦的最大值;salience 是 S-Norm 显著性 z 分数(库太小
/// 算不出分布时 None);name 空串=未命名(前端按「说话人 N」兜底)。
#[derive(Debug, Clone, Serialize)]
pub struct PersonMergeSuggestion {
    pub loser: String,
    pub loser_name: String,
    pub winner: String,
    pub winner_name: String,
    pub similarity: f32,
    pub source: String,
    pub salience: Option<f32>,
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

/// 会后 Aing 进度，事件名 "refine"。stage ∈ {"filter","recluster","llm","all"}；
/// state ∈ {"running","done","failed","partial","skipped","off"}（含义随 stage 而定，
/// 语义与 store::RefineStages/RefinedDoc.stages 的字符串一致）。
#[derive(Debug, Clone, Serialize)]
pub struct RefineEvent {
    pub note_id: String,
    pub stage: String,
    pub state: String,
}

/// 用户显式发起的关系补建请求。`note_ids=None` 使用只读 preview 的默认选择；
/// 显式列表始终逐个校验，不会在坏 id 时退回全库扫描。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackfillRequest {
    pub note_ids: Option<Vec<String>>,
    pub provider: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackfillPreview {
    pub note_ids: Vec<String>,
    pub provider: String,
    pub model: String,
    pub contract_version: u32,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BackfillFailure {
    pub note_id: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackfillProgress {
    pub state: String,
    pub completed: usize,
    pub total: usize,
    pub current_note_id: Option<String>,
    pub failed: Vec<BackfillFailure>,
}
