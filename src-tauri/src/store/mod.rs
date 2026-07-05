pub mod audio;
pub mod writer;
mod export;
mod notes;
mod voiceprints;
pub use notes::NoteStore;
pub use voiceprints::VoiceprintStore; // lib.rs 四命令 + 种子/入库回写直接消费,无需 allow。
// Person/PersonCentroid/Voiceprints/AUTO_ENROLL_MS 曾在此 re-export(供未来前端类型
// 生成/测试引用),但全仓 grep 确认无一处经 store:: 路径消费——终审删掉,要用时再导出。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const SCHEMA_VERSION: u32 = 1;

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
}

/// 一场会议的完整内容（详情页 / 导出用）。
#[derive(Debug, Clone, Serialize)]
pub struct Note {
    pub meta: NoteMeta,
    pub segments: Vec<SegmentRecord>,
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
