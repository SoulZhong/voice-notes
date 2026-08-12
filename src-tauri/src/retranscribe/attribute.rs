//! 说话人归属(离线):复用实时链路的 SpeakerRegistry 在线聚类,但**只读声纹库**——
//! 不设 enroller、不 enroll_pending、不把结果回写库(三期 spec §偏离①:历史笔记的
//! 旧音频已污染过一轮库,重转写结果回写等于二次污染)。

use crate::diar::registry::{ClusterSnapshot, SeedCluster, SpeakerInfo, SpeakerRegistry};
use crate::diar::SpeakerEmbedder;
use crate::store::{SegmentRecord, SpeakerMeta};
use std::collections::BTreeMap;

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
    speaker_match: &str,
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
    // 说话人识别方法(settings.speaker_match):与实时路同一策略注册表。
    registry.set_matcher(crate::diar::registry::matcher_from_key(speaker_match));
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

/// 继承兜底的时间重叠下限(相对判定口径同 overlap_fraction)。
pub const INHERIT_OVERLAP_MIN: f32 = 0.3;

#[derive(Debug, Default)]
pub struct FinalizeStats {
    pub seed_matched: usize,
    pub inherited: usize,
}

/// 归属定稿:种子命中保簇,未命中按时间重叠继承旧人工归属,并重建 speakers 表。
/// 规则见本文件头与三期 spec §说话人归属;撞号/同人合流的取舍在测试里逐条锁死。
pub fn finalize_speakers(
    segs: &[RecSeg],
    clusters: &[Option<String>],
    infos: &[SpeakerInfo],
    snaps: &[ClusterSnapshot],
    old_segs: &[SegmentRecord],
    old_speakers: &BTreeMap<String, SpeakerMeta>,
) -> (Vec<Option<String>>, BTreeMap<String, SpeakerMeta>, FinalizeStats) {
    let info_by_id: BTreeMap<&str, &SpeakerInfo> = infos.iter().map(|i| (i.id.as_str(), i)).collect();
    let snap_by_id: BTreeMap<&str, &ClusterSnapshot> = snaps.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut stats = FinalizeStats::default();

    // 第一遍:每段定成三种之一——Cluster(簇 id)/ Inherit(旧 speaker id)/ None。
    enum Pick { Cluster(String), Inherit(String), Nothing }
    let picks: Vec<Pick> = segs.iter().zip(clusters).map(|(seg, cluster)| {
        if let Some(id) = cluster {
            if info_by_id.get(id.as_str()).is_some_and(|i| i.person.is_some()) {
                stats.seed_matched += 1;
                return Pick::Cluster(id.clone());
            }
        }
        // 继承候选:同 source 优先(mixed 与任意 source 比),取重叠占比最大者。
        // overlap_fraction 分母取第一实参(seg)自身时长——分母语义是「新段
        // 被旧段盖住的比例」,故新段参数必须放第一位,不能反过来按旧段时长算。
        let candidate = |same_source: bool| {
            old_segs.iter()
                .filter(|o| o.speaker.is_some())
                .filter(|o| !same_source || o.source == seg.source || seg.source == "mixed")
                .map(|o| (crate::session::overlap_fraction(seg.start_ms, seg.end_ms, o.start_ms, o.end_ms), o))
                .filter(|(f, _)| *f >= INHERIT_OVERLAP_MIN)
                .max_by(|(a, _), (b, _)| a.total_cmp(b))
                .map(|(_, o)| o)
        };
        let hit = candidate(true).or_else(|| candidate(false));
        if let Some(old) = hit {
            let sid = old.speaker.as_ref().unwrap();
            if old_speakers.get(sid).is_some_and(|m| !m.name.is_empty() || m.person_id.is_some()) {
                stats.inherited += 1;
                return Pick::Inherit(sid.clone());
            }
        }
        match cluster {
            Some(id) => Pick::Cluster(id.clone()),
            None => Pick::Nothing,
        }
    }).collect();

    // 第二遍:定 id 映射并建表。
    //
    // Fix 2(codex 第二轮,根治撞号预写污染):以前只给"继承的旧 id"重编号避让新簇,
    // 落盘的簇 id 本身原样沿用聚类器吐出的 "S{n}"——它与旧表的编号空间是同一套
    // "S{n}"记法,天然可能撞号(旧表 S1、新簇也叫 S1,但是不同人)。commit.rs §3
    // 的并集预写在"冲突键取新值"时会用新簇的空 name 覆盖旧 S1 的人工归属,
    // 一旦 rename(commit 步骤 4)失败,旧稿说话人被张冠李戴,击穿"Err ⇒ 原稿未动"
    // 承诺。根治办法不是选边,而是让新表与旧表键域天生不相交:全部落盘 id(簇 id
    // 与继承重编号 id)都从 `base = max(旧表全部 id 的数字部分)` 之后起跳,commit.rs
    // 的并集因此永远无冲突键(见该文件 §3 断言注释)。
    let numeric = |s: &str| s.strip_prefix('S').and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
    let base = old_speakers.keys().map(|k| numeric(k)).max().unwrap_or(0);
    // 簇 id 按数字升序稳定映射到 S{base+1}, S{base+2}, ...——BTreeSet<&str> 按字典序
    // 排列会把 "S10" 排在 "S2" 前面,必须显式按数字排序才是确定性可测的映射。
    let mut used_clusters: Vec<&str> = picks.iter()
        .filter_map(|p| match p { Pick::Cluster(id) => Some(id.as_str()), _ => None })
        .collect();
    used_clusters.sort_by_key(|id| numeric(id));
    used_clusters.dedup();
    let mut cluster_map: BTreeMap<String, String> = BTreeMap::new();
    let mut next = base;
    for old_id in &used_clusters {
        next += 1;
        cluster_map.insert(old_id.to_string(), format!("S{next}"));
    }
    let mut table: BTreeMap<String, SpeakerMeta> = BTreeMap::new();
    for old_id in &used_clusters {
        let new_id = &cluster_map[*old_id];
        let info = info_by_id.get(old_id);
        let snap = snap_by_id.get(old_id);
        table.insert(new_id.clone(), SpeakerMeta {
            name: info.and_then(|i| i.name.clone()).unwrap_or_default(),
            sources: info.map(|i| i.sources.iter().cloned().collect()).unwrap_or_default(),
            centroid: snap.map(|s| s.centroid.clone()),
            count: snap.map(|s| s.count).unwrap_or(0),
            person_id: info.and_then(|i| i.person.clone()),
        });
    }
    // 继承 id 的重编号起点从同一个计数器(`next`)继续,不重新从 base 起跳——保证
    // 继承 id 与刚分配的簇 id 也不相撞(两者都已经在 base 之上的同一段连续区间里)。
    let mut inherit_map: BTreeMap<String, String> = BTreeMap::new();
    for p in &picks {
        let Pick::Inherit(old_id) = p else { continue };
        if inherit_map.contains_key(old_id) {
            continue;
        }
        let old_meta = &old_speakers[old_id];
        // 同人合流:旧归属关联的库人物已被某个簇命中 → 直接用那个簇的**新** id
        // (table 此刻的键已经是重映射后的簇 id,天然拿到正确值,无需额外换算)。
        let unified = old_meta.person_id.as_ref().and_then(|pid| {
            table.iter().find(|(_, m)| m.person_id.as_deref() == Some(pid)).map(|(k, _)| k.clone())
        });
        let new_id = match unified {
            Some(id) => id,
            None => {
                next += 1;
                let id = format!("S{next}");
                table.insert(id.clone(), old_meta.clone());
                id
            }
        };
        inherit_map.insert(old_id.clone(), new_id);
    }
    let speakers = picks.iter().map(|p| match p {
        Pick::Cluster(id) => Some(cluster_map[id].clone()),
        Pick::Inherit(old_id) => Some(inherit_map[old_id].clone()),
        Pick::Nothing => None,
    }).collect();
    (speakers, table, stats)
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
        let (clusters, infos, _snaps) = assign_clusters(&segs, &mut emb, seeds, false, "nearest");
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
        let (clusters, infos, snaps) = assign_clusters(&segs, &mut emb, vec![], false, "nearest");
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
        let (clusters, infos, _snaps) = assign_clusters(&segs, &mut emb, seeds, true, "nearest");
        let id = clusters[0].clone().expect("source 改写后 mixed 段应走同信道快路命中");
        assert_eq!(infos.iter().find(|i| i.id == id).unwrap().person.as_deref(), Some("P1"));
    }

    use crate::store::{SegmentRecord, SpeakerMeta};
    use std::collections::BTreeMap;

    fn old_seg(seq: u64, source: &str, start: u64, end: u64, speaker: Option<&str>) -> SegmentRecord {
        SegmentRecord {
            seq, source: source.into(), text: "旧".into(), start_ms: start, end_ms: end,
            speaker: speaker.map(String::from), rms: None,
        }
    }

    fn named_meta(name: &str, person: Option<&str>) -> SpeakerMeta {
        SpeakerMeta {
            name: name.into(), sources: vec!["mic".into()], centroid: Some(vec![1.0]),
            count: 5, person_id: person.map(String::from),
        }
    }

    fn info(id: &str, person: Option<&str>, name: Option<&str>) -> SpeakerInfo {
        SpeakerInfo {
            id: id.into(), sources: std::collections::BTreeSet::from(["mic".to_string()]),
            person: person.map(String::from), name: name.map(String::from),
        }
    }

    fn snap(id: &str) -> ClusterSnapshot {
        ClusterSnapshot {
            id: id.into(), centroid: vec![1.0], count: 3,
            sources: std::collections::BTreeSet::from(["mic".to_string()]),
            person: None, total_ms: 5000,
        }
    }

    /// Fix 2(codex 第二轮,原名 inherited_old_id_renumbered_on_collision——语义已变,
    /// 改名如实反映):旧表有 S1(张三),落盘的新簇 id 必须整体避让旧表键域,不再是
    /// "簇 id 原样、只有继承 id 重编号"。旧表 max 数字部分是 1 → 新簇 S1 落盘为 S2 起,
    /// 继承的旧 S1 接着编到 S3,两者与旧表键域两两不相交。
    #[test]
    fn cluster_and_inherited_ids_avoid_old_table_domain_on_collision() {
        let segs = vec![rec("mic", 0, 2000), rec("mic", 10_000, 2000)];
        // 段0 归新簇 S1(无 person);段1 无簇
        let clusters = vec![Some("S1".to_string()), None];
        let infos = vec![info("S1", None, None)];
        let snaps = vec![snap("S1")];
        let old_segs = vec![old_seg(1, "mic", 10_000, 12_000, Some("S1"))];
        let old_speakers = BTreeMap::from([("S1".to_string(), named_meta("张三", None))]);
        let (speakers, table, stats) =
            finalize_speakers(&segs, &clusters, &infos, &snaps, &old_segs, &old_speakers);
        assert_eq!(speakers[0].as_deref(), Some("S2"), "新簇 S1 避让旧表 S1,落盘为 S2");
        let inherited = speakers[1].clone().expect("重叠继承应命中");
        assert_eq!(inherited, "S3", "继承的旧 S1 接着簇 id 的计数器继续编号");
        assert_eq!(table[&inherited].name, "张三");
        assert_eq!(table["S2"].name, "", "新簇自己的 meta 落在它的新 id 下");
        assert!(!table.contains_key("S1"), "新表不应再出现与旧表撞号的 S1 键");
        assert_eq!((stats.seed_matched, stats.inherited), (0, 1));
    }

    /// 同人合流:旧说话人关联的 person 与种子命中簇相同 → 复用簇的**新** id(避让旧表
    /// 键域后的编号),不开第二行。
    #[test]
    fn inherited_speaker_unified_with_seed_cluster_by_person() {
        let segs = vec![rec("mic", 0, 3000), rec("mic", 10_000, 1000)];
        // 段0 种子命中簇 S1(person P7);段1 无簇(短段)
        let clusters = vec![Some("S1".to_string()), None];
        let infos = vec![info("S1", Some("P7"), Some("张三"))];
        let snaps = vec![snap("S1")];
        let old_segs = vec![old_seg(1, "mic", 10_000, 11_000, Some("S9"))];
        let old_speakers = BTreeMap::from([("S9".to_string(), named_meta("张三", Some("P7")))]);
        let (speakers, table, stats) =
            finalize_speakers(&segs, &clusters, &infos, &snaps, &old_segs, &old_speakers);
        // 旧表 max 数字部分是 9 → 新簇 S1 落盘为 S10。
        assert_eq!(speakers[1].as_deref(), Some("S10"), "同 person 并入种子簇(避让后的新 id)");
        assert_eq!(table.len(), 1);
        assert_eq!(table["S10"].person_id.as_deref(), Some("P7"));
        assert_eq!((stats.seed_matched, stats.inherited), (1, 1));
    }

    /// Fix 2 新增:旧表 {S1,S3},新簇 {S1,S2}(均未撞种子/继承候选) → 全部落盘 id
    /// 都必须严格大于旧表最大数字部分(3),且新表键与旧表键 {S1,S3} 完全不相交
    /// ——commit.rs §3 的并集预写因此天然无冲突键(见 commit.rs 对应测试)。
    #[test]
    fn all_landed_ids_strictly_exceed_old_table_max_without_collision() {
        let segs = vec![rec("mic", 0, 2000), rec("mic", 5_000, 2000)];
        let clusters = vec![Some("S1".to_string()), Some("S2".to_string())];
        let infos = vec![info("S1", None, None), info("S2", None, None)];
        let snaps = vec![snap("S1"), snap("S2")];
        let old_segs: Vec<SegmentRecord> = vec![]; // 无重叠候选,两段都走簇 id 兜底
        let old_speakers = BTreeMap::from([
            ("S1".to_string(), named_meta("甲", None)),
            ("S3".to_string(), named_meta("乙", None)),
        ]);
        let (speakers, table, _stats) =
            finalize_speakers(&segs, &clusters, &infos, &snaps, &old_segs, &old_speakers);
        assert_eq!(speakers[0].as_deref(), Some("S4"));
        assert_eq!(speakers[1].as_deref(), Some("S5"));
        for id in table.keys() {
            let n = id.strip_prefix('S').and_then(|n| n.parse::<u64>().ok()).unwrap();
            assert!(n > 3, "落盘 id {id} 必须严格大于旧表最大数字部分 3");
        }
        let new_keys: std::collections::BTreeSet<&String> = table.keys().collect();
        let old_keys: std::collections::BTreeSet<&String> = old_speakers.keys().collect();
        assert!(new_keys.is_disjoint(&old_keys), "新表键与旧表键 {{S1,S3}} 必须不相交");
    }

    /// 无人工价值的旧归属(name 空且无 person)不继承——继承是保人工劳动,不是保编号。
    #[test]
    fn valueless_old_speaker_not_inherited() {
        let segs = vec![rec("mic", 0, 2000)];
        let clusters = vec![None];
        let old_segs = vec![old_seg(1, "mic", 0, 2000, Some("S3"))];
        let old_speakers = BTreeMap::from([("S3".to_string(), named_meta("", None))]);
        let (speakers, table, stats) =
            finalize_speakers(&segs, &clusters, &[], &[], &old_segs, &old_speakers);
        assert_eq!(speakers[0], None);
        assert!(table.is_empty());
        assert_eq!(stats.inherited, 0);
    }

    /// 重叠不足 30% 不继承;同 source 候选优先于跨 source。
    #[test]
    fn overlap_threshold_and_same_source_priority() {
        let segs = vec![rec("mic", 0, 4000)];
        let clusters = vec![None];
        let old_segs = vec![
            old_seg(1, "mic", 3800, 8000, Some("S1")),    // 与新段仅重叠 200ms/4000ms=5%
            old_seg(2, "system", 0, 4000, Some("S2")),    // 100% 重叠但跨 source
            old_seg(3, "mic", 500, 4000, Some("S3")),     // 同 source 87.5% 重叠
        ];
        let old_speakers = BTreeMap::from([
            ("S1".to_string(), named_meta("甲", None)),
            ("S2".to_string(), named_meta("乙", None)),
            ("S3".to_string(), named_meta("丙", None)),
        ]);
        let (speakers, table, _) =
            finalize_speakers(&segs, &clusters, &[], &[], &old_segs, &old_speakers);
        let id = speakers[0].clone().expect("87.5% 重叠应继承");
        assert_eq!(table[&id].name, "丙", "同 source 最大重叠者胜出");
    }
}
