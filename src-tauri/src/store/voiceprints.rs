//! 全局声纹库(跨会议说话人身份)。单文件 voiceprints.json,挂在 app_data_dir 根
//! (与逐场笔记目录并列,不属于任何一场会议)。设计详见
//! docs/superpowers/specs/2026-07-05-voice-notes-voiceprint-library-design.md。
//!
//! 与 notes.rs 同一套原子写/静态锁/损坏容忍哲学,但库缺失/损坏绝不能挡住录制,
//! 因此 load 侧永不返回 Err——降级为空库 + eprintln。
//!
//! lib.rs 已接线:种子注入(load_voiceprint_seeds)、停止时 upsert_from_session、
//! 以及 list/rename/merge/delete 四个 Tauri command,全部公开 API 均被消费。

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
    /// 录音样本随合并迁移:winner 无样本则继承 loser 的,有则删 loser 的(best-effort,
    /// 文件操作失败不回滚已保存的库——样本是试听增值层,库结构一致性优先)。
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
        self.save(&vp)?;

        if let (Some(lw), Some(ww)) = (self.sample_path(loser), self.sample_path(winner)) {
            if lw.exists() {
                let res =
                    if ww.exists() { std::fs::remove_file(&lw) } else { std::fs::rename(&lw, &ww) };
                if let Err(e) = res {
                    eprintln!("声纹样本迁移失败({loser}->{winner},不影响库): {e}");
                }
            }
        }
        Ok(())
    }

    /// 删除人物:移除 people 项 + 清掉所有指向它的 redirects(悬空引用交给 resolve 容忍)
    /// + 连带删除录音样本(best-effort)。
    pub fn delete(&self, id: &str) -> anyhow::Result<()> {
        let _guard = vp_guard();
        let mut vp = self.load();
        vp.people.remove(id);
        vp.redirects.retain(|_, target| target != id);
        vp.redirects.remove(id);
        self.save(&vp)?;
        if let Some(sample) = self.sample_path(id) {
            if sample.exists() {
                if let Err(e) = std::fs::remove_file(&sample) {
                    eprintln!("声纹样本删除失败({id},不影响库): {e}");
                }
            }
        }
        Ok(())
    }

    /// 人物录音样本路径:app_data/voiceprints/<id>.wav。id 含路径分隔等异常字符时
    /// 返回 None(防御 IPC 传入构造路径;正常 id 恒为 P<n>)——绝不能映射到共享
    /// 兜底名,否则两个异常 id 会互相覆盖/串听对方的样本。
    fn sample_path(&self, id: &str) -> Option<PathBuf> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        Some(self.root.join("voiceprints").join(format!("{id}.wav")))
    }

    /// 样本存在则返回绝对路径(list_people 用)。
    pub fn sample_path_if_exists(&self, id: &str) -> Option<PathBuf> {
        let p = self.sample_path(id)?;
        p.exists().then_some(p)
    }

    /// 为人物写入代表性录音样本(16k 单声道 s16 WAV):
    /// - id 先经 redirects 解析(会话快照里的 person 引用可能已被合并);
    /// - 已有样本不覆盖(样本是「确认此人是谁」的稳定参照,不随会话滚动);
    /// - 解析失败(人物已删)静默跳过。
    /// 返回是否实际写入。
    ///
    /// 持 vp_guard:与 merge/delete 的样本文件迁移串行化,否则「停止入库写样本」
    /// 与管理页并发合并/删除会写出无主孤儿样本或把错人的音频挂到 winner 上。
    pub fn write_sample_if_missing(&self, id: &str, samples: &[f32]) -> anyhow::Result<bool> {
        let _guard = vp_guard();
        let vp = self.load();
        let Some(resolved) = Self::resolve(&vp, id).map(str::to_string) else {
            return Ok(false);
        };
        let Some(path) = self.sample_path(&resolved) else {
            return Ok(false);
        };
        if path.exists() || samples.is_empty() {
            return Ok(false);
        }
        std::fs::create_dir_all(path.parent().expect("sample_path 恒有父目录"))?;
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: crate::store::audio::AUDIO_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        // 先写 .tmp 再 rename:样本文件也保持「任何时刻磁盘上都是完整 WAV」。
        let tmp = path.with_extension("wav.tmp");
        let mut w = hound::WavWriter::create(&tmp, spec)?;
        for s in samples {
            w.write_sample(crate::store::audio::f32_to_s16(*s))?;
        }
        w.finalize()?;
        std::fs::rename(&tmp, &path)?;
        Ok(true)
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
            // sources 恒空 ⇔ 未命中的库种子簇,勿回写勿入库(终审 triage①):assign 命中
            // 必 sources.insert,空集是种子铺底后本场从未被认领的信号,不是"真实说话人"。
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
/// incoming.count 恒为本场会话的净增量(registry::SpeakerRegistry::snapshot 已减去
/// 种子/续录带入的历史基数,见终审 triage②)，而不是"种子基数 + 本场增量"的全量——
/// 否则这里的加权平均会把库里已经计入过的历史样本数再计一遍，count 随每场停止
/// 复利膨胀，把新会话的质心增量权重错误地稀释掉。
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

    #[test]
    fn sample_write_read_merge_delete_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![
            snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
        ];
        let links = store.upsert_from_session(&snaps, "t1").unwrap();
        let (p1, p2) = (links["S1"].clone(), links["S2"].clone());

        // 写入 + 不覆盖语义。
        assert!(store.write_sample_if_missing(&p1, &[0.5; 160]).unwrap());
        assert!(!store.write_sample_if_missing(&p1, &[0.9; 160]).unwrap(), "已有样本不覆盖");
        assert!(store.sample_path_if_exists(&p1).is_some());
        assert!(store.sample_path_if_exists(&p2).is_none());
        let mut r = hound::WavReader::open(store.sample_path_if_exists(&p1).unwrap()).unwrap();
        assert_eq!(r.spec().sample_rate, 16_000);
        assert_eq!(r.samples::<i16>().count(), 160);

        // 合并:winner(p2)无样本 → 继承 loser(p1)的。
        store.merge(&p1, &p2).unwrap();
        assert!(store.sample_path_if_exists(&p2).is_some(), "winner 继承 loser 样本");
        assert!(store.sample_path_if_exists(&p1).is_none());

        // 经 redirects 的写入解析到 winner:winner 已有样本 → 不写。
        assert!(!store.write_sample_if_missing(&p1, &[0.1; 160]).unwrap());

        // 删除连带删样本。
        store.delete(&p2).unwrap();
        assert!(store.sample_path_if_exists(&p2).is_none(), "删除人物连带删样本");
    }

    #[test]
    fn merge_removes_loser_sample_when_winner_already_has_one() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![
            snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
        ];
        let links = store.upsert_from_session(&snaps, "t1").unwrap();
        let (p1, p2) = (links["S1"].clone(), links["S2"].clone());
        store.write_sample_if_missing(&p1, &[0.5; 16]).unwrap();
        store.write_sample_if_missing(&p2, &[0.7; 32]).unwrap();
        store.merge(&p1, &p2).unwrap();
        assert!(store.sample_path_if_exists(&p1).is_none(), "loser 样本已清理");
        let mut r = hound::WavReader::open(store.sample_path_if_exists(&p2).unwrap()).unwrap();
        assert_eq!(r.samples::<i16>().count(), 32, "winner 自己的样本保留");
    }

    #[test]
    fn sample_path_rejects_traversal_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        for bad in ["../x", "a/b", "", "a\\b", ".."] {
            assert!(store.sample_path(bad).is_none(), "非法 id 应得 None(不得映射共享兜底名): {bad:?}");
            assert!(store.sample_path_if_exists(bad).is_none());
        }
        // 写侧:未知 id 经 resolve 为 None,静默跳过不落文件。
        assert!(!store.write_sample_if_missing("../x", &[0.1; 16]).unwrap());
        assert!(!tmp.path().join("voiceprints").exists(), "非法 id 不产生任何样本文件");
    }

    /// 会话中实时入库端到端:enroller 装配后,无主簇够料(≥AUTO_ENROLL_MS)当场入库
    /// 领 P id;停止时的 snapshot→upsert 只再报入库之后的净增量,库 count/total_ms
    /// 线性增长不双计(与种子 triage②同一套增量语义)。
    #[test]
    fn live_enroll_then_stop_upsert_does_not_double_count() {
        use crate::diar::registry::SpeakerRegistry;

        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let store_cb = VoiceprintStore::new(tmp.path().to_path_buf());

        let mut r = SpeakerRegistry::new();
        r.set_enroller(
            AUTO_ENROLL_MS,
            Box::new(move |snap| {
                store_cb.upsert_from_session(std::slice::from_ref(snap), "t-live").ok()
                    .and_then(|links| links.get(&snap.id).cloned())
            }),
        );

        // 5 段 × 2s = 10s 恰达门槛;每段后跑一轮 enroll_pending(仿 process_final 节奏)。
        for _ in 0..5 {
            r.assign(&[1.0, 0.0, 0.0], "mic", 32000).unwrap();
            r.enroll_pending();
        }
        let pid = r.speakers()[0].person.clone().expect("够料后应已实时入库");
        {
            let vp = store.load();
            assert_eq!(vp.people[&pid].centroids["mic"].count, 5);
            assert_eq!(vp.people[&pid].total_ms, AUTO_ENROLL_MS);
        }

        // 入库后又说 2 段(4s),停止:snapshot 应只报增量,upsert 后线性累计。
        r.assign(&[1.0, 0.0, 0.0], "mic", 32000).unwrap();
        r.enroll_pending();
        r.assign(&[1.0, 0.0, 0.0], "mic", 32000).unwrap();
        r.enroll_pending();
        let snaps = r.snapshot();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].person.as_deref(), Some(pid.as_str()));
        assert_eq!(snaps[0].count, 2, "停止快照只报入库后的净增量");
        assert_eq!(snaps[0].total_ms, 4000);
        store.upsert_from_session(&snaps, "t-stop").unwrap();
        let vp = store.load();
        assert_eq!(vp.people[&pid].centroids["mic"].count, 7, "5+2 线性增长,不双计");
        assert_eq!(vp.people[&pid].total_ms, AUTO_ENROLL_MS + 4000);
        assert_eq!(vp.people[&pid].last_seen, "t-stop");
    }

    /// 终审 triage②端到端:种子带库 count=40 注入本场 registry,命中两次长段后停止。
    /// registry::snapshot() 应只导出本场净增量(2),upsert 回库后 count 应线性长到
    /// 42,而不是把种子基数 40 再报一遍变成 82(40+42)——回归"每场停止近似翻倍,库
    /// 质心学习率几何衰减"的复利膨胀问题。
    #[test]
    fn seed_count_does_not_compound_across_a_session_end_to_end() {
        use crate::diar::registry::{SeedCluster, SpeakerRegistry};

        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());

        // 库里先有一个人,mic 质心 count=40(模拟此前多场累积的样本数)。
        let seed_snap = vec![snap("S0", vec![1.0, 0.0, 0.0], 40, &["mic"], None, AUTO_ENROLL_MS)];
        let links = store.upsert_from_session(&seed_snap, "t0").unwrap();
        let pid = links["S0"].clone();
        assert_eq!(store.load().people[&pid].centroids["mic"].count, 40);

        // 本场:该人作为种子注入(count=40),命中两段长音频。
        let seeds =
            vec![SeedCluster { person: pid.clone(), name: String::new(), centroid: vec![1.0, 0.0, 0.0], count: 40 }];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        r.assign(&[1.0, 0.0, 0.0], "mic", 32000).unwrap();
        r.assign(&[1.0, 0.0, 0.0], "mic", 32000).unwrap();
        let session_snaps = r.snapshot();
        assert_eq!(session_snaps.len(), 1);
        assert_eq!(session_snaps[0].count, 2, "registry 导出应只是本场净增量,不含种子基数 40");
        assert_eq!(session_snaps[0].person.as_deref(), Some(pid.as_str()));

        // upsert 回库:应是 40+2=42,不该翻倍成 40+42=82。
        store.upsert_from_session(&session_snaps, "t1").unwrap();
        let vp = store.load();
        assert_eq!(
            vp.people[&pid].centroids["mic"].count, 42,
            "库 count 应线性增长,不因种子基数被重复计入而复利膨胀"
        );
    }
}
