//! 在线增量声纹聚类:两路(mic/system)嵌入汇入同一 Registry,
//! 得全局「S1..Sn」。纯逻辑、无模型依赖、单线程持有于 ASR worker。

use std::collections::BTreeSet;

/// 归簇阈值(余弦)。fixture 校准初值,冒烟后可调。
pub const ASSIGN_THRESHOLD: f32 = 0.55;
/// 簇间合并阈值(余弦,高于归簇阈值防过度合并)。fixture 校准初值。
pub const MERGE_THRESHOLD: f32 = 0.68;
/// 低于此样本数(16kHz)的段不允许新建簇(短段声纹不可靠)。
pub const MIN_NEW_CLUSTER_SAMPLES: usize = 16000;
/// 每 N 次 assign 做一次簇间合并检查。
pub const MERGE_CHECK_INTERVAL: u64 = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerInfo {
    pub id: String,
    pub sources: BTreeSet<String>,
}

struct Cluster {
    id: String,
    /// 成员单位向量的均值,再归一化。
    centroid: Vec<f32>,
    count: u64,
    sources: BTreeSet<String>,
}

pub struct SpeakerRegistry {
    clusters: Vec<Cluster>,
    next_id: u32,
    assigns: u64,
    pending_merges: Vec<(String, String)>,
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
        Self { clusters: Vec::new(), next_id: 1, assigns: 0, pending_merges: Vec::new() }
    }

    /// 归簇:与各质心比余弦,≥ 阈值归入最相似簇并更新质心;
    /// 否则段够长才新建簇。返回说话人 id;不可用嵌入/短段无归属返回 None。
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
            .map(|c| (dot(&c.centroid, &unit), c))
            .max_by(|(a, _), (b, _)| a.total_cmp(b));

        if let Some((sim, cluster)) = best {
            if sim >= ASSIGN_THRESHOLD {
                // 质心 running mean(在单位向量上),再归一化
                let n = cluster.count as f32;
                for (ci, ui) in cluster.centroid.iter_mut().zip(&unit) {
                    *ci = (*ci * n + ui) / (n + 1.0);
                }
                if let Some(renorm) = normalize(&cluster.centroid) {
                    cluster.centroid = renorm;
                }
                cluster.count += 1;
                cluster.sources.insert(source.to_string());
                return Some(cluster.id.clone());
            }
        }

        if num_samples < MIN_NEW_CLUSTER_SAMPLES {
            return None; // 短段不建簇
        }
        let id = format!("S{}", self.next_id);
        self.next_id += 1;
        self.clusters.push(Cluster {
            id: id.clone(),
            centroid: unit,
            count: 1,
            sources: BTreeSet::from([source.to_string()]),
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
                    if dot(&self.clusters[i].centroid, &self.clusters[j].centroid) >= MERGE_THRESHOLD {
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
            winner.sources.extend(loser.sources.iter().cloned());
            self.pending_merges.push((loser.id.clone(), winner.id.clone()));
        }
    }

    pub fn speakers(&self) -> Vec<SpeakerInfo> {
        self.clusters
            .iter()
            .map(|c| SpeakerInfo { id: c.id.clone(), sources: c.sources.clone() })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三维正交基方便构造:e1/e2 相似度 0,混合向量可控。
    fn v(x: f32, y: f32, z: f32) -> Vec<f32> {
        vec![x, y, z]
    }
    const LONG: usize = 32000; // 2s,足以建簇

    #[test]
    fn first_assign_creates_s1() {
        let mut r = SpeakerRegistry::new();
        assert_eq!(r.assign(&v(1.0, 0.0, 0.0), "mic", LONG), Some("S1".into()));
        let sp = r.speakers();
        assert_eq!(sp.len(), 1);
        assert_eq!(sp[0].id, "S1");
        assert!(sp[0].sources.contains("mic"));
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
        // 两簇初始正交
        for _ in 0..6 {
            r.assign(&v(1.0, 0.0, 0.0), "mic", LONG); // S1(大簇)
        }
        r.assign(&v(0.0, 1.0, 0.0), "system", LONG); // S2(小簇)
        assert!(r.take_merges().is_empty(), "正交簇不该合并");
        // 把 S2 的质心喂到与 S1 高度相似
        for _ in 0..12 {
            r.assign(&v(0.9, 0.435, 0.0), "system", LONG); // 与 e1 余弦≈0.9 → 落 S1? 不——
            // 注:该向量与 S1 质心(≈e1)余弦 ≈ 0.9 > ASSIGN_THRESHOLD,会直接归入 S1,
            // 这正是在线聚类的常态;为构造"两簇漂移到相似"的场景,直接喂与 S2 相似、
            // 同时逐渐偏向 e1 的序列:
        }
        let mut r = SpeakerRegistry::new();
        for _ in 0..6 {
            r.assign(&v(1.0, 0.0, 0.0), "mic", LONG); // S1(大簇)
        }
        r.assign(&v(0.30, 0.954, 0.0), "system", LONG); // S2(小簇)
        assert!(r.take_merges().is_empty(), "正交簇不该合并");
        // S2 的后续成员逐渐偏向 e1
        for k in 1..=10 {
            let t = 0.30 + 0.05 * k as f32;
            let y = (1.0 - t * t).max(0.0).sqrt();
            r.assign(&v(t, y, 0.0), "system", LONG);
        }
        // 继续推动 S2 质心更接近 S1
        for _ in 0..12 {
            r.assign(&v(0.90, 0.436, 0.0), "system", LONG);
        }
        // 触发周期性合并检查(take_merges 内部在每 MERGE_CHECK_INTERVAL 次 assign 后检测)
        let merges = r.take_merges();
        assert_eq!(merges.len(), 1, "漂移后两簇应合并");
        let (loser, winner) = &merges[0];
        assert_eq!(winner, "S1", "小簇并入大簇");
        assert_eq!(loser, "S2");
        assert_eq!(r.speakers().len(), 1);
        // 合并后 sources 汇总
        assert!(r.speakers()[0].sources.contains("system"));
    }

    #[test]
    fn zero_or_mismatched_dim_embedding_returns_none() {
        let mut r = SpeakerRegistry::new();
        assert_eq!(r.assign(&[], "mic", LONG), None);
        r.assign(&v(1.0, 0.0, 0.0), "mic", LONG);
        assert_eq!(r.assign(&[1.0, 0.0], "mic", LONG), None, "维度不符丢弃");
        assert_eq!(r.assign(&[0.0, 0.0, 0.0], "mic", LONG), None, "零向量丢弃");
    }
}
