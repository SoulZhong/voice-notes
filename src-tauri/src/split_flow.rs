//! 拆分状态机:打标 → 样本处置 → 残留 → 分组提交 → 解除隔离,可从任一阶段断点续跑。
//!
//! 对外界的全部依赖经 [`SplitEnv`] 注入(生产实现是 lib.rs 的 `TauriSplitEnv`);
//! Tauri 命令壳留在 lib.rs,只负责建 env、切到阻塞线程池。整链行为由
//! split_flow_tests.rs 用假 env + 临时目录里的真实笔记与声纹库钉住。
//!
//! 阶段与持久化见 store/split_ops.rs;设计见
//! docs/superpowers/specs/2026-08-20-mixed-speaker-split-design.md 与
//! 2026-08-22-one-click-split-design.md。

use crate::{diar, feedback, lifecycle, occupancy, refine, store, tr, FEEDBACK_GATE, IDENTIFY_ACT_GATE};
use std::path::PathBuf;

/// 拆分状态机(打标 → 样本处置 → 残留 → 分组提交 → 解除隔离,可从任一阶段断点续跑)
/// 对外界的全部依赖。生产实现是 [`TauriSplitEnv`];测试用假实现驱动整个状态机
/// (见 split_flow_tests.rs),不需要 AppHandle、lifecycle actor 或真实嵌入模型。
///
/// 进程级的门(IDENTIFY_ACT_GATE / FEEDBACK_GATE / split_op_lock)不在这里:它们守的是
/// 本进程内的并发,与"外界是谁"无关。重建单飞例外——它与全库重建共用,测试并行跑时
/// 必须各用各的,所以经 env 走。
pub(crate) trait SplitEnv: Send + Sync {
    /// app_data_dir(声纹库、split_ops 所在)。
    fn root(&self) -> anyhow::Result<PathBuf>;
    /// 笔记根目录。
    fn notes_dir(&self) -> anyhow::Result<PathBuf>;
    /// 笔记侧编辑(生产经 lifecycle actor 串行,持 NoteLock)。
    fn edit_note(&self, op: lifecycle::machine::EditOp) -> Result<(), String>;
    /// 命令入口准入(见 occupancy.rs)。
    fn admit(&self, note_id: &str, intent: occupancy::Intent) -> Result<(), String>;
    /// 按当前选型建嵌入器(标签与权重同源)。
    fn open_embedder(&self) -> anyhow::Result<diar::TaggedEmbedder>;
    /// 指定空间的声纹种子(拆分分组给去处建议用)。
    fn seeds_for(&self, tag: &str) -> Vec<diar::registry::SeedCluster>;
    /// 抢占重建单飞(与全库重建互斥);false = 已有重建在跑。
    fn begin_exclusive_rebuild(&self) -> bool;
    fn end_exclusive_rebuild(&self);
    /// 消化排队中的全库重建(解除隔离之后调:人物还隔离着时全库重建会清空刚算的基线)。
    fn consume_pending_rebuild(&self);
    /// 回灌纠错把某人质心清空了:丢弃常驻嵌入器并排一次全库重建。
    fn request_rebuild(&self, reason: &'static str);
    /// 整个 op 收尾(DONE 之前):刷热词缓存;拆分模式另排人物图谱重建。`root` 由调用方给
    /// (就是这次收尾读写 split_ops 的那个目录),不在这里重取——重取会与收尾所用的目录分叉。
    fn on_split_done(&self, root: &std::path::Path, split_commit: bool) -> Result<(), String>;
    /// 分组嵌入进度(大簇要算数分钟,前端靠它区分「在算」与「卡死」)。
    fn split_progress(&self, note_id: &str, done: usize, total: usize);
}

pub(crate) fn mark_speaker_multi_with(
    env: &dyn SplitEnv,
    note_id: String,
    speaker_ids: Vec<String>,
) -> Result<String, String> {
    store::validate_note_id(&note_id).map_err(|e| e.to_string())?;
    if speaker_ids.is_empty() {
        return Err(tr!("没有选择说话人", "No speaker selected"));
    }
    let root = env.root().map_err(|e| e.to_string())?;
    let nroot = env.notes_dir().map_err(|e| e.to_string())?;
    let note = store::NoteStore::new(nroot.clone()).load(&note_id).map_err(|e| e.to_string())?;
    for sid in &speaker_ids {
        if !note.speakers.contains_key(sid) {
            return Err(tr!("笔记中没有该说话人: {sid}", "No such speaker in this note: {sid}", sid = sid));
        }
    }
    let vp_store = store::VoiceprintStore::new(root.clone());
    let now = chrono::Local::now().to_rfc3339();
    let marked_seqs: std::collections::BTreeSet<u64> = note
        .segments
        .iter()
        .filter(|s| s.speaker.as_deref().is_some_and(|sp| speaker_ids.iter().any(|x| x == sp)))
        .map(|s| s.seq)
        .collect();
    let dir = nroot.join(&note_id);
    // **先持 IDENTIFY_ACT_GATE,罩住计划+隔离+作废全程**:auto_apply 持同一把门做
    // 关联+回灌,不前置的话它可以插在"计划算完(目标还是 suggested,没进 affected)"
    // 与"作废落盘"之间完成回灌——混杂段进了一个没被隔离的人(codex 实现轮二 P1②)。
    // 锁序恒 IDENTIFY_ACT_GATE → vp_guard/actor,与 auto_apply_one 同向。
    let _act_gate = IDENTIFY_ACT_GATE.lock().unwrap();
    let members = store::load_refined(&dir)
        .map(|doc| refine::identify::cluster_members_from_doc(&doc))
        .unwrap_or_default();
    // ── plan + 受影响人物 + 隔离:同一 vp_guard 内原子完成(解析与置位之间不许插入
    //    合并;plan 先于隔离落盘)。复用既有 plan 阶段的 op(上次卡在后半程)。 ──
    static SPLIT_OP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // 时间戳进 id:PID 会复用、计数器重启归零,只有 pid+计数在重启后能撞上旧 op 并
    // 覆盖它(codex 实现轮二 P1⑦);create() 的存在性检查是最后一道闸。
    let candidate_op_id = format!(
        "so-{}-{}-{}",
        chrono::Local::now().format("%Y%m%d%H%M%S%3f"),
        std::process::id(),
        SPLIT_OP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let op_id = vp_store
        .with_guard(|| {
            let mut sorted_ids = speaker_ids.clone();
            sorted_ids.sort();
            // 复用:同笔记同说话人集合、卡在 plan 的 op。
            let existing = store::split_ops::open_ops_for_note(&root, &note_id)
                .into_iter()
                .find(|o| {
                    let mut a = o.speaker_ids.clone();
                    a.sort();
                    o.phase == store::split_ops::phase::PLAN && a == sorted_ids
                });
            let vp = vp_store.load();
            let mut affected: std::collections::BTreeSet<String> = Default::default();
            for sid in &speaker_ids {
                if let Some(pid) = note.speakers.get(sid).and_then(|m| m.person_id.as_deref()) {
                    if let Some(r) = store::VoiceprintStore::resolve(&vp, pid) {
                        affected.insert(r.to_string());
                    }
                }
            }
            for r in store::sample_trace::read_centroid_receipts(&root) {
                if r.note_id == note_id && speaker_ids.iter().any(|s| s == &r.cluster_id) {
                    if let Some(p) = store::VoiceprintStore::resolve(&vp, &r.resolved_person) {
                        affected.insert(p.to_string());
                    }
                }
            }
            // identify 自动应用过、且簇成员与被标段重叠的人物:混杂段已被回灌进去,
            // 一并隔离(codex 实现轮一 P1⑬)。
            if let Some(idoc) = refine::identify::load_identify(&dir) {
                for a in &idoc.assignments {
                    if a.status != "auto_applied" && a.status != "applied" {
                        continue;
                    }
                    let overlap = members
                        .get(&a.cluster)
                        .is_some_and(|m| m.intersection(&marked_seqs).next().is_some());
                    if overlap {
                        if let Some(pid) = a.person_id.as_deref() {
                            if let Some(r) = store::VoiceprintStore::resolve(&vp, pid) {
                                affected.insert(r.to_string());
                            }
                        }
                    }
                }
            }
            let op = match existing {
                Some(mut o) => {
                    // 并集,不覆盖:上次已执行的 SetMultiSpeaker 清掉了 person_id,
                    // 这次重算会看不到那些人——缩小集合等于把已隔离的人永久遗弃
                    // (codex 实现轮二 P1①)。
                    for p in &o.affected_persons {
                        affected.insert(p.clone());
                    }
                    o.affected_persons = affected.iter().cloned().collect();
                    o.updated_at = now.clone();
                    store::split_ops::save(&root, &o)?;
                    o
                }
                None => {
                    // 撤销要恢复的人物关联快照:SetMultiSpeaker 马上会清掉 person_id,
                    // 此刻不记就没了(undo_auto_split 只动本篇表项,不触库)。
                    let mut prior_links: std::collections::BTreeMap<String, String> =
                        Default::default();
                    for sid in &speaker_ids {
                        if let Some(pid) = note.speakers.get(sid).and_then(|m| m.person_id.as_deref())
                        {
                            if let Some(r) = store::VoiceprintStore::resolve(&vp, pid) {
                                prior_links.insert(sid.clone(), r.to_string());
                            }
                        }
                    }
                    let o = store::split_ops::SplitOp {
                        op_id: candidate_op_id.clone(),
                        mode: "quarantine_only".into(),
                        note_id: note_id.clone(),
                        speaker_ids: speaker_ids.clone(),
                        affected_persons: affected.iter().cloned().collect(),
                        phase: store::split_ops::phase::PLAN.into(),
                        residual_choice: None,
                        samples_confirm_seen: false,
                        plan_groups: Vec::new(),
                        prior_links,
                        undone_at: None,
                        created_at: now.clone(),
                        updated_at: now.clone(),
                    };
                    store::split_ops::create(&root, &o)?;
                    o
                }
            };
            // 隔离置位与解析同锁:中间不可能插入合并让 id 失效。
            let mut vp = vp_store.load();
            let mut changed = false;
            for pid in &op.affected_persons {
                if let Some(p) = vp.people.get_mut(pid) {
                    if !p.voiceprint_quarantined {
                        p.voiceprint_quarantined = true;
                        changed = true;
                    }
                }
            }
            if changed {
                vp_store.save_for_split(&vp)?;
            }
            Ok(op.op_id)
        })
        .map_err(|e| e.to_string())?;
    // ── 作废旧 identify 建议:在 IDENTIFY_ACT_GATE 内,失败就失败(op 停在 plan,
    //    可重试)——静默吞掉的话旧建议还能把混杂段灌回库(codex 实现轮一 P1⑬)。 ──
    {
        refine::identify::invalidate_for_marking(&dir, &{
            store::split_ops::load(&root, &op_id)
                .map_err(|e| e.to_string())?
                .affected_persons
                .into_iter()
                .collect()
        }, &marked_seqs, &members, &now)
        .map_err(|e| e.to_string())?;
    }
    // ── 笔记侧标记(actor 持 NoteLock 串行落盘;附带清 person_id)。幂等。 ──
    for sid in &speaker_ids {
        env.edit_note(lifecycle::machine::EditOp::SetMultiSpeaker {
            id: note_id.clone(),
            speaker_id: sid.clone(),
        })?;
    }
    store::split_ops::advance_guarded(
        &vp_store,
        &root,
        &op_id,
        &[store::split_ops::phase::PLAN],
        store::split_ops::phase::MARKED,
        &now,
    )
    .map_err(|e| e.to_string())?;
    Ok(op_id)
}

pub(crate) fn confirm_multi_samples_with(
    env: &dyn SplitEnv,
    op_id: String,
    extra_delete: Vec<String>,
    confirm_seen: bool,
) -> Result<u32, String> {
    let root = env.root().map_err(|e| e.to_string())?;
    let op_lock = split_op_lock(&op_id);
    let _op_guard = op_lock.lock().unwrap();
    // **锁后重读**:锁前的快照可能停在"还没选残留"的旧状态——residual 若在本请求
    // 等锁期间落了意图并跑完 baseline,拿旧快照过冻结检查再删样本,删完才在阶段
    // CAS 上失败,但删除已无法撤销(codex 实现轮四 P1①)。
    let op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
    if op.phase != store::split_ops::phase::MARKED
        && op.phase != store::split_ops::phase::SAMPLES_HANDLED
    {
        return Err(tr!("当前阶段不能处置样本: {p}", "Cannot handle samples in phase {p}", p = &op.phase));
    }
    // 意图落盘后样本集合冻结:residual 选择(尤其 baseline)以当时的样本为输入,
    // 再 purge 会让重算的幂等性失效(codex 实现轮三 P2)。
    if op.residual_choice.is_some() {
        return Err(tr!(
            "残留处置已开始,样本集合已冻结",
            "Residual handling has started; the sample set is frozen"
        ));
    }
    // 阶段落盘必须证明用户看到过信息缺口:不带确认不推进,重入也不许把已确认改回
    // 未确认(codex 实现轮一 P2)。
    if !confirm_seen {
        return Err(tr!(
            "请先确认已了解样本无法归因的说明",
            "Please confirm you understand the attribution gap first"
        ));
    }
    let vp_store = store::VoiceprintStore::new(root.clone());
    let deleted = vp_store
        .purge_marked_samples(&op, &extra_delete)
        .map_err(|e| e.to_string())?;
    let now = chrono::Local::now().to_rfc3339();
    vp_store
        .with_guard(|| {
            let mut o = store::split_ops::load(&root, &op_id)?;
            anyhow::ensure!(
                o.phase == store::split_ops::phase::MARKED
                    || o.phase == store::split_ops::phase::SAMPLES_HANDLED,
                "阶段已变: {}",
                o.phase
            );
            o.phase = store::split_ops::phase::SAMPLES_HANDLED.into();
            o.samples_confirm_seen = true;
            o.updated_at = now.clone();
            store::split_ops::save(&root, &o)
        })
        .map_err(|e| e.to_string())?;
    Ok(deleted)
}

pub(crate) fn resolve_multi_residual_with(
    env: &dyn SplitEnv,
    op_id: String,
    choice: String,
    then_split: bool,
) -> Result<(), String> {
    if choice != "accept" && choice != "baseline" {
        return Err(tr!("未知选项: {choice}", "Unknown choice: {choice}", choice = &choice));
    }
    {
        let root = env.root().map_err(|e| e.to_string())?;
        let op_lock = split_op_lock(&op_id);
        let _op_guard = op_lock.lock().unwrap();
        let op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
        let vp_store = store::VoiceprintStore::new(root.clone());
        let now = chrono::Local::now().to_rfc3339();
        use store::split_ops::phase as ph;
        match op.phase.as_str() {
            p if p == ph::SAMPLES_HANDLED => {
                // 意图先落盘,副作用后执行,阶段最后推进(codex 实现轮二 P1④)。
                // 意图 CAS 收进 guard 内(锁内重读):两个并发请求各自拿着"还没选"的
                // 旧快照时,后进 guard 的那个必须看见先者写的 choice 并被拒
                // (codex 实现轮三 P1④;op 锁已串行化,这是纵深防御)。
                vp_store
                    .with_guard(|| {
                        let mut o = store::split_ops::load(&root, &op_id)?;
                        anyhow::ensure!(o.phase == ph::SAMPLES_HANDLED, "阶段已变: {}", o.phase);
                        match &o.residual_choice {
                            Some(stored) if stored != &choice => {
                                anyhow::bail!("已选择过「{stored}」,恢复中不能改选")
                            }
                            Some(_) => {} // 同值重入:不重写
                            None => {
                                o.residual_choice = Some(choice.clone());
                                if then_split {
                                    o.mode = "split_commit".into();
                                }
                                o.updated_at = now.clone();
                                store::split_ops::save(&root, &o)?;
                            }
                        }
                        Ok(())
                    })
                    .map_err(|e| e.to_string())?;
                if choice == "baseline" {
                    // 幂等:rebuild_person_from_samples 重跑得到同一结果。
                    if let Err(e) = run_baseline_reset(env, &vp_store, &op) {
                        env.consume_pending_rebuild(); // pending 不能没人管
                        return Err(e);
                    }
                }
                // **以落盘的 mode 为准,不信本次请求参数**(codex 实现轮四 P1②):
                // advance 的返回值就是刚落盘的 op,不再单独 load——单独 load 的瞬时
                // 失败若回退到请求参数,旧问题原样回来(轮五 P1②)。
                let advanced = store::split_ops::advance_guarded(
                    &vp_store, &root, &op_id, &[ph::SAMPLES_HANDLED], ph::RESIDUAL_DECIDED, &now,
                )
                .map_err(|e| e.to_string())?;
                if advanced.mode == "split_commit" {
                    // pending 重建**不在这里**消化:人物还隔离着,全库重建会把刚算的
                    // 基线按"隔离只清空"冲掉(codex 实现轮三 P1②)。commit/cancel
                    // 完成解除后消化。
                    return Ok(());
                }
            }
            p if p == ph::RESIDUAL_DECIDED => {
                // 重入:选择已落盘已执行,只允许同值收尾;拆分模式不从这里收尾。
                let stored = op.residual_choice.clone().unwrap_or_default();
                if stored != choice {
                    return Err(tr!(
                        "已选择过「{stored}」,恢复中不能改选",
                        "Already chose \"{stored}\"; cannot change during recovery",
                        stored = &stored
                    ));
                }
                if op.mode == "split_commit" {
                    // 落盘意图优先于请求参数(轮四 P1②)。
                    return Err(tr!("拆分模式由拆分流程收尾", "Split mode finishes via the split flow"));
                }
            }
            p if p == ph::RELEASED => {
                // 重入:公共收尾(重跑解除+done+pending+缓存+图谱,轮四 P1③)。
                return complete_released(env, &vp_store, &root, &op_id, &now);
            }
            p => {
                return Err(tr!("先完成样本处置(当前阶段: {p})", "Handle samples first (phase: {p})", p = p));
            }
        }
        finish_and_release(&vp_store, &root, &op_id, &[ph::RESIDUAL_DECIDED], ph::RELEASED, &now)
            .map_err(|e| e.to_string())?;
        complete_released(env, &vp_store, &root, &op_id, &now)
    }
}

/// baseline 重算的执行体:与全库重建共用 REBUILD_RUNNING 单飞;结束后消化
/// REBUILD_PENDING(期间若有人切模型,请求被记为 pending,没人消化的话库会长期
/// 停在旧空间——codex 实现轮一 P1⑨)。
fn run_baseline_reset(
    env: &dyn SplitEnv,
    vp_store: &store::VoiceprintStore,
    op: &store::split_ops::SplitOp,
) -> Result<(), String> {
    if !env.begin_exclusive_rebuild() {
        return Err(tr!("声纹库重建进行中,稍后再试", "A library rebuild is running; try again later"));
    }
    let r = (|| -> Result<(), String> {
        let mut e = env.open_embedder()
            .map_err(|e| tr!("声纹模型不可用: {e}", "Speaker model unavailable: {e}", e = e))?;
        let tag = e.model().to_string();
        for pid in &op.affected_persons {
            vp_store.rebuild_person_from_samples(pid, &mut e, &tag).map_err(|e| e.to_string())?;
        }
        Ok(())
    })();
    env.end_exclusive_rebuild();
    // 注意:排队的全库重建**不在这里**消化——人物还隔离着,全库重建会把刚算的基线
    // 清空。调用方在解除隔离之后调 consume_pending_rebuild(codex 实现轮二 P1⑤);
    // 出错路径也由调用方兜(pending 不能没人管)。PENDING/标记留在原位,进程中途
    // 退出也有启动补跑兜住(标记是排队那次 spawn 在 CTL 内写的)。
    r
}

/// released 阶段的**公共收尾**(commit/residual 两侧共用,幂等):重跑解除(兜底)→
/// 补 done → 消化 pending 重建 → 刷缓存 →(拆分模式)排图谱重建。没有它,DONE 推进
/// 失败后的重入会各走各的半截路径,漏掉 pending/缓存/图谱(codex 实现轮四 P1③)。
fn complete_released(
    env: &dyn SplitEnv,
    vp_store: &store::VoiceprintStore,
    root: &std::path::Path,
    op_id: &str,
    now: &str,
) -> Result<(), String> {
    vp_store
        .with_guard(|| {
            let o = store::split_ops::load(root, op_id)?;
            release_for_op_locked(vp_store, root, &o)
        })
        .map_err(|e| e.to_string())?;
    // DONE **最后**落:它一落操作就从恢复列表消失,之前任何一步(图谱排队可失败)
    // 没做完都补不回来(codex 实现轮五 P1①)。前面各步全部幂等,重试安全。
    let op = store::split_ops::load(root, op_id).map_err(|e| e.to_string())?;
    env.consume_pending_rebuild();
    env.on_split_done(root, op.mode == "split_commit")?;
    store::split_ops::advance_guarded(
        vp_store,
        root,
        op_id,
        &[store::split_ops::phase::RELEASED],
        store::split_ops::phase::DONE,
        now,
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// **同一 vp_guard 内**把本 op 推进到"不再持有"的阶段并解除隔离。两件事必须原子:
/// 分开做的话,重叠的两个 op 各自在解除时看到对方"未关单"而跳过共享人物,随后又
/// 各自关单——全部完成、人物却永久隔离(codex 实现轮二 P1③)。原子化后,后关单的
/// 那个 op 一定能看到先关单者已处于非持有阶段,补上解除。
/// `from`/`to`:本 op 的阶段推进(to 必须是非持有阶段:released / cancelled)。
fn finish_and_release(
    vp_store: &store::VoiceprintStore,
    root: &std::path::Path,
    op_id: &str,
    from: &[&str],
    to: &str,
    now: &str,
) -> anyhow::Result<()> {
    vp_store.with_guard(|| {
        // **先解除、后推进**(同一 guard):顺序反过来的话,"阶段已 released、解除没写成"
        // 的崩溃窗会让独占人物永久隔离——released 重入只补 done,cancelled 连恢复列表
        // 都不进(codex 实现轮三 P1①)。现在崩在中间 → 阶段未变 → 重入重跑解除(幂等)。
        // 持有者判定读的是**别的 op** 的盘面阶段,与自身阶段无关,所以先后互换不
        // 影响轮二 P1③ 的原子性结论:后关单者仍然会看到先关单者已是非持有态。
        let op = store::split_ops::load(root, op_id)?;
        anyhow::ensure!(
            from.contains(&op.phase.as_str()),
            "阶段不符:当前 {},不能收尾到 {to}",
            op.phase
        );
        release_for_op_locked(vp_store, root, &op)?;
        store::split_ops::advance(root, op_id, from, to, now)?;
        Ok(())
    })
}

/// 解除本 op 人物的隔离(排除其它持有者)。**调用方须已持 vp_guard**。幂等。
fn release_for_op_locked(
    vp_store: &store::VoiceprintStore,
    root: &std::path::Path,
    op: &store::split_ops::SplitOp,
) -> anyhow::Result<()> {
    let held: std::collections::BTreeSet<String> = store::split_ops::open_ops_all(root)
        .into_iter()
        .filter(|o| o.op_id != op.op_id && store::split_ops::holds_quarantine(o))
        .flat_map(|o| o.affected_persons)
        .collect();
    let mut vp = vp_store.load();
    let mut changed = false;
    for pid in &op.affected_persons {
        if held.contains(pid) {
            eprintln!("解除隔离跳过:{pid} 仍被其它未完成处置持有");
            continue;
        }
        if let Some(p) = vp.people.get_mut(pid) {
            if p.voiceprint_quarantined {
                p.voiceprint_quarantined = false;
                changed = true;
            }
        }
    }
    if changed {
        vp_store.save_for_split(&vp)?;
    }
    Ok(())
}

/// 进程内按 op 串行化(commit/cancel/confirm/residual 四命令共用):同一 op 的两个
/// 命令并发交错会产生"读旧阶段 → 各做各的副作用"的竞态(codex 实现轮三 P1⑤)。
/// 客户端单进程,进程内互斥即可;跨进程不在本设计承诺内(见设计文档推迟项)。
fn split_op_lock(op_id: &str) -> std::sync::Arc<std::sync::Mutex<()>> {
    static LOCKS: std::sync::Mutex<
        std::collections::BTreeMap<String, std::sync::Arc<std::sync::Mutex<()>>>,
    > = std::sync::Mutex::new(std::collections::BTreeMap::new());
    LOCKS
        .lock()
        .unwrap()
        .entry(op_id.to_string())
        .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(())))
        .clone()
}

#[derive(serde::Serialize)]
pub(crate) struct SplitSuggestGroup {
    pub(crate) seqs: Vec<u64>,
    pub(crate) total_ms: u64,
    pub(crate) suggested: Option<(String, String, f32)>, // (person_id, name, cosine)
}

#[derive(serde::Serialize)]
pub(crate) struct SplitSuggestOut {
    pub(crate) groups: Vec<SplitSuggestGroup>,
    /// 无法判定的段(过短/嵌入失败/轨道缺失):不猜,单独一桶交给人。
    pub(crate) undetermined: Vec<u64>,
}

#[derive(serde::Deserialize)]
pub(crate) struct SplitGroupIn {
    pub(crate) seqs: Vec<u64>,
    pub(crate) dest_kind: String, // existing_speaker | person | new_speaker | keep
    pub(crate) dest_id: Option<String>,
}

pub(crate) fn suggest_split_groups_with(env: &dyn SplitEnv, op_id: String) -> Result<SplitSuggestOut, String> {
    {
        let root = env.root().map_err(|e| e.to_string())?;
        let op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
        let nroot = env.notes_dir().map_err(|e| e.to_string())?;
        let dir = nroot.join(&op.note_id);
        let note = store::NoteStore::new(nroot).load(&op.note_id).map_err(|e| e.to_string())?;
        let segs: Vec<&store::SegmentRecord> = note
            .segments
            .iter()
            .filter(|s| s.speaker.as_deref().is_some_and(|sp| op.speaker_ids.iter().any(|x| x == sp)))
            .collect();
        if segs.is_empty() {
            return Err(tr!("被标说话人名下没有段落", "The marked speakers have no segments"));
        }
        let _fb = FEEDBACK_GATE.lock().unwrap();
        // 标签、权重、种子同源(同一次设置读取)。
        let mut embedder = env
            .open_embedder()
            .map_err(|e| tr!("声纹模型不可用: {e}", "Speaker model unavailable: {e}", e = e))?;
        let tag = embedder.model().to_string();
        // 进度事件:大簇逐段嵌入要数分钟,前端横幅靠它区分「在算」与「卡死」。
        // 每 10 段发一次 + 首尾各一次,避免事件风暴。
        let embs = refine::embed_all_with_progress(&dir, &segs, &mut embedder, &tag, &|done, total| {
            if done == 1 || done == total || done % 10 == 0 {
                env.split_progress(&op.note_id, done, total);
            }
        })
        .map_err(|e| e.to_string())?;
        let inputs: Vec<refine::recluster::SegInput> = segs
            .iter()
            .map(|s| refine::recluster::SegInput {
                seq: s.seq,
                start_ms: s.start_ms,
                end_ms: s.end_ms,
                source: s.source.clone(),
                old_speaker: s.speaker.clone(),
            })
            .collect();
        let seeds = env.seeds_for(&tag);
        let sug = refine::recluster::recluster_split(&inputs, &embs, &seeds);
        Ok(SplitSuggestOut {
            groups: sug
                .groups
                .into_iter()
                .map(|g| SplitSuggestGroup {
                    seqs: g.member_idx.iter().map(|&i| inputs[i].seq).collect(),
                    total_ms: g.total_ms,
                    suggested: g.suggested,
                })
                .collect(),
            undetermined: sug.undetermined_idx.iter().map(|&i| inputs[i].seq).collect(),
        })
    }
}

pub(crate) fn commit_split_with(env: &dyn SplitEnv, op_id: String, groups: Vec<SplitGroupIn>) -> Result<String, String> {
    {
        let root = env.root().map_err(|e| e.to_string())?;
        // 同一 op 的 commit/cancel/confirm/residual 进程内串行:并发交错 = 各拿旧阶段
        // 做各的副作用(codex 实现轮三 P1⑤)。
        let op_lock = split_op_lock(&op_id);
        let _op_guard = op_lock.lock().unwrap();
        let vp_store = store::VoiceprintStore::new(root.clone());
        let mut op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
        if op.mode != "split_commit" {
            return Err(tr!("该操作不在拆分模式", "This operation is not in split mode"));
        }
        let nroot = env.notes_dir().map_err(|e| e.to_string())?;
        let nstore = store::NoteStore::new(nroot.clone());
        let dir = nroot.join(&op.note_id);
        let now = chrono::Local::now().to_rfc3339();

        // ── 阶段 1:计划定稿 + 占号(residual_decided → reserved) ──
        if op.phase == store::split_ops::phase::RESIDUAL_DECIDED {
            if groups.is_empty() {
                return Err(tr!("没有分组", "No groups"));
            }
            // 孤儿清理**先于**选号与建表读取:上次占号成功、阶段没落盘时,残留的预留
            // 项会把 max(S) 抬高,每次重试都换更大的号;放在 to_reserve 判空之后则
            // "重试计划不需要新号"时孤儿永远清不掉(codex 实现轮二 P2⑧)。
            env.edit_note(lifecycle::machine::EditOp::ReleaseReservedSpeakers {
                id: op.note_id.clone(),
                op_id: op.op_id.clone(),
            })?;
            let note = nstore.load(&op.note_id).map_err(|e| e.to_string())?;
            let vp = vp_store.load();
            let by_seq: std::collections::BTreeMap<u64, &store::SegmentRecord> =
                note.segments.iter().map(|s| (s.seq, s)).collect();
            // seq 不重不漏地属于被标说话人。
            let mut seen_seqs: std::collections::BTreeSet<u64> = Default::default();
            for g in &groups {
                for q in &g.seqs {
                    let seg = by_seq
                        .get(q)
                        .ok_or_else(|| tr!("段不存在: {q}", "No such segment: {q}", q = q))?;
                    let sp = seg.speaker.as_deref().unwrap_or("");
                    if !op.speaker_ids.iter().any(|x| x == sp) {
                        return Err(tr!("段 {q} 不属于被标说话人", "Segment {q} is not under a marked speaker", q = q));
                    }
                    if !seen_seqs.insert(*q) {
                        return Err(tr!("段 {q} 出现在多个组", "Segment {q} appears in multiple groups", q = q));
                    }
                }
            }
            // 不漏:分组必须覆盖被标说话人名下的**全部**段(UI 的"无法判定"桶以
            // dest=keep 送来,等式才成立)。漏段静默留在混杂簇里,op 却能关单
            // (codex 实现轮二 P2⑨)。
            let all_marked: std::collections::BTreeSet<u64> = note
                .segments
                .iter()
                .filter(|sg| {
                    sg.speaker.as_deref().is_some_and(|sp| op.speaker_ids.iter().any(|x| x == sp))
                })
                .map(|sg| sg.seq)
                .collect();
            if seen_seqs != all_marked {
                let missing = all_marked.difference(&seen_seqs).count();
                return Err(tr!(
                    "分组漏了 {missing} 段(被标说话人的段必须全部指定去处,拿不准选「保持不动」)",
                    "{missing} segments missing from groups (every marked segment needs a destination; pick keep-as-is when unsure)",
                    missing = missing
                ));
            }
            // 目标解析 + 预留数量。
            let mut need_reserve = 0usize;
            for g in &groups {
                match g.dest_kind.as_str() {
                    "existing_speaker" => {
                        let sid = g.dest_id.as_deref().unwrap_or("");
                        match note.speakers.get(sid) {
                            None => {
                                return Err(tr!("目标说话人不存在: {sid}", "No such speaker: {sid}", sid = sid))
                            }
                            Some(m) => {
                                // 别的 op 的预留号不许当去向(codex 实现轮二 P1⑥)。
                                if m.reserved_by.as_deref().is_some_and(|o| o != op.op_id) {
                                    return Err(tr!(
                                        "说话人 {sid} 是另一次拆分的预留号",
                                        "Speaker {sid} is reserved by another split",
                                        sid = sid
                                    ));
                                }
                            }
                        }
                    }
                    "person" => {
                        let pid = g.dest_id.as_deref().unwrap_or("");
                        let resolved = store::VoiceprintStore::resolve(&vp, pid)
                            .ok_or_else(|| tr!("声纹库中没有该人物: {pid}", "No such person: {pid}", pid = pid))?;
                        // 隔离中的人物只允许「本 op 的 A 类认领」:别的 op 正处置中的
                        // 人物不许当去向——那会绕过它的门禁写入(codex 实现轮一 P1⑥)。
                        let q = vp.people.get(resolved).is_some_and(|p| p.voiceprint_quarantined);
                        if q && !op.affected_persons.iter().any(|a| a == resolved) {
                            return Err(tr!(
                                "人物 {pid} 正被隔离处置,不能作为去向",
                                "Person {pid} is quarantined by another cleanup and cannot be a destination",
                                pid = pid
                            ));
                        }
                        // 已有关联 S 则复用,否则要一个新号。
                        let linked = note
                            .speakers
                            .iter()
                            .any(|(_, m)| m.person_id.as_deref() == Some(resolved));
                        if !linked {
                            need_reserve += 1;
                        }
                    }
                    "new_speaker" => need_reserve += 1,
                    "keep" => {}
                    other => return Err(tr!("未知去处: {other}", "Unknown destination: {other}", other = other)),
                }
            }
            let mut fresh = nstore
                .peek_next_speaker_ids(&op.note_id, need_reserve)
                .map_err(|e| e.to_string())?
                .into_iter();
            let mut plan: Vec<store::split_ops::SplitPlanGroup> = Vec::new();
            let mut to_reserve: Vec<String> = Vec::new();
            for g in &groups {
                let dest_speaker = match g.dest_kind.as_str() {
                    "existing_speaker" => g.dest_id.clone(),
                    "person" => {
                        let resolved = store::VoiceprintStore::resolve(&vp, g.dest_id.as_deref().unwrap())
                            .expect("上面已校验")
                            .to_string();
                        match note
                            .speakers
                            .iter()
                            .find(|(_, m)| m.person_id.as_deref() == Some(resolved.as_str()))
                        {
                            Some((sid, _)) => Some(sid.clone()),
                            None => {
                                let sid = fresh.next().expect("need_reserve 已计数");
                                to_reserve.push(sid.clone());
                                Some(sid)
                            }
                        }
                    }
                    "new_speaker" => {
                        let sid = fresh.next().expect("need_reserve 已计数");
                        to_reserve.push(sid.clone());
                        Some(sid)
                    }
                    _ => None, // keep
                };
                let mut seqs = g.seqs.clone();
                seqs.sort_unstable();
                let expected: Vec<String> = seqs
                    .iter()
                    .map(|q| by_seq[q].speaker.clone().unwrap_or_default())
                    .collect();
                plan.push(store::split_ops::SplitPlanGroup {
                    seqs,
                    expected_speakers: expected,
                    dest_kind: g.dest_kind.clone(),
                    dest_id: g.dest_id.clone(),
                    dest_speaker,
                });
            }
            // 计划先落盘,再占号(占号后崩溃:reserved_by 所有权 + 计划都在,可恢复可取消)。
            vp_store
                .with_guard(|| {
                    let mut o = store::split_ops::load(&root, &op_id)?;
                    o.plan_groups = plan.clone();
                    o.updated_at = now.clone();
                    store::split_ops::save(&root, &o)
                })
                .map_err(|e| e.to_string())?;
            if !to_reserve.is_empty() {
                env.edit_note(lifecycle::machine::EditOp::ReserveSpeakers {
                    id: op.note_id.clone(),
                    speaker_ids: to_reserve,
                    op_id: op.op_id.clone(),
                })?;
            }
            op = store::split_ops::advance_guarded(
                &vp_store,
                &root,
                &op_id,
                &[store::split_ops::phase::RESIDUAL_DECIDED],
                store::split_ops::phase::RESERVED,
                &now,
            )
            .map_err(|e| e.to_string())?;
        }

        // ── 阶段 2:条件关联 → 批量改派 → 修订稿同步(reserved → segments_reassigned)。
        //    关联放**最前**:它带 CAS,用户改过关联时在任何段被动过之前干净停下
        //    (codex 实现轮三 P1③——冲突不是跳过继续,是停止计划);重入幂等(已是目标
        //    人物直接放行)。 ──
        if op.phase == store::split_ops::phase::RESERVED {
            {
                let vp = vp_store.load();
                for g in &op.plan_groups {
                    if g.dest_kind == "person" {
                        let (Some(pid), Some(sid)) = (g.dest_id.as_deref(), g.dest_speaker.as_deref()) else {
                            continue;
                        };
                        if let Some(resolved) = store::VoiceprintStore::resolve(&vp, pid) {
                            env.edit_note(lifecycle::machine::EditOp::AssignPersonIf {
                                id: op.note_id.clone(),
                                speaker_id: sid.to_string(),
                                person_id: resolved.to_string(),
                            })?;
                        }
                    }
                }
            }
            let moves: Vec<(u64, String, String)> = op
                .plan_groups
                .iter()
                .filter_map(|g| g.dest_speaker.as_ref().map(|d| (g, d)))
                .flat_map(|(g, d)| {
                    g.seqs
                        .iter()
                        .zip(&g.expected_speakers)
                        .filter(|(_, exp)| exp.as_str() != d.as_str())
                        .map(|(q, exp)| (*q, exp.clone(), d.clone()))
                        .collect::<Vec<_>>()
                })
                .collect();
            if !moves.is_empty() {
                env.edit_note(lifecycle::machine::EditOp::SplitReassign {
                    id: op.note_id.clone(),
                    moves: moves.clone(),
                    op_id: op.op_id.clone(),
                })?;
            }
            // 修订稿同步:全组同去向原位改;跨组标 stale(一期边界)。
            // 一波说话人(2026-08-21):段落只改归属,身份显示现查 note.speakers,
            // 原 person_name 快照(codex 实现轮一 P2 的保身份逻辑)随之整体删除。
            let moved: std::collections::BTreeMap<u64, String> = op
                .plan_groups
                .iter()
                .filter_map(|g| g.dest_speaker.as_ref().map(|d| (g, d)))
                .flat_map(|(g, d)| g.seqs.iter().map(|q| (*q, d.clone())).collect::<Vec<_>>())
                .collect();
            if !moved.is_empty() && store::aing_exists(&dir) {
                match store::sync_refined_after_split(&dir, &moved) {
                    Ok(true) => eprintln!("拆分({op_id}):修订稿存在跨组段落,已标 stale 待重新 Aing"),
                    Ok(false) => {}
                    Err(e) => {
                        // 原始段已改派而修订稿没跟上:不标脏的话默认视图显示旧归属,
                        // 用户以为拆完了。降级标 stale;连 stale 都标不上就整体失败
                        // (重试安全:改派 CAS 认已完成态)——codex 实现轮一 P1⑪。
                        eprintln!("拆分({op_id}):修订稿同步失败,降级标 stale: {e}");
                        store::mark_refined_stale(&dir).map_err(|e2| {
                            format!("修订稿同步失败且标 stale 也失败,先重试: {e};{e2}")
                        })?;
                    }
                }
            }
            op = store::split_ops::advance_guarded(
                &vp_store,
                &root,
                &op_id,
                &[store::split_ops::phase::RESERVED],
                store::split_ops::phase::SEGMENTS_REASSIGNED,
                &now,
            )
            .map_err(|e| e.to_string())?;
        }

        // ── 阶段 3:受权回灌(segments_reassigned → reenrolled)。可选增强:失败如实
        //    报告,不回滚拆分、不挡收尾(设计:回灌只有方向性,不宣称修复)。 ──
        let mut enroll_notes: Vec<String> = Vec::new();
        if op.phase == store::split_ops::phase::SEGMENTS_REASSIGNED {
            let person_groups: Vec<_> =
                op.plan_groups.iter().filter(|g| g.dest_kind == "person").collect();
            if !person_groups.is_empty() {
                let _fb = FEEDBACK_GATE.lock().unwrap();
                match env.open_embedder() {
                    Ok(mut embedder) => {
                        let note = nstore.load(&op.note_id).map_err(|e| e.to_string())?;
                        for g in person_groups {
                            let Some(pid) = g.dest_id.as_deref() else { continue };
                            let seqs: std::collections::BTreeSet<u64> = g.seqs.iter().copied().collect();
                            let mut needs_rebuild = false;
                            let r = feedback::reinforce_person(
                                &dir,
                                &note.segments,
                                &feedback::SegFilter::Seqs(seqs),
                                pid,
                                &vp_store,
                                &mut embedder,
                                &now,
                                Some(&op.op_id),
                                &mut needs_rebuild,
                                // 只对本 op 认领的隔离人物放行;其它目标走普通门禁
                                // (codex 实现轮一 P1⑥:布尔旁路必须限定在 op 范围内)。
                                op.affected_persons.iter().any(|a| Some(a.as_str()) == g.dest_id.as_deref()),
                            );
                            if needs_rebuild {
                                env.request_rebuild("拆分回灌纠错后质心置空");
                            }
                            match r {
                                Ok(feedback::ReinforceResult::Applied { .. }) => {}
                                Ok(other) => enroll_notes.push(format!("{pid}: {other:?}")),
                                Err(e) => enroll_notes.push(format!("{pid}: {e}")),
                            }
                        }
                    }
                    Err(e) => enroll_notes.push(format!("嵌入器不可用,回灌全部跳过: {e}")),
                }
            }
            op = store::split_ops::advance_guarded(
                &vp_store,
                &root,
                &op_id,
                &[store::split_ops::phase::SEGMENTS_REASSIGNED],
                store::split_ops::phase::REENROLLED,
                &now,
            )
            .map_err(|e| e.to_string())?;
        }

        // ── 阶段 4:解除隔离并收尾(推进+解除同一 guard,排除其它持有者) ──
        if op.phase == store::split_ops::phase::REENROLLED {
            finish_and_release(
                &vp_store,
                &root,
                &op_id,
                &[store::split_ops::phase::REENROLLED],
                store::split_ops::phase::RELEASED,
                &now,
            )
            .map_err(|e| e.to_string())?;
            op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
        }
        // released 的公共收尾(轮四 P1③):REENROLLED 刚推进来的、以及上次 DONE 没写成
        // 的重入,都从这里走同一条幂等路径(done+pending+缓存+图谱)。
        if op.phase == store::split_ops::phase::RELEASED {
            complete_released(env, &vp_store, &root, &op_id, &now)?;
        }
        Ok(if enroll_notes.is_empty() { String::new() } else { enroll_notes.join("; ") })
    }
}

pub(crate) fn cancel_split_with(env: &dyn SplitEnv, op_id: String) -> Result<(), String> {
    let root = env.root().map_err(|e| e.to_string())?;
    let op_lock = split_op_lock(&op_id);
    let _op_guard = op_lock.lock().unwrap();
    let vp_store = store::VoiceprintStore::new(root.clone());
    let op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
    let now = chrono::Local::now().to_rfc3339();
    match op.phase.as_str() {
        p if p == store::split_ops::phase::RESIDUAL_DECIDED
            || p == store::split_ops::phase::RESERVED
            || p == store::split_ops::phase::CANCEL_REQUESTED => {}
        p if p == store::split_ops::phase::SEGMENTS_REASSIGNED
            || p == store::split_ops::phase::REENROLLED =>
        {
            return Err(tr!("段落已改派,只能继续完成拆分", "Segments already reassigned; finish the split instead"));
        }
        p => return Err(tr!("当前阶段不能取消: {p}", "Cannot cancel in phase {p}", p = p)),
    }
    store::split_ops::advance_guarded(
        &vp_store,
        &root,
        &op_id,
        &[
            store::split_ops::phase::RESIDUAL_DECIDED,
            store::split_ops::phase::RESERVED,
            store::split_ops::phase::CANCEL_REQUESTED,
        ],
        store::split_ops::phase::CANCEL_REQUESTED,
        &now,
    )
    .map_err(|e| e.to_string())?;
    env.edit_note(lifecycle::machine::EditOp::ReleaseReservedSpeakers {
        id: op.note_id.clone(),
        op_id: op.op_id.clone(),
    })?;
    // 取消拆分 ≠ 取消打标:隔离义务已在样本/残留阶段兑现,这里照常解除。
    // 推进 cancelled 与解除同一 guard(codex 实现轮二 P1③)。
    finish_and_release(
        &vp_store,
        &root,
        &op_id,
        &[store::split_ops::phase::CANCEL_REQUESTED],
        store::split_ops::phase::CANCELLED,
        &now,
    )
    .map_err(|e| e.to_string())?;
    env.consume_pending_rebuild();
    Ok(())
}

/// 某笔记的未完成打标操作(UI 恢复入口)。纯读。
#[derive(serde::Serialize, Clone)]
pub(crate) struct AutoSplitHint {
    pub(crate) person_id: String,
    pub(crate) name: String,
    pub(crate) sim: f32,
}

#[derive(serde::Serialize, Clone)]
pub(crate) struct AutoSplitGroupOut {
    pub(crate) speaker_id: String,
    pub(crate) count: u32,
    pub(crate) dur_ms: u64,
    pub(crate) hint: Option<AutoSplitHint>,
}

#[derive(serde::Serialize)]
pub(crate) struct AutoSplitOut {
    pub(crate) op_id: String,
    /// false = 声纹听下来就是一个人(或全部判不准),没拆,一切已恢复原状。
    pub(crate) split: bool,
    pub(crate) groups: Vec<AutoSplitGroupOut>,
    /// 判不准、留在原说话人的段数。
    pub(crate) kept: u32,
}

/// 一键拆分的本体。各阶段直接调同步核心(原先在异步线程上串 await 各命令),
/// 整条流在阻塞线程池里跑。
pub(crate) fn auto_split_speaker_with(
    env: &dyn SplitEnv,
    note_id: String,
    speaker_id: String,
) -> Result<AutoSplitOut, String> {
    store::validate_note_id(&note_id).map_err(|e| e.to_string())?;
    env.admit(&note_id, occupancy::Intent::EditOutsideAing)?;
    let root = env.root().map_err(|e| e.to_string())?;
    // 断点续跑:同一说话人已有未完成 op(嵌入中途被重启杀掉是常态,实测一天两单)
    // 就接着跑,绝不另起炉灶——重复 mark 会叠出第二个 op,隔离悬置、账目成灾。
    use store::split_ops::phase as ph;
    let existing = store::split_ops::open_ops_for_note(&root, &note_id)
        .into_iter()
        .find(|o| o.speaker_ids == vec![speaker_id.clone()] && o.undone_at.is_none());
    // ① 打标(隔离/作废建议/清关联,记录 prior_links 快照)。PLAN 态的 op 由 mark
    //    自身复用推进。
    let op_id = match &existing {
        Some(o) if o.phase != ph::PLAN => o.op_id.clone(),
        _ => mark_speaker_multi_with(env, note_id.clone(), vec![speaker_id.clone()])?,
    };
    // ② 样本自动清理:零勾选=只删「可归因到本篇被标簇」的样本(receipt 证据),
    //    来源未知的一律保留——比旧流程让用户凭试听勾删更保守。
    let cur = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
    if cur.phase == ph::MARKED {
        confirm_multi_samples_with(env, op_id.clone(), Vec::new(), true)?;
    }
    // ③ 残留默认「接受」:零损失、立即可用,小偏差随后续录音按加权稀释。
    let cur = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
    if cur.phase == ph::SAMPLES_HANDLED {
        resolve_multi_residual_with(env, op_id.clone(), "accept".into(), true)?;
    }
    let nroot = env.notes_dir().map_err(|e| e.to_string())?;
    let nstore = store::NoteStore::new(nroot);
    // 已过计划期(占号/改派/回灌/释放中断):不重新分组,直接把既有计划跑到头。
    let cur = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
    if matches!(cur.phase.as_str(), p if p == ph::RESERVED || p == ph::SEGMENTS_REASSIGNED || p == ph::REENROLLED || p == ph::RELEASED)
    {
        let enroll_notes = commit_split_with(env, op_id.clone(), Vec::new())?;
        if !enroll_notes.is_empty() {
            eprintln!("auto_split({op_id}): {enroll_notes}");
        }
        let op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
        let out_groups: Vec<AutoSplitGroupOut> = op
            .plan_groups
            .iter()
            .filter(|pg| pg.dest_kind == "new_speaker")
            .filter_map(|pg| {
                pg.dest_speaker.clone().map(|sid| AutoSplitGroupOut {
                    speaker_id: sid,
                    count: pg.seqs.len() as u32,
                    dur_ms: 0,
                    hint: None,
                })
            })
            .collect();
        let kept = op
            .plan_groups
            .iter()
            .filter(|pg| pg.dest_kind == "keep")
            .map(|pg| pg.seqs.len() as u32)
            .sum();
        return Ok(AutoSplitOut { op_id, split: true, groups: out_groups, kept });
    }
    // ④ 声纹分组。
    let sug = suggest_split_groups_with(env, op_id.clone())?;
    // ⑤ 只有一组(或全是判不准):不硬拆。取消(解除隔离)并恢复本篇原状。
    if sug.groups.len() <= 1 {
        cancel_split_with(env, op_id.clone())?;
        let op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
        nstore
            .restore_after_unsplit(&note_id, &speaker_id, op.prior_links.get(&speaker_id).map(String::as_str))
            .map_err(|e| e.to_string())?;
        return Ok(AutoSplitOut { op_id, split: false, groups: Vec::new(), kept: 0 });
    }
    // ⑥ 提交:每组新说话人,判不准保持不动。
    let mut groups_in: Vec<SplitGroupIn> = sug
        .groups
        .iter()
        .map(|g| SplitGroupIn { seqs: g.seqs.clone(), dest_kind: "new_speaker".into(), dest_id: None })
        .collect();
    if !sug.undetermined.is_empty() {
        groups_in.push(SplitGroupIn {
            seqs: sug.undetermined.clone(),
            dest_kind: "keep".into(),
            dest_id: None,
        });
    }
    let enroll_notes = commit_split_with(env, op_id.clone(), groups_in)?;
    if !enroll_notes.is_empty() {
        eprintln!("auto_split({op_id}): {enroll_notes}");
    }
    // ⑦ 读回新号,写声纹建议徽标(仅展示;split_born 已随预留项创建置位)。
    let op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
    let vp = store::VoiceprintStore::new(root.clone()).load();
    let mut hints: Vec<(String, String)> = Vec::new();
    let mut out_groups: Vec<AutoSplitGroupOut> = Vec::new();
    // plan_groups 与提交的 groups 同序(commit 按提交顺序落计划);逐一配对建议。
    for (i, pg) in op.plan_groups.iter().enumerate() {
        if pg.dest_kind != "new_speaker" {
            continue;
        }
        let Some(sid) = pg.dest_speaker.clone() else { continue };
        let hint = sug.groups.get(i).and_then(|g| g.suggested.as_ref()).map(|(pid, name, sim)| {
            let resolved = store::VoiceprintStore::resolve(&vp, pid).unwrap_or(pid).to_string();
            AutoSplitHint { person_id: resolved, name: name.clone(), sim: *sim }
        });
        if let Some(h) = &hint {
            hints.push((sid.clone(), h.person_id.clone()));
        }
        out_groups.push(AutoSplitGroupOut {
            speaker_id: sid,
            count: pg.seqs.len() as u32,
            dur_ms: sug.groups.get(i).map(|g| g.total_ms).unwrap_or(0),
            hint,
        });
    }
    nstore.set_speaker_hints(&note_id, &hints).map_err(|e| e.to_string())?;
    Ok(AutoSplitOut {
        op_id,
        split: true,
        groups: out_groups,
        kept: sug.undetermined.len() as u32,
    })
}

pub(crate) fn undo_auto_split_with(env: &dyn SplitEnv, op_id: String) -> Result<(), String> {
    let root = env.root().map_err(|e| e.to_string())?;
    let op_lock = split_op_lock(&op_id);
    let _op_guard = op_lock.lock().unwrap();
    let op = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
    if op.phase != store::split_ops::phase::DONE {
        return Err(tr!("该拆分未完成,不能撤销: {p}", "Split not finished; cannot undo: {p}", p = &op.phase));
    }
    if op.undone_at.is_some() {
        return Err(tr!("该拆分已撤销过", "This split was already undone"));
    }
    env.admit(&op.note_id, occupancy::Intent::EditOutsideAing)?;
    let nroot = env.notes_dir().map_err(|e| e.to_string())?;
    let nstore = store::NoteStore::new(nroot.clone());
    let dir = nroot.join(&op.note_id);
    // 反向搬运表:seq 现在必须仍在拆分去向上(CAS),搬回计划定稿时的原说话人。
    let mut back_moves: Vec<(u64, String, String)> = Vec::new();
    let mut created_sids: std::collections::BTreeSet<String> = Default::default();
    for pg in &op.plan_groups {
        let Some(dest) = &pg.dest_speaker else { continue };
        if pg.dest_kind == "new_speaker" {
            created_sids.insert(dest.clone());
        }
        for (q, orig) in pg.seqs.iter().zip(&pg.expected_speakers) {
            back_moves.push((*q, dest.clone(), orig.clone()));
        }
    }
    if !back_moves.is_empty() {
        nstore
            .batch_set_segment_speaker(&op.note_id, &back_moves, &op.op_id)
            .map_err(|e| {
                tr!(
                    "段落已被后续编辑改动,无法撤销: {e}",
                    "Segments were edited after the split; cannot undo: {e}",
                    e = e
                )
            })?;
    }
    // 空的新说话人删除(段已搬回,必空;删除失败不阻塞其余恢复,如实记 stderr)。
    for sid in &created_sids {
        if let Err(e) = nstore.delete_speaker(&op.note_id, sid) {
            eprintln!("undo_auto_split({op_id}): 删除新说话人 {sid} 失败(忽略): {e}");
        }
    }
    // 多人标记复位 + 原关联恢复(仅本篇表项,不触库)。
    for sid in &op.speaker_ids {
        nstore
            .restore_after_unsplit(&op.note_id, sid, op.prior_links.get(sid).map(String::as_str))
            .map_err(|e| e.to_string())?;
    }
    // 修订稿反向同步:整段同去向原位改回;跨组标 stale(与正向同一口径)。
    let moved_back: std::collections::BTreeMap<u64, String> =
        back_moves.iter().map(|(q, _, back)| (*q, back.clone())).collect();
    if !moved_back.is_empty() && store::aing_exists(&dir) {
        if let Err(e) = store::sync_refined_after_split(&dir, &moved_back) {
            if let Err(e2) = store::mark_refined_stale(&dir) {
                return Err(tr!(
                    "撤销已生效,但修订稿同步失败且无法标记过期: {e} / {e2}",
                    "Undo applied, but refined sync failed and stale-marking failed: {e} / {e2}",
                    e = e,
                    e2 = e2
                ));
            }
        }
    }
    // 落撤销标记(幂等闸)。
    let mut op2 = store::split_ops::load(&root, &op_id).map_err(|e| e.to_string())?;
    op2.undone_at = Some(chrono::Local::now().to_rfc3339());
    op2.updated_at = op2.undone_at.clone().unwrap();
    store::split_ops::save(&root, &op2).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
#[path = "split_flow_tests.rs"]
mod tests;
