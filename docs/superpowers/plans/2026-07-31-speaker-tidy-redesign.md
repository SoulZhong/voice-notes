# 会议搭子整理重设计(自动归并+审阅收件箱)实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 高置信声纹归属建议自动合并(逐条可撤销),其余整理事项收进 `/speakers/tidy` 逐条审阅流,三处分散入口收敛为一个收件箱。

**Architecture:** 后端新增合并日志模块(合并前快照双方记录+样本副本,撤销=按快照还原;失效规则由 store 变更入口钩子维护),命令层新增 `apply_confident_merges`/`undo_merge`/`acknowledge_merge`/`list_merge_receipts` 四命令并让 `merge_person` 返回日志 id。前端重构 tidy store(建议+回执双数据源),新增纯逻辑队列/键盘/试听模块与 `/speakers/tidy` 审阅流页面,概览页与侧栏徽标收敛。

**Tech Stack:** Rust (Tauri 2, serde, anyhow, tempfile 测试) + Svelte 5 (runes) + vitest。无新依赖。

**Spec:** `docs/superpowers/specs/2026-07-31-speaker-tidy-redesign-design.md`(在 `docs/speaker-tidy-redesign` 分支上)。

## Global Constraints

- 分支:从 `master` 建 `feature/speaker-tidy-redesign`,先 `git merge docs/speaker-tidy-redesign` 把设计文档与本计划带进来。不得基于 `feature/notes-markdown-editor`(其 PR#65 待合并)。
- 全部用户可见文案为中文;CSS 只用现有变量族(`--surface`/`--ink*`/`--accent*`/`--warning*`/`--danger*`/`--hairline*`/`--radius*`),形态沿用现有 mini 按钮/surface 卡/banner 家族。
- 自动合并阈值 = 现有 strong 档:裸余弦 ≥ 0.74(新常量 `SUGGEST_STRONG_RAW`)或 S-Norm z ≥ 3.0(现有 `SUGGEST_STRONG_Z`),且 loser 未命名,且 pair 不在落盘拒绝名单。
- 合并日志上限 50 条(`JOURNAL_CAP`);拒绝名单键格式 `"loser>winner"`(与前端 `sugKey` 一致)。
- 合并/删除/撤销在录制中一律拒绝(错误文案原样透出);日志写入失败则放弃该条合并。
- 样本文件操作 best-effort、库结构一致性优先(与现有 merge 哲学一致);声纹库 JSON 恢复是撤销的硬要求,样本还原失败只 eprintln。
- 验证命令:`cd src-tauri && cargo test`、`npm test`、`npm run check`,三者全绿才算任务完成。
- Rust 测试沿用 voiceprints.rs 测试模块的既有辅助(`snap()`、`tempfile::tempdir()`);vitest 测试放 `src/lib/*.test.ts`。

## 文件结构总览

| 文件 | 动作 | 职责 |
|---|---|---|
| `src-tauri/src/store/merge_journal.rs` | 新建 | 合并日志:条目落盘/枚举/失效/复活/确认/淘汰 + 自动合并拒绝名单 |
| `src-tauri/src/store/voiceprints.rs` | 修改 | guard 重构、`merge_journaled`、`undo_merge`、`confident_picks`、`SUGGEST_STRONG_RAW`、六个变更入口失效钩子 |
| `src-tauri/src/store/mod.rs` | 修改 | 导出新模块与新符号 |
| `src-tauri/src/ipc.rs` | 修改 | `MergeReceipt`/`ConfidentMergeOutcome` 类型 |
| `src-tauri/src/lib.rs` | 修改 | `do_merge_person` 提炼、`merge_person` 返回 id、四个新命令、注册 |
| `src/lib/people.ts` | 修改 | 新命令前端接口与类型 |
| `src/lib/tidyQueue.ts` | 新建 | 队列构建/跳过排序/键盘命令映射(纯函数) |
| `src/lib/tidyQueue.test.ts` | 新建 | 上述纯函数 vitest |
| `src/lib/tidyAudio.ts` | 新建 | 单实例试听控制器(工厂可注入) |
| `src/lib/tidyAudio.test.ts` | 新建 | 试听互斥/切换/结束回调 vitest |
| `src/lib/tidy.svelte.ts` | 重写 | 建议+回执+会话忽略/保留集,refresh 走自动归并 |
| `src/lib/recording.svelte.ts` | 修改 | 停止时 `peopleVersion++` |
| `src/routes/+layout.svelte` | 修改 | 全局触发 `tidy.refresh()`(启动+每次 peopleVersion 翻转) |
| `src/lib/Sidebar.svelte` | 修改 | 徽标=队列总数 |
| `src/routes/speakers/tidy/+page.svelte` | 新建 | 逐条审阅流页面 |
| `src/routes/speakers/+page.svelte` | 重写 | 删整理卡+同名横幅,换收件箱摘要卡 |
| `src/routes/speakers/[id]/+page.svelte` | 修改 | 手动合并后的「已合并·撤销」条 |
| `DESIGN.md`、`README.md` | 修改 | 文档同步 |

---

### Task 0: 建实施分支

**Files:** 无代码改动。

- [ ] **Step 1: 建分支并带入设计文档**

```bash
cd "$(git rev-parse --show-toplevel)"
git checkout -b feature/speaker-tidy-redesign master
git merge --no-edit docs/speaker-tidy-redesign
```

Expected: 合并进 `docs/superpowers/specs/2026-07-31-speaker-tidy-redesign-design.md` 与本计划,无冲突。

---

### Task 1: 合并日志模块 `merge_journal.rs`

**Files:**
- Create: `src-tauri/src/store/merge_journal.rs`
- Modify: `src-tauri/src/store/mod.rs`(注册模块+导出)
- Test: 模块内 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `super::voiceprints::Person`(已有 pub struct)。
- Produces(后续任务依赖的精确签名):
  - `pub const JOURNAL_CAP: usize = 50`
  - `pub struct MergeJournalEntry { id, time, origin, loser, winner, loser_name, winner_name, similarity: Option<f32>, loser_person: Person, winner_person: Person, redirects_to_loser: Vec<String>, acknowledged: bool, invalid_reason: Option<String>, invalidated_by: Option<String> }`(全 pub 字段,Serialize+Deserialize+Clone+Debug)
  - `pub struct MergeJournal;` `MergeJournal::new(root: PathBuf)`
  - `pub fn entry(&self, id: &str) -> anyhow::Result<MergeJournalEntry>`
  - `pub fn entries(&self) -> Vec<MergeJournalEntry>`(time 降序)
  - `pub fn append(&self, e: &MergeJournalEntry, loser_samples: &[PathBuf], winner_samples: &[PathBuf]) -> anyhow::Result<()>`
  - `pub fn invalidate(&self, touched: &[&str], reason: &str, by: Option<&str>)`
  - `pub fn invalidate_all(&self, reason: &str)`
  - `pub fn revive_invalidated_by(&self, by_id: &str)`
  - `pub fn acknowledge(&self, id: &str) -> anyhow::Result<()>`
  - `pub fn remove(&self, id: &str) -> anyhow::Result<()>`
  - `pub fn restore_samples(&self, id: &str, vp_samples_dir: &Path) -> anyhow::Result<usize>`
  - `pub fn auto_denylist(&self) -> Vec<String>` / `pub fn deny_auto(&self, pair: &str)`

- [ ] **Step 1: 写失败测试**

新建 `src-tauri/src/store/merge_journal.rs`,先只写文件头注释、`use`、数据结构与空 impl 骨架(方法体 `todo!()`),加测试模块:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::voiceprints::Person;

    fn entry(id: &str, time: &str, loser: &str, winner: &str) -> MergeJournalEntry {
        MergeJournalEntry {
            id: id.into(),
            time: time.into(),
            origin: "auto".into(),
            loser: loser.into(),
            winner: winner.into(),
            loser_name: String::new(),
            winner_name: "张三".into(),
            similarity: Some(0.8),
            loser_person: Person::default(),
            winner_person: Person::default(),
            redirects_to_loser: vec![],
            acknowledged: false,
            invalid_reason: None,
            invalidated_by: None,
        }
    }

    fn fake_sample(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, b"RIFFfake-wav").unwrap();
        p
    }

    #[test]
    fn append_and_entries_roundtrip_with_sample_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        let vpdir = tmp.path().join("voiceprints");
        let ls = fake_sample(&vpdir, "P1.wav");
        let ws = fake_sample(&vpdir, "P2.wav");
        j.append(&entry("m-P1", "t1", "P1", "P2"), &[ls], &[ws]).unwrap();

        let got = j.entry("m-P1").unwrap();
        assert_eq!(got.winner, "P2");
        assert!(tmp.path().join("merge_journal/m-P1/samples/loser/P1.wav").exists());
        assert!(tmp.path().join("merge_journal/m-P1/samples/winner/P2.wav").exists());
        assert_eq!(j.entries().len(), 1);
    }

    #[test]
    fn entries_sorted_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        j.append(&entry("m-P1", "2026-07-31T10:00:00+08:00", "P1", "P9"), &[], &[]).unwrap();
        j.append(&entry("m-P2", "2026-07-31T11:00:00+08:00", "P2", "P9"), &[], &[]).unwrap();
        let ids: Vec<String> = j.entries().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, vec!["m-P2", "m-P1"]);
    }

    #[test]
    fn append_fails_cleanly_when_sample_copy_impossible() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        let missing = tmp.path().join("voiceprints/none.wav"); // 不存在
        assert!(j.append(&entry("m-P1", "t", "P1", "P2"), &[missing], &[]).is_err());
        assert!(!tmp.path().join("merge_journal/m-P1").exists(), "半成品目录必须清掉");
        assert!(j.entries().is_empty());
    }

    #[test]
    fn invalidate_marks_touched_skips_by_id_and_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        j.append(&entry("m-P1", "t1", "P1", "P9"), &[], &[]).unwrap();
        j.append(&entry("m-P2", "t2", "P2", "P8"), &[], &[]).unwrap();
        j.append(&entry("m-P3", "t3", "P3", "P9"), &[], &[]).unwrap();

        j.invalidate(&["P9"], "相关人物随后又被合并", Some("m-P3"));
        assert!(j.entry("m-P1").unwrap().invalid_reason.is_some());
        assert_eq!(j.entry("m-P1").unwrap().invalidated_by.as_deref(), Some("m-P3"));
        assert!(j.entry("m-P2").unwrap().invalid_reason.is_none(), "未触及的不失效");
        assert!(j.entry("m-P3").unwrap().invalid_reason.is_none(), "by 自身除外");
    }

    #[test]
    fn invalidate_by_merge_keeps_samples_permanent_op_deletes_them() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        let vpdir = tmp.path().join("voiceprints");
        let s1 = fake_sample(&vpdir, "P1.wav");
        let s2 = fake_sample(&vpdir, "P2.wav");
        j.append(&entry("m-P1", "t1", "P1", "P9"), &[s1], &[]).unwrap();
        j.append(&entry("m-P2", "t2", "P2", "P9"), &[s2], &[]).unwrap();

        // 由另一次合并失效:可复活,样本副本必须保留
        j.invalidate(&["P1"], "相关人物随后又被合并", Some("m-X"));
        assert!(tmp.path().join("merge_journal/m-P1/samples/loser/P1.wav").exists());
        // 永久性操作失效(by=None):不可复活,样本副本删掉省空间
        j.invalidate(&["P2"], "此人随后被改名", None);
        assert!(!tmp.path().join("merge_journal/m-P2/samples").exists());
    }

    #[test]
    fn revive_restores_entries_invalidated_by_given_id_only() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        j.append(&entry("m-P1", "t1", "P1", "P9"), &[], &[]).unwrap();
        j.append(&entry("m-P2", "t2", "P2", "P8"), &[], &[]).unwrap();
        j.invalidate(&["P9"], "相关人物随后又被合并", Some("m-X"));
        j.invalidate(&["P8"], "此人随后被改名", None);

        j.revive_invalidated_by("m-X");
        assert!(j.entry("m-P1").unwrap().invalid_reason.is_none(), "由 m-X 失效的复活");
        assert!(j.entry("m-P2").unwrap().invalid_reason.is_some(), "永久失效的不复活");
    }

    #[test]
    fn acknowledge_removes_entry_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        j.append(&entry("m-P1", "t1", "P1", "P2"), &[], &[]).unwrap();
        j.acknowledge("m-P1").unwrap();
        assert!(j.entries().is_empty());
        assert!(!tmp.path().join("merge_journal/m-P1").exists());
    }

    #[test]
    fn prune_caps_at_journal_cap_dropping_oldest() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        for i in 0..(JOURNAL_CAP + 3) {
            let id = format!("m-P{i}");
            let time = format!("2026-07-31T10:{:02}:{:02}+08:00", i / 60, i % 60);
            j.append(&entry(&id, &time, &format!("P{i}"), "W"), &[], &[]).unwrap();
        }
        let entries = j.entries();
        assert_eq!(entries.len(), JOURNAL_CAP);
        assert!(entries.iter().all(|e| e.id != "m-P0"), "最旧的被淘汰");
    }

    #[test]
    fn illegal_id_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        assert!(j.entry("../evil").is_err());
        assert!(j.acknowledge("a/b").is_err());
    }

    #[test]
    fn denylist_roundtrip_and_dedup() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        assert!(j.auto_denylist().is_empty());
        j.deny_auto("P1>P2");
        j.deny_auto("P1>P2");
        j.deny_auto("P3>P4");
        assert_eq!(j.auto_denylist(), vec!["P1>P2".to_string(), "P3>P4".to_string()]);
    }

    #[test]
    fn restore_samples_copies_back_both_sides() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        let vpdir = tmp.path().join("voiceprints");
        let ls = fake_sample(&vpdir, "P1.wav");
        let ws = fake_sample(&vpdir, "P2.wav");
        j.append(&entry("m-P1", "t1", "P1", "P2"), &[ls.clone()], &[ws.clone()]).unwrap();
        std::fs::remove_file(&ls).unwrap();
        std::fs::remove_file(&ws).unwrap();

        let n = j.restore_samples("m-P1", &vpdir).unwrap();
        assert_eq!(n, 2);
        assert!(ls.exists() && ws.exists());
    }
}
```

`src-tauri/src/store/mod.rs` 顶部模块声明区加一行(按字母序,`pub mod audio;` 前后):

```rust
pub mod merge_journal;
```

导出区(`pub use voiceprints::…` 附近)加:

```rust
pub use merge_journal::{MergeJournal, MergeJournalEntry}; // 合并日志(lib.rs 命令层消费)。
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test store::merge_journal`
Expected: 编译失败(`todo!()` 可编译,但若骨架未写全则报缺项)或全部 panic at `todo!()`。

- [ ] **Step 3: 实现模块**

`merge_journal.rs` 完整实现(替换骨架方法体):

```rust
//! 合并日志:声纹合并的"可撤销"层。merge 本身不可逆(质心加权平均、样本超额永久
//! 删除、redirect 压扁),因此每条合并前把双方 Person 快照 + 样本文件副本落到
//! merge_journal/<id>/,撤销=按快照还原(还原逻辑在 voiceprints.rs undo_merge,
//! 那里有库的 load/save/guard)。另存自动合并拒绝名单:撤销过的 pair 落盘,自动
//! 归并不再碰——重启也不犯同样的错。
//! 设计:docs/superpowers/specs/2026-07-31-speaker-tidy-redesign-design.md
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::voiceprints::Person;

/// 日志条目上限:超出按 time 淘汰最旧(连同样本副本)。50 条远超一轮整理的量,
/// 上限只是防无界膨胀。
pub const JOURNAL_CAP: usize = 50;

/// 一条合并日志。id 格式 `m-<loser>`:loser 合并后即从库中消失,同一 loser 不会
/// 有两条并存(撤销会删条目,重新合并再落新条),天然唯一。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeJournalEntry {
    pub id: String,
    /// RFC3339 落盘时间(排序/淘汰用,由命令层传入)。
    pub time: String,
    /// "auto" | "manual"。auto 且未确认的条目进审阅流回执卡。
    pub origin: String,
    pub loser: String,
    pub winner: String,
    /// 合并前双方显示名(回执展示用;合并后 loser 已不在库里,无从回查)。
    pub loser_name: String,
    pub winner_name: String,
    #[serde(default)]
    pub similarity: Option<f32>,
    /// 合并前双方完整记录(撤销恢复用)。
    pub loser_person: Person,
    pub winner_person: Person,
    /// 合并前指向 loser 的 redirect 键(merge 压扁会改指 winner,撤销要还原)。
    #[serde(default)]
    pub redirects_to_loser: Vec<String>,
    /// 已确认(回执卡点「好」后删除条目,此字段区分 manual 生来已确认)。
    #[serde(default)]
    pub acknowledged: bool,
    /// 失效原因;None=可撤销。失效条目留着给回执卡说明原因,确认后才删。
    #[serde(default)]
    pub invalid_reason: Option<String>,
    /// 使其失效的后续合并 journal id(仅失效源是另一次合并时有值):撤销那次合并
    /// 时本条复活——链式撤销 LIFO 可行的关键。
    #[serde(default)]
    pub invalidated_by: Option<String>,
}

/// root 为 app_data_dir(与 VoiceprintStore 同根),日志落 root/merge_journal/。
/// 并发:所有调用方(voiceprints.rs 各方法/命令层)都在 vp_guard 内或单线程命令
/// 上下文中操作,模块自身不再加锁。
pub struct MergeJournal {
    root: PathBuf,
}

impl MergeJournal {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn dir(&self) -> PathBuf {
        self.root.join("merge_journal")
    }

    /// 防御 IPC 构造路径:id 只允许 ASCII 字母数字与 '-'。
    fn entry_dir(&self, id: &str) -> Option<PathBuf> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return None;
        }
        Some(self.dir().join(id))
    }

    fn entry_path(&self, id: &str) -> Option<PathBuf> {
        self.entry_dir(id).map(|d| d.join("entry.json"))
    }

    fn samples_dir(&self, id: &str, side: &str) -> Option<PathBuf> {
        self.entry_dir(id).map(|d| d.join("samples").join(side))
    }

    pub fn entry(&self, id: &str) -> anyhow::Result<MergeJournalEntry> {
        let p = self.entry_path(id).ok_or_else(|| anyhow::anyhow!("非法日志 id: {id}"))?;
        let s = std::fs::read_to_string(&p).map_err(|_| anyhow::anyhow!("日志条目不存在: {id}"))?;
        Ok(serde_json::from_str(&s)?)
    }

    /// 全部条目,time 降序(新的在前)。损坏条目跳过(eprintln)——日志是增值层,
    /// 单条坏不该挡整理。
    pub fn entries(&self) -> Vec<MergeJournalEntry> {
        let Ok(rd) = std::fs::read_dir(self.dir()) else { return vec![] };
        let mut out: Vec<MergeJournalEntry> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| {
                let s = std::fs::read_to_string(e.path().join("entry.json")).ok()?;
                match serde_json::from_str(&s) {
                    Ok(v) => Some(v),
                    Err(err) => {
                        eprintln!("合并日志条目损坏,跳过({:?}): {err}", e.path());
                        None
                    }
                }
            })
            .collect();
        out.sort_by(|a, b| b.time.cmp(&a.time));
        out
    }

    /// 原子写条目 JSON(tmp+rename,与库文件同哲学)。
    fn save_entry(&self, e: &MergeJournalEntry) -> anyhow::Result<()> {
        let path = self.entry_path(&e.id).ok_or_else(|| anyhow::anyhow!("非法日志 id: {}", e.id))?;
        std::fs::create_dir_all(path.parent().expect("entry_path 恒有父目录"))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(e)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// 落一条新日志:entry.json + 双方样本文件副本。任一步失败→清掉半成品目录并
    /// Err(调用方据此放弃该条合并——绝不做没有退路的合并)。成功后按上限淘汰最旧。
    pub fn append(
        &self,
        e: &MergeJournalEntry,
        loser_samples: &[PathBuf],
        winner_samples: &[PathBuf],
    ) -> anyhow::Result<()> {
        let res = (|| -> anyhow::Result<()> {
            self.save_entry(e)?;
            for (side, files) in [("loser", loser_samples), ("winner", winner_samples)] {
                let dir = self
                    .samples_dir(&e.id, side)
                    .ok_or_else(|| anyhow::anyhow!("非法日志 id: {}", e.id))?;
                std::fs::create_dir_all(&dir)?;
                for f in files {
                    let name = f.file_name().ok_or_else(|| anyhow::anyhow!("样本路径无文件名: {f:?}"))?;
                    std::fs::copy(f, dir.join(name))?;
                }
            }
            Ok(())
        })();
        if let Err(err) = res {
            if let Some(d) = self.entry_dir(&e.id) {
                let _ = std::fs::remove_dir_all(&d);
            }
            return Err(err);
        }
        self.prune();
        Ok(())
    }

    /// 触及 touched 中任一人物的**有效**条目全部失效。by=使其失效的合并 id(该
    /// 合并被撤销时本条复活,样本副本保留);by=None 为永久性失效(改名/删除/新
    /// 录制等),样本副本删掉省空间。best-effort:失效是保护层,写失败只 eprintln
    /// 不打断主流程(宁可少一个可撤销项,不挡合并/录制)。
    pub fn invalidate(&self, touched: &[&str], reason: &str, by: Option<&str>) {
        for mut e in self.entries() {
            if e.invalid_reason.is_some() {
                continue;
            }
            if !touched.contains(&e.loser.as_str()) && !touched.contains(&e.winner.as_str()) {
                continue;
            }
            if Some(e.id.as_str()) == by {
                continue; // 合并自身的条目不被自己失效
            }
            e.invalid_reason = Some(reason.to_string());
            e.invalidated_by = by.map(str::to_string);
            if let Err(err) = self.save_entry(&e) {
                eprintln!("合并日志失效标记写入失败({}): {err}", e.id);
                continue;
            }
            if by.is_none() {
                if let Some(d) = self.entry_dir(&e.id) {
                    let _ = std::fs::remove_dir_all(d.join("samples"));
                }
            }
        }
    }

    /// 全部有效条目失效(声纹库整体重建等场景)。永久失效,样本副本删除。
    pub fn invalidate_all(&self, reason: &str) {
        let ids: Vec<String> = self
            .entries()
            .iter()
            .filter(|e| e.invalid_reason.is_none())
            .flat_map(|e| [e.loser.clone(), e.winner.clone()])
            .collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        self.invalidate(&refs, reason, None);
    }

    /// 由 by_id 那次合并所失效的条目复活(撤销 by_id = 它施加的失效不再成立)。
    pub fn revive_invalidated_by(&self, by_id: &str) {
        for mut e in self.entries() {
            if e.invalidated_by.as_deref() != Some(by_id) {
                continue;
            }
            e.invalid_reason = None;
            e.invalidated_by = None;
            if let Err(err) = self.save_entry(&e) {
                eprintln!("合并日志复活写入失败({}): {err}", e.id);
            }
        }
    }

    /// 确认(回执卡「好」)= 删除条目(连同样本副本)。
    pub fn acknowledge(&self, id: &str) -> anyhow::Result<()> {
        self.remove(id)
    }

    pub fn remove(&self, id: &str) -> anyhow::Result<()> {
        let _ = self.entry(id)?; // 校验存在与 id 合法性
        let dir = self.entry_dir(id).expect("entry() 已校验合法");
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// 超上限淘汰最旧(entries 已按 time 降序)。
    fn prune(&self) {
        let entries = self.entries();
        for e in entries.iter().skip(JOURNAL_CAP) {
            if let Some(d) = self.entry_dir(&e.id) {
                let _ = std::fs::remove_dir_all(&d);
            }
        }
    }

    /// 把条目的样本副本拷回声纹样本目录(撤销用),返回还原文件数。
    pub fn restore_samples(&self, id: &str, vp_samples_dir: &Path) -> anyhow::Result<usize> {
        std::fs::create_dir_all(vp_samples_dir)?;
        let mut n = 0usize;
        for side in ["loser", "winner"] {
            let Some(dir) = self.samples_dir(id, side) else { continue };
            let Ok(rd) = std::fs::read_dir(&dir) else { continue }; // 该侧无样本,正常
            for f in rd.flatten() {
                std::fs::copy(f.path(), vp_samples_dir.join(f.file_name()))?;
                n += 1;
            }
        }
        Ok(n)
    }

    // ── 自动合并拒绝名单(撤销过的 pair,落盘;仅屏蔽自动合并,不屏蔽人工建议) ──

    fn denylist_path(&self) -> PathBuf {
        self.dir().join("auto_denylist.json")
    }

    /// 缺失/损坏 → 空(拒绝名单是保护层,坏了顶多多合一次,撤销还在)。
    pub fn auto_denylist(&self) -> Vec<String> {
        std::fs::read_to_string(self.denylist_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// 追加 pair(格式 "loser>winner",与前端 sugKey 一致),去重。best-effort。
    pub fn deny_auto(&self, pair: &str) {
        let mut list = self.auto_denylist();
        if list.iter().any(|p| p == pair) {
            return;
        }
        list.push(pair.to_string());
        let res = (|| -> anyhow::Result<()> {
            std::fs::create_dir_all(self.dir())?;
            let tmp = self.denylist_path().with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_string_pretty(&list)?)?;
            std::fs::rename(&tmp, self.denylist_path())?;
            Ok(())
        })();
        if let Err(e) = res {
            eprintln!("自动合并拒绝名单写入失败: {e}");
        }
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test store::merge_journal`
Expected: 11 个测试全 PASS。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/store/merge_journal.rs src-tauri/src/store/mod.rs
git commit -m "feat(store): 合并日志模块(快照/失效/复活/拒绝名单)"
```

---

### Task 2: voiceprints——guard 重构、`merge_journaled`、strong 常量、`confident_picks`

**Files:**
- Modify: `src-tauri/src/store/voiceprints.rs`
- Modify: `src-tauri/src/store/mod.rs`(导出)
- Test: voiceprints.rs 测试模块追加

**Interfaces:**
- Consumes: Task 1 的 `MergeJournal`/`MergeJournalEntry`。
- Produces:
  - `pub const SUGGEST_STRONG_RAW: f32 = 0.74`
  - `pub fn merge_journaled(&self, loser: &str, winner: &str, embedder: Option<&mut dyn crate::diar::SpeakerEmbedder>, origin: &str, similarity: Option<f32>, now: &str) -> anyhow::Result<String>`(返回 journal id,格式 `m-<loser>`;manual 条目 `acknowledged=true`)
  - `pub fn confident_picks(vp: &Voiceprints, sugs: Vec<MergeSuggestion>, deny: &[String]) -> (Vec<MergeSuggestion>, Vec<MergeSuggestion>)`(返回 `(可自动, 留人工)`)

- [ ] **Step 1: 写失败测试**

在 voiceprints.rs 测试模块(`mod tests`)末尾追加(`snap()` 辅助已存在;样本辅助新增):

```rust
    fn write_fake_sample(root: &std::path::Path, name: &str) {
        let d = root.join("voiceprints");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(name), b"RIFFfake-wav").unwrap();
    }

    /// 造两个人:P1 未命名 [1,0],P2 命名"张三" [0,1],各一份样本文件。
    fn two_people_store(tmp: &tempfile::TempDir) -> VoiceprintStore {
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![
            snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
        ];
        store.upsert_from_session(&snaps, "2026-07-31T10:00:00+08:00").unwrap();
        store.rename("P2", "张三").unwrap();
        write_fake_sample(tmp.path(), "P1.wav");
        write_fake_sample(tmp.path(), "P2.wav");
        store
    }

    #[test]
    fn merge_journaled_merges_and_snapshots() {
        let tmp = tempfile::tempdir().unwrap();
        let store = two_people_store(&tmp);
        let jid = store
            .merge_journaled("P1", "P2", None, "auto", Some(0.9), "2026-07-31T11:00:00+08:00")
            .unwrap();
        assert_eq!(jid, "m-P1");

        // 合并生效
        let vp = store.load();
        assert!(!vp.people.contains_key("P1"));
        assert_eq!(vp.redirects.get("P1").map(String::as_str), Some("P2"));

        // 日志条目完整:双方快照、合并前名字、样本副本
        let j = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        let e = j.entry(&jid).unwrap();
        assert_eq!(e.loser_name, "");
        assert_eq!(e.winner_name, "张三");
        assert_eq!(e.loser_person.centroids["mic"].count, 5);
        assert!(!e.acknowledged, "auto 条目未确认,进回执队列");
        assert!(tmp.path().join("merge_journal/m-P1/samples/loser/P1.wav").exists());
    }

    #[test]
    fn merge_journaled_manual_is_preacknowledged() {
        let tmp = tempfile::tempdir().unwrap();
        let store = two_people_store(&tmp);
        let jid = store.merge_journaled("P1", "P2", None, "manual", None, "t").unwrap();
        let j = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        assert!(j.entry(&jid).unwrap().acknowledged, "manual 不进回执队列,撤销走页内撤销条");
    }

    #[test]
    fn merge_journaled_invalidates_prior_entries_touching_same_people() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![
            snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S3", vec![0.7, 0.7], 5, &["mic"], None, AUTO_ENROLL_MS),
        ];
        store.upsert_from_session(&snaps, "t0").unwrap(); // P1 P2 P3,均未命名
        let j1 = store.merge_journaled("P1", "P2", None, "auto", None, "t1").unwrap();
        let j2 = store.merge_journaled("P3", "P2", None, "auto", None, "t2").unwrap();

        let j = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        let e1 = j.entry(&j1).unwrap();
        assert!(e1.invalid_reason.is_some(), "P2 随后又被合并,j1 快照过时");
        assert_eq!(e1.invalidated_by.as_deref(), Some(j2.as_str()), "记下失效源,撤销 j2 时复活");
        assert!(j.entry(&j2).unwrap().invalid_reason.is_none());
    }

    #[test]
    fn confident_picks_partitions_by_strength_name_denylist_and_touch() {
        let mut vp = Voiceprints::default();
        for (id, name) in [("P1", ""), ("P2", "张三"), ("P3", ""), ("P4", ""), ("P5", "李四"), ("P6", "")] {
            vp.people.insert(id.into(), Person { name: name.into(), ..Default::default() });
        }
        let s = |loser: &str, winner: &str, sim: f32, z: Option<f32>| MergeSuggestion {
            loser: loser.into(),
            winner: winner.into(),
            similarity: sim,
            source: "mic".into(),
            salience: z,
        };
        let sugs = vec![
            s("P1", "P2", 0.80, None),        // strong(裸分)→ 自动
            s("P3", "P2", 0.90, None),        // strong 但 P2 本轮已被触及 → 顺延人工
            s("P4", "P5", 0.70, Some(3.5)),   // strong(z 档)但在拒绝名单 → 人工
            s("P6", "P5", 0.70, Some(2.0)),   // 不够 strong → 人工
        ];
        let deny = vec!["P4>P5".to_string()];
        let (autos, manual) = confident_picks(&vp, sugs, &deny);
        assert_eq!(autos.iter().map(|x| x.loser.as_str()).collect::<Vec<_>>(), vec!["P1"]);
        assert_eq!(manual.len(), 3);
    }

    #[test]
    fn confident_picks_skips_named_losers() {
        let mut vp = Voiceprints::default();
        vp.people.insert("P1".into(), Person { name: "王五".into(), ..Default::default() });
        vp.people.insert("P2".into(), Person { name: "张三".into(), ..Default::default() });
        let sugs = vec![MergeSuggestion {
            loser: "P1".into(),
            winner: "P2".into(),
            similarity: 0.95,
            source: "mic".into(),
            salience: None,
        }];
        let (autos, manual) = confident_picks(&vp, sugs, &[]);
        assert!(autos.is_empty(), "已命名条目不自动动");
        assert_eq!(manual.len(), 1);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test store::voiceprints`
Expected: 编译失败——`merge_journaled`/`confident_picks` 未定义。

- [ ] **Step 3: 实现**

3a. 常量区(`SUGGEST_STRONG_Z` 下方,约 668 行)加:

```rust
/// "很可能"徽标的裸余弦档(与前端 tidy.svelte.ts isStrong 同值);自动归并准入
/// 与展示徽标共用同一 strong 档——用户已信任"很可能",且撤销机制兜底。
pub const SUGGEST_STRONG_RAW: f32 = 0.74;
```

3b. guard 重构:`merge_with_embedder` 现有函数体首行 `let _guard = vp_guard();` 删除,整个函数改名为私有 `fn merge_locked(&self, loser: &str, winner: &str, mut embedder: Option<&mut dyn crate::diar::SpeakerEmbedder>) -> anyhow::Result<()>`(doc 注释挪过去,补一句"调用方必须已持 vp_guard");在其上方新增同签名公开包装:

```rust
    pub fn merge_with_embedder(
        &self,
        loser: &str,
        winner: &str,
        embedder: Option<&mut dyn crate::diar::SpeakerEmbedder>,
    ) -> anyhow::Result<()> {
        let _guard = vp_guard();
        self.merge_locked(loser, winner, embedder)
    }
```

3c. `merge_journaled`(放在 `merge_with_embedder` 之后):

```rust
    /// 合并 + 撤销日志:合并前把双方快照与样本副本落入合并日志,再执行合并;返回
    /// 日志条目 id(`m-<loser>`)。日志写不进去就不合并(Err)——绝不做没有退路的
    /// 合并。快照与合并同持 vp_guard,中间不会插入其他变更。
    pub fn merge_journaled(
        &self,
        loser: &str,
        winner: &str,
        embedder: Option<&mut dyn crate::diar::SpeakerEmbedder>,
        origin: &str,
        similarity: Option<f32>,
        now: &str,
    ) -> anyhow::Result<String> {
        let _guard = vp_guard();
        let vp = self.load();
        let loser_person =
            vp.people.get(loser).ok_or_else(|| anyhow::anyhow!("未知人物: {loser}"))?.clone();
        let winner_person =
            vp.people.get(winner).ok_or_else(|| anyhow::anyhow!("未知人物: {winner}"))?.clone();
        let redirects_to_loser: Vec<String> =
            vp.redirects.iter().filter(|(_, t)| t.as_str() == loser).map(|(k, _)| k.clone()).collect();
        let entry = super::merge_journal::MergeJournalEntry {
            id: format!("m-{loser}"),
            time: now.to_string(),
            origin: origin.to_string(),
            loser: loser.to_string(),
            winner: winner.to_string(),
            loser_name: loser_person.name.clone(),
            winner_name: winner_person.name.clone(),
            similarity,
            loser_person,
            winner_person,
            redirects_to_loser,
            // 手动合并生来已确认:不进回执队列,撤销走页内撤销条(行为仍统一可撤)。
            acknowledged: origin == "manual",
            invalid_reason: None,
            invalidated_by: None,
        };
        let journal = super::merge_journal::MergeJournal::new(self.root.clone());
        journal.append(&entry, &self.sample_paths_existing(loser), &self.sample_paths_existing(winner))?;
        if let Err(e) = self.merge_locked(loser, winner, embedder) {
            // 合并没做成,日志不留——否则可"撤销"一次没发生的合并。
            let _ = journal.remove(&entry.id);
            return Err(e);
        }
        // 本次合并使触及双方的既有可撤销条目失效(不含自己):它们的快照已过时。
        // by=本条 id → 撤销本次合并时那些条目复活(LIFO 链式撤销)。
        journal.invalidate(&[loser, winner], "相关人物随后又被合并", Some(&entry.id));
        Ok(entry.id)
    }
```

3d. `confident_picks`(放在 `suggest_merges` 之后,同为自由函数):

```rust
/// 自动归并筛选(纯函数):strong 档(裸余弦 ≥ SUGGEST_STRONG_RAW 或 z ≥
/// SUGGEST_STRONG_Z)+ loser 未命名(已命名条目不自动动)+ 不在拒绝名单 + 双方
/// 未被本轮更早的自动合并触及(同一 winner 一轮只吃一条:第二条会使第一条的回
/// 执立即失效,顺延到下一轮重算后再合)。返回 (可自动合并, 留给人工)。
pub fn confident_picks(
    vp: &Voiceprints,
    sugs: Vec<MergeSuggestion>,
    deny: &[String],
) -> (Vec<MergeSuggestion>, Vec<MergeSuggestion>) {
    let mut autos = Vec::new();
    let mut manual = Vec::new();
    let mut touched: std::collections::BTreeSet<String> = Default::default();
    for s in sugs {
        let strong = s.similarity >= SUGGEST_STRONG_RAW
            || s.salience.map_or(false, |z| z >= SUGGEST_STRONG_Z);
        let unnamed = vp.people.get(&s.loser).map_or(false, |p| p.name.is_empty());
        let denied = deny.iter().any(|d| d == &format!("{}>{}", s.loser, s.winner));
        if strong && unnamed && !denied && !touched.contains(&s.loser) && !touched.contains(&s.winner)
        {
            touched.insert(s.loser.clone());
            touched.insert(s.winner.clone());
            autos.push(s);
        } else {
            manual.push(s);
        }
    }
    (autos, manual)
}
```

3e. `store/mod.rs` 导出区追加:

```rust
pub use voiceprints::confident_picks; // 自动归并筛选(apply_confident_merges 命令消费)。
pub use voiceprints::MergeSuggestion; // confident_picks 出入参类型(lib.rs 消费)。
pub use voiceprints::{SUGGEST_STRONG_RAW, SUGGEST_STRONG_Z}; // strong 档(前端 isStrong 对齐)。
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test store::voiceprints`
Expected: 新增 5 测试 PASS,既有 merge 测试(走 `merge_with_embedder`/`merge`)不回归。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/store/voiceprints.rs src-tauri/src/store/mod.rs
git commit -m "feat(store): merge_journaled 快照合并与 confident_picks 自动归并筛选"
```

---

### Task 3: voiceprints——`undo_merge`

**Files:**
- Modify: `src-tauri/src/store/voiceprints.rs`
- Test: voiceprints.rs 测试模块追加

**Interfaces:**
- Produces: `pub fn undo_merge(&self, journal_id: &str) -> anyhow::Result<()>`(失效条目 → Err 带原因文案"不能撤销:…";成功后 pair 落盘拒绝名单、条目删除、由它失效的条目复活)。

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn undo_merge_restores_records_redirects_samples_and_denylists() {
        let tmp = tempfile::tempdir().unwrap();
        let store = two_people_store(&tmp);
        let before = store.load();
        let jid = store.merge_journaled("P1", "P2", None, "auto", Some(0.9), "t1").unwrap();

        store.undo_merge(&jid).unwrap();

        let vp = store.load();
        assert_eq!(vp.people["P1"], before.people["P1"], "loser 完整还原");
        assert_eq!(vp.people["P2"], before.people["P2"], "winner 完整还原");
        assert!(vp.redirects.is_empty(), "P1->P2 重定向撤掉");
        assert!(tmp.path().join("voiceprints/P1.wav").exists(), "loser 样本文件还原");
        assert!(tmp.path().join("voiceprints/P2.wav").exists());
        assert!(!tmp.path().join("voiceprints/P2-2.wav").exists(), "合并迁移进来的样本清掉");

        let j = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        assert!(j.entries().is_empty(), "条目撤销后删除");
        assert_eq!(j.auto_denylist(), vec!["P1>P2".to_string()], "同样的自动判断不犯第二次");
    }

    #[test]
    fn undo_merge_restores_prior_redirects_to_loser() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![
            snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S3", vec![0.7, 0.7], 5, &["mic"], None, AUTO_ENROLL_MS),
        ];
        store.upsert_from_session(&snaps, "t0").unwrap(); // P1 P2 P3
        store.merge("P3", "P1").unwrap(); // P3 -> P1(历史合并,无日志)
        let jid = store.merge_journaled("P1", "P2", None, "auto", None, "t1").unwrap();
        assert_eq!(store.load().redirects.get("P3").map(String::as_str), Some("P2"), "压扁改指 P2");

        store.undo_merge(&jid).unwrap();
        let vp = store.load();
        assert_eq!(vp.redirects.get("P3").map(String::as_str), Some("P1"), "压扁还原回 P1");
        assert!(!vp.redirects.contains_key("P1"));
    }

    #[test]
    fn undo_merge_rejects_invalidated_entry_with_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let store = two_people_store(&tmp);
        let jid = store.merge_journaled("P1", "P2", None, "auto", None, "t1").unwrap();
        let j = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        j.invalidate(&["P2"], "此人随后被改名", None);

        let err = store.undo_merge(&jid).unwrap_err().to_string();
        assert!(err.contains("不能撤销"), "拒绝并带原因: {err}");
        assert!(err.contains("此人随后被改名"));
    }

    #[test]
    fn chained_undo_lifo_via_revive() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![
            snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S3", vec![0.7, 0.7], 5, &["mic"], None, AUTO_ENROLL_MS),
        ];
        store.upsert_from_session(&snaps, "t0").unwrap();
        let before = store.load();
        let j1 = store.merge_journaled("P1", "P2", None, "auto", None, "t1").unwrap();
        let j2 = store.merge_journaled("P3", "P2", None, "auto", None, "t2").unwrap();

        store.undo_merge(&j1).unwrap_err(); // j1 已被 j2 失效
        store.undo_merge(&j2).unwrap(); // 后进先出:先撤 j2
        let j = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        assert!(j.entry(&j1).unwrap().invalid_reason.is_none(), "j2 撤销后 j1 复活");
        store.undo_merge(&j1).unwrap(); // 再撤 j1,库回到最初
        assert_eq!(store.load().people, before.people);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test store::voiceprints::tests::undo`
Expected: 编译失败——`undo_merge` 未定义。

- [ ] **Step 3: 实现**(`merge_journaled` 之后)

```rust
    /// 按日志条目撤销一次合并:恢复双方记录与 redirects、还原样本副本、pair 落盘
    /// 进自动合并拒绝名单(同样的自动判断不犯第二次,重启也不犯)、由本次被撤销
    /// 合并所失效的旧条目复活(LIFO 链式撤销)。条目已失效 → Err 带原因。
    /// 库记录恢复为硬要求;样本文件 best-effort(与 merge 同哲学:库结构一致性优先)。
    pub fn undo_merge(&self, journal_id: &str) -> anyhow::Result<()> {
        let _guard = vp_guard();
        let journal = super::merge_journal::MergeJournal::new(self.root.clone());
        let entry = journal.entry(journal_id)?;
        if let Some(reason) = &entry.invalid_reason {
            anyhow::bail!("不能撤销:{reason}");
        }
        let mut vp = self.load();
        vp.people.insert(entry.loser.clone(), entry.loser_person.clone());
        vp.people.insert(entry.winner.clone(), entry.winner_person.clone());
        vp.redirects.remove(&entry.loser);
        for k in &entry.redirects_to_loser {
            vp.redirects.insert(k.clone(), entry.loser.clone());
        }
        self.save(&vp)?;
        // 样本还原:双方现存文件清掉(含合并时迁移/兜底截取的),快照副本拷回。
        for id in [entry.loser.as_str(), entry.winner.as_str()] {
            for p in self.sample_paths_existing(id) {
                if let Err(e) = std::fs::remove_file(&p) {
                    eprintln!("撤销合并:清理现存样本失败({id},不影响库): {e}");
                }
            }
        }
        if let Err(e) = journal.restore_samples(journal_id, &self.root.join("voiceprints")) {
            eprintln!("撤销合并:样本副本还原失败(不影响库): {e}");
        }
        journal.deny_auto(&format!("{}>{}", entry.loser, entry.winner));
        journal.remove(journal_id)?;
        journal.revive_invalidated_by(journal_id);
        Ok(())
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test store::voiceprints`
Expected: 新增 4 测试 PASS。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/store/voiceprints.rs
git commit -m "feat(store): undo_merge 按日志快照撤销合并(拒绝名单+LIFO 复活)"
```

---

### Task 4: 变更入口失效钩子

**Files:**
- Modify: `src-tauri/src/store/voiceprints.rs`(6 个方法)
- Test: voiceprints.rs 测试模块追加

**Interfaces:**
- Produces: `append_sample` 语义不变;新增 `pub fn append_sample_for_merge(&self, id: &str, samples: &[f32]) -> anyhow::Result<bool>`(同 append_sample 但**不**触发失效——合并兜底样本是合并动作的一部分,不能反手把刚落的日志灭了;Task 5 的 `do_merge_person` 消费)。

- [ ] **Step 1: 写失败测试**

```rust
    /// 造好合并日志条目后执行某操作,断言条目失效与否。
    fn journaled_entry(store: &VoiceprintStore, tmp: &tempfile::TempDir) -> String {
        let jid = store.merge_journaled("P1", "P2", None, "auto", None, "t1").unwrap();
        let j = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        assert!(j.entry(&jid).unwrap().invalid_reason.is_none());
        jid
    }

    fn assert_invalidated(tmp: &tempfile::TempDir, jid: &str, reason_part: &str) {
        let j = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        let e = j.entry(jid).unwrap();
        let r = e.invalid_reason.expect("应已失效");
        assert!(r.contains(reason_part), "原因不符: {r}");
        assert!(e.invalidated_by.is_none(), "非合并失效不可复活");
    }

    #[test]
    fn rename_invalidates_touching_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let store = two_people_store(&tmp);
        let jid = journaled_entry(&store, &tmp);
        store.rename("P2", "李四").unwrap();
        assert_invalidated(&tmp, &jid, "改名");
    }

    #[test]
    fn delete_invalidates_touching_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let store = two_people_store(&tmp);
        let jid = journaled_entry(&store, &tmp);
        store.delete("P2").unwrap();
        assert_invalidated(&tmp, &jid, "删除");
    }

    #[test]
    fn upsert_invalidates_entries_of_people_who_spoke_again() {
        let tmp = tempfile::tempdir().unwrap();
        let store = two_people_store(&tmp);
        let jid = journaled_entry(&store, &tmp);
        // P2(经 redirect 解析)又录了一场
        let snaps = vec![snap("S9", vec![0.0, 1.0], 3, &["mic"], Some("P2"), 20_000)];
        store.upsert_from_session(&snaps, "t2").unwrap();
        assert_invalidated(&tmp, &jid, "新会议");
    }

    #[test]
    fn append_and_delete_sample_invalidate_but_merge_variant_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        let store = two_people_store(&tmp);
        let jid = journaled_entry(&store, &tmp);
        let j = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());

        // 合并专用变体:不失效(兜底样本是合并动作的一部分)
        store.append_sample_for_merge("P2", &[0.1f32; 1600]).unwrap();
        assert!(j.entry(&jid).unwrap().invalid_reason.is_none());
        // 普通 append:失效
        store.append_sample("P2", &[0.1f32; 1600]).unwrap();
        assert_invalidated(&tmp, &jid, "样本");
    }

    #[test]
    fn delete_sample_invalidates() {
        let tmp = tempfile::tempdir().unwrap();
        let store = two_people_store(&tmp);
        let jid = journaled_entry(&store, &tmp);
        let path = store.sample_paths_existing("P2")[0].clone();
        store.delete_sample("P2", &path).unwrap();
        assert_invalidated(&tmp, &jid, "样本");
    }
```

注意:`snap()` 的第 5 参是 `person: Option<&str>`——沿用测试模块现有签名(见既有用例 `snap("S1", vec![…], 5, &["mic"], None, AUTO_ENROLL_MS)`;若实际形参顺序不同,以现文件为准调整调用)。`rebuild_for_model` 需要嵌入器实例,单测不易构造,失效钩子以人工检查+编译保证(invalidate_all 本体已在 Task 1 测过)。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test store::voiceprints`
Expected: `append_sample_for_merge` 未定义编译失败;或(实现骨架后)invalidate 断言失败。

- [ ] **Step 3: 实现钩子**

3a. voiceprints.rs 内加私有辅助(impl 块内):

```rust
    /// 变更入口统一失效钩子:触及这些人物的合并日志条目不可再撤销(快照已过时)。
    fn journal_invalidate(&self, touched: &[&str], reason: &str) {
        super::merge_journal::MergeJournal::new(self.root.clone()).invalidate(touched, reason, None);
    }
```

3b. 各方法在成功保存后追加调用(返回 Ok 之前):

- `rename`:`self.save(&vp)?;` 改为:

```rust
        self.save(&vp)?;
        self.journal_invalidate(&[id], "此人随后被改名");
        Ok(())
```

- `delete`:`self.save(&vp)?;` 之后(样本删除循环之前)插入 `self.journal_invalidate(&[id], "此人随后被删除");`
- `upsert_from_session`:循环里 `Some(person_id)` 分支中 `resolved` 确定后收集 `touched.push(resolved.clone());`(函数开头 `let mut touched: Vec<String> = Vec::new();`);`self.save(&vp)?;` 之后:

```rust
        if !touched.is_empty() {
            let refs: Vec<&str> = touched.iter().map(String::as_str).collect();
            self.journal_invalidate(&refs, "此人随后又录了新会议");
        }
```

- `append_sample` 拆分:现有函数体改名 `fn append_sample_inner(&self, id: &str, samples: &[f32]) -> anyhow::Result<Option<String>>`,返回值从 `Ok(true)`/`Ok(false)` 改为 `Ok(Some(resolved))`/`Ok(None)`(resolved 已在函数内);新增两个包装:

```rust
    /// 为人物追加一份录音样本(语义见 append_sample_inner)。会使触及此人的合并
    /// 日志条目失效(样本状态与快照脱节)。
    pub fn append_sample(&self, id: &str, samples: &[f32]) -> anyhow::Result<bool> {
        let resolved = self.append_sample_inner(id, samples)?;
        if let Some(rid) = &resolved {
            self.journal_invalidate(&[rid], "此人样本随后有变动");
        }
        Ok(resolved.is_some())
    }

    /// 合并兜底截样专用:不触发日志失效——兜底样本是合并动作自身的一部分,
    /// 不能反手把刚落的这条日志灭了(撤销时快照还原自然会清掉它)。
    pub fn append_sample_for_merge(&self, id: &str, samples: &[f32]) -> anyhow::Result<bool> {
        Ok(self.append_sample_inner(id, samples)?.is_some())
    }
```

- `delete_sample`:`std::fs::remove_file(path)?;` 之后加 `self.journal_invalidate(&[&resolved], "此人样本随后有变动");`
- `rebuild_for_model`:`self.save(&vp)?;` 之后加:

```rust
        super::merge_journal::MergeJournal::new(self.root.clone())
            .invalidate_all("声纹库已按新模型重建");
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test store::voiceprints && cargo test`
Expected: 新增 5 测试 PASS;全仓 Rust 测试不回归(`append_sample` 既有调用方 lib.rs 编译通过——公开签名未变)。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/store/voiceprints.rs
git commit -m "feat(store): 六个变更入口接合并日志失效钩子"
```

---

### Task 5: 命令层——ipc 类型、`do_merge_person`、四个新命令

**Files:**
- Modify: `src-tauri/src/ipc.rs`(`PersonMergeSuggestion` 定义之后)
- Modify: `src-tauri/src/lib.rs`(约 3021-3085 行 merge/delete 区域 + 约 4486 行注册表)
- Test: `cargo check` + 既有测试(命令层依赖 AppHandle,不做单测;筛选逻辑已在 Task 2 `confident_picks` 覆盖)

**Interfaces:**
- Consumes: Task 1-4 的 `MergeJournal`/`merge_journaled`/`undo_merge`/`confident_picks`/`append_sample_for_merge`。
- Produces(前端依赖的命令签名):
  - `merge_person(loser, winner) -> String`(journal id;**返回类型从 () 改为 String**)
  - `apply_confident_merges() -> ConfidentMergeOutcome { applied: MergeReceipt[], remaining: PersonMergeSuggestion[] }`
  - `undo_merge(journal_id: String) -> ()`(参数名 camelCase 侧为 `journalId`)
  - `acknowledge_merge(journal_id: String) -> ()`
  - `list_merge_receipts() -> MergeReceipt[]`(未确认条目,time 降序)

- [ ] **Step 1: ipc.rs 加类型**(`PersonMergeSuggestion` 之后):

```rust
/// 合并回执(合并日志条目的前端视图)。invalid_reason 非空 = 不能再撤销(原因
/// 文案直接展示,如"此人随后又录了新会议");origin="auto" 且未确认的条目出现
/// 在审阅流回执卡队列。
#[derive(Debug, Clone, Serialize)]
pub struct MergeReceipt {
    pub journal_id: String,
    pub time: String,
    pub origin: String,
    pub loser: String,
    pub loser_name: String,
    pub winner: String,
    pub winner_name: String,
    pub similarity: Option<f32>,
    pub invalid_reason: Option<String>,
}

/// apply_confident_merges 返回:本次自动合并的回执 + 留给人工的建议。
#[derive(Debug, Clone, Serialize)]
pub struct ConfidentMergeOutcome {
    pub applied: Vec<MergeReceipt>,
    pub remaining: Vec<PersonMergeSuggestion>,
}
```

- [ ] **Step 2: lib.rs——提炼 `do_merge_person` 并改造 `merge_person`**

现 `merge_person`(3025-3072 行)拆成两段。共享主体(放在 `merge_person` 上方):

```rust
/// merge_person 与 apply_confident_merges 共享的合并主体:落日志→合并→loser 无
/// 样本时从笔记音频兜底截样(walk 同旧 merge_person),返回 journal id。不含录制
/// 中检查与图谱重建(调用方各自处理/批量做)。
fn do_merge_person(
    app: &AppHandle,
    loser: &str,
    winner: &str,
    origin: &str,
    similarity: Option<f32>,
) -> Result<String, String> {
    let root = data_root(app).map_err(|e| e.to_string())?;
    let store = store::VoiceprintStore::new(root);
    let loser_had_samples = !store.sample_paths_existing(loser).is_empty();
    let overflow = store.sample_paths_existing(loser).len()
        + store.sample_paths_existing(winner).len()
        > store::MAX_SAMPLES;
    let mut emb = if overflow {
        match diar::SherpaEmbedder::new(&speaker_model_path(app)) {
            Ok(e) => Some(e),
            Err(e) => {
                eprintln!("合并样本挑选:声纹模型不可用,退回按序保留: {e}");
                None
            }
        }
    } else {
        None
    };
    let now = chrono::Local::now().to_rfc3339();
    let journal_id = store
        .merge_journaled(
            loser,
            winner,
            emb.as_mut().map(|e| e as &mut dyn diar::SpeakerEmbedder),
            origin,
            similarity,
            &now,
        )
        .map_err(|e| e.to_string())?;
    if !loser_had_samples {
        match notes_dir(app) {
            Ok(nroot) => match cut_person_sample_from_notes(&nroot, loser) {
                Some(sample) => {
                    // 兜底样本走 for_merge 变体:不触发日志失效(是合并动作的一部分)。
                    if let Err(e) = store.append_sample_for_merge(winner, &sample) {
                        eprintln!("合并兜底样本写入失败({loser}->{winner},不影响合并): {e}");
                    }
                }
                None => eprintln!("合并兜底:未能从笔记音频截到 {loser} 的样本(可能无笔记/无音频)"),
            },
            Err(e) => eprintln!("合并兜底样本跳过(notes_dir 不可用): {e}"),
        }
    }
    Ok(journal_id)
}

/// 录制中拒绝合并/删除:开录时种子已按当前库结构注入本场 registry,此刻改动库的
/// 引用关系会让"是谁"混乱——比改名危险得多,故禁止。返回合并日志 id(前端撤销条用)。
#[tauri::command]
fn merge_person(
    app: AppHandle,
    state: State<AppState>,
    loser: String,
    winner: String,
) -> Result<String, String> {
    if state.session.lock().unwrap().is_some() {
        return Err("录制中不能合并说话人".into());
    }
    let journal_id = do_merge_person(&app, &loser, &winner, "manual", None)?;
    let root = data_root(&app).map_err(|e| e.to_string())?;
    queue_person_graph_rebuild(&app, root, "人物合并")?;
    Ok(journal_id)
}
```

(旧 `merge_person` 的函数体与 doc 注释被以上取代;`cut_person_sample_from_notes` 等辅助原样保留。)

- [ ] **Step 3: lib.rs——四个新命令**(`delete_person` 之后):

```rust
fn receipt_of(e: &store::MergeJournalEntry) -> ipc::MergeReceipt {
    ipc::MergeReceipt {
        journal_id: e.id.clone(),
        time: e.time.clone(),
        origin: e.origin.clone(),
        loser: e.loser.clone(),
        loser_name: e.loser_name.clone(),
        winner: e.winner.clone(),
        winner_name: e.winner_name.clone(),
        similarity: e.similarity,
        invalid_reason: e.invalid_reason.clone(),
    }
}

/// 整理·自动归并:strong 档且 loser 未命名且不在拒绝名单的建议逐条落日志后合并,
/// 其余留给人工。录制中不动库(建议仍只读算出返回,审阅流此时只读浏览)。单条
/// 失败不挡整批:该条降级人工,eprintln 留痕。
#[tauri::command]
fn apply_confident_merges(
    app: AppHandle,
    state: State<AppState>,
) -> Result<ipc::ConfidentMergeOutcome, String> {
    let root = data_root(&app).map_err(|e| e.to_string())?;
    let store = store::VoiceprintStore::new(root.clone());
    let vp = store.load();
    let sugs = store::suggest_merges(&vp);
    let to_ipc = |s: &store::MergeSuggestion| ipc::PersonMergeSuggestion {
        loser_name: vp.people.get(&s.loser).map(|p| p.name.clone()).unwrap_or_default(),
        winner_name: vp.people.get(&s.winner).map(|p| p.name.clone()).unwrap_or_default(),
        loser: s.loser.clone(),
        winner: s.winner.clone(),
        similarity: s.similarity,
        source: s.source.clone(),
        salience: s.salience,
    };
    if state.session.lock().unwrap().is_some() {
        return Ok(ipc::ConfidentMergeOutcome {
            applied: vec![],
            remaining: sugs.iter().map(to_ipc).collect(),
        });
    }
    let journal = store::MergeJournal::new(root.clone());
    let deny = journal.auto_denylist();
    let (autos, manual) = store::confident_picks(&vp, sugs, &deny);
    let mut remaining: Vec<ipc::PersonMergeSuggestion> = manual.iter().map(to_ipc).collect();
    let mut applied = Vec::new();
    for s in autos {
        match do_merge_person(&app, &s.loser, &s.winner, "auto", Some(s.similarity)) {
            Ok(jid) => match journal.entry(&jid) {
                Ok(e) => applied.push(receipt_of(&e)),
                Err(err) => eprintln!("自动归并回执读取失败({jid}): {err}"),
            },
            Err(err) => {
                eprintln!("自动归并失败({}->{}),留给人工: {err}", s.loser, s.winner);
                remaining.push(to_ipc(&s));
            }
        }
    }
    if !applied.is_empty() {
        queue_person_graph_rebuild(&app, root, "自动归并")?;
    }
    Ok(ipc::ConfidentMergeOutcome { applied, remaining })
}

/// 撤销一次合并(按日志条目)。录制中拒绝:理由同 merge_person。
#[tauri::command]
fn undo_merge(app: AppHandle, state: State<AppState>, journal_id: String) -> Result<(), String> {
    if state.session.lock().unwrap().is_some() {
        return Err("录制中不能撤销合并".into());
    }
    let root = data_root(&app).map_err(|e| e.to_string())?;
    store::VoiceprintStore::new(root.clone())
        .undo_merge(&journal_id)
        .map_err(|e| e.to_string())?;
    queue_person_graph_rebuild(&app, root, "撤销合并")
}

/// 回执卡「好」:确认自动归并,条目(连同样本副本)删除。
#[tauri::command]
fn acknowledge_merge(app: AppHandle, journal_id: String) -> Result<(), String> {
    let root = data_root(&app).map_err(|e| e.to_string())?;
    store::MergeJournal::new(root).acknowledge(&journal_id).map_err(|e| e.to_string())
}

/// 未确认的合并回执(审阅流回执卡数据;含已失效的——卡上撤销钮变灰注明原因)。
/// manual 条目生来已确认,天然不在其中。
#[tauri::command]
fn list_merge_receipts(app: AppHandle) -> Result<Vec<ipc::MergeReceipt>, String> {
    let root = data_root(&app).map_err(|e| e.to_string())?;
    Ok(store::MergeJournal::new(root)
        .entries()
        .iter()
        .filter(|e| !e.acknowledged)
        .map(receipt_of)
        .collect())
}
```

注册表(`suggest_person_merges,` 之后,约 4486 行)追加四行:

```rust
            apply_confident_merges,
            undo_merge,
            acknowledge_merge,
            list_merge_receipts,
```

- [ ] **Step 4: 验证编译与全量测试**

Run: `cd src-tauri && cargo check && cargo test`
Expected: 编译通过,全部测试 PASS。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/ipc.rs src-tauri/src/lib.rs
git commit -m "feat(ipc): 自动归并/撤销/回执四命令,merge_person 返回日志 id"
```

---

### Task 6: 前端接口 + 队列/键盘/试听纯逻辑 + vitest

**Files:**
- Modify: `src/lib/people.ts`
- Create: `src/lib/tidyQueue.ts`、`src/lib/tidyQueue.test.ts`、`src/lib/tidyAudio.ts`、`src/lib/tidyAudio.test.ts`

**Interfaces:**
- Consumes: Task 5 命令。
- Produces:
  - `people.ts`:`MergeReceipt`、`ConfidentMergeOutcome` 类型;`applyConfidentMerges()`、`listMergeReceipts()`、`undoMerge(journalId)`、`acknowledgeMerge(journalId)`;`mergePerson` 返回 `Promise<string>`。
  - `tidyQueue.ts`:`TidyItem`(四变体)、`tidyItemKey(item): string`、`buildTidyQueue(people, suggestions, receipts, dismissed?): TidyItem[]`、`orderWithSkips(items, skippedKeys): TidyItem[]`、`keyCommand(key, kind): TidyCommand | null`。
  - `tidyAudio.ts`:`createAudition(factory, onChange)` → `{ toggle(key, src), stop(), key }`。

- [ ] **Step 1: 写失败测试**

`src/lib/tidyQueue.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import { buildTidyQueue, keyCommand, orderWithSkips, tidyItemKey, type TidyItem } from "./tidyQueue";
import type { MergeReceipt, PersonMergeSuggestion, PersonSummary } from "./people";

const person = (id: string, name = "", samples: string[] = []): PersonSummary => ({
  id,
  name,
  total_ms: 60_000,
  last_seen: "2026-07-31T10:00:00+08:00",
  sources: ["mic"],
  sample_paths: samples,
  sample_dates: samples.map(() => ""),
});

const sug = (loser: string, winner: string): PersonMergeSuggestion => ({
  loser,
  loser_name: "",
  winner,
  winner_name: "张三",
  similarity: 0.7,
  source: "mic",
  salience: null,
});

const receipt = (id: string): MergeReceipt => ({
  journal_id: id,
  time: "t",
  origin: "auto",
  loser: "P1",
  loser_name: "",
  winner: "P2",
  winner_name: "张三",
  similarity: 0.9,
  invalid_reason: null,
});

describe("buildTidyQueue", () => {
  it("按 回执→建议→同名组→无样本 排序,同名分组、无样本逐条", () => {
    const people = [
      person("P2", "张三", ["/a.wav"]),
      person("P3", "张三", ["/b.wav"]),
      person("P4", "", []),
      person("P5", "李四", ["/c.wav"]),
    ];
    const q = buildTidyQueue(people, [sug("P4", "P5")], [receipt("m-P1")]);
    expect(q.map((i) => i.kind)).toEqual(["receipt", "suggestion", "dup", "nosample"]);
    const dup = q[2] as Extract<TidyItem, { kind: "dup" }>;
    expect(dup.name).toBe("张三");
    expect(dup.people.map((p) => p.id)).toEqual(["P2", "P3"]);
  });

  it("dismissed 集合滤掉同名组与无样本条目", () => {
    const people = [person("P2", "张三"), person("P3", "张三"), person("P4", "", [])];
    const all = buildTidyQueue(people, [], []);
    expect(all).toHaveLength(3); // dup + P2/P3 无样本?——张三两条都无样本:dup 1 + nosample 3
    const q = buildTidyQueue(people, [], [], new Set(["d:张三", "n:P4"]));
    expect(q.every((i) => tidyItemKey(i) !== "d:张三" && tidyItemKey(i) !== "n:P4")).toBe(true);
  });

  it("key 稳定且各类型互不冲突", () => {
    expect(tidyItemKey({ kind: "suggestion", suggestion: sug("P4", "P5") })).toBe("s:P4>P5");
    expect(tidyItemKey({ kind: "receipt", receipt: receipt("m-P1") })).toBe("r:m-P1");
    expect(tidyItemKey({ kind: "nosample", person: person("P4") })).toBe("n:P4");
    expect(tidyItemKey({ kind: "dup", name: "张三", people: [] })).toBe("d:张三");
  });
});

describe("orderWithSkips", () => {
  it("跳过的挪到队尾并保持跳过先后序", () => {
    const items = buildTidyQueue(
      [person("P4", "", []), person("P5", "", []), person("P6", "", [])],
      [],
      [],
    );
    const ordered = orderWithSkips(items, ["n:P4", "n:P5"]);
    expect(ordered.map(tidyItemKey)).toEqual(["n:P6", "n:P4", "n:P5"]);
  });
});

describe("keyCommand", () => {
  it("Enter=主动作,S=跳过,数字=试听", () => {
    expect(keyCommand("Enter", "suggestion")).toBe("primary");
    expect(keyCommand("s", "receipt")).toBe("skip");
    expect(keyCommand("1", "suggestion")).toEqual({ play: 0 });
    expect(keyCommand("2", "receipt")).toEqual({ play: 1 });
  });
  it("X 忽略/保留,但回执卡无 X(撤销只走点击,防误触)", () => {
    expect(keyCommand("x", "suggestion")).toBe("dismiss");
    expect(keyCommand("X", "nosample")).toBe("dismiss");
    expect(keyCommand("x", "receipt")).toBeNull();
  });
  it("数字键上限:双栏卡 1-2,同名组卡 1-9", () => {
    expect(keyCommand("3", "suggestion")).toBeNull();
    expect(keyCommand("9", "dup")).toEqual({ play: 8 });
    expect(keyCommand("0", "dup")).toBeNull();
  });
});
```

`src/lib/tidyAudio.test.ts`:

```ts
import { describe, expect, it, vi } from "vitest";
import { createAudition, type PlayerLike } from "./tidyAudio";

function fakePlayer() {
  const p: PlayerLike & { paused: boolean } = {
    paused: false,
    play: vi.fn(),
    pause: vi.fn(() => {
      p.paused = true;
    }),
    onended: null,
  };
  return p;
}

describe("createAudition", () => {
  it("单实例互斥:开新的先停旧的", () => {
    const players: ReturnType<typeof fakePlayer>[] = [];
    const changes: (string | null)[] = [];
    const a = createAudition(
      () => {
        const p = fakePlayer();
        players.push(p);
        return p;
      },
      (k) => changes.push(k),
    );
    a.toggle("k1", "/a.wav");
    a.toggle("k2", "/b.wav");
    expect(players[0].pause).toHaveBeenCalled();
    expect(a.key).toBe("k2");
    expect(changes).toEqual(["k1", null, "k2"]);
  });

  it("再点同 key 停止", () => {
    const a = createAudition(fakePlayer, () => {});
    a.toggle("k1", "/a.wav");
    a.toggle("k1", "/a.wav");
    expect(a.key).toBeNull();
  });

  it("自然播完清态(onended)", () => {
    let last: ReturnType<typeof fakePlayer> | null = null;
    const a = createAudition(
      () => (last = fakePlayer()),
      () => {},
    );
    a.toggle("k1", "/a.wav");
    last!.onended?.();
    expect(a.key).toBeNull();
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `npm test`
Expected: FAIL——模块不存在。

- [ ] **Step 3: 实现**

3a. `src/lib/people.ts`——`mergePerson` 改为:

```ts
/** loser 并入 winner,返回合并日志 id(撤销用);录制中后端拒绝(报错文案原样透出)。 */
export const mergePerson = (loser: string, winner: string) =>
  invoke<string>("merge_person", { loser, winner });
```

文件末尾追加:

```ts
/** 合并回执(合并日志条目):invalid_reason 非空=不能再撤销(原因文案直接展示)。 */
export type MergeReceipt = {
  journal_id: string;
  time: string;
  origin: "auto" | "manual";
  loser: string;
  loser_name: string;
  winner: string;
  winner_name: string;
  similarity: number | null;
  invalid_reason: string | null;
};
/** 整理·自动归并返回:本次合并的回执 + 留给人工的建议。 */
export type ConfidentMergeOutcome = {
  applied: MergeReceipt[];
  remaining: PersonMergeSuggestion[];
};
/** 高置信建议逐条落日志后自动合并;录制中只读算建议不动库。 */
export const applyConfidentMerges = () =>
  invoke<ConfidentMergeOutcome>("apply_confident_merges");
/** 未确认的自动归并回执(重启后仍在,直到「好」/撤销)。 */
export const listMergeReceipts = () => invoke<MergeReceipt[]>("list_merge_receipts");
/** 撤销一次合并(按日志 id);已失效/录制中后端拒绝。 */
export const undoMerge = (journalId: string) => invoke<void>("undo_merge", { journalId });
/** 回执卡「好」:确认自动归并,条目删除。 */
export const acknowledgeMerge = (journalId: string) =>
  invoke<void>("acknowledge_merge", { journalId });
```

3b. `src/lib/tidyQueue.ts`(新建,纯函数无副作用):

```ts
// 整理收件箱的队列纯逻辑:构建、跳过排序、键盘命令映射。
// UI 无关、可单测;/speakers/tidy 页面与侧栏徽标共同消费。
import type { MergeReceipt, PersonMergeSuggestion, PersonSummary } from "$lib/people";

export type TidyItem =
  | { kind: "receipt"; receipt: MergeReceipt }
  | { kind: "suggestion"; suggestion: PersonMergeSuggestion }
  | { kind: "dup"; name: string; people: PersonSummary[] }
  | { kind: "nosample"; person: PersonSummary };

/** 稳定 key(跳过序/忽略集/Svelte each key 共用)。建议键与 sugKey 同格式。 */
export const tidyItemKey = (it: TidyItem): string =>
  it.kind === "receipt"
    ? `r:${it.receipt.journal_id}`
    : it.kind === "suggestion"
      ? `s:${it.suggestion.loser}>${it.suggestion.winner}`
      : it.kind === "dup"
        ? `d:${it.name}`
        : `n:${it.person.id}`;

/** 收件箱队列:回执 → 拿不准的建议 → 同名组 → 无样本。people 按 last_seen 降序
    传入(listPeople 保证),同名组主条目默认取组首=最近活跃。dismissed 是会话级
    忽略/保留集(键=tidyItemKey),建议的忽略在上游 tidy.visible 里已滤。 */
export function buildTidyQueue(
  people: PersonSummary[],
  suggestions: PersonMergeSuggestion[],
  receipts: MergeReceipt[],
  dismissed: Set<string> = new Set(),
): TidyItem[] {
  const items: TidyItem[] = receipts.map((r) => ({ kind: "receipt", receipt: r }));
  for (const s of suggestions) items.push({ kind: "suggestion", suggestion: s });
  const byName = new Map<string, PersonSummary[]>();
  for (const p of people) {
    if (!p.name) continue;
    byName.set(p.name, [...(byName.get(p.name) ?? []), p]);
  }
  for (const [name, g] of byName) {
    if (g.length > 1) items.push({ kind: "dup", name, people: g });
  }
  for (const p of people) {
    if (p.sample_paths.length === 0) items.push({ kind: "nosample", person: p });
  }
  return items.filter((i) => !dismissed.has(tidyItemKey(i)));
}

/** 跳过=挪队尾:未跳过的保持原序,跳过的按跳过先后排最后。 */
export function orderWithSkips(items: TidyItem[], skippedKeys: string[]): TidyItem[] {
  const rank = new Map(skippedKeys.map((k, i) => [k, i]));
  const kept = items.filter((i) => !rank.has(tidyItemKey(i)));
  const skipped = items
    .filter((i) => rank.has(tidyItemKey(i)))
    .sort((a, b) => rank.get(tidyItemKey(a))! - rank.get(tidyItemKey(b))!);
  return [...kept, ...skipped];
}

/** 键盘命令:Enter=主动作,X=忽略/保留(回执卡除外——撤销只走点击防误触),
    S=跳过,数字=试听(双栏卡 1/2=左右方,同名组卡 1-9=第 n 条)。null=此卡无该命令。 */
export type TidyCommand = "primary" | "dismiss" | "skip" | { play: number };
export function keyCommand(key: string, kind: TidyItem["kind"]): TidyCommand | null {
  if (key === "Enter") return "primary";
  if (key === "x" || key === "X") return kind === "receipt" ? null : "dismiss";
  if (key === "s" || key === "S") return "skip";
  const digitMax = kind === "dup" ? 9 : 2;
  const n = Number(key);
  if (Number.isInteger(n) && n >= 1 && n <= digitMax) return { play: n - 1 };
  return null;
}
```

3c. `src/lib/tidyAudio.ts`(新建):

```ts
// 单实例试听控制器:同一时刻至多一个在播,再点同 key 即停。播放器工厂与状态
// 回调可注入——页面传 (src) => new Audio(convertFileSrc(src)) 与 $state 写回,
// 测试传假播放器。三处试听场景(概览已删,审阅流/详情页)语义与旧实现一致。
export type PlayerLike = { play(): unknown; pause(): void; onended: (() => void) | null };

export function createAudition(
  factory: (src: string) => PlayerLike,
  onChange: (key: string | null) => void,
) {
  let player: PlayerLike | null = null;
  let key: string | null = null;
  const stop = () => {
    player?.pause();
    player = null;
    key = null;
    onChange(null);
  };
  const toggle = (k: string, src: string) => {
    if (key === k) {
      stop();
      return;
    }
    if (player) stop();
    const p = factory(src);
    p.onended = () => {
      if (key === k) stop();
    };
    player = p;
    key = k;
    onChange(k);
    try {
      const r = p.play();
      if (r instanceof Promise) r.catch(() => {
        if (key === k) stop();
      });
    } catch {
      stop();
    }
  };
  return {
    toggle,
    stop,
    get key() {
      return key;
    },
  };
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `npm test`
Expected: tidyQueue 8 测试 + tidyAudio 3 测试 PASS,既有测试不回归。

- [ ] **Step 5: 提交**

```bash
git add src/lib/people.ts src/lib/tidyQueue.ts src/lib/tidyQueue.test.ts src/lib/tidyAudio.ts src/lib/tidyAudio.test.ts
git commit -m "feat(tidy): 前端命令接口与队列/键盘/试听纯逻辑"
```

---

### Task 7: tidy store 改造 + 全局触发时机 + 侧栏徽标

**Files:**
- Modify: `src/lib/tidy.svelte.ts`(重写)
- Modify: `src/lib/recording.svelte.ts`(stopped 分支)
- Modify: `src/routes/+layout.svelte`(全局触发 effect)
- Modify: `src/lib/Sidebar.svelte`(徽标与 effect)

**Interfaces:**
- Produces(页面依赖):`tidy.suggestions`、`tidy.receipts`、`tidy.visible`、`tidy.involving(id)`、`tidy.ignore(s)`、`tidy.dismissed: Set<string>`、`tidy.dismiss(key: string)`、`tidy.refresh()`(内部走 `applyConfidentMerges`,有自动合并时 `recording.bumpPeople()`)。`sugKey`/`isStrong` 不变。

- [ ] **Step 1: 重写 `src/lib/tidy.svelte.ts`**

```ts
// 整理收件箱的共享会话态:侧栏徽标、概览摘要卡、审阅流、详情页 ctx 提示四处同源。
// refresh 即"自动归并 + 拉剩余":高置信建议在后端落日志合并(录制中后端只读),
// 返回的 remaining 才是要人工拍板的。忽略(建议)与保留/忽略(无样本/同名组)只在
// 本次运行内生效——建议随库重算,持久忽略会藏住真重复;真正要"永不再犯"的是
// 撤销,它在后端落盘拒绝名单。
import {
  applyConfidentMerges,
  listMergeReceipts,
  type MergeReceipt,
  type PersonMergeSuggestion,
} from "$lib/people";
import { recording } from "$lib/recording.svelte";

export const sugKey = (s: PersonMergeSuggestion) => `${s.loser}>${s.winner}`;

/** "很可能"判定:裸余弦够高(绝对档 0.74),或 S-Norm 显著性够强(z≥3)。
    与后端 SUGGEST_STRONG_RAW / SUGGEST_STRONG_Z 同值(自动归并同一准入)。 */
export const isStrong = (s: PersonMergeSuggestion) =>
  s.similarity >= 0.74 || (s.salience ?? 0) >= 3.0;

class TidyState {
  suggestions = $state<PersonMergeSuggestion[]>([]);
  receipts = $state<MergeReceipt[]>([]);
  ignored = $state<Set<string>>(new Set());
  /** 会话级忽略/保留集(同名组「忽略」、无样本「保留」),键=tidyItemKey。 */
  dismissed = $state<Set<string>>(new Set());
  loading = $state(false);

  /** 未被忽略的建议(展示/计数用)。 */
  get visible(): PersonMergeSuggestion[] {
    return this.suggestions.filter((s) => !this.ignored.has(sugKey(s)));
  }

  /** 与某人相关的建议(详情页上下文提示用)。 */
  involving(personId: string): PersonMergeSuggestion[] {
    return this.visible.filter((s) => s.loser === personId || s.winner === personId);
  }

  ignore(s: PersonMergeSuggestion) {
    this.ignored = new Set([...this.ignored, sugKey(s)]);
  }

  dismiss(key: string) {
    this.dismissed = new Set([...this.dismissed, key]);
  }

  /** 自动归并 + 重拉(启动、录制停止、库变化后调用)。失败静默清空——整理是
      增值层,比对失败不该打扰主流程。有自动合并发生时 bumpPeople 驱动全局刷新
      (再触发的下一轮 refresh 不会再有 applied,自然收敛)。 */
  async refresh() {
    this.loading = true;
    try {
      const outcome = await applyConfidentMerges();
      this.suggestions = outcome.remaining;
      this.receipts = await listMergeReceipts();
      if (outcome.applied.length > 0) recording.bumpPeople();
    } catch {
      this.suggestions = [];
      this.receipts = [];
    }
    this.loading = false;
  }
}

export const tidy = new TidyState();
```

- [ ] **Step 2: recording.svelte.ts——停止时翻 peopleVersion**

在状态事件处理的 `} else if (e.state === "stopped" || e.state.startsWith("error:")) {` 分支内(约 149 行,现有 `statusVersion` 处理旁)追加:

```ts
        // 停止入库可能新建/更新人物与样本:人物简表与整理收件箱都要重算。
        peopleVersion++;
```

- [ ] **Step 3: +layout.svelte——全局触发**

script 区(import 区加 `import { tidy } from "$lib/tidy.svelte";`)顶层加:

```ts
  // 整理收件箱全局驱动:挂载即跑一次(应用启动),之后随 peopleVersion(录制停止
  // /人物增删改)重算——不锁在人物页签,自动归并不等用户逛到那儿才发生。
  $effect(() => {
    void recording.peopleVersion;
    void tidy.refresh();
  });
```

- [ ] **Step 4: Sidebar.svelte——徽标改队列总数,effect 去重**

- import 区加 `import { buildTidyQueue } from "$lib/tidyQueue";`
- 约 82-88 行 effect 中删去 `tidy.refresh();` 一行(layout 已全局驱动,保留 `refreshPeople()`)。
- 删除 `dupGroupCount`(约 147-157 行),`tidyBadge`(约 159 行)替换为:

```ts
  /** 「概览与整理」徽标:收件箱队列总数(回执+建议+同名组+无样本),像收件箱未读。 */
  const tidyBadge = $derived(buildTidyQueue(people, tidy.visible, tidy.receipts, tidy.dismissed).length);
```

- 徽标 title(约 440 行)改为:

```svelte
            <span class="tidy-badge" title="{tidyBadge} 件待整理">{tidyBadge}</span>
```

- [ ] **Step 5: 验证与提交**

Run: `npm test && npm run check`
Expected: 通过(概览页此时仍用旧 `tidy.refresh` 语义,兼容——`visible`/`ignore` 未变;`suggestions` 含义变为"人工余量",概览旧卡照常渲染,Task 9 再收敛)。

```bash
git add src/lib/tidy.svelte.ts src/lib/recording.svelte.ts src/routes/+layout.svelte src/lib/Sidebar.svelte
git commit -m "feat(tidy): store 接自动归并,全局触发时机与侧栏收件箱徽标"
```

---

### Task 8: `/speakers/tidy` 审阅流页面

**Files:**
- Create: `src/routes/speakers/tidy/+page.svelte`

**Interfaces:**
- Consumes: Task 6/7 全部;`personNotes`(会议上下文)、`formatDate`/`formatDuration`/`speakerInk`(notes.ts)。

- [ ] **Step 1: 建页面**(完整文件):

```svelte
<script lang="ts">
  import { onMount } from "svelte";
  import { convertFileSrc } from "@tauri-apps/api/core";
  import { goto } from "$app/navigation";
  import {
    acknowledgeMerge,
    deletePerson,
    listPeople,
    mergePerson,
    personNotes,
    undoMerge,
    type MergeReceipt,
    type PersonMergeSuggestion,
    type PersonSummary,
  } from "$lib/people";
  import { formatDate, formatDuration, speakerInk, type NoteSummary } from "$lib/notes";
  import { isStrong, tidy } from "$lib/tidy.svelte";
  import {
    buildTidyQueue,
    keyCommand,
    orderWithSkips,
    tidyItemKey,
    type TidyItem,
  } from "$lib/tidyQueue";
  import { createAudition } from "$lib/tidyAudio";
  import { recording } from "$lib/recording.svelte";

  // ── 数据:队列由共享 store + 人物表现算;处理完的项随重算自然消失,始终看队首 ──
  let people = $state<PersonSummary[]>([]);
  let error = $state("");
  let busy = $state(false);
  let skipped = $state<string[]>([]);
  let done = $state(0);
  /** 手动合并后的页内撤销条(最近一次;同名组连并多条时只能撤最后一条,撤后其前
      一条经日志复活仍可继续撤)。 */
  let lastManual = $state<{ journalId: string; label: string } | null>(null);
  let confirmClean = $state(false);

  const personById = $derived(new Map(people.map((p) => [p.id, p])));
  const queue = $derived(
    orderWithSkips(buildTidyQueue(people, tidy.visible, tidy.receipts, tidy.dismissed), skipped),
  );
  const current = $derived(queue[0] ?? null);
  const total = $derived(done + queue.length);
  const live = $derived(recording.isLive);

  const plabel = (id: string, name: string) => name || `说话人 ${id.replace(/^P/, "")}`;

  // ── 会议上下文(拍板信息):每人最近 3 场,懒加载缓存 ──
  let notesCache = $state<Record<string, NoteSummary[]>>({});
  async function loadNotes(pid: string) {
    if (notesCache[pid]) return;
    try {
      notesCache[pid] = (await personNotes(pid)).slice(0, 3);
    } catch {
      notesCache[pid] = [];
    }
  }
  /** 当前卡涉及的人物(回执卡的 loser 已并入 winner,看 winner 即可)。 */
  const currentIds = $derived.by(() => {
    if (!current) return [] as string[];
    if (current.kind === "suggestion") return [current.suggestion.loser, current.suggestion.winner];
    if (current.kind === "receipt") return [current.receipt.winner];
    if (current.kind === "dup") return current.people.map((p) => p.id);
    return [current.person.id];
  });
  $effect(() => {
    for (const id of currentIds) void loadNotes(id);
  });

  // ── 试听:单实例互斥,切卡/离开即停 ──
  let playingKey = $state<string | null>(null);
  const audition = createAudition(
    (src) => new Audio(convertFileSrc(src)),
    (k) => (playingKey = k),
  );
  $effect(() => {
    void current;
    audition.stop();
  });
  $effect(() => () => audition.stop());

  /** 播某人最新一份样本(键盘数字键)。无样本静默不动。 */
  function playLatest(pid: string) {
    const p = personById.get(pid);
    const path = p?.sample_paths[p.sample_paths.length - 1];
    if (path) audition.toggle(path, path);
  }

  // ── 同名组主条目(可切换,默认最近活跃=组首) ──
  let dupPrimary = $state<Record<string, string>>({});
  const dupPrimaryId = (name: string, g: PersonSummary[]) => dupPrimary[name] ?? g[0].id;

  async function refreshPeople() {
    try {
      people = await listPeople();
      error = "";
    } catch (e) {
      error = `加载失败: ${e}`;
    }
  }
  onMount(async () => {
    await tidy.refresh();
    await refreshPeople();
  });
  $effect(() => {
    void recording.peopleVersion;
    refreshPeople();
  });

  // ── 动作:每个动作后重算队列;失败卡片留在原地,错误横幅透出后端文案 ──
  async function act(fn: () => Promise<void>) {
    if (busy) return;
    busy = true;
    error = "";
    audition.stop();
    confirmClean = false;
    try {
      await fn();
      done++;
      recording.bumpPeople();
      await tidy.refresh();
      await refreshPeople();
    } catch (e) {
      error = `${e}`;
    }
    busy = false;
  }

  async function doMergeSuggestion(s: PersonMergeSuggestion) {
    await act(async () => {
      const jid = await mergePerson(s.loser, s.winner);
      lastManual = {
        journalId: jid,
        label: `${plabel(s.loser, s.loser_name)} → ${plabel(s.winner, s.winner_name)}`,
      };
    });
  }
  function doIgnoreSuggestion(s: PersonMergeSuggestion) {
    tidy.ignore(s);
    done++;
  }
  async function doMergeDup(name: string, g: PersonSummary[]) {
    await act(async () => {
      const winner = dupPrimaryId(name, g);
      let jid = "";
      for (const p of g) {
        if (p.id !== winner) jid = await mergePerson(p.id, winner);
      }
      lastManual = { journalId: jid, label: `「${name}」并成一条` };
    });
  }
  async function doDeleteNoSample(p: PersonSummary) {
    await act(async () => {
      await deletePerson(p.id);
    });
  }
  /** 一键清理剩余全部无样本条目(二段确认后)。 */
  async function doCleanAll() {
    const rest = queue.filter((i) => i.kind === "nosample");
    await act(async () => {
      for (const i of rest) {
        if (i.kind === "nosample") await deletePerson(i.person.id);
      }
    });
  }
  function doDismiss(item: TidyItem) {
    tidy.dismiss(tidyItemKey(item));
    done++;
  }
  function doSkip() {
    if (current) skipped = [...skipped, tidyItemKey(current)];
  }
  async function doAck(r: MergeReceipt) {
    await act(async () => {
      await acknowledgeMerge(r.journal_id);
    });
  }
  async function doUndo(journalId: string) {
    await act(async () => {
      await undoMerge(journalId);
      lastManual = null;
    });
  }

  // ── 键盘:Enter 主动作 / X 忽略保留 / S 跳过 / 1-9 试听 / Esc 返回 ──
  function onKeydown(e: KeyboardEvent) {
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    if (e.key === "Escape") {
      e.preventDefault();
      goto("/speakers");
      return;
    }
    if (!current || busy) return;
    const cmd = keyCommand(e.key, current.kind);
    if (!cmd) return;
    e.preventDefault();
    if (typeof cmd === "object") {
      playLatest(currentIds[cmd.play] ?? "");
      return;
    }
    if (cmd === "skip") {
      doSkip();
      return;
    }
    if (live) return; // 录制中只许试听/跳过
    const it = current;
    if (cmd === "primary") {
      if (it.kind === "suggestion") void doMergeSuggestion(it.suggestion);
      else if (it.kind === "dup") void doMergeDup(it.name, it.people);
      else if (it.kind === "nosample") void doDeleteNoSample(it.person);
      else void doAck(it.receipt);
    } else if (cmd === "dismiss") {
      if (it.kind === "suggestion") doIgnoreSuggestion(it.suggestion);
      else if (it.kind !== "receipt") doDismiss(it);
    }
  }
</script>

<svelte:window onkeydown={onKeydown} />

{#snippet personPane(pid: string, name: string, digit: number | null)}
  {@const p = personById.get(pid)}
  <div class="pane">
    <div class="pane-head">
      <span class="dot" style="background: {speakerInk(pid, 'mic')}"></span>
      <a class="pname" href="/speakers/{pid}">{plabel(pid, name)}</a>
      {#if digit !== null}<kbd class="kbd">{digit}</kbd>{/if}
    </div>
    {#if p}
      <div class="pane-meta">
        最近 {formatDate(p.last_seen)} · 累计 {formatDuration(Math.floor(p.total_ms / 1000))}
      </div>
      <div class="samples">
        {#each p.sample_paths as path, i (path)}
          <button
            class="chip"
            class:playing={playingKey === path}
            title={playingKey === path ? "停止" : "试听这份原声"}
            onclick={() => audition.toggle(path, path)}
          >
            {playingKey === path ? "◼" : "▶"}
            {p.sample_dates[i] ? formatDate(p.sample_dates[i]).slice(5, 10) : `样本 ${i + 1}`}
          </button>
        {:else}
          <span class="hint">无录音样本</span>
        {/each}
      </div>
      {#if (notesCache[pid] ?? []).length > 0}
        <div class="meets">
          {#each notesCache[pid] as n (n.id)}
            <a class="meet" href="/notes/{n.id}">{n.title || formatDate(n.started_at)}</a>
          {/each}
        </div>
      {/if}
    {:else}
      <div class="pane-meta">已并入,记录随合并转移</div>
    {/if}
  </div>
{/snippet}

<main class="container">
  <header class="head">
    <a class="back" href="/speakers">← 概览</a>
    <h1>整理收件箱</h1>
    {#if total > 0 && queue.length > 0}
      <span class="progress">第 {done + 1} / {total} 件</span>
    {/if}
  </header>

  {#if live}
    <div class="banner warn">录制中不能整理——可以浏览和试听,合并/删除/撤销等停止录制后再做。</div>
  {/if}
  {#if error}
    <div class="banner">{error}</div>
  {/if}
  {#if lastManual}
    <div class="undo-strip">
      已合并:{lastManual.label}
      <button class="mini" disabled={busy || live} onclick={() => doUndo(lastManual!.journalId)}>撤销</button>
      <button class="mini plain" onclick={() => (lastManual = null)}>好</button>
    </div>
  {/if}

  {#if !current}
    <div class="empty">
      <p>都整理完了</p>
      <p class="hint">新的建议会随录制自动出现;高置信的会自动归并并在这里留回执。</p>
      <a class="mini as-link" href="/speakers">返回概览</a>
    </div>
  {:else if current.kind === "receipt"}
    {@const r = current.receipt}
    <section class="card">
      <div class="card-tag">已自动归并</div>
      <div class="card-title">
        {plabel(r.loser, r.loser_name)} → {plabel(r.winner, r.winner_name)}
        {#if r.similarity !== null}
          <span class="sim strong">相似度 {Math.round(r.similarity * 100)}%</span>
        {/if}
      </div>
      <div class="panes">
        {@render personPane(r.winner, r.winner_name, 1)}
      </div>
      <p class="hint">声纹足够相似已自动并入。听一下不对劲就撤销;没问题点「好」。</p>
      <div class="acts">
        <button class="mini accent" disabled={busy} onclick={() => doAck(r)}>好 <kbd class="kbd">⏎</kbd></button>
        {#if r.invalid_reason}
          <button class="mini" disabled title={r.invalid_reason}>撤销(不可用)</button>
          <span class="hint">{r.invalid_reason}</span>
        {:else}
          <button class="mini" disabled={busy || live} onclick={() => doUndo(r.journal_id)}>撤销</button>
        {/if}
        <button class="mini plain" onclick={doSkip}>跳过 <kbd class="kbd">S</kbd></button>
      </div>
    </section>
  {:else if current.kind === "suggestion"}
    {@const s = current.suggestion}
    <section class="card">
      <div class="card-tag">归属建议</div>
      <div class="card-title">
        这两条像同一个人吗?
        <span class="sim" class:strong={isStrong(s)}>
          相似度 {Math.round(s.similarity * 100)}%{isStrong(s) ? " · 很可能" : ""}
        </span>
      </div>
      <div class="panes">
        {@render personPane(s.loser, s.loser_name, 1)}
        <svg class="arrow" width="18" height="18" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <path d="M2.5 8h10M9 4.5L13.5 8 9 11.5" />
        </svg>
        {@render personPane(s.winner, s.winner_name, 2)}
      </div>
      <p class="hint">两边各听一段原声(数字键 1/2 播最新样本),确认是同一个人再合并;合并保留双方声纹,认得更准。</p>
      <div class="acts">
        <button class="mini accent" disabled={busy || live} onclick={() => doMergeSuggestion(s)}>合并 <kbd class="kbd">⏎</kbd></button>
        <button class="mini" disabled={busy} onclick={() => doIgnoreSuggestion(s)}>忽略 <kbd class="kbd">X</kbd></button>
        <button class="mini plain" onclick={doSkip}>跳过 <kbd class="kbd">S</kbd></button>
      </div>
    </section>
  {:else if current.kind === "dup"}
    {@const g = current.people}
    {@const primary = dupPrimaryId(current.name, g)}
    <section class="card">
      <div class="card-tag">同名重复</div>
      <div class="card-title">「{current.name}」有 {g.length} 条,多半是同一个人被拆开了</div>
      <div class="panes wrap">
        {#each g as p, i (p.id)}
          <div class="dup-item" class:primary={p.id === primary}>
            {@render personPane(p.id, p.name, i < 9 ? i + 1 : null)}
            <label class="pick">
              <input
                type="radio"
                name="dup-primary"
                checked={p.id === primary}
                onchange={() => (dupPrimary = { ...dupPrimary, [current.name]: p.id })}
              />
              作为主条目
            </label>
          </div>
        {/each}
      </div>
      <p class="hint">其余条目将并入主条目(默认最近活跃的);数字键逐条试听核对。</p>
      <div class="acts">
        <button class="mini accent" disabled={busy || live} onclick={() => doMergeDup(current.name, g)}>
          全部并入主条目 <kbd class="kbd">⏎</kbd>
        </button>
        <button class="mini" disabled={busy} onclick={() => doDismiss(current)}>忽略 <kbd class="kbd">X</kbd></button>
        <button class="mini plain" onclick={doSkip}>跳过 <kbd class="kbd">S</kbd></button>
      </div>
    </section>
  {:else}
    {@const p = current.person}
    <section class="card">
      <div class="card-tag">无样本条目</div>
      <div class="card-title">{plabel(p.id, p.name)}——没有原声可核对</div>
      <div class="panes">
        {@render personPane(p.id, p.name, null)}
      </div>
      <p class="hint warn-text">
        删除后历史笔记中这个说话人恢复显示为编号,不可恢复。认不出是谁就删,拿不准就保留。
      </p>
      <div class="acts">
        <button class="mini danger" disabled={busy || live} onclick={() => doDeleteNoSample(p)}>删除 <kbd class="kbd">⏎</kbd></button>
        <button class="mini" disabled={busy} onclick={() => doDismiss(current)}>保留 <kbd class="kbd">X</kbd></button>
        <button class="mini plain" onclick={doSkip}>跳过 <kbd class="kbd">S</kbd></button>
        {#if queue.filter((i) => i.kind === "nosample").length > 1}
          <span class="spacer"></span>
          {#if confirmClean}
            <span class="warn-text">共 {queue.filter((i) => i.kind === "nosample").length} 条,删除不可恢复。</span>
            <button class="mini danger" disabled={busy || live} onclick={doCleanAll}>确认清理</button>
            <button class="mini plain" onclick={() => (confirmClean = false)}>取消</button>
          {:else}
            <button class="mini plain" disabled={busy || live} onclick={() => (confirmClean = true)}>
              剩余 {queue.filter((i) => i.kind === "nosample").length} 条无样本条目一键清理
            </button>
          {/if}
        {/if}
      </div>
    </section>
  {/if}

  <footer class="keys">
    <span><kbd class="kbd">⏎</kbd> 主动作</span>
    <span><kbd class="kbd">X</kbd> 忽略/保留</span>
    <span><kbd class="kbd">S</kbd> 跳过</span>
    <span><kbd class="kbd">1-9</kbd> 试听</span>
    <span><kbd class="kbd">Esc</kbd> 返回</span>
  </footer>
</main>

<style>
  .container {
    padding: 1.5rem;
    font-family: -apple-system, system-ui, sans-serif;
    max-width: 44rem;
  }
  .head {
    display: flex;
    align-items: baseline;
    gap: 0.8rem;
    margin-bottom: 1rem;
  }
  .head h1 {
    margin: 0;
    font-size: 1.3rem;
  }
  .back {
    color: var(--ink-secondary);
    text-decoration: none;
    font-size: 0.85rem;
  }
  .back:hover {
    color: var(--accent);
  }
  .progress {
    color: var(--ink-faint);
    font-size: 0.82rem;
  }
  .banner {
    background: var(--danger-tint);
    border: 1px solid var(--danger-line);
    color: var(--danger-ink);
    border-radius: var(--radius-lg);
    padding: 0.6rem 0.8rem;
    margin-bottom: 0.8rem;
    font-size: 0.9rem;
  }
  .banner.warn {
    background: var(--warning-tint);
    border-color: var(--warning-line);
    color: var(--warning-ink);
  }
  .undo-strip {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 0.5rem 0.8rem;
    margin-bottom: 0.8rem;
    font-size: 0.85rem;
    color: var(--ink-secondary);
  }
  .card {
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 1rem 1.1rem;
  }
  .card-tag {
    font-size: 0.75rem;
    color: var(--ink-faint);
    margin-bottom: 0.25rem;
  }
  .card-title {
    font-size: 1rem;
    font-weight: 500;
    color: var(--ink);
    margin-bottom: 0.8rem;
    display: flex;
    align-items: baseline;
    gap: 0.6rem;
    flex-wrap: wrap;
  }
  .sim {
    color: var(--ink-faint);
    font-size: 0.8rem;
    font-weight: 400;
  }
  .sim.strong {
    color: var(--accent);
  }
  .panes {
    display: flex;
    align-items: flex-start;
    gap: 0.8rem;
  }
  .panes.wrap {
    flex-wrap: wrap;
  }
  .arrow {
    color: var(--ink-faint);
    flex: none;
    align-self: center;
  }
  .pane {
    flex: 1;
    min-width: 0;
    background: var(--surface-soft);
    border-radius: var(--radius-md);
    padding: 0.6rem 0.7rem;
  }
  .dup-item {
    flex: 1 1 16rem;
    border: 1px solid transparent;
    border-radius: var(--radius-md);
  }
  .dup-item.primary {
    border-color: var(--accent);
  }
  .pane-head {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  .dot {
    width: 9px;
    height: 9px;
    border-radius: var(--radius-full);
    flex: none;
  }
  .pname {
    color: var(--ink);
    font-weight: 500;
    text-decoration: none;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .pname:hover {
    color: var(--accent);
    text-decoration: underline;
  }
  .pane-meta {
    color: var(--ink-faint);
    font-size: 0.76rem;
    margin: 0.25rem 0 0.4rem;
  }
  .samples {
    display: flex;
    flex-wrap: wrap;
    gap: 0.35rem;
  }
  .chip {
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink-secondary);
    border-radius: var(--radius-full);
    font-size: 0.75rem;
    padding: 0.15em 0.6em;
    cursor: pointer;
  }
  .chip:hover {
    background: var(--surface);
    color: var(--ink);
  }
  .chip.playing {
    border-color: var(--accent);
    color: var(--accent);
  }
  .meets {
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    margin-top: 0.45rem;
  }
  .meet {
    color: var(--ink-secondary);
    font-size: 0.78rem;
    text-decoration: none;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .meet:hover {
    color: var(--accent);
  }
  .pick {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    font-size: 0.78rem;
    color: var(--ink-secondary);
    padding: 0.3rem 0.7rem 0.5rem;
    cursor: pointer;
  }
  .pick input {
    accent-color: var(--accent);
    margin: 0;
  }
  .acts {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin-top: 0.8rem;
    flex-wrap: wrap;
  }
  .spacer {
    flex: 1;
  }
  .mini {
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink);
    border-radius: var(--radius-md);
    font-size: 0.82rem;
    padding: 0.25em 0.75em;
    cursor: pointer;
  }
  .mini:hover:not(:disabled) {
    background: var(--surface-soft);
  }
  .mini:disabled {
    color: var(--ink-faint);
    cursor: default;
  }
  .mini.accent {
    border-color: var(--accent);
    color: var(--accent);
    font-weight: 500;
  }
  .mini.accent:hover:not(:disabled) {
    background: var(--accent-tint);
  }
  .mini.danger {
    border-color: var(--danger);
    color: var(--danger);
    font-weight: 500;
  }
  .mini.danger:hover:not(:disabled) {
    background: var(--danger);
    color: var(--on-record);
  }
  .mini.plain {
    border-color: transparent;
    color: var(--ink-secondary);
  }
  .mini.as-link {
    display: inline-block;
    text-decoration: none;
    margin-top: 0.6rem;
  }
  .kbd {
    font-family: inherit;
    font-size: 0.68rem;
    color: var(--ink-faint);
    border: 1px solid var(--hairline);
    border-radius: 3px;
    padding: 0 0.25em;
    margin-left: 0.15em;
  }
  .hint {
    color: var(--ink-faint);
    font-size: 0.8rem;
  }
  p.hint {
    margin: 0.7rem 0 0;
  }
  .warn-text {
    color: var(--warning-ink);
  }
  .empty {
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 2rem 1.5rem;
    text-align: center;
  }
  .empty p {
    margin: 0 0 0.4rem;
    font-weight: 500;
  }
  .keys {
    display: flex;
    gap: 1rem;
    margin-top: 0.9rem;
    color: var(--ink-faint);
    font-size: 0.75rem;
  }
</style>
```

- [ ] **Step 2: 验证**

Run: `npm run check && npm test`
Expected: svelte-check 无错误(注意 `NoteSummary` 从 `$lib/notes` 导入是 type;`lastManual!` 非空断言处 svelte-check 通过)。

手动冒烟(可选,`npm run tauri dev`):进 `/speakers/tidy`,四类卡渲染、键盘可用、试听互斥、Esc 返回。

- [ ] **Step 3: 提交**

```bash
git add src/routes/speakers/tidy/+page.svelte
git commit -m "feat(tidy): /speakers/tidy 逐条审阅流(四类卡+键盘+试听+撤销)"
```

---

### Task 9: 概览页收敛 + 详情页撤销条

**Files:**
- Modify(整文件重写): `src/routes/speakers/+page.svelte`
- Modify: `src/routes/speakers/[id]/+page.svelte`

**Interfaces:**
- Consumes: `buildTidyQueue`/`tidy`/`undoMerge`;`mergePerson` 新返回值。

- [ ] **Step 1: 重写概览页**(完整替换 `src/routes/speakers/+page.svelte`):

```svelte
<script lang="ts">
  import { onMount } from "svelte";
  import { listPeople, type PersonSummary } from "$lib/people";
  import { tidy } from "$lib/tidy.svelte";
  import { buildTidyQueue } from "$lib/tidyQueue";
  import { recording } from "$lib/recording.svelte";

  // 主从结构的落地页:人物索引在侧栏,本页只做概览引导——不再重复列一遍名单。
  // 整理事项全部收进 /speakers/tidy 收件箱,本页只挂一张摘要卡。
  let people = $state<PersonSummary[]>([]);
  let error = $state("");

  const named = $derived(people.filter((p) => p.name).length);
  const unnamed = $derived(people.length - named);

  /** 待处理件数(建议+同名组+无样本;回执单列)。 */
  const pending = $derived(buildTidyQueue(people, tidy.visible, [], tidy.dismissed).length);
  const receiptsN = $derived(tidy.receipts.length);

  async function refresh() {
    try {
      people = await listPeople();
      error = "";
    } catch (e) {
      error = `加载失败: ${e}`;
    }
  }

  onMount(refresh);
  // 详情页改名/合并/删除、录制停止、自动归并后统计同步。
  $effect(() => {
    void recording.peopleVersion;
    refresh();
  });
</script>

<main class="container">
  <h1>会议搭子</h1>
  <p class="desc">
    录到的说话人会自动登记。给"未命名"的人<strong>命名</strong>后,之后的录制会自动认出他并直接显示名字;
    声纹足够相似的会自动归并,拿不准的进「整理收件箱」等你拍板。从左侧选择一个人查看详情、试听原声或管理。
  </p>

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  {#if people.length === 0}
    <div class="empty">
      <p>还没有说话人。</p>
      <p class="hint">录一场会议(单人说话累计满 30 秒),停止后会自动出现在左侧。</p>
    </div>
  {:else}
    <div class="stats">
      <div class="stat">
        <span class="num">{people.length}</span>
        <span class="label">位说话人</span>
      </div>
      <div class="stat">
        <span class="num">{named}</span>
        <span class="label">已命名</span>
      </div>
      {#if unnamed > 0}
        <div class="stat todo">
          <span class="num">{unnamed}</span>
          <span class="label">待命名</span>
        </div>
      {/if}
    </div>

    <section class="tidy">
      <div class="tidy-head">
        <div>
          <div class="tidy-title">整理收件箱</div>
          <div class="tidy-desc">
            {#if pending + receiptsN === 0}
              没有要整理的。
            {:else}
              {#if pending > 0}{pending} 件待处理{/if}{#if pending > 0 && receiptsN > 0} · {/if}{#if receiptsN > 0}{receiptsN} 条已自动归并{/if}
            {/if}
          </div>
        </div>
        {#if pending + receiptsN > 0}
          <a class="tidy-go" href="/speakers/tidy">开始整理</a>
        {/if}
      </div>
    </section>

    <p class="pick-hint">
      从左侧列表选择一个人查看详情。
      {#if unnamed > 0}「待命名」的人命名后,之后的录制会自动显示名字。{/if}
    </p>
  {/if}
</main>

<style>
  .container {
    padding: 1.5rem;
    font-family: -apple-system, system-ui, sans-serif;
    max-width: 44rem;
  }
  h1 {
    margin: 0 0 0.75rem;
  }
  .desc {
    color: var(--ink-secondary);
    font-size: 0.85rem;
    line-height: 1.5;
    margin: 0 0 1.25rem;
    max-width: 40rem;
  }
  /* 统计卡:surface 底并排三块,数字大字 500 权重(层级靠亮度不靠重字重) */
  .stats {
    display: flex;
    gap: 0.75rem;
    margin-bottom: 1rem;
  }
  .stat {
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 0.9rem 1.3rem;
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    min-width: 6.5rem;
  }
  .num {
    font-size: 1.5rem;
    font-weight: 500;
    color: var(--ink);
    line-height: 1.2;
  }
  .label {
    font-size: 0.8rem;
    color: var(--ink-secondary);
  }
  /* 待命名是待处理项:warning 色系点亮数字提示还有活没干 */
  .stat.todo .num {
    color: var(--warning-ink);
  }
  /* 收件箱摘要卡:surface 底一行,主按钮直达审阅流 */
  .tidy {
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 0.85rem 1rem;
    margin-bottom: 1rem;
  }
  .tidy-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
  }
  .tidy-title {
    font-weight: 500;
    font-size: 0.92rem;
    color: var(--ink);
  }
  .tidy-desc {
    color: var(--ink-secondary);
    font-size: 0.8rem;
    margin-top: 0.15rem;
  }
  .tidy-go {
    border: 1px solid var(--accent);
    color: var(--accent);
    background: transparent;
    border-radius: var(--radius-md);
    font-size: 0.85rem;
    font-weight: 500;
    padding: 0.35em 1em;
    text-decoration: none;
    flex: none;
  }
  .tidy-go:hover {
    background: var(--accent-tint);
  }
  .pick-hint {
    color: var(--ink-faint);
    font-size: 0.85rem;
  }
  .empty {
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 2rem 1.5rem;
    text-align: center;
  }
  .empty p {
    margin: 0 0 0.4rem;
    font-weight: 500;
  }
  .banner {
    background: var(--danger-tint);
    border: 1px solid var(--danger-line);
    color: var(--danger-ink);
    border-radius: var(--radius-lg);
    padding: 0.6rem 0.8rem;
    margin: 0.5rem 0 1rem;
    font-size: 0.95rem;
  }
  .hint {
    color: var(--ink-faint);
    font-weight: 400;
  }
</style>
```

- [ ] **Step 2: 详情页加撤销条**

`src/routes/speakers/[id]/+page.svelte`:

2a. import 区 `mergePerson` 同行加 `undoMerge`(from `$lib/people`)。

2b. script 加状态与动作(`applyCtxSuggestion` 附近):

```ts
  /** 手动合并后的撤销条(最近一次;后端日志兜底,失效时撤销报错原样透出)。 */
  let lastMergeId = $state<string | null>(null);

  async function undoLastMerge() {
    if (!lastMergeId) return;
    try {
      await undoMerge(lastMergeId);
      lastMergeId = null;
      recording.bumpPeople();
      await tidy.refresh();
      await refresh();
    } catch (e) {
      error = `${e}`;
    }
  }
```

2c. 三处 `await mergePerson(…)` 调用改为接住返回值,例如 `applyCtxSuggestion` 中:

```ts
      lastMergeId = await mergePerson(s.loser, s.winner);
```

(约 120 行同名合并、约 186 行「合并到…」同样改 `lastMergeId = await mergePerson(loser, winner);`。)

2d. 错误横幅(`{#if error}` 块,约 304 行)之后加:

```svelte
  {#if lastMergeId}
    <div class="undo-strip">
      已合并。
      <button class="mini" disabled={recording.isLive} onclick={undoLastMerge}>撤销</button>
      <button class="mini" onclick={() => (lastMergeId = null)}>好</button>
    </div>
  {/if}
```

2e. style 区加(复用页内已有 `.mini` 族):

```css
  .undo-strip {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 0.5rem 0.8rem;
    margin: 0.5rem 0 1rem;
    font-size: 0.85rem;
    color: var(--ink-secondary);
  }
```

注意:本人被并走跳转 `goto(/speakers/${s.winner})` 的场景撤销条随页面销毁消失——可接受(回执/日志仍在,极端场景走后端数据修复,不为此加跨页状态)。

- [ ] **Step 3: 验证与提交**

Run: `npm run check && npm test`
Expected: 通过;grep 确认无残留:`grep -rn "toggleSugSample\|recomputeTidy\|tidyOpen" src/routes/speakers/+page.svelte` 无输出。

```bash
git add src/routes/speakers/+page.svelte "src/routes/speakers/[id]/+page.svelte"
git commit -m "feat(speakers): 概览页收敛为收件箱摘要卡,详情页手动合并可撤销"
```

---

### Task 10: 文档同步 + 全量验证

**Files:**
- Modify: `DESIGN.md`(tidy-card 一节,约 160 行)
- Modify: `README.md`(约 24 行、240 行)

- [ ] **Step 1: DESIGN.md**

约 160 行整段 `- **tidy-card**(会议搭子概览页「整理」卡):…` 替换为:

```markdown
- **整理收件箱**(会议搭子整理,三面入口一个队列):高置信归属建议(裸余弦 ≥0.74 或 S-Norm z≥3,且 loser 未命名、不在落盘拒绝名单)在录制停止/启动/库变化时**自动归并**——每条合并前把双方快照+样本副本落 `merge_journal/`(上限 50 条),单条可撤销;后续触及双方的操作使条目失效(由另一次合并失效的,撤销那次合并即复活=LIFO 链式撤销);撤销过的 pair 落盘拒绝名单,自动归并不再碰。拿不准的进 **/speakers/tidy** 逐条审阅流:一次一张大卡,队列=自动归并回执→归属建议→同名组→无样本条目,卡上给双方全部样本试听(带日期)+最近 3 场会议+相似度;动作 合并/忽略(保留)/跳过,回执卡 好/撤销(失效变灰注明原因);键盘 Enter/X/S/1-9(试听)/Esc,试听单实例互斥切卡即停;录制中只读浏览。**入口收敛**:①侧栏「概览与整理」行徽标=队列总数(warning 药丸,0 隐藏);②概览页一张摘要卡(N 件待处理 · M 条已自动归并 + 「开始整理」直达);③详情页头部 ctx-card 照旧(建议跟人走,行内合并/忽略),手动合并后给「已合并·撤销」条。四处同源(`src/lib/tidy.svelte.ts` 会话态+`tidyQueue.ts` 纯队列;忽略不落盘,拒绝名单落盘)。
```

- [ ] **Step 2: README.md**

约 24 行 `- **会议搭子整理与归属建议**:…` 句首部分改写为(保留原句后半"同名重名引导…越用越准"不动):

```markdown
- **会议搭子自动归并与整理收件箱**:未命名说话人自动声纹再辨认(S-Norm 分数正规化,跨会议信道漂移下也能浮出可信推荐),高置信的自动归并且随时可撤销(合并日志快照兜底,撤销过的不再自动犯);拿不准的进整理收件箱逐条审阅,卡上直接试听双方原声、查看出现过的会议再拍板;同名重名引导关联/合并;无样本碎片条目一键清理。每人保留多份"会话质心"(戴耳机/外放等不同状态各有代表声纹),越用越准。
```

约 240 行 `5. 侧栏「概览与整理」有待办徽标时点进去:…` 改为:

```markdown
5. 侧栏「概览与整理」有徽标时点进「整理收件箱」:高置信的已自动归并(回执可撤销),剩下的逐条试听拍板——合并、忽略或清理认不出的无样本条目。
```

- [ ] **Step 3: 全量验证**

```bash
cd src-tauri && cargo test && cd ..
npm test
npm run check
```

Expected: 三者全绿。

- [ ] **Step 4: 提交**

```bash
git add DESIGN.md README.md
git commit -m "docs: 整理收件箱与自动归并文档同步"
```

---

## 自审记录(计划完成后已核对)

- **规格覆盖**:入口收敛(T7/T9)、自动归并管线与触发时机(T2/T5/T7)、合并日志/失效/复活/拒绝名单(T1-T4)、手动合并可撤销(T5/T8/T9)、审阅流四类卡+键盘+试听(T6/T8)、录制中只读(T5/T8)、错误处理(各命令透传+日志失败放弃合并 T2/T5)、测试清单(Rust:快照往返/失效各入口/阈值筛选/上限淘汰;vitest:队列/键盘/试听互斥)、文档同步(T10)。规格「回执卡附双方试听」实现为 winner 侧试听(loser 样本已并入 winner,物理上不可分),偏差已在页面文案与本计划注明。
- **类型一致性**:`MergeReceipt` 字段 Rust/TS 同名同序;`journalId` camelCase 经 Tauri 自动映射 `journal_id`;`tidyItemKey` 前缀(r:/s:/d:/n:)在 store `dismiss`、页面、测试三处一致;`confident_picks` 消费 `SUGGEST_STRONG_RAW` 与前端 `isStrong` 字面值 0.74 对齐并加注释互指。
- **占位符**:无 TBD/TODO;所有代码块完整可编译级;`snap()` 形参以现文件为准的说明是对既有辅助的引用而非占位。
