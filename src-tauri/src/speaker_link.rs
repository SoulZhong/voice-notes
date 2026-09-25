//! 认人:把一篇笔记里的说话人关联到声纹库人物(**指认**),以及它的反向与变体——
//! 取消关联、改名即指认、命名即入库。
//!
//! 一次指认 = 笔记侧写关联(同步)+ 库侧最多三项后台工作:
//! - **确认样本**:切出用户确认过的音频存为此人样本(拆分产物另做单段回灌);
//! - **整组回灌**:普通说话人把本簇全部段回灌此人质心,或把无名的先前人物整个并入;
//! - **退旧样本**:先前关联的是另一个有名字的人时,退掉他从这一簇截的样本并重建。
//!
//! 做哪几项、用哪些段,由纯函数 [`plan_link`] 按笔记快照与用户的试听/勾选算出(表驱动
//! 测试);执行交给 [`LinkEnv::spawn`]。每项后台工作在门内**复核**"此刻仍是这个关联"
//! 再写库——关联与取消关联各起后台任务,门只保证互斥、不保证顺序。
//!
//! 「确认才入库」(2026-08-27):库写入只在用户确认动作之后发生,见 CONTEXT.md。

use crate::voice_env::VoiceEnv;
use crate::{feedback, lifecycle, store, tr, FEEDBACK_GATE};
use std::collections::BTreeSet;

/// 一项库侧后台工作。执行器负责给它一个 env(生产:随线程带走的 TauriEnv 克隆)。
pub(crate) type LinkTask = Box<dyn FnOnce(&dyn LinkEnv) + Send>;

pub(crate) trait LinkEnv: VoiceEnv {
    /// 后台执行一项库侧工作(生产:独立线程;测试:先排队、由用例决定何时跑)。
    fn spawn(&self, task: LinkTask);
    /// 按样本重建某人声纹(退样本之后)。
    fn rebuild_person(&self, person_id: &str) -> Result<(), String>;
}

// ── 计划 ──

/// 一次指认的库侧计划。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LinkPlan {
    /// 退掉这个人从本簇截的样本(先前关联的另一个有名字的人)。
    pub retire_prior: Option<String>,
    /// 确认样本:存为样本的段;`reinforce` = 这些段另做单段回灌(拆分产物)。
    pub sample: Option<SamplePlan>,
    /// 整组回灌 / 并入无名先前人物(普通说话人)。`prior` = (先前人物 id, 库名)。
    pub group_feedback: Option<Option<(String, String)>>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SamplePlan {
    pub picks: Vec<store::SegmentRecord>,
    pub reinforce: bool,
}

/// 从确认段池挑样本料:按时长降序累计到 ≥AUTO_ENROLL_MS(10s)即止。
/// 总量不足返回空(调用方跳过)。只拼**用户确认动作覆盖的段**——与 #167 备忘
/// 「只拼用户确认过的段」一致;未确认段永不入选。
pub(crate) fn pick_confirmed_sample_segs(pool: &[store::SegmentRecord]) -> Vec<store::SegmentRecord> {
    let mut segs: Vec<store::SegmentRecord> = pool.to_vec();
    segs.sort_by_key(|s| std::cmp::Reverse(s.end_ms.saturating_sub(s.start_ms)));
    let mut acc = 0u64;
    let mut picks = Vec::new();
    for s in segs {
        if acc >= store::AUTO_ENROLL_MS {
            break;
        }
        acc += s.end_ms.saturating_sub(s.start_ms);
        picks.push(s);
    }
    if acc < store::AUTO_ENROLL_MS {
        return Vec::new();
    }
    picks
}

/// 算一次指认的库侧计划(纯函数)。`resolved` 是已经过 redirects 归一的目标人物。
pub(crate) fn plan_link(
    note: &store::Note,
    vp: &store::Voiceprints,
    speaker_id: &str,
    resolved: &str,
    audited_seq: Option<u64>,
    selected_seqs: &[u64],
) -> LinkPlan {
    let prior = note
        .speakers
        .get(speaker_id)
        .and_then(|m| m.person_id.as_deref())
        .and_then(|pid| store::VoiceprintStore::resolve(vp, pid))
        .map(|rid| (rid.to_string(), vp.people.get(rid).map(|p| p.name.clone()).unwrap_or_default()));
    // 样本↔会议反向同步:先前关联的是**另一个有名字的人** → 他从这簇截的样本退掉。
    // 无名先前人物走整组回灌的 MergePrior(整人并入目标,样本随之迁移),不在此处理。
    let retire_prior = prior
        .as_ref()
        .filter(|(pid, pname)| pid != resolved && !pname.is_empty())
        .map(|(pid, _)| pid.clone());
    let split_born = note.speakers.get(speaker_id).is_some_and(|m| m.split_born);
    if split_born {
        // 「确认才入库」(2026-08-22-one-click-split-design.md):拆分产物说话人关联时
        // **不做整组批量回灌**——混杂簇是批量喂库的污染源。只有用户确认过的段进库:
        // 勾选了「作为样本」的段(Codex P1:此前拆分产物忽略了勾选),否则刚试听过的
        // 那一段(audited_seq)。都须确属该说话人。不拼其它段(拆分簇未确认段有混杂
        // 风险),不足 10s 会被时长门拒——如实跳过,总好过拿未确认段凑数。
        let owned = |q: u64| {
            note.segments.iter().find(|s| s.seq == q && s.speaker.as_deref() == Some(speaker_id)).cloned()
        };
        let picks: Vec<store::SegmentRecord> = if !selected_seqs.is_empty() {
            selected_seqs.iter().filter_map(|q| owned(*q)).collect()
        } else {
            audited_seq.and_then(owned).into_iter().collect()
        };
        return LinkPlan {
            retire_prior,
            sample: (!picks.is_empty()).then_some(SamplePlan { picks, reinforce: true }),
            group_feedback: None,
        };
    }
    // 确认才入库时代的样本闭环(codex:停录不再自动写样本后,经确认的人物若一份样本都
    // 没有,换声纹模型 rebuild 时质心被清空、无从重算,人就没了)。关联即确认:切该说话人
    // 本篇的段存为样本;append_confirmed_sample 内部自带隔离/满员/去重门。
    let pool: Vec<store::SegmentRecord> =
        note.segments.iter().filter(|s| s.speaker.as_deref() == Some(speaker_id)).cloned().collect();
    // 用户明确勾选了「作为样本」的段(2026-08-30):样本只由这些段构成,不按最长补——
    // "我听过的最有代表性的那几段,而不是全部"。合计不足 10s 则不入库。
    let selected: Vec<store::SegmentRecord> =
        selected_seqs.iter().filter_map(|q| pool.iter().find(|s| s.seq == *q).cloned()).collect();
    let mut picks = if !selected.is_empty() {
        let total: u64 = selected.iter().map(|s| s.end_ms.saturating_sub(s.start_ms)).sum();
        if total < store::AUTO_ENROLL_MS {
            eprintln!("确认样本跳过({resolved}):勾选段合计 {total}ms < 10s,不入库(未按最长补)");
            Vec::new()
        } else {
            selected
        }
    } else {
        pick_confirmed_sample_segs(&pool)
    };
    // 2026-08-30 用户问"样本是最具代表性的吗":用户刚试听过的那一段是他亲耳确认过
    // "这是这个人"的音频,优先做样本核心,其余按最长补足 10s——此前只按最长挑,最长的段
    // 恰好混了别人时,样本就带着别人的声音入库。
    if let Some(seq) = audited_seq.filter(|_| selected_seqs.is_empty()) {
        if let Some(a) = pool.iter().find(|s| s.seq == seq) {
            picks.retain(|s| s.seq != seq);
            picks.insert(0, a.clone());
            // 试听段进来后,后面的段只补到累计够 10s 为止(段保持完整;最后一段整段保留,
            // 样本可能略超 10s——Codex P2,接受)。
            let mut acc = 0u64;
            picks.retain(|s| {
                let keep = acc < store::AUTO_ENROLL_MS;
                acc += s.end_ms.saturating_sub(s.start_ms);
                keep
            });
        }
    }
    if picks.is_empty() {
        eprintln!("确认样本跳过({resolved}):确认段总时长不足 10s");
    }
    LinkPlan {
        retire_prior,
        sample: (!picks.is_empty()).then_some(SamplePlan { picks, reinforce: false }),
        group_feedback: Some(prior),
    }
}

// ── 入口 ──

/// 指认:把说话人关联到库人物并排好库侧工作。调用方自备准入(env.admit)。
/// 手动关联、识别建议采纳、命名即入库共用这一个入口。
pub(crate) fn link_speaker_with(
    env: &dyn LinkEnv,
    note_id: &str,
    speaker_id: &str,
    person_id: &str,
    audited_seq: Option<u64>,
    selected_seqs: &[u64],
) -> Result<(), String> {
    let root = env.root().map_err(|e| e.to_string())?;
    let vp = store::VoiceprintStore::new(root).load();
    let Some(resolved) = store::VoiceprintStore::resolve(&vp, person_id).map(str::to_string) else {
        return Err(tr!(
            "声纹库中没有该人物: {person_id}",
            "No such person in the voiceprint library: {person_id}",
            person_id = person_id
        ));
    };
    // 计划的输入必须在写入前同步取好:指认时刻的段快照与先前关联,后台任务不再回读
    // 笔记来决定"做什么",避免基于"稍后状态"的混合版本回灌(spec P1-2)。
    let dir = env.notes_dir().map_err(|e| e.to_string())?;
    let note = store::NoteStore::new(dir).load(note_id).map_err(|e| e.to_string())?;
    let plan = plan_link(&note, &vp, speaker_id, &resolved, audited_seq, selected_seqs);
    env.edit_note(lifecycle::machine::EditOp::AssignPerson {
        id: note_id.to_string(),
        speaker_id: speaker_id.to_string(),
        person_id: resolved.clone(),
    })?;
    if let Some(pid) = plan.retire_prior {
        spawn_retire_samples(env, pid, note_id.to_string(), speaker_id.to_string());
    }
    if let Some(sample) = plan.sample {
        spawn_confirmed_sample(
            env,
            note_id.to_string(),
            speaker_id.to_string(),
            resolved.clone(),
            note.segments.clone(),
            sample.picks,
            sample.reinforce,
        );
    }
    if let Some(prior) = plan.group_feedback {
        spawn_group_feedback(
            env,
            note_id.to_string(),
            note.segments,
            feedback::SegFilter::Speakers(BTreeSet::from([speaker_id.to_string()])),
            prior,
            resolved,
            Some(speaker_id.to_string()),
        );
    }
    Ok(())
}

/// 取消关联:只断开与库人物的绑定(表项与段落归属不动),有溯源的样本退还并重建。
/// 调用方自备准入。
///
/// **不连带撤销这次关联带来的质心回灌**(2026-08-19 范围决定):撤销要求"撤销任务"与
/// "回灌任务"两个后台任务正确排序,而它们只隔着一把不保证顺序的门——三轮 codex review
/// 里最难缠的几条 P1 根源全在这里。样本是真源,退掉样本再重建,等价于把这段声音从他的
/// 声纹里拿走(2026-08-29)。见 docs/superpowers/specs/2026-08-19-voiceprint-model-space-design.md。
pub(crate) fn unlink_speaker_with(env: &dyn LinkEnv, note_id: &str, speaker_id: &str) -> Result<(), String> {
    let dir = env.notes_dir().map_err(|e| e.to_string())?;
    let note = store::NoteStore::new(dir).load(note_id).map_err(|e| e.to_string())?;
    // 清空之前取:清完就查不到当初关联的是谁了(load 后经 redirects 归一)。
    let linked = note.speakers.get(speaker_id).and_then(|m| m.person_id.clone());
    env.edit_note(lifecycle::machine::EditOp::ClearPerson {
        id: note_id.to_string(),
        speaker_id: speaker_id.to_string(),
    })?;
    if let Some(pid) = linked {
        spawn_retire_samples(env, pid, note_id.to_string(), speaker_id.to_string());
    }
    Ok(())
}

/// 改名(非录制中的笔记)= 指认。改成**与库中现名不同**的名字,视为"这不是库里那个人":
/// 解除旧关联与改名在**同一次**笔记写入里完成(此前分两步,中途失败会停在"已解除、
/// 名字未改"),退还旧人样本,再按命名即入库重新走。改回与库名相同的名字则关联不动。
pub(crate) fn rename_speaker_with(
    env: &dyn LinkEnv,
    note_id: &str,
    speaker_id: &str,
    name: &str,
    audited_seq: Option<u64>,
    selected_seqs: &[u64],
) -> Result<(), String> {
    let relink = (|| -> Option<String> {
        let note = store::NoteStore::new(env.notes_dir().ok()?).load(note_id).ok()?;
        let pid = note.speakers.get(speaker_id)?.person_id.clone()?;
        let vp = store::VoiceprintStore::new(env.root().ok()?).load();
        let lib_name = store::VoiceprintStore::resolve(&vp, &pid)
            .and_then(|rid| vp.people.get(rid))
            .map(|p| p.name.clone())
            .unwrap_or_default();
        (lib_name != name).then_some(pid)
    })();
    env.edit_note(lifecycle::machine::EditOp::RenameSpeaker {
        id: note_id.to_string(),
        speaker_id: speaker_id.to_string(),
        name: name.to_string(),
        unlink_from: relink.clone(),
    })?;
    if let Some(old_pid) = relink {
        spawn_retire_samples(env, old_pid, note_id.to_string(), speaker_id.to_string());
    }
    enroll_named_with(env, note_id, speaker_id, name, audited_seq, selected_seqs);
    Ok(())
}

/// 命名即入库(2026-08-27「确认才入库」的转正通路):无主说话人得名 → 库里恰有一个
/// 同名人则关联它,没有则建人再关联,重名多于一个则跳过(自动挑人必错,让用户走关联
/// 动线亲自选)。仍关联者(= 名字与库名相同)不动。失败只记日志,笔记内命名不受影响。
pub(crate) fn enroll_named_with(
    env: &dyn LinkEnv,
    note_id: &str,
    speaker_id: &str,
    name: &str,
    audited_seq: Option<u64>,
    selected_seqs: &[u64],
) {
    let run = || -> anyhow::Result<()> {
        let note = store::NoteStore::new(env.notes_dir()?).load(note_id)?;
        let Some(m) = note.speakers.get(speaker_id) else { return Ok(()) };
        if m.person_id.is_some() {
            return Ok(());
        }
        let vp_store = store::VoiceprintStore::new(env.root()?);
        let vp = vp_store.load();
        let same: Vec<&String> = vp.people.iter().filter(|(_, p)| p.name == name).map(|(id, _)| id).collect();
        let (target, created) = match same.len() {
            0 => (vp_store.create_person(name, &chrono::Local::now().to_rfc3339())?, true),
            1 => (same[0].clone(), false),
            n => {
                eprintln!("命名入库跳过({name}):库中有 {n} 个同名人,请手动关联挑选");
                return Ok(());
            }
        };
        if let Err(e) = link_speaker_with(env, note_id, speaker_id, &target, audited_seq, selected_seqs) {
            if created {
                // 刚建的空人别留孤儿(与识别建议采纳同款收尾)。
                let _ = vp_store.delete_person_if_empty(&target);
            }
            return Err(anyhow::Error::msg(e));
        }
        eprintln!("命名入库:{note_id}/{speaker_id} 「{name}」 → {target}");
        Ok(())
    };
    if let Err(e) = run() {
        eprintln!("命名入库失败(笔记内命名不受影响): {e}");
    }
}

// ── 库侧后台工作(门内复核后才写库) ──

/// 确认样本落库:把 picks(多段拼接,凑够 10s 门槛——单挑最长段会被
/// append_confirmed_sample 的时长门拒掉,确认过的人物零样本,换模型 rebuild 直接
/// 把人清没)切音频写为人物样本;reinforce=true 时另做这些段的质心回灌(拆分确认
/// 路径用;普通关联的回灌由整组回灌承担,不在此重复)。失败只记日志——本篇关联已生效,
/// 库写入是增强不是前提。
pub(crate) fn spawn_confirmed_sample(
    env: &dyn LinkEnv,
    note_id: String,
    speaker_id: String,
    person_id: String,
    segments: Vec<store::SegmentRecord>,
    picks: Vec<store::SegmentRecord>,
    reinforce: bool,
) {
    if picks.is_empty() && !reinforce {
        eprintln!("确认样本跳过({person_id}):确认段总时长不足 10s");
        return;
    }
    env.spawn(Box::new(move |env: &dyn LinkEnv| {
        if let Err(e) = run_confirmed_sample(env, &note_id, &speaker_id, &person_id, &segments, picks, reinforce) {
            eprintln!("确认样本入库失败(本篇关联不受影响): {e}");
        }
    }));
}

fn run_confirmed_sample(
    env: &dyn LinkEnv,
    note_id: &str,
    speaker_id: &str,
    person_id: &str,
    segments: &[store::SegmentRecord],
    picks: Vec<store::SegmentRecord>,
    reinforce: bool,
) -> anyhow::Result<()> {
    let root = env.root()?;
    let nroot = env.notes_dir()?;
    let dir = nroot.join(note_id);
    let vp_store = store::VoiceprintStore::new(root);
    let _fb = FEEDBACK_GATE.lock().unwrap();
    // 门内复核(codex 五轮 P1):命令返回到本任务执行之间,用户可能已解除/改走关联,或
    // 段落已被改派——拿旧料写样本会把音频永久挂到错人身上。① 说话人现关联仍是
    // person_id;② picks 逐段仍归该说话人(改派段剔除),剔完不足 10s 门槛即放弃。
    let fresh = store::NoteStore::new(nroot).load(note_id)?;
    let vp_now = vp_store.load();
    // person_id 也过 redirects 归一(codex 末轮 P2):任务等待期间目标被合并时,现关联
    // 解析到 winner 而入参还是 loser,直接比会冤枉合法关联。
    let person_id = store::VoiceprintStore::resolve(&vp_now, person_id)
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("目标人物已不在库,样本放弃"))?;
    let cur = fresh
        .speakers
        .get(speaker_id)
        .and_then(|m| m.person_id.as_deref())
        .and_then(|pid| store::VoiceprintStore::resolve(&vp_now, pid))
        .map(str::to_string);
    anyhow::ensure!(cur.as_deref() == Some(person_id.as_str()), "说话人现关联({cur:?})已不是 {person_id},样本放弃");
    let picks: Vec<store::SegmentRecord> = picks
        .into_iter()
        .filter(|p| fresh.segments.iter().any(|s| s.seq == p.seq && s.speaker.as_deref() == Some(speaker_id)))
        .collect();
    let picked_ms: u64 = picks.iter().map(|s| s.end_ms.saturating_sub(s.start_ms)).sum();
    // 时长门只挡「样本落盘」,不挡回灌(codex 末轮 P2):拆分 audited 段在 1.5s~10s 之间时,
    // 样本存不了,但用户确认过的质心回灌照做——这是旧行为,不能倒退。
    let sample_ok = picked_ms >= store::AUTO_ENROLL_MS;
    anyhow::ensure!(sample_ok || reinforce, "确认段被改派后剩余 {picked_ms}ms 不足门槛,样本放弃");
    // 切音频:与 feedback 同一口径(track_pcm + offset_ms,16k f32)。每源全场 PCM 只读一次。
    let meta = store::audio::load_audio_meta(&dir);
    let mut pcm_by_src: std::collections::HashMap<String, Vec<f32>> = std::collections::HashMap::new();
    let mut sample: Vec<f32> = Vec::new();
    for seg in &picks {
        if !pcm_by_src.contains_key(&seg.source) {
            pcm_by_src.insert(seg.source.clone(), store::transcode::track_pcm(&dir, &seg.source)?);
        }
        let pcm = &pcm_by_src[&seg.source];
        let offset = meta.tracks.get(&seg.source).map(|t| t.offset_ms).unwrap_or(0);
        let start = (seg.start_ms.saturating_sub(offset) as usize).saturating_mul(16);
        let end = ((seg.end_ms.saturating_sub(offset) as usize).saturating_mul(16)).min(pcm.len());
        if start < end {
            sample.extend_from_slice(&pcm[start..end]);
        }
    }
    anyhow::ensure!(!sample.is_empty() || reinforce, "确认段落全部在音轨覆盖范围之外");
    if sample_ok && !sample.is_empty() {
        let wrote = vp_store.append_confirmed_sample(&person_id, &sample, note_id, speaker_id)?;
        if !wrote {
            eprintln!("确认样本未写入(隔离/满员/时长门/空音频): {person_id}");
        }
    } else {
        eprintln!("确认样本不足 10s 门槛({person_id}),只回灌不存样本");
    }
    if reinforce {
        // 确认段回灌质心(模型门禁/账本/黑名单照过)。
        let mut embedder = env.open_embedder()?;
        let mut needs_rebuild = false;
        let now = chrono::Local::now().to_rfc3339();
        let seqs: BTreeSet<u64> = picks.iter().map(|s| s.seq).collect();
        let r = feedback::reinforce_person(
            &dir,
            segments,
            &feedback::SegFilter::Seqs(seqs),
            &person_id,
            &vp_store,
            &mut embedder,
            &now,
            None,
            &mut needs_rebuild,
            false,
        )?;
        if needs_rebuild {
            env.request_rebuild("确认样本回灌纠错后质心置空");
        }
        eprintln!("确认样本入库: {person_id} {} 段 → {r:?}", picks.len());
    } else {
        eprintln!("确认样本入库: {person_id} {} 段(回灌由关联流程承担)", picks.len());
    }
    Ok(())
}

/// 整组回灌 / 并入无名先前人物(spec P1-2 纠错回灌)。`verify_speaker`:提交前复核用的
/// 原始稿说话人 id——**必须在真正写库之前再查一次**:关联与取消关联各起后台任务,
/// FEEDBACK_GATE 只保证互斥、不保证顺序,撤销先跑的话这个回灌照样落库,人物明明已经
/// 解除关联,增量却留在库里(codex review 二轮 P1#3)。
fn spawn_group_feedback(
    env: &dyn LinkEnv,
    note_id: String,
    segs: Vec<store::SegmentRecord>,
    filter: feedback::SegFilter,
    prior: Option<(String, String)>,
    target: String,
    verify_speaker: Option<String>,
) {
    env.spawn(Box::new(move |env: &dyn LinkEnv| {
        // 声明在 run 之外:纠错还原一旦清空了旧人物的质心,这件事就已经落盘了,之后 run
        // 无论返回 Ok 还是 Err 都必须补一次重建,否则那个人永远没有声纹(codex 实现轮二 P1)。
        let mut needs_rebuild = false;
        let outcome = run_group_feedback(env, &note_id, &segs, &filter, &prior, &target, &verify_speaker, &mut needs_rebuild);
        // 先无条件处理重建,再看回灌结果——顺序不能反,run 出错时也要重建。
        if needs_rebuild {
            eprintln!("feedback: 纠错还原清空了旧人物质心,排一次重建 note={note_id}");
            env.request_rebuild("纠错还原清空质心");
        }
        if let Err(e) = outcome {
            eprintln!("feedback: 回灌失败(不影响指认) note={note_id}: {e}");
        }
    }));
}

#[allow(clippy::too_many_arguments)]
fn run_group_feedback(
    env: &dyn LinkEnv,
    note_id: &str,
    segs: &[store::SegmentRecord],
    filter: &feedback::SegFilter,
    prior: &Option<(String, String)>,
    target: &str,
    verify_speaker: &Option<String>,
    needs_rebuild: &mut bool,
) -> anyhow::Result<()> {
    let vp = store::VoiceprintStore::new(env.root()?);
    let now = chrono::Local::now().to_rfc3339();
    let action = feedback::plan_action(prior.as_ref().map(|(i, n)| (i.as_str(), n.as_str())), target);
    // 门要先拿,复核要在门内做。**复核覆盖所有分支**——MergePrior 会把一整个人物并进目标,
    // 是比回灌更重的库级写入,且明确不由取消关联撤销(codex review 二轮 P1#2)。
    let _gate = FEEDBACK_GATE.lock().unwrap();
    if let Some(sid) = verify_speaker {
        let still_linked = env
            .notes_dir()
            .ok()
            .and_then(|d| store::NoteStore::new(d).load(note_id).ok())
            .and_then(|n| n.speakers.get(sid).and_then(|m| m.person_id.clone()))
            .is_some_and(|pid| pid == target);
        if !still_linked {
            eprintln!("feedback: note={note_id} {sid} 已不再关联 {target},跳过本次回灌/合并");
            return Ok(());
        }
    }
    match action {
        feedback::FeedbackAction::Noop => Ok(()),
        feedback::FeedbackAction::MergePrior { prior } => {
            // 不带嵌入器:并的是库里已有的质心,本来就同空间,传库当前标签。
            let lib_model = vp.load().embedding_model.clone();
            let receipt = vp.merge_journaled(&prior, target, None, "feedback-assign", None, &now, &lib_model)?;
            eprintln!("feedback: 无名先前人物 {prior} 已并入 {target}(回执 {receipt})");
            Ok(())
        }
        feedback::FeedbackAction::Reinforce => {
            let note_dir = env.notes_dir()?.join(note_id);
            let mut embedder = env.open_embedder()?;
            let r = feedback::reinforce_person(
                &note_dir, segs, filter, target, &vp, &mut embedder, &now, None, needs_rebuild, false,
            )?;
            eprintln!("feedback: note={note_id} target={target} result={r:?}");
            Ok(())
        }
    }
}

/// 反向同步:笔记里把某簇改派/解除时,旧人从这簇截下的样本退掉并重建旧人声纹(否则
/// 那段声音还留在旧人档案里参与识别——P11-5.wav 那种重复件就是这么来的)。只处理有
/// 溯源真值的样本;失败只记日志。
pub(crate) fn spawn_retire_samples(env: &dyn LinkEnv, person: String, note_id: String, cluster_id: String) {
    env.spawn(Box::new(move |env: &dyn LinkEnv| {
        let Ok(root) = env.root() else { return };
        let store = store::VoiceprintStore::new(root);
        // 复核(Codex P1):任务排队期间用户可能已把这簇改回给同一个人——那他的样本就
        // 不该退。只有簇此刻不再归他时才动手。
        let still_his = env
            .notes_dir()
            .ok()
            .and_then(|d| store::NoteStore::new(d).load(&note_id).ok())
            .and_then(|n| n.speakers.get(&cluster_id).and_then(|m| m.person_id.clone()))
            .and_then(|pid| store::VoiceprintStore::resolve(&store.load(), &pid).map(str::to_string))
            .is_some_and(|r| store::VoiceprintStore::resolve(&store.load(), &person).is_some_and(|p| p == r));
        if still_his {
            return;
        }
        let paths = store.samples_traced_to(&person, &note_id, &cluster_id);
        if paths.is_empty() {
            return;
        }
        // 已知残余(Codex 复审 P1,接受):复核与删除跨两个存储(笔记 / 声纹库),做不成
        // 一把锁内的原子;窗口是"复核通过后到删除这几毫秒内用户又改回",后果只是少一份
        // 样本(声纹随重建仍正确),不值得把两个锁嵌套起来。
        for p in &paths {
            if let Err(e) = store.delete_sample(&person, p) {
                eprintln!("退掉旧人样本失败({person} {}): {e}", p.display());
            }
        }
        eprintln!("笔记改派:退掉 {person} 来自 {note_id}/{cluster_id} 的 {} 份样本,重建声纹", paths.len());
        if let Err(e) = env.rebuild_person(&person) {
            eprintln!("退样本后重建 {person} 失败: {e}");
        }
    }));
}

#[cfg(test)]
#[path = "speaker_link_tests.rs"]
mod tests;
