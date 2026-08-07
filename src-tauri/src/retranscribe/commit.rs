//! 重转写的提交侧。破坏性覆盖三重保险(spec §提交与安全网):一次性备份、
//! 调用方提交门(本文件不管)、tmp+rename 原子切换。全部操作要求调用方持有
//! NoteLock——签名收 &NoteLock 是类型层面的持锁证明(照 write_refined_atomic_locked)。

use crate::store::notelock::NoteLock;
use crate::store::{SegmentRecord, SpeakerMeta};
use std::collections::BTreeMap;
use std::path::Path;

/// 首次重转写前的原稿备份。write-once:保「最初的原始转写」,不随后续轮次滚动。
pub const SEGMENTS_BACKUP_FILE: &str = "segments.orig.jsonl";

/// 提交 segments/speakers 的原子替换,并使 aing 结果失效。
///
/// 失败矩阵(Fix 2:可失败步骤全部移到 rename 之前):任何 `Err` 返回 ⇒
/// `segments.jsonl` 一字未动(旧内容原样在盘)。可能残留的副作用均良性且幂等,
/// 重跑本函数会收敛到一致状态:
///  - 备份可能已建(write-once,不影响正确性);
///  - aing 文档可能已提前标 stale + revision 进位(用户会看到「需要重新 Aing」的
///    横幅,旧 segments/speakers 仍完全一致,无数据损坏);
///  - speakers.json 可能已被写成"旧表 ∪ 新表"的并集超集(比调用方传入的精确表多
///    出一些孤儿条目,本仓已明文接受孤儿说话人无害——见 notes.rs delete_segment
///    注释;旧 segments 引用的所有说话人 id 仍在这份并集里,不会渲染出缺失引用)。
/// rename 之后只剩清抑制表/删 align(容忍失败)与 speakers 精确表收尾写(尽力而为,
/// 失败只 eprintln 不返回 Err)——这些步骤即便部分失败,live segments 也已经是
/// 新内容且自洽,不会污染到旧提交。
pub fn commit(
    note_dir: &Path,
    lock: &NoteLock,
    segs: &[SegmentRecord],
    speakers: &BTreeMap<String, SpeakerMeta>,
) -> anyhow::Result<()> {
    let live = note_dir.join("segments.jsonl");
    let backup = note_dir.join(SEGMENTS_BACKUP_FILE);

    // 1) 备份(write-once)。失败 → Err,盘上什么都还没动。
    if live.exists() && !backup.exists() {
        std::fs::copy(&live, &backup)?;
    }

    // 2) aing 标 stale + revision 进位,原样逻辑但移到最前(与 run_local 同一套
    // 语义——过期编辑器会话的保存必因 revision 不匹配而冲突,不留 rev 窗口)。
    // 失败 → Err,segments/speakers 都还没动,盘上只可能多一个备份。成功但后续
    // 步骤失败:aing 已提前标 stale,良性——横幅提示重新 Aing,旧文件仍一致。
    if let Some(mut doc) = crate::store::refined::load_refined_locked(note_dir, lock) {
        doc.stale = true;
        doc.revision = doc.revision.saturating_add(1);
        crate::store::refined::write_refined_atomic_locked(note_dir, &doc, lock)?;
    }

    // 3) speakers 预写:旧表 ∪ 新表,冲突键取新值。旧表读法照 read_old_speakers 的
    // 容忍式(缺失/损坏按空表处理,不因说话人表读取失败挡住提交)。并集保留旧 id
    // 项 → 本步成功而 rename(步骤 4)失败时,live 仍是旧 segments,它们引用的旧
    // 说话人 id 全部还在,照样可渲染;冲突取新值 → rename 成功而收尾精确写(步骤 5)
    // 失败时,新 segments 引用的说话人 id 也全部正确,只多出孤儿旧条目——无害。
    let mut union_speakers = super::read_old_speakers(note_dir);
    for (id, meta) in speakers {
        union_speakers.insert(id.clone(), meta.clone());
    }
    crate::store::write_speakers_atomic(note_dir, &union_speakers)?;

    // 4) segments tmp 写 + rename——不可回退点。失败 → Err,live 未动(rename 是
    // 同一文件系统上的原子操作,要么整体替换要么完全不变;tmp+rename 与 notes.rs
    // write_jsonl_atomic 同哲学,那边收 JsonlLine 私有类型、这里整表全新生成、
    // 无损坏行保留需求,不共用实现)。
    let tmp = note_dir.join("segments.jsonl.tmp");
    let mut out = String::new();
    for s in segs {
        out.push_str(&serde_json::to_string(s)?);
        out.push('\n');
    }
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, &live)?;

    // 5) rename 之后只剩不可失败/容忍步骤。seq 全变:抑制表按旧 seq 隐藏会误伤
    // 新段,整个清掉;align.json 是旧 mic 时间戳的展示侧纠正,新段时间戳直接来自
    // 离线切段,旧映射不再适用。删除失败不致命(仅影响展示),不挡提交。
    let _ = std::fs::remove_file(note_dir.join(crate::store::SEGMENT_SUPPRESSIONS_FILE));
    let _ = std::fs::remove_file(note_dir.join(crate::store::align::ALIGN_FILE));
    // speakers 精确表收尾写:把步骤 3 的并集超集收窄成调用方传入的精确表(去掉
    // 孤儿旧条目)。失败不返回 Err——此时 segments.jsonl 已经是新内容且已提交
    // 成功,盘面只是"精确表未来得及收窄成并集超集"这一良性状态,重跑本函数会
    // 自然收敛(下次提交仍会重算并集并再次尝试精确写)。
    if let Err(e) = crate::store::write_speakers_atomic(note_dir, speakers) {
        eprintln!("重转写提交:说话人表精确收尾写失败(已落盘并集超集,无害): {e}");
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

    /// Fix 2 故障注入:speakers 预写(步骤 3)失败——commit 必须整体 Err,且此时
    /// segments.jsonl(步骤 4,晚于步骤 3)必然一字未动,与提交前逐字节相等。
    /// 在旧顺序(segments rename 先于 speakers 写)下,这个用例会 RED:旧代码先替换
    /// segments 再写 speakers,speakers 写失败时 segments 早已经是新内容,
    /// 逐字节相等断言会失败——这正是 Fix 2 要堵的"部分提交"窗口。
    /// 备份是否已建不作断言(write-once 语义下允许已建,是良性副作用)。
    #[test]
    fn speakers_prewrite_failure_leaves_segments_untouched() {
        let dir = setup();
        let lock = NoteLock::acquire(dir.path()).unwrap().unwrap();
        let before = std::fs::read(dir.path().join("segments.jsonl")).unwrap();

        // 让 speakers.json.tmp 路径已被一个目录占用——write_speakers_atomic 内部
        // std::fs::write(&tmp, ...) 对已存在的目录路径必然失败。
        std::fs::create_dir(dir.path().join("speakers.json.tmp")).unwrap();

        let speakers = BTreeMap::from([("S1".to_string(), SpeakerMeta {
            name: "李四".into(), sources: vec!["mic".into()], centroid: None, count: 0, person_id: None,
        })]);
        let result = commit(dir.path(), &lock, &[seg(1, "新文本")], &speakers);
        assert!(result.is_err(), "speakers 预写失败必须整体 Err");

        let after = std::fs::read(dir.path().join("segments.jsonl")).unwrap();
        assert_eq!(before, after, "speakers 预写失败时 segments.jsonl 必须逐字节未动");
    }

    /// Fix 2 故障注入:segments rename(步骤 4,不可回退点)失败——commit 必须
    /// 整体 Err。fixture 用"把 segments.jsonl 路径换成目录"逼真实 rename 失败
    /// (先删文件再 create_dir);这是反常 fixture,只用来锁死 Err 传播这一路径,
    /// 不代表真实场景会出现目录冲突。预先造好备份文件,跳过步骤 1 的
    /// `std::fs::copy`——否则 copy 会先撞上"源是目录"而提前失败,真正想复现的
    /// rename 失败就走不到。
    #[test]
    fn segments_rename_failure_propagates_err() {
        let dir = setup();
        let lock = NoteLock::acquire(dir.path()).unwrap().unwrap();
        std::fs::copy(dir.path().join("segments.jsonl"), dir.path().join(SEGMENTS_BACKUP_FILE)).unwrap();

        std::fs::remove_file(dir.path().join("segments.jsonl")).unwrap();
        std::fs::create_dir(dir.path().join("segments.jsonl")).unwrap();

        let result = commit(dir.path(), &lock, &[seg(1, "新文本")], &BTreeMap::new());
        assert!(result.is_err(), "rename 目标被目录占位时必须 Err(锁死失败传播)");
    }
}
