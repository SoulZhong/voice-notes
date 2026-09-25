//! 识别建议的动作:AI 身份推断(identify)产出的建议,被采纳、拒绝、自动应用、回执确认、
//! 撤销,以及崩溃后的前滚恢复。
//!
//! 每次自动应用都先落一条意向记录(identify-ops.json 里的 `IdentifyOp`),按阶段
//! pending → assigned → reinforced → done 推进,撤销按 undo_pending → link_cleared → undone
//! 推进;崩溃由 [`recover_identify_ops`] 前滚。所有动作在 `IDENTIFY_ACT_GATE` 内串行,
//! 需要写库时再取 `FEEDBACK_GATE`(锁序恒 ACT → FEEDBACK)。
//!
//! 「这簇现在关联的是谁」一律查**说话人表**:#141「一波说话人」之后修订稿段落不再携带
//! 身份(段落 person_id 只是展示联表的产物),见 [`cluster_linked_person`]。
//!
//! 采纳建议的关联经 `speaker_link::link_speaker_with`,与手动关联同一入口。

use crate::speaker_link::{self, LinkEnv};
use crate::{feedback, ipc, occupancy, refine, store, tr, FEEDBACK_GATE, IDENTIFY_ACT_GATE};

/// 这篇笔记里 `speaker` 此刻关联的人物(经 redirects 归一)。无关联 → None。
pub(crate) fn cluster_linked_person(
    speakers: &std::collections::BTreeMap<String, store::SpeakerMeta>,
    vp: &store::Voiceprints,
    speaker: &str,
) -> Option<String> {
    speakers
        .get(speaker)
        .and_then(|m| m.person_id.as_deref())
        .and_then(|pid| store::VoiceprintStore::resolve(vp, pid))
        .map(str::to_string)
}

/// `speaker` 此刻是否关联着 `target`(两边都经 redirects 归一再比)。
fn cluster_linked_to(
    speakers: &std::collections::BTreeMap<String, store::SpeakerMeta>,
    vp: &store::Voiceprints,
    speaker: &str,
    target: &str,
) -> bool {
    let target = store::VoiceprintStore::resolve(vp, target).unwrap_or(target);
    cluster_linked_person(speakers, vp, speaker).as_deref() == Some(target)
}

/// P2b 操作 id:进程号 + 计数 + 时间戳,不求密码学强度,只求全局不重。
fn identify_op_id() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "iop-{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        chrono::Utc::now().timestamp_millis()
    )
}

/// P2b 自动应用单条(意向日志护航,调用方须已确认 identify_auto_apply 开):
/// 写序 pending→assign→assigned→同步回灌→reinforced→status→done,每步落盘,
/// 崩溃由 recover_identify_ops 前滚。不做 is_refining 守卫——只在 Aing 线程/
/// identify_note 线程内调用,彼时无并发编辑。锁序:IDENTIFY_ACT_GATE →
/// FEEDBACK_GATE,绝不反向。
pub(crate) fn auto_apply_one(env: &dyn LinkEnv, note_id: &str, fingerprint: &str) -> anyhow::Result<()> {
    let _gate = IDENTIFY_ACT_GATE.lock().unwrap();
    let root = env.notes_dir()?;
    let dir = root.join(note_id);
    let mut idoc = refine::identify::load_identify(&dir)
        .ok_or_else(|| anyhow::anyhow!("identify.json 缺失"))?;
    let doc = store::load_refined(&dir).ok_or_else(|| anyhow::anyhow!("精修稿缺失"))?;
    // 资格在锁内复核(auto_apply_targets 含"说话人当前无关联无手填名"的全部前置;
    // 一波说话人后前置查 speakers.json,须在锁内取新鲜表)。
    let fresh = store::NoteStore::new(root.clone()).load(note_id)?;
    let eligible = refine::identify::auto_apply_targets(&idoc, &doc, &fresh.speakers)
        .iter()
        .any(|a| a.fingerprint == fingerprint);
    anyhow::ensure!(eligible, "条目已不满足自动应用前置");
    let a = idoc
        .assignments
        .iter()
        .find(|a| a.fingerprint == fingerprint)
        .expect("eligible 已含存在性")
        .clone();
    let target = a.person_id.clone().expect("eligible 已含库内人前置");
    let vp_store = store::VoiceprintStore::new(env.root()?);
    let vp = vp_store.load();
    let Some(resolved) = store::VoiceprintStore::resolve(&vp, &target).map(str::to_string) else {
        anyhow::bail!("目标人物已不在库");
    };
    let name = vp.people.get(&resolved).map(|p| p.name.clone()).unwrap_or_default();
    let members = refine::identify::cluster_members_from_doc(&doc);
    let (speaker, seqs) = members
        .iter()
        .find(|(_, sq)| refine::identify::cluster_fingerprint(sq) == fingerprint)
        .map(|(sp, sq)| (sp.clone(), sq.clone()))
        .ok_or_else(|| anyhow::anyhow!("指纹已不对应任何簇"))?;

    // ① 意向记录先落盘(pending)。
    let now = chrono::Local::now().to_rfc3339();
    let op_id = identify_op_id();
    let mut ops = refine::identify::load_ops(&dir);
    ops.ops.push(refine::identify::IdentifyOp {
        op_id: op_id.clone(),
        fingerprint: fingerprint.to_string(),
        cluster: speaker.clone(),
        seqs: seqs.iter().copied().collect(),
        target_person: resolved.clone(),
        target_name: name.clone(),
        quote: a.evidence.first().map(|e| e.quote.clone()).unwrap_or_default(),
        quote_type: a.evidence.first().map(|e| e.r#type.clone()).unwrap_or_default(),
        created_at: now.clone(),
        stage: "pending".into(),
        acknowledged: false,
        reinforce_skipped: None,
        undo_stage: None,
        non_revertible: None,
    });
    refine::identify::save_ops(&dir, &ops)?;
    let set_stage = |dir: &std::path::Path, op_id: &str, stage: &str, skipped: Option<String>| {
        let mut ops = refine::identify::load_ops(dir);
        if let Some(op) = ops.ops.iter_mut().find(|o| o.op_id == op_id) {
            op.stage = stage.into();
            if skipped.is_some() {
                op.reinforce_skipped = skipped;
            }
        }
        let _ = refine::identify::save_ops(dir, &ops);
    };

    // ② 关联:一波说话人——写 speakers.json,CAS「当前未关联才写」(store 层自取
    // NoteLock;与资格复核间若被用户抢先关联,这里原子拒绝)。
    store::NoteStore::new(root.clone()).assign_speaker_person_if(note_id, &speaker, &resolved)?;
    set_stage(&dir, &op_id, "assigned", None);

    // ③ 回灌已摘(2026-08-27「确认才入库」全面推广,issue #166):自动应用是
    // LLM 推断的身份,没有用户确认动作——它可以替用户做**笔记内**关联(可撤销、
    // 建议卡上有痕),但不配写库。原同步 reinforce_person 调用整块移除;库写入
    // 只在用户亲手关联/确认时发生。stage 记 skipped 供审计与撤销语义对齐。
    let skipped: Option<String> = Some("确认才入库:自动应用不写库".to_string());
    set_stage(&dir, &op_id, "reinforced", skipped);

    // ④ 状态落盘 + done。
    refine::identify::mark_transition(&mut idoc, fingerprint, &["suggested"], "auto_applied", &now)?;
    refine::identify::save_identify(&dir, &idoc)?;
    set_stage(&dir, &op_id, "done", None);
    eprintln!("identify({note_id}): 已自动认出 {speaker} = {name}(op {op_id})");
    Ok(())
}

/// P2b 崩溃恢复:未完成 op 前滚(pending 且未见关联=放弃;assigned 起=补回灌/补状态)。
/// 在自动应用循环前、IDENTIFY_ACT_GATE 内由调用方间接串行(本函数自取锁)。
pub(crate) fn recover_identify_ops(env: &dyn LinkEnv, note_id: &str) {
    let run = || -> anyhow::Result<()> {
        let _gate = IDENTIFY_ACT_GATE.lock().unwrap();
        let root = env.notes_dir()?;
        let dir = root.join(note_id);
        let mut ops = refine::identify::load_ops(&dir);
        let pending: Vec<String> = ops
            .ops
            .iter()
            .filter(|o| {
                (o.stage != "done" && o.undo_stage.is_none())
                    || matches!(o.undo_stage.as_deref(), Some("undo_pending") | Some("link_cleared"))
            })
            .map(|o| o.op_id.clone())
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        let doc = store::load_refined(&dir).ok_or_else(|| anyhow::anyhow!("精修稿缺失"))?;
        let mut idoc = refine::identify::load_identify(&dir)
            .ok_or_else(|| anyhow::anyhow!("identify.json 缺失"))?;
        // 关联现状查说话人表(段落不带身份,见模块文档)。只有前滚判定要用;读不到时
        // 那几条 op 本轮先不动(下次再判),撤销中途的恢复不依赖它。
        let speakers = store::NoteStore::new(root.clone()).load(note_id).map(|n| n.speakers);
        if let Err(e) = &speakers {
            eprintln!("identify({note_id}): 说话人表读取失败,本轮只恢复撤销中途的 op: {e}");
        }
        let vp = store::VoiceprintStore::new(env.root()?).load();
        for op_id in pending {
            let Some(op) = ops.ops.iter_mut().find(|o| o.op_id == op_id) else { continue };
            // 撤销中途崩溃的恢复:undo_pending=实质动作未发生,回退到可撤状态;
            // link_cleared=关联已清,前滚完成质心还原与拒绝键(与撤销命令 ②③ 同)。
            match op.undo_stage.as_deref() {
                Some("undo_pending") => {
                    op.undo_stage = None;
                    continue;
                }
                Some("link_cleared") => {
                    let seqs: std::collections::BTreeSet<u64> =
                        op.seqs.iter().copied().collect();
                    if op.reinforce_skipped.is_none() {
                        if let Ok(vp_store) = env.root().map(store::VoiceprintStore::new) {
                            match feedback::undo_reinforce_op(
                                &dir,
                                &seqs,
                                &op.target_person,
                                &op.op_id,
                                &vp_store,
                            ) {
                                Ok(feedback::UndoOutcome::Restored) => {}
                                // 快照来自另一个模型空间:质心已置空,必须**当场**排重建。
                                // 早先这里只记日志,理由写的是"下次启动自愈会兜住"——
                                // 那是错的:restore_feedback 不改库标签,标签恒相等,
                                // 自愈的判据永远不成立(codex review 实现轮 P1)。
                                Ok(feedback::UndoOutcome::RestoredNeedsRebuild) => {
                                    eprintln!("identify 恢复:回灌快照来自另一空间,质心已置空,排重建");
                                    env.request_rebuild("identify 恢复后质心置空");
                                }
                                Ok(feedback::UndoOutcome::NoEntry) => {
                                    op.non_revertible
                                        .get_or_insert("ledger-lost(账本缺失,污染未回滚)".into());
                                }
                                Ok(feedback::UndoOutcome::NotRevertible(r)) => {
                                    op.non_revertible.get_or_insert(r.into());
                                }
                                Err(e) => {
                                    op.non_revertible.get_or_insert(format!("restore-error: {e}"));
                                }
                            }
                        }
                    }
                    if refine::identify::mark_rejected(&mut idoc, &op.fingerprint, &op.created_at)
                        .is_err()
                    {
                        // 条目已被新一轮吞掉:拒绝键直接落(同目标不再建议)。
                        idoc.rejected.insert(
                            refine::identify::rejected_key(&op.fingerprint, &op.target_person),
                            op.created_at.clone(),
                        );
                    }
                    op.undo_stage = Some("undone".into());
                    continue;
                }
                _ => {}
            }
            let Ok(speakers) = &speakers else { continue };
            let seqs: std::collections::BTreeSet<u64> = op.seqs.iter().copied().collect();
            let linked_to_target = refine::identify::cluster_members_from_doc(&doc)
                .iter()
                .find(|(_, sq)| **sq == seqs)
                .is_some_and(|(sp, _)| cluster_linked_to(speakers, &vp, sp, &op.target_person));
            if op.stage == "pending" && !linked_to_target {
                // assign 未发生:放弃,建议卡还在,无痕。
                op.stage = "aborted".into();
                op.undo_stage = Some("undone".into());
                continue;
            }
            // assigned/reinforced(或 pending 但已见关联):前滚补状态;回灌交给
            // 幂等账本(同 scope 已有条目则 reinforce 已发生;缺则标 skipped——
            // 崩溃点在回灌中途时宁可少灌,绝不重复加权)。
            if op.stage == "pending" || op.stage == "assigned" {
                op.reinforce_skipped
                    .get_or_insert("crash-before-reinforce(未回灌,宁缺勿重)".into());
                op.stage = "reinforced".into();
            }
            if refine::identify::mark_transition(
                &mut idoc,
                &op.fingerprint,
                &["suggested"],
                "auto_applied",
                &op.created_at,
            )
            .is_err()
                && !idoc.assignments.iter().any(|a| a.fingerprint == op.fingerprint)
            {
                // 条目已被新一轮吞掉:按 op 记录合成回执条目,撤销入口不丢。
                idoc.assignments.push(refine::identify::IdentifyAssignment {
                    fingerprint: op.fingerprint.clone(),
                    cluster: op.cluster.clone(),
                    person_id: Some(op.target_person.clone()),
                    new_name: None,
                    tier: refine::identify::Tier::High,
                    llm_confidence: "high".into(),
                    acoustic: None,
                    acoustic_z: None,
                    evidence: vec![],
                    status: "auto_applied".into(),
                    decided_at: Some(op.created_at.clone()),
                });
            }
            op.stage = "done".into();
        }
        refine::identify::save_identify(&dir, &idoc)?;
        refine::identify::save_ops(&dir, &ops)?;
        Ok(())
    };
    if let Err(e) = run() {
        eprintln!("identify({note_id}): op 恢复失败(忽略): {e}");
    }
}

/// 收件箱身份建议列表:扫各笔记 identify.json,只收 status=suggested 且「新鲜」
/// (source_hash 与现稿一致、指纹仍对应某簇、该簇当前无关联、目标人仍在库)。
pub(crate) fn list_identify_suggestions_with(env: &dyn LinkEnv) -> Result<Vec<ipc::IdentifySuggestion>, String> {
    let root = env.notes_dir().map_err(|e| e.to_string())?;
    let vp = store::VoiceprintStore::new(env.root().map_err(|e| e.to_string())?).load();
    let nstore = store::NoteStore::new(root.clone());
    let notes = nstore.list();
    // 每篇的说话人表只读一次(建议与回执两轮共用)。Err = 读取失败,与「无关联」区分。
    let mut speakers_cache: std::collections::HashMap<String, Result<std::collections::BTreeMap<String, store::SpeakerMeta>, String>> =
        Default::default();
    let mut speakers_of = |id: &str| {
        speakers_cache
            .entry(id.to_string())
            .or_insert_with(|| nstore.load(id).map(|nt| nt.speakers).map_err(|e| e.to_string()))
            .clone()
    };
    let mut out: Vec<ipc::IdentifySuggestion> = Vec::new();
    for n in &notes {
        let dir = root.join(&n.id);
        let Some(idoc) = refine::identify::load_identify(&dir) else { continue };
        if idoc.assignments.iter().all(|a| a.status != "suggested") {
            continue;
        }
        let Some(doc) = store::load_refined(&dir) else { continue };
        if store::source_hash(&doc.paragraphs) != idoc.source_hash {
            continue; // 稿已被精修/编辑,证据锚点不可信:等下轮 identify 重建
        }
        let members = refine::identify::cluster_members_from_doc(&doc);
        let fp_to_speaker: std::collections::BTreeMap<String, String> = members
            .iter()
            .map(|(sp, seqs)| (refine::identify::cluster_fingerprint(seqs), sp.clone()))
            .collect();
        // 已关联的说话人查说话人表(段落不带身份,见模块文档)。读不到就跳过这篇:
        // 当成「无关联」会把已关联的簇的过期建议又摆出来。
        let speakers = match speakers_of(&n.id) {
            Ok(sp) => sp,
            Err(e) => {
                eprintln!("识别建议:{} 说话人表读取失败,本篇建议暂不展示: {e}", n.id);
                continue;
            }
        };
        let linked: std::collections::BTreeSet<&str> = speakers
            .iter()
            .filter(|(_, m)| m.person_id.is_some())
            .map(|(sid, _)| sid.as_str())
            .collect();
        for a in idoc.assignments.iter().filter(|a| a.status == "suggested") {
            let Some(speaker) = fp_to_speaker.get(&a.fingerprint) else { continue };
            if linked.contains(speaker.as_str()) {
                continue; // 用户已手动关联,不再打扰
            }
            let (person_id, person_name, is_new) = match (&a.person_id, &a.new_name) {
                (Some(pid), _) => {
                    let Some(rid) = store::VoiceprintStore::resolve(&vp, pid) else { continue };
                    let name = vp.people.get(rid).map(|p| p.name.clone()).unwrap_or_default();
                    if name.trim().is_empty() {
                        continue;
                    }
                    (Some(rid.to_string()), name, false)
                }
                (None, Some(nn)) => (None, nn.clone(), true),
                _ => continue,
            };
            let ev = a.evidence.first();
            out.push(ipc::IdentifySuggestion {
                note_id: n.id.clone(),
                note_title: n.title.clone(),
                cluster: speaker.clone(),
                fingerprint: a.fingerprint.clone(),
                person_id,
                person_name,
                is_new,
                tier: match a.tier {
                    refine::identify::Tier::High => "high",
                    refine::identify::Tier::Medium => "medium",
                    refine::identify::Tier::Low => "low",
                }
                .into(),
                quote: ev.map(|e| e.quote.clone()).unwrap_or_default(),
                evidence_type: ev.map(|e| e.r#type.clone()).unwrap_or_default(),
                generated_at: idoc.generated_at.clone(),
                status: "suggested".into(),
                op_id: None,
                revertible: true,
            });
        }
    }
    out.sort_by(|a, b| b.generated_at.cmp(&a.generated_at));
    out.truncate(50);

    // P2b 自动回执:渲染自意向日志(永续可见,不受新鲜度与 50 条上限约束——
    // 撤销入口不能因稿变化/淘汰而消失);revertible=簇仍可按指纹定位且关联仍是
    // 自动目标(否则冲突态只留「好」)。
    let mut receipts: Vec<ipc::IdentifySuggestion> = Vec::new();
    for n in &notes {
        let dir = root.join(&n.id);
        let ops = refine::identify::load_ops(&dir);
        let pending: Vec<_> = ops
            .ops
            .iter()
            .filter(|o| o.stage == "done" && !o.acknowledged && o.undo_stage.is_none())
            .collect();
        if pending.is_empty() {
            continue;
        }
        let doc = store::load_refined(&dir);
        // 回执永续可见(撤销入口不能丢);说话人表读不到时照常展示,但不给撤销——
        // 拿不准现关联时撤销可能覆盖用户的改动。
        let speakers = speakers_of(&n.id).unwrap_or_else(|e| {
            eprintln!("识别回执:{} 说话人表读取失败,本次不提供撤销: {e}", n.id);
            Default::default()
        });
        for op in pending {
            let revertible = doc.as_ref().is_some_and(|d| {
                let seqs: std::collections::BTreeSet<u64> = op.seqs.iter().copied().collect();
                refine::identify::cluster_members_from_doc(d)
                    .iter()
                    .find(|(_, sq)| **sq == seqs)
                    .is_some_and(|(sp, _)| cluster_linked_to(&speakers, &vp, sp, &op.target_person))
            });
            let name = store::VoiceprintStore::resolve(&vp, &op.target_person)
                .and_then(|rid| vp.people.get(rid))
                .map(|p| p.name.clone())
                .filter(|nm| !nm.trim().is_empty())
                .unwrap_or_else(|| op.target_name.clone());
            receipts.push(ipc::IdentifySuggestion {
                note_id: n.id.clone(),
                note_title: n.title.clone(),
                cluster: op.cluster.clone(),
                fingerprint: op.fingerprint.clone(),
                person_id: Some(op.target_person.clone()),
                person_name: name,
                is_new: false,
                tier: "high".into(),
                quote: op.quote.clone(),
                evidence_type: if op.quote_type.is_empty() { "self_intro".into() } else { op.quote_type.clone() },
                generated_at: op.created_at.clone(),
                status: "auto_applied".into(),
                op_id: Some(op.op_id.clone()),
                revertible,
            });
        }
    }
    receipts.sort_by(|a, b| b.generated_at.cmp(&a.generated_at));
    receipts.extend(out);
    Ok(receipts)
}

/// P2b 回执「好」:确认自动认人(回执消失);identify.json 状态 auto_applied→applied
/// best-effort(稿重生成后条目可能不在,确认动作以 op 记录为准)。
pub(crate) fn acknowledge_identify_with(env: &dyn LinkEnv, note_id: String, op_id: String) -> Result<(), String> {
    store::validate_note_id(&note_id).map_err(|e| e.to_string())?;
    let _gate = IDENTIFY_ACT_GATE.lock().unwrap();
    let dir = env.notes_dir().map_err(|e| e.to_string())?.join(&note_id);
    let mut ops = refine::identify::load_ops(&dir);
    let op = ops
        .ops
        .iter_mut()
        .find(|o| o.op_id == op_id && o.stage == "done" && o.undo_stage.is_none())
        .ok_or_else(|| tr!("回执不存在或已处理", "Receipt missing or already handled"))?;
    op.acknowledged = true;
    let fp = op.fingerprint.clone();
    let ack_seqs: std::collections::BTreeSet<u64> = op.seqs.iter().copied().collect();
    let ack_target = op.target_person.clone();
    refine::identify::save_ops(&dir, &ops).map_err(|e| e.to_string())?;
    if let Some(mut idoc) = refine::identify::load_identify(&dir) {
        let now = chrono::Local::now().to_rfc3339();
        if refine::identify::mark_transition(&mut idoc, &fp, &["auto_applied"], "applied", &now).is_ok() {
            let _ = refine::identify::save_identify(&dir, &idoc);
        }
    }
    // 「Good」就是用户确认(codex,确认才入库的写入边界):此刻补做自动应用时刻意
    // 跳过的回灌——与手动关联的库待遇对齐。后台执行,门内复核撤销态。
    spawn_ack_reinforce(env, note_id, op_id, ack_seqs, ack_target);
    Ok(())
}

/// 回执确认后的回灌:锁序 IDENTIFY_ACT_GATE → FEEDBACK_GATE(与 auto_apply_one
/// 一致);门内复核 op 仍是「已确认且未撤销」——确认与本任务之间用户可能已点撤销,
/// 撤销后再回灌就是把解除掉的关联偷偷写回库(codex review 二轮 P1#3 同款教训)。
fn spawn_ack_reinforce(
    env: &dyn LinkEnv,
    note_id: String,
    op_id: String,
    seqs: std::collections::BTreeSet<u64>,
    target: String,
) {
    env.spawn(Box::new(move |env: &dyn LinkEnv| {
        let run = || -> anyhow::Result<()> {
            let root = env.notes_dir()?;
            let dir = root.join(&note_id);
            let _gate = IDENTIFY_ACT_GATE.lock().unwrap();
            let ops = refine::identify::load_ops(&dir);
            let Some(op) =
                ops.ops.iter().find(|o| o.op_id == op_id && o.acknowledged && o.undo_stage.is_none())
            else {
                return Ok(()); // 已撤销/状态变了:不写库
            };
            anyhow::ensure!(op.target_person == target, "op 目标已变,放弃回灌");
            let op_cluster = op.cluster.clone();
            let vp_store = store::VoiceprintStore::new(env.root()?);
            let now = chrono::Local::now().to_rfc3339();
            let skipped = {
                let _fb = FEEDBACK_GATE.lock().unwrap();
                let note = store::NoteStore::new(root.clone()).load(&note_id)?;
                // 门内复核关联现状(codex P1):确认与本任务之间用户可能已把该说话人
                // 手动改给别人——op 记录还在,但笔记里的关联已不是 target,此刻回灌
                // 会跟更新的手动 feedback 抢写,把错人留在库里。现关联≠target 即放弃。
                let vp_now = vp_store.load();
                // target 先过 redirects 归一(codex 六轮):自动应用到用户点 Good 之间
                // 目标人物可能已被合并,笔记侧关联解析到的是 winner,拿 loser 比对会
                // 白白放弃用户的确认。
                let Some(target) =
                    store::VoiceprintStore::resolve(&vp_now, &target).map(str::to_string)
                else {
                    eprintln!("回执确认回灌放弃:目标人物已不在库");
                    return Ok(());
                };
                // seqs 归属复核(codex 五/六轮):重聚类会换簇号,op.cluster 可能已
                // 过期——现簇从 seqs 反推(撤销路径同款思路):全部段必须存在且归
                // 同一个说话人,否则组已散,放弃不拿混料喂库。
                let owners: std::collections::BTreeSet<&str> = seqs
                    .iter()
                    .filter_map(|q| {
                        note.segments.iter().find(|s| s.seq == *q).and_then(|s| s.speaker.as_deref())
                    })
                    .collect();
                let all_present = seqs
                    .iter()
                    .all(|q| note.segments.iter().any(|s| s.seq == *q && s.speaker.is_some()));
                let Some(cur_cluster) = (if all_present && owners.len() == 1 {
                    owners.iter().next().map(|s| s.to_string())
                } else {
                    None
                }) else {
                    eprintln!("回执确认回灌放弃:op 覆盖的段已散/被改派");
                    return Ok(());
                };
                let _ = &op_cluster; // 旧簇号仅供日志,判定一律以 seqs 反推为准
                // 现关联复核(codex 五轮 P1):确认与本任务之间用户可能已把该说话人
                // 改给别人;现关联(归一后)≠target 即放弃,不与更新的 feedback 抢写。
                let cur = note
                    .speakers
                    .get(&cur_cluster)
                    .and_then(|m| m.person_id.as_deref())
                    .and_then(|pid| store::VoiceprintStore::resolve(&vp_now, pid));
                if cur != Some(target.as_str()) {
                    eprintln!("回执确认回灌放弃:说话人现关联({cur:?})已不是 {target}");
                    return Ok(());
                }
                match env.open_embedder() {
                    Ok(mut embedder) => {
                        let mut needs_rebuild = false;
                        let r = feedback::reinforce_person(
                            &dir,
                            &note.segments,
                            &feedback::SegFilter::Seqs(seqs.clone()),
                            &target,
                            &vp_store,
                            &mut embedder,
                            &now,
                            Some(&op_id),
                            &mut needs_rebuild,
                            false,
                        );
                        if needs_rebuild {
                            env.request_rebuild("回执确认回灌触发质心置空");
                        }
                        let sk = match r {
                            Ok(feedback::ReinforceResult::Applied { .. }) => None,
                            Ok(other) => Some(format!("{other:?}")),
                            Err(e) => Some(format!("回灌失败: {e}")),
                        };
                        (sk, target, cur_cluster)
                    }
                    Err(e) => (Some(format!("声纹模型不可用: {e}")), target, cur_cluster),
                }
            };
            let (skipped, target, cur_cluster) = skipped;
            let mut ops = refine::identify::load_ops(&dir);
            if let Some(op) = ops.ops.iter_mut().find(|o| o.op_id == op_id) {
                op.reinforce_skipped = skipped.or(Some("回执确认后已回灌".to_string()));
            }
            let _ = refine::identify::save_ops(&dir, &ops);
            // 确认样本(codex 三轮):被确认目标若零样本,换模型 rebuild 会把人清没。
            // 料 = 本 op 覆盖的段(用户「Good」确认的正是这组识别),凑长同一口径。
            // 簇号/目标一律用门内复核出的现值(codex 六轮:旧簇号/被合并的旧 id
            // 会让复核白白失败,吞掉用户的确认)。
            {
                let note = store::NoteStore::new(root.clone()).load(&note_id)?;
                let pool: Vec<store::SegmentRecord> =
                    note.segments.iter().filter(|s| seqs.contains(&s.seq)).cloned().collect();
                let picks = speaker_link::pick_confirmed_sample_segs(&pool);
                speaker_link::spawn_confirmed_sample(
                    env,
                    note_id.clone(),
                    cur_cluster,
                    target,
                    note.segments,
                    picks,
                    false,
                );
            }
            Ok(())
        };
        if let Err(e) = run() {
            eprintln!("回执确认回灌失败(关联不受影响): {e}");
        }
    }));
}

/// P2b 回执「撤销」:CAS 解除关联 + 按 op 对账还原质心 + 拒绝键。返回质心是否
/// 还原(false=已被后续写动过,关联已解除但声纹保留,前端如实提示)。
/// 调用方自备 id 校验与准入(命令壳在切到阻塞线程池之前做,与原实现同时机)。
pub(crate) fn undo_identify_apply_with(env: &dyn LinkEnv, note_id: String, op_id: String) -> Result<bool, String> {
    {
        let _gate = IDENTIFY_ACT_GATE.lock().unwrap();
        let root = env.notes_dir().map_err(|e| e.to_string())?;
        let dir = root.join(&note_id);
        let mut ops = refine::identify::load_ops(&dir);
        let idx = ops
            .ops
            .iter()
            .position(|o| o.op_id == op_id && o.stage == "done" && !o.acknowledged)
            .ok_or_else(|| tr!("回执不存在或已处理", "Receipt missing or already handled"))?;
        let (seqs, target, fp, reinforced) = {
            let op = &mut ops.ops[idx];
            if op.undo_stage.as_deref() == Some("undone") {
                return Ok(op.non_revertible.is_none()); // 幂等重入
            }
            op.undo_stage = Some("undo_pending".into());
            (
                op.seqs.iter().copied().collect::<std::collections::BTreeSet<u64>>(),
                op.target_person.clone(),
                op.fingerprint.clone(),
                op.reinforce_skipped.is_none(),
            )
        };
        refine::identify::save_ops(&dir, &ops).map_err(|e| e.to_string())?;

        // ① 定位当前簇并 CAS 解除关联(用户已手改则拒绝并回退 undo 状态)。
        let doc = store::load_refined(&dir)
            .ok_or_else(|| tr!("精修稿缺失", "Refined doc missing"))?;
        let speaker = refine::identify::cluster_members_from_doc(&doc)
            .iter()
            .find(|(_, sq)| **sq == seqs)
            .map(|(sp, _)| sp.clone());
        let Some(speaker) = speaker else {
            ops.ops[idx].undo_stage = None;
            ops.ops[idx].non_revertible = Some("clusters-changed".into());
            let _ = refine::identify::save_ops(&dir, &ops);
            return Err(tr!(
                "说话人分组已变化,无法自动撤销(可手动改指认)",
                "Speaker clusters changed; cannot auto-undo (reassign manually)"
            ));
        };
        if let Err(e) =
            store::NoteStore::new(root.clone()).clear_speaker_person_if(&note_id, &speaker, &target)
        {
            ops.ops[idx].undo_stage = None;
            let _ = refine::identify::save_ops(&dir, &ops);
            return Err(e.to_string());
        }
        ops.ops[idx].undo_stage = Some("link_cleared".into());
        refine::identify::save_ops(&dir, &ops).map_err(|e| e.to_string())?;

        // ② 质心还原(按 op 对账;不可还原如实记录,绝不错撤后续人工作业)。
        // FEEDBACK_GATE:与人工回灌对同一账本/人物快照的读改写互斥
        // (锁序恒 IDENTIFY_ACT_GATE → FEEDBACK_GATE)。
        let vp_store = store::VoiceprintStore::new(env.root().map_err(|e| e.to_string())?);
        let restored = {
            let _fb = FEEDBACK_GATE.lock().unwrap();
            match feedback::undo_reinforce_op(&dir, &seqs, &target, &op_id, &vp_store) {
                Ok(feedback::UndoOutcome::Restored) => true,
                Ok(feedback::UndoOutcome::RestoredNeedsRebuild) => {
                    // 撤销成功,但质心因跨空间被置空 → 排一次重建把这个人从样本长回来。
                    // 放在门内是安全的:排重建只是起线程,不取 vp_guard。
                    env.request_rebuild("撤销回灌后质心置空");
                    true
                }
                Ok(feedback::UndoOutcome::NoEntry) if !reinforced => true, // 未曾回灌=无污染
                Ok(feedback::UndoOutcome::NoEntry) => {
                    // op 声称回灌过而账本无条:账丢了,污染无法回滚,如实报。
                    ops.ops[idx].non_revertible = Some("ledger-lost".into());
                    false
                }
                Ok(feedback::UndoOutcome::NotRevertible(reason)) => {
                    ops.ops[idx].non_revertible = Some(reason.into());
                    false
                }
                Err(e) => {
                    ops.ops[idx].non_revertible = Some(format!("restore-error: {e}"));
                    false
                }
            }
        };

        // ③ 状态与拒绝键(同目标不再建议)+ undone。
        if let Some(mut idoc) = refine::identify::load_identify(&dir) {
            let now = chrono::Local::now().to_rfc3339();
            if refine::identify::mark_rejected(&mut idoc, &fp, &now).is_err() {
                // 条目已被新一轮吞掉:拒绝键直接落,同目标不再建议。
                idoc.rejected
                    .insert(refine::identify::rejected_key(&fp, &target), now.clone());
            }
            let _ = refine::identify::save_identify(&dir, &idoc);
        }
        ops.ops[idx].undo_stage = Some("undone".into());
        refine::identify::save_ops(&dir, &ops).map_err(|e| e.to_string())?;
        Ok(restored)
    }
}

/// 确认身份建议:锁外走 do_assign_refined_person(内部自取 NoteLock,不嵌套);
/// 新面孔先建档,assign 失败补偿删除空档案;成功后回写 status=applied。
pub(crate) fn apply_identify_suggestion_with(
    env: &dyn LinkEnv,
    note_id: String,
    fingerprint: String,
) -> Result<(), String> {
    store::validate_note_id(&note_id).map_err(|e| e.to_string())?;
    {
        let _gate = IDENTIFY_ACT_GATE.lock().unwrap();
        let dir = env.notes_dir().map_err(|e| e.to_string())?.join(&note_id);
        let mut idoc = refine::identify::load_identify(&dir)
            .ok_or_else(|| tr!("建议已失效", "Suggestion no longer valid"))?;
        let a = idoc
            .assignments
            .iter()
            .find(|a| a.fingerprint == fingerprint && a.status == "suggested")
            .ok_or_else(|| tr!("建议已失效", "Suggestion no longer valid"))?
            .clone();
        // 指纹复核:现稿仍有该成员集的簇。R 号可以变(重聚类重编号),成员集不能变。
        let doc = store::load_refined(&dir)
            .ok_or_else(|| tr!("精修稿缺失", "Refined doc missing"))?;
        let members = refine::identify::cluster_members_from_doc(&doc);
        let speaker = members
            .iter()
            .find(|(_, seqs)| refine::identify::cluster_fingerprint(seqs) == fingerprint)
            .map(|(sp, _)| sp.clone());
        let now = chrono::Local::now().to_rfc3339();
        let Some(speaker) = speaker else {
            let _ = refine::identify::mark_rejected(&mut idoc, &fingerprint, &now);
            let _ = refine::identify::save_identify(&dir, &idoc);
            return Err(tr!(
                "建议已过期(说话人分组已变化)",
                "Suggestion expired (speaker clusters changed)"
            ));
        };
        let vp_store = store::VoiceprintStore::new(env.root().map_err(|e| e.to_string())?);
        let (target, created) = match (&a.person_id, &a.new_name) {
            (Some(pid), _) => (pid.clone(), false),
            (None, Some(nn)) => (vp_store.create_person(nn, &now).map_err(|e| e.to_string())?, true),
            _ => return Err(tr!("建议数据异常", "Corrupt suggestion")),
        };
        // 录制中拒绝(speakers.json 由 writer 独占;与手动关联命令同守卫)。
        if let Err(e) = env
            .admit(&note_id, occupancy::Intent::Edit)
            .and_then(|_| speaker_link::link_speaker_with(env, &note_id, &speaker, &target, None, &[]))
        {
            if created {
                let _ = vp_store.delete_person_if_empty(&target);
            }
            return Err(e);
        }
        // P3:参会人邮箱记录——三重唯一性防线(目标人名与某参会人名精确相等、
        // 该名在参会人中唯一、该名在库中唯一)防同名污染;残余风险=确认本身指错。
        if let Ok(note) = store::NoteStore::new(env.notes_dir().map_err(|e| e.to_string())?).load(&note_id) {
            if let Some(cal) = &note.meta.calendar {
                let vp_now = vp_store.load();
                if let Some(target_name) =
                    store::VoiceprintStore::resolve(&vp_now, &target).and_then(|rid| vp_now.people.get(rid)).map(|p| p.name.clone())
                {
                    let hits: Vec<_> = cal
                        .attendees
                        .iter()
                        .filter(|a| !a.email.is_empty() && a.name == target_name)
                        .collect();
                    let library_unique =
                        vp_now.people.values().filter(|p| p.name == target_name).count() == 1;
                    if hits.len() == 1 && library_unique && !target_name.trim().is_empty() {
                        if let Err(e) = vp_store.add_person_email(&target, &hits[0].email) {
                            eprintln!("calendar: 记录参会人邮箱失败(忽略): {e}");
                        }
                    }
                }
            }
        }
        refine::identify::mark_applied(&mut idoc, &fingerprint, &now).map_err(|e| e.to_string())?;
        refine::identify::save_identify(&dir, &idoc).map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// 拒绝身份建议:status=rejected + 拒绝表记「指纹|目标」——同目标永不再建议,
/// 其它候选不受影响。走后端真值,不用前端 dismissed 字符串名单。
pub(crate) fn reject_identify_suggestion_with(
    env: &dyn LinkEnv,
    note_id: String,
    fingerprint: String,
) -> Result<(), String> {
    store::validate_note_id(&note_id).map_err(|e| e.to_string())?;
    let _gate = IDENTIFY_ACT_GATE.lock().unwrap();
    let dir = env.notes_dir().map_err(|e| e.to_string())?.join(&note_id);
    let mut idoc = refine::identify::load_identify(&dir)
        .ok_or_else(|| tr!("建议已失效", "Suggestion no longer valid"))?;
    let now = chrono::Local::now().to_rfc3339();
    refine::identify::mark_rejected(&mut idoc, &fingerprint, &now).map_err(|e| e.to_string())?;
    refine::identify::save_identify(&dir, &idoc).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
#[path = "identify_actions_tests.rs"]
mod tests;
