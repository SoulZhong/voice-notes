//! 重转写的提交侧。破坏性覆盖三重保险(spec §提交与安全网):一次性备份、
//! 调用方提交门(本文件不管)、tmp+rename 原子切换。全部操作要求调用方持有
//! NoteLock——签名收 &NoteLock 是类型层面的持锁证明(照 write_refined_atomic_locked)。

use crate::store::notelock::NoteLock;
use crate::store::{SegmentRecord, SpeakerMeta};
use std::collections::BTreeMap;
use std::path::Path;

/// 首次重转写前的原稿备份。write-once:保「最初的原始转写」,不随后续轮次滚动。
pub const SEGMENTS_BACKUP_FILE: &str = "segments.orig.jsonl";

pub fn commit(
    note_dir: &Path,
    lock: &NoteLock,
    segs: &[SegmentRecord],
    speakers: &BTreeMap<String, SpeakerMeta>,
) -> anyhow::Result<()> {
    let live = note_dir.join("segments.jsonl");
    let backup = note_dir.join(SEGMENTS_BACKUP_FILE);
    if live.exists() && !backup.exists() {
        std::fs::copy(&live, &backup)?;
    }
    // tmp+rename(与 notes.rs write_jsonl_atomic 同哲学;那边收 JsonlLine 私有类型,
    // 这里整表全新生成、无损坏行保留需求,不共用)。
    let tmp = note_dir.join("segments.jsonl.tmp");
    let mut out = String::new();
    for s in segs {
        out.push_str(&serde_json::to_string(s)?);
        out.push('\n');
    }
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, &live)?;
    crate::store::write_speakers_atomic(note_dir, speakers)?;
    // seq 全变:抑制表按旧 seq 隐藏会误伤新段,整个清掉;align.json 是旧 mic 时间戳的
    // 展示侧纠正,新段时间戳直接来自离线切段,旧映射不再适用。删除失败不致命(仅
    // 影响展示),不挡提交。
    let _ = std::fs::remove_file(note_dir.join(crate::store::SEGMENT_SUPPRESSIONS_FILE));
    let _ = std::fs::remove_file(note_dir.join(crate::store::align::ALIGN_FILE));
    // aing 标失效:revision 显式进位(与 run_local 同一套语义——过期编辑器会话的
    // 保存必因 revision 不匹配而冲突,不留 rev 窗口)。
    if let Some(mut doc) = crate::store::refined::load_refined_locked(note_dir, lock) {
        doc.stale = true;
        doc.revision = doc.revision.saturating_add(1);
        crate::store::refined::write_refined_atomic_locked(note_dir, &doc, lock)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::notelock::NoteLock;
    use crate::store::{SegmentRecord, SpeakerMeta};
    use std::collections::BTreeMap;

    fn seg(seq: u64, text: &str) -> SegmentRecord {
        SegmentRecord {
            seq, source: "mic".into(), text: text.into(), start_ms: 0, end_ms: 1000,
            speaker: Some("S1".into()), rms: Some(0.1),
        }
    }

    fn setup() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("segments.jsonl"),
            "{\"seq\":1,\"source\":\"mic\",\"text\":\"旧文本\",\"start_ms\":0,\"end_ms\":500,\"speaker\":null}\n").unwrap();
        std::fs::write(dir.path().join("segment-suppressions.jsonl"), "{\"seq\":1,\"reason\":\"echo\"}\n").unwrap();
        std::fs::write(dir.path().join("align.json"), "{}").unwrap();
        dir
    }

    /// 首次提交:备份旧稿、原子写新段与说话人表、清抑制表、删 align。
    #[test]
    fn commit_backs_up_then_replaces_everything() {
        let dir = setup();
        let lock = NoteLock::acquire(dir.path()).unwrap().unwrap();
        let speakers = BTreeMap::from([("S1".to_string(), SpeakerMeta {
            name: "张三".into(), sources: vec!["mic".into()], centroid: None, count: 0, person_id: None,
        })]);
        commit(dir.path(), &lock, &[seg(1, "新文本")], &speakers).unwrap();
        let backup = std::fs::read_to_string(dir.path().join(SEGMENTS_BACKUP_FILE)).unwrap();
        assert!(backup.contains("旧文本"), "备份应是重转写前的原稿");
        let new = std::fs::read_to_string(dir.path().join("segments.jsonl")).unwrap();
        assert!(new.contains("新文本") && !new.contains("旧文本"));
        assert!(std::fs::read_to_string(dir.path().join("speakers.json")).unwrap().contains("张三"));
        assert!(!dir.path().join("segment-suppressions.jsonl").exists(), "seq 全变,抑制表必须清");
        assert!(!dir.path().join("align.json").exists(), "旧时基映射不再适用");
    }

    /// 二次提交:备份不被覆盖——保的是「最初的原始转写」,不是上一轮重转写结果。
    #[test]
    fn backup_is_write_once() {
        let dir = setup();
        let lock = NoteLock::acquire(dir.path()).unwrap().unwrap();
        commit(dir.path(), &lock, &[seg(1, "第一轮")], &BTreeMap::new()).unwrap();
        commit(dir.path(), &lock, &[seg(1, "第二轮")], &BTreeMap::new()).unwrap();
        let backup = std::fs::read_to_string(dir.path().join(SEGMENTS_BACKUP_FILE)).unwrap();
        assert!(backup.contains("旧文本"), "备份恒为最初原稿");
        assert!(std::fs::read_to_string(dir.path().join("segments.jsonl")).unwrap().contains("第二轮"));
    }

    /// 有 aing.json 时:标 stale + revision 进位(过期编辑器会话保存必冲突);无则不创建。
    #[test]
    fn aing_doc_marked_stale_with_revision_bump() {
        let dir = setup();
        // 用生产写入口造一份合法 aing.json(revision=3)
        let lock = NoteLock::acquire(dir.path()).unwrap().unwrap();
        let mut doc = crate::store::refined::RefinedDoc {
            schema_version: crate::store::refined::REFINED_SCHEMA_VERSION,
            generated_at: "t".into(), llm_model: None,
            stages: crate::store::refined::RefineStages {
                filter: "done".into(), recluster: "done".into(), llm: "off".into(),
                entities: "off".into(), relations: "off".into(),
            },
            discarded_seqs: vec![], entities: vec![], graph_extraction: None,
            relations: vec![], graph_support_mentions: vec![], revision: 3,
            stale: false, paragraphs: vec![],
        };
        crate::store::refined::write_refined_atomic_locked(dir.path(), &doc, &lock).unwrap();
        commit(dir.path(), &lock, &[seg(1, "新")], &BTreeMap::new()).unwrap();
        doc = crate::store::refined::load_refined_locked(dir.path(), &lock).unwrap();
        assert!(doc.stale);
        assert!(doc.revision >= 4, "revision 必须进位: {}", doc.revision);

        let dir2 = tempfile::tempdir().unwrap();
        std::fs::write(dir2.path().join("segments.jsonl"), "").unwrap();
        let lock2 = NoteLock::acquire(dir2.path()).unwrap().unwrap();
        commit(dir2.path(), &lock2, &[seg(1, "新")], &BTreeMap::new()).unwrap();
        assert!(crate::store::refined::load_refined_locked(dir2.path(), &lock2).is_none(),
            "从未 Aing 过的笔记不该被 commit 凭空造出 aing.json");
    }
}
