//! 全局声纹库(跨会议说话人身份)。单文件 voiceprints.json,挂在 app_data_dir 根
//! (与逐场笔记目录并列,不属于任何一场会议)。设计详见
//! docs/superpowers/specs/2026-07-05-voice-notes-voiceprint-library-design.md。
//!
//! 与 notes.rs 同一套原子写/静态锁/损坏容忍哲学,但库缺失/损坏绝不能挡住录制,
//! 因此 load 侧永不返回 Err——降级为空库 + eprintln。
//!
//! 本任务只落地本模块;lib.rs/session.rs 的接线(种子注入、停止时 upsert)是
//! 后续任务,在那之前生产代码路径不会调用这里的公开 API——本文件测试已覆盖
//! 全部行为,允许 dead_code 是有意为之,不是遗漏。

#![allow(dead_code)]

use crate::diar::registry::ClusterSnapshot;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

const SCHEMA_VERSION: u32 = 1;

/// 停止时够料自动入库的门槛(累计发声毫秒)。待真实会议数据校准。
pub const AUTO_ENROLL_MS: u64 = 10_000;

/// resolve 跟随 redirects 链的步数上限。merge 已做链条压扁,正常情况下一跳到底;
/// 这里是纯防御性上限,防止任何异常写入(例如手工改坏文件成环)导致死循环。
const MAX_REDIRECT_HOPS: u32 = 8;

/// 单一信道(mic/system)的声纹质心。count 是加权样本数——merge/upsert 按
/// (旧质心, count) 与 (新质心, count) 做加权平均,而非简单替换,防止新会话的
/// 短样本把稳定质心带偏。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonCentroid {
    pub vec: Vec<f32>,
    #[serde(default)]
    pub count: u64,
}

/// 库中一个人。name 空串 = 未命名,展示端兜底"未命名 · 最近出现 …"。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Person {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub centroids: BTreeMap<String, PersonCentroid>,
    #[serde(default)]
    pub total_ms: u64,
    #[serde(default)]
    pub last_seen: String,
}

/// voiceprints.json 整体结构。全部字段 `#[serde(default)]`:旧文件缺字段、
/// 未来新增字段都不该让解析失败——失败即触发 load 的"空库"降级,风险太大。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Voiceprints {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub next_person: u32,
    #[serde(default)]
    pub people: BTreeMap<String, Person>,
    /// 合并产生的旧引用重定向:loser id -> winner id。resolve 时链式跟随。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub redirects: BTreeMap<String, String>,
}

impl Default for Voiceprints {
    fn default() -> Self {
        Self { schema_version: SCHEMA_VERSION, next_person: 1, people: BTreeMap::new(), redirects: BTreeMap::new() }
    }
}

/// 全局写锁:voiceprints.json 的 read-modify-write 之间没有互斥会互相覆盖丢更新。
/// 与 notes.rs 的 EDIT_LOCK 同一哲学,但用独立锁——声纹库与笔记编辑是两类无关操作,
/// 没必要互相阻塞。毒化忽略(into_inner):每次落盘各自原子,持锁线程 panic 不留半写状态。
static VP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn vp_guard() -> std::sync::MutexGuard<'static, ()> {
    VP_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 全局声纹库静态读写。root 为 app_data_dir,文件固定名 voiceprints.json。
pub struct VoiceprintStore {
    root: PathBuf,
}

impl VoiceprintStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn path(&self) -> PathBuf {
        self.root.join("voiceprints.json")
    }

    /// 缺失/损坏 → 空库 + eprintln,绝不 Err:声纹库是识别增强功能,不能挡住录制主流程。
    pub fn load(&self) -> Voiceprints {
        match std::fs::read_to_string(self.path()) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                eprintln!("voiceprints.json 解析失败,按空库处理: {e}");
                Voiceprints::default()
            }),
            Err(_) => Voiceprints::default(),
        }
    }

    /// 原子写:先写 .tmp 再 rename,任何时刻磁盘上的 voiceprints.json 都完整。
    /// 首次覆盖已有文件前备份一份 .bak(仅当 .bak 尚不存在时才拷贝,保留的是
    /// "本机第一次跑这版代码前"的起点,而不是被每次写入滚动覆盖成最新内容)。
    fn save(&self, vp: &Voiceprints) -> anyhow::Result<()> {
        let path = self.path();
        if path.exists() {
            let bak = self.root.join("voiceprints.json.bak");
            if !bak.exists() {
                std::fs::copy(&path, &bak)?;
            }
        }
        let tmp = self.root.join("voiceprints.json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(vp)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// 跟随 redirects 链解析出当前有效 person id。悬空引用(目标已被删除)、
    /// 环形引用一律返回 None——调用方(notes join / upsert)容忍 None,不 panic。
    pub fn resolve<'a>(vp: &'a Voiceprints, id: &'a str) -> Option<&'a str> {
        let mut cur = id;
        for _ in 0..MAX_REDIRECT_HOPS {
            match vp.redirects.get(cur) {
                Some(next) => cur = next,
                None => return if vp.people.contains_key(cur) { Some(cur) } else { None },
            }
        }
        None // 超过步数上限,视为异常环,容忍返回 None 而非死循环
    }

    /// 改人物显示名。
    pub fn rename(&self, id: &str, name: &str) -> anyhow::Result<()> {
        let _guard = vp_guard();
        let mut vp = self.load();
        let person = vp.people.get_mut(id).ok_or_else(|| anyhow::anyhow!("未知人物: {id}"))?;
        person.name = name.to_string();
        self.save(&vp)
    }

    /// 把 loser 合并进 winner:质心逐 source 并入(同 source 加权平均,异 source 直插),
    /// total_ms 相加,winner 无名而 loser 有名则继承 loser 名;loser 从 people 移除,
    /// redirects 记 loser->winner 且把既有指向 loser 的项一并改指 winner(压扁链条)。
    pub fn merge(&self, loser: &str, winner: &str) -> anyhow::Result<()> {
        let _guard = vp_guard();
        let mut vp = self.load();
        if loser == winner {
            anyhow::bail!("不能与自己合并");
        }
        let loser_person = vp.people.remove(loser).ok_or_else(|| anyhow::anyhow!("未知人物: {loser}"))?;
        {
            let winner_person =
                vp.people.get_mut(winner).ok_or_else(|| anyhow::anyhow!("未知人物: {winner}"))?;
            for (source, lc) in loser_person.centroids {
                merge_centroid(winner_person, &source, lc);
            }
            winner_person.total_ms += loser_person.total_ms;
            if winner_person.name.is_empty() && !loser_person.name.is_empty() {
                winner_person.name = loser_person.name;
            }
        }
        for target in vp.redirects.values_mut() {
            if target == loser {
                *target = winner.to_string();
            }
        }
        vp.redirects.insert(loser.to_string(), winner.to_string());
        self.save(&vp)
    }

    /// 删除人物:移除 people 项 + 清掉所有指向它的 redirects(悬空引用交给 resolve 容忍)。
    pub fn delete(&self, id: &str) -> anyhow::Result<()> {
        let _guard = vp_guard();
        let mut vp = self.load();
        vp.people.remove(id);
        vp.redirects.retain(|_, target| target != id);
        vp.redirects.remove(id);
        self.save(&vp)
    }

    /// 停止时把本场簇快照并入库。
    /// - person=Some(经 redirects 解析) 的簇:按簇 sources 的主 source(BTreeSet 首个)
    ///   加权并入该 person 的质心,total_ms 累加,last_seen=now。
    /// - person=None 且 total_ms>=AUTO_ENROLL_MS 且质心非空:新建未命名 person。
    /// - 其余(不够料 / 悬空引用 / 无质心)一律忽略,不入库。
    /// 返回值:本次新建的 (会话簇 id -> person id) 映射,供调用方回填本场 speakers 表。
    pub fn upsert_from_session(
        &self,
        snaps: &[ClusterSnapshot],
        now: &str,
    ) -> anyhow::Result<BTreeMap<String, String>> {
        let _guard = vp_guard();
        let mut vp = self.load();
        let mut new_links = BTreeMap::new();
        for snap in snaps {
            let Some(source) = snap.sources.iter().next().cloned() else { continue };
            if let Some(person_id) = &snap.person {
                let Some(resolved) = Self::resolve(&vp, person_id).map(str::to_string) else {
                    continue; // 悬空引用(库中已删除该人):容忍跳过,不重建
                };
                if snap.centroid.is_empty() {
                    continue;
                }
                let person = vp.people.get_mut(&resolved).expect("resolve 已校验存在");
                let incoming = PersonCentroid { vec: snap.centroid.clone(), count: snap.count.max(1) };
                merge_centroid(person, &source, incoming);
                person.total_ms += snap.total_ms;
                person.last_seen = now.to_string();
            } else if snap.total_ms >= AUTO_ENROLL_MS && !snap.centroid.is_empty() {
                let id = format!("P{}", vp.next_person);
                vp.next_person += 1;
                let mut centroids = BTreeMap::new();
                centroids.insert(source, PersonCentroid { vec: snap.centroid.clone(), count: snap.count.max(1) });
                vp.people.insert(
                    id.clone(),
                    Person { name: String::new(), centroids, total_ms: snap.total_ms, last_seen: now.to_string() },
                );
                new_links.insert(snap.id.clone(), id);
            }
        }
        self.save(&vp)?;
        Ok(new_links)
    }
}

/// 同 source 质心按 count 加权平均后归一(与 diar/registry.rs detect_merges 同公式,
/// 两处独立维护是因为一个是会话内簇合并、一个是跨会话库合并,数据结构不同不便复用);
/// 异 source 直插(不同信道的声纹本就该独立保留,见 spec"数据模型"节)。
fn merge_centroid(person: &mut Person, source: &str, incoming: PersonCentroid) {
    match person.centroids.get_mut(source) {
        Some(existing) => {
            let (wn, ln) = (existing.count as f32, incoming.count as f32);
            let denom = (wn + ln).max(1.0); // 防两侧 count 均为 0 时除零成 NaN
            let mut merged: Vec<f32> =
                existing.vec.iter().zip(&incoming.vec).map(|(a, b)| (a * wn + b * ln) / denom).collect();
            if let Some(renorm) = normalize(&merged) {
                merged = renorm;
            }
            existing.vec = merged;
            existing.count += incoming.count;
        }
        None => {
            person.centroids.insert(source.to_string(), incoming);
        }
    }
}

fn normalize(v: &[f32]) -> Option<Vec<f32>> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !norm.is_finite() || norm < 1e-6 {
        return None;
    }
    Some(v.iter().map(|x| x / norm).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: &str, centroid: Vec<f32>, count: u64, sources: &[&str], person: Option<&str>, total_ms: u64) -> ClusterSnapshot {
        ClusterSnapshot {
            id: id.to_string(),
            centroid,
            count,
            sources: sources.iter().map(|s| s.to_string()).collect(),
            person: person.map(str::to_string),
            total_ms,
        }
    }

    #[test]
    fn load_missing_file_returns_empty_library() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let vp = store.load();
        assert!(vp.people.is_empty());
        assert!(vp.redirects.is_empty());
        assert_eq!(vp.next_person, 1);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        store.rename("P1", "张三").unwrap_err(); // 尚不存在,rename 应报错(非本用例重点,先确认无 panic)

        // 用 upsert 造一个人,再改名验证往返
        let snaps = vec![snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS)];
        let links = store.upsert_from_session(&snaps, "2026-07-05T10:00:00+08:00").unwrap();
        let pid = links.get("S1").unwrap().clone();
        store.rename(&pid, "张三").unwrap();

        let vp = store.load();
        assert_eq!(vp.people[&pid].name, "张三");
        assert_eq!(vp.people[&pid].total_ms, AUTO_ENROLL_MS);
        assert_eq!(vp.people[&pid].centroids["mic"].count, 5);
    }

    #[test]
    fn corrupt_file_falls_back_to_empty_library_without_panic() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("voiceprints.json"), "not json {{{").unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let vp = store.load();
        assert!(vp.people.is_empty());
    }

    #[test]
    fn save_backs_up_existing_file_before_first_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let bak_path = tmp.path().join("voiceprints.json.bak");

        // 第一次写入:文件尚不存在,没有"已有内容"可备份,不应产生 .bak。
        let snaps1 = vec![snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS)];
        store.upsert_from_session(&snaps1, "t1").unwrap();
        assert!(!bak_path.exists(), "首次创建不产生 .bak(没有旧内容可备份)");
        let content_after_first = std::fs::read_to_string(tmp.path().join("voiceprints.json")).unwrap();

        // 第二次写入:文件已存在,覆盖前应先备份"覆盖前"的内容。
        let snaps2 = vec![snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS)];
        store.upsert_from_session(&snaps2, "t2").unwrap();
        assert!(bak_path.exists());
        let bak_first = std::fs::read_to_string(&bak_path).unwrap();
        assert_eq!(bak_first, content_after_first, ".bak 保存的是覆盖前的内容");

        // 第三次写入:.bak 已存在,不应再被滚动覆盖(保留最早一次的备份起点)。
        let snaps3 = vec![snap("S3", vec![1.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS)];
        store.upsert_from_session(&snaps3, "t3").unwrap();
        let bak_after = std::fs::read_to_string(&bak_path).unwrap();
        assert_eq!(bak_first, bak_after, ".bak 只在首次覆盖前写一次,不随后续写入滚动");
    }

    #[test]
    fn resolve_follows_redirect_chain() {
        let mut vp = Voiceprints::default();
        vp.people.insert("P1".into(), Person { name: "张三".into(), ..Default::default() });
        vp.redirects.insert("P2".into(), "P1".into());
        vp.redirects.insert("P3".into(), "P2".into());
        assert_eq!(VoiceprintStore::resolve(&vp, "P3"), Some("P1"));
        assert_eq!(VoiceprintStore::resolve(&vp, "P2"), Some("P1"));
        assert_eq!(VoiceprintStore::resolve(&vp, "P1"), Some("P1"));
    }

    #[test]
    fn resolve_tolerates_self_loop_without_hanging() {
        let mut vp = Voiceprints::default();
        vp.people.insert("P1".into(), Person { name: "张三".into(), ..Default::default() });
        vp.redirects.insert("P1".into(), "P1".into()); // 手工损坏成环
        assert_eq!(VoiceprintStore::resolve(&vp, "P1"), None, "环形引用容忍返回 None,不死循环");
    }

    #[test]
    fn resolve_dangling_redirect_returns_none() {
        let vp = Voiceprints::default(); // P1 不存在
        assert_eq!(VoiceprintStore::resolve(&vp, "P1"), None);
        let mut vp2 = Voiceprints::default();
        vp2.redirects.insert("P2".into(), "P1".into()); // 目标 P1 已被删除
        assert_eq!(VoiceprintStore::resolve(&vp2, "P2"), None);
    }

    #[test]
    fn rename_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS)];
        let links = store.upsert_from_session(&snaps, "t1").unwrap();
        let pid = links["S1"].clone();
        store.rename(&pid, "李四").unwrap();
        assert_eq!(store.load().people[&pid].name, "李四");
    }

    #[test]
    fn merge_inserts_distinct_source_directly() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        // winner: mic 质心 [1,0] count=10;loser: mic 质心 [0,1] count=10(异 source: loser 多个 system)
        let snaps = vec![
            snap("S1", vec![1.0, 0.0], 10, &["mic"], None, AUTO_ENROLL_MS),
            snap("S2", vec![0.0, 1.0], 10, &["system"], None, AUTO_ENROLL_MS),
        ];
        let links = store.upsert_from_session(&snaps, "t1").unwrap();
        let winner = links["S1"].clone();
        let loser = links["S2"].clone();

        store.merge(&loser, &winner).unwrap();
        let vp = store.load();
        assert!(!vp.people.contains_key(&loser), "loser 从 people 移除");
        let w = &vp.people[&winner];
        // mic 只在 winner 里,直接保留;system 是 loser 独有,直插
        assert!(w.centroids.contains_key("mic"));
        assert!(w.centroids.contains_key("system"));
        assert_eq!(w.total_ms, AUTO_ENROLL_MS * 2);
        assert_eq!(vp.redirects.get(&loser), Some(&winner));
    }

    #[test]
    fn merge_same_source_weighted_average() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![
            snap("S1", vec![1.0, 0.0], 10, &["mic"], None, AUTO_ENROLL_MS),
            snap("S2", vec![0.0, 1.0], 10, &["mic"], None, AUTO_ENROLL_MS),
        ];
        let links = store.upsert_from_session(&snaps, "t1").unwrap();
        let winner = links["S1"].clone();
        let loser = links["S2"].clone();
        store.merge(&loser, &winner).unwrap();
        let vp = store.load();
        let mic = &vp.people[&winner].centroids["mic"];
        // 等权重(各10) → 归一化后约 [0.707, 0.707]
        assert!((mic.vec[0] - mic.vec[1]).abs() < 1e-4, "等权重加权平均应接近对称: {:?}", mic.vec);
        assert_eq!(mic.count, 20);
    }

    #[test]
    fn merge_inherits_loser_name_when_winner_unnamed_and_flattens_redirects() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![
            snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S3", vec![1.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
        ];
        let links = store.upsert_from_session(&snaps, "t1").unwrap();
        let (p1, p2, p3) = (links["S1"].clone(), links["S2"].clone(), links["S3"].clone());
        store.rename(&p2, "王五").unwrap(); // p1 无名,p2 有名

        // 先把 p3 合并进 p2(制造一条指向 p2 的既有 redirect),再把 p2 合并进 p1,
        // 验证 p3 -> p1 (压扁),且 p1 继承 "王五"。
        store.merge(&p3, &p2).unwrap();
        store.merge(&p2, &p1).unwrap();

        let vp = store.load();
        assert_eq!(vp.people[&p1].name, "王五", "winner 无名时继承 loser 名");
        assert_eq!(vp.redirects.get(&p2), Some(&p1));
        assert_eq!(vp.redirects.get(&p3), Some(&p1), "既有指向 p2 的重定向被压扁指向 p1");
        assert_eq!(VoiceprintStore::resolve(&vp, &p3), Some(p1.as_str()));
    }

    #[test]
    fn merge_rejects_self_and_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS)];
        let links = store.upsert_from_session(&snaps, "t1").unwrap();
        let p1 = links["S1"].clone();
        assert!(store.merge(&p1, &p1).is_err());
        assert!(store.merge(&p1, "P999").is_err());
        assert!(store.merge("P999", &p1).is_err());
    }

    #[test]
    fn delete_removes_person_and_dangling_redirects_are_tolerated() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![
            snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
        ];
        let links = store.upsert_from_session(&snaps, "t1").unwrap();
        let (p1, p2) = (links["S1"].clone(), links["S2"].clone());
        store.merge(&p2, &p1).unwrap(); // 制造指向 p1 的 redirect

        store.delete(&p1).unwrap();
        let vp = store.load();
        assert!(!vp.people.contains_key(&p1));
        assert!(!vp.redirects.contains_key(&p2), "指向被删人物的 redirect 一并清除");
        assert_eq!(VoiceprintStore::resolve(&vp, &p2), None, "悬空引用由 resolve 容忍返回 None");
    }

    #[test]
    fn upsert_writes_back_weighted_centroid_for_known_person() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        // 先建一个已知 person(种子来源:上一场的 upsert)
        let seed = vec![snap("S1", vec![1.0, 0.0], 10, &["mic"], None, AUTO_ENROLL_MS)];
        let links = store.upsert_from_session(&seed, "t1").unwrap();
        let pid = links["S1"].clone();

        // 第二场:该簇已带 person=Some(续录/种子命中),回写加权质心 + 累加 total_ms
        let second = vec![snap("S9", vec![0.0, 1.0], 10, &["mic"], Some(&pid), 3000)];
        let links2 = store.upsert_from_session(&second, "t2").unwrap();
        assert!(links2.is_empty(), "已关联 person 的簇不产生新建映射");

        let vp = store.load();
        let p = &vp.people[&pid];
        assert_eq!(p.total_ms, AUTO_ENROLL_MS + 3000);
        assert_eq!(p.last_seen, "t2");
        let mic = &p.centroids["mic"];
        assert!((mic.vec[0] - mic.vec[1]).abs() < 1e-4, "等权重回写应接近对称: {:?}", mic.vec);
        assert_eq!(mic.count, 20);
    }

    #[test]
    fn upsert_enrolls_new_person_when_over_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS)];
        let links = store.upsert_from_session(&snaps, "t1").unwrap();
        assert_eq!(links.len(), 1);
        let pid = &links["S1"];
        let vp = store.load();
        assert_eq!(vp.people[pid].name, "", "新建人物未命名");
        assert_eq!(vp.people[pid].total_ms, AUTO_ENROLL_MS);
        assert_eq!(vp.next_person, 2);
    }

    #[test]
    fn upsert_ignores_cluster_below_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS - 1)];
        let links = store.upsert_from_session(&snaps, "t1").unwrap();
        assert!(links.is_empty());
        assert!(store.load().people.is_empty());
    }

    #[test]
    fn upsert_ignores_cluster_with_empty_centroid_even_over_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![snap("S1", vec![], 0, &["mic"], None, AUTO_ENROLL_MS)];
        let links = store.upsert_from_session(&snaps, "t1").unwrap();
        assert!(links.is_empty(), "空质心不入库,即使 total_ms 够格");
        assert!(store.load().people.is_empty());
    }

    #[test]
    fn upsert_dangling_person_reference_is_skipped_not_recreated() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        // person 指向一个从未存在过的 id:resolve 应返回 None,upsert 应跳过而非报错/新建
        let snaps = vec![snap("S1", vec![1.0, 0.0], 5, &["mic"], Some("P999"), AUTO_ENROLL_MS)];
        let links = store.upsert_from_session(&snaps, "t1").unwrap();
        assert!(links.is_empty());
        assert!(store.load().people.is_empty());
    }
}
