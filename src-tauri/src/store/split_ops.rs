//! 打标/拆分操作的阶段机(落盘)。
//!
//! 打标横跨四种资源(speakers.json、声纹库、样本文件、段落归属),中间任何一步崩溃
//! 都留下从外部判不出来的中间态——阶段必须落盘,不能靠盘面推断(教训见
//! merge_journal 的 undo_phase;设计:2026-08-20-mixed-speaker-split-design.md)。
//!
//! quarantine_only:  plan → marked → samples_handled → residual_decided → released → done
//! (split_commit 的扩展阶段在 D 期加入。)
//!
//! `plan` 先于一切副作用落盘;`marked` 停留是合法驻留态(打标即止血,清理与解除
//! 由用户在 UI 里推进,不自动续跑)。并发:读写都要求已持 vp_guard(op 文件与声纹库
//! 同根、同锁——阶段推进总是伴随库变更,分锁必竞态)。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod phase {
    pub const PLAN: &str = "plan";
    pub const MARKED: &str = "marked";
    pub const SAMPLES_HANDLED: &str = "samples_handled";
    pub const RESIDUAL_DECIDED: &str = "residual_decided";
    // ── split_commit 扩展(D 期):residual_decided 之后 ──
    pub const RESERVED: &str = "reserved";
    pub const SEGMENTS_REASSIGNED: &str = "segments_reassigned";
    pub const REENROLLED: &str = "reenrolled";
    pub const CANCEL_REQUESTED: &str = "cancel_requested";
    pub const CANCELLED: &str = "cancelled";
    pub const RELEASED: &str = "released";
    pub const DONE: &str = "done";
}

/// 拆分计划里的一个组(commit 前由 UI 定稿,落盘后不可变)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitPlanGroup {
    /// 组内段 seq(升序)。
    pub seqs: Vec<u64>,
    /// 每个 seq 在计划定稿时的原 speaker:批量改派逐段 CAS 用——对不上就是第三态
    /// (用户或其他 writer 动过),停止提交保持隔离(codex 设计轮三 P1④)。
    pub expected_speakers: Vec<String>,
    /// existing_speaker | person | new_speaker | keep
    pub dest_kind: String,
    /// existing_speaker → S id;person → P id;new_speaker 不用。
    #[serde(default)]
    pub dest_id: Option<String>,
    /// 段落实际改派到的 S id:existing_speaker=dest_id;person=既有关联 S 或预留新 S;
    /// new_speaker=预留新 S;keep=None(不动)。
    #[serde(default)]
    pub dest_speaker: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitOp {
    pub op_id: String,
    /// "quarantine_only"(D 期加 "split_commit")。
    pub mode: String,
    pub note_id: String,
    /// 被打标的原始稿说话人(S 编号)。标记以 S 为单位:修订稿的 R 是重聚类产物,
    /// 没有可挂标记的存储位;UI 从 R 映射回其 source_seqs 涉及的 S 让用户勾选。
    pub speaker_ids: Vec<String>,
    /// 受影响的库人物(resolve 后):speakers.json 的关联 + 质心贡献 receipt 的归属。
    pub affected_persons: Vec<String>,
    pub phase: String,
    #[serde(default)]
    pub residual_choice: Option<String>, // accept | baseline
    /// 用户确认已看到「历史样本无法归因到本篇」的信息缺口(诚实呈现的落盘证据)。
    #[serde(default)]
    pub samples_confirm_seen: bool,
    /// 拆分计划(split_commit 模式;commit 前落盘,之后不可变)。
    #[serde(default)]
    pub plan_groups: Vec<SplitPlanGroup>,
    /// 打标时各被标说话人的人物关联快照(sid → resolve 后 person id)。一键撤销
    /// (undo_auto_split)恢复本篇表项用;仅笔记级,不触库
    /// (2026-08-22-one-click-split-design.md)。
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub prior_links: std::collections::BTreeMap<String, String>,
    /// 一键撤销时间戳(undo_auto_split 幂等闸;None=未撤销)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undone_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn ops_dir(root: &Path) -> PathBuf {
    root.join("split_ops")
}

fn op_path(root: &Path, op_id: &str) -> Option<PathBuf> {
    // op_id 由我们生成(so-<pid>-<n>),仍校验字符集防路径逃逸。
    if op_id.is_empty() || !op_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    Some(ops_dir(root).join(format!("{op_id}.json")))
}

/// 新建(防覆盖):op 文件已存在即失败——op_id 含时间戳+pid+计数,理论不撞,
/// 这里是最后一道闸(codex 实现轮二 P1⑦)。调用方须已持 vp_guard。
pub fn create(root: &Path, op: &SplitOp) -> anyhow::Result<()> {
    let path = op_path(root, &op.op_id).ok_or_else(|| anyhow::anyhow!("非法 op id: {}", op.op_id))?;
    anyhow::ensure!(!path.exists(), "op id 撞了: {}", op.op_id);
    save(root, op)
}

/// 该 op 当前是否仍「持有」其人物的隔离(解除判定用)。正在收尾/取消中的不算持有——
/// 否则两个重叠 op 各自看到对方未关单,互相跳过解除,全部完成后人物永久隔离
/// (codex 实现轮二 P1③)。
pub fn holds_quarantine(op: &SplitOp) -> bool {
    !matches!(
        op.phase.as_str(),
        p if p == phase::DONE
            || p == phase::CANCELLED
            || p == phase::RELEASED
            || p == phase::CANCEL_REQUESTED
    )
}

/// 原子写。调用方须已持 vp_guard。
pub fn save(root: &Path, op: &SplitOp) -> anyhow::Result<()> {
    let path = op_path(root, &op.op_id).ok_or_else(|| anyhow::anyhow!("非法 op id: {}", op.op_id))?;
    std::fs::create_dir_all(path.parent().expect("恒有父目录"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(op)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn load(root: &Path, op_id: &str) -> anyhow::Result<SplitOp> {
    let path = op_path(root, op_id).ok_or_else(|| anyhow::anyhow!("非法 op id: {op_id}"))?;
    let s = std::fs::read_to_string(&path).map_err(|_| anyhow::anyhow!("打标操作不存在: {op_id}"))?;
    Ok(serde_json::from_str(&s)?)
}

/// 某笔记的全部未完成操作(phase != done),UI 恢复入口用。损坏条目跳过。
pub fn open_ops_for_note(root: &Path, note_id: &str) -> Vec<SplitOp> {
    let Ok(rd) = std::fs::read_dir(ops_dir(root)) else { return Vec::new() };
    let mut out: Vec<SplitOp> = rd
        .flatten()
        .filter_map(|f| std::fs::read_to_string(f.path()).ok())
        .filter_map(|s| serde_json::from_str::<SplitOp>(&s).ok())
        .filter(|o| o.note_id == note_id && o.phase != phase::DONE && o.phase != phase::CANCELLED)
        .collect();
    out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    out
}

/// 最近一次已完成且未撤销的拆分(撤销入口用):结果横幅被关掉后,撤销必须仍有
/// 处可寻(2026-08-22 一键拆分)。
pub fn latest_undoable_for_note(root: &Path, note_id: &str) -> Option<SplitOp> {
    let rd = std::fs::read_dir(ops_dir(root)).ok()?;
    rd.flatten()
        .filter_map(|f| std::fs::read_to_string(f.path()).ok())
        .filter_map(|s| serde_json::from_str::<SplitOp>(&s).ok())
        .filter(|o| {
            o.note_id == note_id
                && o.phase == phase::DONE
                && o.undone_at.is_none()
                && o.mode == "split_commit"
        })
        .max_by(|a, b| a.created_at.cmp(&b.created_at))
}

/// 带锁推进(命令层便捷入口)。
pub fn advance_guarded(
    store: &super::VoiceprintStore,
    root: &Path,
    op_id: &str,
    from: &[&str],
    to: &str,
    now: &str,
) -> anyhow::Result<SplitOp> {
    store.with_guard(|| advance(root, op_id, from, to, now))
}

/// 全库范围的未完成操作(跨笔记;解除隔离时排除其它 op 仍持有的人物用)。
pub fn open_ops_all(root: &Path) -> Vec<SplitOp> {
    let Ok(rd) = std::fs::read_dir(ops_dir(root)) else { return Vec::new() };
    rd.flatten()
        .filter_map(|f| std::fs::read_to_string(f.path()).ok())
        .filter_map(|s| serde_json::from_str::<SplitOp>(&s).ok())
        .filter(|o| o.phase != phase::DONE && o.phase != phase::CANCELLED)
        .collect()
}

/// 推进阶段并落盘,**带 expected-phase CAS**:当前阶段不在 from 集合内即拒绝——
/// 没有它,并发的 commit/cancel/confirm 可以互相回退或覆盖(codex 实现轮一 P1③)。
/// 该阶段真正做完之后才调(先做后记:没记上就重做,重做幂等)。
pub fn advance(root: &Path, op_id: &str, from: &[&str], to: &str, now: &str) -> anyhow::Result<SplitOp> {
    let mut op = load(root, op_id)?;
    anyhow::ensure!(
        from.contains(&op.phase.as_str()),
        "阶段不符:当前 {},不能推进到 {to}",
        op.phase
    );
    op.phase = to.to_string();
    op.updated_at = now.to_string();
    save(root, &op)?;
    Ok(op)
}
