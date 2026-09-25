//! 识别建议动作的整链测试:假 [`LinkEnv`] + 临时目录里真实的笔记、修订稿、identify.json
//! 与声纹库。后台工作先排队、由用例决定何时跑。
//!
//! 其中三条钉住 #141 之后的回归(判「这簇关联着谁」读了不再带身份的段落):回执可撤销、
//! 已手动关联的说话人不再出建议、崩溃恢复认得已落地的关联。

use super::*;
use crate::diar::{SpeakerEmbedder, TaggedEmbedder};
use crate::lifecycle;
use crate::refine::identify::{self as idf, IdentifyAssignment, IdentifyDoc, Tier};
use crate::voice_env::VoiceEnv;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex as StdMutex;

const SEG_MS: u64 = 6_000;

struct ConstEmbedder;
impl SpeakerEmbedder for ConstEmbedder {
    fn embed(&mut self, _samples: &[f32]) -> anyhow::Result<Vec<f32>> {
        Ok(vec![1.0, 0.0, 0.0, 0.0])
    }
}

#[derive(Default)]
struct FakeEnv {
    root: PathBuf,
    tasks: StdMutex<Vec<speaker_link::LinkTask>>,
    fail_edit_once: StdMutex<Option<&'static str>>,
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
    fn vstore(&self) -> store::VoiceprintStore {
        store::VoiceprintStore::new(self.root.clone())
    }
    fn run_tasks(&self) {
        loop {
            let batch = std::mem::take(&mut *self.tasks.lock().unwrap());
            if batch.is_empty() {
                return;
            }
            for t in batch {
                t(self);
            }
        }
    }
}

impl VoiceEnv for FakeEnv {
    fn root(&self) -> anyhow::Result<PathBuf> {
        Ok(self.root.clone())
    }
    fn notes_dir(&self) -> anyhow::Result<PathBuf> {
        Ok(self.root.join("notes"))
    }
    fn edit_note(&self, op: lifecycle::machine::EditOp) -> Result<(), String> {
        use lifecycle::machine::EditOp as E;
        let kind = match &op {
            E::AssignPerson { .. } => "AssignPerson",
            other => panic!("识别建议流程不该发出 {other:?}"),
        };
        {
            let mut f = self.fail_edit_once.lock().unwrap();
            if *f == Some(kind) {
                *f = None;
                return Err(format!("注入失败: {kind}"));
            }
        }
        match op {
            E::AssignPerson {
                id,
                speaker_id,
                person_id,
            } => self
                .nstore()
                .assign_speaker_person(&id, &speaker_id, &person_id)
                .map_err(|e| e.to_string()),
            _ => unreachable!(),
        }
    }
    fn admit(&self, _note_id: &str, _intent: occupancy::Intent) -> Result<(), String> {
        Ok(())
    }
    fn open_embedder(&self) -> anyhow::Result<TaggedEmbedder> {
        let tag = self.vstore().load().embedding_model;
        Ok(TaggedEmbedder::new(tag, Box::new(ConstEmbedder)))
    }
    fn request_rebuild(&self, _reason: &'static str) {}
}

impl LinkEnv for FakeEnv {
    fn spawn(&self, task: speaker_link::LinkTask) {
        self.tasks.lock().unwrap().push(task);
    }
    fn rebuild_person(&self, _person_id: &str) -> Result<(), String> {
        Ok(())
    }
}

// ── 夹具 ──

/// S1 = seq 0..4,S2 = seq 4..8,每段 6s;写 mic.wav、修订稿(一段一个说话人)。
fn make_note(root: &std::path::Path) -> String {
    let notes = root.join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    let now = chrono::Local::now();
    let mut w = store::writer::NoteWriter::create(&notes, now).unwrap();
    let id = w.note_id().to_string();
    for i in 0..8u64 {
        let sp = if i < 4 { "S1" } else { "S2" };
        w.append_final(
            "mic",
            &format!("第{i}段"),
            i * SEG_MS,
            (i + 1) * SEG_MS,
            Some(sp),
            None,
        )
        .unwrap();
    }
    w.sync_speakers(&[
        ("S1".into(), vec!["mic".into()]),
        ("S2".into(), vec!["mic".into()]),
    ])
    .unwrap();
    w.finalize(now).unwrap();
    drop(w);
    let dir = notes.join(&id);
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav = hound::WavWriter::create(dir.join("mic.wav"), spec).unwrap();
    for _ in 0..(8 * SEG_MS * 16) {
        wav.write_sample(4000i16).unwrap();
    }
    wav.finalize().unwrap();
    let doc: store::RefinedDoc = serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "generated_at": "t0",
        "stages": { "filter": "done", "recluster": "done", "llm": "done" },
        "paragraphs": [
            { "speaker": "S1", "start_ms": 0, "end_ms": 4 * SEG_MS, "text": "我是王虎", "source_seqs": [0, 1, 2, 3] },
            { "speaker": "S2", "start_ms": 4 * SEG_MS, "end_ms": 8 * SEG_MS, "text": "你好", "source_seqs": [4, 5, 6, 7] },
        ]
    }))
    .unwrap();
    store::refined::write_refined_atomic(&dir, &doc).unwrap();
    id
}

fn seed_people(root: &std::path::Path, people: &[(&str, &str)]) {
    let mut ps = serde_json::Map::new();
    for (pid, name) in people {
        ps.insert(
            pid.to_string(),
            serde_json::json!({ "name": name, "total_ms": 0, "last_seen": "" }),
        );
    }
    let v = serde_json::json!({ "schema_version": 1, "next_person": 99, "people": ps });
    std::fs::write(root.join("voiceprints.json"), v.to_string()).unwrap();
}

fn fp(seqs: &[u64]) -> String {
    idf::cluster_fingerprint(&seqs.iter().copied().collect::<BTreeSet<u64>>())
}
const S1: &[u64] = &[0, 1, 2, 3];
const S2: &[u64] = &[4, 5, 6, 7];

fn assignment(
    seqs: &[u64],
    person: Option<&str>,
    new_name: Option<&str>,
    tier: Tier,
) -> IdentifyAssignment {
    IdentifyAssignment {
        fingerprint: fp(seqs),
        cluster: String::new(),
        person_id: person.map(Into::into),
        new_name: new_name.map(Into::into),
        tier,
        llm_confidence: "high".into(),
        acoustic: person.map(|p| (p.to_string(), 0.9)),
        acoustic_z: None,
        evidence: vec![],
        status: "suggested".into(),
        decided_at: None,
    }
}

/// 写 identify.json(source_hash 与现稿一致,建议才算新鲜)。
fn write_identify(env: &FakeEnv, id: &str, assignments: Vec<IdentifyAssignment>) {
    let dir = env.root.join("notes").join(id);
    let doc = store::load_refined(&dir).unwrap();
    let idoc = IdentifyDoc {
        schema_version: idf::IDENTIFY_SCHEMA_VERSION,
        generated_at: "t1".into(),
        provider: "test".into(),
        model: "test".into(),
        revision: doc.revision,
        source_hash: store::source_hash(&doc.paragraphs),
        assignments,
        rejected: Default::default(),
    };
    idf::save_identify(&dir, &idoc).unwrap();
}

fn dir_of(env: &FakeEnv, id: &str) -> PathBuf {
    env.root.join("notes").join(id)
}
fn linked(env: &FakeEnv, id: &str, sp: &str) -> Option<String> {
    env.nstore().load(id).unwrap().speakers[sp]
        .person_id
        .clone()
}
fn status_of(env: &FakeEnv, id: &str, seqs: &[u64]) -> String {
    let f = fp(seqs);
    idf::load_identify(&dir_of(env, id))
        .unwrap()
        .assignments
        .iter()
        .find(|a| a.fingerprint == f)
        .unwrap()
        .status
        .clone()
}

/// 自动应用一条高置信建议(S1 = P1),返回 op_id。
fn auto_applied(env: &FakeEnv, id: &str) -> String {
    write_identify(env, id, vec![assignment(S1, Some("P1"), None, Tier::High)]);
    auto_apply_one(env, id, &fp(S1)).unwrap();
    idf::load_ops(&dir_of(env, id)).ops[0].op_id.clone()
}

// ── 自动应用与回执 ──

#[test]
fn auto_apply_links_in_the_note_but_never_writes_the_library() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎")]);
    let id = make_note(tmp.path());
    auto_applied(&env, &id);
    assert_eq!(linked(&env, &id, "S1").as_deref(), Some("P1"));
    assert_eq!(status_of(&env, &id, S1), "auto_applied");
    let op = &idf::load_ops(&dir_of(&env, &id)).ops[0];
    assert_eq!(op.stage, "done");
    assert!(op.reinforce_skipped.is_some(), "确认才入库:自动应用不写库");
    assert!(env.vstore().load().people["P1"].centroids.is_empty());
    assert!(env.vstore().sample_paths_existing("P1").is_empty());
}

#[test]
fn auto_apply_refuses_a_speaker_the_user_already_linked() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎"), ("P2", "李四")]);
    let id = make_note(tmp.path());
    env.nstore().assign_speaker_person(&id, "S1", "P2").unwrap();
    write_identify(
        &env,
        &id,
        vec![assignment(S1, Some("P1"), None, Tier::High)],
    );
    assert!(auto_apply_one(&env, &id, &fp(S1)).is_err());
    assert_eq!(linked(&env, &id, "S1").as_deref(), Some("P2"));
    assert!(
        idf::load_ops(&dir_of(&env, &id)).ops.is_empty(),
        "不落意向记录"
    );
}

/// 回归(#141 之后):回执的「撤销」按钮靠 revertible;此前读段落身份恒为 false。
#[test]
fn receipt_is_revertible_while_the_link_still_holds() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎"), ("P2", "李四")]);
    let id = make_note(tmp.path());
    auto_applied(&env, &id);
    let list = list_identify_suggestions_with(&env).unwrap();
    let r = list
        .iter()
        .find(|s| s.status == "auto_applied")
        .expect("有回执");
    assert!(r.revertible, "关联仍是自动目标:可撤销");
    env.nstore().assign_speaker_person(&id, "S1", "P2").unwrap();
    let list = list_identify_suggestions_with(&env).unwrap();
    assert!(
        !list
            .iter()
            .find(|s| s.status == "auto_applied")
            .unwrap()
            .revertible,
        "用户改过:冲突态"
    );
}

/// 回归(#141 之后):已手动关联的说话人不该再冒建议。
#[test]
fn suggestions_skip_speakers_the_user_already_linked() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎"), ("P2", "李四")]);
    let id = make_note(tmp.path());
    write_identify(
        &env,
        &id,
        vec![assignment(S2, Some("P2"), None, Tier::Medium)],
    );
    assert_eq!(list_identify_suggestions_with(&env).unwrap().len(), 1);
    env.nstore().assign_speaker_person(&id, "S2", "P1").unwrap();
    assert!(
        list_identify_suggestions_with(&env).unwrap().is_empty(),
        "用户已手动关联,不再打扰"
    );
}

#[test]
fn undo_clears_the_link_and_never_suggests_that_person_again() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎")]);
    let id = make_note(tmp.path());
    let op = auto_applied(&env, &id);
    assert!(
        undo_identify_apply_with(&env, id.clone(), op.clone()).unwrap(),
        "从未回灌:视为已还原"
    );
    assert_eq!(linked(&env, &id, "S1"), None);
    let dir = dir_of(&env, &id);
    assert_eq!(
        idf::load_ops(&dir).ops[0].undo_stage.as_deref(),
        Some("undone")
    );
    assert!(idf::load_identify(&dir)
        .unwrap()
        .rejected
        .contains_key(&idf::rejected_key(&fp(S1), "P1")));
    assert!(
        undo_identify_apply_with(&env, id, op).unwrap(),
        "重复撤销幂等(界面连点不报错)"
    );
}

#[test]
fn undo_refuses_when_the_user_already_relinked() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎"), ("P2", "李四")]);
    let id = make_note(tmp.path());
    let op = auto_applied(&env, &id);
    env.nstore().assign_speaker_person(&id, "S1", "P2").unwrap();
    assert!(undo_identify_apply_with(&env, id.clone(), op).is_err());
    assert_eq!(
        linked(&env, &id, "S1").as_deref(),
        Some("P2"),
        "不覆盖用户的改动"
    );
    assert_eq!(
        idf::load_ops(&dir_of(&env, &id)).ops[0].undo_stage,
        None,
        "回退到可撤状态"
    );
}

#[test]
fn acknowledging_feeds_the_library_once_confirmed() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎")]);
    let id = make_note(tmp.path());
    let op = auto_applied(&env, &id);
    acknowledge_identify_with(&env, id.clone(), op.clone()).unwrap();
    assert_eq!(status_of(&env, &id, S1), "applied");
    env.run_tasks();
    assert!(
        !env.vstore().load().people["P1"].centroids.is_empty(),
        "「好」= 用户确认:补回灌"
    );
    assert_eq!(
        env.vstore().samples_traced_to("P1", &id, "S1").len(),
        1,
        "并存确认样本"
    );
    let rec = &idf::load_ops(&dir_of(&env, &id)).ops[0];
    assert!(rec.acknowledged);
}

#[test]
fn acknowledged_feedback_backs_off_if_the_user_relinked_meanwhile() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎"), ("P2", "李四")]);
    let id = make_note(tmp.path());
    let op = auto_applied(&env, &id);
    acknowledge_identify_with(&env, id.clone(), op).unwrap();
    env.nstore().assign_speaker_person(&id, "S1", "P2").unwrap();
    env.run_tasks();
    assert!(
        env.vstore().load().people["P1"].centroids.is_empty(),
        "门内复核:现关联已不是目标"
    );
    assert!(env.vstore().sample_paths_existing("P1").is_empty());
}

// ── 崩溃恢复 ──

fn crash_op(env: &FakeEnv, id: &str, stage: &str) {
    let dir = dir_of(env, id);
    let mut ops = idf::load_ops(&dir);
    ops.ops.push(idf::IdentifyOp {
        op_id: "iop-crash".into(),
        fingerprint: fp(S1),
        cluster: "S1".into(),
        seqs: S1.to_vec(),
        target_person: "P1".into(),
        target_name: "王虎".into(),
        quote: String::new(),
        quote_type: String::new(),
        created_at: "t2".into(),
        stage: stage.into(),
        acknowledged: false,
        reinforce_skipped: None,
        undo_stage: None,
        non_revertible: None,
    });
    idf::save_ops(&dir, &ops).unwrap();
}

/// 回归(#141 之后):崩在「关联已写、阶段未记」时,恢复必须认得这条关联(此前读段落
/// 身份恒认不出,op 被标放弃、回执消失,关联却已生效)。
#[test]
fn recovery_rolls_forward_an_op_whose_link_already_landed() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎")]);
    let id = make_note(tmp.path());
    write_identify(
        &env,
        &id,
        vec![assignment(S1, Some("P1"), None, Tier::High)],
    );
    env.nstore()
        .assign_speaker_person_if(&id, "S1", "P1")
        .unwrap();
    crash_op(&env, &id, "pending");
    recover_identify_ops(&env, &id);
    let op = &idf::load_ops(&dir_of(&env, &id)).ops[0];
    assert_eq!(op.stage, "done", "前滚完成");
    assert_eq!(op.undo_stage, None, "回执保留,可撤销");
    assert_eq!(status_of(&env, &id, S1), "auto_applied");
}

#[test]
fn recovery_abandons_an_op_that_never_linked() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎")]);
    let id = make_note(tmp.path());
    write_identify(
        &env,
        &id,
        vec![assignment(S1, Some("P1"), None, Tier::High)],
    );
    crash_op(&env, &id, "pending");
    recover_identify_ops(&env, &id);
    let op = &idf::load_ops(&dir_of(&env, &id)).ops[0];
    assert_eq!(op.stage, "aborted");
    assert_eq!(status_of(&env, &id, S1), "suggested", "建议卡还在");
}

// ── 人工采纳 / 拒绝 ──

#[test]
fn applying_a_new_face_creates_the_person_or_cleans_up_on_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[]);
    let id = make_note(tmp.path());
    write_identify(
        &env,
        &id,
        vec![assignment(S2, None, Some("赵六"), Tier::Medium)],
    );
    *env.fail_edit_once.lock().unwrap() = Some("AssignPerson");
    assert!(apply_identify_suggestion_with(&env, id.clone(), fp(S2)).is_err());
    assert!(
        env.vstore()
            .load()
            .people
            .values()
            .all(|p| p.name != "赵六"),
        "关联失败:刚建的空人收回"
    );
    apply_identify_suggestion_with(&env, id.clone(), fp(S2)).unwrap();
    let pid = linked(&env, &id, "S2").expect("已关联");
    assert_eq!(env.vstore().load().people[&pid].name, "赵六");
    assert_eq!(status_of(&env, &id, S2), "applied");
}

#[test]
fn rejecting_blocks_that_target_for_the_cluster() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎")]);
    let id = make_note(tmp.path());
    write_identify(
        &env,
        &id,
        vec![assignment(S2, Some("P1"), None, Tier::Medium)],
    );
    reject_identify_suggestion_with(&env, id.clone(), fp(S2)).unwrap();
    assert_eq!(status_of(&env, &id, S2), "rejected");
    assert!(list_identify_suggestions_with(&env).unwrap().is_empty());
}
