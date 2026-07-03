//! 在线增量声纹聚类:两路(mic/system)嵌入汇入同一 Registry,
//! 得全局「S1..Sn」。纯逻辑、无模型依赖、单线程持有于 ASR worker。

use std::collections::BTreeSet;

/// 归簇阈值(余弦)。首轮真实会议校准(10+人短句场景)调整:原 0.55 下不同人被吸入同簇。
pub const ASSIGN_THRESHOLD: f32 = 0.62;
/// 簇间合并阈值(余弦,高于归簇阈值防过度合并)。首轮真实会议校准(10+人短句场景)调整:原 0.68 下触发过度合并。
pub const MERGE_THRESHOLD: f32 = 0.74;
/// 低于此样本数(16kHz)的段不允许新建簇(短段声纹不可靠)。首轮真实会议校准(10+人短句场景)调整:原 16000(1.0s) 下 0.9s 真句子被拦截。
pub const MIN_NEW_CLUSTER_SAMPLES: usize = 9600; // 0.6s
/// 每 N 次 assign 做一次簇间合并检查。
pub const MERGE_CHECK_INTERVAL: u64 = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerInfo {
    pub id: String,
    pub sources: BTreeSet<String>,
}

/// 一簇的可导出快照(质心/计数/来源),用于跨会话续接说话人编号(P4.5)。
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterSnapshot {
    pub id: String,
    pub centroid: Vec<f32>,
    pub count: u64,
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
            .map(|c| (dot(&c.centroid, &unit), c))
            .max_by(|(a, _), (b, _)| a.total_cmp(b));

        if let Some((sim, cluster)) = best {
            if sim >= ASSIGN_THRESHOLD {
                cluster.sources.insert(source.to_string());
                // 短段不更新质心、不增count(短段声纹噪声大,防拖歪质心)
                if num_samples >= MIN_NEW_CLUSTER_SAMPLES {
                    // 质心 running mean(在单位向量上),再归一化
                    let n = cluster.count as f32;
                    for (ci, ui) in cluster.centroid.iter_mut().zip(&unit) {
                        *ci = (*ci * n + ui) / (n + 1.0);
                    }
                    if let Some(renorm) = normalize(&cluster.centroid) {
                        cluster.centroid = renorm;
                    }
                    cluster.count += 1;
                }
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

    /// 导出全部簇的质心快照(供会话结束时交给 DiarEvent::Snapshot,P4.5 续录铺底)。
    pub fn snapshot(&self) -> Vec<ClusterSnapshot> {
        self.clusters
            .iter()
            .map(|c| ClusterSnapshot {
                id: c.id.clone(),
                centroid: c.centroid.clone(),
                count: c.count,
                sources: c.sources.clone(),
            })
            .collect()
    }

    /// 从质心快照重建 registry:编号续接(解析所有 "S{n}" 取最大 n,next_id = n+1)；
    /// 质心为空的项不建簇但计入编号。空切片 ≡ new()。
    pub fn from_snapshot(snaps: &[ClusterSnapshot]) -> Self {
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
                });
            }
        }
        Self { clusters, next_id, assigns: 0, pending_merges: Vec::new() }
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
}
