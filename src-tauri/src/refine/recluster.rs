//! A1 离线全局重聚类:AHC 平均链接(质心近似)。纯逻辑,嵌入由调用方提供。
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
    centroid: Vec<f32>,   // 单位化
    members: Vec<usize>,  // inputs 下标
    total_ms: u64,
}

/// 合并两簇质心:按成员数加权平均后再归一化。这是平均链接(average-linkage)的
/// 质心近似——真实"全体样本重新求均值"与"两质心按成员数加权"在数学上等价当
/// 且仅当两侧质心本身就是各自成员的精确均值;多轮合并累积后会有轻微漂移误差,
/// 但对聚类判定(是否越过阈值)影响可忽略,换来的是不必存储/重扫全部原始向量。
fn merge_centroid(a: &Cl, b: &Cl) -> Vec<f32> {
    let wa = a.members.len() as f32;
    let wb = b.members.len() as f32;
    let mixed: Vec<f32> = a.centroid.iter().zip(&b.centroid).map(|(x, y)| x * wa + y * wb).collect();
    normalize(&mixed).unwrap_or_else(|| a.centroid.clone())
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
/// - **不做碎片吞并**(MIN_CLUSTER_MS 不生效):混杂簇里的短发言者正是要分出来的人,
///   常规路径会把 <8s 的簇无条件并进最近大簇,功能等于没有(codex 设计轮一 P1⑦)
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
                centroid: u,
                members: vec![i],
                total_ms: inputs[i].end_ms.saturating_sub(inputs[i].start_ms),
            }),
            None => undetermined_idx.push(i),
        }
    }
    // AHC(与常规同一内核同一阈值),但到此为止——没有碎片吞并、没有传播。
    loop {
        let mut best: Option<(usize, usize, f32)> = None;
        for i in 0..cls.len() {
            for j in (i + 1)..cls.len() {
                let sim = dot(&cls[i].centroid, &cls[j].centroid);
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
                a.centroid = merge_centroid(a, &b);
                a.members.extend(b.members);
                a.total_ms += b.total_ms;
            }
            _ => break,
        }
    }
    // 同名强建议合组(2026-08-22 用户拍板「甲」):多个组的最近种子指向同一人且都
    // 过 SEED_ASSIGN_THRESHOLD,拆开自相矛盾(724 段大簇实测碎成 15 组、5 组同标
    // 「像是徐万振?」)。声纹既然笃定是同一人,就并成一组;低于阈值的仅是参考,
    // 不并(种子只是建议,不越权)。
    let seed_of = |c: &Cl| -> Option<(String, String, f32)> {
        // 最像的种子:同一 person 多种子取 max(与 seed_clusters 的多种子语义一致)。
        let mut best: Option<(String, String, f32)> = None;
        for sd in seeds {
            if let Some(u) = normalize(&sd.centroid) {
                let sim = dot(&c.centroid, &u);
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
                    t.centroid = merge_centroid(t, &c);
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
