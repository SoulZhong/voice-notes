//! 在线增量声纹聚类:两路(mic/system)嵌入汇入同一 Registry,
//! 得全局「S1..Sn」。无模型依赖、单线程持有于 ASR worker。
//! 唯一的外部副作用点是可选的 enroller 回调(会话中实时入库拿全局 person id),
//! 由 lib.rs 注入、enroll_pending 触发;不注入时保持纯逻辑。

use std::collections::{BTreeSet, VecDeque};

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
/// 首轮校准是 0.6s(质心更新的容错比建簇高,running mean 稀释单段噪声)。
/// 二〇二六-〇八根因分析:短段系统性偏移非白噪声,running-mean 稀释不足以
/// 抵御,提到 1.5s;0.6~1.5s 段仍正常归簇打标签,但不止"无权改质心"——
/// 不更新质心、不计入 count/total_ms、也不进"近期贡献环"(见 assign_inner)。
/// 终审 F2:这连带让 enroll 累计口径收紧——AUTO_ENROLL_MS 的门槛依赖
/// total_ms 累计,0.6~1.5s 的碎句不再计入,同等语速下攒够登记门槛变慢,
/// 即碎句说话人的登记门槛隐性抬高。方向与"压制批量造新人"一致,是随本次
/// 校准接受的显式决定,不是遗漏。
pub const MIN_CENTROID_UPDATE_SAMPLES: usize = 24_000; // 1.5s
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
/// 段短于此(16kHz 样本数,2s)不参与种子命中:<2s 嵌入可靠性跳崖(文献:2s 条件
/// EER 可翻 3 倍),短段无权拍板"这是谁";仍可归场内簇/软归属。待评测集校准的初值。
pub const SEED_MIN_SAMPLES: usize = 32_000;
/// AS-Norm 增益通道:对称 z(与整理层 suggest 同式)达标且裸分不低于地板即命中种子。
/// 3.0 与自动归并 SUGGEST_STRONG_Z 同档。待评测集校准的初值。
pub const SEED_ASSIGN_Z: f32 = 3.0;
/// 见 SEED_ASSIGN_Z:z 通道命中仍要求的裸分地板。
///
/// **2026-08-23 试过 0.30,量完回退。** 留档以免有人照着"同人余弦中位只有 0.44"
/// 这条观察再改一次:
///
/// 观察是真的——241 份样本 wav 直接过 eres2netv2(绕开声纹库簿记),同一个人两段
/// 音频余弦中位 **0.44**(p25=0.29 p75=0.58),不同人中位 0.21、p90 0.39,EER 22.7%
/// 落在 0.31。地板 0.50 确实高于同人分数中位数。**但把地板降到 0.30 并不改善端到端
/// 识别。** speaker_loso_eval 上的 2×2(148 条探针,同一批样本,只动表示与地板):
///
/// ```text
/// 表示 \ 地板        0.50            0.30
/// 每样本一变体   84.5% / 62.8%   85.0% / 64.9%
/// 仅样本均值     86.5% / 60.8%   84.3% / 61.5%   (出手准确率 / 召回)
/// ```
///
/// 四格全落在噪声里(n=148、p≈0.62 时 95% 区间约 ±8 点),没有一格显著胜出;而
/// 配合"仅样本均值"表示时 0.30 把认错从 14 抬到 17。没有证据支持就不改,维持 0.50。
///
/// 顺带记下"为什么 0.62/0.68/0.74 那一档看起来偏高":它们是拿**会上的簇质心**
/// (几十段的平均,噪声被抹平)标定的,却被用到**跨场的单段探针**上。与 2026-07-12
/// 换模型无关——campplus 同口径类内中位 0.403、eres2netv2 0.436,两者一样。
/// 真要动这一档,先把评测集做厚(现在只有 240 段样本、38 人能当探针)。
pub const SEED_ASSIGN_RAW_FLOOR: f32 = 0.50;
/// KnnVoteMatcher 的票决席位数:探针只与最相近的这么多个库种子席位计票。
pub const SEED_KNN_K: usize = 5;
/// settings.speaker_match 取值:单最近邻(默认)。
pub const SPEAKER_MATCH_NEAREST: &str = "nearest";
/// settings.speaker_match 取值:top-K 多数票(实验项)。
pub const SPEAKER_MATCH_KNN_VOTE: &str = "knn_vote";
/// "近期贡献环"容量:回声追溯撤回窗口(session.rs RETRACT_WINDOW_MS=30s)内可能
/// 被撤销的 assign 调用数上限,防止长会话无界增长。超出容量的旧条目被静默淘汰
/// ——淘汰后对应的撤回请求视为 no-op(近似值,回声窗口内的高频 assign 场景下
/// 64 条已覆盖远超 30s 的真实语速)。
const CONTRIBUTION_RING_CAP: usize = 64;
/// 种子侧 AS-Norm cohort 统计的最少对比人数:库太小(其他人物 <3)算不出稳定
/// 分布。与 store 层 suggest_merges 的同名门槛(voiceprints.rs:877)同值同理——
/// 两处场景不同(这里是种子注入时预计算,那边是整理建议时计算)各自独立定义，
/// 不共享同一常量,避免跨模块耦合。
pub(crate) const SNORM_MIN_COHORT: usize = 3;

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

/// "近期贡献环"里的一条记录:某次 assign 调用真正更新了某簇质心/计数/时长,
/// 记下逆更新所需的最小信息。回声追溯撤回(session.rs EchoRetract)命中时,
/// 凭 seg_key 找到这条记录并调用 retract_contribution 冲抵。
struct Contribution {
    /// 调用方给的稳定段标识(session.rs 用 "{source}:{start_ms}")。
    seg_key: String,
    cluster_id: String,
    /// 该次 assign 用于更新质心的单位向量(assign 内部算出的 `unit`)。
    embedding: Vec<f32>,
    /// 该次贡献计入 total_ms 的增量。
    ms: u64,
}

/// 库里的一个种子人物:注入 registry 供本场优先命中,免得同一人在新会话里
/// 从零建簇、需要用户重新点名。
pub struct SeedCluster {
    pub person: String,
    pub name: String,
    pub centroid: Vec<f32>,
    pub count: u64,
    /// 该质心来自的信道(如 "mic"/"system")。跨信道种子只走归一化通道
    /// (Task 2)的前提:注入后要知道自己质心的信道身份。
    pub source: String,
}

/// —— 种子匹配策略(说话人识别方法)——
///
/// 探针对全部库种子的一次判定被抽象为「席位表 → 选席」:一个席位 = 某人某份
/// 质心(主质心/会话变体)对探针的裸相似度 + 是否已过命中三闸(段长/同信道
/// 快路/跨信道 z 通道)。门槛判定始终在 registry/调用方,策略只做排名与选择
/// ——新增第三种识别方法时只需一个 impl 并在 matcher_from_key 注册,
/// settings.speaker_match 即可选到。
pub struct SeedSeat<'a> {
    pub person: &'a str,
    pub sim: f32,
    /// 已过命中三闸。claim 返回的席位必须 eligible(认领=建立库关联,不合格
    /// 不许认);reference 不受此约束。
    pub eligible: bool,
}

pub trait SeedMatcher: Send {
    /// 认领席位下标:策略在合格性约束下选出可认领的席位;None = 不认领。
    fn claim(&self, seats: &[SeedSeat<'_>]) -> Option<usize>;
    /// 参考近邻下标(无合格性约束):Aing 簇统计据此记录「最佳库近邻」供
    /// identify 裁决层参考,即使未达认领门槛也要记录。
    fn reference(&self, seats: &[SeedSeat<'_>]) -> Option<usize>;
}

/// 单最近邻(默认):合格席位中取最近。2026-08-12 离线评测(真实库 334 探针
/// 留一)出手准确率 95.5%/召回 95.2%,显著优于多数票——库内每人席位少
/// (中位 2~3 席),票决邻域多为不同人各一席,多数票反而压掉正确最近邻。
pub struct NearestMatcher;

impl SeedMatcher for NearestMatcher {
    // 严格大于 + 保先到:与 PR#94 之前的选择循环逐位一致——库里存在完全相同的
    // 质心(同声重复入库)时平手,历史行为取先注入者,max_by 会取后者(实测
    // P14/P342 同分翻案,评测互验抓到),故不用 max_by。
    fn claim(&self, seats: &[SeedSeat<'_>]) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, s) in seats.iter().enumerate() {
            if s.eligible && best.is_none_or(|b: usize| s.sim > seats[b].sim) {
                best = Some(i);
            }
        }
        best
    }
    fn reference(&self, seats: &[SeedSeat<'_>]) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, s) in seats.iter().enumerate() {
            if best.is_none_or(|b: usize| s.sim > seats[b].sim) {
                best = Some(i);
            }
        }
        best
    }
}

/// top-K 多数票(PR#94,实验项;每人样本足够密时理论上抗单席离群劫持):最近
/// SEED_KNN_K 席按 person 计席,席位最多者胜出(平票取最高分席位所属的人,
/// 同分先到先占),认领还须胜者有合格席位——胜者不过阈不降格给第二名
/// (误命名比不命名糟)。
pub struct KnnVoteMatcher;

impl KnnVoteMatcher {
    /// 票决胜者及其最高分席位下标(无合格性约束)。
    fn winner_best(seats: &[SeedSeat<'_>]) -> Option<usize> {
        // 有界 top-K 选择(O(S·K));>= 同分后插,先到的同分席位不被挤出。
        let mut neighbors: Vec<usize> = Vec::with_capacity(SEED_KNN_K + 1);
        for (i, s) in seats.iter().enumerate() {
            let pos = neighbors.partition_point(|&j| seats[j].sim >= s.sim);
            if pos < SEED_KNN_K {
                neighbors.insert(pos, i);
                neighbors.truncate(SEED_KNN_K);
            }
        }
        // (席位数, 最高分, 最高分席位, 最早席位)。最早席位参与终局裁决:席位数
        // 与最高分都相等的跨人全平票保先注入者——BTreeMap 迭代按 person id 序,
        // 单靠 max_by 会取字典序最大的 id(codex 终审 P2)。
        let mut tally: std::collections::BTreeMap<&str, (usize, f32, usize, usize)> =
            std::collections::BTreeMap::new();
        for &i in &neighbors {
            let e = tally.entry(seats[i].person).or_insert((0, f32::MIN, i, i));
            e.0 += 1;
            if seats[i].sim > e.1 {
                e.1 = seats[i].sim;
                e.2 = i;
            }
            e.3 = e.3.min(i);
        }
        tally
            .into_values()
            .max_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)).then(b.3.cmp(&a.3)))
            .map(|(_, _, i, _)| i)
    }
}

impl SeedMatcher for KnnVoteMatcher {
    fn claim(&self, seats: &[SeedSeat<'_>]) -> Option<usize> {
        let winner = seats[Self::winner_best(seats)?].person;
        // 胜者在票决邻域内的最高分合格席位;重算邻域成本可忽略(K=5)。
        let mut neighbors: Vec<usize> = Vec::with_capacity(SEED_KNN_K + 1);
        for (i, s) in seats.iter().enumerate() {
            let pos = neighbors.partition_point(|&j| seats[j].sim >= s.sim);
            if pos < SEED_KNN_K {
                neighbors.insert(pos, i);
                neighbors.truncate(SEED_KNN_K);
            }
        }
        // 严格大于保先到:胜者多个同分合格席位时取先注入者,与本策略其余
        // 同分保序语义一致(max_by 平手取后者,codex P2)。
        let mut best: Option<usize> = None;
        for i in neighbors {
            if seats[i].person == winner
                && seats[i].eligible
                && best.is_none_or(|b: usize| seats[i].sim > seats[b].sim)
            {
                best = Some(i);
            }
        }
        best
    }
    fn reference(&self, seats: &[SeedSeat<'_>]) -> Option<usize> {
        Self::winner_best(seats)
    }
}

/// settings.speaker_match → 策略实例。未知/脏值回落默认最近邻——配置不该
/// 挡识别,与 settings 读失败回落默认的纪律一致。
pub fn matcher_from_key(key: &str) -> Box<dyn SeedMatcher> {
    match key {
        SPEAKER_MATCH_KNN_VOTE => Box::new(KnnVoteMatcher),
        _ => Box::new(NearestMatcher),
    }
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
    /// 种子质心来自的信道(如 "mic"/"system")。只在库种子注入路径(with_seeds
    /// 的 seeds 参数)设置；快照恢复的簇(续录/跨场)恢复无信道保证，保守起见留
    /// None，语义由 Task 2 处理。供后续任务判断"跨信道种子只走归一化通道"。
    seed_source: Option<String>,
    /// 该种子的 AS-Norm 种子侧 cohort 统计 (均值 μb, 样本标准差 σb)：库中其他每个
    /// 人物的种子对本种子质心的最高余弦，取这些"每人一分"的均值/标准差(σ 夹到
    /// 1e-3 下限，防止除零)。其他人物数 < SNORM_MIN_COHORT 时统计不稳定，记 None。
    /// 只在库种子注入路径预计算一次(O(种子²·维度)，with_seeds 内一次性代价)；
    /// 快照恢复的簇同样保守留 None，语义由 Task 2 处理。
    seed_cohort: Option<(f32, f32)>,
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

/// 匹配决策日志条目(2026-08-23 数据积累,issue #163):只记两类有信息量的时刻——
/// 新簇诞生(为什么没人认领:附当时种子人物 top3 裸分)与种子簇首次被真实段命中
/// (谁赢、赢了多少)。量级与簇数同阶,停录随 DiarEvent::MatchLog 导出。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MatchTrace {
    /// "new_cluster" | "seed_adopt"
    pub event: String,
    pub cluster: String,
    pub source: String,
    /// 段身份("mic:12345",assign 无时间轴,借 seg_key 定位;无则空)。
    pub seg_key: String,
    /// seed_adopt:命中的人物与分数。
    pub person: Option<String>,
    pub sim: Option<f32>,
    /// 决策当时的种子人物 top3(person, 裸分)。
    pub top: Vec<(String, f32)>,
}

/// 匹配日志容量上限:防极端碎片化场刷爆内存。
const MATCH_TRACE_CAP: usize = 500;

pub struct SpeakerRegistry {
    clusters: Vec<Cluster>,
    next_id: u32,
    assigns: u64,
    pending_merges: Vec<(String, String)>,
    /// (够料门槛 ms, 入库回调)。None = 不做实时入库(测试/库路径不可用)。
    enroller: Option<(u64, EnrollFn)>,
    /// 近期贡献环,见 Contribution 注释。容量 CONTRIBUTION_RING_CAP,超出淘汰最旧。
    contributions: VecDeque<Contribution>,
    /// 三期 mixed 降级口径:关闭种子 z 通道(assign_inner 的 z_hit 整体短路)。
    /// mixed 单轨无信道可分,AS-Norm 的"跨信道归一化"前提不成立(spec §降级口径)。
    seed_z_disabled: bool,
    /// 种子匹配策略(settings.speaker_match):默认单最近邻,set_matcher 切换。
    matcher: Box<dyn SeedMatcher>,
    /// 匹配决策日志(见 MatchTrace)。
    match_trace: Vec<MatchTrace>,
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

/// 扫描侧 AS-Norm 统计:person_max 是本次 assign 扫描中每个种子人物对探针的
/// 最高分,排除 `exclude`(候选人物自身的全部种子)后取剩余"每人一分"的
/// 均值/样本标准差(下限 1e-3)。人数(即其他人物数)< SNORM_MIN_COHORT 时
/// 统计不稳定,返回 None。
fn cohort_stats(person_max: &std::collections::BTreeMap<&str, f32>, exclude: &str) -> Option<(f32, f32)> {
    let scores: Vec<f32> = person_max.iter().filter(|(p, _)| **p != exclude).map(|(_, s)| *s).collect();
    if scores.len() < SNORM_MIN_COHORT {
        return None;
    }
    let mean = scores.iter().sum::<f32>() / scores.len() as f32;
    let var = scores.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / (scores.len() - 1) as f32;
    Some((mean, var.sqrt().max(1e-3)))
}

/// 跨信道种子命中走的 AS-Norm 对称 z(与整理层 suggest_merges 同式):扫描侧
/// (μa,σa,本次探针对其他种子人物的最高分统计)与种子侧(μb,σb,cluster.seed_cohort,
/// with_seeds 注入时预计算)各出一个 z,取均值。任一侧统计不足(cohort 人数 <
/// SNORM_MIN_COHORT)→ 通道整体关闭(None),裸分再高也无法核实。
fn seed_z(
    sim: f32,
    person_max: &std::collections::BTreeMap<&str, f32>,
    candidate_person: &str,
    seed_cohort: Option<(f32, f32)>,
) -> Option<f32> {
    let (mu_a, sigma_a) = cohort_stats(person_max, candidate_person)?;
    let (mu_b, sigma_b) = seed_cohort?;
    let za = (sim - mu_a) / sigma_a;
    let zb = (sim - mu_b) / sigma_b;
    Some((za + zb) / 2.0)
}

impl Default for SpeakerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SpeakerRegistry {
    pub fn new() -> Self {
        Self {
            clusters: Vec::new(),
            next_id: 1,
            assigns: 0,
            pending_merges: Vec::new(),
            enroller: None,
            contributions: VecDeque::new(),
            seed_z_disabled: false,
            matcher: Box::new(NearestMatcher),
            match_trace: Vec::new(),
        }
    }

    /// 取走匹配决策日志(停录导出;取一次即清)。
    pub fn take_match_trace(&mut self) -> Vec<MatchTrace> {
        std::mem::take(&mut self.match_trace)
    }

    /// 装配会话中实时入库回调(lib.rs 在 with_seeds 之后调用)。
    pub fn set_enroller(&mut self, min_ms: u64, f: EnrollFn) {
        self.enroller = Some((min_ms, f));
    }

    /// 切换种子匹配策略(settings.speaker_match → matcher_from_key)。
    pub fn set_matcher(&mut self, m: Box<dyn SeedMatcher>) {
        self.matcher = m;
    }

    /// 关闭种子 AS-Norm z 通道(mixed 重转写用;默认开启,实时链路不受影响)。
    pub fn disable_seed_z(&mut self) {
        self.seed_z_disabled = true;
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
    /// 不记录"近期贡献"(不可撤回)——调用方需要事后追溯撤回时改用 assign_tracked。
    pub fn assign(&mut self, embedding: &[f32], source: &str, num_samples: usize) -> Option<String> {
        self.assign_inner(embedding, source, num_samples, None)
    }

    /// 同 assign,额外记一条"近期贡献"(seg_key 标识该次调用,见 Contribution 注释),
    /// 供事后 retract_contribution 撤回。session.rs 的 mic 段回声追溯撤回场景使用;
    /// 其余调用方(测试等)用普通 assign 即可,不需要为不会被撤回的段占用环容量。
    pub fn assign_tracked(&mut self, embedding: &[f32], source: &str, num_samples: usize, seg_key: &str) -> Option<String> {
        self.assign_inner(embedding, source, num_samples, Some(seg_key))
    }

    fn assign_inner(&mut self, embedding: &[f32], source: &str, num_samples: usize, seg_key: Option<&str>) -> Option<String> {
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

        // 一遍扫描:先把每个簇对本次探针的裸分算好,顺带按 person 收集"扫描侧"
        // cohort(每个种子人物对本次探针的最高分),供跨信道 z 通道的 μa/σa 复用——
        // 判定阶段只读这份已算好的分数,不重复算点积(热路径不做双重扫描)。
        let sims: Vec<f32> = self.clusters.iter().map(|c| dot(&c.centroid, &unit)).collect();
        let mut person_max: std::collections::BTreeMap<&str, f32> = std::collections::BTreeMap::new();
        for (c, &sim) in self.clusters.iter().zip(&sims) {
            if c.seed_source.is_some() {
                if let Some(person) = c.person.as_deref() {
                    person_max.entry(person).and_modify(|m| if sim > *m { *m = sim }).or_insert(sim);
                }
            }
        }
        // 匹配日志用的 owned top3(person_max 借着 clusters,后面的可变借用前先物化)。
        let trace_top3: Vec<(String, f32)> = {
            let mut v: Vec<(String, f32)> =
                person_max.iter().map(|(p, s)| (p.to_string(), *s)).collect();
            v.sort_by(|a, b| b.1.total_cmp(&a.1));
            v.truncate(3);
            v
        };

        // 种子命中三闸(闭包供 k-NN 票决后的资格审查调用,判定逻辑与旧单最近邻
        // 逐位一致):①段长 ≥ SEED_MIN_SAMPLES 才有资格拍板;②同信道走裸分快路
        // (阈值不变);③跨信道只走 AS-Norm z 通道——裸余弦跨信道不可比,mic 段撞
        // system 质心分数系统性走低/走高都不可信,归一化后才有资格认领。
        // 惰性化:cohort_stats/seed_z 各带一次 Vec 分配,只在真正可能走到 z 通道时
        // 才算——快路已命中或不够资格或裸分够不着地板时,z 值本就无关判定结果。
        let seed_z_disabled = self.seed_z_disabled;
        let seed_hit = |c: &Cluster, sim: f32| -> bool {
            match c.seed_source.as_deref() {
                Some(seed_src) => {
                    let same_channel = seed_src == source;
                    let seed_eligible = num_samples >= SEED_MIN_SAMPLES;
                    let fast_hit = same_channel && sim >= SEED_ASSIGN_THRESHOLD;
                    let z_hit = !seed_z_disabled
                        && seed_eligible
                        && !fast_hit
                        && sim >= SEED_ASSIGN_RAW_FLOOR
                        && c.person.as_deref().is_some_and(|p| {
                            matches!(
                                seed_z(sim, &person_max, p, c.seed_cohort),
                                Some(z) if z >= SEED_ASSIGN_Z
                            )
                        });
                    seed_eligible && (fast_hit || z_hit)
                }
                // 续录恢复簇信道未知,维持原语义,待快照带信道后收紧:裸
                // cos ≥ SEED_ASSIGN_THRESHOLD 即命中,不分信道、无 z 通道、
                // 也不受三闸①的短段门槛约束。
                None => sim >= SEED_ASSIGN_THRESHOLD,
            }
        };

        // 普通簇(本场实时聚出):维持单最近邻 + ASSIGN_THRESHOLD。种子的更高门槛
        // 只在种子路生效——若全局最相似是"够不着的种子簇",不得挡住本可命中的
        // 普通簇(否则会话内簇碎片化)。
        let mut best_regular: Option<(f32, usize)> = None;
        for (idx, (c, &sim)) in self.clusters.iter().zip(&sims).enumerate() {
            if !c.is_seed() && sim >= ASSIGN_THRESHOLD && best_regular.is_none_or(|(bs, _)| sim > bs) {
                best_regular = Some((sim, idx));
            }
        }

        // 种子簇(库中采集来的声纹,seed_source 有值):席位表交给可插拔匹配策略
        // (settings.speaker_match,默认单最近邻;策略语义见 SeedMatcher 各实现的
        // 文档注释)。席位顺序与簇注入顺序一致,策略的同分保序依赖它。
        // 续录快照恢复的簇(seed_source=None)不进票池(codex 终审 P1):with_seeds
        // 已压掉快照 person 的库种子,该人恒只有 1 席,票决下 cos 1.0 的精确续录
        // 匹配会被别人的多席变体投翻——快照簇恒走最近邻通道(其命中语义本就是
        // 裸阈值单席,与策略无关),与策略认领比分取高者。
        let mut seat_cluster: Vec<usize> = Vec::new();
        let mut seats: Vec<SeedSeat<'_>> = Vec::new();
        let mut best_snapshot: Option<(f32, usize)> = None;
        for (idx, (c, &sim)) in self.clusters.iter().zip(&sims).enumerate() {
            if c.person.is_none() || !c.is_seed() {
                continue;
            }
            if c.seed_source.is_none() {
                if seed_hit(c, sim)
                    && best_snapshot.is_none_or(|(bs, _): (f32, usize)| sim > bs)
                {
                    best_snapshot = Some((sim, idx));
                }
                continue;
            }
            seat_cluster.push(idx);
            seats.push(SeedSeat { person: c.person.as_deref().unwrap(), sim, eligible: seed_hit(c, sim) });
        }
        let strategy_claim: Option<(f32, usize)> =
            self.matcher.claim(&seats).map(|i| (seats[i].sim, seat_cluster[i]));

        // 候选合并:比分取高,同分保簇注入序靠前者(快照先于种子、种子先于普通簇,
        // 与旧单遍扫描"严格大于保先"逐位一致,codex 终审 P2)。
        let pick = |a: Option<(f32, usize)>, b: Option<(f32, usize)>| match (a, b) {
            (Some(x), Some(y)) => Some(if y.0 > x.0 || (y.0 == x.0 && y.1 < x.1) { y } else { x }),
            (x, y) => x.or(y),
        };
        let best_seed = pick(strategy_claim, best_snapshot);
        let best_idx = pick(best_regular, best_seed);
        let best = best_idx.map(|(sim, idx)| (sim, &mut self.clusters[idx]));

        if let Some((sim, cluster)) = best {
            // 匹配日志:种子簇首次被真实段命中 = 本场认领了这个库人物。
            if cluster.is_seed() && cluster.sources.is_empty() && self.match_trace.len() < MATCH_TRACE_CAP
            {
                self.match_trace.push(MatchTrace {
                    event: "seed_adopt".into(),
                    cluster: cluster.id.clone(),
                    source: source.to_string(),
                    seg_key: seg_key.unwrap_or("").to_string(),
                    person: cluster.person.clone(),
                    sim: Some(sim),
                    top: trace_top3.clone(),
                });
            }
            cluster.sources.insert(source.to_string());
            // 短段不更新质心、不增count(短段声纹噪声大,防拖歪质心)
            let mut contribution: Option<(String, u64)> = None;
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
                let ms = (num_samples / 16) as u64;
                cluster.total_ms += ms;
                contribution = Some((cluster.id.clone(), ms));
            }
            let id = cluster.id.clone();
            if let (Some(key), Some((cluster_id, ms))) = (seg_key, contribution) {
                self.record_contribution(key, cluster_id, unit, ms);
            }
            return Some(id);
        }

        // 软归属:严格阈值未命中,但与某个**普通簇**(非种子)的相似度落在灰区
        // [SOFT, ASSIGN) → 归入该簇打标签,不更新质心/计数/时长(弱证据不留痕:
        // 不拖质心、不计入自动入库时长)。种子簇除外——弱证据错认领人名比碎片化更糟。
        // 未真正更新质心,不记贡献:retract_contribution 对这类 seg_key 天然是 no-op。
        // 复用扫描阶段已算好的 sims,不重新点积。
        let mut soft_idx: Option<(f32, usize)> = None;
        for (idx, (c, &sim)) in self.clusters.iter().zip(&sims).enumerate() {
            if c.is_seed() {
                continue;
            }
            if sim >= SOFT_ASSIGN_THRESHOLD && soft_idx.is_none_or(|(bs, _)| sim > bs) {
                soft_idx = Some((sim, idx));
            }
        }
        let soft = soft_idx.map(|(sim, idx)| (sim, &mut self.clusters[idx]));
        if let Some((_sim, cluster)) = soft {
            cluster.sources.insert(source.to_string());
            return Some(cluster.id.clone());
        }

        if num_samples < MIN_NEW_CLUSTER_SAMPLES {
            return None; // 短段不建簇(也够不着任何软归属 → 留空,Aing 兜底)
        }
        let id = format!("S{}", self.next_id);
        self.next_id += 1;
        // 匹配日志:新簇诞生 = 没有任何人认领这段声音;top3 记下"差多远",
        // 灰区近失(如 0.6x)正是阈值校准与分身死循环诊断的关键证据。
        if self.match_trace.len() < MATCH_TRACE_CAP {
            self.match_trace.push(MatchTrace {
                event: "new_cluster".into(),
                cluster: id.clone(),
                source: source.to_string(),
                seg_key: seg_key.unwrap_or("").to_string(),
                person: None,
                sim: None,
                top: trace_top3,
            });
        }
        let ms = (num_samples / 16) as u64;
        let ring_embedding = seg_key.map(|_| unit.clone());
        self.clusters.push(Cluster {
            id: id.clone(),
            centroid: unit,
            count: 1,
            sources: BTreeSet::from([source.to_string()]),
            person: None,
            person_name: None,
            // 建簇本身要求 num_samples >= MIN_NEW_CLUSTER_SAMPLES(足够长的段),
            // 首个成员的时长直接计入,与既有簇长段累加同一口径。
            total_ms: ms,
            // 会话内新建的普通簇没有"历史基数"，count 从 0 开始就是纯增量。
            seed_base_count: 0,
            reported_ms: 0,
            session_enrolled: false,
            // 本场刚聚出来的普通簇,不是库种子,无信道/cohort 语义。
            seed_source: None,
            seed_cohort: None,
        });
        // 新建簇同样是"真正更新了质心"(从无到有):记为贡献,撤回时若这是簇内
        // 唯一成员,count 落到 0 且无 person → 整簇连带撤销(见 retract_contribution)。
        if let (Some(key), Some(emb)) = (seg_key, ring_embedding) {
            self.record_contribution(key, id.clone(), emb, ms);
        }
        Some(id)
    }

    /// 把一条"近期贡献"记入环,超容量淘汰最旧(见 CONTRIBUTION_RING_CAP)。
    fn record_contribution(&mut self, seg_key: &str, cluster_id: String, embedding: Vec<f32>, ms: u64) {
        self.contributions.push_back(Contribution { seg_key: seg_key.to_string(), cluster_id, embedding, ms });
        while self.contributions.len() > CONTRIBUTION_RING_CAP {
            self.contributions.pop_front();
        }
    }

    /// 撤回一次此前经 assign_tracked 记录的贡献:近似逆更新质心
    /// `c' = normalize(c*count - e)`——running mean 每步都重新归一化,`c*count`
    /// 并非精确的和向量(丢了历史范数),但簇内向量高度相似时 `‖sum‖ ≈ count`
    /// (对齐的单位向量之和,模长趋近个数),误差可忽略(校准记录:两段同簇场景
    /// 下与"仅保留另一段"的真实质心余弦 > 0.99)。
    /// count/total_ms 相应回退(下限 0);簇回退到 0 且无 person → 整簇移除
    /// (含从未真正认领的会话内簇撤销),有 person 则保留(库层净增量自然缩小)。
    /// 找不到该 seg_key(未记录 / 已被环淘汰)或对应簇已不存在(已被合并/移除
    /// 后环内条目未及清理)时是 no-op,返回 false——回声撤回是尽力而为，不panic。
    pub fn retract_contribution(&mut self, seg_key: &str) -> bool {
        let Some(pos) = self.contributions.iter().position(|c| c.seg_key == seg_key) else {
            return false;
        };
        let contrib = self.contributions.remove(pos).expect("pos 刚定位到");
        let Some(idx) = self.clusters.iter().position(|c| c.id == contrib.cluster_id) else {
            return false;
        };
        let old_count = self.clusters[idx].count;
        let new_count = old_count.saturating_sub(1);
        self.clusters[idx].total_ms = self.clusters[idx].total_ms.saturating_sub(contrib.ms);

        if new_count == 0 && self.clusters[idx].person.is_none() {
            let removed = self.clusters.remove(idx);
            // 该簇已死,环里其余指向它的条目(若有)也失去回退目标,一并丢弃。
            self.contributions.retain(|c| c.cluster_id != removed.id);
            return true;
        }

        let n = old_count as f32;
        let cluster = &mut self.clusters[idx];
        let mut sum: Vec<f32> = cluster.centroid.iter().map(|x| x * n).collect();
        for (s, e) in sum.iter_mut().zip(contrib.embedding.iter()) {
            *s -= e;
        }
        if let Some(renorm) = normalize(&sum) {
            cluster.centroid = renorm;
        }
        cluster.count = new_count;
        true
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
                    // 终审 F1:双方都是"跨会话种子"(seed_source 非 None)的配对直接跳过,
                    // 不参与合并——with_seeds 给同一 person 的主质心+每条会话变体各建
                    // 一个种子簇,匹配语义本就取 max(assign_inner 逐簇比对,不要求
                    // "唯一命中"),同人变体互相 cos ≥0.68 是常态,互并对识别零收益;
                    // 反而会毁掉信道结构与 cohort 统计——互并后下面的降级规则会把
                    // winner 的 seed_source/seed_cohort 清空,该人退化为"恢复簇"语义
                    // (裸 0.68 跨信道复活、丢失短段闸①),person_max 计数还可能因种子
                    // 减少而跌破 SNORM_MIN_COHORT,连累其他人物的 z 通道被关掉——三闸
                    // 对多数真实人物形同未做(冒烟实锤:同人两 mic 种子链式合并数条
                    // final 内吃光多数种子)。不同 person 的种子簇本就被下面 person
                    // 不等分支挡住;无主会话簇 ↔ 种子簇的合并(死区修复,F3a 见下)
                    // 不受影响,只有 j 循环里非种子一侧 seed_source 为 None 时才会走到
                    // 下面的判定。放在点积计算之前是 O(1) 早退,顺带砍掉种子↔种子的
                    // 每次 final O(S²·D) 扫描(种子多时这条本身就是显著开销)。
                    if a.seed_source.is_some() && b.seed_source.is_some() {
                        continue;
                    }
                    // 合并门槛按配对身份分档:
                    // - 不同 person 禁止自动互并("库里两人实为一人"只能用户在管理页显式合并);
                    // - 无主簇(person=None)↔ 有 person 一侧:
                    //   · 若有 person 一侧不是种子(本场实时入库的普通簇)→ 维持 MERGE_THRESHOLD,
                    //     与"尚未入库"时行为一致;
                    //   · 若是种子且该种子无信道记录(seed_source=None,即续录恢复簇,信道
                    //     未知)→ 维持旧语义,裸 SEED_ASSIGN_THRESHOLD(0.68)不分信道,
                    //     待快照带信道后收紧;
                    //   · 若是种子且带信道记录:同信道维持 SEED_ASSIGN_THRESHOLD(0.68,
                    //     开场短段声纹噪声大够不着种子门槛时会另立无主簇,若其质心本可
                    //     归簇命中种子就该并回去,否则卡在 [0.68,0.74) 死区裂人——冒烟
                    //     实锤:同人双簇余弦 0.711 永远等不到 0.74);跨信道抬到
                    //     MERGE_THRESHOLD(终审 F3a——assign 路刚关掉的 mic 撞 system 裸
                    //     0.68 不能从合并路走回来;0.74 与场内簇间合并同档,待评测集校准);
                    // - 同 person 双簇(必是"无主簇后来追认成同一 person"与种子/入库簇配对,
                    //   seed↔seed 已被上面挡掉)沿用 SEED_ASSIGN_THRESHOLD,信道判定留给
                    //   assign_inner 的三闸,这里不重复;
                    // - 无主 ↔ 无主维持 MERGE_THRESHOLD(0.74 为首轮真实会议校准,防过度合并)。
                    let pair_threshold = match (&a.person, &b.person) {
                        (Some(x), Some(y)) if x != y => continue,
                        (Some(_), None) | (None, Some(_)) => {
                            let (masterless, other) = if a.person.is_none() { (a, b) } else { (b, a) };
                            if other.is_seed() {
                                match &other.seed_source {
                                    Some(src) if !masterless.sources.contains(src) => MERGE_THRESHOLD,
                                    _ => SEED_ASSIGN_THRESHOLD,
                                }
                            } else {
                                MERGE_THRESHOLD
                            }
                        }
                        (Some(_), Some(_)) => {
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
            // T1 交接项(终审 F1 后收窄):seed↔seed 的配对已被上面的 continue 挡在
            // detect_merges 之外,能走到这里、且任一侧 seed_source 非 None 的合并
            // 只剩"非种子簇(无主簇,或本场追认成同一 person 的入库簇)↔ 种子簇"这
            // 一种——混合后的质心已不再单纯属于种子那侧的信道,winner 的信道身份与
            // AS-Norm 统计随之作废,退化为"恢复簇"语义(seed_source=None,不分信道
            // 走裸分;seed_cohort 同步清空,注释见 assign_inner 的三闸判定)。这份
            // 降级本身仍然成立,予以保留。
            if winner.seed_source.is_some() || loser.seed_source.is_some() {
                winner.seed_source = None;
                winner.seed_cohort = None;
            }
            // winner 无 person 而 loser 有 → 继承(person 冲突的情形已被上面的检查挡掉)；
            // session_enrolled 随 person 一起继承，阈值档位跟着身份走。
            if winner.person.is_none() {
                winner.person = loser.person.clone();
                winner.person_name = loser.person_name.clone();
                winner.session_enrolled = loser.session_enrolled;
            }
            // 环里指向 loser 的贡献重映射到 winner:loser 簇本身已被摘掉,这些
            // 条目若不跟着改指向,日后撤回会因"找不到簇"而静默失效(见
            // retract_contribution 的簇缺失分支)——貌似安全但会让本可撤销的
            // 贡献提前失效,故这里主动重映射而非留给缺失分支兜底。
            let winner_id = winner.id.clone();
            for c in self.contributions.iter_mut() {
                if c.cluster_id == loser.id {
                    c.cluster_id = winner_id.clone();
                }
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
                    // 跨场恢复无信道保证,语义由 Task 2 处理:保守起见留 None,
                    // 不参与"跨信道种子走归一化通道"与 cohort 判定。
                    seed_source: None,
                    seed_cohort: None,
                });
            }
        }
        Self {
            clusters,
            next_id,
            assigns: 0,
            pending_merges: Vec::new(),
            enroller: None,
            contributions: VecDeque::new(),
            seed_z_disabled: false,
            matcher: Box::new(NearestMatcher),
            match_trace: Vec::new(),
        }
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
                // 跨信道种子只走归一化通道(Task 2)的前提:记下该质心的信道身份。
                seed_source: Some(s.source.clone()),
                // 下面统一预计算(precompute_seed_cohorts),此处先占位。
                seed_cohort: None,
            });
        }
        r.precompute_seed_cohorts(seeds);
        r
    }

    /// 为本次库种子注入预计算 AS-Norm 种子侧 cohort 统计:对每个刚注入的种子簇 i
    /// (seed_source 非 None),按 person 归组算「其他每个人物的种子对 i.centroid
    /// 的最高余弦」，得每人一个分；样本数(即其他人物数)< SNORM_MIN_COHORT → None，
    /// 否则 (均值, 样本标准差.max(1e-3))。快照恢复的簇(seed_source=None)不计算，
    /// 保持 None(跨场恢复无信道保证,语义由 Task 2 处理)。
    ///
    /// 一次性 O(种子²·维度)代价,发生在 with_seeds(会话开录/建种子时),不在
    /// assign 热路径上——之后每次 assign 只读取预计算好的 seed_cohort。
    fn precompute_seed_cohorts(&mut self, seeds: &[SeedCluster]) {
        // 按 person 归组的归一化质心(跳过零向量/非法种子),供下面逐簇取"每人最高分"复用。
        let mut by_person: std::collections::BTreeMap<&str, Vec<Vec<f32>>> = std::collections::BTreeMap::new();
        for s in seeds {
            if let Some(u) = normalize(&s.centroid) {
                by_person.entry(s.person.as_str()).or_default().push(u);
            }
        }
        for c in self.clusters.iter_mut() {
            if c.seed_source.is_none() {
                continue; // 只为本次真正注入的种子簇计算
            }
            let Some(person) = c.person.as_deref() else { continue };
            let scores: Vec<f32> = by_person
                .iter()
                .filter(|(p, _)| **p != person)
                .map(|(_, centroids)| {
                    centroids.iter().map(|v| dot(v, &c.centroid)).fold(f32::NEG_INFINITY, f32::max)
                })
                .collect();
            c.seed_cohort = if scores.len() < SNORM_MIN_COHORT {
                None
            } else {
                let mean = scores.iter().sum::<f32>() / scores.len() as f32;
                let var =
                    scores.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / (scores.len() - 1) as f32;
                Some((mean, var.sqrt().max(1e-3)))
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 匹配决策日志(issue #163):新簇诞生与种子首次认领各记一条(带 top 候选),
    /// 取走即清。
    #[test]
    fn match_trace_records_new_cluster_and_seed_adopt() {
        let mut r = SpeakerRegistry::with_seeds(&[], &[SeedCluster {
            person: "P1".into(),
            name: "甲".into(),
            centroid: vec![1.0, 0.0, 0.0],
            count: 5,
            source: "mic".into(),
        }]);
        // 与种子高相似 + 段长过闸 → seed_adopt
        let hit = r.assign(&[1.0, 0.01, 0.0], "mic", 16_000 * 5);
        assert!(hit.is_some());
        // 与谁都不像 → new_cluster(top 里躺着 P1 的低分作近失证据)
        let miss = r.assign(&[0.0, 1.0, 0.0], "mic", 16_000 * 5);
        assert!(miss.is_some());
        let trace = r.take_match_trace();
        let events: Vec<&str> = trace.iter().map(|t| t.event.as_str()).collect();
        assert!(events.contains(&"seed_adopt"), "{events:?}");
        assert!(events.contains(&"new_cluster"), "{events:?}");
        let adopt = trace.iter().find(|t| t.event == "seed_adopt").unwrap();
        assert_eq!(adopt.person.as_deref(), Some("P1"));
        assert!(adopt.sim.unwrap() > 0.9);
        let nc = trace.iter().find(|t| t.event == "new_cluster").unwrap();
        assert_eq!(nc.top.first().map(|(p, _)| p.as_str()), Some("P1"), "近失证据在 top 里");
        assert!(r.take_match_trace().is_empty(), "取走即清");
    }


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
            source: "mic".into(),
        }];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        // 与种子余弦 0.5(灰区):不得软归属领走人名 → 长段落地为新普通簇
        let y = (1.0f32 - 0.25).sqrt();
        let id = r.assign(&v(0.5, y, 0.0), "mic", LONG).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person, None, "灰区弱证据不得认领种子人物");
    }

    /// Task 1:with_seeds 注入后按"对其他每个人物的种子取 i.centroid 的最高 cos"
    /// 预计算 AS-Norm 种子侧 cohort 统计;人物数(不含自己)< SNORM_MIN_COHORT(3)
    /// 时关断为 None。判定逻辑本身不变——本测试只锁死管道贯通,不锁死后续任务的
    /// 消费方式。
    #[test]
    fn with_seeds_precomputes_cohort_stats_per_seed() {
        // 4 个人物(其他人数=3,达门槛)→ 每个种子簇的 seed_cohort 都应是 Some。
        let seeds4 = vec![
            SeedCluster { person: "P1".into(), name: "甲".into(), centroid: vec![1.0, 0.0, 0.0, 0.0], count: 5, source: "mic".into() },
            SeedCluster { person: "P2".into(), name: "乙".into(), centroid: vec![0.0, 1.0, 0.0, 0.0], count: 5, source: "mic".into() },
            SeedCluster { person: "P3".into(), name: "丙".into(), centroid: vec![0.0, 0.0, 1.0, 0.0], count: 5, source: "system".into() },
            SeedCluster { person: "P4".into(), name: "丁".into(), centroid: vec![0.0, 0.0, 0.0, 1.0], count: 5, source: "system".into() },
        ];
        let r = SpeakerRegistry::with_seeds(&[], &seeds4);
        assert_eq!(r.clusters.len(), 4);
        for c in &r.clusters {
            let (_, sigma) = c.seed_cohort.expect("4 人物(3 个其他人)应达 cohort 门槛");
            assert!(sigma >= 1e-3, "样本标准差应夹到下限 1e-3,实际 {sigma}");
        }

        // 只有 2 个人物(1 个其他人)→ 未达门槛,全部 None。
        let seeds2 = vec![
            SeedCluster { person: "P1".into(), name: "甲".into(), centroid: vec![1.0, 0.0, 0.0, 0.0], count: 5, source: "mic".into() },
            SeedCluster { person: "P2".into(), name: "乙".into(), centroid: vec![0.0, 1.0, 0.0, 0.0], count: 5, source: "mic".into() },
        ];
        let r2 = SpeakerRegistry::with_seeds(&[], &seeds2);
        for c in &r2.clusters {
            assert!(c.seed_cohort.is_none(), "只有 1 个其他人物,未达门槛应为 None");
        }
    }

    /// mixed 降级口径(三期 spec):disable_seed_z 后,z 通道对种子簇整体关闭——
    /// 裸分落在 [RAW_FLOOR, SEED_ASSIGN) 灰区的段不再可能经 z 命中种子;
    /// 0.68 同信道快路不受影响。
    #[test]
    fn disable_seed_z_closes_z_channel_but_keeps_fast_path() {
        // 4 个种子人物凑足 SNORM_MIN_COHORT,cohort 统计可用,z 通道原本开着。
        // 主种子方向 e0;干扰种子方向远离(e1/e2/e3),对探针余弦≈0 → z 值巨大,
        // 未关开关时灰区裸分必经 z 命中。
        let dim = 8;
        let unit = |i: usize| { let mut v = vec![0.0f32; dim]; v[i] = 1.0; v };
        let seeds: Vec<SeedCluster> = (0..4).map(|i| SeedCluster {
            person: format!("P{i}"), name: format!("人{i}"),
            centroid: unit(i), count: 10, source: "mic".into(),
        }).collect();
        // 探针:与 P0 余弦 0.6(灰区:≥0.50 地板、<0.68 快路)
        let mut probe = vec![0.0f32; dim];
        probe[0] = 0.6; probe[4] = 0.8; // 0.6²+0.8²=1,单位向量
        let long = SEED_MIN_SAMPLES; // 过三闸①的段长

        let mut open = SpeakerRegistry::with_seeds(&[], &seeds);
        let hit_open = open.assign_tracked(&probe, "mic", long, "mic:0");
        let mut closed = SpeakerRegistry::with_seeds(&[], &seeds);
        closed.disable_seed_z();
        let hit_closed = closed.assign_tracked(&probe, "mic", long, "mic:0");

        assert!(hit_open.is_some(), "z 通道开着时灰区高 z 段应命中种子");
        assert!(hit_closed.is_none(), "disable_seed_z 后灰区段不得再经 z 命中");

        // 快路不受影响:裸分 ≥0.68 照常命中
        let mut strong = vec![0.0f32; dim];
        strong[0] = 0.9; strong[4] = (1.0f32 - 0.81).sqrt();
        let mut closed2 = SpeakerRegistry::with_seeds(&[], &seeds);
        closed2.disable_seed_z();
        assert!(closed2.assign_tracked(&strong, "mic", long, "mic:1").is_some());
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
            SeedCluster { person: "P1".into(), name: "张三".into(), centroid: v(1.0, 0.0, 0.0), count: 40, source: "mic".into() },
            SeedCluster { person: "P9".into(), name: "旧人".into(), centroid: v(0.0, 1.0, 0.0), count: 7, source: "mic".into() },
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
        let seeds = vec![SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 10, source: "mic".into() }];
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
        // 无主簇来源与种子同为 "mic"(同信道):终审 F3a 给这类配对加了信道判定,
        // 跨信道抬到 MERGE_THRESHOLD(见 masterless_to_seed_merge_cross_channel_threshold_raised
        // 专项锁测);同信道维持本测试锁死的 SEED_ASSIGN_THRESHOLD(0.68)不变。
        let snap = ClusterSnapshot {
            id: "S9".into(),
            centroid: v(0.71, 0.70413, 0.0), // 与 e1 余弦 ≈ 0.710
            count: 2,
            sources: BTreeSet::from(["mic".to_string()]),
            person: None,
            total_ms: 3000,
        };
        let seeds = vec![SeedCluster {
            person: "P1".into(),
            name: "甲".into(),
            centroid: v(1.0, 0.0, 0.0),
            count: 10,
            source: "mic".into(),
        }];
        let mut r = SpeakerRegistry::with_seeds(&[snap], &seeds);
        assert_eq!(r.take_merges().len(), 1, "同信道下,无主↔有主在 0.71 应按归簇同款阈值合并");
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
            SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 10, source: "mic".into() },
            SeedCluster { person: "P2".into(), name: "乙".into(), centroid: v(0.9805, 0.19612, 0.0), count: 10, source: "mic".into() },
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
            &[SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 1, source: "mic".into() }],
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
        // 中段(1.5s~2.5s,MIN_CENTROID_UPDATE_SAMPLES~MIN_NEW_CLUSTER_SAMPLES):
        // 可更新质心与时长,但无权建簇(见 assign 的双门槛)。
        r.assign(&v(1.0, 0.0, 0.0), "mic", 25600).unwrap(); // 1.6s
        let snap = r.snapshot();
        assert_eq!(snap[0].total_ms, 3000 + 1600);
    }

    /// 终审 triage①锁死判别式:sources 为空 ⇔ 未命中的种子簇。两个种子(甲/乙)注入,
    /// 只对甲做一次命中 assign,乙从未被认领。speakers() 里能看到两个簇(种子铺底
    /// 阶段就已存在),但只有命中的甲 sources 非空——这是 lib.rs/writer.rs 两处过滤
    /// 用来分辨"真实说话人"与"未命中的库种子"的唯一依据,此测试锁死其正确性。
    #[test]
    fn unhit_seed_cluster_has_empty_sources_hit_one_has_nonempty() {
        let seeds = vec![
            SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 10, source: "mic".into() },
            SeedCluster { person: "P2".into(), name: "乙".into(), centroid: v(0.0, 1.0, 0.0), count: 10, source: "mic".into() },
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
        let seeds = vec![SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 40, source: "mic".into() }];
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
        let seeds = vec![SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(0.0, 1.0, 0.0), count: 10, source: "mic".into() }];
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
        let seeds = vec![SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 10, source: "mic".into() }];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds); // S1 = 种子簇 A
        let b_id = r.assign(&v(0.0, 1.0, 0.0), "mic", LONG).unwrap(); // S2 = 会话普通簇 B(与 A 正交)
        assert_eq!(r.speakers().len(), 2);

        let p3 = (1.0f32 - 0.65 * 0.65 - 0.63 * 0.63).sqrt();
        let probe = v(0.65, 0.63, p3);
        let id = r.assign(&probe, "mic", LONG).unwrap();
        assert_eq!(id, b_id, "应命中够得着的普通簇 B,而非被够不着的种子簇 A 挡住去新建簇");
        assert_eq!(r.speakers().len(), 2, "总簇数不变(命中已有簇,未新建)");
    }

    /// 回声追溯撤回①:两段进同簇,撤最后一段 → count/total_ms 回滚到仅剩第一段的
    /// 状态,质心近似逆更新后与剩余那段的嵌入余弦 > 0.99(近似逆更新的校准口径)。
    #[test]
    fn retract_contribution_rolls_back_count_ms_and_centroid() {
        let mut r = SpeakerRegistry::new();
        // segA:建簇,3000ms;segB:同簇内第二段(与 segA 余弦 0.8,够得着 ASSIGN_THRESHOLD,
        // 1.6s ≥ MIN_CENTROID_UPDATE_SAMPLES 新门槛,足以更新质心),1600ms。
        assert_eq!(r.assign_tracked(&v(1.0, 0.0, 0.0), "mic", LONG, "segA"), Some("S1".into()));
        assert_eq!(r.assign_tracked(&v(0.8, 0.6, 0.0), "mic", 25600, "segB"), Some("S1".into()));
        let before = r.snapshot();
        assert_eq!(before[0].count, 2);
        assert_eq!(before[0].total_ms, 3000 + 1600);

        assert!(r.retract_contribution("segB"), "已记录的贡献应能成功撤回");

        let after = r.snapshot();
        assert_eq!(after.len(), 1, "簇仍在(还有 segA 撑着)");
        assert_eq!(after[0].count, 1, "count 应回退到只剩 segA");
        assert_eq!(after[0].total_ms, 3000, "total_ms 应回退到只剩 segA 的时长");
        assert!(
            dot(&after[0].centroid, &[1.0, 0.0, 0.0]) > 0.99,
            "近似逆更新后质心应与剩余的 segA 嵌入高度重合(cos > 0.99),实际 dot={}",
            dot(&after[0].centroid, &[1.0, 0.0, 0.0])
        );

        // 同一 seg_key 只能撤一次:重复调用是 no-op。
        assert!(!r.retract_contribution("segB"), "重复撤回同一 seg_key 应是 no-op");
    }

    /// 回声追溯撤回②:软归属段(灰区,不更新质心/计数)从未被记入贡献环,
    /// 撤回请求天然找不到条目,是 no-op,簇状态原封不动。
    #[test]
    fn retract_contribution_on_soft_assigned_segment_is_noop() {
        let mut r = SpeakerRegistry::new();
        assert_eq!(r.assign_tracked(&v(1.0, 0.0, 0.0), "mic", LONG, "segA"), Some("S1".into()));
        // 余弦 0.5 ∈ [SOFT, ASSIGN) 灰区 → 软归属,不拖质心/计数,不记贡献。
        let y = (1.0f32 - 0.25).sqrt();
        assert_eq!(r.assign_tracked(&v(0.5, y, 0.0), "system", LONG, "segSoft"), Some("S1".into()));

        assert!(!r.retract_contribution("segSoft"), "软归属段没有对应贡献记录,应为 no-op");

        let snap = r.snapshot();
        assert_eq!(snap[0].count, 1, "软归属撤回不应影响簇状态");
        assert_eq!(snap[0].total_ms, 3000);
    }

    /// 回声追溯撤回③:两簇合并后,撤销一条"合并前属于 loser 簇"的历史贡献,
    /// 应从合并后的 winner 簇上正确回滚(环内条目随合并重映射到 winner)。
    #[test]
    fn retract_contribution_after_merge_rolls_back_from_winner() {
        let mut r = SpeakerRegistry::new();
        // 直接摆两个"高度相似但仍是两个簇"的状态,绕开 assign() 阈值互斥的物理约束
        // (同一测试文件内 mod tests 对私有字段可见,构造纯逻辑场景更直接)。
        r.clusters.push(Cluster {
            id: "S1".into(),
            centroid: v(1.0, 0.0, 0.0),
            count: 2,
            sources: BTreeSet::from(["mic".to_string()]),
            person: None,
            person_name: None,
            total_ms: 5000,
            seed_base_count: 0,
            reported_ms: 0,
            session_enrolled: false,
            seed_source: None,
            seed_cohort: None,
        });
        r.clusters.push(Cluster {
            id: "S2".into(),
            centroid: v(1.0, 0.0, 0.0),
            count: 1,
            sources: BTreeSet::from(["mic".to_string()]),
            person: None,
            person_name: None,
            total_ms: 1000,
            seed_base_count: 0,
            reported_ms: 0,
            session_enrolled: false,
            seed_source: None,
            seed_cohort: None,
        });
        r.record_contribution("segX", "S2".into(), v(1.0, 0.0, 0.0), 1000);

        let merges = r.take_merges();
        assert_eq!(merges, vec![("S2".to_string(), "S1".to_string())], "计数大者(S1)胜出");
        assert_eq!(r.speakers().len(), 1, "两簇应已合并为一");

        assert!(
            r.retract_contribution("segX"),
            "合并前记在 loser(S2)身上的贡献,合并后应仍可通过重映射撤回"
        );
        let snap = r.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, "S1", "撤回应作用在合并后的 winner 簇上");
        assert_eq!(snap[0].count, 2, "count 应从合并后的 3 回退到 2(冲抵 segX 的贡献)");
        assert_eq!(snap[0].total_ms, 5000, "total_ms 应从合并后的 6000 回退到 5000");
    }

    /// 回声追溯撤回④:贡献环容量有限(CONTRIBUTION_RING_CAP),超容量后最旧条目
    /// 被淘汰;针对已淘汰 seg_key 的撤回请求是 no-op,不 panic,簇状态不受影响。
    #[test]
    fn retract_contribution_after_ring_eviction_is_noop_not_panic() {
        let mut r = SpeakerRegistry::new();
        r.clusters.push(Cluster {
            id: "S1".into(),
            centroid: v(1.0, 0.0, 0.0),
            count: 1,
            sources: BTreeSet::from(["mic".to_string()]),
            person: None,
            person_name: None,
            total_ms: 1000,
            seed_base_count: 0,
            reported_ms: 0,
            session_enrolled: false,
            seed_source: None,
            seed_cohort: None,
        });
        r.record_contribution("seg0", "S1".into(), v(1.0, 0.0, 0.0), 1000);
        // 填满环把 seg0 挤出去。
        for i in 0..CONTRIBUTION_RING_CAP {
            r.record_contribution(&format!("filler{i}"), "S1".into(), v(1.0, 0.0, 0.0), 1);
        }

        assert!(!r.retract_contribution("seg0"), "已被环淘汰的 seg_key 撤回应是 no-op");
        assert_eq!(r.clusters[0].count, 1, "被淘汰贡献的撤回请求不应影响簇状态(count 未受 filler 记录影响)");

        // 未知 seg_key(从未记录过)同样是 no-op,不 panic。
        assert!(!r.retract_contribution("never-existed"));
    }

    /// Task 2 三闸①:段短于 SEED_MIN_SAMPLES(2s)不参与种子命中——即使裸分很高,
    /// 场内无其他簇可归、种子又不参与软归属,只能留空;够长(2.1s)立即命中。
    #[test]
    fn short_segment_never_claims_seed() {
        let seeds = vec![SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 10, source: "mic".into() }];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        let probe = v(0.9, (1.0f32 - 0.81).sqrt(), 0.0); // 同信道裸分 0.9,足以命中快路
        // 1.9s(30400 样本)< SEED_MIN_SAMPLES(32000,2s):无权拍板种子。
        assert_eq!(r.assign(&probe, "mic", 30400), None, "1.9s 段不得命中种子,也无处可归");
        assert_eq!(
            r.speakers().iter().find(|s| s.person.as_deref() == Some("P1")).unwrap().sources.len(),
            0,
            "未命中,种子簇 sources 仍应为空"
        );
        // 2.1s(33600 样本)≥ 门槛,同信道裸分 0.9 ≥ SEED_ASSIGN_THRESHOLD → 命中。
        let id = r.assign(&probe, "mic", 33600).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person.as_deref(), Some("P1"), "2.1s 段应命中种子");
    }

    /// Task 2 三闸③(收紧点):跨信道裸分不可比,库里只有一个人物(cohort<3)
    /// 时 z 通道天然关闭——旧行为下裸 cos 0.70 ≥ 0.68 会命中,新语义下必须拒绝。
    #[test]
    fn cross_channel_raw_hit_no_longer_accepted() {
        let seeds = vec![SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 10, source: "system".into() }];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        let probe = v(0.70, (1.0f32 - 0.49).sqrt(), 0.0); // mic 段 vs system 种子,裸分 0.70
        let id = r.assign(&probe, "mic", LONG).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person, None, "跨信道裸分 0.70 但 z 通道关闭(cohort<3),不得命中种子,应新建普通簇");
    }

    /// Task 2 三闸③:跨信道走 AS-Norm z 通道。5 维正交基,P1~P3(mic)是陪衬人物,
    /// P4(system)是候选;探针对陪衬人物只有 ~0.1 裸分、对 P4 是 0.55——裸分本身
    /// 够不着任何阈值,但陪衬人物普遍低分把 0.55 衬成统计显著(z ≥ 3.0)。
    #[test]
    fn cross_channel_high_z_accepted() {
        let seeds = vec![
            SeedCluster { person: "P1".into(), name: "一".into(), centroid: vec![1.0, 0.0, 0.0, 0.0, 0.0], count: 10, source: "mic".into() },
            SeedCluster { person: "P2".into(), name: "二".into(), centroid: vec![0.0, 1.0, 0.0, 0.0, 0.0], count: 10, source: "mic".into() },
            SeedCluster { person: "P3".into(), name: "三".into(), centroid: vec![0.0, 0.0, 1.0, 0.0, 0.0], count: 10, source: "mic".into() },
            SeedCluster { person: "P4".into(), name: "四".into(), centroid: vec![0.0, 0.0, 0.0, 1.0, 0.0], count: 10, source: "system".into() },
        ];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        let tail = (1.0f32 - (0.12f32.powi(2) + 0.09f32.powi(2) + 0.09f32.powi(2) + 0.55f32.powi(2))).sqrt();
        let probe = vec![0.12, 0.09, 0.09, 0.55, tail];
        let id = r.assign(&probe, "mic", LONG).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person.as_deref(), Some("P4"), "跨信道裸分 0.55 但 z ≥ 3.0 → 应命中种子 P4");
    }

    /// Task 2 三闸②:同信道快路阈值与现状逐位一致——0.69 命中,0.67 不命中
    /// (库里只此一人,z 通道关闭,与修改前的行为完全一致)。
    #[test]
    fn same_channel_fast_path_unchanged() {
        let mk_seed = || vec![SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 10, source: "mic".into() }];

        let mut r1 = SpeakerRegistry::with_seeds(&[], &mk_seed());
        let probe69 = v(0.69, (1.0f32 - 0.69 * 0.69).sqrt(), 0.0);
        let id69 = r1.assign(&probe69, "mic", LONG).unwrap();
        assert_eq!(
            r1.speakers().into_iter().find(|s| s.id == id69).unwrap().person.as_deref(),
            Some("P1"),
            "同信道 0.69 ≥ 0.68 应命中"
        );

        let mut r2 = SpeakerRegistry::with_seeds(&[], &mk_seed());
        let probe67 = v(0.67, (1.0f32 - 0.67 * 0.67).sqrt(), 0.0);
        let id67 = r2.assign(&probe67, "mic", LONG).unwrap();
        assert_eq!(
            r2.speakers().into_iter().find(|s| s.id == id67).unwrap().person,
            None,
            "同信道 0.67 < 0.68 且 z 通道关闭,不得命中"
        );
    }

    /// Task 2 三闸③边界:cohort 人数(其他人物数)< SNORM_MIN_COHORT(3) 时 z 通道
    /// 整体关闭——哪怕裸分本可算出很高的 z,也无法核实,不得命中。
    #[test]
    fn z_path_disabled_below_min_cohort() {
        let seeds = vec![
            SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 10, source: "system".into() },
            SeedCluster { person: "P2".into(), name: "乙".into(), centroid: v(0.0, 0.0, 1.0), count: 10, source: "mic".into() },
        ];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        let probe = v(0.55, (1.0f32 - 0.55 * 0.55).sqrt(), 0.0); // mic 段 vs system 种子 P1,与 P2(e3)正交
        let id = r.assign(&probe, "mic", LONG).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person, None, "只有 2 个人物(1 个其他人物)< 门槛,z 通道应关闭,不得命中");
    }

    /// Task 2:MIN_CENTROID_UPDATE_SAMPLES 提到 24_000(1.5s)。1.4s 段命中后
    /// 质心不动(后续探针仍按原质心判定);1.6s 段命中后质心按 running mean 更新。
    #[test]
    fn centroid_update_gate_raised() {
        let mut r = SpeakerRegistry::new();
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap(); // S1 质心 = e1
        // 1.4s(22400 样本)< 24_000:命中但不得拖动质心。
        assert_eq!(r.assign(&v(0.8, 0.6, 0.0), "mic", 22400), Some("S1".into()));
        // 探针与原质心 e1 余弦 0.35(< SOFT 0.45):若质心未被拖动,应新建 S2;
        // 若质心被拖向 (0.8,0.6,0),该探针会离新质心更近而被吸入 S1(回归即失败)。
        assert_eq!(
            r.assign(&v(0.35, (1.0f32 - 0.35 * 0.35).sqrt(), 0.0), "mic", LONG),
            Some("S2".into()),
            "1.4s 段不得更新质心(低于新门槛 24_000)"
        );

        let mut r2 = SpeakerRegistry::new();
        r2.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap(); // S1 质心 = e1
        // 8 次 1.6s(25600 样本)段命中且更新质心,质心逐渐偏向 (1.0,0.8,0) 方向。
        for _ in 0..8 {
            r2.assign(&v(1.0, 0.8, 0.0), "mic", 25600).unwrap();
        }
        assert_eq!(
            r2.assign(&v(0.55, 0.75, 0.0), "mic", LONG),
            Some("S1".into()),
            "1.6s 段应更新质心(达到新门槛 24_000),质心随成员漂移"
        );
    }

    /// T1 审查交接项锁死:同一 person 的多信道种子簇经 detect_merges 自动合并后,
    /// winner 质心已是跨信道混合质心,不再单纯属于某一信道——种子身份与
    /// AS-Norm 统计随之作废,退化为"恢复簇"语义(seed_source/seed_cohort 清空)。
    #[test]
    fn seed_merge_degrades_seed_source_and_cohort_to_recovered_semantics() {
        // 复用死区修复场景:无主簇(mic,余弦 0.71)漂到够得着种子(mic,P1)的
        // 归簇死区阈值,自动并入种子(种子 count 更大,winner)。同信道(终审
        // F3a 之后仍走 SEED_ASSIGN_THRESHOLD 0.68,与既有测试一致);跨信道
        // 场景已改抬到 MERGE_THRESHOLD,见专项锁测,这里只关心降级规则本身。
        let snap = ClusterSnapshot {
            id: "S9".into(),
            centroid: v(0.71, 0.70413, 0.0),
            count: 2,
            sources: BTreeSet::from(["mic".to_string()]),
            person: None,
            total_ms: 3000,
        };
        let seeds = vec![SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 10, source: "mic".into() }];
        let mut r = SpeakerRegistry::with_seeds(&[snap], &seeds);
        assert_eq!(r.take_merges().len(), 1, "同信道下,无主↔有主在 0.71 应按归簇同款阈值合并(前置条件与既有测试一致)");
        let winner = r.clusters.iter().find(|c| c.person.as_deref() == Some("P1")).unwrap();
        assert!(winner.seed_source.is_none(), "混合信道质心不再属于单一信道,seed_source 应被清空");
        assert!(winner.seed_cohort.is_none(), "统计随信道身份一并作废,seed_cohort 应被清空");
    }

    /// 恢复簇(seed_source=None 且 is_seed())语义锁死:续录快照恢复的簇信道未知,
    /// 维持旧行为——裸 cos ≥ SEED_ASSIGN_THRESHOLD 命中,不分信道、无 z 通道,
    /// 也不受 Task 2 新增的短段门槛(SEED_MIN_SAMPLES)约束,待快照带信道后再收紧。
    #[test]
    fn recovered_cluster_keeps_legacy_raw_threshold_regardless_of_channel_or_length() {
        let snap = ClusterSnapshot {
            id: "S1".into(),
            centroid: v(1.0, 0.0, 0.0),
            count: 5,
            sources: BTreeSet::from(["mic".to_string()]),
            person: Some("P1".into()),
            total_ms: 0,
        };
        let mut r = SpeakerRegistry::from_snapshot(&[snap]);
        let probe = v(0.70, (1.0f32 - 0.49).sqrt(), 0.0);
        // 1s 短段(16000 样本,远低于 SEED_MIN_SAMPLES),来源随便标"system"
        // (恢复簇无信道记录,不做同信道校验):裸分 0.70 ≥ 0.68 仍应命中。
        let id = r.assign(&probe, "system", 16000).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(
            info.person.as_deref(),
            Some("P1"),
            "恢复簇应维持旧行为:裸分达标即命中,不受短段/信道新门槛约束"
        );
    }

    /// T2 审查必修项:锁死对称 z 公式的**形态**,而非只锁"能不能走通 z 通道"。
    /// 既有的 `cross_channel_high_z_accepted` 用正交基构造 cohort,方差为 0 被
    /// σ 下限(1e-3)夹住,z 算出几百量级——za-only/zb-only/漏 ÷2 三种错误实现
    /// 换上去照样命中,毫无判别力。
    ///
    /// 本测试构造双侧**真实方差**(σa=0.1/0.15、σb=0.1,远离 1e-3 下限),让
    /// za、zb 刻意不对称(相差 2.6),对称 z 精确落在 3.0 边界两侧:
    /// - 命中用例:za=1.8、zb=4.4 → 对称 z=3.1(≥3,命中)。
    ///   若变异成 za-only,z 就是 1.8(<3)→ 误判不命中。
    /// - 镜像用例(zb 不变,只调低 za):za=1.4、zb=4.4 → 对称 z=2.9(<3,不命中)。
    ///   若变异成 zb-only,z 就是 4.4(≥3)→ 误判命中;
    ///   若漏掉 ÷2,z 就是 za+zb=5.8(≥3)→ 同样误判命中。
    ///
    /// 几何构造(python 预演,见 PR 描述/task-2-report.md):5 维正交基,
    /// C(候选 P4,system)=e1;P1/P2/P3(mic,陪衬)分别是 e1 与 e2/e3/e4 的混合,
    /// 对 C 的种子侧余弦固定在 0.10/0.20/0.30(→ μb=0.2,σb=0.1,与信道无关,
    /// 两个探针共用);探针在 e1 分量固定 0.64(= sim),e2/e3/e4 分量各自反解
    /// 使 dot(Pi,探针) 落在预设的扫描侧分数上,e5 分量归一化兜底。
    ///
    /// 自证(变异测试):临时把 `seed_z` 改成 `Some(za)`(za-only),重跑本测试,
    /// 命中用例的 `assert_eq!` 从绿转红(实测输出与证据见 task-2-report.md),
    /// 证明本测试确能捕获该类回归;验证后已改回对称式。
    #[test]
    fn symmetric_z_formula_rejects_single_sided_or_undivided_variants() {
        let seeds = vec![
            SeedCluster { person: "P1".into(), name: "一".into(), centroid: vec![0.10, 0.994_987_5, 0.0, 0.0, 0.0], count: 10, source: "mic".into() },
            SeedCluster { person: "P2".into(), name: "二".into(), centroid: vec![0.20, 0.0, 0.979_795_9, 0.0, 0.0], count: 10, source: "mic".into() },
            SeedCluster { person: "P3".into(), name: "三".into(), centroid: vec![0.30, 0.0, 0.0, 0.953_939_2, 0.0], count: 10, source: "mic".into() },
            SeedCluster { person: "P4".into(), name: "四".into(), centroid: vec![1.0, 0.0, 0.0, 0.0, 0.0], count: 10, source: "system".into() },
        ];

        // 命中用例:za=1.8、zb=4.4,对称 z=3.1≥3。
        let mut r_hit = SpeakerRegistry::with_seeds(&[], &seeds);
        let probe_hit = vec![0.64, 0.297_491_2, 0.338_846_1, 0.385_768_8, 0.488_123_7];
        let id = r_hit.assign(&probe_hit, "mic", LONG).unwrap();
        let info = r_hit.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(
            info.person.as_deref(),
            Some("P4"),
            "za=1.8、zb=4.4,对称 z=3.1≥3 应命中(za-only 变异会把 z 算成 1.8,误判不命中)"
        );

        // 镜像用例:zb 不变(4.4),只把 za 调到 1.4 → 对称 z=2.9<3,不应命中。
        let mut r_miss = SpeakerRegistry::with_seeds(&[], &seeds);
        let probe_miss = vec![0.64, 0.217_088_2, 0.308_227_5, 0.406_734_5, 0.531_822_9];
        let id2 = r_miss.assign(&probe_miss, "mic", LONG).unwrap();
        let info2 = r_miss.speakers().into_iter().find(|s| s.id == id2).unwrap();
        assert_eq!(
            info2.person, None,
            "za=1.4、zb=4.4,对称 z=2.9<3 不应命中(zb-only 会把 z 算成 4.4,漏÷2 会把 z 算成 5.8,均误判命中)"
        );
    }

    /// T2 审查顺手项①:z 通道未按信道区分,同信道裸分卡在
    /// [SEED_ASSIGN_RAW_FLOOR, SEED_ASSIGN_THRESHOLD) 死区(够不着快路 0.68)时,
    /// z 通道依旧可依统计显著性放行——这是有意的**召回增益**设计(本应命中的
    /// 同信道段不因裸分差一点点而落空),不是收紧。锁死增益方向:若未来有人
    /// 误给 z 通道加上"仅跨信道生效"的限制,本测试会变红,提示这是行为变更
    /// 而非无害重构。复用上一测试(命中用例)的同一套向量,仅把候选人物 P4 的
    /// 种子信道与探针信道都改成 "mic"(同信道)。
    #[test]
    fn same_channel_z_channel_widens_recall_not_tightens() {
        let seeds = vec![
            SeedCluster { person: "P1".into(), name: "一".into(), centroid: vec![0.10, 0.994_987_5, 0.0, 0.0, 0.0], count: 10, source: "mic".into() },
            SeedCluster { person: "P2".into(), name: "二".into(), centroid: vec![0.20, 0.0, 0.979_795_9, 0.0, 0.0], count: 10, source: "mic".into() },
            SeedCluster { person: "P3".into(), name: "三".into(), centroid: vec![0.30, 0.0, 0.0, 0.953_939_2, 0.0], count: 10, source: "mic".into() },
            // 候选人物、陪衬人物、探针全部同信道("mic")——若 z 通道只对跨信道
            // 开放,这里应因裸分 0.64<0.68(快路够不着)而落空;实测未按信道
            // 区分,z=3.1≥3 依旧命中。
            SeedCluster { person: "P4".into(), name: "四".into(), centroid: vec![1.0, 0.0, 0.0, 0.0, 0.0], count: 10, source: "mic".into() },
        ];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        let probe = vec![0.64, 0.297_491_2, 0.338_846_1, 0.385_768_8, 0.488_123_7]; // sim=0.64 ∈ [0.50,0.68)
        let id = r.assign(&probe, "mic", LONG).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(
            info.person.as_deref(),
            Some("P4"),
            "同信道裸分 0.64 ∈ [0.50,0.68) 死区但 z=3.1≥3,z 通道对同信道同样开放(召回增益)应命中"
        );
    }

    /// 终审 F1 锁测:with_seeds 给同一 person 的主质心+每条会话变体各建一个
    /// 种子簇(匹配语义取 max),两个 P1 种子(主质心/变体)余弦 0.9 ≥
    /// SEED_ASSIGN_THRESHOLD(0.68)——修前会在第一次 detect_merges 里互并,
    /// 混合质心后 seed_source/seed_cohort 被下面的降级规则清空,该人退化为
    /// "恢复簇"语义(裸 0.68 跨信道复活、丢短段闸)。P2/P3/P4 只是凑够
    /// SNORM_MIN_COHORT(3 个其他人物)让 P1 的 seed_cohort 非 None,便于校验
    /// 互并未清空它。多轮 take_merges 与穿插的无关 assign(触发周期性
    /// detect_merges)之后,P1 应始终是两个独立、身份字段完好的种子簇。
    #[test]
    fn same_person_seed_variants_never_automerge_and_keep_seed_identity() {
        let seeds = vec![
            SeedCluster { person: "P1".into(), name: "甲".into(), centroid: vec![1.0, 0.0, 0.0, 0.0, 0.0], count: 40, source: "mic".into() },
            // 与主质心余弦 = 0.9(0.9² + 0.435_889_9² ≈ 1)。
            SeedCluster { person: "P1".into(), name: "甲".into(), centroid: vec![0.9, 0.435_889_9, 0.0, 0.0, 0.0], count: 5, source: "mic".into() },
            SeedCluster { person: "P2".into(), name: "乙".into(), centroid: vec![0.0, 1.0, 0.0, 0.0, 0.0], count: 5, source: "mic".into() },
            SeedCluster { person: "P3".into(), name: "丙".into(), centroid: vec![0.0, 0.0, 1.0, 0.0, 0.0], count: 5, source: "mic".into() },
            SeedCluster { person: "P4".into(), name: "丁".into(), centroid: vec![0.0, 0.0, 0.0, 1.0, 0.0], count: 5, source: "system".into() },
        ];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        assert_eq!(r.clusters.len(), 5, "P1 的两条种子(主质心+变体)都应各自建簇");
        let p1_before = r.clusters.iter().filter(|c| c.person.as_deref() == Some("P1")).count();
        assert_eq!(p1_before, 2, "互并前 P1 应有两个独立种子簇");
        for c in r.clusters.iter().filter(|c| c.person.as_deref() == Some("P1")) {
            assert!(c.seed_cohort.is_some(), "3 个其他人物已达 SNORM_MIN_COHORT,seed_cohort 应为 Some");
        }

        for _ in 0..5 {
            assert!(r.take_merges().is_empty(), "同人种子簇互并零收益,终审 F1 应恒不触发合并");
        }
        // 穿插几条与所有种子正交的无关 assign(第 5 维),验证 MERGE_CHECK_INTERVAL
        // 周期触发的 detect_merges 同样不会误伤 P1 的两个种子簇。
        for _ in 0..8 {
            r.assign(&[0.0, 0.0, 0.0, 0.0, 1.0], "mic", LONG);
        }
        assert!(r.take_merges().is_empty());

        let p1_after: Vec<&Cluster> = r.clusters.iter().filter(|c| c.person.as_deref() == Some("P1")).collect();
        assert_eq!(p1_after.len(), 2, "P1 的两个种子簇应始终独立存在,未被互并");
        for c in &p1_after {
            assert!(c.seed_source.is_some(), "F1 修复后 P1 种子簇的 seed_source 不应被互并清空");
            assert!(c.seed_cohort.is_some(), "F1 修复后 P1 种子簇的 seed_cohort 不应被互并清空");
        }
    }

    /// 终审 F1 死区回归锁测:无主会话簇 ↔ 种子簇的合并(既有死区修复)不因
    /// F1 的"seed↔seed 早退"而受影响——早退条件要求双方 seed_source 均非
    /// None,无主簇(person=None)天然不是种子,不会被早退挡住。构造与
    /// `different_persons_never_automerge_and_winner_inherits_person` 相同
    /// (相向漂移把跨信道质心拉到 ≥ MERGE_THRESHOLD,F3a 的信道判定同样放行)。
    #[test]
    fn masterless_to_seed_merge_still_works_after_f1() {
        let mut r2 = SpeakerRegistry::with_seeds(
            &[],
            &[SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 1, source: "mic".into() }],
        );
        // 建无主簇(system 信道),与种子(mic)正交起步。
        r2.assign(&v(0.30, (1.0f32 - 0.09f32).sqrt(), 0.0), "system", LONG).unwrap();
        for k in 1..=10 {
            let t = 0.30 + 0.05 * k as f32; // 0.35..0.80
            let y = (1.0 - t * t).max(0.0).sqrt();
            r2.assign(&v(t, y, 0.0), "system", LONG).unwrap();
        }
        // 再用同信道(mic)样本把种子质心拖向无主簇方向,两者相向漂移到
        // 跨信道也够得着的 MERGE_THRESHOLD。
        for _ in 0..9 {
            r2.assign(&v(0.90, 0.436, 0.0), "mic", LONG).unwrap();
        }
        let merges = r2.take_merges();
        assert_eq!(merges.len(), 1, "无主会话簇(跨信道)最终质心贴近种子达 MERGE_THRESHOLD 时仍应合并,死区语义不回归");
    }

    /// 终审 F3a 锁测:无主会话簇 ↔ 种子簇的合并,信道一致时维持
    /// SEED_ASSIGN_THRESHOLD(0.68,死区修复原语义);跨信道时抬到
    /// MERGE_THRESHOLD(0.74)——assign 路已经不允许 mic 段裸 0.68 撞 system
    /// 种子直接命中,不能让它从"合并"这条路绕回去。种子固定来源 "system";
    /// 无主簇用与种子正交的向量建出(sim=0,不触发任何归簇/命中判定),再直接
    /// 改写其质心到目标余弦,只验证 detect_merges 的门槛逻辑本身。
    #[test]
    fn masterless_to_seed_merge_cross_channel_threshold_raised() {
        let seeds = vec![SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 10, source: "system".into() }];

        // 跨信道(无主簇来源 "mic"):0.70 < 0.74,不应合并。
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        let mid = r.assign(&v(0.0, 1.0, 0.0), "mic", LONG).unwrap();
        let idx = r.clusters.iter().position(|c| c.id == mid).unwrap();
        r.clusters[idx].centroid = v(0.70, (1.0f32 - 0.70 * 0.70).sqrt(), 0.0);
        assert!(r.take_merges().is_empty(), "跨信道 0.70 < MERGE_THRESHOLD(0.74) 不应合并");
        assert_eq!(r.speakers().len(), 2);

        // 跨信道:0.75 ≥ 0.74,应合并。
        let mut r2 = SpeakerRegistry::with_seeds(&[], &seeds);
        let mid2 = r2.assign(&v(0.0, 1.0, 0.0), "mic", LONG).unwrap();
        let idx2 = r2.clusters.iter().position(|c| c.id == mid2).unwrap();
        r2.clusters[idx2].centroid = v(0.75, (1.0f32 - 0.75 * 0.75).sqrt(), 0.0);
        assert_eq!(r2.take_merges().len(), 1, "跨信道 0.75 ≥ MERGE_THRESHOLD(0.74) 应合并");

        // 同信道(无主簇来源同为 "system"):0.70 ≥ SEED_ASSIGN_THRESHOLD(0.68),
        // 维持死区修复的原行为,应合并。
        let mut r3 = SpeakerRegistry::with_seeds(&[], &seeds);
        let mid3 = r3.assign(&v(0.0, 1.0, 0.0), "system", LONG).unwrap();
        let idx3 = r3.clusters.iter().position(|c| c.id == mid3).unwrap();
        r3.clusters[idx3].centroid = v(0.70, (1.0f32 - 0.70 * 0.70).sqrt(), 0.0);
        assert_eq!(r3.take_merges().len(), 1, "同信道 0.70 ≥ SEED_ASSIGN_THRESHOLD(0.68) 照并,死区语义不变");
    }

    /// 与探针余弦恰为 x 的单位向量(同 v 的三维空间,y 分量补齐模长)。
    fn at_cos(x: f32) -> Vec<f32> {
        v(x, (1.0f32 - x * x).sqrt(), 0.0)
    }

    #[test]
    fn knn_vote_majority_beats_single_nearest() {
        // 乙有 3 份采集来的变体质心(0.72/0.71/0.70,全过同信道快路 0.68),
        // 甲只有 1 份但离探针最近(0.80)。旧单最近邻会判甲;k-NN 票决下
        // top-4 席位乙占 3 席,乙胜出,认领落在乙的最高分席位上。
        let seeds = vec![
            SeedCluster { person: "PA".into(), name: "甲".into(), centroid: at_cos(0.80), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.72), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.71), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.70), count: 10, source: "mic".into() },
        ];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        r.set_matcher(Box::new(KnnVoteMatcher));
        let id = r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person.as_deref(), Some("PB"), "多数票(3 席)应压过单个更近的离群席位");
    }

    #[test]
    fn knn_vote_winner_without_qualified_seat_claims_nothing() {
        // 乙 3 席全在阈值下(0.64/0.63/0.62 < 0.68,两人成不了 z cohort),
        // 甲 1 席 0.80 过阈但只是少数派。票决胜者乙无合格席位 → 种子路
        // 不认领也不降格给第二名(误命名比不命名糟),探针新建无主簇。
        let seeds = vec![
            SeedCluster { person: "PA".into(), name: "甲".into(), centroid: at_cos(0.80), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.64), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.63), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.62), count: 10, source: "mic".into() },
        ];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        r.set_matcher(Box::new(KnnVoteMatcher));
        let id = r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person, None, "票决胜者不过阈,不得回落认领少数派");
    }

    #[test]
    fn knn_vote_tie_breaks_by_best_similarity() {
        // 甲乙各 2 席平票,甲的最高分(0.75)高于乙(0.73)→ 甲胜。
        let seeds = vec![
            SeedCluster { person: "PA".into(), name: "甲".into(), centroid: at_cos(0.75), count: 10, source: "mic".into() },
            SeedCluster { person: "PA".into(), name: "甲".into(), centroid: at_cos(0.70), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.73), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.72), count: 10, source: "mic".into() },
        ];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        r.set_matcher(Box::new(KnnVoteMatcher));
        let id = r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person.as_deref(), Some("PA"), "平票应取最高分席位所属的人");
    }

    /// 锁定默认最近邻语义:更近但不合格的席位不得挡住更远的合格席位——
    /// 「合格者中取最近」,不是「最近者必须合格」(后者是 k=1 票决的语义)。
    /// 甲跨信道 0.80(仅 2 人,z cohort 不足,不合格);乙同信道 0.70 合格 → 认乙。
    #[test]
    fn nearest_picks_best_eligible_not_nearest_overall() {
        let seeds = vec![
            SeedCluster { person: "PA".into(), name: "甲".into(), centroid: at_cos(0.80), count: 10, source: "system".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.70), count: 10, source: "mic".into() },
        ];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        let id = r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person.as_deref(), Some("PB"), "跨信道不合格的更近席位不得挡合格席位");
    }

    /// 续录快照簇不进票池(codex 终审 P1):with_seeds 会压掉快照 person 的库
    /// 种子,该人恒只有 1 席;票决下 cos 1.0 的精确续录匹配会被别人的多席
    /// 变体投翻。快照簇走独立最近邻通道,精确匹配必须赢。
    #[test]
    fn knn_vote_does_not_outvote_resumed_snapshot_exact_match() {
        let snap = ClusterSnapshot {
            id: "S7".into(),
            centroid: v(1.0, 0.0, 0.0),
            count: 5,
            sources: BTreeSet::from(["mic".to_string()]),
            person: Some("P9".into()),
            total_ms: 0,
        };
        let seeds = vec![
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.72), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.71), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.70), count: 10, source: "mic".into() },
        ];
        let mut r = SpeakerRegistry::with_seeds(&[snap], &seeds);
        r.set_matcher(Box::new(KnnVoteMatcher));
        let id = r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person.as_deref(), Some("P9"), "续录精确匹配不得被库种子多数票压掉");
    }

    /// 票决跨人完全平票(席位数与最高分都相等)保先注入,不按 person id 字典序
    /// (codex 终审 P2):PA 先注入且字典序更小——若实现按 id 序取最大,会错判
    /// 后注入的 PB;正确行为是保先注入的 PA。
    #[test]
    fn knn_vote_full_tie_across_persons_keeps_first_seen() {
        let seeds = vec![
            SeedCluster { person: "PA".into(), name: "甲".into(), centroid: at_cos(0.70), count: 10, source: "mic".into() },
            SeedCluster { person: "PA".into(), name: "甲".into(), centroid: at_cos(0.69), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.70), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.69), count: 10, source: "mic".into() },
        ];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        r.set_matcher(Box::new(KnnVoteMatcher));
        let id = r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person.as_deref(), Some("PA"), "全平票应保先注入者,不得按 id 字典序");
    }

    /// 种子簇与普通簇同分(codex 终审 P2):种子先于普通簇注入,历史单遍扫描
    /// 严格大于保先——同分应认种子(带人名),不得改判后建的普通簇。
    #[test]
    fn nearest_tie_between_seed_and_regular_keeps_seed() {
        let seeds = vec![SeedCluster {
            person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 10, source: "mic".into(),
        }];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        // 先造一个正交方向的普通簇。
        r.assign(&v(0.0, 1.0, 0.0), "mic", LONG).unwrap();
        // 与种子/普通簇等距(cos ≈ 0.7071,两边都过各自阈值)。
        let probe = v(std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2, 0.0);
        let id = r.assign(&probe, "mic", LONG).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person.as_deref(), Some("P1"), "同分应保先扫到的种子簇");
    }

    /// matcher_from_key:未知/脏配置值回落默认最近邻,不 panic 不挡识别。
    #[test]
    fn matcher_from_key_falls_back_to_nearest() {
        let seeds = vec![
            SeedCluster { person: "PA".into(), name: "甲".into(), centroid: at_cos(0.80), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.72), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.71), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.70), count: 10, source: "mic".into() },
        ];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        r.set_matcher(matcher_from_key("garbage-value"));
        let id = r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person.as_deref(), Some("PA"), "脏值回落最近邻:0.80 单席应胜");
        let mut r2 = SpeakerRegistry::with_seeds(&[], &seeds);
        r2.set_matcher(matcher_from_key(SPEAKER_MATCH_KNN_VOTE));
        let id2 = r2.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap();
        let info2 = r2.speakers().into_iter().find(|s| s.id == id2).unwrap();
        assert_eq!(info2.person.as_deref(), Some("PB"), "knn_vote 键应选到票决策略");
    }

    #[test]
    fn knn_tied_seats_at_boundary_keep_first_seen_order() {
        // 六席全同分(同一质心):甲 3 席先注入,乙 3 席后注入。top-5 必须保序
        // 留下 甲甲甲乙乙 → 甲 3:2 胜;若同分后来者挤出先到者则乙反超(回归)。
        let seeds: Vec<SeedCluster> = ["PA", "PA", "PA", "PB", "PB", "PB"]
            .iter()
            .map(|p| SeedCluster {
                person: (*p).into(),
                name: if *p == "PA" { "甲".into() } else { "乙".into() },
                centroid: at_cos(0.70),
                count: 10,
                source: "mic".into(),
            })
            .collect();
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        r.set_matcher(Box::new(KnnVoteMatcher));
        let id = r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person.as_deref(), Some("PA"), "同分席位先到先占,K 边界不得翻票");
    }

    #[test]
    fn knn_vote_only_counts_five_nearest_seats() {
        // 乙 4 份远席位(0.30 附近)加 1 份 0.69 近席位;甲 4 席 0.75~0.72。
        // top-5 = 甲 4 席 + 乙 0.69 一席 → 甲以 4:1 胜;若错把全库席位都
        // 计票,乙 5 席反超,此测试即失败。
        let seeds = vec![
            SeedCluster { person: "PA".into(), name: "甲".into(), centroid: at_cos(0.75), count: 10, source: "mic".into() },
            SeedCluster { person: "PA".into(), name: "甲".into(), centroid: at_cos(0.74), count: 10, source: "mic".into() },
            SeedCluster { person: "PA".into(), name: "甲".into(), centroid: at_cos(0.73), count: 10, source: "mic".into() },
            SeedCluster { person: "PA".into(), name: "甲".into(), centroid: at_cos(0.72), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.69), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.32), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.31), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.30), count: 10, source: "mic".into() },
            SeedCluster { person: "PB".into(), name: "乙".into(), centroid: at_cos(0.29), count: 10, source: "mic".into() },
        ];
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        r.set_matcher(Box::new(KnnVoteMatcher));
        let id = r.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person.as_deref(), Some("PA"), "只有最近 5 席有投票权");
    }
}
