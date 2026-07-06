//! 精修产物 refined.json:原始三文件之外的独立终稿,损坏/缺失时 UI 回落原始逐字稿。

use serde::{Deserialize, Serialize};
use std::path::Path;

pub const REFINED_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinedParagraph {
    pub speaker: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    pub source_seqs: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefineStages {
    pub filter: String,
    pub recluster: String,
    pub llm: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinedDoc {
    pub schema_version: u32,
    pub generated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_model: Option<String>,
    pub stages: RefineStages,
    #[serde(default)]
    pub discarded_seqs: Vec<u64>,
    pub paragraphs: Vec<RefinedParagraph>,
}

pub fn write_refined_atomic(note_dir: &Path, doc: &RefinedDoc) -> anyhow::Result<()> {
    let tmp = note_dir.join("refined.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(doc)?)?;
    std::fs::rename(&tmp, note_dir.join("refined.json"))?;
    Ok(())
}

pub fn load_refined(note_dir: &Path) -> Option<RefinedDoc> {
    let bytes = std::fs::read(note_dir.join("refined.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_corrupt_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_refined(dir.path()).is_none(), "缺失返回 None");
        let doc = RefinedDoc {
            schema_version: 1,
            generated_at: "2026-07-06T15:00:00+08:00".into(),
            llm_model: Some("deepseek-chat".into()),
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "off".into() },
            discarded_seqs: vec![1, 2],
            paragraphs: vec![RefinedParagraph {
                speaker: "R1".into(), name: Some("张三".into()),
                start_ms: 0, end_ms: 5000, text: "你好。".into(), source_seqs: vec![0, 3],
            }],
        };
        write_refined_atomic(dir.path(), &doc).unwrap();
        let got = load_refined(dir.path()).expect("写后可读");
        assert_eq!(got.paragraphs.len(), 1);
        assert_eq!(got.discarded_seqs, vec![1, 2]);
        assert_eq!(got.paragraphs[0].name.as_deref(), Some("张三"));
        std::fs::write(dir.path().join("refined.json"), "{broken").unwrap();
        assert!(load_refined(dir.path()).is_none(), "损坏返回 None 不 panic");
    }
}
