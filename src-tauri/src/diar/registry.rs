//! 在线增量声纹聚类:两路(mic/system)嵌入汇入同一 Registry,
//! 得全局「S1..Sn」。无模型依赖、单线程持有于 ASR worker。
//! 唯一的外部副作用点是可选的 enroller 回调(会话中实时入库拿全局 person id),
//! 由 lib.rs 注入、enroll_pending 触发;不注入时保持纯逻辑。

use std::collections::BTreeSet;

/// 归簇阈值(余弦)。首轮真实会议校准(10+人短句场景)调整:原 0.55 下不同人被吸入同簇。
pub const ASSIGN_THRESHOLD: f32 = 0.62;
/// 簇间合并阈值(余弦,高于归簇阈值防过度合并)。首轮真实会议校准(10+人短句场景)调整:原 0.68 下触发过度合并。
pub const MERGE_THRESHOLD: f32 = 0.74;
/// 低于此样本数(16kHz)的段不允许新建簇(短段声纹不可靠)。二轮校准(2026-07-08,
/// 用户实锤三场会议在线 17/54/108 簇、单段簇占比 65%+,Aing 后收敛 2/8 人):
/// 0.6s 太松,短段嵌入落在灰区就开新簇是单段簇的主因;提到 2.5s 与 Aing 管线的
/// 短段判定(refine SHORT_MS)对齐——不足 2.5s 的段走软归属或留空,绝不开簇。
/// (历史:1.0s 首轮校准降到 0.6s 是因为拦了 0.9s 真句子——那个问题由软归属
/// 兜住:真句子仍拿到最近簇的标签,只是无权开簇。)
pub const MIN_NEW_CLUSTER_SAMPLES: usize = 40_000; // 2.5s
/// 低于此样本数(16kHz)的段不参与质心更新(短段声纹噪声大,防拖歪质心)。
/// 维持首轮校准的 0.6s:质心更新的容错比建簇高(running mean 稀释单段噪声)。
pub const MIN_CENTROID_UPDATE_SAMPLES: usize = 9600; // 0.6s
/// 软归属下限(余弦):相似度落在 [SOFT_ASSIGN_THRESHOLD, ASSIGN_THRESHOLD) 灰区的段,
/// 归入最近的普通簇(打标签但不更新质心/计数,防弱证据污染),而非开新簇——近场 mic
/// 同人嵌入散布 0.4~0.67(2026-07-06 校准记录),灰区裂簇是同人多簇的主因。
/// 种子簇不参与软归属:弱证据错认領人名比碎片化更糟。
pub const SOFT_ASSIGN_THRESHOLD: f32 = 0.45;
/// 每 N 次 assign 做一次簇间合并检查。
pub const MERGE_CHECK_INTERVAL: u64 = 8;
/// 种子簇(已关联库人物)的归簇阈值,高于普通阈值。跨会议信道差异比同会议内大,
/// 命错人比不命名更糟,故要求更高相似度才认领。待真实会议数据校准。
pub const SEED_ASSIGN_THRESHOLD: f32 = 0.68;

#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerInfo {
    pub id: String,
    pub sources: BTreeSet<String>,
    /// 关联的库人物 id(种子命中或续录带入);None = 尚未关联任何库人物。
    pub person: Option<String>,
    /// 库人物姓名(随 person 一起出现;快照恢复路径不带,需上层从库/笔记表补)。
    pub name: Option<String>,
}

/// 一簇的可导出快照(质心/计数/来源),用于跨会话续接说话人编号(P4.5)。
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterSnapshot {
    pub id: String,
    pub centroid: Vec<f32>,
    pub count: u64,
    pub sources: BTreeSet<String>,
    /// 关联的库人物 id(续录场景:上一场已关联,这一场铺底恢复)。
    pub person: Option<String>,
    /// 本簇累计的长段时长(毫秒),供停止时的入库门槛判定与库累计使用。
    pub total_ms: u64,
}

/// 库里的一个种子人物:注入 registry 供本场优先命中,免得同一人在新会话里
/// 从零建簇、需要用户重新点名。
pub struct SeedCluster {
    pub person: String,
    pub name: String,
    pub centroid: Vec<f32>,
    pub count: u64,
}

struct Cluster {
    id: String,
    /// 成员单位向量的均值,再归一化。
    centroid: Vec<f32>,
    count: u64,
    sources: BTreeSet<String>,
    /// 关联的库人物 id;Some 表示该簇已"认领"为库中某人(种子命中或续录恢复)。
    person: Option<String>,
    /// 种子携带的库人物姓名,只在种子注入路径设置——不进快照,名字真值在库/笔记表,
    /// 避免快照成为姓名的第二份真源。
    person_name: Option<String>,
    /// 累计长段时长(毫秒,仅长段更新质心时累加),供入库门槛与库时长统计。
    total_ms: u64,
    /// count 里"非本场新增"的基数：种子簇 = 注入时携带的库 count;从快照恢复的簇 =
    /// 快照里的 count(代表此前场次已上报过的历史累计)；会话内新建的普通簇恒 0。
    /// snapshot() 导出 count 时减去这部分，只报告本场的净增量——否则种子/续录带来
    /// 的历史 count 会随每场停止 upsert 与库里的 existing count 相加，几何级数膨胀
    /// (见终审 triage②)。合并时两侧基数相加，增量语义在合并后仍然成立。
    /// 会话中实时入库(mark_enrolled)同样把已上报的 count 记入此基数。
    seed_base_count: u64,
    /// total_ms 里"已上报给库"的基数(会话中实时入库时记账)。snapshot() 导出
    /// total_ms 时减去它，停止时的 upsert 才不会把入库时已累加过的时长再加一遍。
    /// 合并时两侧相加，与 seed_base_count 同一套增量语义。
    reported_ms: u64,
    /// 本场实时入库标记。区分「跨会话种子/续录恢复的关联」与「本场新识别出的人」：
    /// 前者质心来自别的信道/场次，归簇与合并都要用更严的种子阈值防误认；后者质心
    /// 就是本场刚聚出来的，拿到全局 id 不该反而让归簇变严(否则同一人会话内碎片化)。
    session_enrolled: bool,
}

impl Cluster {
    /// 是否按"跨会话种子"对待(套严格阈值)：关联库人物且非本场实时入库。
    fn is_seed(&self) -> bool {
        self.person.is_some() && !self.session_enrolled
    }
}

/// 会话中实时入库回调：入参为够料的无主簇快照,返回库分配的全局 person id
/// (None = 库不可用/入库失败,该簇留待下条 final 重试)。
pub type EnrollFn = Box<dyn FnMut(&ClusterSnapshot) -> Option<String> + Send>;

pub struct SpeakerRegistry {
    clusters: Vec<Cluster>,
    next_id: u32,
    assigns: u64,
    pending_merges: Vec<(String, String)>,
    /// (够料门槛 ms, 入库回调)。None = 不做实时入库(测试/库路径不可用)。
    enroller: Option<(u64, EnrollFn)>,
}

fn normalize(v: &[f32]) -> Option<Vec<f32>> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !norm.is_finite() || norm < 1e-6 {
        return None;
    }
    Some(v.iter().map(|x| x / norm).collect())
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

impl Default for SpeakerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SpeakerRegistry {
    pub fn new() -> Self {
        Self { clusters: Vec::new(), next_id: 1, assigns: 0, pending_merges: Vec::new(), enroller: None }
    }

    /// 装配会话中实时入库回调(lib.rs 在 with_seeds 之后调用)。
    pub fn set_enroller(&mut self, min_ms: u64, f: EnrollFn) {
        self.enroller = Some((min_ms, f));
    }

    /// 跑一轮实时入库：把够料(≥ min_ms)的无主簇经回调入库并 mark_enrolled。
    /// ASR worker 每条 final 定稿后调用;无回调/无候选时零开销。回调返回 None
    /// (库降级)不做任何标记,下轮自然重试。
    pub fn enroll_pending(&mut self) {
        let Some((min_ms, mut f)) = self.enroller.take() else { return };
        for cand in self.enroll_candidates(min_ms) {
            if let Some(pid) = f(&cand) {
                self.mark_enrolled(&cand.id, &pid);
            }
        }
        self.enroller = Some((min_ms, f));
    }

    /// 归簇:与各质心比余弦,≥ 阈值归入最相似簇;
    /// 长段(≥ MIN_NEW_CLUSTER_SAMPLES)更新质心、增计数; 短段仅记录来源、不拖质心(防噪声污染)。
    /// 不相似且段够长才新建簇。返回说话人 id;不可用嵌入/短段无归属返回 None。
    pub fn assign(&mut self, embedding: &[f32], source: &str, num_samples: usize) -> Option<String> {
        let unit = normalize(embedding)?;
        if let Some(c) = self.clusters.first() {
            if c.centroid.len() != unit.len() {
                return None; // 维度不符(模型换了?)丢弃
            }
        }
        self.assigns += 1;
        if self.assigns % MERGE_CHECK_INTERVAL == 0 {
            self.detect_merges();
        }

        let best = self
            .clusters
            .iter_mut()
            .filter_map(|c| {
                let sim = dot(&c.centroid, &unit);
                // 种子簇(关联库 person 且非本场实时入库)用更高阈值:跨会议信道差异大,
                // 误命名比不命名糟(待校准)。本场实时入库的簇质心是本场新鲜聚出的,
                // 维持普通阈值——拿到全局 id 不该让归簇变严。
                // 阈值在候选过滤阶段生效——若全局最相似是"够不着的种子簇",不得挡住
                // 本可命中的普通簇(否则会话内簇碎片化)。
                let threshold = if c.is_seed() { SEED_ASSIGN_THRESHOLD } else { ASSIGN_THRESHOLD };
                (sim >= threshold).then_some((sim, c))
            })
            .max_by(|(a, _), (b, _)| a.total_cmp(b));

        if let Some((_sim, cluster)) = best {
            cluster.sources.insert(source.to_string());
            // 短段不更新质心、不增count(短段声纹噪声大,防拖歪质心)
            if num_samples >= MIN_CENTROID_UPDATE_SAMPLES {
                // 质心 running mean(在单位向量上),再归一化
                let n = cluster.count as f32;
                for (ci, ui) in cluster.centroid.iter_mut().zip(&unit) {
                    *ci = (*ci * n + ui) / (n + 1.0);
                }
                if let Some(renorm) = normalize(&cluster.centroid) {
                    cluster.centroid = renorm;
                }
                cluster.count += 1;
                cluster.total_ms += (num_samples / 16) as u64;
            }
            return Some(cluster.id.clone());
        }

        // 软归属:严格阈值未命中,但与某个**普通簇**(非种子)的相似度落在灰区
        // [SOFT, ASSIGN) → 归入该簇打标签,不更新质心/计数/时长(弱证据不留痕:
        // 不拖质心、不计入自动入库时长)。种子簇除外——弱证据错认领人名比碎片化更糟。
        let soft = self
            .clusters
            .iter_mut()
            .filter_map(|c| {
                if c.is_seed() {
                    return None;
                }
                let sim = dot(&c.centroid, &unit);
                (sim >= SOFT_ASSIGN_THRESHOLD).then_some((sim, c))
            })
            .max_by(|(a, _), (b, _)| a.total_cmp(b));
        if let Some((_sim, cluster)) = soft {
            cluster.sources.insert(source.to_string());
            return Some(cluster.id.clone());
        }

        if num_samples < MIN_NEW_CLUSTER_SAMPLES {
            return None; // 短段不建簇(也够不着任何软归属 → 留空,Aing 兜底)
        }
        let id = format!("S{}", self.next_id);
        self.next_id += 1;
        self.clusters.push(Cluster {
            id: id.clone(),
            centroid: unit,
            count: 1,
            sources: BTreeSet::from([source.to_string()]),
            person: None,
            person_name: None,
            // 建簇本身要求 num_samples >= MIN_NEW_CLUSTER_SAMPLES(足够长的段),
            // 首个成员的时长直接计入,与既有簇长段累加同一口径。
            total_ms: (num_samples / 16) as u64,
            // 会话内新建的普通簇没有"历史基数"，count 从 0 开始就是纯增量。
            seed_base_count: 0,
            reported_ms: 0,
            session_enrolled: false,
        });
        Some(id)
    }

    /// 取走自上次调用以来检测到的合并对 (被并 id, 并入 id)。
    pub fn take_merges(&mut self) -> Vec<(String, String)> {
        self.detect_merges();
        std::mem::take(&mut self.pending_merges)
    }

    fn detect_merges(&mut self) {
        loop {
            let mut found: Option<(usize, usize)> = None;
            'outer: for i in 0..self.clusters.len() {
                for j in (i + 1)..self.clusters.len() {
                    let (a, b) = (&self.clusters[i], &self.clusters[j]);
                    // 合并门槛按配对身份分档:
                    // - 不同 person 禁止自动互并("库里两人实为一人"只能用户在管理页显式合并);
                    // - 无主簇 ↔ 有主簇(或同 person 双簇)用归簇同款 SEED_ASSIGN_THRESHOLD:
                    //   开场短段声纹噪声大、够不着种子门槛时会另立无主簇,若其质心本可归簇
                    //   命中种子(≥0.68),就该并回去——否则卡在 [0.68, 0.74) 死区裂人
                    //   (冒烟实锤:同人双簇余弦 0.711 永远等不到 0.74);
                    // - 无主 ↔ 无主维持 MERGE_THRESHOLD(0.74 为首轮真实会议校准,防过度合并)。
                    let pair_threshold = match (&a.person, &b.person) {
                        (Some(x), Some(y)) if x != y => continue,
                        // 放宽到归簇同款阈值只适用于"跨会话种子"参与的配对(死区修复的
                        // 本意)：本场实时入库的簇不是种子,与无主簇合并仍走普通门槛,
                        // 行为与"尚未入库"时完全一致。
                        (Some(_), None) | (None, Some(_)) | (Some(_), Some(_)) => {
                            if a.is_seed() || b.is_seed() { SEED_ASSIGN_THRESHOLD } else { MERGE_THRESHOLD }
                        }
                        (None, None) => MERGE_THRESHOLD,
                    };
                    if dot(&a.centroid, &b.centroid) >= pair_threshold {
                        found = Some((i, j));
                        break 'outer;
                    }
                }
            }
            let Some((i, j)) = found else { break };
            // 小簇并入大簇(计数大者胜;平局取 i)
            let (win, lose) = if self.clusters[j].count > self.clusters[i].count { (j, i) } else { (i, j) };
            let loser = self.clusters.remove(lose);
            let win = if lose < win { win - 1 } else { win };
            let winner = &mut self.clusters[win];
            let (wn, ln) = (winner.count as f32, loser.count as f32);
            for (wc, lc) in winner.centroid.iter_mut().zip(&loser.centroid) {
                *wc = (*wc * wn + *lc * ln) / (wn + ln);
            }
            if let Some(renorm) = normalize(&winner.centroid) {
                winner.centroid = renorm;
            }
            winner.count += loser.count;
            winner.seed_base_count += loser.seed_base_count;
            winner.total_ms += loser.total_ms;
            winner.reported_ms += loser.reported_ms;
            winner.sources.extend(loser.sources.iter().cloned());
            // winner 无 person 而 loser 有 → 继承(person 冲突的情形已被上面的检查挡掉)；
            // session_enrolled 随 person 一起继承，阈值档位跟着身份走。
            if winner.person.is_none() {
                winner.person = loser.person.clone();
                winner.person_name = loser.person_name.clone();
                winner.session_enrolled = loser.session_enrolled;
            }
            self.pending_merges.push((loser.id.clone(), winner.id.clone()));
        }
    }

    pub fn speakers(&self) -> Vec<SpeakerInfo> {
        self.clusters
            .iter()
            .map(|c| SpeakerInfo {
                id: c.id.clone(),
                sources: c.sources.clone(),
                person: c.person.clone(),
                name: c.person_name.clone(),
            })
            .collect()
    }

    /// 导出全部簇的质心快照(供会话结束时交给 DiarEvent::Snapshot,P4.5 续录铺底)。
    /// count 只导本场净增量(= 内部累计 count - seed_base_count):种子簇/续录恢复的
    /// 簇带着历史基数,如果原样导出全量 count,停止时 upsert 会把这份历史基数再次
    /// 加到库里已有的 count 上——每场近似翻倍,几何级数膨胀,质心学习率随之失真
    /// (见终审 triage②)。saturating_sub 兜底:正常不应发生但防御 count 被异常改小。
    pub fn snapshot(&self) -> Vec<ClusterSnapshot> {
        self.clusters
            .iter()
            .map(|c| ClusterSnapshot {
                id: c.id.clone(),
                centroid: c.centroid.clone(),
                count: c.count.saturating_sub(c.seed_base_count),
                sources: c.sources.clone(),
                person: c.person.clone(),
                // 会话中实时入库(mark_enrolled)已把入库时刻的 total_ms 报给库,
                // 这里同 count 一样只导净增量,防停止时 upsert 重复累加。
                total_ms: c.total_ms.saturating_sub(c.reported_ms),
            })
            .collect()
    }

    /// 会话中实时入库候选：无主(person=None)、有质心、本场确有出现(sources 非空,
    /// 排除未命中种子)、累计发声 ≥ min_ms 的簇快照。调用方(lib.rs)入库拿到全局
    /// person id 后须回调 mark_enrolled，否则每条 final 都会重复导出同一批候选。
    pub fn enroll_candidates(&self, min_ms: u64) -> Vec<ClusterSnapshot> {
        self.clusters
            .iter()
            .filter(|c| {
                c.person.is_none()
                    && !c.centroid.is_empty()
                    && !c.sources.is_empty()
                    && c.total_ms >= min_ms
            })
            .map(|c| ClusterSnapshot {
                id: c.id.clone(),
                centroid: c.centroid.clone(),
                count: c.count.saturating_sub(c.seed_base_count),
                sources: c.sources.clone(),
                person: None,
                total_ms: c.total_ms,
            })
            .collect()
    }

    /// 标记簇已实时入库为全局人物：设 person、置 session_enrolled(维持普通归簇/
    /// 合并阈值,见 is_seed)，并把当前 count/total_ms 记为已上报基数——停止时
    /// snapshot() 只再导出入库之后的净增量，库侧不双计。未知 id 静默忽略
    /// (入库与归簇之间该簇可能已被合并掉，person 已由合并继承逻辑处理)。
    pub fn mark_enrolled(&mut self, id: &str, person: &str) {
        if let Some(c) = self.clusters.iter_mut().find(|c| c.id == id) {
            c.person = Some(person.to_string());
            c.session_enrolled = true;
            c.seed_base_count = c.count;
            c.reported_ms = c.total_ms;
        }
    }

    /// 从质心快照重建 registry:编号续接(解析所有 "S{n}" 取最大 n,next_id = n+1)；
    /// 质心为空的项不建簇但计入编号。空切片 ≡ new()。等价 `with_seeds(snaps, &[])`。
    pub fn from_snapshot(snaps: &[ClusterSnapshot]) -> Self {
        Self::with_seeds(snaps, &[])
    }

    fn from_snapshot_inner(snaps: &[ClusterSnapshot]) -> Self {
        let mut next_id = 1u32;
        let mut clusters = Vec::new();
        for s in snaps {
            if let Some(n) = s.id.strip_prefix('S').and_then(|rest| rest.parse::<u32>().ok()) {
                if n + 1 > next_id {
                    next_id = n + 1;
                }
            }
            if !s.centroid.is_empty() {
                clusters.push(Cluster {
                    id: s.id.clone(),
                    centroid: s.centroid.clone(),
                    count: s.count,
                    sources: s.sources.clone(),
                    person: s.person.clone(),
                    // 快照不带姓名(名字真值在库/笔记表,由上层从 writer 表/库补);
                    // 只有种子注入路径才带上 person_name。
                    person_name: None,
                    total_ms: s.total_ms,
                    // 恢复出的 count 本身就是"此前已上报过"的历史累计(上一场
                    // snapshot() 导出的净增量,resume 时原样存进了 speakers.json 的
                    // count 字段)。把它设为本簇的基数，本场结束再导出时才只报告
                    // 本场新产生的净增量，不重复上报上一场已经报过的部分。
                    seed_base_count: s.count,
                    reported_ms: 0,
                    // 快照恢复的关联是"上一场"建立的：本场视作跨会话种子(严格阈值)。
                    session_enrolled: false,
                });
            }
        }
        Self { clusters, next_id, assigns: 0, pending_merges: Vec::new(), enroller: None }
    }

    /// 库种子注入:先铺会话快照(续录),再为快照中未出现的 person 建种子簇。
    /// 快照优先:续录质心更贴近本场信道。count 从库带(权重大、漂移慢),
    /// total_ms 归 0(只统计本场,供停止时的入库门槛与库累计)。
    pub fn with_seeds(snaps: &[ClusterSnapshot], seeds: &[SeedCluster]) -> Self {
        let mut r = Self::from_snapshot_inner(snaps);
        let known: BTreeSet<String> = r.clusters.iter().filter_map(|c| c.person.clone()).collect();
        for s in seeds {
            if known.contains(&s.person) {
                continue; // 快照中已关联该 person(续录场景),不重复建簇
            }
            let Some(centroid) = normalize(&s.centroid) else {
                continue; // 零向量/非法种子质心丢弃,不建残废簇
            };
            let id = format!("S{}", r.next_id);
            r.next_id += 1;
            let base_count = s.count.max(1);
            r.clusters.push(Cluster {
                id,
                centroid,
                count: base_count,
                sources: BTreeSet::new(),
                person: Some(s.person.clone()),
                person_name: Some(s.name.clone()),
                total_ms: 0,
                // 种子的 count 从库带(库里已有的历史样本数)，是纯基数：本场哪怕
                // 一次都没命中，导出时也不该把这份库存量再报一遍给库自己。
                seed_base_count: base_count,
                reported_ms: 0,
                session_enrolled: false,
            });
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三维正交基方便构造:e1/e2 相似度 0,混合向量可控。
    fn v(x: f32, y: f32, z: f32) -> Vec<f32> {
        vec![x, y, z]
    }
    const LONG: usize = 48000; // 3s,足以建簇(≥ MIN_NEW_CLUSTER_SAMPLES 2.5s)

    #[test]
    fn first_assign_creates_s1() {
        let mut r = SpeakerRegistry::new();
        assert_eq!(r.assign(&v(1.0, 0.0, 0.0), "mic", LONG), Some("S1".into()));
        let sp = r.speakers();
        assert_eq!(sp.len(), 1);
        assert_eq!(sp[0].id, "S1");
        assert!(sp[0].sources.contains("mic"));
    }

    /// 二轮校准(2026-07-08)核心行为:灰区 [SOFT, ASSIGN) 软归属最近普通簇,
    /// 不建簇、不拖质心;低于 SOFT 且够长才建簇;短段(<2.5s)永远无权建簇。
    #[test]
    fn gray_zone_soft_assigns_without_touching_centroid() {
        let mut r = SpeakerRegistry::new();
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap(); // S1
        // 余弦 0.5(灰区):软归属 S1,不开新簇
        let y = (1.0f32 - 0.25).sqrt();
        assert_eq!(r.assign(&v(0.5, y, 0.0), "system", LONG), Some("S1".into()));
        assert_eq!(r.speakers().len(), 1, "灰区不得开新簇");
        assert!(r.speakers()[0].sources.contains("system"), "软归属应记录来源");
        // 质心未被拖动:snapshot 的 count/total_ms 均不含软归属段
        let snap = r.snapshot();
        assert_eq!(snap[0].count, 1, "软归属不更新计数");
        assert_eq!(snap[0].total_ms, 3000, "软归属不累计时长(不影响自动入库门槛)");
        // 质心仍是 (1,0,0):正交向量余弦 0 < SOFT → 长段建新簇
        assert_eq!(r.assign(&v(0.0, 0.0, 1.0), "mic", LONG), Some("S2".into()));
    }

    #[test]
    fn short_segment_soft_assigns_but_never_creates_cluster() {
        let mut r = SpeakerRegistry::new();
        // 短段(1s)在空表上:无簇可归 → None(留空,Aing 兜底),绝不建簇
        assert_eq!(r.assign(&v(1.0, 0.0, 0.0), "mic", 16000), None);
        assert_eq!(r.speakers().len(), 0);
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap(); // S1
        // 短段灰区(余弦 0.5)→ 软归属 S1
        let y = (1.0f32 - 0.25).sqrt();
        assert_eq!(r.assign(&v(0.5, y, 0.0), "mic", 16000), Some("S1".into()));
        // 中段(1s,不足 2.5s)即使与所有簇正交也无权建簇 → None
        assert_eq!(r.assign(&v(0.0, 0.0, 1.0), "mic", 16000), None);
        assert_eq!(r.speakers().len(), 1);
    }

    #[test]
    fn seed_cluster_excluded_from_soft_assign() {
        let seeds = vec![SeedCluster {
            person: "P1".into(),
            name: "甲".into(),
            centroid: v(1.0, 0.0, 0.0),
            count: 5,
        }];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        // 与种子余弦 0.5(灰区):不得软归属领走人名 → 长段落地为新普通簇
        let y = (1.0f32 - 0.25).sqrt();
        let id = r.assign(&v(0.5, y, 0.0), "mic", LONG).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person, None, "灰区弱证据不得认领种子人物");
    }

    #[test]
    fn similar_joins_dissimilar_creates_new() {
        let mut r = SpeakerRegistry::new();
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG);
        // 与 e1 余弦 ≈ 0.995,归入 S1
        assert_eq!(r.assign(&v(1.0, 0.1, 0.0), "system", LONG), Some("S1".into()));
        // 正交,新建 S2
        assert_eq!(r.assign(&v(0.0, 1.0, 0.0), "system", LONG), Some("S2".into()));
        // S1 记录了两个来源
        let sp = r.speakers();
        let s1 = sp.iter().find(|s| s.id == "S1").unwrap();
        assert!(s1.sources.contains("mic") && s1.sources.contains("system"));
    }

    #[test]
    fn centroid_tracks_running_mean() {
        let mut r = SpeakerRegistry::new();
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG);
        // 多次喂入偏向 e1+e2 混合的向量后,质心偏移,原纯 e2 向量也能归入
        for _ in 0..8 {
            r.assign(&v(1.0, 0.8, 0.0), "mic", LONG);
        }
        assert_eq!(
            r.assign(&v(0.55, 0.75, 0.0), "mic", LONG),
            Some("S1".into()),
            "质心应随成员漂移"
        );
    }

    #[test]
    fn short_segment_never_creates_cluster_but_can_join() {
        let mut r = SpeakerRegistry::new();
        // 短段 + 无既有簇 → None
        assert_eq!(r.assign(&v(1.0, 0.0, 0.0), "mic", 8000), None);
        // 建立 S1 后,短段相似 → 归入
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG);
        assert_eq!(r.assign(&v(1.0, 0.05, 0.0), "mic", 8000), Some("S1".into()));
        // 短段不相似 → None(不建新簇)
        assert_eq!(r.assign(&v(0.0, 1.0, 0.0), "mic", 8000), None);
        assert_eq!(r.speakers().len(), 1);
    }

    #[test]
    fn drifting_clusters_get_merged_small_into_large() {
        let mut r = SpeakerRegistry::new();
        // S1(大簇,全部来自 mic):质心 ≈ e1
        for _ in 0..6 {
            r.assign(&v(1.0, 0.0, 0.0), "mic", LONG);
        }
        // S2 种子(来自 system):与 e1 余弦 0.30 < ASSIGN_THRESHOLD → 新建
        r.assign(&v(0.30, 0.954, 0.0), "system", LONG);
        assert_eq!(r.speakers().len(), 2);
        assert!(r.take_merges().is_empty(), "初始两簇远离,不该合并");

        // 相向漂移(在线聚类的真实收敛方式):
        // 1) 这批向量与 S2 当前质心更近 → 归入 S2,把 S2 从 72.5° 拖向 e1
        for k in 1..=10 {
            let t = 0.30 + 0.05 * k as f32; // 0.35..0.80
            let y = (1.0 - t * t).max(0.0).sqrt();
            r.assign(&v(t, y, 0.0), "system", LONG);
        }
        // 2) 这批与 S1 质心(≈e1,余弦 0.90)更近 → 归入 S1,把 S1 拖向 S2
        for _ in 0..12 {
            r.assign(&v(0.90, 0.436, 0.0), "mic", LONG);
        }

        // 两簇相向漂移后质心相似度过 MERGE_THRESHOLD → 合并,小簇 S2 并入大簇 S1
        let merges = r.take_merges();
        assert_eq!(merges.len(), 1, "相向漂移后两簇应合并");
        let (loser, winner) = &merges[0];
        assert_eq!(winner, "S1", "小簇并入大簇");
        assert_eq!(loser, "S2");
        assert_eq!(r.speakers().len(), 1);
        // sources 并集:S1 成员全是 mic,"system" 只能来自被并入的 S2
        assert!(r.speakers()[0].sources.contains("system"), "合并须汇总 sources");
        assert!(r.speakers()[0].sources.contains("mic"));
    }

    #[test]
    fn short_joins_do_not_drag_centroid() {
        let mut r = SpeakerRegistry::new();
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG); // S1 质心 = e1
        // 10 个短段归入 S1(相似度 0.8 ≥ 0.62),但不得拖动质心
        for _ in 0..10 {
            assert_eq!(r.assign(&v(0.8, 0.6, 0.0), "mic", 8000), Some("S1".into()));
        }
        // 探针:与 e1 余弦 0.35 —— 若质心仍是 e1 → 低于阈值,新建 S2;
        // 若质心被短段拖向 (0.8,0.6) → 会被吸入 S1(回归即失败)
        assert_eq!(r.assign(&v(0.35, 0.937, 0.0), "mic", LONG), Some("S2".into()));
    }

    #[test]
    fn zero_or_mismatched_dim_embedding_returns_none() {
        let mut r = SpeakerRegistry::new();
        assert_eq!(r.assign(&[], "mic", LONG), None);
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG);
        assert_eq!(r.assign(&[1.0, 0.0], "mic", LONG), None, "维度不符丢弃");
        assert_eq!(r.assign(&[0.0, 0.0, 0.0], "mic", LONG), None, "零向量丢弃");
    }

    #[test]
    fn snapshot_roundtrip_preserves_clusters_and_continues_assign() {
        let mut r = SpeakerRegistry::new();
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG);
        r.assign(&v(0.0, 1.0, 0.0), "system", LONG);
        let snaps = r.snapshot();
        assert_eq!(snaps.len(), 2);
        let s1 = snaps.iter().find(|s| s.id == "S1").unwrap();
        assert_eq!(s1.count, 1);
        assert!(s1.sources.contains("mic"));
        assert!((s1.centroid[0] - 1.0).abs() < 1e-6);

        let mut r2 = SpeakerRegistry::from_snapshot(&snaps);
        assert_eq!(r2.speakers().len(), 2);
        // 继续 assign 相同向量归入原簇(质心/簇结构被完整还原)
        assert_eq!(r2.assign(&v(1.0, 0.0, 0.0), "mic", LONG), Some("S1".into()));
        assert_eq!(r2.assign(&v(0.0, 1.0, 0.0), "system", LONG), Some("S2".into()));
    }

    #[test]
    fn from_snapshot_continues_numbering_past_max_existing_id() {
        let snaps = vec![ClusterSnapshot {
            id: "S3".into(),
            centroid: v(1.0, 0.0, 0.0),
            count: 5,
            sources: BTreeSet::from(["mic".to_string()]),
            person: None,
            total_ms: 0,
        }];
        let mut r = SpeakerRegistry::from_snapshot(&snaps);
        // 与 S3 质心正交 → 新建簇,编号应续接为 S4(而非从 S1 重来)
        assert_eq!(r.assign(&v(0.0, 1.0, 0.0), "system", LONG), Some("S4".into()));
    }

    #[test]
    fn from_snapshot_empty_centroid_item_counts_id_but_builds_no_cluster() {
        let snaps = vec![ClusterSnapshot {
            id: "S5".into(),
            centroid: Vec::new(),
            count: 0,
            sources: BTreeSet::new(),
            person: None,
            total_ms: 0,
        }];
        let mut r = SpeakerRegistry::from_snapshot(&snaps);
        assert_eq!(r.speakers().len(), 0, "空质心项不建簇");
        // 编号仍续接到 S6(计入编号)
        assert_eq!(r.assign(&v(1.0, 0.0, 0.0), "mic", LONG), Some("S6".into()));
    }

    #[test]
    fn from_snapshot_empty_slice_equals_new() {
        let mut r = SpeakerRegistry::from_snapshot(&[]);
        let mut r2 = SpeakerRegistry::new();
        assert_eq!(r.speakers(), r2.speakers());
        assert_eq!(r.assign(&v(1.0, 0.0, 0.0), "mic", LONG), Some("S1".into()));
        assert_eq!(r2.assign(&v(1.0, 0.0, 0.0), "mic", LONG), Some("S1".into()));
    }

    #[test]
    fn seeds_inject_match_and_dedup() {
        // 库里张三(P1)质心 e1;快照里已有 S2 关联 P9(续录场景)
        let snap = ClusterSnapshot {
            id: "S2".into(),
            centroid: v(0.0, 1.0, 0.0),
            count: 5,
            sources: BTreeSet::from(["mic".to_string()]),
            person: Some("P9".into()),
            total_ms: 0,
        };
        let seeds = vec![
            SeedCluster { person: "P1".into(), name: "张三".into(), centroid: v(1.0, 0.0, 0.0), count: 40 },
            SeedCluster { person: "P9".into(), name: "旧人".into(), centroid: v(0.0, 1.0, 0.0), count: 7 },
        ];
        let mut r = SpeakerRegistry::with_seeds(&[snap], &seeds);
        // P9 已在快照,种子去重:簇数 = 快照1 + P1 种子1
        assert_eq!(r.speakers().len(), 2);
        // 命中张三种子:余弦 0.98 > 0.68 → 返回其 S 号,speakers() 带 person/name
        let id = r.assign(&v(0.98, 0.199, 0.0), "mic", LONG).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person.as_deref(), Some("P1"));
        assert_eq!(info.name.as_deref(), Some("张三"));
    }

    #[test]
    fn seed_threshold_is_stricter_than_session_threshold() {
        // 相似度 ~0.65:普通簇能命中(≥0.62),种子簇不能(<0.68)
        let seeds = vec![SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 10 }];
        let mut seeded = SpeakerRegistry::with_seeds(&[], &seeds);
        let probe = v(0.65, (1.0f32 - 0.65 * 0.65).sqrt(), 0.0);
        let id = seeded.assign(&probe, "mic", LONG).unwrap();
        let info = seeded.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person, None, "0.65 < 0.68,不得吸入种子簇,应新建普通簇");

        let mut plain = SpeakerRegistry::new();
        plain.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap();
        let id2 = plain.assign(&probe, "mic", LONG).unwrap();
        assert_eq!(plain.speakers().len(), 1, "0.65 ≥ 0.62,普通簇应命中: {id2}");
    }

    #[test]
    fn unowned_cluster_in_dead_zone_merges_into_seed_at_assign_threshold() {
        // 冒烟实锤场景:开场短段够不着种子 0.68 另立无主簇,漂到与种子余弦 0.711——
        // 落在 [SEED_ASSIGN, MERGE) 死区,旧逻辑永不合并,同一人被裂成两个。
        let snap = ClusterSnapshot {
            id: "S9".into(),
            centroid: v(0.71, 0.70413, 0.0), // 与 e1 余弦 ≈ 0.710
            count: 2,
            sources: BTreeSet::from(["system".to_string()]),
            person: None,
            total_ms: 3000,
        };
        let seeds = vec![SeedCluster {
            person: "P1".into(),
            name: "甲".into(),
            centroid: v(1.0, 0.0, 0.0),
            count: 10,
        }];
        let mut r = SpeakerRegistry::with_seeds(&[snap], &seeds);
        assert_eq!(r.take_merges().len(), 1, "无主↔有主在 0.71 应按归簇同款阈值合并");
        let s = r.speakers();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].person.as_deref(), Some("P1"), "种子 count 大为 winner,person 保留");

        // 对照:无主↔无主同样 0.71 不并——0.74 门槛(首轮真实会议校准)不受影响。
        let snaps2 = [
            ClusterSnapshot {
                id: "S1".into(),
                centroid: v(1.0, 0.0, 0.0),
                count: 2,
                sources: BTreeSet::from(["mic".to_string()]),
                person: None,
                total_ms: 0,
            },
            ClusterSnapshot {
                id: "S2".into(),
                centroid: v(0.71, 0.70413, 0.0),
                count: 2,
                sources: BTreeSet::from(["mic".to_string()]),
                person: None,
                total_ms: 0,
            },
        ];
        let mut r2 = SpeakerRegistry::with_seeds(&snaps2, &[]);
        assert!(r2.take_merges().is_empty(), "无主↔无主 0.71 < 0.74 不得合并");
        assert_eq!(r2.speakers().len(), 2);
    }

    #[test]
    fn different_persons_never_automerge_and_winner_inherits_person() {
        let seeds = vec![
            SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 10 },
            SeedCluster { person: "P2".into(), name: "乙".into(), centroid: v(0.9805, 0.19612, 0.0), count: 10 },
        ];
        // 两种子余弦 ~0.98 ≥ MERGE_THRESHOLD,但 person 不同 → 永不自动合并
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        assert!(r.take_merges().is_empty());
        assert_eq!(r.speakers().len(), 2);

        // 无 person 簇与有 person 簇可并,winner 继承 person:
        // S1 = 种子 P1(质心 e1,count 小);S2 = 普通簇,经相向漂移后与 S1 质心
        // 余弦 ≥ MERGE_THRESHOLD,且 count 更大 → S2 胜出,继承 S1 的 person。
        let mut r2 = SpeakerRegistry::with_seeds(
            &[],
            &[SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 1 }],
        );
        // 建 S2(与 e1 余弦 0.30,低于种子阈值 0.68 → 新建普通簇)
        r2.assign(&v(0.30, (1.0f32 - 0.09f32).sqrt(), 0.0), "system", LONG).unwrap();
        // 相向漂移(1):这批向量与 S2 当前质心更近 → 归入 S2,把 S2 拖向 e1
        for k in 1..=10 {
            let t = 0.30 + 0.05 * k as f32; // 0.35..0.80
            let y = (1.0 - t * t).max(0.0).sqrt();
            r2.assign(&v(t, y, 0.0), "system", LONG).unwrap();
        }
        // 相向漂移(2):这批与 S1(种子,阈值 0.68)质心(余弦 0.90)更近 → 归入 S1,
        // 把 S1 拖向 S2;迭代次数(9)刻意少于 S2 的 count(11),让 S2 保持更大 count。
        for _ in 0..9 {
            r2.assign(&v(0.90, 0.436, 0.0), "mic", LONG).unwrap();
        }

        let merges = r2.take_merges();
        assert_eq!(merges.len(), 1, "相向漂移后两簇应合并");
        let (loser, winner) = &merges[0];
        assert_eq!(winner, "S2", "S2 count(11) > S1 count(10),大簇胜出");
        assert_eq!(loser, "S1");
        let info = r2.speakers().into_iter().find(|s| &s.id == winner).unwrap();
        assert_eq!(info.person.as_deref(), Some("P1"), "无 person 的胜者须继承败者的 person");
    }

    #[test]
    fn total_ms_accumulates_only_on_long_segments() {
        let mut r = SpeakerRegistry::new();
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap(); // 3s
        r.assign(&v(1.0, 0.0, 0.0), "mic", 4800).unwrap(); // 0.3s 短段不计
        // 中段(0.6s~2.5s):可更新质心与时长,但无权建簇(见 assign 的双门槛)。
        r.assign(&v(1.0, 0.0, 0.0), "mic", 16000).unwrap(); // 1s
        let snap = r.snapshot();
        assert_eq!(snap[0].total_ms, 3000 + 1000);
    }

    /// 终审 triage①锁死判别式:sources 为空 ⇔ 未命中的种子簇。两个种子(甲/乙)注入,
    /// 只对甲做一次命中 assign,乙从未被认领。speakers() 里能看到两个簇(种子铺底
    /// 阶段就已存在),但只有命中的甲 sources 非空——这是 lib.rs/writer.rs 两处过滤
    /// 用来分辨"真实说话人"与"未命中的库种子"的唯一依据,此测试锁死其正确性。
    #[test]
    fn unhit_seed_cluster_has_empty_sources_hit_one_has_nonempty() {
        let seeds = vec![
            SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 10 },
            SeedCluster { person: "P2".into(), name: "乙".into(), centroid: v(0.0, 1.0, 0.0), count: 10 },
        ];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        // 只命中甲(与 e1 余弦 ≈0.99 ≥ 种子阈值 0.68);乙从未被 assign 到。
        let hit_id = r.assign(&v(0.99, 0.14, 0.0), "mic", LONG).unwrap();
        let infos = r.speakers();
        assert_eq!(infos.len(), 2, "两个种子簇都在(种子铺底不因未命中而消失)");
        let hit = infos.iter().find(|s| s.id == hit_id).unwrap();
        assert_eq!(hit.person.as_deref(), Some("P1"));
        assert!(!hit.sources.is_empty(), "命中的种子簇 sources 非空");
        let unhit = infos.iter().find(|s| s.id != hit_id).unwrap();
        assert_eq!(unhit.person.as_deref(), Some("P2"));
        assert!(unhit.sources.is_empty(), "未命中的种子簇 sources 恒空——判别式的锁死点");
    }

    /// 终审 triage②:种子 count 增量导出防复利膨胀。种子带库 count=40,本场命中两次
    /// 长段(assign 各更新一次 count),snapshot() 导出的 count 应只是本场净增量(2),
    /// 而不是内部累计的全量(42)——否则每场停止 upsert 都会把库里已有的历史样本数
    /// 再报一遍,库 count 几何级数膨胀,质心学习率随之衰减到接近失效。
    #[test]
    fn snapshot_exports_incremental_count_not_seed_base_for_seed_cluster() {
        let seeds = vec![SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 40 }];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap();
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap();
        let snap = r.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].count, 2, "导出 count 应只是本场净增量,不含种子基数 40");
    }

    #[test]
    fn enroll_candidates_filters_by_person_centroid_sources_and_ms() {
        let mut r = SpeakerRegistry::new();
        // S1: 48000 样本 = 3000ms 长段。
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap();
        assert!(r.enroll_candidates(4000).is_empty(), "累计 3000ms < 4000ms 不够料");
        let c = r.enroll_candidates(3000);
        assert_eq!(c.len(), 1, "达到门槛应导出候选");
        assert_eq!(c[0].id, "S1");
        assert_eq!(c[0].person, None);
        assert_eq!(c[0].total_ms, 3000);

        // 已关联 person 的簇不再是候选。
        r.mark_enrolled("S1", "P7");
        assert!(r.enroll_candidates(2000).is_empty(), "已入库簇不重复候选");

        // 未命中的种子簇(sources 空)不是候选。
        let seeds = vec![SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(0.0, 1.0, 0.0), count: 10 }];
        let r2 = SpeakerRegistry::with_seeds(&[], &seeds);
        assert!(r2.enroll_candidates(0).is_empty(), "种子簇有主且 sources 空,不候选");
    }

    #[test]
    fn enroll_pending_enrolls_once_and_tolerates_callback_failure() {
        use std::sync::{Arc, Mutex};
        let calls = Arc::new(Mutex::new(0u32));
        let calls2 = calls.clone();
        let mut r = SpeakerRegistry::new();
        // 第一次回调失败(库降级),之后成功发 P9。
        r.set_enroller(
            2000,
            Box::new(move |snap| {
                assert_eq!(snap.id, "S1");
                let mut n = calls2.lock().unwrap();
                *n += 1;
                if *n == 1 { None } else { Some("P9".into()) }
            }),
        );
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap(); // 2000ms 够料
        r.enroll_pending(); // 回调失败 → 不标记,留待重试
        assert_eq!(r.speakers()[0].person, None);
        r.enroll_pending(); // 重试成功
        assert_eq!(r.speakers()[0].person.as_deref(), Some("P9"));
        r.enroll_pending(); // 已入库,不再候选
        assert_eq!(*calls.lock().unwrap(), 2, "入库成功后不再重复回调");
    }

    #[test]
    fn mark_enrolled_sets_person_and_snapshot_exports_only_increments() {
        let mut r = SpeakerRegistry::new();
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap(); // count=1, 3000ms
        r.mark_enrolled("S1", "P3");
        let info = &r.speakers()[0];
        assert_eq!(info.person.as_deref(), Some("P3"));

        // 入库后再来一条长段:snapshot 只报入库之后的净增量。
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap();
        let snap = r.snapshot();
        assert_eq!(snap[0].count, 1, "count 只报入库后增量(入库前 1 条已上报)");
        assert_eq!(snap[0].total_ms, 3000, "total_ms 只报入库后增量");
        assert_eq!(snap[0].person.as_deref(), Some("P3"));
    }

    #[test]
    fn session_enrolled_cluster_keeps_normal_assign_threshold() {
        // 相似度 ~0.65:普通阈值(0.62)可命中,种子阈值(0.68)不可。
        // 实时入库后仍应命中——拿到全局 id 不该让归簇变严。
        let mut r = SpeakerRegistry::new();
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap();
        r.mark_enrolled("S1", "P1");
        let probe = v(0.65, (1.0f32 - 0.65 * 0.65).sqrt(), 0.0);
        assert_eq!(r.assign(&probe, "mic", LONG), Some("S1".into()), "0.65 ≥ 0.62 应命中本场入库簇");
    }

    #[test]
    fn session_enrolled_cluster_merges_at_normal_threshold_not_seed_threshold() {
        // 死区修复(0.68 放宽)只适用于跨会话种子:本场入库簇 ↔ 无主簇在 0.71 不得合并
        // (行为与未入库时一致,无主↔无主 0.71 < 0.74 不并)。
        let snaps = [
            ClusterSnapshot {
                id: "S1".into(),
                centroid: v(1.0, 0.0, 0.0),
                count: 2,
                sources: BTreeSet::from(["mic".to_string()]),
                person: None,
                total_ms: 0,
            },
            ClusterSnapshot {
                id: "S2".into(),
                centroid: v(0.71, 0.70413, 0.0), // 与 e1 余弦 ≈ 0.710
                count: 1,
                sources: BTreeSet::from(["mic".to_string()]),
                person: None,
                total_ms: 0,
            },
        ];
        let mut r = SpeakerRegistry::with_seeds(&snaps, &[]);
        r.mark_enrolled("S1", "P1");
        assert!(r.take_merges().is_empty(), "本场入库簇按普通门槛,0.71 < 0.74 不并");

        // 对照:同构型但 S1 是快照恢复的有主簇(跨会话) → 0.68 档,0.71 应并。
        let mut snaps_seeded = snaps.clone();
        snaps_seeded[0].person = Some("P1".into());
        let mut r2 = SpeakerRegistry::with_seeds(&snaps_seeded, &[]);
        assert_eq!(r2.take_merges().len(), 1, "跨会话有主簇维持死区修复档,0.71 应并");
    }

    #[test]
    fn merge_sums_reported_ms_and_stop_snapshot_stays_incremental() {
        let mut r = SpeakerRegistry::new();
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap(); // S1 3000ms
        r.mark_enrolled("S1", "P1"); // 报掉 3000ms
        // S2 无主,与 S1 相向漂移后合并(S1 已入库,S2 并入)。
        r.assign(&v(0.30, 0.954, 0.0), "mic", LONG).unwrap(); // S2 2000ms
        for k in 1..=10 {
            let t = 0.30 + 0.05 * k as f32;
            let y = (1.0 - t * t).max(0.0).sqrt();
            r.assign(&v(t, y, 0.0), "mic", LONG).unwrap();
        }
        for _ in 0..12 {
            r.assign(&v(0.90, 0.436, 0.0), "mic", LONG).unwrap();
        }
        let merges = r.take_merges();
        assert_eq!(merges.len(), 1, "相向漂移后应合并");
        assert_eq!(r.speakers().len(), 1);
        assert_eq!(r.speakers()[0].person.as_deref(), Some("P1"), "person 随合并保留");
        let snap = r.snapshot();
        // 全部长段 24 条 × 3000ms = 72000ms,入库时已报 3000ms → 增量 69000ms。
        assert_eq!(snap[0].total_ms, 69000, "合并后 reported_ms 基数保留,导出仍是净增量");
    }

    #[test]
    fn seed_threshold_no_longer_blocks_reachable_session_cluster() {
        // 库种子簇 A = e1,阈值 0.68;会话普通簇 B = e2(与 A 正交,dot=0,清清白白的独立普通簇),阈值 0.62。
        // 探针 P = (0.65, 0.63, sqrt(1-0.65²-0.63²)):与 A 余弦 0.65 ∈(0.62,0.68)(够不着种子阈值),
        // 与 B 余弦 0.63 ∈(0.62,0.68)(够得着普通阈值),且 0.63 < 0.65(全局最相似是 A,不是 B)。
        // 修前:全局 argmax 先选中 A(0.65 全局最大),再验 A 的阈值 0.68 → 失败 → 错误新建第三个簇。
        // 修后:先按各簇自己的阈值过滤合格候选(A 不合格被滤掉,只剩 B),合格者中取最相似 → 命中 B。
        let seeds = vec![SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 10 }];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds); // S1 = 种子簇 A
        let b_id = r.assign(&v(0.0, 1.0, 0.0), "mic", LONG).unwrap(); // S2 = 会话普通簇 B(与 A 正交)
        assert_eq!(r.speakers().len(), 2);

        let p3 = (1.0f32 - 0.65 * 0.65 - 0.63 * 0.63).sqrt();
        let probe = v(0.65, 0.63, p3);
        let id = r.assign(&probe, "mic", LONG).unwrap();
        assert_eq!(id, b_id, "应命中够得着的普通簇 B,而非被够不着的种子簇 A 挡住去新建簇");
        assert_eq!(r.speakers().len(), 2, "总簇数不变(命中已有簇,未新建)");
    }
}
