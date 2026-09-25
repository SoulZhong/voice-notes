//! 说话人表(speakers.json)的领域规则:一篇笔记里每个说话人的名字、人物关联、
//! 声纹建议、多人标记与拆分占号,各种编辑下这些字段**怎么一起变**,只在这里定义。
//!
//! 为什么单独成模块:这些字段的联动规则此前散在 NoteStore 的十几个写方法里各写
//! 一遍,已经写出分歧——一键拆分收尾的关联不清本地名、识别撤销的解除关联不清
//! 名字也不清建议,盘上于是出现「关联已断、名字还挂着」(2026-09-20 用户实报,
//! #231)。现在所有路径共用下面这几条规则:
//!
//! - **关联**(link):写 person_id;本地名清空(展示走库名 join,本地名留着会永远
//!   压过库名);声纹建议清空(人工结论取代建议);占号所有权清空(关联即启用)。
//! - **解除关联**(unlink):清 person_id;本地名**等于那个人库里现名**才清(那名字
//!   本就是那个人的,用户自己起的别的标签不动);建议指向那个人时清(刚否掉的人
//!   不得立刻又被建议一遍)。表项与段落归属一概不动。
//! - **改名**(rename):写本地名,清建议(用户亲自命名 = 已有判断)。
//! - **恢复原状**(restore_after_unsplit)刻意不走关联:它是撤销,不是新结论——
//!   打标时只清了 person_id、名字一直留着,只回填 person_id 才能原样复原。
//!
//! 规则本身是纯内存的(测试不需要临时目录);`read`/`flush` 只负责搬运,不加锁。
//! 加锁与读-改-写的时序由 NoteStore 的事务入口(`NoteStore::edit_speakers`)负责,
//! 调用方拿不到裸表去手写读-改-写。

use super::SpeakerMeta;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// 一篇笔记的说话人表。记住上次落盘的内容,`flush` 只在真有变化时写。
#[derive(Debug, Clone)]
pub(crate) struct SpeakerTable {
    map: BTreeMap<String, SpeakerMeta>,
    saved: BTreeMap<String, SpeakerMeta>,
}

impl SpeakerTable {
    /// 读盘。缺失/损坏视为空表(与既有 read_speakers 口径一致:老笔记没有这个文件)。
    pub(crate) fn read(dir: &Path) -> Self {
        let map: BTreeMap<String, SpeakerMeta> = std::fs::read_to_string(dir.join("speakers.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            saved: map.clone(),
            map,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_map(map: BTreeMap<String, SpeakerMeta>) -> Self {
        Self {
            saved: map.clone(),
            map,
        }
    }

    /// 有变化才原子落盘。事务入口在收尾时调一次;需要"先入表再改段"写序的路径
    /// (分配新号)可以中途先调,收尾时就不会重复写。
    pub(crate) fn flush(&mut self, dir: &Path) -> anyhow::Result<()> {
        if self.map != self.saved {
            super::write_speakers_atomic(dir, &self.map)?;
            self.saved = self.map.clone();
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn get(&self, speaker_id: &str) -> Option<&SpeakerMeta> {
        self.map.get(speaker_id)
    }

    pub(crate) fn contains(&self, speaker_id: &str) -> bool {
        self.map.contains_key(speaker_id)
    }

    fn existing(&mut self, speaker_id: &str) -> anyhow::Result<&mut SpeakerMeta> {
        self.map
            .get_mut(speaker_id)
            .ok_or_else(|| anyhow::anyhow!("笔记中没有该说话人: {speaker_id}"))
    }

    /// 改本地名。表项不存在则新建(改名可以作用于还没进表的说话人)。
    pub(crate) fn rename(&mut self, speaker_id: &str, name: &str) {
        let m = self
            .map
            .entry(speaker_id.to_string())
            .or_insert_with(SpeakerMeta::blank);
        m.name = name.to_string();
        m.hint_person = None;
    }

    /// 关联到库人物。说话人必须已存在——关联是"指认现有声音",不凭空造表项。
    pub(crate) fn link(&mut self, speaker_id: &str, person_id: &str) -> anyhow::Result<()> {
        let m = self.existing(speaker_id)?;
        m.person_id = Some(person_id.to_string());
        m.name = String::new();
        m.hint_person = None;
        m.reserved_by = None;
        Ok(())
    }

    /// 条件关联:当前关联为空或已是该人物才写,否则 Err(尊重已有关联)。幂等。
    pub(crate) fn link_if_unlinked(
        &mut self,
        speaker_id: &str,
        person_id: &str,
    ) -> anyhow::Result<()> {
        match self.existing(speaker_id)?.person_id.as_deref() {
            Some(cur) if cur == person_id => Ok(()),
            Some(cur) => anyhow::bail!("说话人 {speaker_id} 已关联 {cur},不覆盖(尊重已有关联)"),
            None => self.link(speaker_id, person_id),
        }
    }

    /// 解除关联。本就没关联时幂等返回。`library_name` 查那个人库里现名(查不到 → None,
    /// 此时名字一律不清:缺失/损坏时宁可留着,也不猜)。
    pub(crate) fn unlink(
        &mut self,
        speaker_id: &str,
        library_name: impl FnOnce(&str) -> Option<String>,
    ) -> anyhow::Result<()> {
        let m = self.existing(speaker_id)?;
        let Some(person_id) = m.person_id.take() else {
            return Ok(());
        };
        if let Some(pname) = library_name(&person_id) {
            if !m.name.is_empty() && m.name.trim() == pname.trim() {
                m.name = String::new();
            }
        }
        // 建议清不清只看 id,不依赖查库:未命名的自动人物是常态,查不到名字时
        // 把建议留着,就等于把用户刚否掉的结论原样再劝一遍。
        if m.hint_person.as_deref() == Some(person_id.as_str()) {
            m.hint_person = None;
        }
        Ok(())
    }

    /// CAS 解除:仅当当前关联仍是 `expect_person` 才解除,已被改成别人则拒绝。
    pub(crate) fn unlink_if(
        &mut self,
        speaker_id: &str,
        expect_person: &str,
        library_name: impl FnOnce(&str) -> Option<String>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.existing(speaker_id)?.person_id.as_deref() == Some(expect_person),
            "当前关联已被修改,拒绝撤销覆盖"
        );
        self.unlink(speaker_id, library_name)
    }

    /// CAS 改派/解除:当前关联原样等于 `expect_person` 才改成 `person_id`(None = 解除)。
    /// 幂等:已是目标值直接 Ok。
    pub(crate) fn relink_if(
        &mut self,
        speaker_id: &str,
        expect_person: &str,
        person_id: Option<&str>,
        library_name: impl FnOnce(&str) -> Option<String>,
    ) -> anyhow::Result<()> {
        let cur = self.existing(speaker_id)?.person_id.clone();
        if cur.as_deref() == person_id {
            return Ok(());
        }
        anyhow::ensure!(
            cur.as_deref() == Some(expect_person),
            "说话人 {speaker_id} 的关联已被改动,同步不覆盖"
        );
        match person_id {
            Some(p) => self.link(speaker_id, p),
            None => self.unlink(speaker_id, library_name),
        }
    }

    /// 打「多人混杂」标:置位同时清 person_id(混杂簇挂单人关联本身就是错的)。
    /// 名字保留,恢复原状时才能原样复原。
    pub(crate) fn mark_multi(&mut self, speaker_id: &str) -> anyhow::Result<()> {
        let m = self.existing(speaker_id)?;
        m.multi_speaker = true;
        m.person_id = None;
        Ok(())
    }

    /// 一键拆分的恢复原状:多人标记复位;人物关联按打标前快照回填(仅当现值为空——
    /// 用户此后自己关联过就不覆盖)。不走 link:见模块文档。
    pub(crate) fn restore_after_unsplit(
        &mut self,
        speaker_id: &str,
        prior_person: Option<&str>,
    ) -> anyhow::Result<()> {
        let m = self.existing(speaker_id)?;
        m.multi_speaker = false;
        if m.person_id.is_none() {
            m.person_id = prior_person.map(str::to_string);
        }
        Ok(())
    }

    /// 写声纹建议(仅展示)。表里没有的说话人跳过。
    pub(crate) fn set_hints(&mut self, hints: &[(String, String)]) {
        for (sid, pid) in hints {
            if let Some(m) = self.map.get_mut(sid) {
                m.hint_person = Some(pid.clone());
            }
        }
    }

    /// 接下来 n 个空闲 S 编号。max 跨表内键与段落里出现过的 id(防与孤儿 id 撞号)。
    pub(crate) fn next_ids<'a>(
        &self,
        seg_speakers: impl Iterator<Item = &'a str>,
        n: usize,
    ) -> Vec<String> {
        let num = |s: &str| {
            s.strip_prefix('S')
                .and_then(|x| x.parse::<u64>().ok())
                .unwrap_or(0)
        };
        let max_known = self
            .map
            .keys()
            .map(|k| num(k))
            .chain(seg_speakers.map(num))
            .max()
            .unwrap_or(0);
        (1..=n as u64)
            .map(|i| format!("S{}", max_known + i))
            .collect()
    }

    /// 新建一个空表项(分配新号用)。
    pub(crate) fn insert_blank(&mut self, speaker_id: &str) {
        self.map
            .insert(speaker_id.to_string(), SpeakerMeta::blank());
    }

    /// 拆分占号:一次性创建若干空表项,带 reserved_by 所有权与 split_born。
    /// 任一 id 已存在即整体失败(撞号:计划外有并发编辑,调用方重建计划)。
    pub(crate) fn reserve(&mut self, speaker_ids: &[String], op_id: &str) -> anyhow::Result<()> {
        if let Some(sid) = speaker_ids.iter().find(|s| self.map.contains_key(*s)) {
            anyhow::bail!("占号撞了({sid} 已存在),拆分计划需要重建");
        }
        for sid in speaker_ids {
            let mut m = SpeakerMeta::blank();
            m.reserved_by = Some(op_id.to_string());
            m.split_born = true;
            self.map.insert(sid.clone(), m);
        }
        Ok(())
    }

    /// 拆分改派前的目标校验:目标必须已存在;是别的 op 的预留号则拒绝
    /// (两个拆分共用同一私有号会互相覆盖关联)。
    pub(crate) fn check_split_target(&self, speaker_id: &str, op_id: &str) -> anyhow::Result<()> {
        let Some(m) = self.map.get(speaker_id) else {
            anyhow::bail!("目标说话人不存在: {speaker_id}(拆分不现场分配编号)");
        };
        if let Some(owner) = m.reserved_by.as_deref() {
            anyhow::ensure!(owner == op_id, "目标 {speaker_id} 是另一次拆分的预留号");
        }
        Ok(())
    }

    /// 改派启用预留号:只清**本 op** 的占号(清别人的等于窃取另一次拆分的预留)。
    pub(crate) fn claim_reserved(&mut self, speaker_id: &str, op_id: &str) {
        if let Some(m) = self.map.get_mut(speaker_id) {
            if m.reserved_by.as_deref() == Some(op_id) {
                m.reserved_by = None;
            }
        }
    }

    /// 取消拆分时清理占号:只删「本 op 预留、仍为空(未命名、未关联、无引用)」的表项。
    /// 返回删除数。
    pub(crate) fn release_reserved(&mut self, op_id: &str, referenced: &BTreeSet<String>) -> usize {
        let victims: Vec<String> = self
            .map
            .iter()
            .filter(|(sid, m)| {
                m.reserved_by.as_deref() == Some(op_id)
                    && m.name.is_empty()
                    && m.person_id.is_none()
                    && !referenced.contains(*sid)
            })
            .map(|(sid, _)| sid.clone())
            .collect();
        for sid in &victims {
            self.map.remove(sid);
        }
        victims.len()
    }

    /// 删表项(段落改归属由调用方先做,见 NoteStore::delete_speaker 的写序说明)。
    pub(crate) fn remove(&mut self, speaker_id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.map.remove(speaker_id).is_some(),
            "未知说话人: {speaker_id}"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(name: &str, person: Option<&str>, hint: Option<&str>) -> SpeakerMeta {
        let mut m = SpeakerMeta::blank();
        m.name = name.into();
        m.person_id = person.map(Into::into);
        m.hint_person = hint.map(Into::into);
        m
    }

    fn table(entries: &[(&str, SpeakerMeta)]) -> SpeakerTable {
        SpeakerTable::from_map(
            entries
                .iter()
                .map(|(k, m)| (k.to_string(), m.clone()))
                .collect(),
        )
    }

    const LIB: fn(&str) -> Option<String> = |p| (p == "P7").then(|| "王虎".to_string());

    /// 所有「关联」路径对字段的作用必须一致:这正是 #231 之前分歧的地方——
    /// 一键拆分收尾的条件关联不清本地名,取消关联后名字还挂着。
    #[test]
    fn every_link_path_clears_local_name_hint_and_reservation() {
        type Link = fn(&mut SpeakerTable) -> anyhow::Result<()>;
        let paths: [(&str, Link); 3] = [
            ("link", |t| t.link("S1", "P7")),
            ("link_if_unlinked", |t| t.link_if_unlinked("S1", "P7")),
            ("relink_if", |t| t.relink_if("S1", "P9", Some("P7"), LIB)),
        ];
        for (label, f) in paths {
            let mut start = meta("本地名", None, Some("P3"));
            start.reserved_by = Some("so-x".into());
            if label == "relink_if" {
                start.person_id = Some("P9".into());
            }
            let mut t = table(&[("S1", start)]);
            f(&mut t).unwrap();
            let m = t.get("S1").unwrap();
            assert_eq!(m.person_id.as_deref(), Some("P7"), "{label}");
            assert_eq!(m.name, "", "{label}:关联须清本地名,否则永远压过库名");
            assert_eq!(m.hint_person, None, "{label}:人工结论取代建议");
            assert_eq!(m.reserved_by, None, "{label}:关联即启用");
        }
    }

    /// 所有「解除关联」路径:那个人的名字随关联一起走,用户自己的标签留下,
    /// 指向那个人的建议清掉。
    #[test]
    fn every_unlink_path_drops_that_persons_name_and_hint_but_keeps_user_labels() {
        type Unlink = fn(&mut SpeakerTable) -> anyhow::Result<()>;
        let paths: [(&str, Unlink); 3] = [
            ("unlink", |t| t.unlink("S1", LIB)),
            ("unlink_if", |t| t.unlink_if("S1", "P7", LIB)),
            ("relink_if(None)", |t| t.relink_if("S1", "P7", None, LIB)),
        ];
        for (label, f) in paths {
            let mut t = table(&[("S1", meta("王虎", Some("P7"), Some("P7")))]);
            f(&mut t).unwrap();
            let m = t.get("S1").unwrap();
            assert_eq!(m.person_id, None, "{label}");
            assert_eq!(m.name, "", "{label}:本地名就是那个人的名字 → 清");
            assert_eq!(m.hint_person, None, "{label}:刚否掉的人不再被建议");

            let mut t = table(&[("S1", meta("左边那位", Some("P7"), Some("P3")))]);
            f(&mut t).unwrap();
            let m = t.get("S1").unwrap();
            assert_eq!(m.name, "左边那位", "{label}:用户自己的标签不替他丢");
            assert_eq!(
                m.hint_person.as_deref(),
                Some("P3"),
                "{label}:指向别人的建议不动"
            );
        }
    }

    #[test]
    fn unlink_keeps_name_when_library_is_unreadable_but_still_drops_hint() {
        let mut t = table(&[("S1", meta("王虎", Some("P7"), Some("P7")))]);
        t.unlink("S1", |_| None).unwrap();
        let m = t.get("S1").unwrap();
        assert_eq!(m.name, "王虎", "查不到库名就不猜");
        assert_eq!(m.hint_person, None, "建议只看 id");
    }

    #[test]
    fn cas_paths_refuse_when_link_changed_underneath() {
        let mut t = table(&[("S1", meta("", Some("P9"), None))]);
        assert!(t.link_if_unlinked("S1", "P7").is_err());
        assert!(t.unlink_if("S1", "P7", LIB).is_err());
        assert!(t.relink_if("S1", "P7", None, LIB).is_err());
        assert_eq!(
            t.get("S1").unwrap().person_id.as_deref(),
            Some("P9"),
            "拒绝时不动"
        );
        // 幂等:已是目标值
        t.link_if_unlinked("S1", "P9").unwrap();
        t.relink_if("S1", "P0", Some("P9"), LIB).unwrap();
    }

    #[test]
    fn unknown_speaker_is_refused_except_rename() {
        let mut t = table(&[]);
        assert!(t.link("S9", "P7").is_err());
        assert!(t.unlink("S9", LIB).is_err());
        assert!(t.mark_multi("S9").is_err());
        assert!(t.remove("S9").is_err());
        t.rename("S9", "新人");
        assert_eq!(t.get("S9").unwrap().name, "新人");
    }

    /// 打标 → 恢复原状是一对撤销:恢复后与打标前逐字段相同(名字、关联都回来)。
    #[test]
    fn mark_multi_then_restore_round_trips() {
        let before = meta("王虎", Some("P7"), None);
        let mut t = table(&[("S1", before.clone())]);
        t.mark_multi("S1").unwrap();
        assert!(t.get("S1").unwrap().multi_speaker);
        assert_eq!(t.get("S1").unwrap().person_id, None);
        t.restore_after_unsplit("S1", Some("P7")).unwrap();
        assert_eq!(t.get("S1").unwrap(), &before);
        // 此后用户自己关联过 → 恢复不覆盖
        t.link("S1", "P9").unwrap();
        t.restore_after_unsplit("S1", Some("P7")).unwrap();
        assert_eq!(t.get("S1").unwrap().person_id.as_deref(), Some("P9"));
    }

    #[test]
    fn next_ids_skip_orphan_segment_ids() {
        let t = table(&[("S1", meta("", None, None)), ("S3", meta("", None, None))]);
        assert_eq!(t.next_ids(["S5", "R2"].into_iter(), 2), vec!["S6", "S7"]);
        assert_eq!(table(&[]).next_ids(std::iter::empty(), 1), vec!["S1"]);
    }

    #[test]
    fn reservations_are_owned_by_one_split() {
        let mut t = table(&[("S1", meta("", None, None))]);
        assert!(t.reserve(&["S1".into()], "so-x").is_err(), "撞号整体失败");
        t.reserve(&["S2".into(), "S3".into()], "so-x").unwrap();
        assert!(t.get("S2").unwrap().split_born);
        assert!(
            t.check_split_target("S2", "so-y").is_err(),
            "别的 op 的预留号不许写"
        );
        assert!(t.check_split_target("S9", "so-x").is_err(), "不现场分配");
        t.claim_reserved("S2", "so-y");
        assert!(
            t.get("S2").unwrap().reserved_by.is_some(),
            "不窃取别人的预留"
        );
        t.claim_reserved("S2", "so-x");
        assert!(t.get("S2").unwrap().reserved_by.is_none());
        let referenced: BTreeSet<String> = BTreeSet::new();
        assert_eq!(
            t.release_reserved("so-x", &referenced),
            1,
            "只删仍为空的 S3"
        );
        assert!(t.contains("S2") && !t.contains("S3"));
    }

    #[test]
    fn flush_writes_only_on_change() {
        let tmp = tempfile::tempdir().unwrap();
        let mut t = SpeakerTable::read(tmp.path());
        t.flush(tmp.path()).unwrap();
        assert!(!tmp.path().join("speakers.json").exists(), "无变化不落盘");
        t.rename("S1", "甲");
        t.flush(tmp.path()).unwrap();
        assert_eq!(SpeakerTable::read(tmp.path()).get("S1").unwrap().name, "甲");
    }
}
