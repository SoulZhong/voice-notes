//! 认人流程的测试:计划纯函数的表驱动用例 + 假 [`LinkEnv`] 下的整链用例(临时目录里的
//! 真实笔记与声纹库;后台工作先排队,由用例决定何时跑——用来钉住「门内复核」)。

use super::*;
use crate::diar::{SpeakerEmbedder, TaggedEmbedder};
use crate::occupancy;
use crate::voice_env::VoiceEnv;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    tasks: StdMutex<Vec<LinkTask>>,
    fail_edit_once: StdMutex<Option<&'static str>>,
    rebuilt: StdMutex<Vec<String>>,
    rebuild_requests: AtomicUsize,
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
    /// 跑完排队中的后台工作(含工作里再排的)。
    fn run_tasks(&self) -> usize {
        let mut n = 0;
        loop {
            let batch: Vec<LinkTask> = std::mem::take(&mut *self.tasks.lock().unwrap());
            if batch.is_empty() {
                return n;
            }
            for t in batch {
                t(self);
                n += 1;
            }
        }
    }
    fn fail_next(&self, kind: &'static str) {
        *self.fail_edit_once.lock().unwrap() = Some(kind);
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
            E::ClearPerson { .. } => "ClearPerson",
            E::RenameSpeaker { .. } => "RenameSpeaker",
            other => panic!("认人流程不该发出 {other:?}"),
        };
        {
            let mut f = self.fail_edit_once.lock().unwrap();
            if *f == Some(kind) {
                *f = None;
                return Err(format!("注入失败: {kind}"));
            }
        }
        let s = self.nstore();
        match op {
            E::AssignPerson {
                id,
                speaker_id,
                person_id,
            } => s.assign_speaker_person(&id, &speaker_id, &person_id),
            E::ClearPerson { id, speaker_id } => s.clear_speaker_person(&id, &speaker_id),
            E::RenameSpeaker {
                id,
                speaker_id,
                name,
                unlink_from,
            } => s.rename_speaker_unlinking(&id, &speaker_id, &name, unlink_from.as_deref()),
            _ => unreachable!(),
        }
        .map_err(|e| e.to_string())
    }
    fn admit(&self, _note_id: &str, _intent: occupancy::Intent) -> Result<(), String> {
        Ok(())
    }
    fn open_embedder(&self) -> anyhow::Result<TaggedEmbedder> {
        let tag = self.vstore().load().embedding_model;
        Ok(TaggedEmbedder::new(tag, Box::new(ConstEmbedder)))
    }
    fn request_rebuild(&self, _reason: &'static str) {
        self.rebuild_requests.fetch_add(1, Ordering::SeqCst);
    }
}

impl LinkEnv for FakeEnv {
    fn spawn(&self, task: LinkTask) {
        self.tasks.lock().unwrap().push(task);
    }
    fn rebuild_person(&self, person_id: &str) -> Result<(), String> {
        self.rebuilt.lock().unwrap().push(person_id.to_string());
        Ok(())
    }
}

// ── 夹具 ──

/// 一篇笔记:`durs_ms[i]` 是第 i 段时长,全部归 S1(拆分产物时 S1 带 split_born)。写 mic.wav。
fn make_note(root: &std::path::Path, durs_ms: &[u64], split_born: bool) -> String {
    let notes = root.join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    let now = chrono::Local::now();
    let mut w = store::writer::NoteWriter::create(&notes, now).unwrap();
    let id = w.note_id().to_string();
    let mut t = 0;
    for (i, d) in durs_ms.iter().enumerate() {
        w.append_final("mic", &format!("第{i}段"), t, t + d, Some("S1"), None)
            .unwrap();
        t += d;
    }
    w.sync_speakers(&[("S1".to_string(), vec!["mic".to_string()])])
        .unwrap();
    w.finalize(now).unwrap();
    drop(w);
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav = hound::WavWriter::create(notes.join(&id).join("mic.wav"), spec).unwrap();
    for _ in 0..(t * 16) {
        wav.write_sample(4000i16).unwrap();
    }
    wav.finalize().unwrap();
    if split_born {
        let p = notes.join(&id).join("speakers.json");
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        v["S1"]["split_born"] = serde_json::json!(true);
        std::fs::write(&p, v.to_string()).unwrap();
    }
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

fn linked(env: &FakeEnv, id: &str) -> Option<String> {
    env.nstore().load(id).unwrap().speakers["S1"]
        .person_id
        .clone()
}

fn plan_for(
    env: &FakeEnv,
    id: &str,
    target: &str,
    audited: Option<u64>,
    selected: &[u64],
) -> LinkPlan {
    let note = env.nstore().load(id).unwrap();
    plan_link(&note, &env.vstore().load(), "S1", target, audited, selected)
}

fn seqs(p: &LinkPlan) -> Vec<u64> {
    p.sample
        .as_ref()
        .map(|s| s.picks.iter().map(|x| x.seq).collect())
        .unwrap_or_default()
}

// ── 计划(纯函数) ──

#[test]
fn plan_ordinary_speaker_takes_longest_until_ten_seconds_and_feeds_the_group() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎")]);
    // 段长:3s 7s 4s 6s → 最长优先 7s+6s=13s ≥ 10s 即止
    let id = make_note(tmp.path(), &[3_000, 7_000, 4_000, 6_000], false);
    let p = plan_for(&env, &id, "P1", None, &[]);
    assert_eq!(seqs(&p), vec![1, 3]);
    assert!(!p.sample.as_ref().unwrap().reinforce);
    assert_eq!(
        p.group_feedback,
        Some(None),
        "普通说话人整组回灌,无先前人物"
    );
    assert_eq!(p.retire_prior, None);
}

#[test]
fn plan_puts_the_auditioned_segment_first_then_fills_to_ten_seconds() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎")]);
    let id = make_note(tmp.path(), &[3_000, 7_000, 4_000, 6_000], false);
    // 试听了 3s 的第 0 段:它打头,其后按最长补到够 10s(7s)
    let p = plan_for(&env, &id, "P1", Some(0), &[]);
    assert_eq!(seqs(&p), vec![0, 1]);
}

#[test]
fn plan_selected_segments_are_used_as_is_or_not_at_all() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎")]);
    let id = make_note(tmp.path(), &[3_000, 7_000, 4_000, 6_000], false);
    assert_eq!(
        seqs(&plan_for(&env, &id, "P1", Some(1), &[0, 2, 3])),
        vec![0, 2, 3],
        "勾选优先于试听,不补"
    );
    let p = plan_for(&env, &id, "P1", None, &[0, 2]);
    assert_eq!(p.sample, None, "勾选合计 7s 不足 10s:不入库、也不按最长补");
    assert!(p.sample_skipped.is_some(), "跳过原因随计划返回,由执行层记日志");
    assert_eq!(p.group_feedback, Some(None), "整组回灌照做");
}

#[test]
fn plan_split_born_only_uses_what_the_user_confirmed_and_never_feeds_the_group() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎")]);
    let id = make_note(tmp.path(), &[3_000, 7_000, 4_000, 6_000], true);
    let p = plan_for(&env, &id, "P1", Some(1), &[2, 3]);
    assert_eq!(seqs(&p), vec![2, 3]);
    assert!(
        p.sample.as_ref().unwrap().reinforce,
        "拆分产物:确认段另做单段回灌"
    );
    assert_eq!(p.group_feedback, None, "混杂簇不做整组回灌");
    assert_eq!(
        seqs(&plan_for(&env, &id, "P1", Some(1), &[])),
        vec![1],
        "没勾选就用试听那段,不凑数"
    );
    let p = plan_for(&env, &id, "P1", None, &[]);
    assert_eq!(
        (p.sample, p.group_feedback),
        (None, None),
        "什么都没确认:库零写入"
    );
}

#[test]
fn plan_retires_only_a_named_different_prior_and_merges_an_unnamed_one() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎"), ("P2", "李四"), ("P3", "")]);
    let id = make_note(tmp.path(), &[6_000, 6_000], false);
    env.nstore().assign_speaker_person(&id, "S1", "P1").unwrap();
    let p = plan_for(&env, &id, "P2", None, &[]);
    assert_eq!(
        p.retire_prior.as_deref(),
        Some("P1"),
        "改指别的有名字的人:退旧人样本"
    );
    assert_eq!(p.group_feedback, Some(Some(("P1".into(), "王虎".into()))));
    assert_eq!(
        plan_for(&env, &id, "P1", None, &[]).retire_prior,
        None,
        "还是同一个人:不退"
    );
    env.nstore().assign_speaker_person(&id, "S1", "P3").unwrap();
    let p = plan_for(&env, &id, "P2", None, &[]);
    assert_eq!(p.retire_prior, None, "无名先前人物不退样本");
    assert_eq!(
        p.group_feedback,
        Some(Some(("P3".into(), String::new()))),
        "而是整人并入"
    );
}

// ── 整链 ──

#[test]
fn link_writes_the_sample_and_feeds_the_centroid() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎")]);
    let id = make_note(tmp.path(), &[SEG_MS, SEG_MS], false);
    link_speaker_with(&env, &id, "S1", "P1", None, &[]).unwrap();
    assert_eq!(linked(&env, &id).as_deref(), Some("P1"), "笔记侧同步生效");
    assert_eq!(env.run_tasks(), 2, "确认样本 + 整组回灌");
    assert_eq!(
        env.vstore().samples_traced_to("P1", &id, "S1").len(),
        1,
        "样本带溯源入库"
    );
    assert!(
        !env.vstore().load().people["P1"].centroids.is_empty(),
        "质心已回灌"
    );
}

#[test]
fn unlinking_before_background_work_runs_makes_it_back_off() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎")]);
    let id = make_note(tmp.path(), &[SEG_MS, SEG_MS], false);
    link_speaker_with(&env, &id, "S1", "P1", None, &[]).unwrap();
    // 关联在后台工作跑之前被别的路径解除(直接改笔记,不经 unlink_speaker_with——
    // 那条会另排退样本任务,把没复核就写进去的样本又删掉,掩盖问题)。
    env.nstore().clear_speaker_person(&id, "S1").unwrap();
    env.run_tasks();
    assert!(
        env.vstore().sample_paths_existing("P1").is_empty(),
        "门内复核:已解除就不写样本"
    );
    assert!(
        env.vstore().load().people["P1"].centroids.is_empty(),
        "也不回灌"
    );
}

#[test]
fn relinking_to_another_named_person_retires_the_old_samples() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎"), ("P2", "李四")]);
    let id = make_note(tmp.path(), &[SEG_MS, SEG_MS], false);
    link_speaker_with(&env, &id, "S1", "P1", None, &[]).unwrap();
    env.run_tasks();
    assert_eq!(env.vstore().samples_traced_to("P1", &id, "S1").len(), 1);
    link_speaker_with(&env, &id, "S1", "P2", None, &[]).unwrap();
    env.run_tasks();
    assert!(
        env.vstore().samples_traced_to("P1", &id, "S1").is_empty(),
        "旧人从这簇截的样本退掉"
    );
    assert_eq!(
        *env.rebuilt.lock().unwrap(),
        vec!["P1".to_string()],
        "退样本后重建旧人"
    );
    assert_eq!(env.vstore().samples_traced_to("P2", &id, "S1").len(), 1);
}

#[test]
fn unlink_retires_traced_samples() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎")]);
    let id = make_note(tmp.path(), &[SEG_MS, SEG_MS], false);
    link_speaker_with(&env, &id, "S1", "P1", None, &[]).unwrap();
    env.run_tasks();
    unlink_speaker_with(&env, &id, "S1").unwrap();
    assert_eq!(linked(&env, &id), None);
    env.run_tasks();
    assert!(env.vstore().samples_traced_to("P1", &id, "S1").is_empty());
}

/// 改名成与库名不同的名字 = 「这不是那个人」:同一次写入里解除旧关联并改名,
/// 退旧人样本,再按命名即入库建新人并关联。
#[test]
fn renaming_to_a_different_name_relinks_to_a_new_person() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎")]);
    let id = make_note(tmp.path(), &[SEG_MS, SEG_MS], false);
    link_speaker_with(&env, &id, "S1", "P1", None, &[]).unwrap();
    env.run_tasks();
    rename_speaker_with(&env, &id, "S1", "李四", None, &[]).unwrap();
    let now = linked(&env, &id).expect("命名即入库:关联到新人");
    assert_ne!(now, "P1");
    assert_eq!(env.vstore().load().people[&now].name, "李四");
    env.run_tasks();
    assert!(
        env.vstore().samples_traced_to("P1", &id, "S1").is_empty(),
        "旧人样本退掉"
    );
}

#[test]
fn renaming_to_the_library_name_keeps_the_link() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎")]);
    let id = make_note(tmp.path(), &[SEG_MS, SEG_MS], false);
    link_speaker_with(&env, &id, "S1", "P1", None, &[]).unwrap();
    env.run_tasks();
    rename_speaker_with(&env, &id, "S1", "王虎", None, &[]).unwrap();
    assert_eq!(linked(&env, &id).as_deref(), Some("P1"));
    env.run_tasks();
    assert_eq!(
        env.vstore().samples_traced_to("P1", &id, "S1").len(),
        1,
        "不退样本"
    );
}

#[test]
fn renaming_to_an_ambiguous_name_unlinks_but_does_not_guess() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(
        tmp.path(),
        &[("P1", "王虎"), ("P2", "张三"), ("P3", "张三")],
    );
    let id = make_note(tmp.path(), &[SEG_MS, SEG_MS], false);
    link_speaker_with(&env, &id, "S1", "P1", None, &[]).unwrap();
    rename_speaker_with(&env, &id, "S1", "张三", None, &[]).unwrap();
    let note = env.nstore().load(&id).unwrap();
    assert_eq!(note.speakers["S1"].person_id, None, "旧关联解除");
    assert_eq!(note.speakers["S1"].name, "张三", "名字已改");
}

/// 改名的笔记侧是一次写入:解除关联只在「此刻仍关联那个人」时发生。
#[test]
fn rename_unlinks_only_the_expected_person() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[("P1", "王虎"), ("P2", "李四")]);
    let id = make_note(tmp.path(), &[SEG_MS], false);
    env.nstore().assign_speaker_person(&id, "S1", "P2").unwrap();
    env.nstore()
        .rename_speaker_unlinking(&id, "S1", "阿三", Some("P1"))
        .unwrap();
    let m = env.nstore().load(&id).unwrap().speakers["S1"].clone();
    assert_eq!(
        m.person_id.as_deref(),
        Some("P2"),
        "期间已被改成别人:不解除"
    );
    assert_eq!(m.name, "阿三");
}

#[test]
fn naming_a_stranger_that_fails_to_link_leaves_no_orphan_person() {
    let tmp = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(tmp.path());
    seed_people(tmp.path(), &[]);
    let id = make_note(tmp.path(), &[SEG_MS], false);
    env.nstore().rename_speaker(&id, "S1", "新人").unwrap();
    env.fail_next("AssignPerson");
    enroll_named_with(&env, &id, "S1", "新人", None, &[]);
    assert!(
        env.vstore()
            .load()
            .people
            .values()
            .all(|p| p.name != "新人"),
        "刚建的空人被收回"
    );
    enroll_named_with(&env, &id, "S1", "新人", None, &[]);
    assert!(linked(&env, &id).is_some(), "重试成功");
}
