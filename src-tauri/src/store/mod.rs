pub mod writer;

use serde::{Deserialize, Serialize};
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
}

/// 一场会议的完整内容（详情页 / 导出用）。
#[derive(Debug, Clone, Serialize)]
pub struct Note {
    pub meta: NoteMeta,
    pub segments: Vec<SegmentRecord>,
    /// load 时因损坏被跳过的行数（>0 时前端可提示）。
    pub skipped_lines: u32,
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

/// meta.json 原子写：先写 meta.json.tmp 再 rename，任何时刻磁盘上的 meta.json 都完整。
pub(crate) fn write_meta_atomic(note_dir: &Path, meta: &NoteMeta) -> anyhow::Result<()> {
    let tmp = note_dir.join("meta.json.tmp");
    let json = serde_json::to_string_pretty(meta)?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, note_dir.join("meta.json"))?;
    Ok(())
}
