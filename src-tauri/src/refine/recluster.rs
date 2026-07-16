//! A1 离线全局重聚类:AHC 平均链接(质心近似)。纯逻辑,嵌入由调用方提供。
//! 在线单遍聚类(registry.rs)只做录制中临时标签;本模块产终稿。

use crate::diar::registry::{SeedCluster, SEED_ASSIGN_THRESHOLD};

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

pub struct Assignment {
    pub seq: u64,
    pub speaker: String,
    pub name: Option<String>,
    /// 命中的声纹库人物 id(P<n>):有它修订稿才能把改名同步进声纹库。
    pub person: Option<String>,
}

fn normalize(v: &[f32]) -> Option<Vec<f32>> {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !n.is_finite() || n < 1e-6 {
        return None;
    }
    Some(v.iter().map(|x| x / n).collect())
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

pub fn recluster(inputs: &[SegInput], embs: &[Option<Vec<f32>>], seeds: &[SeedCluster]) -> Vec<Assignment> {
    assert_eq!(inputs.len(), embs.len());

    // 1. 建初始簇:每个有嵌入的段各自成簇。
    let mut cls: Vec<Cl> = Vec::new();
    for (i, e) in embs.iter().enumerate() {
        if let Some(u) = e.as_ref().and_then(|v| normalize(v)) {
            cls.push(Cl {
                centroid: u,
                members: vec![i],
                total_ms: inputs[i].end_ms.saturating_sub(inputs[i].start_ms),
            });
        }
    }

    // 2. AHC:每轮找全局最相似簇对,≥ 阈值则合并,否则停。
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
                // 循环恒有 i < j(内层从 i+1 起),故 swap_remove(j) 绝不会把最后一个
                // 元素换到 i 的位置——i 处元素在移除 j 前后都稳定不变,可直接借用 cls[i]。
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

    // 3. 碎片治理:总时长 < MIN_CLUSTER_MS 的簇无条件并入质心最近的大簇;若全场
    //    无大簇则保留(避免把仅有的簇也吞并成 0 个)。
    //    注意:这里用 Vec::remove(有序删除、O(n) 平移)而非 swap_remove——下面的
    //    tgt 下标修正公式(`tgt > frag_idx ? tgt-1 : tgt`)只对有序删除成立;
    //    若改回 swap_remove,最后一个元素会被换到 frag_idx 位置,该公式将失配
    //    导致合并到错误的簇(当 tgt 恰为移除前的最后一个下标时最明显)。n ≤ 450,
    //    有序删除的平移开销可忽略。
    loop {
        let Some(frag_idx) = cls
            .iter()
            .enumerate()
            .filter(|(_, c)| c.total_ms < MIN_CLUSTER_MS)
            .min_by_key(|(_, c)| c.total_ms)
            .map(|(i, _)| i)
        else {
            break;
        };
        let bigs: Vec<usize> =
            (0..cls.len()).filter(|&i| i != frag_idx && cls[i].total_ms >= MIN_CLUSTER_MS).collect();
        if bigs.is_empty() {
            break; // 全场无大簇,保留碎片,防止无限循环/清空聚类结果
        }
        let tgt = *bigs
            .iter()
            .max_by(|&&a, &&b| {
                dot(&cls[frag_idx].centroid, &cls[a].centroid)
                    .total_cmp(&dot(&cls[frag_idx].centroid, &cls[b].centroid))
            })
            .unwrap();
        let f = cls.remove(frag_idx);
        let tgt = if tgt > frag_idx { tgt - 1 } else { tgt };
        let t = &mut cls[tgt];
        t.centroid = merge_centroid(t, &f);
        t.members.extend(f.members);
        t.total_ms += f.total_ms;
    }

    // 4. 按总时长降序编号 R1..Rk。
    cls.sort_by(|a, b| b.total_ms.cmp(&a.total_ms));

    // 5. 种子命名/认人:簇质心对每个 seed 算余弦,最高且 ≥ SEED_ASSIGN_THRESHOLD 者
    //    取其 (person, name)。未命名的库人物也参与——person id 是修订稿改名同步进
    //    声纹库的锚点,不能因为还没名字就丢掉身份。
    let matches: Vec<Option<(String, String)>> = cls
        .iter()
        .map(|c| {
            seeds
                .iter()
                .filter_map(|s| normalize(&s.centroid).map(|u| (s, dot(&c.centroid, &u))))
                .filter(|(_, sim)| *sim >= SEED_ASSIGN_THRESHOLD)
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(s, _)| (s.person.clone(), s.name.clone()))
        })
        .collect();

    // 6. 输出:先落有嵌入段的簇标签,再给无嵌入段找时间上最近的有标签邻段(前后
    //    各找,取 gap 小者);全场无簇(labeled 为空)时保留 old_speaker 原值。
    let mut label: Vec<Option<usize>> = vec![None; inputs.len()];
    for (k, c) in cls.iter().enumerate() {
        for &m in &c.members {
            label[m] = Some(k);
        }
    }
    let labeled: Vec<usize> = (0..inputs.len()).filter(|&i| label[i].is_some()).collect();

    (0..inputs.len())
        .map(|i| {
            let k = label[i].or_else(|| {
                labeled
                    .iter()
                    .min_by_key(|&&j| {
                        let (a, b) = (&inputs[i], &inputs[j]);
                        // 无重叠段的时间 gap;有重叠或相邻则为 0。两个分支互斥且各自
                        // 保证减法不下溢(只在确认 end<=start 时才相减)。
                        if b.end_ms <= a.start_ms {
                            a.start_ms - b.end_ms
                        } else if a.end_ms <= b.start_ms {
                            b.start_ms - a.end_ms
                        } else {
                            0
                        }
                    })
                    .and_then(|&j| label[j])
            });
            match k {
                Some(k) => Assignment {
                    seq: inputs[i].seq,
                    speaker: format!("R{}", k + 1),
                    name: matches[k]
                        .as_ref()
                        .map(|(_, n)| n.clone())
                        .filter(|n| !n.is_empty()),
                    person: matches[k].as_ref().map(|(p, _)| p.clone()),
                },
                None => Assignment {
                    seq: inputs[i].seq,
                    speaker: inputs[i].old_speaker.clone().unwrap_or_else(|| "R1".into()),
                    name: None,
                    person: None,
                },
            }
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

    #[test]
    fn two_speakers_separate_and_fragments_absorbed() {
        let a = [1.0, 0.0, 0.0];
        let b = [0.0, 1.0, 0.0];
        let inputs = vec![
            seg(0, 0, 10_000), seg(1, 10_000, 20_000), seg(2, 20_000, 30_000), // A 30s
            seg(3, 30_000, 40_000), seg(4, 40_000, 50_000),                     // B 20s
            seg(5, 50_000, 52_000),                                             // A 碎片 2s(独立会成小簇)
        ];
        let embs = vec![v(a, 0.01), v(a, 0.02), v(a, 0.0), v(b, 0.01), v(b, 0.0), v(a, 0.03)];
        let out = recluster(&inputs, &embs, &[]);
        let l = |q: u64| out.iter().find(|x| x.seq == q).unwrap().speaker.clone();
        assert_eq!(l(0), l(1));
        assert_eq!(l(0), l(2));
        assert_eq!(l(3), l(4));
        assert_ne!(l(0), l(3));
        assert_eq!(l(5), l(0), "2s 碎片簇应并入最近大簇 A");
        assert_eq!(l(0), "R1", "A 总时长最长应为 R1");
    }

    #[test]
    fn short_segment_without_embedding_follows_nearest_neighbor() {
        let a = [1.0, 0.0, 0.0];
        let inputs = vec![seg(0, 0, 10_000), seg(1, 10_100, 11_000), seg(2, 20_000, 30_000)];
        let embs = vec![v(a, 0.0), None, v(a, 0.01)];
        let out = recluster(&inputs, &embs, &[]);
        assert_eq!(out[1].speaker, out[0].speaker, "无嵌入短段跟时间最近邻(gap 100ms < 9s)");
    }

    #[test]
    fn seed_naming_applies_above_threshold() {
        let a = [1.0, 0.0, 0.0];
        let inputs = vec![seg(0, 0, 10_000), seg(1, 10_000, 20_000)];
        let embs = vec![v(a, 0.0), v(a, 0.01)];
        let seeds = vec![crate::diar::registry::SeedCluster {
            person: "P1".into(), name: "张三".into(), centroid: vec![1.0, 0.0, 0.0], count: 5,
        }];
        let out = recluster(&inputs, &embs, &seeds);
        assert_eq!(out[0].name.as_deref(), Some("张三"));
        assert_eq!(out[0].person.as_deref(), Some("P1"), "命中种子须带出人物 id");
    }

    /// 未命名的库人物命中也要建立 person 关联(改名同步的锚点),name 保持 None。
    #[test]
    fn unnamed_seed_links_person_without_name() {
        let a = [1.0, 0.0, 0.0];
        let inputs = vec![seg(0, 0, 10_000), seg(1, 10_000, 20_000)];
        let embs = vec![v(a, 0.0), v(a, 0.01)];
        let seeds = vec![crate::diar::registry::SeedCluster {
            person: "P4".into(), name: String::new(), centroid: vec![1.0, 0.0, 0.0], count: 5,
        }];
        let out = recluster(&inputs, &embs, &seeds);
        assert!(out[0].name.is_none(), "未命名人物不产生名字");
        assert_eq!(out[0].person.as_deref(), Some("P4"));
    }

    #[test]
    fn all_none_embeddings_keeps_old_speakers() {
        let mut i0 = seg(0, 0, 1000); i0.old_speaker = Some("S8".into());
        let out = recluster(&[i0], &[None], &[]);
        assert_eq!(out[0].speaker, "S8");
    }
}
