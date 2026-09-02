//! A1 离线全局重聚类:AHC 平均链接(精确:簇向量存**未归一化**成员均值,两均值点积
//! 恰等于两簇成员两两余弦的平均值)。纯逻辑,嵌入由调用方提供。
//! 在线单遍聚类(registry.rs)只做录制中临时标签;本模块产终稿。

use std::collections::BTreeMap;

use crate::diar::registry::{SeedCluster, SeedMatcher, SeedSeat, SEED_ASSIGN_THRESHOLD};

/// AHC 合并阈值(余弦)。低于在线 MERGE_THRESHOLD(0.74):全局视角下可更宽。
/// golden 校准定为 0.68:0.60 时次大簇(R2)污染更重,0.72+ 标签数超标(>12);
/// 0.68 是标签数与簇纯度的最优折中。golden 校准记录不入库(源数据不可再分发),
/// 关键数据已内联于本注释。
pub const AHC_THRESHOLD: f32 = 0.68;
/// 小于此总时长(ms)的簇为碎片,无条件并入最近大簇。
pub const MIN_CLUSTER_MS: u64 = 8000;
/// 段时长低于此值(ms)不提嵌入(调用方遵守;本模块按 embs=None 处理)。
pub const MIN_EMBED_MS: u64 = 1500;
/// 拆分路径的碎片门槛(ms):AHC 后总时长低于此值的组是"碎片",只在与某个大组
/// (≥ 此值)足够像时并入它;否则仍独立成组(短发言者正是要分出来的人,不强并)。
///
/// 2026-08-31 现场会(8 人、单麦远场)实测:平均连接 0.68 得 167 组 = 8 个 ≥1 分钟的
/// 大组 + 157 个 <30s 碎片(其中 150+ 是单段),不吞并就是 76 个「新说话人」胸牌。
pub const SPLIT_FRAGMENT_MS: u64 = 30_000;
/// 碎片并入最近大组所需的最低质心余弦。近场会议跨人相似 0.3~0.45,远场同房间
/// 跨人可达 0.6~0.8——0.5 让近场的短插话者保持独立,远场的碎片归到最像的人。
/// 同场实测 floor 0.5 吞并 152/157,留 5 个 1.5s 单段。
pub const SPLIT_ABSORB_SIM: f32 = 0.5;

pub struct SegInput {
    pub seq: u64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub source: String,
    pub old_speaker: Option<String>,
}

/// 会后簇统计:identify 身份推断的声学输入(P2a)。按原始稿说话人(S 域)分组。
#[derive(Debug, Clone)]
pub struct ClusterStat {
    /// 原始稿说话人 id(与 note.speakers / 修订稿段落 speaker 同键)。
    pub speaker: String,
    /// 按信道分组的单位质心:成员段先按 source 分组,组内已归一嵌入求均值再归一。
    /// 跨信道混合质心没有声学意义(与库内按信道存质心同理),不导出混合值。
    pub centroids: BTreeMap<String, Vec<f32>>,
    pub total_ms: u64,
    /// 信道 -> 该信道成员段时长(ms)。
    pub source_ms: BTreeMap<String, u64>,
    /// 有嵌入的成员段 seq(仅调试参考;无嵌入段计入时长但不在此集合,
    /// 身份指纹一律取最终 RefinedDoc 的 source_seqs)。
    pub core_seqs: Vec<u64>,
    /// 最佳库种子近邻:(person_id, name, cosine, adopted)。adopted=命名被采纳
    /// (cosine ≥ SEED_ASSIGN_THRESHOLD);未采纳的近邻仅供裁决层参考。
    pub seed: Option<(String, String, f32, bool)>,
}

fn normalize(v: &[f32]) -> Option<Vec<f32>> {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !n.is_finite() || n < 1e-6 {
        return None;
    }
    Some(v.iter().map(|x| x / n).collect())
}

/// 簇质心对库种子的策略判定:reference=参考近邻(无合格性约束,进 ClusterStat
/// 供 identify 裁决参考,未达阈也记录),claim=认领(合格性约束下的采纳判定,
/// 与实时路/重转写同一 claim 契约——若只按裸阈值判采纳,自带弃权语义的策略
/// 会被绕过,三条路径认人分叉,codex P2)。合格性口径:Aing 离线侧无信道/z
/// 通道语义(质心已按信道分组导出,匹配用裸余弦),eligible = sim ≥
/// SEED_ASSIGN_THRESHOLD。返回 (参考近邻, 认领结果),元素均为
/// (person, name, 该席位裸分)。
#[allow(clippy::type_complexity)]
fn seed_pick(
    centroid: &[f32],
    seeds: &[SeedCluster],
    matcher: &dyn SeedMatcher,
) -> (Option<(String, String, f32)>, Option<(String, String, f32)>) {
    let units: Vec<(usize, Vec<f32>)> = seeds
        .iter()
        .enumerate()
        .filter_map(|(i, s)| normalize(&s.centroid).map(|u| (i, u)))
        .collect();
    let seats: Vec<SeedSeat<'_>> = units
        .iter()
        .map(|(i, u)| {
            let sim = dot(centroid, u);
            SeedSeat {
                person: seeds[*i].person.as_str(),
                sim,
                eligible: sim >= SEED_ASSIGN_THRESHOLD,
                named: !seeds[*i].name.is_empty(),
            }
        })
        .collect();
    let pick = |k: usize| {
        let i = units[k].0;
        (seeds[i].person.clone(), seeds[i].name.clone(), seats[k].sim)
    };
    (matcher.reference(&seats).map(pick), matcher.claim(&seats).map(pick))
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

struct Cl {
    /// 成员单位向量的**未归一化**均值。两簇 `dot(mean_a, mean_b)` 恰等于成员两两余弦
    /// 的平均值(Σ_a Σ_b a·b / (|A||B|) = mean_A · mean_B),即精确的平均链接。
    ///
    /// 2026-08-31 事故:此前这里存的是归一化质心,合并后再归一——归一化把噪声
    /// 平均掉之后,两簇质心的余弦**系统性高于**成员两两余弦的平均值;远场单麦
    /// 会议里跨人段两两 ~0.55 却质心 ~0.75,巨簇越大越像"房间平均声",8 人 1100 段
    /// 链式并成 1 个巨簇 + 104 个单段。去掉归一化就是精确平均链接,同场分出 8 人。
    mean: Vec<f32>,
    members: Vec<usize>, // inputs 下标
    total_ms: u64,
}

impl Cl {
    /// 单位化质心(种子比对/碎片归属用;聚类判定不用它)。
    fn centroid(&self) -> Vec<f32> {
        normalize(&self.mean).unwrap_or_else(|| self.mean.clone())
    }
}

/// 合并两簇均值:按成员数加权,不归一化(见 Cl::mean)。
fn merge_mean(a: &Cl, b: &Cl) -> Vec<f32> {
    let wa = a.members.len() as f32;
    let wb = b.members.len() as f32;
    a.mean.iter().zip(&b.mean).map(|(x, y)| (x * wa + y * wb) / (wa + wb)).collect()
}

/// 拆分建议的一个组:成员段(下标指向 inputs)+ 最像的库种子(去处建议)。
pub struct SplitGroup {
    pub member_idx: Vec<usize>,
    pub total_ms: u64,
    /// (person_id, name, cosine):建议去处,仅供 UI 展示,不自动应用。
    pub suggested: Option<(String, String, f32)>,
}

/// 拆分建议:AHC 分组 + 无法判定桶。
pub struct SplitSuggestion {
    pub groups: Vec<SplitGroup>,
    /// 无嵌入的段下标(过短/嵌入失败/轨道缺失):不猜,单独一桶交给人。
    pub undetermined_idx: Vec<usize>,
}

/// 混杂簇的**拆分专用**聚类(设计:2026-08-20-mixed-speaker-split-design.md 承诺三)。
/// 与常规 recluster 的三点差别,都是刻意的:
/// - **不无条件吞并碎片**(MIN_CLUSTER_MS 不生效):混杂簇里的短发言者正是要分出来的人,
///   常规路径会把 <8s 的簇无条件并进最近大簇,功能等于没有(codex 设计轮一 P1⑦)。
///   2026-08-31 起改为**有条件吞并**:<SPLIT_FRAGMENT_MS 的组只在与某大组质心余弦
///   ≥ SPLIT_ABSORB_SIM 时并入;不像任何大组的仍独立成组(远场单麦会议不吞并会
///   得 76 个胸牌,见常量注释)
/// - 无嵌入的段**不按相邻传播**:这簇的段本来就不连续,"相邻"没有意义;统一进
///   无法判定桶(不只 <1.5s 的,嵌入失败/轨道缺失都算,codex 设计轮三 P2②)
/// - 种子只用来给每组标"最像谁"(建议去处),不改变分组本身
/// 组按时长降序;singleton 折叠等展示策略在 UI 层。
pub fn recluster_split(
    inputs: &[SegInput],
    embs: &[Option<Vec<f32>>],
    seeds: &[SeedCluster],
) -> SplitSuggestion {
    assert_eq!(inputs.len(), embs.len());
    let mut cls: Vec<Cl> = Vec::new();
    let mut undetermined_idx: Vec<usize> = Vec::new();
    for (i, e) in embs.iter().enumerate() {
        match e.as_ref().and_then(|v| normalize(v)) {
            Some(u) => cls.push(Cl {
                mean: u,
                members: vec![i],
                total_ms: inputs[i].end_ms.saturating_sub(inputs[i].start_ms),
            }),
            None => undetermined_idx.push(i),
        }
    }
    // AHC(精确平均链接:均值点积),没有传播。
    loop {
        let mut best: Option<(usize, usize, f32)> = None;
        for i in 0..cls.len() {
            for j in (i + 1)..cls.len() {
                let sim = dot(&cls[i].mean, &cls[j].mean);
                if best.map_or(true, |(_, _, s)| sim > s) {
                    best = Some((i, j, sim));
                }
            }
        }
        match best {
            Some((i, j, sim)) if sim >= AHC_THRESHOLD => {
                debug_assert!(i < j);
                let b = cls.swap_remove(j);
                let a = &mut cls[i];
                a.mean = merge_mean(a, &b);
                a.members.extend(b.members);
                a.total_ms += b.total_ms;
            }
            _ => break,
        }
    }
    // 有条件碎片吞并:碎片(<SPLIT_FRAGMENT_MS)并入最像的大组(≥SPLIT_FRAGMENT_MS),
    // 要求质心余弦 ≥ SPLIT_ABSORB_SIM;没有大组或不够像则原样保留。大组质心在
    // 吞并过程中固定(单遍、与顺序无关)。
    let (mut bigs, frags): (Vec<Cl>, Vec<Cl>) = cls.into_iter().partition(|c| c.total_ms >= SPLIT_FRAGMENT_MS);
    let big_centroids: Vec<Vec<f32>> = bigs.iter().map(Cl::centroid).collect();
    let mut cls: Vec<Cl> = Vec::new();
    for f in frags {
        let fc = f.centroid();
        let best = big_centroids
            .iter()
            .enumerate()
            .map(|(k, bc)| (k, dot(&fc, bc)))
            .max_by(|a, b| a.1.total_cmp(&b.1));
        match best {
            Some((k, sim)) if sim >= SPLIT_ABSORB_SIM => {
                let t = &mut bigs[k];
                t.mean = merge_mean(t, &f);
                t.members.extend(f.members);
                t.total_ms += f.total_ms;
            }
            _ => cls.push(f),
        }
    }
    cls.extend(bigs);
    // 同名强建议合组(2026-08-22 用户拍板「甲」):多个组的最近种子指向同一人且都
    // 过 SEED_ASSIGN_THRESHOLD,拆开自相矛盾(724 段大簇实测碎成 15 组、5 组同标
    // 「像是徐万振?」)。声纹既然笃定是同一人,就并成一组;低于阈值的仅是参考,
    // 不并(种子只是建议,不越权)。
    let seed_of = |c: &Cl| -> Option<(String, String, f32)> {
        // 最像的种子:同一 person 多种子取 max(与 seed_clusters 的多种子语义一致)。
        let mut best: Option<(String, String, f32)> = None;
        let cc = c.centroid();
        for sd in seeds {
            if let Some(u) = normalize(&sd.centroid) {
                let sim = dot(&cc, &u);
                if best.as_ref().map_or(true, |(_, _, s)| sim > *s) {
                    best = Some((sd.person.clone(), sd.name.clone(), sim));
                }
            }
        }
        best
    };
    let mut by_person: BTreeMap<String, usize> = BTreeMap::new();
    let mut merged: Vec<Cl> = Vec::new();
    for c in cls {
        match seed_of(&c).filter(|(_, _, sim)| *sim >= SEED_ASSIGN_THRESHOLD) {
            Some((pid, _, _)) => match by_person.get(&pid) {
                Some(&k) => {
                    let t = &mut merged[k];
                    t.mean = merge_mean(t, &c);
                    t.members.extend(c.members);
                    t.total_ms += c.total_ms;
                }
                None => {
                    by_person.insert(pid, merged.len());
                    merged.push(c);
                }
            },
            None => merged.push(c),
        }
    }
    merged.sort_by(|a, b| b.total_ms.cmp(&a.total_ms));
    let groups = merged
        .into_iter()
        .map(|c| {
            let best = seed_of(&c);
            let mut member_idx = c.members;
            member_idx.sort_unstable();
            SplitGroup { member_idx, total_ms: c.total_ms, suggested: best }
        })
        .collect();
    SplitSuggestion { groups, undetermined_idx }
}

/// 按原始稿说话人(S 域)分组的簇统计:identify 的声学输入。
///
/// 一波说话人设计(2026-08-21-one-speaker-set-design.md)后,pipeline 不再重分组
/// (原 AHC recluster 已删),修订稿段落直接沿用 note 说话人;这里把每个 S 说话人
/// 聚合成一个"簇":分信道质心 + 时长分布 + 种子近邻。无 speaker 的段不入组。
/// seed 的 adopted 语义不变:整体质心的策略认领(claim)命中该近邻(裸分 ≥
/// SEED_ASSIGN_THRESHOLD 且策略未弃权/未认别人)。
pub fn stats_by_speaker(
    inputs: &[SegInput],
    embs: &[Option<Vec<f32>>],
    seeds: &[SeedCluster],
    matcher: &dyn SeedMatcher,
) -> Vec<ClusterStat> {
    assert_eq!(inputs.len(), embs.len());
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, sg) in inputs.iter().enumerate() {
        if let Some(sp) = sg.old_speaker.as_deref() {
            groups.entry(sp.to_string()).or_default().push(i);
        }
    }
    groups
        .into_iter()
        .map(|(sp, members)| {
            let mut acc: BTreeMap<&str, (Vec<f32>, usize)> = BTreeMap::new();
            let mut overall: Option<(Vec<f32>, usize)> = None;
            let mut source_ms: BTreeMap<String, u64> = BTreeMap::new();
            let mut total_ms = 0u64;
            let mut core_seqs: Vec<u64> = Vec::new();
            for &m in &members {
                let dur = inputs[m].end_ms.saturating_sub(inputs[m].start_ms);
                total_ms += dur;
                *source_ms.entry(inputs[m].source.clone()).or_default() += dur;
                if let Some(u) = embs[m].as_ref().and_then(|v| normalize(v)) {
                    core_seqs.push(inputs[m].seq);
                    let entry = acc
                        .entry(inputs[m].source.as_str())
                        .or_insert_with(|| (vec![0.0; u.len()], 0));
                    if entry.0.len() == u.len() {
                        for (a, b) in entry.0.iter_mut().zip(&u) {
                            *a += b;
                        }
                        entry.1 += 1;
                    }
                    let o = overall.get_or_insert_with(|| (vec![0.0; u.len()], 0));
                    if o.0.len() == u.len() {
                        for (a, b) in o.0.iter_mut().zip(&u) {
                            *a += b;
                        }
                        o.1 += 1;
                    }
                }
            }
            core_seqs.sort_unstable();
            let centroids: BTreeMap<String, Vec<f32>> = acc
                .into_iter()
                .filter_map(|(src, (sum, n))| {
                    if n == 0 {
                        return None;
                    }
                    normalize(&sum).map(|u| (src.to_string(), u))
                })
                .collect();
            let seed = overall
                .and_then(|(sum, n)| if n == 0 { None } else { normalize(&sum) })
                .and_then(|c| {
                    let (reference, claim) = seed_pick(&c, seeds, matcher);
                    reference.map(|(p, n, sim)| {
                        let adopted = claim.as_ref().is_some_and(|(cp, _, _)| cp == &p);
                        (p, n, sim, adopted)
                    })
                });
            ClusterStat { speaker: sp, centroids, total_ms, source_ms, core_seqs, seed }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(seq: u64, start: u64, end: u64) -> SegInput {
        SegInput { seq, start_ms: start, end_ms: end, source: "mic".into(), old_speaker: None }
    }
    /// 三维玩具向量:同人同方向+微噪
    fn v(base: [f32; 3], jitter: f32) -> Option<Vec<f32>> {
        Some(vec![base[0] + jitter, base[1] - jitter, base[2]])
    }

    fn sseg(seq: u64, start: u64, end: u64, sp: &str) -> SegInput {
        let mut s = seg(seq, start, end);
        s.old_speaker = Some(sp.into());
        s
    }

    #[test]
    fn stats_group_by_note_speaker_and_skip_unlabeled() {
        let a = [1.0, 0.0, 0.0];
        let b = [0.0, 1.0, 0.0];
        let inputs = vec![
            sseg(0, 0, 10_000, "1"),
            sseg(1, 10_000, 20_000, "1"),
            sseg(2, 20_000, 30_000, "2"),
            seg(3, 30_000, 31_000), // 无 speaker:不入组
        ];
        let embs = vec![v(a, 0.0), v(a, 0.01), v(b, 0.0), v(b, 0.01)];
        let stats = stats_by_speaker(&inputs, &embs, &[], &crate::diar::registry::NearestMatcher);
        assert_eq!(stats.len(), 2, "无 speaker 段不得成组");
        let s1 = stats.iter().find(|s| s.speaker == "1").unwrap();
        assert_eq!(s1.total_ms, 20_000);
        assert_eq!(s1.core_seqs, vec![0, 1]);
        assert!((s1.centroids["mic"][0] - 1.0).abs() < 0.05);
    }

    /// 分信道质心不得跨信道平均(与旧 recluster 同口径)。
    #[test]
    fn stats_export_per_source_centroids() {
        let mut s0 = sseg(0, 0, 10_000, "1");
        s0.source = "mic".into();
        let mut s1 = sseg(1, 10_000, 22_000, "1");
        s1.source = "system".into();
        let embs = vec![Some(vec![1.0, 0.0, 0.0]), Some(vec![0.8, 0.6, 0.0])];
        let stats = stats_by_speaker(&[s0, s1], &embs, &[], &crate::diar::registry::NearestMatcher);
        assert_eq!(stats.len(), 1);
        let st = &stats[0];
        assert!((st.centroids["mic"][0] - 1.0).abs() < 1e-5, "mic 质心保持本信道方向");
        assert!((st.centroids["system"][1] - 0.6).abs() < 1e-5, "system 质心保持本信道方向");
        assert_eq!(st.source_ms["mic"], 10_000);
        assert_eq!(st.source_ms["system"], 12_000);
        assert_eq!(st.total_ms, 22_000);
        assert!(st.seed.is_none());
    }

    /// 无嵌入段计入时长但不进 core_seqs,也不参与质心。
    #[test]
    fn stats_count_unembedded_duration_without_core_membership() {
        let inputs = vec![sseg(0, 0, 10_000, "1"), sseg(1, 10_000, 11_000, "1")];
        let embs = vec![v([1.0, 0.0, 0.0], 0.0), None];
        let stats = stats_by_speaker(&inputs, &embs, &[], &crate::diar::registry::NearestMatcher);
        assert_eq!(stats[0].total_ms, 11_000);
        assert_eq!(stats[0].core_seqs, vec![0]);
    }

    /// 种子近邻带 adopted 标记:过阈值采纳、低于阈值仅留档(与旧 recluster 同口径);
    /// KnnVoteMatcher 的多数票语义同样生效。
    #[test]
    fn stats_seed_neighbor_adoption_matches_matcher_policy() {
        let a = [1.0, 0.0, 0.0];
        let inputs = vec![sseg(0, 0, 10_000, "1"), sseg(1, 10_000, 20_000, "1")];
        let embs = vec![v(a, 0.0), v(a, 0.01)];
        let hit = vec![crate::diar::registry::SeedCluster {
            person: "P1".into(), name: "张三".into(), centroid: vec![1.0, 0.0, 0.0], count: 5, source: "mic".into(),
        }];
        let stats = stats_by_speaker(&inputs, &embs, &hit, &crate::diar::registry::NearestMatcher);
        let (pid, name, sim, adopted) = stats[0].seed.clone().unwrap();
        assert_eq!((pid.as_str(), name.as_str()), ("P1", "张三"));
        assert!(sim >= SEED_ASSIGN_THRESHOLD && adopted);

        let far = vec![crate::diar::registry::SeedCluster {
            person: "P9".into(), name: "王五".into(), centroid: vec![0.5, 0.86, 0.0], count: 5, source: "mic".into(),
        }];
        let stats = stats_by_speaker(&inputs, &embs, &far, &crate::diar::registry::NearestMatcher);
        let (pid, _, sim, adopted) = stats[0].seed.clone().unwrap();
        assert_eq!(pid, "P9");
        assert!(sim < SEED_ASSIGN_THRESHOLD && !adopted, "低于阈值仅留档不采纳");
    }

    // ── 拆分专用聚类(recluster_split) ──

    #[test]
    fn split_keeps_short_speaker_as_own_group() {
        // 常规 recluster 会把 <8s 的簇吞进最近大簇;拆分模式必须保住短发言者。
        let inputs = vec![seg(0, 0, 5000), seg(1, 5000, 10_000), seg(2, 10_000, 13_000)];
        let embs = vec![v([1.0, 0.0, 0.0], 0.01), v([1.0, 0.0, 0.0], -0.01), v([0.0, 1.0, 0.0], 0.0)];
        let sug = recluster_split(&inputs, &embs, &[]);
        assert_eq!(sug.groups.len(), 2, "3 秒短发言者必须独立成组");
        assert!(sug.groups.iter().any(|g| g.member_idx == vec![2]));
        assert!(sug.undetermined_idx.is_empty());
    }

    /// 大组在场时,不像任何大组的短发言者仍独立成组;像某大组的碎片并入它。
    #[test]
    fn split_absorbs_similar_fragment_but_keeps_dissimilar_short_speaker() {
        let mut inputs = Vec::new();
        let mut embs = Vec::new();
        // 大组:8 段 × 5s = 40s(≥ SPLIT_FRAGMENT_MS)
        for i in 0..8u64 {
            inputs.push(seg(i, i * 5000, i * 5000 + 5000));
            embs.push(v([1.0, 0.0, 0.0], 0.01 * (i as f32 % 2.0)));
        }
        // 碎片 A(3s):与大组余弦 ~0.7(≥0.5)→ 并入
        inputs.push(seg(8, 50_000, 53_000));
        embs.push(Some(vec![0.7, 0.71, 0.0]));
        // 碎片 B(3s):正交(0)→ 独立成组
        inputs.push(seg(9, 60_000, 63_000));
        embs.push(v([0.0, 0.0, 1.0], 0.0));
        let sug = recluster_split(&inputs, &embs, &[]);
        assert_eq!(sug.groups.len(), 2, "像的碎片并入大组,不像的独立");
        assert!(sug.groups[0].member_idx.contains(&8), "碎片 A 应并入大组");
        assert_eq!(sug.groups[1].member_idx, vec![9]);
    }

    /// 2026-08-31 现场会回归:同房间远场——每段 = 房间分量 + 说话人分量 + 噪声。
    /// 归一化质心法会把 3 人链式并成 1 组(质心去噪后跨人余弦被抬高),精确平均
    /// 链接分成 3 个纯组。
    #[test]
    fn split_separates_far_field_speakers_sharing_room_signature() {
        // 确定性伪随机(LCG)→ 近似正态(12 个均匀数求和)
        let mut st: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut normal = || -> f32 {
            let mut acc = 0.0f32;
            for _ in 0..12 {
                st = st.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                acc += ((st >> 33) as f32) / (u32::MAX >> 1) as f32;
            }
            acc - 6.0
        };
        const DIM: usize = 64;
        let room: Vec<f32> = (0..DIM).map(|_| normal()).collect();
        let spk: Vec<Vec<f32>> = (0..3).map(|_| (0..DIM).map(|_| normal()).collect()).collect();
        let mut inputs = Vec::new();
        let mut embs = Vec::new();
        let mut truth = Vec::new();
        for k in 0..3usize {
            for i in 0..40u64 {
                let seq = (k as u64) * 40 + i;
                inputs.push(seg(seq, seq * 4000, seq * 4000 + 3000));
                let v: Vec<f32> = (0..DIM).map(|d| 1.6 * room[d] + 1.0 * spk[k][d] + 0.9 * normal()).collect();
                embs.push(Some(v));
                truth.push(k);
            }
        }
        let sug = recluster_split(&inputs, &embs, &[]);
        let big: Vec<&SplitGroup> = sug.groups.iter().filter(|g| g.total_ms >= SPLIT_FRAGMENT_MS).collect();
        assert_eq!(big.len(), 3, "3 个远场说话人应分成 3 个大组,实得 {} 组", sug.groups.len());
        for g in &big {
            let k0 = truth[g.member_idx[0]];
            assert!(g.member_idx.iter().all(|&i| truth[i] == k0), "大组必须纯:一组只含一个人");
        }
    }

    #[test]
    fn split_puts_unembedded_into_undetermined_bucket() {
        // 无嵌入(过短/失败/轨道缺失)不按相邻传播——这簇的段本来就不连续,"相邻"无意义。
        let inputs = vec![seg(0, 0, 5000), seg(1, 5000, 6000), seg(2, 6000, 11_000)];
        let embs = vec![v([1.0, 0.0, 0.0], 0.0), None, v([1.0, 0.0, 0.0], 0.01)];
        let sug = recluster_split(&inputs, &embs, &[]);
        assert_eq!(sug.undetermined_idx, vec![1]);
        assert_eq!(sug.groups.len(), 1);
        assert_eq!(sug.groups[0].member_idx, vec![0, 2]);
    }

    #[test]
    fn split_survives_many_dissimilar_noise_segments() {
        // 几十个互不相似的 2-4s 噪声段:各自成 singleton,不允许被硬上限强并,
        // 也不允许崩(折叠是 UI 层的事)。构造两两正交夹角的稀疏向量。
        let n = 40usize;
        let mut inputs = Vec::new();
        let mut embs: Vec<Option<Vec<f32>>> = Vec::new();
        for i in 0..n {
            inputs.push(seg(i as u64, (i as u64) * 3000, (i as u64) * 3000 + 2500));
            let mut e = vec![0.0f32; n];
            e[i] = 1.0;
            embs.push(Some(e));
        }
        let sug = recluster_split(&inputs, &embs, &[]);
        assert_eq!(sug.groups.len(), n, "互不相似 → 各自成组,不强并");
    }

    /// 甲(2026-08-22):同名强建议合组——两组都笃定是同一人就并;低于阈值仅参考不并。
    #[test]
    fn split_merges_groups_sharing_confident_seed_but_not_weak_ones() {
        let sc = |p: &str, c: Vec<f32>| crate::diar::registry::SeedCluster {
            person: p.into(), name: p.into(), centroid: c, count: 5, source: "mic".into(),
        };
        // 两个互不相似(dot=0 < AHC 阈值)的方向,但都与种子 PX 高相似(0.8+)。
        let a = [1.0, 0.0, 0.0];
        let b = [0.0, 1.0, 0.0];
        let seed_mid = vec![0.7071, 0.7071, 0.0]; // 与 a、b 余弦均 ~0.707 ≥ 0.68
        let inputs = vec![seg(0, 0, 10_000), seg(1, 10_000, 20_000)];
        let embs = vec![v(a, 0.0), v(b, 0.0)];
        let sug = recluster_split(&inputs, &embs, &[sc("PX", seed_mid.clone())]);
        assert_eq!(sug.groups.len(), 1, "同名强建议必须并组");
        assert_eq!(sug.groups[0].member_idx, vec![0, 1]);
        assert_eq!(sug.groups[0].suggested.as_ref().unwrap().0, "PX");

        // 低于阈值的同名建议:仅参考,不并。
        let weak = vec![0.35, 0.35, 0.87]; // 与 a、b 余弦 ~0.35 < 0.68
        let sug = recluster_split(&inputs, &embs, &[sc("PY", weak)]);
        assert_eq!(sug.groups.len(), 2, "弱建议不得并组");
    }

    #[test]
    fn split_suggests_nearest_seed_per_group() {
        let inputs = vec![seg(0, 0, 9000)];
        let embs = vec![v([1.0, 0.0, 0.0], 0.0)];
        let seeds = vec![
            SeedCluster { person: "P7".into(), name: "甲".into(), centroid: vec![0.99, 0.05, 0.0], count: 5, source: "mic".into() },
            SeedCluster { person: "P8".into(), name: "乙".into(), centroid: vec![0.0, 1.0, 0.0], count: 5, source: "mic".into() },
        ];
        let sug = recluster_split(&inputs, &embs, &seeds);
        let (pid, name, sim) = sug.groups[0].suggested.clone().expect("应有建议");
        assert_eq!((pid.as_str(), name.as_str()), ("P7", "甲"));
        assert!(sim > 0.9);
    }
}
