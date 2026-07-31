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

    /// 被并入方(loser)合并前的样本快照副本路径,按文件名序。条目不存在/已被永久
    /// 失效清理/该侧无样本 → 空(回执卡隐藏该试听行)。
    pub fn loser_sample_copies(&self, id: &str) -> Vec<PathBuf> {
        let Some(dir) = self.samples_dir(id, "loser") else { return vec![] };
        let Ok(rd) = std::fs::read_dir(&dir) else { return vec![] };
        let mut out: Vec<PathBuf> = rd.flatten().map(|f| f.path()).collect();
        out.sort();
        out
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
    fn loser_sample_copies_lists_snapshot_or_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let j = MergeJournal::new(tmp.path().to_path_buf());
        let vpdir = tmp.path().join("voiceprints");
        let ls = fake_sample(&vpdir, "P1.wav");
        j.append(&entry("m-P1", "t1", "P1", "P2"), &[ls], &[]).unwrap();
        assert_eq!(j.loser_sample_copies("m-P1").len(), 1);
        assert!(j.loser_sample_copies("../evil").is_empty());
        // 永久性失效清理样本副本后为空
        j.invalidate(&["P1"], "此人随后被改名", None);
        assert!(j.loser_sample_copies("m-P1").is_empty());
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
