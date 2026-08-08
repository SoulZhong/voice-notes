//! P2a 说话人身份推断(identify,只读期):精修定稿后,把带说话人标签的段落、
//! 声纹库候选与簇声学统计打包给 LLM,推断「R 簇 → 真实身份」;程序侧五道裁决
//! 后只产建议(identify.json + 收件箱建议卡),零自动写入。
//! spec: docs/superpowers/specs/2026-08-08-speaker-context-inference-design.md(rev2)
//! plan: docs/superpowers/plans/2026-08-08-speaker-context-p2a-identify.md(rev2)

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::recluster::ClusterStat;
use crate::store::{VoiceprintStore, Voiceprints};
use crate::store::RefinedDoc;

pub const MAX_CANDIDATES: usize = 30;
/// 每簇声学近邻 Top-K 后取并集——全局 Top-K 会被单一大簇占满,后面的簇颗粒无收。
pub const ACOUSTIC_TOP_K_PER_CLUSTER: usize = 5;
pub const RECENT_TOP_K: usize = 10;
pub const SAMPLE_CHAR_BUDGET: usize = 6000;
/// 采样阶段单段截断长度(超预算段截前 N 字符,按截断后长度计入预算)。
pub const SAMPLE_TRUNCATE_CHARS: usize = 200;
/// 人名 contains 召回的最短字符数:单字名误命中海量(「王」「李」满地都是)。
pub const NAME_HIT_MIN_CHARS: usize = 2;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Candidate {
    pub person_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClusterBrief {
    pub speaker: String,
    pub fingerprint: String,
    pub total_ms: u64,
    pub dominant_source: String,
    /// 次信道时长占比 >20% 视为混合簇:声学门不适用(裁决层用)。
    pub mixed: bool,
    /// mic 主导:大概率是「我」(prompt 先验)。
    pub is_mic: bool,
    /// 已采纳的关联(种子命中或既有 person_id):有它且无矛盾证据的簇不出建议。
    pub linked: Option<(String, String)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SampledParagraph {
    pub paragraph_index: usize,
    pub speaker: String,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct IdentifyContext {
    pub note_id: String,
    pub revision: u64,
    pub source_hash: String,
    pub clusters: Vec<ClusterBrief>,
    pub candidates: Vec<Candidate>,
    pub sampled: Vec<SampledParagraph>,
}

/// 簇指纹:与 P1 回灌账本同口径(feedback::seq_fingerprint)。
pub fn cluster_fingerprint(seqs: &BTreeSet<u64>) -> String {
    crate::feedback::seq_fingerprint(seqs)
}

/// 从最终稿重建每个 R 簇的成员 seq 集(source_seqs 并集)。指纹一律以此为准:
/// recluster 的 core_seqs 不含无嵌入传播段,与最终稿不一致,不能当身份指纹。
pub fn cluster_members_from_doc(doc: &RefinedDoc) -> BTreeMap<String, BTreeSet<u64>> {
    let mut out: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
    for p in &doc.paragraphs {
        out.entry(p.speaker.clone())
            .or_default()
            .extend(p.source_seqs.iter().copied());
    }
    out
}

fn dot(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let v: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    v.is_finite().then_some(v)
}

/// 候选三路召回(内部带参 K,常量是默认值;测试可收紧)。只收有名人物——
/// 无名 P<n> 对"起真名"毫无信息量。acoustic_enabled=false(模型门禁不一致)
/// 时声学路整体关闭,绝不在跨模型向量空间做相似度。
fn recall_candidates(
    stats: &[ClusterStat],
    vp: &Voiceprints,
    acoustic_enabled: bool,
    k_acoustic: usize,
    k_recent: usize,
    cap: usize,
) -> Vec<Candidate> {
    let mut picked: Vec<Candidate> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut push = |id: &str, name: &str, picked: &mut Vec<Candidate>, seen: &mut BTreeSet<String>| {
        if name.trim().is_empty() {
            return;
        }
        if seen.insert(id.to_string()) {
            picked.push(Candidate { person_id: id.to_string(), name: name.to_string() });
        }
    };

    // ① 声学近邻:每簇各信道质心 × 每人同信道主质心,每簇 top-k 并集。
    if acoustic_enabled {
        for stat in stats {
            let mut sims: Vec<(f32, &str, &str)> = Vec::new();
            for (pid, person) in &vp.people {
                if VoiceprintStore::resolve(vp, pid) != Some(pid.as_str()) || person.name.trim().is_empty() {
                    continue;
                }
                let best = stat
                    .centroids
                    .iter()
                    .filter_map(|(src, c)| person.centroids.get(src).and_then(|pc| dot(c, &pc.vec)))
                    .fold(None::<f32>, |acc, s| Some(acc.map_or(s, |a| a.max(s))));
                if let Some(sim) = best {
                    sims.push((sim, pid, &person.name));
                }
            }
            sims.sort_by(|a, b| b.0.total_cmp(&a.0));
            for (_, pid, name) in sims.into_iter().take(k_acoustic) {
                push(pid, name, &mut picked, &mut seen);
            }
        }
    }

    // ② last_seen 最近的有名人物。
    let mut recent: Vec<(&String, &crate::store::Person)> = vp
        .people
        .iter()
        .filter(|(pid, p)| {
            VoiceprintStore::resolve(vp, pid) == Some(pid.as_str()) && !p.name.trim().is_empty()
        })
        .collect();
    recent.sort_by(|a, b| b.1.last_seen.cmp(&a.1.last_seen));
    for (pid, p) in recent.into_iter().take(k_recent) {
        push(pid, &p.name, &mut picked, &mut seen);
    }

    // ③ 已采纳的种子命中人(adopted):当场证据,最强先验。
    for stat in stats {
        if let Some((pid, name, _, true)) = &stat.seed {
            push(pid, name, &mut picked, &mut seen);
        }
    }

    picked.truncate(cap);
    picked
}

/// 采样:按优先级填充段落,BTreeSet 去重,超预算段截断且按截断后长度计入预算。
/// 优先级:每簇开场 2 段 → 含候选人名(≥NAME_HIT_MIN_CHARS 字符)的段 →
/// 自报句式段 → 簇切换边界前后段。不依赖知识图谱(它在 identify 之后才生成)。
fn sample_paragraphs(
    doc: &RefinedDoc,
    candidates: &[Candidate],
    budget: usize,
) -> Vec<SampledParagraph> {
    let ps = &doc.paragraphs;
    let mut ordered: Vec<usize> = Vec::new();
    let mut chosen: BTreeSet<usize> = BTreeSet::new();
    let mut add = |i: usize, ordered: &mut Vec<usize>, chosen: &mut BTreeSet<usize>| {
        if i < ps.len() && chosen.insert(i) {
            ordered.push(i);
        }
    };

    // a) 每簇开场 2 段。
    let mut per_speaker: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, p) in ps.iter().enumerate() {
        let c = per_speaker.entry(p.speaker.as_str()).or_default();
        if *c < 2 {
            *c += 1;
            add(i, &mut ordered, &mut chosen);
        }
    }
    // b) 含候选人名的段。
    let names: Vec<&str> = candidates
        .iter()
        .map(|c| c.name.as_str())
        .filter(|n| n.chars().count() >= NAME_HIT_MIN_CHARS)
        .collect();
    for (i, p) in ps.iter().enumerate() {
        if names.iter().any(|n| p.text.contains(n)) {
            add(i, &mut ordered, &mut chosen);
        }
    }
    // c) 自报句式段。
    const INTRO_PATTERNS: [&str; 4] = ["我是", "我叫", "这边是", "我这边是"];
    for (i, p) in ps.iter().enumerate() {
        if INTRO_PATTERNS.iter().any(|pat| p.text.contains(pat)) {
            add(i, &mut ordered, &mut chosen);
        }
    }
    // d) 簇切换边界前后段。
    for i in 1..ps.len() {
        if ps[i].speaker != ps[i - 1].speaker {
            add(i - 1, &mut ordered, &mut chosen);
            add(i, &mut ordered, &mut chosen);
        }
    }

    // 预算填充:按优先级序;超长段截前 SAMPLE_TRUNCATE_CHARS 并按截断后长度计。
    let mut out: Vec<SampledParagraph> = Vec::new();
    let mut used = 0usize;
    for i in ordered {
        let full: Vec<char> = ps[i].text.chars().collect();
        let text: String = if full.len() > SAMPLE_TRUNCATE_CHARS {
            full[..SAMPLE_TRUNCATE_CHARS].iter().collect()
        } else {
            ps[i].text.clone()
        };
        let n = text.chars().count();
        if used + n > budget {
            break;
        }
        used += n;
        out.push(SampledParagraph { paragraph_index: i, speaker: ps[i].speaker.clone(), text });
    }
    out.sort_by_key(|s| s.paragraph_index); // 时间序输出,LLM 读对话流更自然
    out
}

pub fn build_context(
    note_id: &str,
    doc: &RefinedDoc,
    stats: &[ClusterStat],
    vp: &Voiceprints,
    acoustic_enabled: bool,
) -> IdentifyContext {
    let members = cluster_members_from_doc(doc);
    let candidates = recall_candidates(
        stats,
        vp,
        acoustic_enabled,
        ACOUSTIC_TOP_K_PER_CLUSTER,
        RECENT_TOP_K,
        MAX_CANDIDATES,
    );

    // 段落既有关联(种子命中会写进 paragraphs 的 person_id/name)。
    let mut linked_by_speaker: BTreeMap<&str, (String, String)> = BTreeMap::new();
    for p in &doc.paragraphs {
        if let Some(pid) = &p.person_id {
            linked_by_speaker
                .entry(p.speaker.as_str())
                .or_insert_with(|| (pid.clone(), p.name.clone().unwrap_or_default()));
        }
    }

    let clusters = members
        .iter()
        .map(|(speaker, seqs)| {
            let stat = stats.iter().find(|s| &s.speaker == speaker);
            let (total_ms, dominant_source, mixed) = match stat {
                Some(s) => {
                    let dominant = s
                        .source_ms
                        .iter()
                        .max_by_key(|(_, ms)| **ms)
                        .map(|(src, _)| src.clone())
                        .unwrap_or_default();
                    let dom_ms = s.source_ms.get(&dominant).copied().unwrap_or(0);
                    let other_ms = s.total_ms.saturating_sub(dom_ms);
                    (s.total_ms, dominant, s.total_ms > 0 && other_ms * 5 > s.total_ms)
                }
                None => (0, String::new(), false),
            };
            let linked = linked_by_speaker
                .get(speaker.as_str())
                .cloned()
                .or_else(|| {
                    stat.and_then(|s| {
                        s.seed
                            .as_ref()
                            .filter(|(_, _, _, adopted)| *adopted)
                            .map(|(pid, name, _, _)| (pid.clone(), name.clone()))
                    })
                });
            ClusterBrief {
                speaker: speaker.clone(),
                fingerprint: cluster_fingerprint(seqs),
                total_ms,
                is_mic: dominant_source == "mic",
                dominant_source,
                mixed,
                linked,
            }
        })
        .collect();

    let sampled = sample_paragraphs(doc, &candidates, SAMPLE_CHAR_BUDGET);
    IdentifyContext {
        note_id: note_id.to_string(),
        revision: doc.revision,
        source_hash: crate::store::source_hash(&doc.paragraphs),
        clusters,
        candidates,
        sampled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::refined::{RefineStages, RefinedDoc, RefinedParagraph};

    fn para(speaker: &str, seqs: &[u64], text: &str) -> RefinedParagraph {
        RefinedParagraph {
            speaker: speaker.into(),
            name: None,
            person_id: None,
            start_ms: 0,
            end_ms: 1000,
            text: text.into(),
            source_seqs: seqs.to_vec(),
            mentions: vec![],
        }
    }

    fn doc_with(paragraphs: Vec<RefinedParagraph>) -> RefinedDoc {
        RefinedDoc {
            schema_version: crate::store::refined::REFINED_SCHEMA_VERSION,
            generated_at: "t".into(),
            llm_model: None,
            stages: RefineStages {
                filter: "done".into(),
                recluster: "done".into(),
                llm: "done".into(),
                entities: "off".into(),
                relations: "off".into(),
            },
            discarded_seqs: vec![],
            entities: vec![],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            revision: 3,
            stale: false,
            paragraphs,
        }
    }

    fn person(name: &str, source: &str, vec: Vec<f32>, last_seen: &str) -> crate::store::Person {
        use crate::store::{Person, PersonCentroid};
        let mut centroids = BTreeMap::new();
        centroids.insert(
            source.to_string(),
            PersonCentroid { vec, count: 5, seen: String::new() },
        );
        Person {
            name: name.into(),
            centroids,
            session_centroids: BTreeMap::new(),
            total_ms: 10_000,
            last_seen: last_seen.into(),
        }
    }

    fn stat(speaker: &str, source: &str, centroid: Vec<f32>, ms: u64) -> ClusterStat {
        let mut centroids = BTreeMap::new();
        centroids.insert(source.to_string(), centroid);
        let mut source_ms = BTreeMap::new();
        source_ms.insert(source.to_string(), ms);
        ClusterStat {
            speaker: speaker.into(),
            centroids,
            total_ms: ms,
            source_ms,
            core_seqs: vec![],
            seed: None,
        }
    }

    fn vp_with(people: Vec<(&str, crate::store::Person)>) -> Voiceprints {
        let mut vp = Voiceprints::default();
        for (id, p) in people {
            vp.people.insert(id.to_string(), p);
        }
        vp
    }

    #[test]
    fn fingerprint_matches_feedback_and_members_come_from_doc() {
        let doc = doc_with(vec![
            para("R1", &[0, 1], "a"),
            para("R1", &[2], "b"),
            para("R2", &[5], "c"),
        ]);
        let members = cluster_members_from_doc(&doc);
        assert_eq!(members["R1"], [0u64, 1, 2].into_iter().collect());
        assert_eq!(members["R2"], [5u64].into_iter().collect());
        assert_eq!(
            cluster_fingerprint(&members["R1"]),
            crate::feedback::seq_fingerprint(&members["R1"])
        );
    }

    #[test]
    fn candidates_per_cluster_topk_and_recent_union_dedup() {
        // A: 与簇质心同向(声学最近);B: last_seen 最近;C: 无名(必须被排除);
        // D: 又远又旧(K 收紧后被挤出)。
        let vp = vp_with(vec![
            ("P1", person("阿声", "mic", vec![1.0, 0.0, 0.0, 0.0], "2026-01-01")),
            ("P2", person("阿近", "mic", vec![0.0, 1.0, 0.0, 0.0], "2026-08-01")),
            ("P3", person("", "mic", vec![1.0, 0.0, 0.0, 0.0], "2026-08-02")),
            ("P4", person("阿远", "mic", vec![0.0, 0.0, 1.0, 0.0], "2020-01-01")),
        ]);
        let stats = vec![stat("R1", "mic", vec![1.0, 0.0, 0.0, 0.0], 60_000)];
        let got = recall_candidates(&stats, &vp, true, 1, 1, 10);
        let ids: Vec<&str> = got.iter().map(|c| c.person_id.as_str()).collect();
        assert!(ids.contains(&"P1"), "声学近邻必在: {ids:?}");
        assert!(ids.contains(&"P2"), "时近必在: {ids:?}");
        assert!(!ids.contains(&"P3"), "无名人物不入候选");
        assert!(!ids.contains(&"P4"), "K=1+1 时又远又旧被挤出");
        // 声学路关闭:只剩时近路。
        let got2 = recall_candidates(&stats, &vp, false, 1, 1, 10);
        assert_eq!(got2.len(), 1);
        assert_eq!(got2[0].person_id, "P2");
    }

    #[test]
    fn sampling_dedups_and_respects_budget() {
        // 段 0:开场 + 含人名 + 自报句式,三重命中只入选一次。
        let long = "很".repeat(500);
        let doc = doc_with(vec![
            para("R1", &[0], "我是张伟,今天说三点"),
            para("R1", &[1], &long),
            para("R2", &[2], "好的"),
        ]);
        let cands = vec![Candidate { person_id: "P1".into(), name: "张伟".into() }];
        let out = sample_paragraphs(&doc, &cands, 260);
        let idx: Vec<usize> = out.iter().map(|s| s.paragraph_index).collect();
        assert_eq!(idx.iter().filter(|&&i| i == 0).count(), 1, "去重");
        // 预算 260:段0(11 字)+ 长段截断 200 字 = 211 入选;段2 是 R2 开场(2 字)也可入;
        let total: usize = out.iter().map(|s| s.text.chars().count()).sum();
        assert!(total <= 260, "总量不破预算: {total}");
        assert!(
            out.iter().any(|s| s.paragraph_index == 1 && s.text.chars().count() == 200),
            "超长段截断至 200"
        );
    }

    #[test]
    fn build_context_marks_mixed_and_linked() {
        let mut p0 = para("R1", &[0], "开场");
        p0.person_id = Some("P7".into());
        p0.name = Some("老熟人".into());
        let doc = doc_with(vec![p0, para("R2", &[1], "另一位")]);
        // R1 混合簇:mic 6s + system 4s(次信道 40% > 20%)。
        let mut s1 = stat("R1", "mic", vec![1.0, 0.0], 6_000);
        s1.source_ms.insert("system".into(), 4_000);
        s1.total_ms = 10_000;
        let stats = vec![s1, stat("R2", "system", vec![0.0, 1.0], 8_000)];
        let ctx = build_context("n1", &doc, &stats, &Voiceprints::default(), true);
        let r1 = ctx.clusters.iter().find(|c| c.speaker == "R1").unwrap();
        assert!(r1.mixed);
        assert!(r1.is_mic);
        assert_eq!(r1.linked.as_ref().unwrap().0, "P7");
        let r2 = ctx.clusters.iter().find(|c| c.speaker == "R2").unwrap();
        assert!(!r2.mixed);
        assert!(r2.linked.is_none());
        assert_eq!(ctx.revision, 3);
        assert!(!ctx.source_hash.is_empty());
    }
}
