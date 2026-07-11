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

/// 够料自动入库的门槛(累计发声毫秒)。2026-07-11 按真实库复盘上调 10s→30s:
/// 10s 时代攒出 37 个 0-8 分钟的未命名碎片(低电平/杂音段凑够即领 P 号),质心近
/// 随机、谁都认不出还污染种子池;30s 才够格算"真实参会者"。
pub const AUTO_ENROLL_MS: u64 = 30_000;

/// 每人录音样本上限。样本按会议逐份累积(试听区分"哪场的声音"),合并时双方样本
/// 合池、按声纹多样性保留(见 merge_with_embedder);超出上限的不再写/合并时按
/// "保留最不相似的组合"丢弃,防止长期使用无界膨胀(每份 ≤15s 16k s16 ≈ 480KB,
/// 10 份 ≈ 4.8MB/人封顶)。
pub const MAX_SAMPLES: usize = 10;

/// resolve 跟随 redirects 链的步数上限。merge 已做链条压扁,正常情况下一跳到底;
/// 这里是纯防御性上限,防止任何异常写入(例如手工改坏文件成环)导致死循环。
const MAX_REDIRECT_HOPS: u32 = 8;

/// 单一信道(mic/system)的声纹质心。count 是加权样本数——merge/upsert 按
/// (旧质心, count) 与 (新质心, count) 做加权平均,而非简单替换,防止新会话的
/// 短样本把稳定质心带偏。seen 是产生时间(会话质心用,主质心历史数据为空串)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonCentroid {
    pub vec: Vec<f32>,
    #[serde(default)]
    pub count: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub seen: String,
}

/// 每人每信道保留的会话质心("状态变体")上限:环形,满了挤最旧。
pub const SESSION_CENTROIDS_MAX: usize = 5;
/// 一场净增量够此时长才记会话质心:太短的出场代表不了一种"状态"。
pub const SESSION_CENTROID_MIN_MS: u64 = 10_000;

/// 库中一个人。name 空串 = 未命名,展示端兜底"未命名 · 最近出现 …"。
/// centroids 是每信道**主质心**(全部历史加权平均,识别的稳定锚);
/// session_centroids 是每信道最近若干场的**会话质心**——同一个人不同状态
/// (戴耳机/外放/不同增益时代)各有代表向量,匹配取 max 解决"平均把状态搅在
/// 一起"的跨场认不回问题。旧 voiceprints.json 无此字段,serde default 兼容。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Person {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub centroids: BTreeMap<String, PersonCentroid>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub session_centroids: BTreeMap<String, Vec<PersonCentroid>>,
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

    /// 把 loser 合并进 winner(无嵌入器变体:样本超额时退回"winner 全留、loser 按序
    /// 补空槽"的旧行为)。命令层能拿到声纹模型时请走 merge_with_embedder。
    pub fn merge(&self, loser: &str, winner: &str) -> anyhow::Result<()> {
        self.merge_with_embedder(loser, winner, None)
    }

    /// 把 loser 合并进 winner:质心逐 source 并入(同 source 加权平均,异 source 直插),
    /// total_ms 相加,winner 无名而 loser 有名则继承 loser 名;loser 从 people 移除,
    /// redirects 记 loser->winner 且把既有指向 loser 的项一并改指 winner(压扁链条)。
    ///
    /// 录音样本随合并**合池保留**:双方样本合计 ≤ MAX_SAMPLES 时全部保留(loser 的迁入
    /// winner 空槽);超额时按**声纹多样性**挑保留集——对每份样本算嵌入,farthest-point
    /// 贪心保留彼此最不相似的 MAX_SAMPLES 份(样本的价值是"这个人听起来的不同样子",
    /// 留最不相似的组合比按时间/槽位序保留信息量大),winner 侧未入选的也会删。嵌入
    /// 不可得(embedder=None/模型损坏/文件读失败)的样本排最后按序补位,全部不可得时
    /// 即退化为旧行为。文件操作 best-effort,失败不回滚已保存的库——样本是试听增值层,
    /// 库结构一致性优先。
    pub fn merge_with_embedder(
        &self,
        loser: &str,
        winner: &str,
        mut embedder: Option<&mut dyn crate::diar::SpeakerEmbedder>,
    ) -> anyhow::Result<()> {
        let _guard = vp_guard();
        let mut vp = self.load();
        if loser == winner {
            anyhow::bail!("不能与自己合并");
        }
        let loser_person = vp.people.remove(loser).ok_or_else(|| anyhow::anyhow!("未知人物: {loser}"))?;
        {
            let winner_person =
                vp.people.get_mut(winner).ok_or_else(|| anyhow::anyhow!("未知人物: {winner}"))?;
            for (source, lc) in &loser_person.centroids {
                merge_centroid(winner_person, source, lc.clone());
                // loser 主质心降级为 winner 的会话变体:合并常见于"同一人不同状态被
                // 拆开",被并一方的状态画像正是要保留可匹配的信息。
                let mut v = lc.clone();
                if v.seen.is_empty() {
                    v.seen = loser_person.last_seen.clone();
                }
                winner_person.session_centroids.entry(source.clone()).or_default().push(v);
            }
            for (source, list) in &loser_person.session_centroids {
                winner_person
                    .session_centroids
                    .entry(source.clone())
                    .or_default()
                    .extend(list.iter().cloned());
            }
            // 变体归序截容:seen 升序(空串=最老的历史数据沉底优先淘汰),超限挤最旧。
            for list in winner_person.session_centroids.values_mut() {
                list.sort_by(|a, b| a.seen.cmp(&b.seen));
                let overflow = list.len().saturating_sub(SESSION_CENTROIDS_MAX);
                if overflow > 0 {
                    list.drain(0..overflow);
                }
            }
            winner_person.total_ms += loser_person.total_ms;
            if winner_person.name.is_empty() && !loser_person.name.is_empty() {
                winner_person.name = loser_person.name.clone();
            }
        }
        for target in vp.redirects.values_mut() {
            if target == loser {
                *target = winner.to_string();
            }
        }
        vp.redirects.insert(loser.to_string(), winner.to_string());
        self.save(&vp)?;

        // ── 样本归并 ──
        let w_paths = self.sample_paths_existing(winner);
        let l_paths = self.sample_paths_existing(loser);
        let mut keep_loser: Vec<PathBuf> = l_paths.clone();
        if w_paths.len() + l_paths.len() > MAX_SAMPLES {
            // 超额:全体候选(winner 在前,loser 在后——嵌入全不可得时的兜底序即旧行为)
            // 算嵌入,按多样性选保留集。
            let all: Vec<&PathBuf> = w_paths.iter().chain(l_paths.iter()).collect();
            let embs: Vec<Option<Vec<f32>>> = all
                .iter()
                .map(|p| embedder.as_deref_mut().and_then(|e| embed_wav_sample(p, e)))
                .collect();
            let keep = select_diverse(&embs, MAX_SAMPLES);
            // winner 侧未入选的就地删(腾出槽位),loser 侧只迁移入选的。
            for (i, p) in w_paths.iter().enumerate() {
                if !keep.contains(&i) {
                    if let Err(e) = std::fs::remove_file(p) {
                        eprintln!("声纹样本淘汰失败({winner},不影响库): {e}");
                    }
                }
            }
            keep_loser = l_paths
                .iter()
                .enumerate()
                .filter(|(i, _)| keep.contains(&(w_paths.len() + i)))
                .map(|(_, p)| p.clone())
                .collect();
            for lp in &l_paths {
                if !keep_loser.contains(lp) {
                    if let Err(e) = std::fs::remove_file(lp) {
                        eprintln!("声纹样本淘汰失败({loser},不影响库): {e}");
                    }
                }
            }
        }
        // 迁移保留的 loser 样本进 winner 空槽(容量经上面淘汰后必然足够)。
        for lp in keep_loser {
            let res = match self.next_free_sample_slot(winner) {
                Some(wp) => std::fs::rename(&lp, &wp),
                None => std::fs::remove_file(&lp),
            };
            if let Err(e) = res {
                eprintln!("声纹样本迁移失败({loser}->{winner},不影响库): {e}");
            }
        }
        Ok(())
    }

    /// 删除人物:移除 people 项 + 清掉所有指向它的 redirects(悬空引用交给 resolve 容忍)
    /// + 连带删除全部录音样本(best-effort)。
    pub fn delete(&self, id: &str) -> anyhow::Result<()> {
        let _guard = vp_guard();
        let mut vp = self.load();
        vp.people.remove(id);
        vp.redirects.retain(|_, target| target != id);
        vp.redirects.remove(id);
        self.save(&vp)?;
        for sample in self.sample_paths_existing(id) {
            if let Err(e) = std::fs::remove_file(&sample) {
                eprintln!("声纹样本删除失败({id},不影响库): {e}");
            }
        }
        Ok(())
    }

    /// 人物第 slot 份样本的路径:slot=1 沿用历史布局 voiceprints/<id>.wav(多样本
    /// 之前写下的旧样本天然是第 1 份),slot≥2 为 <id>-<slot>.wav。id 含路径分隔等
    /// 异常字符时返回 None(防御 IPC 传入构造路径;正常 id 恒为 P<n>)——绝不能映射
    /// 到共享兜底名,否则两个异常 id 会互相覆盖/串听对方的样本。
    fn sample_slot_path(&self, id: &str, slot: usize) -> Option<PathBuf> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        let name = if slot == 1 { format!("{id}.wav") } else { format!("{id}-{slot}.wav") };
        Some(self.root.join("voiceprints").join(name))
    }

    /// 人物现存的全部样本绝对路径,按槽位序(list_people 与合并迁移用)。
    /// 中间槽位缺失(历史删除)不影响后续槽位被列出。
    pub fn sample_paths_existing(&self, id: &str) -> Vec<PathBuf> {
        (1..=MAX_SAMPLES)
            .filter_map(|n| self.sample_slot_path(id, n))
            .filter(|p| p.exists())
            .collect()
    }

    /// 首个空样本槽(≤ MAX_SAMPLES);满员/非法 id 返回 None。
    fn next_free_sample_slot(&self, id: &str) -> Option<PathBuf> {
        (1..=MAX_SAMPLES)
            .filter_map(|n| self.sample_slot_path(id, n))
            .find(|p| !p.exists())
    }

    /// 为人物追加一份录音样本(16k 单声道 s16 WAV),写入首个空槽:
    /// - id 先经 redirects 解析(会话快照里的 person 引用可能已被合并);
    /// - 已有样本不覆盖(每场会议至多追加一份,试听可区分"哪场的声音");
    /// - 满员(MAX_SAMPLES)/解析失败(人物已删)/空样本静默跳过。
    /// 返回是否实际写入。
    ///
    /// 持 vp_guard:与 merge/delete 的样本文件迁移串行化,否则「停止入库写样本」
    /// 与管理页并发合并/删除会写出无主孤儿样本或把错人的音频挂到 winner 上。
    pub fn append_sample(&self, id: &str, samples: &[f32]) -> anyhow::Result<bool> {
        let _guard = vp_guard();
        let vp = self.load();
        let Some(resolved) = Self::resolve(&vp, id).map(str::to_string) else {
            return Ok(false);
        };
        if samples.is_empty() {
            return Ok(false);
        }
        let Some(path) = self.next_free_sample_slot(&resolved) else {
            return Ok(false); // 满员:样本够用了,不再累积
        };
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

    /// 删除某人的一份录音样本(按绝对路径指认,试听纠错用;样本不参与识别,删除
    /// 不影响认人)。路径必须是该人现存样本之一——IPC 传入的任意路径不可信,
    /// 绝不能直接 remove_file。id 先经 redirects 解析(详情页可能拿着旧引用)。
    /// 持 vp_guard:与 merge/delete 的样本文件迁移串行化,防删到正在迁移的文件。
    /// 删出的空槽由下一场会议的 append_sample(找首个空槽)自然补上。
    pub fn delete_sample(&self, id: &str, path: &std::path::Path) -> anyhow::Result<()> {
        let _guard = vp_guard();
        let vp = self.load();
        let Some(resolved) = Self::resolve(&vp, id).map(str::to_string) else {
            anyhow::bail!("未知人物: {id}");
        };
        if !self.sample_paths_existing(&resolved).iter().any(|p| p == path) {
            anyhow::bail!("不是该人物的样本文件");
        }
        std::fs::remove_file(path)?;
        Ok(())
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
                let incoming =
                    PersonCentroid { vec: snap.centroid.clone(), count: snap.count.max(1), seen: String::new() };
                merge_centroid(person, &source, incoming);
                person.total_ms += snap.total_ms;
                person.last_seen = now.to_string();
                push_session_centroid(person, &source, &snap.centroid, snap.count.max(1), snap.total_ms, now);
            } else if snap.total_ms >= AUTO_ENROLL_MS && !snap.centroid.is_empty() {
                let id = format!("P{}", vp.next_person);
                vp.next_person += 1;
                let mut centroids = BTreeMap::new();
                centroids.insert(
                    source.clone(),
                    PersonCentroid { vec: snap.centroid.clone(), count: snap.count.max(1), seen: String::new() },
                );
                let mut person = Person {
                    name: String::new(),
                    centroids,
                    session_centroids: BTreeMap::new(),
                    total_ms: snap.total_ms,
                    last_seen: now.to_string(),
                };
                push_session_centroid(&mut person, &source, &snap.centroid, snap.count.max(1), snap.total_ms, now);
                vp.people.insert(id.clone(), person);
                new_links.insert(snap.id.clone(), id);
            }
        }
        self.save(&vp)?;
        Ok(new_links)
    }
}

/// 会话质心入环:本场净增量够料(≥SESSION_CENTROID_MIN_MS)才记为一个"状态代表";
/// Vec 序即时间序,超限挤最旧。
fn push_session_centroid(
    person: &mut Person,
    source: &str,
    vec: &[f32],
    count: u64,
    total_ms: u64,
    now: &str,
) {
    if total_ms < SESSION_CENTROID_MIN_MS || vec.is_empty() {
        return;
    }
    let list = person.session_centroids.entry(source.to_string()).or_default();
    list.push(PersonCentroid { vec: vec.to_vec(), count, seen: now.to_string() });
    if list.len() > SESSION_CENTROIDS_MAX {
        list.remove(0);
    }
}

/// 声纹库 → 开录/精修种子(纯函数):每人每信道的主质心 + 各会话质心各成一个种子
/// 簇——同一个人不同状态各有代表向量,任一被命中即认出此人(匹配取 max 的簇级
/// 实现,registry 本就支持同 person 多种子簇)。已被合并/悬空引用剔除。
pub fn seed_clusters(vp: &Voiceprints) -> Vec<crate::diar::registry::SeedCluster> {
    let mut seeds = Vec::new();
    for (id, person) in &vp.people {
        if VoiceprintStore::resolve(vp, id) != Some(id.as_str()) {
            continue;
        }
        for c in person.centroids.values() {
            seeds.push(crate::diar::registry::SeedCluster {
                person: id.clone(),
                name: person.name.clone(),
                centroid: c.vec.clone(),
                count: c.count,
            });
        }
        for c in person.session_centroids.values().flatten() {
            seeds.push(crate::diar::registry::SeedCluster {
                person: id.clone(),
                name: person.name.clone(),
                centroid: c.vec.clone(),
                count: c.count,
            });
        }
    }
    seeds
}

/// 样本保留集选择:给定各样本的嵌入(None=取不到),容量 k,返回保留下标(升序)。
/// 原则=最大化两两不相似(farthest-point 贪心):种子取相似度最低的一对,之后每轮
/// 选"与已选集合的最大相似度最小"者。嵌入缺失的样本排最后按原序补位——能比对的
/// 优先按多样性选,比不了的听天由命;全部缺失时退化为按原序取前 k(=旧行为)。
pub(crate) fn select_diverse(embs: &[Option<Vec<f32>>], k: usize) -> Vec<usize> {
    let n = embs.len();
    if n <= k {
        return (0..n).collect();
    }
    let unit: Vec<Option<Vec<f32>>> =
        embs.iter().map(|e| e.as_ref().and_then(|v| normalize(v))).collect();
    let valid: Vec<usize> = (0..n).filter(|&i| unit[i].is_some()).collect();
    let mut picked: Vec<usize> = Vec::new();

    if valid.len() >= 2 {
        let sim = |a: usize, b: usize| -> f32 {
            let (x, y) = (unit[a].as_ref().unwrap(), unit[b].as_ref().unwrap());
            x.iter().zip(y).map(|(p, q)| p * q).sum()
        };
        // 种子:最不相似的一对(平手取下标序,保证确定性)。
        let (mut si, mut sj, mut smin) = (valid[0], valid[1], f32::INFINITY);
        for (ai, &a) in valid.iter().enumerate() {
            for &b in &valid[ai + 1..] {
                let s = sim(a, b);
                if s < smin {
                    (si, sj, smin) = (a, b, s);
                }
            }
        }
        picked.push(si);
        if picked.len() < k {
            picked.push(sj);
        }
        // 贪心扩:每轮加入"与已选的最大相似度最小"者。
        while picked.len() < k {
            let cand = valid
                .iter()
                .filter(|i| !picked.contains(i))
                .min_by(|&&a, &&b| {
                    let ma = picked.iter().map(|&p| sim(a, p)).fold(f32::NEG_INFINITY, f32::max);
                    let mb = picked.iter().map(|&p| sim(b, p)).fold(f32::NEG_INFINITY, f32::max);
                    ma.total_cmp(&mb)
                })
                .copied();
            match cand {
                Some(c) => picked.push(c),
                None => break, // 有效样本用尽,余量给无嵌入的补
            }
        }
    } else if valid.len() == 1 {
        picked.push(valid[0]);
    }
    // 无嵌入的按原序补满容量。
    for i in 0..n {
        if picked.len() >= k {
            break;
        }
        if unit[i].is_none() && !picked.contains(&i) {
            picked.push(i);
        }
    }
    picked.sort_unstable();
    picked
}

/// 嵌入前响度归一的目标 RMS。样本横跨不同增益时代(输入音量修复前后/AGC 演进),
/// 电平差会渗进嵌入拉低同人相似度;比较域内所有样本统一归到同一响度再嵌入。
const EMBED_TARGET_RMS: f32 = 0.08;

/// 波形响度归一:整体缩放到目标 RMS,削波保护(峰值封 0.99)。近静音(RMS<1e-4)
/// 不放大——那是无声/噪声底,抬上来只会放大垃圾。
pub(crate) fn normalize_loudness(samples: &mut [f32]) {
    let rms = (samples.iter().map(|x| x * x).sum::<f32>() / samples.len().max(1) as f32).sqrt();
    if rms < 1e-4 {
        return;
    }
    let peak = samples.iter().fold(0f32, |m, x| m.max(x.abs()));
    let scale = (EMBED_TARGET_RMS / rms).min(0.99 / peak.max(1e-6));
    for x in samples.iter_mut() {
        *x *= scale;
    }
}

/// 读样本 WAV 并算整段声纹嵌入(响度归一 + 向量归一化)。<1s 的样本嵌不出稳定
/// 声纹,视为不可得;读失败/嵌入失败一律 None——调用方按"排最后补位"容忍。
fn embed_wav_sample(path: &std::path::Path, e: &mut dyn crate::diar::SpeakerEmbedder) -> Option<Vec<f32>> {
    let mut r = hound::WavReader::open(path).ok()?;
    let mut samples: Vec<f32> =
        r.samples::<i16>().filter_map(|s| s.ok()).map(|v| v as f32 / 32768.0).collect();
    if samples.len() < 16_000 {
        return None;
    }
    normalize_loudness(&mut samples);
    e.embed(&samples).ok().and_then(|v| normalize(&v))
}

/// 整理·合并建议的**绝对档**相似度下限(裸余弦)。与种子命中(SEED_ASSIGN 0.68)/
/// 离线重聚类(AHC 0.68)同档;≥0.74 前端标"很可能"。
pub const SUGGEST_MERGE_THRESHOLD: f32 = 0.68;
/// **相对显著档**(S-Norm)的 z 分数下限:同人跨场信道漂移会把裸余弦压到 0.4-0.6,
/// 绝对档看不见;把分数换算成"相对这两人各自与全库其他人相似度分布的显著性"
/// (z=均值化的标准分)后,鹤立鸡群的配对即使裸分不高也值得推荐。
/// 校准依据(2026-07-11 真实库 63 人):raw≥0.68 建议数为 0;z≥2.5 浮出 12 对,
/// 人工核验方向合理(多个未命名指向同一真人)。
pub const SUGGEST_Z_THRESHOLD: f32 = 2.5;
/// 相对档的裸余弦地板:z 再高,裸分低于此值大概率是统计巧合,不推。
pub const SUGGEST_RAW_FLOOR: f32 = 0.45;
/// "很可能"徽标的显著性档(供前端与 ipc 层判断)。
pub const SUGGEST_STRONG_Z: f32 = 3.0;
/// cohort 统计的最少对比人数:库太小算不出稳定分布,只走绝对档。
const SNORM_MIN_COHORT: usize = 3;

/// 一条整理合并建议:把 loser 并入 winner。方向=未命名并入已命名;双方都未命名
/// 时数据薄的并入厚的。similarity 取双方共有信道质心余弦的最大值,source 是取到
/// 最大值的那个信道;salience 是该配对的 S-Norm 显著性(库太小算不出时 None)。
#[derive(Debug, Clone, PartialEq)]
pub struct MergeSuggestion {
    pub loser: String,
    pub winner: String,
    pub similarity: f32,
    pub source: String,
    pub salience: Option<f32>,
}

/// 整理·再辨认:未命名人物("待辨认"对象)逐一与库中其他人比对声纹质心,给出
/// 合并建议。准入=绝对档(raw ≥ 0.68)或相对档(S-Norm z ≥ 2.5 且 raw ≥ 0.45)。
/// 纯函数不做 IO,只读不改库——建议由用户确认后走既有 merge_person。每人只报
/// 最显著的一个归属;两个未命名互相命中只产出一条(配对去重)。
pub fn suggest_merges(vp: &Voiceprints) -> Vec<MergeSuggestion> {
    let ids: Vec<&String> = vp.people.keys().collect();
    let n = ids.len();
    // 每人每信道的向量组:主质心 + 各会话质心(状态变体),配对相似度取全组 max。
    let units: Vec<BTreeMap<&String, Vec<Vec<f32>>>> = ids
        .iter()
        .map(|id| {
            let p = &vp.people[*id];
            let mut m: BTreeMap<&String, Vec<Vec<f32>>> = BTreeMap::new();
            for (src, c) in &p.centroids {
                if let Some(u) = normalize(&c.vec) {
                    m.entry(src).or_default().push(u);
                }
            }
            for (src, list) in &p.session_centroids {
                for c in list {
                    if let Some(u) = normalize(&c.vec) {
                        m.entry(src).or_default().push(u);
                    }
                }
            }
            m
        })
        .collect();

    // 全配对相似度矩阵:共有信道内全组交叉取最大,记录取到最大值的信道。
    let mut sim: Vec<Vec<Option<(f32, String)>>> = vec![vec![None; n]; n];
    for i in 0..n {
        for j in (i + 1)..n {
            let mut best: Option<(f32, String)> = None;
            for (src, avs) in &units[i] {
                let Some(bvs) = units[j].get(*src) else { continue };
                for a in avs {
                    for b in bvs {
                        let s: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
                        if best.as_ref().map_or(true, |(bs, _)| s > *bs) {
                            best = Some((s, src.to_string()));
                        }
                    }
                }
            }
            sim[i][j] = best.clone();
            sim[j][i] = best;
        }
    }

    // 每人的 cohort 统计(与全库其他人的相似度均值/标准差),不足样本给 None。
    let stats: Vec<Option<(f32, f32)>> = (0..n)
        .map(|i| {
            let vals: Vec<f32> = (0..n).filter_map(|j| sim[i][j].as_ref().map(|(s, _)| *s)).collect();
            if vals.len() < SNORM_MIN_COHORT {
                return None;
            }
            let mean = vals.iter().sum::<f32>() / vals.len() as f32;
            let var = vals.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / (vals.len() - 1) as f32;
            Some((mean, var.sqrt().max(1e-3)))
        })
        .collect();
    let z_of = |i: usize, j: usize, s: f32| -> Option<f32> {
        let (ma, sa) = stats[i]?;
        let (mb, sb) = stats[j]?;
        Some(((s - ma) / sa + (s - mb) / sb) / 2.0)
    };

    let mut out: Vec<MergeSuggestion> = Vec::new();
    let mut seen_pairs: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    for i in 0..n {
        if !vp.people[ids[i]].name.is_empty() {
            continue; // 只有未命名者是"待辨认"对象
        }
        // 候选目标里挑最显著者:有 z 按 z,全无 z(小库)按裸分。
        let mut best: Option<(usize, f32, String, Option<f32>)> = None;
        for j in 0..n {
            if i == j {
                continue;
            }
            let Some((s, src)) = sim[i][j].clone() else { continue };
            let z = z_of(i, j, s);
            let eligible = s >= SUGGEST_MERGE_THRESHOLD
                || (z.map_or(false, |z| z >= SUGGEST_Z_THRESHOLD) && s >= SUGGEST_RAW_FLOOR);
            if !eligible {
                continue;
            }
            let key = (z.unwrap_or(f32::NEG_INFINITY), s);
            if best
                .as_ref()
                .map_or(true, |(_, bs, _, bz)| key > (bz.unwrap_or(f32::NEG_INFINITY), *bs))
            {
                best = Some((j, s, src, z));
            }
        }
        let Some((j, s, src, z)) = best else { continue };
        let (a, b) = (ids[i], ids[j]);
        let other_named = !vp.people[b].name.is_empty();
        let (loser, winner) = if other_named {
            (a.clone(), b.clone())
        } else if vp.people[a].total_ms > vp.people[b].total_ms {
            (b.clone(), a.clone())
        } else {
            (a.clone(), b.clone())
        };
        let pair = (loser.clone().min(winner.clone()), loser.clone().max(winner.clone()));
        if !seen_pairs.insert(pair) {
            continue;
        }
        out.push(MergeSuggestion { loser, winner, similarity: s, source: src, salience: z });
    }
    // 最显著的排前(小库无 z 时按裸分)。
    out.sort_by(|a, b| {
        let ka = (a.salience.unwrap_or(f32::NEG_INFINITY), a.similarity);
        let kb = (b.salience.unwrap_or(f32::NEG_INFINITY), b.similarity);
        kb.partial_cmp(&ka).unwrap_or(std::cmp::Ordering::Equal)
    });
    out
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

        // 逐份追加:第 1 份走历史布局 <id>.wav,第 2 份 <id>-2.wav。
        assert!(store.append_sample(&p1, &[0.5; 160]).unwrap());
        assert!(store.append_sample(&p1, &[0.9; 320]).unwrap());
        let paths = store.sample_paths_existing(&p1);
        assert_eq!(paths.len(), 2);
        assert!(paths[0].ends_with(format!("{p1}.wav")), "首份沿用旧布局: {paths:?}");
        assert!(paths[1].ends_with(format!("{p1}-2.wav")));
        assert!(store.sample_paths_existing(&p2).is_empty());
        let mut r = hound::WavReader::open(&paths[0]).unwrap();
        assert_eq!(r.spec().sample_rate, 16_000);
        assert_eq!(r.samples::<i16>().count(), 160);

        // 合并:winner(p2)无样本 → 继承 loser(p1)的全部两份。
        store.merge(&p1, &p2).unwrap();
        assert_eq!(store.sample_paths_existing(&p2).len(), 2, "winner 继承 loser 全部样本");
        assert!(store.sample_paths_existing(&p1).is_empty());

        // 经 redirects 的追加解析到 winner:winner 未满 → 继续追加成第 3 份。
        assert!(store.append_sample(&p1, &[0.1; 160]).unwrap());
        assert_eq!(store.sample_paths_existing(&p2).len(), 3);

        // 删除连带删全部样本。
        store.delete(&p2).unwrap();
        assert!(store.sample_paths_existing(&p2).is_empty(), "删除人物连带删全部样本");
    }

    /// suggest_merges 用的库构造:直接拼 Voiceprints(不经 upsert,好精确控制质心)。
    fn vp_with(people: &[(&str, &str, &str, Vec<f32>, u64)]) -> Voiceprints {
        // (id, name, source, centroid, total_ms)
        let mut vp = Voiceprints::default();
        for (id, name, src, vec, ms) in people {
            let mut centroids = BTreeMap::new();
            centroids.insert(src.to_string(), PersonCentroid { vec: vec.clone(), count: 5, seen: String::new() });
            vp.people.insert(
                id.to_string(),
                Person { name: name.to_string(), centroids, total_ms: *ms, last_seen: "t".into(), ..Default::default() },
            );
        }
        vp
    }

    #[test]
    fn suggest_merges_attributes_unnamed_to_similar_named_person() {
        // P1 张三 与 P2(未命名)同方向;P3(未命名)方向相反,不该有归属。
        let vp = vp_with(&[
            ("P1", "张三", "mic", vec![1.0, 0.0, 0.02], 60_000),
            ("P2", "", "mic", vec![0.99, 0.0, 0.0], 12_000),
            ("P3", "", "mic", vec![0.0, 1.0, 0.0], 12_000),
        ]);
        let s = suggest_merges(&vp);
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!(s[0].loser, "P2");
        assert_eq!(s[0].winner, "P1", "未命名并入已命名");
        assert!(s[0].similarity >= SUGGEST_MERGE_THRESHOLD);
        assert_eq!(s[0].source, "mic");
    }

    #[test]
    fn suggest_merges_pairs_unnamed_thin_into_thick_and_dedups() {
        let vp = vp_with(&[
            ("P1", "", "mic", vec![1.0, 0.0], 30_000),
            ("P2", "", "mic", vec![0.98, 0.05], 10_000),
        ]);
        let s = suggest_merges(&vp);
        assert_eq!(s.len(), 1, "双未命名互相命中只产出一条: {s:?}");
        assert_eq!(s[0].loser, "P2", "薄并入厚");
        assert_eq!(s[0].winner, "P1");
    }

    #[test]
    fn suggest_merges_ignores_below_threshold_disjoint_sources_and_named_candidates() {
        // 相似度不够:两方向余弦 ≈ 0.5 < 0.68。
        let low = vp_with(&[
            ("P1", "张三", "mic", vec![1.0, 0.0], 60_000),
            ("P2", "", "mic", vec![0.5, 0.87], 12_000),
        ]);
        assert!(suggest_merges(&low).is_empty());
        // 无共有信道:mic vs system 不可比。
        let disjoint = vp_with(&[
            ("P1", "张三", "mic", vec![1.0, 0.0], 60_000),
            ("P2", "", "system", vec![1.0, 0.0], 12_000),
        ]);
        assert!(suggest_merges(&disjoint).is_empty());
        // 已命名的人不是"待辨认"对象:两个同声纹的已命名人不产建议(重名有另一套流程)。
        let named = vp_with(&[
            ("P1", "张三", "mic", vec![1.0, 0.0], 60_000),
            ("P2", "李四", "mic", vec![1.0, 0.0], 30_000),
        ]);
        assert!(suggest_merges(&named).is_empty());
    }

    /// 相对显著档(S-Norm):裸余弦 0.55 达不到绝对档,但在"其他人全是陌生方向"的
    /// 库里鹤立鸡群 → 必须给建议且带 salience;反之在"大家彼此都 0.55"的拥挤库里,
    /// 同样的 0.55 毫无显著性 → 不给建议。
    #[test]
    fn suggest_merges_snorm_surfaces_standout_pair_and_rejects_crowded_cohort() {
        // 12 维:P1 张三=[1,0,...],候选 P2 与他 cos=0.55;其余 10 个已命名人各占一个
        // 正交基(与两者余弦 0)。
        let dim = 12usize;
        let mut ppl: Vec<(String, String, Vec<f32>, u64)> = Vec::new();
        let mut e1 = vec![0.0; dim];
        e1[0] = 1.0;
        ppl.push(("P1".into(), "张三".into(), e1, 60_000));
        let mut cand = vec![0.0; dim];
        cand[0] = 0.55;
        cand[1] = (1.0f32 - 0.55 * 0.55).sqrt();
        ppl.push(("P2".into(), String::new(), cand, 12_000));
        for k in 0..10 {
            let mut v = vec![0.0; dim];
            v[k + 2] = 1.0;
            ppl.push((format!("P{}", k + 3), format!("路人{k}"), v, 30_000));
        }
        let mut vp = Voiceprints::default();
        for (id, name, vec, ms) in &ppl {
            let mut centroids = BTreeMap::new();
            centroids.insert("mic".to_string(), PersonCentroid { vec: vec.clone(), count: 5, seen: String::new() });
            vp.people.insert(
                id.clone(),
                Person { name: name.clone(), centroids, total_ms: *ms, last_seen: "t".into(), ..Default::default() },
            );
        }
        let s = suggest_merges(&vp);
        assert_eq!(s.len(), 1, "鹤立鸡群的 0.55 必须浮出: {s:?}");
        assert_eq!((s[0].loser.as_str(), s[0].winner.as_str()), ("P2", "P1"));
        assert!(s[0].similarity > 0.54 && s[0].similarity < 0.56);
        assert!(s[0].salience.unwrap() >= SUGGEST_Z_THRESHOLD, "{:?}", s[0].salience);

        // 拥挤库:同样 0.55,但其余 10 人与双方也都 ~0.55(全库彼此都半像)→ z≈0,不推。
        let mut vp2 = Voiceprints::default();
        let mk = |theta: f32| -> Vec<f32> {
            // 全部向量与 e1 夹角相同(cos=0.55),彼此之间也大致同距:绕 e1 的锥面取点。
            let r = (1.0f32 - 0.55 * 0.55).sqrt();
            let mut v = vec![0.0; dim];
            v[0] = 0.55;
            v[1] = r * theta.cos();
            v[2] = r * theta.sin();
            v
        };
        let mut e1b = vec![0.0; dim];
        e1b[0] = 1.0;
        vp2.people.insert("P1".into(), Person { name: "张三".into(), centroids: BTreeMap::from([("mic".to_string(), PersonCentroid { vec: e1b, count: 5, seen: String::new() })]), total_ms: 60_000, last_seen: "t".into(), ..Default::default() });
        for k in 0..11 {
            let name = if k == 0 { String::new() } else { format!("路人{k}") };
            vp2.people.insert(
                format!("P{}", k + 2),
                Person { name, centroids: BTreeMap::from([("mic".to_string(), PersonCentroid { vec: mk(k as f32 * 0.5), count: 5, seen: String::new() })]), total_ms: 30_000, last_seen: "t".into(), ..Default::default() },
            );
        }
        let s2 = suggest_merges(&vp2);
        assert!(
            s2.iter().all(|m| m.similarity >= SUGGEST_MERGE_THRESHOLD),
            "拥挤 cohort 里 0.55 无显著性,只允许绝对档建议冒头: {s2:?}"
        );
    }

    #[test]
    fn normalize_loudness_scales_quiet_up_clamps_peak_and_skips_silence() {
        // 小声:RMS 0.01 → 抬到目标 0.08。
        let mut quiet = vec![0.01f32; 1000];
        normalize_loudness(&mut quiet);
        let rms = (quiet.iter().map(|x| x * x).sum::<f32>() / 1000.0).sqrt();
        assert!((rms - 0.08).abs() < 1e-3, "{rms}");
        // 高峰值:目标增益会削波 → 按峰值封顶(≤0.99)。
        let mut peaky: Vec<f32> = vec![0.01; 999];
        peaky.push(0.5);
        normalize_loudness(&mut peaky);
        assert!(peaky.iter().fold(0f32, |m, x| m.max(x.abs())) <= 0.99);
        // 近静音:不放大(抬上来只是噪声底)。
        let mut silent = vec![1e-5f32; 1000];
        normalize_loudness(&mut silent);
        assert!((silent[0] - 1e-5).abs() < 1e-9);
    }

    #[test]
    fn upsert_records_session_centroids_with_gate_and_ring_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let links = store
            .upsert_from_session(&[snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS)], "t0")
            .unwrap();
        let pid = links["S1"].clone();
        assert_eq!(store.load().people[&pid].session_centroids["mic"].len(), 1, "入库场即第一份变体");

        // 净增量 <10s 的场不记变体;≥10s 的记,环形上限 5(挤最旧)。
        store.upsert_from_session(&[snap("Sx", vec![0.9, 0.1], 2, &["mic"], Some(&pid), 5_000)], "t-short").unwrap();
        assert_eq!(store.load().people[&pid].session_centroids["mic"].len(), 1, "短场不记");
        for i in 0..6 {
            store
                .upsert_from_session(&[snap("Sx", vec![1.0, i as f32 * 0.1], 3, &["mic"], Some(&pid), 12_000)], &format!("t{}", i + 1))
                .unwrap();
        }
        let list = &store.load().people[&pid].session_centroids["mic"];
        assert_eq!(list.len(), SESSION_CENTROIDS_MAX);
        assert_eq!(list[0].seen, "t2", "最旧的(t0/t1)被挤出");
        assert_eq!(list.last().unwrap().seen, "t6");
    }

    #[test]
    fn merge_demotes_loser_main_centroid_to_winner_variant() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let links = store
            .upsert_from_session(
                &[
                    snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS),
                    snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
                ],
                "t1",
            )
            .unwrap();
        let (loser, winner) = (links["S1"].clone(), links["S2"].clone());
        store.merge(&loser, &winner).unwrap();
        let vp = store.load();
        let variants = &vp.people[&winner].session_centroids["mic"];
        // winner 自己的入库变体 + loser 的入库变体 + loser 主质心降级 = 3
        assert_eq!(variants.len(), 3, "{variants:?}");
        assert!(
            variants.iter().any(|v| { let u = normalize(&v.vec).unwrap(); u[0] > 0.99 }),
            "loser 的状态向量([1,0])必须以变体形式保留"
        );
    }

    #[test]
    fn seed_clusters_include_session_variants_and_skip_dangling() {
        let mut vp = Voiceprints::default();
        let pc = |x: f32, y: f32| PersonCentroid { vec: vec![x, y], count: 5, seen: "t".into() };
        vp.people.insert(
            "P1".into(),
            Person {
                name: "张三".into(),
                centroids: BTreeMap::from([("mic".to_string(), pc(1.0, 0.0))]),
                session_centroids: BTreeMap::from([("mic".to_string(), vec![pc(0.0, 1.0), pc(0.7, 0.7)])]),
                total_ms: 60_000,
                last_seen: "t".into(),
            },
        );
        vp.redirects.insert("P2".into(), "P1".into()); // 悬空/重定向不产种子
        let seeds = seed_clusters(&vp);
        assert_eq!(seeds.len(), 3, "主质心 1 + 变体 2");
        assert!(seeds.iter().all(|s| s.person == "P1" && s.name == "张三"));
    }

    #[test]
    fn suggest_merges_matches_via_session_variant_when_main_drifted() {
        // P1 张三主质心已被平均"搅偏"([0,1]),但留有一份 [1,0] 状态变体;
        // 未命名 P2 主质心 [1,0] → 全组 max 命中变体,裸分 1.0 走绝对档。
        let mut vp = Voiceprints::default();
        let pc = |x: f32, y: f32| PersonCentroid { vec: vec![x, y], count: 5, seen: "t".into() };
        vp.people.insert(
            "P1".into(),
            Person {
                name: "张三".into(),
                centroids: BTreeMap::from([("mic".to_string(), pc(0.0, 1.0))]),
                session_centroids: BTreeMap::from([("mic".to_string(), vec![pc(1.0, 0.0)])]),
                total_ms: 60_000,
                last_seen: "t".into(),
            },
        );
        vp.people.insert(
            "P2".into(),
            Person {
                name: String::new(),
                centroids: BTreeMap::from([("mic".to_string(), pc(1.0, 0.0))]),
                total_ms: 12_000,
                last_seen: "t".into(),
                ..Default::default()
            },
        );
        let s = suggest_merges(&vp);
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!((s[0].loser.as_str(), s[0].winner.as_str()), ("P2", "P1"));
        assert!(s[0].similarity > 0.99, "取全组 max 应命中变体: {}", s[0].similarity);
    }

    #[test]
    fn delete_sample_removes_named_slot_rejects_foreign_and_slot_gets_refilled() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let links = store
            .upsert_from_session(&[snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS)], "t1")
            .unwrap();
        let p1 = links["S1"].clone();
        for _ in 0..3 {
            store.append_sample(&p1, &[0.5; 16]).unwrap();
        }
        let paths = store.sample_paths_existing(&p1);
        assert_eq!(paths.len(), 3);

        // 删中间槽(第 2 份):只少这一份,其余槽位不动。
        store.delete_sample(&p1, &paths[1]).unwrap();
        let left = store.sample_paths_existing(&p1);
        assert_eq!(left.len(), 2);
        assert!(!left.contains(&paths[1]));

        // 再删同一路径:文件已不存在 → 不再属于该人样本,拒绝。
        assert!(store.delete_sample(&p1, &paths[1]).is_err());
        // 外来路径(存在但不是他的样本):拒绝且文件安然无恙。
        let foreign = tmp.path().join("innocent.wav");
        std::fs::write(&foreign, b"x").unwrap();
        assert!(store.delete_sample(&p1, &foreign).is_err());
        assert!(foreign.exists(), "校验失败绝不能碰无关文件");
        // 未知人物:拒绝。
        assert!(store.delete_sample("P999", &left[0]).is_err());

        // 删出的空槽被下一份样本自然补上(append 找首个空槽)。
        assert!(store.append_sample(&p1, &[0.7; 16]).unwrap());
        assert_eq!(store.sample_paths_existing(&p1).len(), 3);
        assert!(store.sample_paths_existing(&p1).contains(&paths[1]), "新样本落回被删的槽位");
    }

    #[test]
    fn append_sample_stops_at_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let links = store
            .upsert_from_session(&[snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS)], "t1")
            .unwrap();
        let p1 = links["S1"].clone();
        for _ in 0..MAX_SAMPLES {
            assert!(store.append_sample(&p1, &[0.5; 16]).unwrap());
        }
        assert!(!store.append_sample(&p1, &[0.5; 16]).unwrap(), "满员后不再追加");
        assert_eq!(store.sample_paths_existing(&p1).len(), MAX_SAMPLES);
    }

    #[test]
    fn merge_without_embedder_falls_back_to_slot_order_and_drops_excess() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![
            snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
        ];
        let links = store.upsert_from_session(&snaps, "t1").unwrap();
        let (p1, p2) = (links["S1"].clone(), links["S2"].clone());
        // loser 3 份、winner 上限-1 份:无嵌入器(旧行为)= winner 全留,loser 按序补
        // 1 个空槽,余下 2 份删除。样本 <1s,即使有嵌入器也嵌不出(排最后补位同序)。
        for _ in 0..3 {
            store.append_sample(&p1, &[0.5; 16]).unwrap();
        }
        for _ in 0..(MAX_SAMPLES - 1) {
            store.append_sample(&p2, &[0.7; 32]).unwrap();
        }
        store.merge(&p1, &p2).unwrap();
        assert!(store.sample_paths_existing(&p1).is_empty(), "loser 样本全部离场");
        assert_eq!(store.sample_paths_existing(&p2).len(), MAX_SAMPLES, "winner 填满即止,超额删除");
    }

    /// 假嵌入器:按信号符号翻转次数分档(响度不变特征——嵌入前的响度归一不该
    /// 影响分档,恒定直流=声线 A,交替方波=声线 B)。
    struct FlipEmbedder;
    impl crate::diar::SpeakerEmbedder for FlipEmbedder {
        fn embed(&mut self, s: &[f32]) -> anyhow::Result<Vec<f32>> {
            let flips = s.windows(2).filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0)).count();
            Ok(if flips > 100 { vec![0.0, 1.0] } else { vec![1.0, 0.0] })
        }
    }

    #[test]
    fn merge_with_embedder_keeps_most_dissimilar_samples() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![
            snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
        ];
        let links = store.upsert_from_session(&snaps, "t1").unwrap();
        let (loser, winner) = (links["S1"].clone(), links["S2"].clone());
        // winner 满员 10 份、全是同一声线(恒定直流);loser 1 份独特声线(交替方波)。
        // 旧行为会因 winner 满员直接丢掉 loser 的独特样本;多样性挑选必须留下它。
        for _ in 0..MAX_SAMPLES {
            store.append_sample(&winner, &vec![0.1; 16_000]).unwrap();
        }
        let square: Vec<f32> = (0..16_000).map(|i| if i % 2 == 0 { 0.5 } else { -0.5 }).collect();
        store.append_sample(&loser, &square).unwrap();

        let mut e = FlipEmbedder;
        store
            .merge_with_embedder(&loser, &winner, Some(&mut e as &mut dyn crate::diar::SpeakerEmbedder))
            .unwrap();

        let kept = store.sample_paths_existing(&winner);
        assert_eq!(kept.len(), MAX_SAMPLES, "保留数=上限");
        assert!(store.sample_paths_existing(&loser).is_empty());
        // 独特声线的那份必须幸存(方波含负采样,直流样本全为正)。
        let has_unique = kept.iter().any(|p| {
            let mut r = hound::WavReader::open(p).unwrap();
            r.samples::<i16>().filter_map(|s| s.ok()).any(|v| v < 0)
        });
        assert!(has_unique, "最不相似的样本必须保留,不能按槽位序丢弃: {kept:?}");
    }

    #[test]
    fn select_diverse_prefers_dissimilar_and_backfills_missing() {
        let v = |x: f32, y: f32| Some(vec![x, y]);
        // 3 选 2:两份近同 + 一份正交 → 保留正交那份 + 近同二选一。
        let picked = select_diverse(&[v(1.0, 0.0), v(0.99, 0.01), v(0.0, 1.0)], 2);
        assert!(picked.contains(&2), "{picked:?}");
        assert_eq!(picked.len(), 2);
        // 容量足够:全保留。
        assert_eq!(select_diverse(&[v(1.0, 0.0), None], 5), vec![0, 1]);
        // 嵌入缺失排最后补位:2 个有效正交 + 2 个 None,取 3 → 两个有效 + 第一个 None。
        let picked = select_diverse(&[None, v(1.0, 0.0), None, v(0.0, 1.0)], 3);
        assert_eq!(picked, vec![0, 1, 3], "有效优先,None 按原序补第一个: {picked:?}");
        // 全部缺失:退化为按原序取前 k(旧行为)。
        assert_eq!(select_diverse(&[None, None, None], 2), vec![0, 1]);
    }

    #[test]
    fn sample_path_rejects_traversal_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        for bad in ["../x", "a/b", "", "a\\b", ".."] {
            assert!(store.sample_paths_existing(bad).is_empty(), "非法 id 应得空(不得映射共享兜底名): {bad:?}");
        }
        // 写侧:未知 id 经 resolve 为 None,静默跳过不落文件。
        assert!(!store.append_sample("../x", &[0.1; 16]).unwrap());
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

        // N 段 × 2.5s 恰达 AUTO_ENROLL_MS 门槛;每段后跑一轮 enroll_pending(仿 process_final 节奏)。
        let n_segs = (AUTO_ENROLL_MS / 2500) as usize;
        for _ in 0..n_segs {
            r.assign(&[1.0, 0.0, 0.0], "mic", 40000).unwrap();
            r.enroll_pending();
        }
        let pid = r.speakers()[0].person.clone().expect("够料后应已实时入库");
        {
            let vp = store.load();
            assert_eq!(vp.people[&pid].centroids["mic"].count, n_segs as u64);
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
        assert_eq!(vp.people[&pid].centroids["mic"].count, n_segs as u64 + 2, "入库段+2 线性增长,不双计");
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
