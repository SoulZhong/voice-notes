//! 说话人归属(离线):复用实时链路的 SpeakerRegistry 在线聚类,但**只读声纹库**——
//! 不设 enroller、不 enroll_pending、不把结果回写库(三期 spec §偏离①:历史笔记的
//! 旧音频已污染过一轮库,重转写结果回写等于二次污染)。

use crate::diar::registry::{ClusterSnapshot, SeedCluster, SpeakerInfo, SpeakerRegistry};
use crate::diar::SpeakerEmbedder;

pub struct RecSeg {
    pub source: String,
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub samples: Vec<f32>,
    pub rms: f32,
}

/// 逐段嵌入 + 在线归簇(段须已按时间排序,在线聚类对顺序敏感)。收尾统一套用场内
/// 合并映射。mixed=true 时:种子 source 改写为 "mixed"(0.68 同信道快路对全部种子
/// 生效)+ 关闭 z 通道(spec §降级口径)。
pub fn assign_clusters(
    segs: &[RecSeg],
    embedder: &mut Option<Box<dyn SpeakerEmbedder>>,
    seeds: Vec<SeedCluster>,
    mixed: bool,
) -> (Vec<Option<String>>, Vec<SpeakerInfo>, Vec<ClusterSnapshot>) {
    let Some(embedder) = embedder.as_mut() else {
        return (vec![None; segs.len()], Vec::new(), Vec::new());
    };
    let seeds: Vec<SeedCluster> = if mixed {
        seeds.into_iter()
            .map(|s| SeedCluster { source: "mixed".into(), ..s })
            .collect()
    } else {
        seeds
    };
    let mut registry = SpeakerRegistry::with_seeds(&[], &seeds);
    if mixed {
        registry.disable_seed_z();
    }
    let mut clusters: Vec<Option<String>> = Vec::with_capacity(segs.len());
    for seg in segs {
        let seg_key = format!("{}:{}", seg.source, seg.start_ms);
        let assigned = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            embedder.embed(&seg.samples)
        })) {
            Ok(Ok(emb)) => registry.assign_tracked(&emb, &seg.source, seg.samples.len(), &seg_key),
            Ok(Err(err)) => {
                eprintln!("重转写声纹提取失败({}:{}ms): {err}", seg.source, seg.start_ms);
                None
            }
            Err(_) => {
                eprintln!("重转写声纹提取 panic({}:{}ms),该段无标签", seg.source, seg.start_ms);
                None
            }
        };
        clusters.push(assigned);
    }
    // 场内簇合并:把已分配的 loser id 统一映射到 winner(传递闭包)。
    let merges = registry.take_merges();
    if !merges.is_empty() {
        let resolve = |id: &str| -> String {
            let mut cur = id.to_string();
            // 链最长不过合并次数,防环兜底按次数上限走
            for _ in 0..merges.len() {
                match merges.iter().find(|(loser, _)| *loser == cur) {
                    Some((_, winner)) => cur = winner.clone(),
                    None => break,
                }
            }
            cur
        };
        for c in clusters.iter_mut() {
            if let Some(id) = c {
                *c = Some(resolve(id));
            }
        }
    }
    (clusters, registry.speakers(), registry.snapshot())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diar::registry::SeedCluster;
    use crate::diar::SpeakerEmbedder;

    /// 脚本嵌入器:按段顺序吐预设向量。
    struct ScriptEmbedder(Vec<Vec<f32>>, usize);
    impl SpeakerEmbedder for ScriptEmbedder {
        fn embed(&mut self, _samples: &[f32]) -> anyhow::Result<Vec<f32>> {
            let v = self.0[self.1.min(self.0.len() - 1)].clone();
            self.1 += 1;
            Ok(v)
        }
    }

    fn rec(source: &str, start: u64, dur_ms: u64) -> RecSeg {
        RecSeg {
            source: source.into(), text: "x".into(), start_ms: start, end_ms: start + dur_ms,
            samples: vec![0.1; (dur_ms * 16) as usize], rms: 0.1,
        }
    }

    fn unit(i: usize, dim: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; dim]; v[i] = 1.0; v
    }

    /// 双轨模式:同信道种子裸分命中 → 段拿到关联库人物的簇;registry 无 enroller,
    /// speakers() 带 person。
    #[test]
    fn dual_mode_seed_hit_yields_owned_cluster() {
        let seeds = vec![SeedCluster {
            person: "P1".into(), name: "张三".into(), centroid: unit(0, 8), count: 10, source: "mic".into(),
        }];
        let segs = vec![rec("mic", 0, 3000)]; // 3s=48000 样本,过 SEED_MIN_SAMPLES
        let mut emb: Option<Box<dyn SpeakerEmbedder>> = Some(Box::new(ScriptEmbedder(vec![unit(0, 8)], 0)));
        let (clusters, infos, _snaps) = assign_clusters(&segs, &mut emb, seeds, false);
        let id = clusters[0].clone().expect("裸分 1.0 必命中种子");
        let info = infos.iter().find(|i| i.id == id).unwrap();
        assert_eq!(info.person.as_deref(), Some("P1"));
        assert_eq!(info.name.as_deref(), Some("张三"));
    }

    /// 无 embedder:全段无簇,infos 空——降级由 finalize 的继承兜底,不 panic。
    #[test]
    fn missing_embedder_degrades_to_no_clusters() {
        let segs = vec![rec("mic", 0, 3000)];
        let mut emb: Option<Box<dyn SpeakerEmbedder>> = None;
        let (clusters, infos, snaps) = assign_clusters(&segs, &mut emb, vec![], false);
        assert_eq!(clusters, vec![None]);
        assert!(infos.is_empty() && snaps.is_empty());
    }

    /// mixed 模式:种子 source 被改写为 "mixed",同信道 0.68 快路对 mixed 段生效。
    #[test]
    fn mixed_mode_rewrites_seed_source_for_fast_path() {
        let seeds = vec![SeedCluster {
            person: "P1".into(), name: "张三".into(), centroid: unit(0, 8), count: 10, source: "mic".into(),
        }];
        let segs = vec![rec("mixed", 0, 3000)];
        let mut emb: Option<Box<dyn SpeakerEmbedder>> = Some(Box::new(ScriptEmbedder(vec![unit(0, 8)], 0)));
        let (clusters, infos, _snaps) = assign_clusters(&segs, &mut emb, seeds, true);
        let id = clusters[0].clone().expect("source 改写后 mixed 段应走同信道快路命中");
        assert_eq!(infos.iter().find(|i| i.id == id).unwrap().person.as_deref(), Some("P1"));
    }
}
