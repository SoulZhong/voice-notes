pub mod aing_graph;
pub mod align;
pub mod audio;
pub mod disk;
mod export;
pub mod merge_journal;
pub mod sample_trace;
pub mod split_ops;
pub mod migrate;
pub mod mix_regen;
pub mod notelock;
mod notes;
pub mod refined;
pub mod transcode;
mod voiceprints;
pub mod writer;
pub use aing_graph::{
    ensure_graph_ids, evidence_id, mention_id, relation_fact_id, source_hash, stable_id,
    GraphExtraction, RelationEvidence, RelationFact, RelationPredicate,
};
pub use export::render_refined; // GUI 导出与 MCP get_note 共用的 Aing 渲染(format_ts 已无 store:: 路径消费者,不再 re-export)。
pub use notes::NoteStore;
pub use refined::apply_refined_texts; // Agent Aing 写回(mcp::tools 消费)。
pub use refined::load_refined_for_display; // 纯展示面(笔记页/导出)专用:额外套跨轨时基投影,绝不可用于任何会写回的路径。
pub use refined::{aing_exists, AING_DOC_FILE, LEGACY_REFINED_FILE}; // 迁移感知的存在性判断 + 落盘/旧文件名(mcp::tools、refine::agent 消费)。
pub use refined::{assign_refined_person, join_library_names, rename_refined_speaker, unassign_refined_person_if}; // 修订稿说话人编辑三件套(lib.rs 命令层消费)。
pub use refined::{save_refined_paragraphs, ParagraphPayload}; // 笔记页 WYSIWYG 整篇保存(lib.rs 命令层消费)。
pub use refined::{
    load_refined, write_refined_atomic, Entity, Mention, RefineStages, RefinedDoc, RefinedParagraph,
};
pub use voiceprints::RestoreOutcome; // 快照回放结局(feedback 消费,含"质心已置空待重建")。
pub use voiceprints::confident_picks; // 自动归并筛选(apply_confident_merges 命令消费)。
pub use voiceprints::seed_clusters; // 开录/Aing 种子构建(主质心+会话变体,lib.rs 消费)。
pub use voiceprints::suggest_merges; // 整理·再辨认(suggest_person_merges 命令消费)。
pub use voiceprints::MergeSuggestion; // confident_picks 出入参类型(lib.rs 消费)。
pub use voiceprints::VoiceprintStore; // lib.rs 四命令 + 种子/入库回写直接消费,无需 allow。
pub use voiceprints::{Person, PersonCentroid}; // refine::identify 候选召回读取质心/last_seen,及其测试构造消费。
pub use voiceprints::Voiceprints; // graph::resolve_global_id 命名此类型(人实体→person_id 匹配)。
pub use voiceprints::AUTO_ENROLL_MS; // lib.rs 实时入库回调(registry enroller)用同一门槛。
pub use voiceprints::MAX_SAMPLES; // merge_person 判断样本是否超额(超额才付声纹模型加载成本)。
                                  // Person/PersonCentroid/AUTO_ENROLL_MS 曾在此 re-export(供未来前端类型生成/测试引用),
                                  // 但全仓 grep 确认无一处经 store:: 路径消费——终审删掉,要用时再导出。Voiceprints 例外:
                                  // graph::resolve_global_id 需要具名此类型,已于上方重新导出。
pub use merge_journal::{MergeJournal, MergeJournalEntry}; // 合并日志(lib.rs 命令层消费)。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const SCHEMA_VERSION: u32 = 1;
pub const SEGMENT_SUPPRESSIONS_FILE: &str = "segment-suppressions.jsonl";

/// 一场会议的元数据，存 meta.json（原子写）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteMeta {
    pub schema_version: u32,
    pub id: String,
    pub title: String,
    /// RFC3339 本地时区；meta 损坏兜底时可为空串。
    pub started_at: String,
    pub ended_at: Option<String>,
    /// "recording" | "complete"
    pub state: String,
    /// 匹配到的日历事件快照(P3):serde default 兼容旧 meta;未匹配省略键。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar: Option<CalendarSnapshot>,
    /// 用户明确清除过日历关联的 tombstone:自动匹配/backfill 永不再绑,
    /// 手动改选会复位。没有它,「清除」在下一次 backfill 就被推翻。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub calendar_cleared: bool,
    /// 本场转写实际使用的识别引擎("firered"/"qwen3"/…,云端记 "cloud:厂商")。
    /// 每场覆盖:续录换了引擎以最后一场为准(与 SyncInfo 同限制)。2026-08-14
    /// 排查教训:引擎选型与实际生效可能不一致(模型未就绪、录制中切换),
    /// 不落盘就无从对证是哪个引擎转的这场。serde default 兼容旧 meta。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_engine: Option<String>,
}

/// 日历事件快照(P3):落盘即快照——title/attendees 是匹配时刻的副本,不依赖
/// event_id 活性(事件被改/删后快照仍自洽);event_id 仅供改选时重新定位。
/// 字段固定序列化(不 skip):前端 TS 类型全必填,空参会人也写空数组。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarSnapshot {
    pub event_id: String,
    pub title: String,
    #[serde(default)]
    pub attendees: Vec<CalendarAttendee>,
    pub matched_at: String,
    /// "auto"(停止后/backfill 自动匹配)| "manual"(用户改选)。
    #[serde(default)]
    pub match_kind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarAttendee {
    #[serde(default)]
    pub name: String,
    /// 已规范化(trim+小写,mailto 剥离+percent-decode);无邮箱为空串。
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub is_me: bool,
}

/// 一条定稿段，存 segments.jsonl（每段一行）。speaker 为 P4 说话人区分预留。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentRecord {
    pub seq: u64,
    pub source: String, // "mic" | "system"
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub speaker: Option<String>,
    /// 段音频均方根(16k f32),纯诊断:为 AEC 残渣能量门槛攒真实数据(A1 backlog)。
    /// 旧笔记无此键 → None;None 不写盘,新旧行形状双向兼容。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rms: Option<f32>,
}

/// 对原始段的可逆隐藏决定。原始 `segments.jsonl` 永不因自动规则删除；默认视图
/// 应用本记录隐藏命中段，诊断/恢复路径仍可读取 `Note::suppressed_segments`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentSuppression {
    pub seq: u64,
    pub reason: String,
}

/// 一位说话人的可持久化信息，存 speakers.json（键为说话人 id，如 "S1"）。
/// name 空串 = 未改名，显示端兜底「说话人 N」。
/// centroid/count 为 P4.5 续录铺底新增字段：serde default + skip_serializing_if 保证
/// 旧 speakers.json（无这两字段）可解析，且无质心时序列化省去 centroid 键。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeakerMeta {
    pub name: String,
    pub sources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub centroid: Option<Vec<f32>>,
    #[serde(default)]
    pub count: u64,
    /// 关联的全局声纹库人物 id(经 VoiceprintStore::resolve 解析)。P4 registry
    /// 种子命中/入库时回填;serde default + skip_serializing_if 保证旧
    /// speakers.json(无该键)可解析,且未关联时序列化省去该键。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_id: Option<String>,
    /// 「多人混杂」标记:这个簇里不止一个人在说话,归给谁都是错的。事后打标
    /// (录音时无从知道,重聚类才暴露)。置位后重转写/再入库不入库、不写样本,
    /// UI 提供拆分入口。见 docs/superpowers/specs/2026-08-20-mixed-speaker-split-design.md。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub multi_speaker: bool,
}

/// 一场会议的完整内容（详情页 / 导出用）。
#[derive(Debug, Clone, Serialize)]
pub struct Note {
    pub meta: NoteMeta,
    pub segments: Vec<SegmentRecord>,
    /// 被自动规则隐藏的原始段。与 `segments` 互斥，按原始时间顺序返回。
    pub suppressed_segments: Vec<SegmentRecord>,
    /// load 时因损坏被跳过的行数（>0 时前端可提示）。
    pub skipped_lines: u32,
    pub speakers: BTreeMap<String, SpeakerMeta>,
}

/// 列表项。state 除 meta 的两态外，command 层会把当前活动会话改写为 "active"。
#[derive(Debug, Clone, Serialize)]
pub struct NoteSummary {
    pub id: String,
    pub title: String,
    pub started_at: String,
    pub duration_secs: Option<u64>,
    pub state: String,
}

/// 笔记 id 合法性校验（防路径穿越），供 NoteStore::note_dir 与 NoteWriter::resume 共用。
pub(crate) fn validate_note_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty() || id.contains('/') || id.contains('\\') || id.contains("..") {
        anyhow::bail!("非法笔记 id: {id:?}");
    }
    Ok(())
}

/// meta.json 原子写：先写 meta.json.tmp 再 rename，任何时刻磁盘上的 meta.json 都完整。
/// 改写某篇笔记的 `asr_engine`(离线路径用:重转写换了引擎之后要如实记账,
/// 否则「疑似识别失败,换引擎重转写」的建议会照着旧引擎反复提示同一篇)。
/// 只动这一个字段,其余原样读回写回。
pub fn set_note_asr_engine(note_dir: &Path, engine: &str) -> anyhow::Result<()> {
    let path = note_dir.join("meta.json");
    let text = std::fs::read_to_string(&path)?;
    let mut meta: NoteMeta = serde_json::from_str(&text)?;
    meta.asr_engine = Some(engine.to_string());
    write_meta_atomic(note_dir, &meta)
}

pub(crate) fn write_meta_atomic(note_dir: &Path, meta: &NoteMeta) -> anyhow::Result<()> {
    let tmp = note_dir.join("meta.json.tmp");
    let json = serde_json::to_string_pretty(meta)?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, note_dir.join("meta.json"))?;
    Ok(())
}

/// speakers.json 原子写：同 meta 策略，先写 speakers.json.tmp 再 rename。
pub(crate) fn write_speakers_atomic(
    note_dir: &Path,
    speakers: &BTreeMap<String, SpeakerMeta>,
) -> anyhow::Result<()> {
    let tmp = note_dir.join("speakers.json.tmp");
    let json = serde_json::to_string_pretty(speakers)?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, note_dir.join("speakers.json"))?;
    Ok(())
}
