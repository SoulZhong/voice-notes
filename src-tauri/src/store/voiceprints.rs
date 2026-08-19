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

/// 够料自动识别/入库的门槛(累计发声毫秒)。累计 10s 即建立身份，
/// 让短发言者也能在本场会议内及时获得稳定人物编号。
pub const AUTO_ENROLL_MS: u64 = 10_000;

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
    /// 参会人邮箱(P3 日历):identify 建议确认时在三重唯一性防线下记录,
    /// 下一场参会人 email 精确命中即免模糊猜。已规范化(小写)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emails: Vec<String>,
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
    /// 产生质心的嵌入模型标签("campplus"/"eres2netv2")。不同模型的向量空间不可
    /// 混比;与当前选型不一致时种子注入/停止回写被 lib.rs 门禁跳过,直到重建完成。
    #[serde(default = "default_embedding_model")]
    pub embedding_model: String,
}

fn default_embedding_model() -> String {
    "campplus".into()
}

impl Default for Voiceprints {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            next_person: 1,
            people: BTreeMap::new(),
            redirects: BTreeMap::new(),
            embedding_model: default_embedding_model(),
        }
    }
}

/// 全局写锁:voiceprints.json 的 read-modify-write 之间没有互斥会互相覆盖丢更新。
/// 与 notes.rs 的 EDIT_LOCK 同一哲学,但用独立锁——声纹库与笔记编辑是两类无关操作,
/// 没必要互相阻塞。毒化忽略(into_inner):每次落盘各自原子,持锁线程 panic 不留半写状态。
static VP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 快照回放的净化:快照里的质心属于 `snapshot_model` 空间,库现在是 `lib_model`。
/// 不符(或快照来源不明,即空串)就**只还身份、把两张质心表都清空**,返回 true 表示
/// "这个人现在没有声纹了,需要一次重建把他从样本重新长出来"。
///
/// **两张表都要清**:种子注入与自动归并同时消费 `centroids` 与 `session_centroids`,
/// 只清主质心仍会把旧空间的会话质心注入新模型。
///
/// 选"清空"而不是"拒绝回放":人被拆回来了、名字在、样本文件也还原了,只是暂时没有
/// 声纹——下次重建就从样本重新长出来。拒绝回放会让用户彻底拿不回这个人。
fn sanitize_replayed(person: &mut Person, snapshot_model: &str, lib_model: &str) -> bool {
    if !snapshot_model.is_empty() && snapshot_model == lib_model {
        return false;
    }
    person.centroids.clear();
    person.session_centroids.clear();
    true
}

/// 向量空间门禁:这组向量是用哪个模型算的?与库当前 `embedding_model` 不符一律丢弃。
///
/// **为什么必须在写入这一侧判,而不是靠调用方自觉**:2026-08-19 定位到的一类问题是
/// "更早启动、用旧模型算完的写入,在切模型重建成功之后才落盘"——库标签是新的、内容
/// 是混的,而启动自愈只比标签不比内容,永远发现不了。调用方在算之前判过一次不算数,
/// 因为算的过程(解码 + 逐段嵌入)本身就要几十秒到几分钟,期间足够切一次模型。
///
/// 判据放在取锁之后、动数据之前:此刻读到的 `embedding_model` 就是本次写入将要落进
/// 的那一份,中间不会再变。
fn space_ok(vp: &Voiceprints, model: &str) -> bool {
    vp.embedding_model == model
}

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

    /// 变更入口统一失效钩子:触及这些人物的合并日志条目不可再撤销(快照已过时)。
    fn journal_invalidate(&self, touched: &[&str], reason: &str) {
        super::merge_journal::MergeJournal::new(self.root.clone()).invalidate(touched, reason, None);
    }

    /// 改人物显示名。
    pub fn rename(&self, id: &str, name: &str) -> anyhow::Result<()> {
        let _guard = vp_guard();
        let mut vp = self.load();
        let person = vp.people.get_mut(id).ok_or_else(|| anyhow::anyhow!("未知人物: {id}"))?;
        person.name = name.to_string();
        self.save(&vp)?;
        self.journal_invalidate(&[id], "此人随后被改名");
        Ok(())
    }

    /// 把 loser 合并进 winner(无嵌入器变体:样本超额时退回"winner 全留、loser 按序
    /// 补空槽"的旧行为)。命令层能拿到声纹模型时请走 merge_with_embedder。
    pub fn merge(&self, loser: &str, winner: &str) -> anyhow::Result<()> {
        // 不带嵌入器 = 只并库里已有的质心,两边本来就在同一空间;传库当前标签,
        // 门禁对这条路径恒成立(留着是为了让所有入口走同一道判据,不开后门)。
        let model = self.load().embedding_model.clone();
        self.merge_with_embedder(loser, winner, None, &model)
    }

    /// 把 loser 合并进 winner(拿锁 + 委托 merge_locked)。
    pub fn merge_with_embedder(
        &self,
        loser: &str,
        winner: &str,
        embedder: Option<&mut dyn crate::diar::SpeakerEmbedder>,
        model: &str,
    ) -> anyhow::Result<()> {
        let _guard = vp_guard();
        // 合并会把 loser 的质心并进 winner:两边必须同空间,否则并出来的是噪音。
        if !space_ok(&self.load(), model) {
            anyhow::bail!("声纹空间不符,拒绝合并(向量是 {model} 算的)");
        }
        self.merge_locked(loser, winner, embedder)
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
    ///
    /// 调用方必须已持 vp_guard(merge_with_embedder 的薄包装、merge_journaled 的
    /// 快照+合并同锁场景)。
    fn merge_locked(
        &self,
        loser: &str,
        winner: &str,
        mut embedder: Option<&mut dyn crate::diar::SpeakerEmbedder>,
    ) -> anyhow::Result<()> {
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
            // emails 并集:不并入会在撤销/拆回时静默丢失 loser 的邮箱。
            for e in &loser_person.emails {
                if !winner_person.emails.contains(e) {
                    winner_person.emails.push(e.clone());
                }
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
        model: &str,
    ) -> anyhow::Result<String> {
        let _guard = vp_guard();
        let vp = self.load();
        // 与 merge_with_embedder 同一道判据。不带嵌入器时并的是库里已有的质心、
        // 本来就同空间,调用方传库当前标签即可;带嵌入器时这道判据才真正起作用。
        if !space_ok(&vp, model) {
            anyhow::bail!("声纹空间不符,拒绝合并(向量是 {model} 算的)");
        }
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
            // 取自**落盘这一刻库的标签**,而不是调用方传进来的 model:快照记录的是
            // "被存下来的这两份质心属于哪个空间",那就是库当时的空间。
            embedding_model: vp.embedding_model.clone(),
            undo_phase: String::new(),
            undo_cleared_centroids: false,
        };
        let journal = super::merge_journal::MergeJournal::new(self.root.clone());
        journal.append(&entry, &self.sample_paths_existing(loser), &self.sample_paths_existing(winner))?;
        if let Err(e) = self.merge_locked(loser, winner, embedder) {
            // 合并没做成,日志不留——否则可"撤销"一次没发生的合并。
            if let Err(e2) = journal.remove(&entry.id) {
                eprintln!("合并失败后的日志清理也失败({}),该条目撤销等价于空操作: {e2}", entry.id);
            }
            return Err(e);
        }
        // 本次合并使触及双方的既有可撤销条目失效(不含自己):它们的快照已过时。
        // by=本条 id → 撤销本次合并时那些条目复活(LIFO 链式撤销)。
        journal.invalidate(&[loser, winner], "相关人物随后又被合并", Some(&entry.id));
        Ok(entry.id)
    }

    /// 按日志条目撤销一次合并:恢复双方记录与 redirects、还原样本副本、pair 落盘
    /// 进自动合并拒绝名单(同样的自动判断不犯第二次,重启也不犯)、由本次被撤销
    /// 合并所失效的旧条目复活(LIFO 链式撤销)。条目已失效 → Err 带原因。
    /// 库记录与**样本文件**都是硬要求:上面刚把双方现存样本全清了,样本恢复再失败
    /// 就无从复原(样本恢复只补缺失、不覆盖既有,失败保留 journal 供重试)。
    /// `needs_rebuild` 是**出参**(同 feedback::reinforce_person):质心一旦因跨空间被
    /// 置空就已经落盘,与后面样本恢复、删 journal 成不成功无关。做成返回值的话,
    /// 任何后续错误都会把它连同 `Err` 一起丢掉,那两个人从此永远没有声纹——库标签
    /// 没变,启动自愈也不会触发(codex review 实现轮三 P1)。调用方须先无条件处理它,
    /// 且**在释放锁之后**才发起重建。
    pub fn undo_merge(&self, journal_id: &str, needs_rebuild: &mut bool) -> anyhow::Result<()> {
        let _guard = vp_guard();
        let journal = super::merge_journal::MergeJournal::new(self.root.clone());
        let entry = journal.entry(journal_id)?;
        if let Some(reason) = &entry.invalid_reason {
            anyhow::bail!("不能撤销:{reason}");
        }
        let mut vp = self.load();
        use super::merge_journal::undo_phase;
        // 上次撤销做到哪一步了,**读盘上的记录,不靠推断**。"loser 在不在 people 里"
        // 只能证明库已还原,证明不了样本清理做到哪:删了一半就失败的话,跳过清理会让
        // 剩下的合并后样本占住槽位,快照那份被 copy_no_overwrite 静默跳过
        // (codex review 实现轮四 P1)。
        let phase = entry.undo_phase.as_str();
        // 上一次若已因跨空间清空过质心,这次仍欠一次重建——哪怕中途的重建已经从
        // 部分样本长出了非空质心(codex review 实现轮四 P2)。
        *needs_rebuild |= entry.undo_cleared_centroids;
        // 快照里的质心可能来自另一个模型空间(换模型重建之后撤销一次旧合并)。
        let mut loser_person = entry.loser_person.clone();
        let mut winner_person = entry.winner_person.clone();
        let mut cleared = sanitize_replayed(&mut loser_person, &entry.embedding_model, &vp.embedding_model);
        cleared |= sanitize_replayed(&mut winner_person, &entry.embedding_model, &vp.embedding_model);
        if phase.is_empty() {
            if cleared {
                eprintln!(
                    "撤销合并:快照来自 {} 空间(库现在是 {}),只还身份、质心置空,待重建",
                    if entry.embedding_model.is_empty() { "未知" } else { &entry.embedding_model },
                    vp.embedding_model
                );
            }
            vp.people.insert(entry.loser.clone(), loser_person);
            vp.people.insert(entry.winner.clone(), winner_person);
            vp.redirects.remove(&entry.loser);
            for k in &entry.redirects_to_loser {
                // 快照回放不得遮蔽现存 person:k 若已重建为独立说话人(拆回),redirect 插回
                // 会让 resolve 把他解析走,笔记归属与新样本落盘全部错人。
                if vp.people.contains_key(k) { continue; }
                vp.redirects.insert(k.clone(), entry.loser.clone());
            }
            self.save(&vp)?;
            // 置位放在 save **之后**:save 失败则质心还没被清空,不必重建。
            *needs_rebuild |= cleared;
            journal.set_undo_phase(journal_id, undo_phase::LIBRARY_RESTORED, cleared)?;
        }
        if phase != undo_phase::SAMPLES_CLEARED {
            // 样本还原:双方现存文件清掉(含合并时迁移/兜底截取的),快照副本拷回。
            // **清理失败也是硬条件**:恢复用的 copy_no_overwrite 遇到已存在的目标会原子
            // 跳过(那是给"只补缺失"的重试语义),所以这里没清干净的话,留在库里的就是
            // 合并**后**的那份样本,而快照那份被静默丢弃——撤销等于没撤干净。宁可整体
            // 失败、保留 journal 让用户重试(codex review 实现轮二 P1 的连带面)。
            for id in [entry.loser.as_str(), entry.winner.as_str()] {
                for p in self.sample_paths_existing(id) {
                    if let Err(e) = std::fs::remove_file(&p) {
                        return Err(anyhow::anyhow!(
                            "清理现存样本失败({id}),已保留可重试的日志条目: {e}"
                        ));
                    }
                }
            }
            // 清理确实做完了才记这一步。没记上就重做——重做会删掉两次尝试之间用户
            // 新录的东西,但那好过留一个"清了一半"的中间态永远判不出来。
            journal.set_undo_phase(journal_id, undo_phase::SAMPLES_CLEARED, cleared)?;
        }
        // **样本恢复是硬条件**(与 restore_merged_person 同):上面刚把双方现存样本
        // 全清了,而质心可能因跨空间被置空——此刻样本是唯一的恢复依据。失败就保留
        // journal 不删,留着让用户重试;删了就永久销毁依据(codex review 实现轮 P1)。
        match journal.restore_samples(journal_id, &self.root.join("voiceprints")) {
            // 这次补进去了新样本 → 之前那次重建(如果跑过)没看见它们,再排一次。
            Ok(n) if n > 0 && entry.undo_cleared_centroids => *needs_rebuild = true,
            Ok(_) => {}
            Err(e) => {
                eprintln!("撤销合并:样本还原失败,保留日志条目以便重试: {e}");
                return Err(anyhow::anyhow!("样本还原失败,已保留可重试的日志条目: {e}"));
            }
        }
        journal.deny_auto(&format!("{}>{}", entry.loser, entry.winner));
        // remove 失败则 revive 不执行:重试撤销幂等,链上条目只是暂失撤销入口。
        journal.remove(journal_id)?;
        journal.revive_invalidated_by(journal_id);
        Ok(())
    }

    /// 从失效日志条目拆回被并入方:按快照把 loser 重建为原编号独立说话人,还原
    /// 指向他的 redirects(历史笔记段落重新归他),pair 进自动归并拒绝名单,条目
    /// 删除。与 undo_merge 的区别:不动 winner(那次合并未被撤销,质心里已混入的
    /// 贡献不抽回,后续录制自然纠正),也不 revive 链上条目(它们的前置状态未还原,
    /// 复活会造成"可撤销"假象)。仅失效条目可拆;有效条目走 undo_merge。
    /// 拆回时该补哪一侧的样本副本。
    ///
    /// 平常只补 loser:那次合并没被撤销,winner 的样本一直好好的。**但如果这条目上有
    /// 一次没做完的撤销**(undo_phase 到了 samples_cleared),winner 的现存样本已经被那次
    /// 撤销删光了,只是还没拷回来。此时若仍只补 loser,随后条目被删,winner 的样本就永久
    /// 没了——而"进行中的撤销又被普通人物变更失效"恰恰会把用户逼上拆回这条路
    /// (codex review 实现轮六 P1)。两个恢复函数都只补缺失、不覆盖既有,多补一侧是安全的。
    fn restore_samples_for_phase(
        &self,
        journal: &super::merge_journal::MergeJournal,
        journal_id: &str,
        entry: &super::merge_journal::MergeJournalEntry,
    ) -> anyhow::Result<usize> {
        let dir = self.root.join("voiceprints");
        if entry.undo_phase == super::merge_journal::undo_phase::SAMPLES_CLEARED {
            eprintln!("拆回说话人:该条目上有一次没做完的撤销,winner 的样本也要补回来");
            journal.restore_samples(journal_id, &dir)
        } else {
            journal.restore_loser_samples(journal_id, &dir)
        }
    }

    /// 返回拆回的人物 id。`needs_rebuild` 是**出参**(同 undo_merge):置位表示质心被
    /// 清空了(快照来自另一个模型空间),调用方**在释放锁之后**要发起一次重建,否则这
    /// 个人永远没有声纹——库标签已是当前模型,启动自愈不会再动它,而 append_sample
    /// 只写 WAV 不产生质心。做成出参是因为清空已经落盘,后面样本恢复或删条目再失败
    /// 也不能把它跟着 `Err` 丢掉(codex review 实现轮三 P1)。
    pub fn restore_merged_person(
        &self,
        journal_id: &str,
        needs_rebuild: &mut bool,
    ) -> anyhow::Result<String> {
        let _guard = vp_guard();
        let journal = super::merge_journal::MergeJournal::new(self.root.clone());
        let entry = journal.entry(journal_id)?;
        if entry.invalid_reason.is_none() {
            anyhow::bail!("该条仍可直接撤销,不需要拆回");
        }
        let mut vp = self.load();
        use super::merge_journal::undo_phase;
        // 阶段读盘,不靠"loser 在不在 people 里"推断(同 undo_merge,实现轮四 P1)。
        // 拆回不清样本,所以只有"库已还原"一个阶段。
        // 上一次若已因跨空间清空过质心,这次仍欠一次重建——即使中途的重建已经从**部分**
        // 样本长出了非空质心,补齐样本之后还得再算一次(codex review 实现轮四 P2)。
        *needs_rebuild |= entry.undo_cleared_centroids;
        let restored_already =
            entry.undo_phase == undo_phase::LIBRARY_RESTORED || vp.people.contains_key(&entry.loser);
        if restored_already {
            // 上一次拆回已经把 loser 写回库(含 redirect),但在收尾前被打断,条目还留着。
            // 库不能再写一遍(会用快照覆盖掉此后可能发生的新变化),但**样本必须继续补**:
            // 上一次正是卡在那里,跳过它、直接删条目 = 永久销毁恢复依据。
            // restore_loser_samples 只补缺失、不覆盖既有,重试幂等。
            eprintln!("拆回说话人:{} 已还原过,视为上次拆回的重试(只补样本)", entry.loser);
            match self.restore_samples_for_phase(&journal, journal_id, &entry) {
                Err(e) => {
                    eprintln!("拆回说话人(重试):样本还原仍失败,继续保留日志条目: {e}");
                    return Err(anyhow::anyhow!("样本还原失败,已保留可重试的日志条目: {e}"));
                }
                // 这次补进去了新样本 → 之前那次重建(如果跑过)没看见它们,再排一次。
                Ok(n) if n > 0 => *needs_rebuild = true,
                Ok(_) => {}
            }
            // 兜底:库里那位质心为空(上次被净化过或本就无质心)同样欠一次重建。
            *needs_rebuild |= vp
                .people
                .get(&entry.loser)
                .is_some_and(|p| p.centroids.is_empty() && p.session_centroids.is_empty());
        } else {
            let mut loser_person = entry.loser_person.clone();
            let cleared = sanitize_replayed(&mut loser_person, &entry.embedding_model, &vp.embedding_model);
            if cleared {
                eprintln!(
                    "拆回说话人:快照来自 {} 空间(库现在是 {}),只还身份、质心置空,待重建",
                    if entry.embedding_model.is_empty() { "未知" } else { &entry.embedding_model },
                    vp.embedding_model
                );
            }
            vp.people.insert(entry.loser.clone(), loser_person);
            vp.redirects.remove(&entry.loser);
            for k in &entry.redirects_to_loser {
                // 快照回放不得遮蔽现存 person:k 若已重建为独立说话人(拆回),redirect 插回
                // 会让 resolve 把他解析走,笔记归属与新样本落盘全部错人。
                if vp.people.contains_key(k) { continue; }
                vp.redirects.insert(k.clone(), entry.loser.clone());
            }
            self.save(&vp)?;
            // 置位放在 save **之后**:save 失败则质心还没被清空,不必重建。
            *needs_rebuild |= cleared;
            journal.set_undo_phase(journal_id, undo_phase::LIBRARY_RESTORED, cleared)?;
            // **样本恢复是硬条件,不是 best-effort**:质心已经被清空(或本来就要靠样本
            // 重建),样本是此后唯一能把这个人的声纹长回来的依据。失败就**保留 journal**
            // ——留着条目,用户可以重试拆回;删了就永久失去恢复依据
            // (codex review 设计轮二 P1)。
            if let Err(e) = self.restore_samples_for_phase(&journal, journal_id, &entry) {
                eprintln!("拆回说话人:样本还原失败,保留日志条目以便重试: {e}");
                return Err(anyhow::anyhow!("样本还原失败,已保留可重试的日志条目: {e}"));
            }
        }
        journal.deny_auto(&format!("{}>{}", entry.loser, entry.winner));
        // winner 多半已被再次合并(这正是条目失效的主因):把它的当前化身也进名单,
        // 否则下一轮自动归并会把刚拆回的人立刻并进化身,拆回等于没拆。
        if let Some(current) = Self::resolve(&vp, &entry.winner) {
            if current != entry.winner {
                journal.deny_auto(&format!("{}>{}", entry.loser, current));
            }
        }
        journal.remove(journal_id)?;
        Ok(entry.loser.clone())
    }

    /// 确认合并回执(删条目及样本副本)。异步化后命令不再天然串行,journal 目录
    /// 写删必须与 invalidate/save_entry 等在同一把 vp_guard 下互斥,否则并发的
    /// invalidate 会把刚删的条目以"无快照失效条"复活。
    pub fn acknowledge_merge(&self, journal_id: &str) -> anyhow::Result<()> {
        let _guard = vp_guard();
        super::merge_journal::MergeJournal::new(self.root.clone()).acknowledge(journal_id)
    }

    /// 合并兜底截声写入日志条目目录。写条目目录必须持 vp_guard(异步化后命令
    /// 并发,裸写与 acknowledge/invalidate 的目录删写会交错);上游音频截取是
    /// 秒级重活,留在锁外,只锁这笔写。
    pub fn write_journal_cut_sample(&self, journal_id: &str, loser: &str, samples: &[f32]) {
        let _guard = vp_guard();
        super::merge_journal::MergeJournal::new(self.root.clone())
            .write_loser_cut_sample(journal_id, loser, samples);
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
        self.journal_invalidate(&[id], "此人随后被删除");
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
    /// 返回实际写入时解析后的人物 id(未写入则 None)。
    ///
    /// 持 vp_guard:与 merge/delete 的样本文件迁移串行化,否则「停止入库写样本」
    /// 与管理页并发合并/删除会写出无主孤儿样本或把错人的音频挂到 winner 上。
    fn append_sample_inner(&self, id: &str, samples: &[f32]) -> anyhow::Result<Option<String>> {
        let _guard = vp_guard();
        let vp = self.load();
        let Some(resolved) = Self::resolve(&vp, id).map(str::to_string) else {
            return Ok(None);
        };
        if samples.is_empty() {
            return Ok(None);
        }
        let Some(path) = self.next_free_sample_slot(&resolved) else {
            return Ok(None); // 满员:样本够用了,不再累积
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
        Ok(Some(resolved))
    }

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

    /// 切换嵌入模型后的库重建:每人用其录音样本按新模型重算质心(样本=人工核验过
    /// 的原声真值)。主质心=样本嵌入均值;会话变体=各样本嵌入(样本本就是历史场次
    /// 快照);样本无从追溯信道,统一写入该人既有信道键(无则 "mic")。无样本/嵌入
    /// 全失败的人清空质心——身份与历史保留,但新空间里无从匹配,等下次录到重新积累。
    /// 返回成功重建的人数。持 vp_guard 整程串行。
    pub fn rebuild_for_model(
        &self,
        model_tag: &str,
        e: &mut dyn crate::diar::SpeakerEmbedder,
    ) -> anyhow::Result<usize> {
        let _guard = vp_guard();
        let mut vp = self.load();
        let ids: Vec<String> = vp.people.keys().cloned().collect();
        let mut rebuilt = 0usize;
        for id in ids {
            let embs: Vec<Vec<f32>> = self
                .sample_paths_existing(&id)
                .iter()
                .filter_map(|p| embed_wav_sample(p, e))
                .collect();
            let person = vp.people.get_mut(&id).expect("刚枚举的 key");
            person.session_centroids.clear();
            if embs.is_empty() {
                person.centroids.clear();
                continue;
            }
            let dim = embs[0].len();
            let mut mean = vec![0f32; dim];
            for v in &embs {
                for (m, x) in mean.iter_mut().zip(v) {
                    *m += x;
                }
            }
            let mean = normalize(&mean).unwrap_or(mean);
            let count = embs.len() as u64;
            let sources: Vec<String> = if person.centroids.is_empty() {
                vec!["mic".into()]
            } else {
                person.centroids.keys().cloned().collect()
            };
            person.centroids.clear();
            let variants: Vec<PersonCentroid> = embs
                .iter()
                .take(SESSION_CENTROIDS_MAX)
                .map(|v| PersonCentroid { vec: v.clone(), count: 1, seen: String::new() })
                .collect();
            for src in &sources {
                person
                    .centroids
                    .insert(src.clone(), PersonCentroid { vec: mean.clone(), count, seen: String::new() });
                person.session_centroids.insert(src.clone(), variants.clone());
            }
            rebuilt += 1;
        }
        vp.embedding_model = model_tag.to_string();
        self.save(&vp)?;
        super::merge_journal::MergeJournal::new(self.root.clone())
            .invalidate_all("声纹库已按新模型重建");
        Ok(rebuilt)
    }

    /// 库内「无录音样本」的人数——切换嵌入模型前供前端预告:这些人 rebuild_for_model
    /// 会清空质心(新模型空间无从重建),重建完成前无法自动认出(名字/历史笔记不受
    /// 影响,等下次录到重新积累样本)。判据与 rebuild_for_model 一致(样本槽是否存在
    /// 于磁盘),只读不加锁。
    pub fn count_people_without_samples(&self) -> usize {
        let vp = self.load();
        vp.people.keys().filter(|id| self.sample_paths_existing(id).is_empty()).count()
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
        self.journal_invalidate(&[&resolved], "此人样本随后有变动");
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
        model: &str,
    ) -> anyhow::Result<BTreeMap<String, String>> {
        let _guard = vp_guard();
        let mut vp = self.load();
        if !space_ok(&vp, model) {
            eprintln!(
                "声纹入库丢弃:本场向量是 {model} 算的,库现在是 {};空间不可比,不写",
                vp.embedding_model
            );
            return Ok(BTreeMap::new());
        }
        let mut new_links = BTreeMap::new();
        let mut touched: Vec<String> = Vec::new();
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
                touched.push(resolved.clone());
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
                    emails: Vec::new(),
                };
                push_session_centroid(&mut person, &source, &snap.centroid, snap.count.max(1), snap.total_ms, now);
                vp.people.insert(id.clone(), person);
                new_links.insert(snap.id.clone(), id);
            }
        }
        self.save(&vp)?;
        if !touched.is_empty() {
            let refs: Vec<&str> = touched.iter().map(String::as_str).collect();
            self.journal_invalidate(&refs, "此人随后又录了新会议");
        }
        Ok(new_links)
    }

    /// 纠错回灌(spec P1-2):把人工指认段的重嵌入统计并入指定人物。与
    /// upsert_from_session 的区别:输入是历史段重嵌入(非会话净增量)、悬空
    /// 人物显式报错(回灌"成功"却没入库是最糟的静默)、返回前后快照供纠错
    /// 还原。质心/会话质心/时长/last_seen 的并入口径与会话路径一致;合并建议
    /// 回执按「此人有纠错回灌」失效——质心动了,旧建议的相似度不再可信,与
    /// "又录了新会议"同理。
    pub fn reinforce_feedback(
        &self,
        person_id: &str,
        stats: &[(String, Vec<f32>, u64, u64)], // (source, centroid, count, total_ms)
        now: &str,
        model: &str,
    ) -> anyhow::Result<Option<FeedbackApplied>> {
        let _guard = vp_guard();
        let mut vp = self.load();
        if !space_ok(&vp, model) {
            eprintln!(
                "回灌丢弃:这组向量是 {model} 算的,库现在是 {};空间不可比,不写",
                vp.embedding_model
            );
            return Ok(None);
        }
        let Some(resolved) = Self::resolve(&vp, person_id).map(str::to_string) else {
            anyhow::bail!("未知人物: {person_id}");
        };
        let person = vp.people.get_mut(&resolved).expect("resolve 已校验存在");
        let person_before = serde_json::to_string(person)?;
        for (source, centroid, count, total_ms) in stats {
            if centroid.is_empty() {
                continue;
            }
            merge_centroid(
                person,
                source,
                PersonCentroid { vec: centroid.clone(), count: (*count).max(1), seen: String::new() },
            );
            person.total_ms += total_ms;
            push_session_centroid(person, source, centroid, (*count).max(1), *total_ms, now);
        }
        person.last_seen = now.to_string();
        let person_after = serde_json::to_string(person)?;
        self.save(&vp)?;
        self.journal_invalidate(&[resolved.as_str()], "此人有纠错回灌");
        Ok(Some(FeedbackApplied { person_before, person_after }))
    }

    /// 建空人物(P2a 新面孔确认用):VP_LOCK 内分配 P<next_person>;空名报错;
    /// 质心/样本全空——确认后的 P1 feedback 回灌会让质心自然长出。
    pub fn create_person(&self, name: &str, now: &str) -> anyhow::Result<String> {
        let _guard = vp_guard();
        let name = name.trim();
        anyhow::ensure!(!name.is_empty(), "人物名不能为空");
        let mut vp = self.load();
        let id = format!("P{}", vp.next_person);
        vp.next_person += 1;
        vp.people.insert(
            id.clone(),
            Person {
                name: name.to_string(),
                centroids: BTreeMap::new(),
                session_centroids: BTreeMap::new(),
                total_ms: 0,
                last_seen: now.to_string(),
                emails: Vec::new(),
            },
        );
        self.save(&vp)?;
        Ok(id)
    }

    /// 记录参会人邮箱(P3):VP_LOCK 内规范化去重追加;质点变了,该人相关的
    /// 合并建议回执一并失效(与质心更新同理)。
    pub fn add_person_email(&self, id: &str, email: &str) -> anyhow::Result<()> {
        let _guard = vp_guard();
        let normalized = email.trim().to_ascii_lowercase();
        anyhow::ensure!(!normalized.is_empty(), "邮箱为空");
        let mut vp = self.load();
        let Some(resolved) = Self::resolve(&vp, id).map(str::to_string) else {
            anyhow::bail!("未知人物: {id}");
        };
        let person = vp.people.get_mut(&resolved).expect("resolve 已校验存在");
        if person.emails.contains(&normalized) {
            return Ok(());
        }
        person.emails.push(normalized);
        self.save(&vp)?;
        self.journal_invalidate(&[resolved.as_str()], "此人档案有更新");
        Ok(())
    }

    /// 补偿删除:仅当此人仍是空质心/空会话质心/无样本时删除(apply 半途失败的
    /// 孤儿清理)。有任何积累就拒删返回 false——那已经是有信息的真人档案。
    pub fn delete_person_if_empty(&self, id: &str) -> anyhow::Result<bool> {
        let _guard = vp_guard();
        let mut vp = self.load();
        let Some(resolved) = Self::resolve(&vp, id).map(str::to_string) else {
            return Ok(false);
        };
        let Some(p) = vp.people.get(&resolved) else {
            return Ok(false);
        };
        if !p.centroids.is_empty()
            || !p.session_centroids.is_empty()
            || !p.emails.is_empty()
            || !self.sample_paths_existing(&resolved).is_empty()
        {
            return Ok(false);
        }
        vp.people.remove(&resolved);
        self.save(&vp)?;
        Ok(true)
    }

    /// 纠错还原:当前状态仍逐字节等于 expected_after 时恢复 before,返回 true;
    /// 已被其它写(新会议/合并/再回灌)动过则不动返回 false——宁可留污染,
    /// 也不覆盖新信息。
    pub fn restore_feedback(
        &self,
        person_id: &str,
        before: &str,
        expected_after: &str,
        snapshot_model: &str,
    ) -> anyhow::Result<RestoreOutcome> {
        let _guard = vp_guard();
        let mut vp = self.load();
        let lib_model = vp.embedding_model.clone();
        let Some(resolved) = Self::resolve(&vp, person_id).map(str::to_string) else {
            return Ok(RestoreOutcome::Skipped); // 人都没了,无从还原也无需还原
        };
        let person = vp.people.get_mut(&resolved).expect("resolve 已校验存在");
        // **顺序是硬性的:先 CAS,再按空间决定还原什么。**
        // 倒过来做(先看空间、不符就直接改)会绕过 CAS,把后来发生的改名、邮箱等
        // 修改一起覆盖掉(codex review 设计轮二 P2)。
        if serde_json::to_string(person)? != expected_after {
            return Ok(RestoreOutcome::Skipped);
        }
        let mut restored: Person = serde_json::from_str(before)?;
        let cleared = sanitize_replayed(&mut restored, snapshot_model, &lib_model);
        *person = restored;
        self.save(&vp)?;
        self.journal_invalidate(&[resolved.as_str()], "纠错回灌已撤销");
        Ok(if cleared { RestoreOutcome::RestoredNeedsRebuild } else { RestoreOutcome::Restored })
    }
}

/// 回放一份历史快照的结局。
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum RestoreOutcome {
    /// 还原成功,快照与库同空间,质心照原样还回去了。
    Restored,
    /// 还原成功,但快照来自另一个模型空间 → 质心已置空,**调用方须在释放锁之后
    /// 发起一次重建**,否则这个人永远没有声纹。
    RestoredNeedsRebuild,
    /// 没还原:人已不在库里,或 CAS 不符(库被后续写动过)。
    Skipped,
}

/// reinforce_feedback 的前后快照:磁盘壳把它写进笔记级账本,纠错时
/// 「比对 after 未被动过才还原 before」。
pub struct FeedbackApplied {
    pub person_before: String,
    pub person_after: String,
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

/// 声纹库 → 开录/Aing 种子(纯函数):每人每信道的主质心 + 各会话质心各成一个种子
/// 簇——同一个人不同状态各有代表向量,任一被命中即认出此人(匹配取 max 的簇级
/// 实现,registry 本就支持同 person 多种子簇)。已被合并/悬空引用剔除。
pub fn seed_clusters(vp: &Voiceprints) -> Vec<crate::diar::registry::SeedCluster> {
    let mut seeds = Vec::new();
    for (id, person) in &vp.people {
        if VoiceprintStore::resolve(vp, id) != Some(id.as_str()) {
            continue;
        }
        for (src, c) in &person.centroids {
            seeds.push(crate::diar::registry::SeedCluster {
                person: id.clone(),
                name: person.name.clone(),
                centroid: c.vec.clone(),
                count: c.count,
                source: src.clone(),
            });
        }
        for (src, list) in &person.session_centroids {
            for c in list {
                seeds.push(crate::diar::registry::SeedCluster {
                    person: id.clone(),
                    name: person.name.clone(),
                    centroid: c.vec.clone(),
                    count: c.count,
                    source: src.clone(),
                });
            }
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
/// "很可能"徽标的裸余弦档(与前端 tidy.svelte.ts isStrong 同值);自动归并准入
/// 与展示徽标共用同一 strong 档——用户已信任"很可能",且撤销机制兜底。
pub const SUGGEST_STRONG_RAW: f32 = 0.74;
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

/// 自动归并筛选(纯函数):strong 档(裸余弦 ≥ SUGGEST_STRONG_RAW 或 z ≥
/// SUGGEST_STRONG_Z)+ loser 未命名(已命名条目不自动动)+ winner 已命名(往"说话人 N"上
/// 自动并毫无意义——归并的价值是把碎片归到确定身份上;未命名互并留给人工)+ 不在拒绝名单
/// + 双方未被本轮更早的自动合并触及(同一 winner 一轮只吃一条:第二条会使第一条的回
/// 执立即失效,顺延到下一轮重算后再合)。返回 (可自动合并, 留给人工)。
///
/// 拒绝名单匹配方向不敏感:两个未命名人之间谁是 loser/winner 由 total_ms 决定,
/// 会随后续入库时长变化而翻转(见 suggest_merges 的 loser/winner 择定),用户撤销
/// 时记下的 "P1>P2" 不能挡不住后来反向浮现的 "P2>P1" 建议——同一对人一旦被撤销
/// 过一次,两个方向都不应再被自动合并。
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
        let winner_named = vp.people.get(&s.winner).map_or(false, |p| !p.name.is_empty());
        let denied = deny.iter().any(|d| {
            d == &format!("{}>{}", s.loser, s.winner) || d == &format!("{}>{}", s.winner, s.loser)
        });
        if strong && unnamed && winner_named && !denied && !touched.contains(&s.loser) && !touched.contains(&s.winner)
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
    /// 测试库的默认模型标签(Voiceprints::default 的 embedding_model)。
    /// 向量空间门禁按它比对,测试里恒相符。
    const MODEL: &str = "campplus";
    /// 另一个模型空间的标签。门禁用例专用。
    const OTHER_MODEL: &str = "eres2netv2";

    /// 2026-08-19 设计轮 P1:换模型重建会把**全部**合并日志标记失效,而「拆回人物」
    /// **恰恰只接受失效条目**——于是重建之后任何一条老日志都能把旧空间的质心原样
    /// 插回新库,库标签还是新的。这条用例钉住:身份回来、声纹不回来。
    #[test]
    fn 跨空间拆回只还身份不还声纹() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![
            snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
        ];
        let links = store
            .upsert_from_session(&snaps, "2026-08-19T00:00:00+08:00", MODEL)
            .unwrap();
        let loser = links.get("S1").unwrap().clone();
        let winner = links.get("S2").unwrap().clone();
        store.rename(&loser, "张三").unwrap();
        let jid = store
            .merge_journaled(&loser, &winner, None, "manual", None, "2026-08-19T00:01:00+08:00", MODEL)
            .unwrap();

        // 换模型重建:库标签变成另一个空间,并把全部日志标记失效
        let mut emb = FlipEmbedder;
        let _ = store.rebuild_for_model(OTHER_MODEL, &mut emb);
        assert_eq!(store.load().embedding_model, OTHER_MODEL);

        let mut needs_rebuild = false;
        let restored_id = store.restore_merged_person(&jid, &mut needs_rebuild).unwrap();
        assert_eq!(restored_id, loser);
        assert!(needs_rebuild, "跨空间回放必须报「需要重建」");

        let vp = store.load();
        let p = vp.people.get(&loser).expect("身份必须回来");
        assert_eq!(p.name, "张三", "名字属于身份,必须还原");
        assert!(p.centroids.is_empty(), "旧空间的主质心必须清空");
        assert!(p.session_centroids.is_empty(), "会话质心同样要清——种子注入也消费它");
        assert!(
            seed_clusters(&vp).iter().all(|c| c.person != loser),
            "质心清空之后不得再为此人产出种子"
        );
    }

    /// 同空间回放照常整份还原,不能被上一条误伤。
    #[test]
    fn 同空间拆回照常还原声纹() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![
            snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
        ];
        let links = store
            .upsert_from_session(&snaps, "2026-08-19T00:00:00+08:00", MODEL)
            .unwrap();
        let loser = links.get("S1").unwrap().clone();
        let winner = links.get("S2").unwrap().clone();
        let jid = store
            .merge_journaled(&loser, &winner, None, "manual", None, "2026-08-19T00:01:00+08:00", MODEL)
            .unwrap();
        // 不重建,直接让条目失效(模拟"又被并了一次")
        store.merge_journaled(&winner, &winner, None, "manual", None, "t", MODEL).ok();
        let j = super::super::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        j.invalidate(&[&loser], "测试置失效", None);

        let mut needs_rebuild = false;
        let _id = store.restore_merged_person(&jid, &mut needs_rebuild).unwrap();
        assert!(!needs_rebuild, "同空间不该报需要重建");
        let vp = store.load();
        assert!(!vp.people[&loser].centroids.is_empty(), "同空间的质心必须原样还回来");
    }

    /// 2026-08-19:切模型重建成功之后,一个更早启动、用旧模型算完的写入落盘,
    /// 库标签是新的、内容是混的,而启动自愈只比标签不比内容,永远发现不了。
    /// 门禁就是为这条路存在的——它必须挡在**写入这一侧**,不能靠调用方自觉。
    #[test]
    fn 旧模型空间的向量写不进新库() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        // 先用当前空间入一个人,拿到 pid
        let snaps = vec![snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS)];
        let links = store
            .upsert_from_session(&snaps, "2026-08-19T00:00:00+08:00", MODEL)
            .unwrap();
        let pid = links.get("S1").unwrap().clone();
        let before = store.load();

        // 换个空间来写:三条向量写入路径都必须原地不动
        let other = vec![snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS)];
        let links2 = store
            .upsert_from_session(&other, "2026-08-19T00:01:00+08:00", OTHER_MODEL)
            .unwrap();
        assert!(links2.is_empty(), "异空间入库必须什么都不返回");

        let applied = store
            .reinforce_feedback(
                &pid,
                &[("mic".into(), vec![0.0, 1.0], 2, 4_000)],
                "2026-08-19T00:02:00+08:00",
                OTHER_MODEL,
            )
            .unwrap();
        assert!(applied.is_none(), "异空间回灌必须丢弃");

        let after = store.load();
        assert_eq!(
            serde_json::to_string(&before).unwrap(),
            serde_json::to_string(&after).unwrap(),
            "异空间写入之后库必须一字未动"
        );
    }

    /// 合并会把 loser 的质心并进 winner,两边必须同空间;异空间应当**报错**而不是
    /// 静默丢弃——合并是用户主动动作,失败要让他知道(与回灌的降级语义刻意不同)。
    #[test]
    fn 异空间的合并被拒绝() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![
            snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
        ];
        let links = store
            .upsert_from_session(&snaps, "2026-08-19T00:00:00+08:00", MODEL)
            .unwrap();
        let a = links.get("S1").unwrap().clone();
        let b = links.get("S2").unwrap().clone();
        assert!(store.merge_with_embedder(&a, &b, None, OTHER_MODEL).is_err());
        assert!(store
            .merge_journaled(&a, &b, None, "test", None, "2026-08-19T00:03:00+08:00", OTHER_MODEL)
            .is_err());
        // 同空间照常放行
        assert!(store.merge_with_embedder(&a, &b, None, MODEL).is_ok());
    }

    /// 原始录音与模型无关,而且正是重建赖以重算质心的素材——按旧标签拒绝它
    /// 只会丢掉有价值的原声。它**不该**受门禁。
    #[test]
    fn 原始录音样本不受空间门禁() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS)];
        let pid = store
            .upsert_from_session(&snaps, "2026-08-19T00:00:00+08:00", MODEL)
            .unwrap()
            .get("S1")
            .unwrap()
            .clone();
        // append_sample 没有 model 参数,签名本身就是"不受门禁"的证据
        assert!(store.append_sample(&pid, &[0.1f32; 16_000]).unwrap());
        assert!(!store.sample_paths_existing(&pid).is_empty(), "样本必须真写进去了");
    }

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
        let links = store.upsert_from_session(&snaps, "2026-07-05T10:00:00+08:00", MODEL).unwrap();
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
        store.upsert_from_session(&snaps1, "t1", MODEL).unwrap();
        assert!(!bak_path.exists(), "首次创建不产生 .bak(没有旧内容可备份)");
        let content_after_first = std::fs::read_to_string(tmp.path().join("voiceprints.json")).unwrap();

        // 第二次写入:文件已存在,覆盖前应先备份"覆盖前"的内容。
        let snaps2 = vec![snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS)];
        store.upsert_from_session(&snaps2, "t2", MODEL).unwrap();
        assert!(bak_path.exists());
        let bak_first = std::fs::read_to_string(&bak_path).unwrap();
        assert_eq!(bak_first, content_after_first, ".bak 保存的是覆盖前的内容");

        // 第三次写入:.bak 已存在,不应再被滚动覆盖(保留最早一次的备份起点)。
        let snaps3 = vec![snap("S3", vec![1.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS)];
        store.upsert_from_session(&snaps3, "t3", MODEL).unwrap();
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
        let links = store.upsert_from_session(&snaps, "t1", MODEL).unwrap();
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
        let links = store.upsert_from_session(&snaps, "t1", MODEL).unwrap();
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
        let links = store.upsert_from_session(&snaps, "t1", MODEL).unwrap();
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
        let links = store.upsert_from_session(&snaps, "t1", MODEL).unwrap();
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
        let links = store.upsert_from_session(&snaps, "t1", MODEL).unwrap();
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
        let links = store.upsert_from_session(&snaps, "t1", MODEL).unwrap();
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
        let links = store.upsert_from_session(&seed, "t1", MODEL).unwrap();
        let pid = links["S1"].clone();

        // 第二场:该簇已带 person=Some(续录/种子命中),回写加权质心 + 累加 total_ms
        let second = vec![snap("S9", vec![0.0, 1.0], 10, &["mic"], Some(&pid), 3000)];
        let links2 = store.upsert_from_session(&second, "t2", MODEL).unwrap();
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
        let links = store.upsert_from_session(&snaps, "t1", MODEL).unwrap();
        assert_eq!(links.len(), 1);
        let pid = &links["S1"];
        let vp = store.load();
        assert_eq!(vp.people[pid].name, "", "新建人物未命名");
        assert_eq!(vp.people[pid].total_ms, AUTO_ENROLL_MS);
        assert_eq!(vp.next_person, 2);
    }

    #[test]
    fn auto_enroll_threshold_is_ten_seconds() {
        assert_eq!(AUTO_ENROLL_MS, 10_000);
    }

    #[test]
    fn upsert_ignores_cluster_below_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS - 1)];
        let links = store.upsert_from_session(&snaps, "t1", MODEL).unwrap();
        assert!(links.is_empty());
        assert!(store.load().people.is_empty());
    }

    #[test]
    fn upsert_ignores_cluster_with_empty_centroid_even_over_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![snap("S1", vec![], 0, &["mic"], None, AUTO_ENROLL_MS)];
        let links = store.upsert_from_session(&snaps, "t1", MODEL).unwrap();
        assert!(links.is_empty(), "空质心不入库,即使 total_ms 够格");
        assert!(store.load().people.is_empty());
    }

    #[test]
    fn upsert_dangling_person_reference_is_skipped_not_recreated() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        // person 指向一个从未存在过的 id:resolve 应返回 None,upsert 应跳过而非报错/新建
        let snaps = vec![snap("S1", vec![1.0, 0.0], 5, &["mic"], Some("P999"), AUTO_ENROLL_MS)];
        let links = store.upsert_from_session(&snaps, "t1", MODEL).unwrap();
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
        let links = store.upsert_from_session(&snaps, "t1", MODEL).unwrap();
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
            .upsert_from_session(&[snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS)], "t0", MODEL)
            .unwrap();
        let pid = links["S1"].clone();
        assert_eq!(store.load().people[&pid].session_centroids["mic"].len(), 1, "入库场即第一份变体");

        // 净增量 <10s 的场不记变体;≥10s 的记,环形上限 5(挤最旧)。
        store.upsert_from_session(&[snap("Sx", vec![0.9, 0.1], 2, &["mic"], Some(&pid), 5_000)], "t-short", MODEL).unwrap();
        assert_eq!(store.load().people[&pid].session_centroids["mic"].len(), 1, "短场不记");
        for i in 0..6 {
            store
                .upsert_from_session(&[snap("Sx", vec![1.0, i as f32 * 0.1], 3, &["mic"], Some(&pid), 12_000)], &format!("t{}", i + 1), MODEL)
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
            MODEL,
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
                emails: Vec::new(),
            },
        );
        vp.redirects.insert("P2".into(), "P1".into()); // 悬空/重定向不产种子
        let seeds = seed_clusters(&vp);
        assert_eq!(seeds.len(), 3, "主质心 1 + 变体 2");
        assert!(seeds.iter().all(|s| s.person == "P1" && s.name == "张三"));
    }

    /// 跨信道种子只走归一化通道(Task 2)的前提:种子簇要知道自己质心来自哪个
    /// 信道。P1 有 mic 主质心 + system 会话变体 → 两个种子各带各的信道来源。
    #[test]
    fn seed_clusters_carry_channel_source() {
        let mut vp = Voiceprints::default();
        let pc = |x: f32, y: f32| PersonCentroid { vec: vec![x, y], count: 5, seen: "t".into() };
        vp.people.insert(
            "P1".into(),
            Person {
                name: "张三".into(),
                centroids: BTreeMap::from([("mic".to_string(), pc(1.0, 0.0))]),
                session_centroids: BTreeMap::from([("system".to_string(), vec![pc(0.0, 1.0)])]),
                total_ms: 60_000,
                last_seen: "t".into(),
                emails: Vec::new(),
            },
        );
        let seeds = seed_clusters(&vp);
        assert!(seeds.iter().any(|s| s.person == "P1" && s.source == "mic"));
        assert!(seeds.iter().any(|s| s.person == "P1" && s.source == "system"));
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
                emails: Vec::new(),
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
    fn rebuild_for_model_reembeds_from_samples_and_tags_library() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let links = store
            .upsert_from_session(
                &[
                    snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS),
                    snap("S2", vec![0.0, 1.0], 5, &["system"], None, AUTO_ENROLL_MS),
                ],
                "t1",
            MODEL,
            )
            .unwrap();
        let (pa, pb) = (links["S1"].clone(), links["S2"].clone());
        assert_eq!(store.load().embedding_model, "campplus", "旧库缺省标签");
        // pa 有一份方波样本;pb 无样本。
        let square: Vec<f32> = (0..16_000).map(|i| if i % 2 == 0 { 0.5 } else { -0.5 }).collect();
        store.append_sample(&pa, &square).unwrap();
        // pb 的样本文件删掉(upsert 不写样本,本就无)。
        let mut e = FlipEmbedder;
        let n = store.rebuild_for_model("eres2netv2", &mut e).unwrap();
        assert_eq!(n, 1, "只有 pa 有样本可重建");
        let vp = store.load();
        assert_eq!(vp.embedding_model, "eres2netv2");
        let a = &vp.people[&pa];
        // 方波经 FlipEmbedder → [0,1];主质心与变体都应是新空间向量。
        assert!((normalize(&a.centroids["mic"].vec).unwrap()[1] - 1.0).abs() < 1e-4);
        assert_eq!(a.session_centroids["mic"].len(), 1, "变体=各样本嵌入");
        let b = &vp.people[&pb];
        assert!(b.centroids.is_empty(), "无样本者清空质心(身份保留,等重新积累)");
        assert!(b.session_centroids.is_empty());
    }

    /// T3:切换声纹模型前的边界显性化——2 人有样本、1 人无样本,
    /// count_people_without_samples 要能把无样本者单独数出来,且 rebuild_for_model
    /// 跑完后该人 people 记录(名字)仍完整保留,只是质心被清空(新空间无从匹配)。
    #[test]
    fn count_people_without_samples_distinguishes_and_preserves_people_record() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let links = store
            .upsert_from_session(
                &[
                    snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS),
                    snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
                    snap("S3", vec![1.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
                ],
                "t1",
            MODEL,
            )
            .unwrap();
        let (pa, pb, pc) = (links["S1"].clone(), links["S2"].clone(), links["S3"].clone());
        let square: Vec<f32> = (0..16_000).map(|i| if i % 2 == 0 { 0.5 } else { -0.5 }).collect();
        // pa、pb 各有一份样本;pc 无样本。
        store.append_sample(&pa, &square).unwrap();
        store.append_sample(&pb, &square).unwrap();
        store.rename(&pc, "无样本的人").unwrap();
        assert_eq!(store.count_people_without_samples(), 1, "仅 pc 无样本");

        let mut e = FlipEmbedder;
        let n = store.rebuild_for_model("eres2netv2", &mut e).unwrap();
        assert_eq!(n, 2, "pa、pb 有样本可重建");
        assert_eq!(store.count_people_without_samples(), 1, "重建后无样本人数不变");

        let vp = store.load();
        let c = &vp.people[&pc];
        assert_eq!(c.name, "无样本的人", "无样本者 people 记录(名字)完整保留");
        assert!(c.centroids.is_empty(), "无样本者质心已清空(新空间无从匹配)");
    }

    #[test]
    fn delete_sample_removes_named_slot_rejects_foreign_and_slot_gets_refilled() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let links = store
            .upsert_from_session(&[snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS)], "t1", MODEL)
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
            .upsert_from_session(&[snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS)], "t1", MODEL)
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
        let links = store.upsert_from_session(&snaps, "t1", MODEL).unwrap();
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
        let links = store.upsert_from_session(&snaps, "t1", MODEL).unwrap();
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
            .merge_with_embedder(&loser, &winner, Some(&mut e as &mut dyn crate::diar::SpeakerEmbedder), MODEL)
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
                store_cb.upsert_from_session(std::slice::from_ref(snap), "t-live", MODEL).ok()
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
        store.upsert_from_session(&snaps, "t-stop", MODEL).unwrap();
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
        let links = store.upsert_from_session(&seed_snap, "t0", MODEL).unwrap();
        let pid = links["S0"].clone();
        assert_eq!(store.load().people[&pid].centroids["mic"].count, 40);

        // 本场:该人作为种子注入(count=40),命中两段长音频。
        let seeds =
            vec![SeedCluster { person: pid.clone(), name: String::new(), centroid: vec![1.0, 0.0, 0.0], count: 40, source: "mic".into() }];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        r.assign(&[1.0, 0.0, 0.0], "mic", 32000).unwrap();
        r.assign(&[1.0, 0.0, 0.0], "mic", 32000).unwrap();
        let session_snaps = r.snapshot();
        assert_eq!(session_snaps.len(), 1);
        assert_eq!(session_snaps[0].count, 2, "registry 导出应只是本场净增量,不含种子基数 40");
        assert_eq!(session_snaps[0].person.as_deref(), Some(pid.as_str()));

        // upsert 回库:应是 40+2=42,不该翻倍成 40+42=82。
        store.upsert_from_session(&session_snaps, "t1", MODEL).unwrap();
        let vp = store.load();
        assert_eq!(
            vp.people[&pid].centroids["mic"].count, 42,
            "库 count 应线性增长,不因种子基数被重复计入而复利膨胀"
        );
    }

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
        store.upsert_from_session(&snaps, "2026-07-31T10:00:00+08:00", MODEL).unwrap();
        store.rename("P2", "张三").unwrap();
        write_fake_sample(tmp.path(), "P1.wav");
        write_fake_sample(tmp.path(), "P2.wav");
        store
    }

    /// 造好合并日志条目后执行某操作,断言条目失效与否。
    fn journaled_entry(store: &VoiceprintStore, tmp: &tempfile::TempDir) -> String {
        let jid = store.merge_journaled("P1", "P2", None, "auto", None, "t1", MODEL).unwrap();
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
        store.upsert_from_session(&snaps, "t2", MODEL).unwrap();
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

    #[test]
    fn merge_journaled_merges_and_snapshots() {
        let tmp = tempfile::tempdir().unwrap();
        let store = two_people_store(&tmp);
        let jid = store
            .merge_journaled("P1", "P2", None, "auto", Some(0.9), "2026-07-31T11:00:00+08:00", MODEL)
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
        let jid = store.merge_journaled("P1", "P2", None, "manual", None, "t", MODEL).unwrap();
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
        store.upsert_from_session(&snaps, "t0", MODEL).unwrap(); // P1 P2 P3,均未命名
        let j1 = store.merge_journaled("P1", "P2", None, "auto", None, "t1", MODEL).unwrap();
        let j2 = store.merge_journaled("P3", "P2", None, "auto", None, "t2", MODEL).unwrap();

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

    #[test]
    fn confident_picks_denylist_blocks_reversed_direction() {
        // 撤销记的是 "P4>P5"(P4 曾是 loser);total_ms 变化后 suggest_merges 反向
        // 推出 P5→P4(P5 是 loser),同一对人不该因方向翻转而绕过拒绝名单。
        let mut vp = Voiceprints::default();
        vp.people.insert("P4".into(), Person { name: "".into(), ..Default::default() });
        vp.people.insert("P5".into(), Person { name: "".into(), ..Default::default() });
        let sugs = vec![MergeSuggestion {
            loser: "P5".into(),
            winner: "P4".into(),
            similarity: 0.80,
            source: "mic".into(),
            salience: None,
        }];
        let deny = vec!["P4>P5".to_string()];
        let (autos, manual) = confident_picks(&vp, sugs, &deny);
        assert!(autos.is_empty(), "反向建议仍应被拒绝名单挡住,落入人工");
        assert_eq!(manual.len(), 1);
    }

    #[test]
    fn confident_picks_requires_named_winner() {
        let mut vp = Voiceprints::default();
        vp.people.insert("P1".into(), Person::default()); // 未命名
        vp.people.insert("P2".into(), Person::default()); // 未命名
        let sugs = vec![MergeSuggestion {
            loser: "P1".into(),
            winner: "P2".into(),
            similarity: 0.95,
            source: "mic".into(),
            salience: Some(9.0),
        }];
        let (autos, manual) = confident_picks(&vp, sugs, &[]);
        assert!(autos.is_empty(), "未命名互并再像也不自动,留给人工");
        assert_eq!(manual.len(), 1);
    }

    #[test]
    fn undo_merge_restores_records_redirects_samples_and_denylists() {
        let tmp = tempfile::tempdir().unwrap();
        let store = two_people_store(&tmp);
        let before = store.load();
        let jid = store.merge_journaled("P1", "P2", None, "auto", Some(0.9), "t1", MODEL).unwrap();

        store.undo_merge(&jid, &mut false).unwrap();

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
        store.upsert_from_session(&snaps, "t0", MODEL).unwrap(); // P1 P2 P3
        store.merge("P3", "P1").unwrap(); // P3 -> P1(历史合并,无日志)
        let jid = store.merge_journaled("P1", "P2", None, "auto", None, "t1", MODEL).unwrap();
        assert_eq!(store.load().redirects.get("P3").map(String::as_str), Some("P2"), "压扁改指 P2");

        store.undo_merge(&jid, &mut false).unwrap();
        let vp = store.load();
        assert_eq!(vp.redirects.get("P3").map(String::as_str), Some("P1"), "压扁还原回 P1");
        assert!(!vp.redirects.contains_key("P1"));
    }

    #[test]
    fn undo_merge_rejects_invalidated_entry_with_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let store = two_people_store(&tmp);
        let jid = store.merge_journaled("P1", "P2", None, "auto", None, "t1", MODEL).unwrap();
        let j = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        j.invalidate(&["P2"], "此人随后被改名", None);

        let err = store.undo_merge(&jid, &mut false).unwrap_err().to_string();
        assert!(err.contains("不能撤销"), "拒绝并带原因: {err}");
        assert!(err.contains("此人随后被改名"));
    }

    /// acknowledge_merge 是 vp_guard 内的薄包装,行为上应与直接调用
    /// MergeJournal::acknowledge 等价(条目连样本副本一并消失)。
    #[test]
    fn acknowledge_merge_removes_entry_same_as_direct_journal_acknowledge() {
        let tmp = tempfile::tempdir().unwrap();
        let store = two_people_store(&tmp);
        let jid = store.merge_journaled("P1", "P2", None, "auto", Some(0.9), "t1", MODEL).unwrap();

        store.acknowledge_merge(&jid).unwrap();

        let journal = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        assert!(journal.entries().is_empty(), "确认后条目消失");
        assert!(journal.entry(&jid).is_err(), "条目目录已删除,与直接调 journal.acknowledge 等价");
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
        store.upsert_from_session(&snaps, "t0", MODEL).unwrap();
        let before = store.load();
        let j1 = store.merge_journaled("P1", "P2", None, "auto", None, "t1", MODEL).unwrap();
        let j2 = store.merge_journaled("P3", "P2", None, "auto", None, "t2", MODEL).unwrap();

        store.undo_merge(&j1, &mut false).unwrap_err(); // j1 已被 j2 失效
        store.undo_merge(&j2, &mut false).unwrap(); // 后进先出:先撤 j2
        let j = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        assert!(j.entry(&j1).unwrap().invalid_reason.is_none(), "j2 撤销后 j1 复活");
        store.undo_merge(&j1, &mut false).unwrap(); // 再撤 j1,库回到最初
        assert_eq!(store.load().people, before.people);
    }

    // ── restore_merged_person(拆回原身份)── 只对失效条目生效,不动 winner,不 revive 链 ──

    /// P1(未命名)并入 P2(张三)落条目,再让条目失效(模拟"P2 随后又被合并"),
    /// 造出可拆回的场景。返回 (store, journal_id, tmp)——tmp 需随用例存活。
    fn setup_invalidated_merge() -> (VoiceprintStore, String, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let store = two_people_store(&tmp);
        let jid = store.merge_journaled("P1", "P2", None, "auto", None, "t1", MODEL).unwrap();
        let j = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        j.invalidate(&["P2"], "相关人物随后又被合并", None);
        (store, jid, tmp)
    }

    /// 同上但条目未失效,仍可直接撤销。
    fn setup_valid_merge() -> (VoiceprintStore, String, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let store = two_people_store(&tmp);
        let jid = store.merge_journaled("P1", "P2", None, "auto", None, "t1", MODEL).unwrap();
        (store, jid, tmp)
    }

    /// 链式合并:P1→P2 落条目 A,再 P2→P3 落条目 B——B 一落地就把触及 P2 的 A
    /// 连带失效(invalidated_by=B)。额外让 B 自身也失效(模拟 P3 随后又变动),
    /// 使 B 具备"可拆回"的前提(仅失效条目可拆)。
    fn setup_chained_merges() -> (VoiceprintStore, String, String, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![
            snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S3", vec![0.7, 0.7], 5, &["mic"], None, AUTO_ENROLL_MS),
        ];
        store.upsert_from_session(&snaps, "t0", MODEL).unwrap(); // P1 P2 P3
        let entry_a = store.merge_journaled("P1", "P2", None, "auto", None, "t1", MODEL).unwrap();
        let entry_b = store.merge_journaled("P2", "P3", None, "auto", None, "t2", MODEL).unwrap();
        let j = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        j.invalidate(&["P3"], "相关人物随后又发生变化", None);
        (store, entry_a, entry_b, tmp)
    }

    #[test]
    fn restore_merged_person_rebuilds_loser_without_touching_winner() {
        let (store, journal_id, tmp) = setup_invalidated_merge();
        let winner_before = store.load().people.get("P2").cloned();

        let mut nr = false;
        let pid = store.restore_merged_person(&journal_id, &mut nr).unwrap();

        assert_eq!(pid, "P1");
        assert!(!nr, "同空间快照不需要重建");
        let vp = store.load();
        assert!(vp.people.contains_key("P1"), "loser 按快照重建");
        assert_eq!(vp.people.get("P2").cloned(), winner_before, "winner 不动");
        assert!(!vp.redirects.contains_key("P1"), "loser 不再被重定向");
        let journal = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        assert!(journal.entry(&journal_id).is_err(), "条目删除");
        assert!(journal.auto_denylist().iter().any(|p| p == "P1>P2"), "pair 进拒绝名单");
    }

    #[test]
    fn restore_merged_person_rejects_valid_entry() {
        let (store, journal_id, _tmp) = setup_valid_merge();
        let err = store.restore_merged_person(&journal_id, &mut false).unwrap_err().to_string();
        assert!(err.contains("撤销"), "有效条目应引导走撤销而非拆回: {err}");
    }

    #[test]
    fn restore_merged_person_does_not_revive_chain() {
        let (store, entry_a_id, entry_b_id, tmp) = setup_chained_merges();

        store.restore_merged_person(&entry_b_id, &mut false).unwrap();

        let journal = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        let a = journal.entry(&entry_a_id).unwrap();
        assert!(a.invalid_reason.is_some(), "链上条目不复活——B 那次合并并未被撤销");
    }

    #[test]
    fn undo_merge_retry_keeps_samples_recorded_between_attempts() {
        // 上一次撤销已把库还原、样本也清完了(阶段落盘 samples_cleared),但卡在拷回或
        // 删条目。重试**不能**再清一遍:两次尝试之间用户又录了新样本,清掉它再拿旧快照
        // 占位,新录音就永久没了(codex review 实现轮三 P1 / 实现轮四 P1)。
        let (store, jid, tmp) = setup_valid_merge();
        let journal = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        let entry = journal.entry(&jid).unwrap();
        // 手动把库还原成"上次撤销已完成前半程"的样子,并把阶段记到盘上。
        let mut vp = store.load();
        vp.people.insert(entry.loser.clone(), entry.loser_person.clone());
        vp.people.insert(entry.winner.clone(), entry.winner_person.clone());
        vp.redirects.remove(&entry.loser);
        store.save(&vp).unwrap();
        journal
            .set_undo_phase(&jid, crate::store::merge_journal::undo_phase::SAMPLES_CLEARED, false)
            .unwrap();
        // 两次尝试之间新录了一段。
        let vpdir = tmp.path().join("voiceprints");
        std::fs::create_dir_all(&vpdir).unwrap();
        let fresh = vpdir.join(format!("{}.wav", entry.loser));
        std::fs::write(&fresh, b"RIFFrecorded-between-attempts").unwrap();

        store.undo_merge(&jid, &mut false).unwrap();

        assert_eq!(
            std::fs::read(&fresh).unwrap(),
            b"RIFFrecorded-between-attempts",
            "重试不得删掉两次尝试之间新录的样本"
        );
        assert!(journal.entry(&jid).is_err(), "重试应收尾成功、清掉条目");
    }

    #[test]
    fn undo_merge_redoes_sample_cleanup_when_that_phase_never_completed() {
        // 反面:上次只做到"库已还原"就失败了,样本清理**没**记上。这时必须重做清理——
        // 留一个"清了一半"的中间态才是真正判不出来的那种坏。代价是这个窗口里新录的
        // 东西会被删掉,这是刻意取舍:撤销的正确性优先。
        let (store, jid, tmp) = setup_valid_merge();
        let journal = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        let entry = journal.entry(&jid).unwrap();
        let mut vp = store.load();
        vp.people.insert(entry.loser.clone(), entry.loser_person.clone());
        vp.people.insert(entry.winner.clone(), entry.winner_person.clone());
        vp.redirects.remove(&entry.loser);
        store.save(&vp).unwrap();
        journal
            .set_undo_phase(&jid, crate::store::merge_journal::undo_phase::LIBRARY_RESTORED, false)
            .unwrap();
        let vpdir = tmp.path().join("voiceprints");
        std::fs::create_dir_all(&vpdir).unwrap();
        let stale = vpdir.join(format!("{}.wav", entry.loser));
        std::fs::write(&stale, b"RIFFpost-merge-leftover").unwrap();

        store.undo_merge(&jid, &mut false).unwrap();

        assert_ne!(
            std::fs::read(&stale).unwrap_or_default(),
            b"RIFFpost-merge-leftover",
            "清理阶段没记上就必须重做,合并后的残留不能占住槽位"
        );
        assert!(journal.entry(&jid).is_err());
    }

    #[test]
    fn split_back_after_a_half_done_undo_also_restores_the_winner_samples() {
        // 死角:撤销做到 samples_cleared(双方现存样本都删了)之后失败,紧接着那两个人
        // 又被合并 → 条目被普通变更失效 → undo_merge 被 invalid_reason 拒、acknowledge
        // 被 undo_phase 拒,用户唯一能走的只剩"拆回"。而拆回平常只补 loser,收完尾就删
        // 条目——winner 那些已经被删掉的样本就永久没了(codex review 实现轮六 P1)。
        let tmp = tempfile::tempdir().unwrap();
        let store = two_people_store(&tmp);
        let jid = store.merge_journaled("P1", "P2", None, "auto", None, "t1", MODEL).unwrap();
        let journal = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        // 撤销做到"样本已清空"就停了。
        let vpdir = tmp.path().join("voiceprints");
        for f in ["P1.wav", "P2.wav"] {
            let _ = std::fs::remove_file(vpdir.join(f));
        }
        let mut vp = store.load();
        vp.people.insert("P1".into(), journal.entry(&jid).unwrap().loser_person);
        vp.people.insert("P2".into(), journal.entry(&jid).unwrap().winner_person);
        vp.redirects.remove("P1");
        store.save(&vp).unwrap();
        journal
            .set_undo_phase(&jid, crate::store::merge_journal::undo_phase::SAMPLES_CLEARED, false)
            .unwrap();
        // 然后普通人物变更把它失效了(第五轮明确要求普通变更照常失效)。
        journal.invalidate(&["P2"], "P2 随后又被合并", None);
        assert!(journal.entry(&jid).unwrap().invalid_reason.is_some());
        // 库里 P1 已经在了,走的是"已还原过"那条分支——它也必须补 winner。
        let mut nr = false;
        store.restore_merged_person(&jid, &mut nr).unwrap();

        assert!(vpdir.join("P1.wav").exists(), "loser 样本要回来");
        assert!(vpdir.join("P2.wav").exists(), "winner 被那次半截撤销删掉的样本也必须回来");
    }

    #[test]
    fn in_progress_undo_survives_a_library_rebuild_invalidation() {
        // 跨空间撤销失败后调用方会立刻排一次重建,而重建结尾 invalidate_all 会把所有条目
        // 标失效——被失效的话重试就被 invalid_reason 挡死,那次本该双边还原的撤销永远
        // 做不完(codex review 实现轮四 P1)。进行中的撤销必须豁免。
        let (store, jid, tmp) = setup_valid_merge();
        let journal = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        journal
            .set_undo_phase(&jid, crate::store::merge_journal::undo_phase::LIBRARY_RESTORED, true)
            .unwrap();

        journal.invalidate_all("声纹库已按新模型重建");

        let e = journal.entry(&jid).unwrap();
        assert!(e.invalid_reason.is_none(), "进行中的撤销不得被重建失效");
        assert!(e.undo_cleared_centroids, "跨空间清空过质心的事实要落盘");
        // 而且重试确实还能走 undo_merge(不被 invalid_reason 挡)。
        let mut nr = false;
        store.undo_merge(&jid, &mut nr).unwrap();
        assert!(nr, "落盘的 undo_cleared_centroids 应让重试仍报需要重建");
    }

    #[test]
    fn undo_merge_reports_rebuild_even_when_a_later_step_fails() {
        // 跨空间快照 → 质心被清空并落盘。之后样本恢复失败返回 Err 时,重建需求必须
        // 已经通过出参传出去了,否则这两个人永远没有声纹:库标签没变,启动自愈不触发
        // (codex review 实现轮三 P1)。
        let (store, jid, tmp) = setup_valid_merge();
        // 把库换到另一个空间,条目标签仍是 MODEL → sanitize_replayed 会清空质心。
        let mut vp = store.load();
        vp.embedding_model = "eres2netv2".into();
        store.save(&vp).unwrap();
        // 让样本恢复必然失败:把 loser 侧样本副本目录换成一个普通文件。
        let journal = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        let side = tmp.path().join("merge_journal").join(&jid).join("samples").join("loser");
        let _ = std::fs::remove_dir_all(&side);
        std::fs::create_dir_all(side.parent().unwrap()).unwrap();
        std::fs::write(&side, b"not a directory").unwrap();

        let mut needs_rebuild = false;
        let r = store.undo_merge(&jid, &mut needs_rebuild);

        assert!(r.is_err(), "样本恢复失败应整体失败");
        assert!(needs_rebuild, "质心已被清空并落盘,重建需求不得随 Err 丢失");
        assert!(journal.entry(&jid).is_ok(), "失败时条目必须保留供重试");
        let vp = store.load();
        for id in ["P1", "P2"] {
            let p = &vp.people[id];
            assert!(p.centroids.is_empty() && p.session_centroids.is_empty(), "{id} 两张质心表都该空");
        }
    }

    #[test]
    fn restore_merged_person_retry_after_partial_failure_is_idempotent() {
        // 模拟"上次拆回已把 loser 写回库,但在 journal.remove 前中断"的场景:
        // 手动把 loser 快照插回库(不经 restore_merged_person),条目仍留着。
        let (store, journal_id, tmp) = setup_invalidated_merge();
        let journal = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        let entry = journal.entry(&journal_id).unwrap();
        let mut vp = store.load();
        vp.people.insert(entry.loser.clone(), entry.loser_person.clone());
        store.save(&vp).unwrap();

        let mut nr = false;
        let pid = store.restore_merged_person(&journal_id, &mut nr).unwrap();

        assert_eq!(pid, "P1", "重试仍应返回 loser 编号");
        let vp = store.load();
        assert_eq!(
            vp.people.get("P1"),
            Some(&entry.loser_person),
            "loser 仍在库,重试不应二次覆盖破坏记录"
        );
        let journal = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        assert!(journal.entry(&journal_id).is_err(), "重试应成功清掉条目,不报'已存在'");
    }

    /// 链式合并 A=P1→P2(被 B 连带失效)、B=P2→P3(仍有效)。先拆回 A 重建 P1,
    /// 再撤销 B——B 的快照里 redirects_to_loser 含 "P1"(B 落地时 P1 还重定向到
    /// P2),回放不能无脑插回,否则 resolve("P1") 会被解析成 P2,刚拆回的人被
    /// 静默架空。
    #[test]
    fn restore_then_undo_chained_merge_does_not_reredirect_restored_person() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![
            snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S3", vec![0.7, 0.7], 5, &["mic"], None, AUTO_ENROLL_MS),
        ];
        store.upsert_from_session(&snaps, "t0", MODEL).unwrap(); // P1 P2 P3
        let entry_a = store.merge_journaled("P1", "P2", None, "auto", None, "t1", MODEL).unwrap();
        let entry_b = store.merge_journaled("P2", "P3", None, "auto", None, "t2", MODEL).unwrap();
        // entry_a 已被 entry_b 的创建连带失效(触及 P2);entry_b 本身仍有效。

        store.restore_merged_person(&entry_a, &mut false).unwrap();
        assert!(store.load().people.contains_key("P1"), "拆回后 P1 重建为独立说话人");

        store.undo_merge(&entry_b, &mut false).unwrap();

        let vp = store.load();
        assert!(!vp.redirects.contains_key("P1"), "拆回的人不能被快照回放重新遮蔽");
        assert!(vp.people.contains_key("P1"));
        assert!(vp.people.contains_key("P2"));
    }

    /// 同样的链式场景:拆回 A 后,winner P2 已经(通过 B)并入 P3——拒绝名单必须
    /// 同时记住原始 pair 与解析到当前化身的 pair,否则下一轮自动归并会立刻把
    /// 刚拆回的人并回 P3,拆回等于没拆。
    #[test]
    fn restore_merged_person_denylists_current_avatar_when_winner_already_merged_again() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let snaps = vec![
            snap("S1", vec![1.0, 0.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S2", vec![0.0, 1.0], 5, &["mic"], None, AUTO_ENROLL_MS),
            snap("S3", vec![0.7, 0.7], 5, &["mic"], None, AUTO_ENROLL_MS),
        ];
        store.upsert_from_session(&snaps, "t0", MODEL).unwrap(); // P1 P2 P3
        let entry_a = store.merge_journaled("P1", "P2", None, "auto", None, "t1", MODEL).unwrap();
        let _entry_b = store.merge_journaled("P2", "P3", None, "auto", None, "t2", MODEL).unwrap();

        store.restore_merged_person(&entry_a, &mut false).unwrap();

        let journal = crate::store::merge_journal::MergeJournal::new(tmp.path().to_path_buf());
        let deny = journal.auto_denylist();
        assert!(deny.iter().any(|p| p == "P1>P2"), "原始 pair 进名单: {deny:?}");
        assert!(deny.iter().any(|p| p == "P1>P3"), "解析后的当前化身 pair 也要进名单: {deny:?}");
    }
    #[test]
    fn reinforce_feedback_merges_and_snapshots() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let links = store
            .upsert_from_session(&[snap("S1", vec![1.0, 0.0, 0.0, 0.0], 4, &["mic"], None, AUTO_ENROLL_MS)], "t0", MODEL)
            .unwrap();
        let pid = links.get("S1").unwrap().clone();

        let applied = store
            .reinforce_feedback(
                &pid,
                &[("mic".into(), vec![0.0, 1.0, 0.0, 0.0], 2, 4_000)],
                "2026-08-08T01:00:00+08:00",
            MODEL,
            )
            .unwrap()
            .expect("同模型,门禁应放行");
        let vp = store.load();
        let p = vp.people.get(&pid).unwrap();
        assert_eq!(p.total_ms, AUTO_ENROLL_MS + 4_000);
        assert_eq!(p.last_seen, "2026-08-08T01:00:00+08:00");
        assert!(p.centroids["mic"].vec[1] > 0.0, "新方向必须并入质心");
        assert_ne!(applied.person_before, applied.person_after);
    }

    #[test]
    fn reinforce_feedback_rejects_unknown_person() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let err = store.reinforce_feedback("P999", &[("mic".into(), vec![1.0], 1, 2_000)], "t", MODEL);
        assert!(err.is_err(), "悬空人物必须显式报错,不得静默成功");
    }

    #[test]
    fn restore_feedback_only_when_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let links = store
            .upsert_from_session(&[snap("S1", vec![1.0, 0.0, 0.0, 0.0], 4, &["mic"], None, AUTO_ENROLL_MS)], "t0", MODEL)
            .unwrap();
        let pid = links.get("S1").unwrap().clone();
        let applied = store
            .reinforce_feedback(&pid, &[("mic".into(), vec![0.0, 1.0, 0.0, 0.0], 2, 4_000)], "t1", MODEL)
            .unwrap().expect("同模型,门禁应放行");

        // 场景一:未被动过 → 还原成功,total_ms 回到初值。
        assert_eq!(
            store
                .restore_feedback(&pid, &applied.person_before, &applied.person_after, MODEL)
                .unwrap(),
            RestoreOutcome::Restored
        );
        assert_eq!(store.load().people.get(&pid).unwrap().total_ms, AUTO_ENROLL_MS);

        // 场景二:重放回灌后又被别的写动过 → 拒绝还原。
        let applied2 = store
            .reinforce_feedback(&pid, &[("mic".into(), vec![0.0, 1.0, 0.0, 0.0], 2, 4_000)], "t2", MODEL)
            .unwrap().expect("同模型,门禁应放行");
        store
            .reinforce_feedback(&pid, &[("mic".into(), vec![0.0, 0.0, 1.0, 0.0], 1, 2_000)], "t3", MODEL)
            .unwrap();
        assert_eq!(
            store
                .restore_feedback(&pid, &applied2.person_before, &applied2.person_after, MODEL)
                .unwrap(),
            RestoreOutcome::Skipped
        );
    }
    #[test]
    fn create_person_and_delete_if_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        assert!(store.create_person("  ", "t").is_err(), "空名拒绝");
        let id1 = store.create_person("张伟", "t").unwrap();
        let id2 = store.create_person("张伟", "t").unwrap();
        assert_ne!(id1, id2, "重名允许(收件箱另有疑似重复提示),id 必须递增");
        assert!(store.delete_person_if_empty(&id2).unwrap(), "空档案可补偿删除");
        assert!(store.load().people.get(&id2).is_none());
        // 有质心的不删。
        store
            .reinforce_feedback(&id1, &[("mic".into(), vec![1.0, 0.0], 1, 2_000)], "t2", MODEL)
            .unwrap();
        assert!(!store.delete_person_if_empty(&id1).unwrap());
        assert!(store.load().people.contains_key(&id1));
    }
    #[test]
    fn person_emails_union_on_merge_dedup_and_block_empty_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let store = VoiceprintStore::new(tmp.path().to_path_buf());
        let a = store.create_person("张伟", "t").unwrap();
        let b = store.create_person("张老板", "t").unwrap();
        store.add_person_email(&a, " ZW@X.com ").unwrap();
        store.add_person_email(&a, "zw@x.com").unwrap(); // 规范化后去重
        store.add_person_email(&b, "boss@x.com").unwrap();
        assert_eq!(store.load().people[&a].emails, vec!["zw@x.com"]);
        // 有邮箱的空质心档案不可被补偿删除。
        assert!(!store.delete_person_if_empty(&a).unwrap());
        // 合并并集:b 并入 a 后邮箱不丢。
        store.merge_journaled(&b, &a, None, "manual", None, "t2", MODEL).unwrap();
        let emails = &store.load().people[&a].emails;
        assert!(emails.contains(&"zw@x.com".to_string()) && emails.contains(&"boss@x.com".to_string()));
    }

}