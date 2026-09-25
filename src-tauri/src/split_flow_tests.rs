//! 拆分状态机的整链测试:假 [`SplitEnv`] + 临时目录里的真实笔记与声纹库。
//!
//! 这些用例钉住的是**现有行为**(搬迁前的特征测试):正常一键拆分、一组不硬拆、撤销、
//! 各阶段中断后重入续跑、取消的阶段门槛、重叠处置共享人物时的隔离交接、残留选择不可
//! 改选、重建单飞被占时的收尾。任何一条变红,说明拆分语义被改了。

use super::*;
use crate::diar::SpeakerEmbedder;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;

const SEG_MS: u64 = 5_000;
/// 两个人的「声音」:段内恒定振幅,假嵌入器按振幅区分身份。
const AMP_A: i16 = 3_000;
const AMP_B: i16 = 15_000;

/// 按样本幅度给出两个正交方向:振幅低 = A,高 = B。
struct AmplitudeEmbedder;
impl SpeakerEmbedder for AmplitudeEmbedder {
    fn embed(&mut self, samples: &[f32]) -> anyhow::Result<Vec<f32>> {
        let amp = samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        Ok(if amp < 0.2 {
            vec![1.0, 0.0, 0.0, 0.0]
        } else {
            vec![0.0, 1.0, 0.0, 0.0]
        })
    }
}

#[derive(Default)]
struct FakeEnv {
    root: PathBuf,
    /// 已执行的笔记编辑(按类型记名)。
    edits: StdMutex<Vec<String>>,
    /// 下一次该类型的编辑失败一次(模拟崩溃/锁拒绝打断在该步)。
    fail_edit_once: StdMutex<Option<&'static str>>,
    fail_done_once: AtomicBool,
    rebuild_running: AtomicBool,
    consumed_pending: AtomicUsize,
    rebuild_requests: AtomicUsize,
    done_calls: StdMutex<Vec<bool>>,
}

impl FakeEnv {
    fn new(root: &std::path::Path) -> Self {
        Self {
            root: root.to_path_buf(),
            ..Default::default()
        }
    }
    fn nstore(&self) -> store::NoteStore {
        store::NoteStore::new(self.root.join("notes"))
    }
    fn fail_next(&self, kind: &'static str) {
        *self.fail_edit_once.lock().unwrap() = Some(kind);
    }
}

impl SplitEnv for FakeEnv {
    fn root(&self) -> anyhow::Result<PathBuf> {
        Ok(self.root.clone())
    }
    fn notes_dir(&self) -> anyhow::Result<PathBuf> {
        Ok(self.root.join("notes"))
    }
    fn edit_note(&self, op: lifecycle::machine::EditOp) -> Result<(), String> {
        use lifecycle::machine::EditOp as E;
        let kind = match &op {
            E::SetMultiSpeaker { .. } => "SetMultiSpeaker",
            E::ReserveSpeakers { .. } => "ReserveSpeakers",
            E::ReleaseReservedSpeakers { .. } => "ReleaseReservedSpeakers",
            E::AssignPersonIf { .. } => "AssignPersonIf",
            E::SplitReassign { .. } => "SplitReassign",
            other => panic!("拆分流程不该发出 {other:?}"),
        };
        {
            let mut f = self.fail_edit_once.lock().unwrap();
            if *f == Some(kind) {
                *f = None;
                return Err(format!("注入失败: {kind}"));
            }
        }
        self.edits.lock().unwrap().push(kind.to_string());
        let s = self.nstore();
        let r = match op {
            E::SetMultiSpeaker { id, speaker_id } => s.set_multi_speaker(&id, &speaker_id),
            E::ReserveSpeakers {
                id,
                speaker_ids,
                op_id,
            } => s.reserve_speakers(&id, &speaker_ids, &op_id),
            E::ReleaseReservedSpeakers { id, op_id } => {
                s.release_reserved_speakers(&id, &op_id).map(|_| ())
            }
            E::AssignPersonIf {
                id,
                speaker_id,
                person_id,
            } => s.assign_speaker_person_if(&id, &speaker_id, &person_id),
            E::SplitReassign { id, moves, op_id } => {
                s.batch_set_segment_speaker(&id, &moves, &op_id)
            }
            _ => unreachable!(),
        };
        r.map_err(|e| e.to_string())
    }
    fn admit(&self, _note_id: &str, _intent: occupancy::Intent) -> Result<(), String> {
        Ok(())
    }
    fn open_embedder(&self) -> anyhow::Result<diar::TaggedEmbedder> {
        let tag = store::VoiceprintStore::new(self.root.clone())
            .load()
            .embedding_model;
        Ok(diar::TaggedEmbedder::new(tag, Box::new(AmplitudeEmbedder)))
    }
    fn seeds_for(&self, _tag: &str) -> Vec<diar::registry::SeedCluster> {
        Vec::new()
    }
    fn begin_exclusive_rebuild(&self) -> bool {
        !self.rebuild_running.swap(true, Ordering::SeqCst)
    }
    fn end_exclusive_rebuild(&self) {
        self.rebuild_running.store(false, Ordering::SeqCst);
    }
    fn consume_pending_rebuild(&self) {
        self.consumed_pending.fetch_add(1, Ordering::SeqCst);
    }
    fn request_rebuild(&self, _reason: &'static str) {
        self.rebuild_requests.fetch_add(1, Ordering::SeqCst);
    }
    fn on_split_done(&self, _root: &std::path::Path, split_commit: bool) -> Result<(), String> {
        if self.fail_done_once.swap(false, Ordering::SeqCst) {
            return Err("注入失败: on_split_done".into());
        }
        self.done_calls.lock().unwrap().push(split_commit);
        Ok(())
    }
    fn split_progress(&self, _note_id: &str, _done: usize, _total: usize) {}
}

// ── 夹具 ──

/// 建一篇笔记:`voices` 逐段给出振幅(决定假嵌入器认成谁),全部归在 `speakers[i]` 名下。
/// 同时写 mic.wav,让分组嵌入真的从音频里切片。
fn make_note(root: &std::path::Path, segs: &[(i16, &str)]) -> String {
    let notes = root.join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    let now = chrono::Local::now();
    let mut w = store::writer::NoteWriter::create(&notes, now).unwrap();
    let id = w.note_id().to_string();
    for (i, (_, sp)) in segs.iter().enumerate() {
        let s = i as u64 * SEG_MS;
        w.append_final("mic", &format!("第{i}段"), s, s + SEG_MS, Some(sp), None)
            .unwrap();
    }
    let mut known: Vec<String> = segs.iter().map(|(_, sp)| sp.to_string()).collect();
    known.sort();
    known.dedup();
    let pairs: Vec<(String, Vec<String>)> =
        known.into_iter().map(|k| (k, vec!["mic".into()])).collect();
    w.sync_speakers(&pairs).unwrap();
    w.finalize(now).unwrap();
    drop(w);
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav = hound::WavWriter::create(notes.join(&id).join("mic.wav"), spec).unwrap();
    for (amp, _) in segs {
        for _ in 0..(SEG_MS * 16) {
            wav.write_sample(*amp).unwrap();
        }
    }
    wav.finalize().unwrap();
    id
}

/// 两个人混在 S1 名下,交替发言,各 8 段 × 5s(每组 40s,过得了碎片门槛)。
fn mixed_note(root: &std::path::Path) -> String {
    let segs: Vec<(i16, &str)> = (0..16)
        .map(|i| (if i % 2 == 0 { AMP_A } else { AMP_B }, "S1"))
        .collect();
    make_note(root, &segs)
}

/// 声纹库里建一个人物(直接写 voiceprints.json,不走已退役的自动入库)。
fn seed_person(root: &std::path::Path, pid: &str, name: &str) {
    let path = root.join("voiceprints.json");
    let mut v: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(
            || serde_json::json!({ "schema_version": 1, "next_person": 1, "people": {} }),
        );
    v["people"][pid] = serde_json::json!({ "name": name, "total_ms": 0, "last_seen": "" });
    v["next_person"] = serde_json::json!(99);
    std::fs::write(&path, v.to_string()).unwrap();
}

fn quarantined(root: &std::path::Path, pid: &str) -> bool {
    store::VoiceprintStore::new(root.to_path_buf())
        .load()
        .people[pid]
        .voiceprint_quarantined
}

fn op_of(root: &std::path::Path, op_id: &str) -> store::split_ops::SplitOp {
    store::split_ops::load(root, op_id).unwrap()
}

fn speakers_of_segments(env: &FakeEnv, id: &str) -> Vec<String> {
    env.nstore()
        .load(id)
        .unwrap()
        .segments
        .iter()
        .map(|s| s.speaker.clone().unwrap_or_default())
        .collect()
}

use store::split_ops::phase as ph;

// ── 用例 ──

#[test]
fn auto_split_happy_path_reassigns_by_voice_and_closes_the_op() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    let id = mixed_note(tmp.path());

    let out = auto_split_speaker_with(&env, id.clone(), "S1".into()).unwrap();
    assert!(out.split);
    assert_eq!(out.groups.len(), 2, "两个声音 → 两组");
    assert_eq!(out.kept, 0);

    let op = op_of(tmp.path(), &out.op_id);
    assert_eq!(op.phase, ph::DONE);
    assert_eq!(op.mode, "split_commit");
    // 段落按声音分进两个新说话人,同一个声音必须落在同一个号上。
    let sp = speakers_of_segments(&env, &id);
    let (a, b): (Vec<_>, Vec<_>) = sp.iter().enumerate().partition(|(i, _)| i % 2 == 0);
    let a: std::collections::BTreeSet<_> = a.into_iter().map(|(_, s)| s.clone()).collect();
    let b: std::collections::BTreeSet<_> = b.into_iter().map(|(_, s)| s.clone()).collect();
    assert_eq!(a.len(), 1, "A 的段落应全部落在同一个号: {sp:?}");
    assert_eq!(b.len(), 1, "B 的段落应全部落在同一个号: {sp:?}");
    assert_ne!(a, b);
    let note = env.nstore().load(&id).unwrap();
    for sid in a.iter().chain(b.iter()) {
        let m = &note.speakers[sid];
        assert!(m.split_born, "新号带 split_born");
        assert!(m.reserved_by.is_none(), "改派后占号所有权清掉");
    }
    assert_eq!(
        *env.done_calls.lock().unwrap(),
        vec![true],
        "收尾钩子恰好一次,且是拆分模式"
    );
}

#[test]
fn single_voice_is_not_force_split_and_everything_is_restored() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_person(tmp.path(), "P1", "王虎");
    let segs: Vec<(i16, &str)> = (0..16).map(|_| (AMP_A, "S1")).collect();
    let id = make_note(tmp.path(), &segs);
    env.nstore().assign_speaker_person(&id, "S1", "P1").unwrap();

    let out = auto_split_speaker_with(&env, id.clone(), "S1".into()).unwrap();
    assert!(!out.split, "只有一个声音不硬拆");
    assert_eq!(op_of(tmp.path(), &out.op_id).phase, ph::CANCELLED);
    let note = env.nstore().load(&id).unwrap();
    let s1 = &note.speakers["S1"];
    assert!(!s1.multi_speaker, "多人标记复位");
    assert_eq!(s1.person_id.as_deref(), Some("P1"), "打标前的关联恢复");
    assert!(!quarantined(tmp.path(), "P1"), "隔离解除");
    assert!(
        speakers_of_segments(&env, &id).iter().all(|s| s == "S1"),
        "段落一段没动"
    );
}

#[test]
fn undo_puts_everything_back_once() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    let id = mixed_note(tmp.path());
    let out = auto_split_speaker_with(&env, id.clone(), "S1".into()).unwrap();
    let created: Vec<String> = out.groups.iter().map(|g| g.speaker_id.clone()).collect();

    undo_auto_split_with(&env, out.op_id.clone()).unwrap();
    assert!(
        speakers_of_segments(&env, &id).iter().all(|s| s == "S1"),
        "段落全部搬回"
    );
    let note = env.nstore().load(&id).unwrap();
    for sid in &created {
        assert!(
            !note.speakers.contains_key(sid),
            "拆出来的空说话人 {sid} 删掉"
        );
    }
    assert!(!note.speakers["S1"].multi_speaker);
    assert!(op_of(tmp.path(), &out.op_id).undone_at.is_some());
    assert!(
        undo_auto_split_with(&env, out.op_id).is_err(),
        "撤销只能一次"
    );
}

/// 中断在「批量改派」:阶段停在 reserved,预留号已建;重跑必须续完,且不换新号。
#[test]
fn interrupted_at_reassign_resumes_without_new_numbers() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    let id = mixed_note(tmp.path());
    env.fail_next("SplitReassign");
    assert!(auto_split_speaker_with(&env, id.clone(), "S1".into()).is_err());
    let op_id = store::split_ops::open_ops_for_note(tmp.path(), &id)[0]
        .op_id
        .clone();
    let op = op_of(tmp.path(), &op_id);
    assert_eq!(op.phase, ph::RESERVED, "停在占号之后");
    let planned: Vec<String> = op
        .plan_groups
        .iter()
        .filter_map(|g| g.dest_speaker.clone())
        .collect();
    assert!(
        speakers_of_segments(&env, &id).iter().all(|s| s == "S1"),
        "段落还没动"
    );

    let out = auto_split_speaker_with(&env, id.clone(), "S1".into()).unwrap();
    assert_eq!(out.op_id, op_id, "续跑同一个 op,不另起");
    assert_eq!(op_of(tmp.path(), &op_id).phase, ph::DONE);
    let mut used: Vec<String> = speakers_of_segments(&env, &id);
    used.sort();
    used.dedup();
    let mut planned_sorted = planned.clone();
    planned_sorted.sort();
    assert_eq!(used, planned_sorted, "落地的号就是计划里的号");
}

/// 中断在收尾钩子(DONE 之前):停在 released,此时不能取消;重跑补完 done 且钩子再调一次。
#[test]
fn interrupted_at_done_hook_cannot_cancel_and_resumes() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    let id = mixed_note(tmp.path());
    env.fail_done_once.store(true, Ordering::SeqCst);
    assert!(auto_split_speaker_with(&env, id.clone(), "S1".into()).is_err());
    let op_id = store::split_ops::open_ops_for_note(tmp.path(), &id)[0]
        .op_id
        .clone();
    assert_eq!(op_of(tmp.path(), &op_id).phase, ph::RELEASED);
    assert!(
        cancel_split_with(&env, op_id.clone()).is_err(),
        "段落已改派,只能前滚"
    );

    auto_split_speaker_with(&env, id.clone(), "S1".into()).unwrap();
    assert_eq!(op_of(tmp.path(), &op_id).phase, ph::DONE);
    assert_eq!(*env.done_calls.lock().unwrap(), vec![true]);
}

/// 中断在占号:阶段停在 residual_decided,可以取消;取消清掉孤儿预留、解除隔离。
#[test]
fn cancel_before_reassign_cleans_reservations_and_quarantine() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_person(tmp.path(), "P1", "王虎");
    let id = mixed_note(tmp.path());
    env.nstore().assign_speaker_person(&id, "S1", "P1").unwrap();
    env.fail_next("ReserveSpeakers");
    assert!(auto_split_speaker_with(&env, id.clone(), "S1".into()).is_err());
    let op_id = store::split_ops::open_ops_for_note(tmp.path(), &id)[0]
        .op_id
        .clone();
    assert_eq!(op_of(tmp.path(), &op_id).phase, ph::RESIDUAL_DECIDED);
    assert!(quarantined(tmp.path(), "P1"), "处置中:人物隔离");

    cancel_split_with(&env, op_id.clone()).unwrap();
    assert_eq!(op_of(tmp.path(), &op_id).phase, ph::CANCELLED);
    assert!(!quarantined(tmp.path(), "P1"), "取消即解除隔离");
    let note = env.nstore().load(&id).unwrap();
    assert!(
        note.speakers.values().all(|m| m.reserved_by.is_none()),
        "没有遗留的预留号"
    );
    assert!(
        env.consumed_pending.load(Ordering::SeqCst) >= 1,
        "取消后消化排队的重建"
    );
}

/// 两个处置共享同一个人:先收尾的那个不能把还被另一个持有的人放出来。
#[test]
fn shared_person_stays_quarantined_until_the_last_holder_finishes() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_person(tmp.path(), "P1", "王虎");
    let segs: Vec<(i16, &str)> = (0..8)
        .map(|i| (AMP_A, if i < 4 { "S1" } else { "S2" }))
        .collect();
    let id = make_note(tmp.path(), &segs);
    env.nstore().assign_speaker_person(&id, "S1", "P1").unwrap();
    env.nstore().assign_speaker_person(&id, "S2", "P1").unwrap();

    let op1 = mark_speaker_multi_with(&env, id.clone(), vec!["S1".into()]).unwrap();
    let op2 = mark_speaker_multi_with(&env, id.clone(), vec!["S2".into()]).unwrap();
    assert!(quarantined(tmp.path(), "P1"));
    for op in [&op1, &op2] {
        confirm_multi_samples_with(&env, op.clone(), Vec::new(), true).unwrap();
    }
    resolve_multi_residual_with(&env, op1.clone(), "accept".into(), false).unwrap();
    assert_eq!(op_of(tmp.path(), &op1).phase, ph::DONE);
    assert!(quarantined(tmp.path(), "P1"), "op2 还持有,不能放");
    resolve_multi_residual_with(&env, op2.clone(), "accept".into(), false).unwrap();
    assert!(!quarantined(tmp.path(), "P1"), "最后一个持有者收尾才解除");
    assert_eq!(
        *env.done_calls.lock().unwrap(),
        vec![false, false],
        "仅隔离模式,不排图谱"
    );
}

#[test]
fn residual_choice_cannot_change_on_reentry() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    let id = mixed_note(tmp.path());
    let op = mark_speaker_multi_with(&env, id, vec!["S1".into()]).unwrap();
    assert!(
        resolve_multi_residual_with(&env, op.clone(), "accept".into(), true).is_err(),
        "样本没处置之前不能选残留"
    );
    confirm_multi_samples_with(&env, op.clone(), Vec::new(), true).unwrap();
    resolve_multi_residual_with(&env, op.clone(), "accept".into(), true).unwrap();
    assert_eq!(
        op_of(tmp.path(), &op).phase,
        ph::RESIDUAL_DECIDED,
        "拆分模式停在这里等提交"
    );
    assert!(
        resolve_multi_residual_with(&env, op.clone(), "baseline".into(), true).is_err(),
        "不能改选"
    );
    assert!(
        confirm_multi_samples_with(&env, op, vec!["x".into()], true).is_err(),
        "残留已选,样本集合冻结"
    );
}

/// baseline 抢不到重建单飞:报错,且排队的重建有人消化;阶段不前进。
#[test]
fn baseline_while_rebuild_running_fails_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    let id = mixed_note(tmp.path());
    let op = mark_speaker_multi_with(&env, id, vec!["S1".into()]).unwrap();
    confirm_multi_samples_with(&env, op.clone(), Vec::new(), true).unwrap();
    env.rebuild_running.store(true, Ordering::SeqCst);
    assert!(resolve_multi_residual_with(&env, op.clone(), "baseline".into(), false).is_err());
    assert_eq!(env.consumed_pending.load(Ordering::SeqCst), 1);
    assert_eq!(op_of(tmp.path(), &op).phase, ph::SAMPLES_HANDLED);
    env.rebuild_running.store(false, Ordering::SeqCst);
    resolve_multi_residual_with(&env, op.clone(), "baseline".into(), false).unwrap();
    assert_eq!(op_of(tmp.path(), &op).phase, ph::DONE);
}

#[test]
fn marking_twice_reuses_the_plan_op_and_rejects_unknown_speakers() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    let id = mixed_note(tmp.path());
    assert!(mark_speaker_multi_with(&env, id.clone(), vec!["S9".into()]).is_err());
    assert!(mark_speaker_multi_with(&env, id.clone(), vec![]).is_err());
    env.fail_next("SetMultiSpeaker");
    assert!(mark_speaker_multi_with(&env, id.clone(), vec!["S1".into()]).is_err());
    let ops = store::split_ops::open_ops_for_note(tmp.path(), &id);
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].phase, ph::PLAN, "卡在打标前半程");
    let again = mark_speaker_multi_with(&env, id.clone(), vec!["S1".into()]).unwrap();
    assert_eq!(again, ops[0].op_id, "复用卡住的 plan,不新建");
    assert_eq!(
        store::split_ops::open_ops_for_note(tmp.path(), &id).len(),
        1
    );
}
