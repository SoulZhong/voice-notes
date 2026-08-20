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

/// 当场会议的日历上下文(P3):闭集先验,不是硬约束(临时加入/代参会存在)。
#[derive(Debug, Serialize)]
pub struct CalendarContext {
    pub title: String,
    /// 参会人名(is_me 的标成「名字(我)」);上限 CALENDAR_ATTENDEES_IN_PROMPT,
    /// 超出末尾追加「(其余 N 人略)」占位——大型活动不许把几百人塞进 prompt。
    pub attendees: Vec<String>,
}

pub const CALENDAR_ATTENDEES_IN_PROMPT: usize = 30;

#[derive(Debug, Serialize)]
pub struct IdentifyContext {
    pub note_id: String,
    pub revision: u64,
    pub source_hash: String,
    pub clusters: Vec<ClusterBrief>,
    pub candidates: Vec<Candidate>,
    pub sampled: Vec<SampledParagraph>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calendar: Option<CalendarContext>,
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

/// 候选第④路(P3):日历参会人。email 精确命中 Person.emails 的人最优先,
/// 其次参会人名精确等于库中人名的人。闭集先验,排在其余各路之前。
fn calendar_candidates(cal: &crate::store::CalendarSnapshot, vp: &Voiceprints) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    // email 精确命中。
    for att in &cal.attendees {
        if att.email.is_empty() {
            continue;
        }
        for (pid, person) in &vp.people {
            if VoiceprintStore::resolve(vp, pid) == Some(pid.as_str())
                && !person.voiceprint_quarantined
                && !person.name.trim().is_empty()
                && person.emails.iter().any(|e| e == &att.email)
                && seen.insert(pid.clone())
            {
                out.push(Candidate { person_id: pid.clone(), name: person.name.clone() });
            }
        }
    }
    // 名字精确命中。
    for att in &cal.attendees {
        let name = att.name.trim();
        if name.is_empty() {
            continue;
        }
        for (pid, person) in &vp.people {
            if VoiceprintStore::resolve(vp, pid) == Some(pid.as_str())
                && !person.voiceprint_quarantined
                && person.name == name
                && seen.insert(pid.clone())
            {
                out.push(Candidate { person_id: pid.clone(), name: person.name.clone() });
            }
        }
    }
    out
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
    let push = |id: &str, name: &str, picked: &mut Vec<Candidate>, seen: &mut BTreeSet<String>| {
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
                if VoiceprintStore::resolve(vp, pid) != Some(pid.as_str())
                    || person.voiceprint_quarantined
                    || person.name.trim().is_empty()
                {
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
            VoiceprintStore::resolve(vp, pid) == Some(pid.as_str())
                && !p.voiceprint_quarantined
                && !p.name.trim().is_empty()
        })
        .collect();
    recent.sort_by(|a, b| b.1.last_seen.cmp(&a.1.last_seen));
    for (pid, p) in recent.into_iter().take(k_recent) {
        push(pid, &p.name, &mut picked, &mut seen);
    }

    // ③ 已采纳的种子命中人(adopted):当场证据,最强先验。
    // 隔离人物再滤一道:新聚类产不出隔离种子(seed_clusters 已过滤),这里兜的是
    // 打标**之前**算好的旧 ClusterStat 被复用的路径。
    for stat in stats {
        if let Some((pid, name, _, true)) = &stat.seed {
            if vp.people.get(pid).is_some_and(|p| p.voiceprint_quarantined) {
                continue;
            }
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
    let add = |i: usize, ordered: &mut Vec<usize>, chosen: &mut BTreeSet<usize>| {
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
    calendar: Option<&crate::store::CalendarSnapshot>,
) -> IdentifyContext {
    let members = cluster_members_from_doc(doc);
    // 第④路(日历)排最前,其余三路去重后补齐;总量仍受 MAX_CANDIDATES。
    let mut candidates = calendar.map(|c| calendar_candidates(c, vp)).unwrap_or_default();
    for c in recall_candidates(
        stats,
        vp,
        acoustic_enabled,
        ACOUSTIC_TOP_K_PER_CLUSTER,
        RECENT_TOP_K,
        MAX_CANDIDATES,
    ) {
        if !candidates.iter().any(|x| x.person_id == c.person_id) {
            candidates.push(c);
        }
    }
    candidates.truncate(MAX_CANDIDATES);

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
    let calendar_ctx = calendar.map(|c| {
        let mut names: Vec<String> = c
            .attendees
            .iter()
            .take(CALENDAR_ATTENDEES_IN_PROMPT)
            .map(|a| if a.is_me { format!("{}(我)", a.name) } else { a.name.clone() })
            .collect();
        let rest = c.attendees.len().saturating_sub(CALENDAR_ATTENDEES_IN_PROMPT);
        if rest > 0 {
            names.push(format!("(其余 {rest} 人略)"));
        }
        CalendarContext { title: c.title.clone(), attendees: names }
    });
    IdentifyContext {
        note_id: note_id.to_string(),
        revision: doc.revision,
        source_hash: crate::store::source_hash(&doc.paragraphs),
        clusters,
        candidates,
        sampled,
        calendar: calendar_ctx,
    }
}

/// identify 执行体:provider 无关(仿 RelationExecutor 模式)。P2a 只有 HTTP
/// 实现;Agent 实现另立计划(不解析 CLI stdout,需文件交换设计)。
pub trait IdentifyExecutor: Send + Sync {
    fn provider(&self) -> &str;
    fn model(&self) -> &str;
    fn infer(
        &self,
        ctx: &IdentifyContext,
        log: Option<&crate::ailog::Ctx>,
    ) -> anyhow::Result<(Vec<RawAssignment>, usize)>;
}

// identify 的 system prompt 见 super::prompts::IDENTIFY_SYSTEM(全部提示词集中在那里)。

/// 簇的主信道与混合判定(与 ClusterBrief 同口径,裁决层复用)。
fn dominant_and_mixed(stat: &ClusterStat) -> (String, bool) {
    let dominant = stat
        .source_ms
        .iter()
        .max_by_key(|(_, ms)| **ms)
        .map(|(src, _)| src.clone())
        .unwrap_or_default();
    let dom_ms = stat.source_ms.get(&dominant).copied().unwrap_or(0);
    let other_ms = stat.total_ms.saturating_sub(dom_ms);
    (dominant, stat.total_ms > 0 && other_ms * 5 > stat.total_ms)
}

/// AS-Norm 对称 z(P2b,与 registry seed_z 同口径):
/// - 查询侧 cohort:该簇质心 × 库内**其它**有名人物(经 redirects 归一)同信道主质心;
/// - 目标侧 cohort:目标人同信道主质心 × 本场**其它**簇的同信道质心;
/// 两侧任一 cohort 不足 SNORM_MIN_COHORT(3,与 registry 同值)返回 None(小库/
/// 少簇回退裸余弦口径,不惩罚小库用户);z = 两侧 z 的均值。只服务自动应用资格,
/// 不改 tier。向量维度不一致/非有限值一律跳过。
pub fn asnorm_z(
    cluster_speaker: &str,
    target_person: &str,
    stats: &[ClusterStat],
    vp: &Voiceprints,
) -> Option<f32> {
    let stat = stats.iter().find(|s| s.speaker == cluster_speaker)?;
    let (dominant, mixed) = dominant_and_mixed(stat);
    if mixed {
        return None;
    }
    let cluster_c = stat.centroids.get(&dominant)?;
    let person_c = &vp.people.get(target_person)?.centroids.get(&dominant)?.vec;
    let raw = dot(cluster_c, person_c)?;

    let z_of = |raw: f32, cohort: &[f32]| -> Option<f32> {
        if cohort.len() < crate::diar::registry::SNORM_MIN_COHORT {
            return None;
        }
        // 与 registry::cohort_stats 同口径:样本方差 /(n-1) + 标准差下限 1e-3——
        // 总体方差 + 1e-6 下限在三人 cohort 上会系统性放大 z(近零方差时放大三个
        // 数量级),把不该过门的身份放进自动应用。
        let n = cohort.len() as f32;
        let mean = cohort.iter().sum::<f32>() / n;
        let var = cohort.iter().map(|c| (c - mean) * (c - mean)).sum::<f32>() / (n - 1.0);
        let std = var.sqrt().max(1e-3);
        Some((raw - mean) / std)
    };

    // 查询侧:簇 × 其它有名人物。
    let query_cohort: Vec<f32> = vp
        .people
        .iter()
        .filter(|(pid, p)| {
            VoiceprintStore::resolve(vp, pid) == Some(pid.as_str())
                && pid.as_str() != target_person
                && !p.name.trim().is_empty()
        })
        .filter_map(|(_, p)| p.centroids.get(&dominant).and_then(|c| dot(cluster_c, &c.vec)))
        .collect();
    // 目标侧:目标人 × 其它簇(同信道质心)。
    let person_cohort: Vec<f32> = stats
        .iter()
        .filter(|s| s.speaker != cluster_speaker)
        .filter_map(|s| s.centroids.get(&dominant).and_then(|c| dot(person_c, c)))
        .collect();
    let zq = z_of(raw, &query_cohort)?;
    let zp = z_of(raw, &person_cohort)?;
    Some((zq + zp) / 2.0)
}

// ---------- 输出解析与裁决 ----------

use serde::Deserialize;

pub const EVIDENCE_TYPES: [&str; 4] =
    ["self_intro", "addressed_reply", "third_person_exclusion", "role_topic"];
pub const NEW_NAME_MAX_CHARS: usize = 32;
/// 声学正向确认阈值:与种子认人同口径(registry::SEED_ASSIGN_THRESHOLD)。
pub use crate::diar::registry::SEED_ASSIGN_THRESHOLD as ACOUSTIC_CONFIRM_THRESHOLD;

#[derive(Debug, Clone, Deserialize)]
pub struct RawAssignment {
    pub cluster: String,
    #[serde(default)]
    pub person_id: Option<String>,
    #[serde(default)]
    pub new_name: Option<String>,
    #[serde(default)]
    pub confidence: String,
    #[serde(default)]
    pub evidence: Vec<RawIdentifyEvidence>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RawIdentifyEvidence {
    pub paragraph_index: usize,
    pub start: usize,
    pub end: usize,
    pub quote: String,
    pub r#type: String,
}

/// 宽松解析:整体先解 Value,assignments 逐条 from_value,坏条目跳过并计数——
/// 单条类型错误不拖垮整批(与实体解析同哲学)。
pub fn parse_raw_identify(content: &str) -> anyhow::Result<(Vec<RawAssignment>, usize)> {
    let v: serde_json::Value = serde_json::from_str(content)?;
    let Some(arr) = v["assignments"].as_array() else {
        anyhow::bail!("响应缺 assignments 数组");
    };
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for item in arr {
        match serde_json::from_value::<RawAssignment>(item.clone()) {
            Ok(a) => out.push(a),
            Err(_) => skipped += 1,
        }
    }
    Ok((out, skipped))
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    High,
    Medium,
    Low,
}

#[derive(Debug)]
pub struct Verdict {
    pub tier: Tier,
    pub acoustic: Option<(String, f32)>,
    /// Some = 整条丢弃(不落建议);&'static str 便于日志聚类。
    pub reject_reason: Option<&'static str>,
    /// 目标键:resolve 后 person_id,或 "name:<new_name>"(拒绝名单/冲突检测共用)。
    pub target_key: String,
    /// 经一致性降级后的有效证据(落盘用)。
    pub evidence: Vec<RawIdentifyEvidence>,
}

fn reject(reason: &'static str) -> Verdict {
    Verdict { tier: Tier::Low, acoustic: None, reject_reason: Some(reason), target_key: String::new(), evidence: vec![] }
}

/// 名字 sanitize:trim + 去控制字符;空/超长返回 None。
fn sanitize_new_name(name: &str) -> Option<String> {
    let cleaned: String = name.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() || cleaned.chars().count() > NEW_NAME_MAX_CHARS {
        None
    } else {
        Some(cleaned)
    }
}

/// Unicode scalar 半开区间取子串;越界返回 None。
fn char_slice(text: &str, start: usize, end: usize) -> Option<String> {
    if start >= end {
        return None;
    }
    let chars: Vec<char> = text.chars().collect();
    if end > chars.len() {
        return None;
    }
    Some(chars[start..end].iter().collect())
}

/// 五道裁决(spec + Codex 修订):①结构 ②区间逐字 ③证据-身份一致性
/// ④冲突 ⑤声学门,再分档。绝不只信 LLM 自报 confidence。
pub fn adjudicate(
    raw: &RawAssignment,
    doc: &RefinedDoc,
    members: &BTreeMap<String, BTreeSet<u64>>,
    stats: &[ClusterStat],
    vp: &Voiceprints,
    acoustic_enabled: bool,
    taken: &BTreeMap<String, String>,
) -> Verdict {
    // ① 结构。
    if !members.contains_key(&raw.cluster) {
        return reject("unknown-cluster");
    }
    let (target_key, target_name) = match (&raw.person_id, &raw.new_name) {
        (Some(pid), None) => {
            let Some(resolved) = VoiceprintStore::resolve(vp, pid) else {
                return reject("dangling-person");
            };
            let name = vp.people.get(resolved).map(|p| p.name.clone()).unwrap_or_default();
            (resolved.to_string(), name)
        }
        (None, Some(name)) => match sanitize_new_name(name) {
            Some(n) => (format!("name:{n}"), n),
            None => return reject("bad-new-name"),
        },
        _ => return reject("person-xor-new-name"),
    };

    // ② 区间逐字校验(任一 evidence 不过 → 整条丢弃:证据造假一票否决)。
    // 未知 type 只丢该条 evidence。
    let mut evidence: Vec<RawIdentifyEvidence> = Vec::new();
    for ev in &raw.evidence {
        if !EVIDENCE_TYPES.contains(&ev.r#type.as_str()) {
            continue;
        }
        let Some(p) = doc.paragraphs.get(ev.paragraph_index) else {
            return reject("evidence-mismatch");
        };
        match char_slice(&p.text, ev.start, ev.end) {
            Some(s) if s == ev.quote => evidence.push(ev.clone()),
            _ => return reject("evidence-mismatch"),
        }
    }

    // ③ 证据-身份一致性:self_intro 的 quote 必须含目标名(防"我是李雷"指认张伟);
    // addressed_reply 必须构成「称呼(他簇,含目标名)+应答(本簇)」配对,
    // 不满足的降级为 role_topic 弱证据。
    let has_reply_in_cluster = evidence.iter().any(|e| {
        e.r#type == "addressed_reply"
            && doc.paragraphs.get(e.paragraph_index).map(|p| p.speaker == raw.cluster).unwrap_or(false)
    });
    let has_call_elsewhere = evidence.iter().any(|e| {
        e.r#type == "addressed_reply"
            && doc
                .paragraphs
                .get(e.paragraph_index)
                .map(|p| p.speaker != raw.cluster)
                .unwrap_or(false)
            && (target_name.is_empty() || e.quote.contains(target_name.as_str()))
    });
    let reply_pair_ok = has_reply_in_cluster && has_call_elsewhere;
    for e in &mut evidence {
        match e.r#type.as_str() {
            "self_intro" if target_name.is_empty() || !e.quote.contains(target_name.as_str()) => {
                e.r#type = "role_topic".into();
            }
            "addressed_reply" if !reply_pair_ok => {
                e.r#type = "role_topic".into();
            }
            _ => {}
        }
    }
    if evidence.is_empty() {
        return reject("no-valid-evidence");
    }

    // ④ 冲突:同簇已有裁决通过的条目、或同目标已被别的簇认领 → 上限 Medium
    //(不丢弃:留给人裁,建议卡本来就要人拍板)。
    let conflicted =
        taken.contains_key(&raw.cluster) || taken.values().any(|t| t == &target_key);

    // ⑤ 声学门:仅库内已有人 + 门禁通过 + 非混合簇 + 目标人有同主信道质心。
    let acoustic = if acoustic_enabled && raw.person_id.is_some() {
        stats.iter().find(|s| s.speaker == raw.cluster).and_then(|stat| {
            let (dominant, mixed) = dominant_and_mixed(stat);
            if mixed {
                return None;
            }
            let cluster_c = stat.centroids.get(&dominant)?;
            let person = vp.people.get(target_key.as_str())?;
            let person_c = person.centroids.get(&dominant)?;
            let cos = dot(cluster_c, &person_c.vec)?;
            (cos >= ACOUSTIC_CONFIRM_THRESHOLD).then_some((dominant, cos))
        })
    } else {
        None
    };

    // 分档:High 需 self_intro + 声学正向确认 + 无冲突;self_intro/addressed_reply
    // 任一有效 → Medium;只剩弱证据 → Low。new_name 无质心可比,天花板 Medium。
    let has_intro = evidence.iter().any(|e| e.r#type == "self_intro");
    let has_strong = has_intro || evidence.iter().any(|e| e.r#type == "addressed_reply");
    let tier = if has_intro && acoustic.is_some() && !conflicted {
        Tier::High
    } else if has_strong {
        Tier::Medium
    } else {
        Tier::Low
    };
    let tier = if conflicted && tier == Tier::High { Tier::Medium } else { tier };

    Verdict { tier, acoustic, reject_reason: None, target_key, evidence }
}

// ---------- identify.json 落盘与生成 ----------

use std::path::Path;

pub const IDENTIFY_FILE: &str = "identify.json";
pub const IDENTIFY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct IdentifyDoc {
    pub schema_version: u32,
    pub generated_at: String,
    pub provider: String,
    pub model: String,
    /// 生成时 RefinedDoc.revision 与段落 source_hash:新鲜度判定的唯一依据。
    pub revision: u64,
    pub source_hash: String,
    pub assignments: Vec<IdentifyAssignment>,
    /// 拒绝记录跨代保留:key = "指纹|目标键"(目标键=resolve 后 person_id 或
    /// "name:<新名>")。拒绝「是张伟」不封杀该簇的其它候选。value = decided_at。
    #[serde(default)]
    pub rejected: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentifyAssignment {
    /// 簇指纹(成员 seq 集合 hash):身份绑定的锚,不绑会变的 R 号。
    pub fingerprint: String,
    /// 生成时的 R 号(展示用)。
    pub cluster: String,
    pub person_id: Option<String>,
    pub new_name: Option<String>,
    /// 生成时裁决档位:不可变——人工动作只改 status,评测读原始 tier。
    pub tier: Tier,
    pub llm_confidence: String,
    pub acoustic: Option<(String, f32)>,
    /// AS-Norm 对称 z(P2b:仅当 cohort 足够时可算;None=小库回退裸余弦口径)。
    /// 只影响自动应用资格,不改 tier——开关关闭时行为与 P2a 完全一致。
    #[serde(default)]
    pub acoustic_z: Option<f32>,
    pub evidence: Vec<RawIdentifyEvidence>,
    /// suggested | applied | rejected。
    pub status: String,
    #[serde(default)]
    pub decided_at: Option<String>,
}

/// 打标(多人混杂)时作废相关建议:被标簇的段落所在的簇、或指向受影响人物的
/// 未决建议全部记入 rejected 表(跨代不复活)。不失效的话,打标**前**生成的旧建议
/// 在打标**后**仍可被应用,把混杂段重新灌回库(设计消费面,codex 设计轮一 P1⑥)。
/// `members`:当前修订稿的 R → 成员 seq 集(调用方从 RefinedDoc 算;无修订稿传空,
/// 此时只按人物匹配)。返回作废条数。
pub fn invalidate_for_marking(
    note_dir: &Path,
    affected_persons: &std::collections::BTreeSet<String>,
    marked_seqs: &std::collections::BTreeSet<u64>,
    members: &BTreeMap<String, std::collections::BTreeSet<u64>>,
    now: &str,
) -> usize {
    let Some(mut doc) = load_identify(note_dir) else { return 0 };
    let hits: Vec<String> = doc
        .assignments
        .iter()
        .filter(|a| a.status == "suggested" || a.status == "auto_applied")
        .filter(|a| {
            let person_hit =
                a.person_id.as_deref().is_some_and(|p| affected_persons.contains(p));
            let seq_hit = members
                .get(&a.cluster)
                .is_some_and(|m| m.intersection(marked_seqs).next().is_some());
            person_hit || seq_hit
        })
        .map(|a| a.fingerprint.clone())
        .collect();
    for fp in &hits {
        if let Err(e) = mark_rejected(&mut doc, fp, now) {
            eprintln!("identify 建议作废失败({fp},继续其余): {e}");
        }
    }
    if !hits.is_empty() {
        if let Err(e) = save_identify(note_dir, &doc) {
            eprintln!("identify 建议作废落盘失败(旧建议可能残留): {e}");
            return 0;
        }
    }
    hits.len()
}

pub fn rejected_key(fingerprint: &str, target_key: &str) -> String {
    format!("{fingerprint}|{target_key}")
}

pub fn load_identify(note_dir: &Path) -> Option<IdentifyDoc> {
    let text = std::fs::read_to_string(note_dir.join(IDENTIFY_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_identify(note_dir: &Path, doc: &IdentifyDoc) -> anyhow::Result<()> {
    let tmp = note_dir.join(format!("{IDENTIFY_FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(doc)?)?;
    std::fs::rename(&tmp, note_dir.join(IDENTIFY_FILE))?;
    Ok(())
}

/// 生成一轮 identify 结果并与旧稿合并:
/// - 旧 rejected 全表继承;命中「指纹|目标」的新建议不再生成;
/// - 旧 assignments 中 status==applied 的按指纹原样保留(用户拍板不重复打扰);
/// - 每簇至多一条新建议(LLM 被要求如此,程序侧兜底);
/// - 裁决 reject/Low 丢弃(留 stderr 痕);已 linked 的簇若建议目标与关联相同则
///   不生成(与关联矛盾的建议保留——那正是纠错价值)。
#[allow(clippy::too_many_arguments)]
pub fn run_identify(
    note_dir: &Path,
    note_id: &str,
    doc: &RefinedDoc,
    stats: &[ClusterStat],
    vp: &Voiceprints,
    acoustic_enabled: bool,
    calendar: Option<&crate::store::CalendarSnapshot>,
    executor: &dyn IdentifyExecutor,
    log: Option<&crate::ailog::Ctx>,
    now: &str,
) -> anyhow::Result<IdentifyDoc> {
    let members = cluster_members_from_doc(doc);
    let ctx = build_context(note_id, doc, stats, vp, acoustic_enabled, calendar);
    let (raws, parse_skipped) = executor.infer(&ctx, log)?;
    if parse_skipped > 0 {
        eprintln!("identify({note_id}): {parse_skipped} 条输出条目类型非法,已跳过");
    }

    let old = load_identify(note_dir);
    let rejected = old.as_ref().map(|o| o.rejected.clone()).unwrap_or_default();
    let mut assignments: Vec<IdentifyAssignment> = Vec::new();
    let mut taken: BTreeMap<String, String> = BTreeMap::new();

    // 已确认(applied)的旧条按指纹保留;指纹仍对应现稿某簇时占坑防重复建议。
    let fp_to_speaker: BTreeMap<String, String> = members
        .iter()
        .map(|(speaker, seqs)| (cluster_fingerprint(seqs), speaker.clone()))
        .collect();
    if let Some(o) = &old {
        for a in &o.assignments {
            if a.status == "applied" || a.status == "auto_applied" {
                if let Some(speaker) = fp_to_speaker.get(&a.fingerprint) {
                    let key = a
                        .person_id
                        .clone()
                        .unwrap_or_else(|| format!("name:{}", a.new_name.clone().unwrap_or_default()));
                    taken.insert(speaker.clone(), key);
                }
                assignments.push(a.clone());
            }
        }
    }

    let linked_by_cluster: BTreeMap<&str, &(String, String)> = ctx
        .clusters
        .iter()
        .filter_map(|c| c.linked.as_ref().map(|l| (c.speaker.as_str(), l)))
        .collect();

    for raw in &raws {
        if taken.contains_key(&raw.cluster) {
            continue; // 每簇一条,已确认/已收下的不再堆卡
        }
        let v = adjudicate(raw, doc, &members, stats, vp, acoustic_enabled, &taken);
        if let Some(reason) = v.reject_reason {
            eprintln!("identify({note_id}): {} 条目被裁决丢弃({reason})", raw.cluster);
            continue;
        }
        if v.tier == Tier::Low {
            continue; // 弱证据不进收件箱(只在 ailog 的原始响应里留痕)
        }
        let Some(seqs) = members.get(&raw.cluster) else { continue };
        let fingerprint = cluster_fingerprint(seqs);
        if rejected.contains_key(&rejected_key(&fingerprint, &v.target_key)) {
            continue; // 用户拒过「这个簇是这个人」,同目标不再打扰
        }
        if let Some((linked_pid, _)) = linked_by_cluster.get(raw.cluster.as_str()) {
            if *linked_pid == v.target_key {
                continue; // 与既有关联相同的建议是废话;相异的保留(纠错价值)
            }
        }
        let (person_id, new_name) = if let Some(rest) = v.target_key.strip_prefix("name:") {
            (None, Some(rest.to_string()))
        } else {
            (Some(v.target_key.clone()), None)
        };
        taken.insert(raw.cluster.clone(), v.target_key.clone());
        // AS-Norm z 只对库内已有人可算(new_name 无质心);None=小库/少簇回退。
        let acoustic_z = person_id
            .as_deref()
            .and_then(|pid| asnorm_z(&raw.cluster, pid, stats, vp));
        assignments.push(IdentifyAssignment {
            fingerprint,
            cluster: raw.cluster.clone(),
            person_id,
            new_name,
            tier: v.tier,
            llm_confidence: raw.confidence.clone(),
            acoustic: v.acoustic,
            acoustic_z,
            evidence: v.evidence,
            status: "suggested".into(),
            decided_at: None,
        });
    }

    Ok(IdentifyDoc {
        schema_version: IDENTIFY_SCHEMA_VERSION,
        generated_at: now.to_string(),
        provider: executor.provider().to_string(),
        model: executor.model().to_string(),
        revision: doc.revision,
        source_hash: crate::store::source_hash(&doc.paragraphs),
        assignments,
        rejected,
    })
}

/// Agent stdout 的哨兵提取(标记带 per-run nonce,防会议正文/指令复述碰撞):
/// 先定位**最后一个结束标记**,再向前找最近的开始标记——末尾孤立的开始标记
/// 不会遮蔽前面的完整结果。载荷容忍 Markdown 代码围栏与多行 JSON。
pub fn extract_marked(stdout: &str, start_tag: &str, end_tag: &str) -> Option<String> {
    let end = stdout.rfind(end_tag)?;
    let start = stdout[..end].rfind(start_tag)?;
    let payload = stdout[start + start_tag.len()..end].trim();
    let payload = payload
        .strip_prefix("```json")
        .or_else(|| payload.strip_prefix("```"))
        .unwrap_or(payload);
    let payload = payload.strip_suffix("```").unwrap_or(payload);
    Some(payload.trim().to_string())
}

/// 状态机:合法转移仅 suggested→{applied,auto_applied,rejected} 与
/// auto_applied→{applied(确认),rejected(撤销)}。任意串直改状态是 bug 温床。
pub fn mark_transition(
    doc: &mut IdentifyDoc,
    fingerprint: &str,
    from: &[&str],
    to: &str,
    now: &str,
) -> anyhow::Result<()> {
    const LEGAL: [(&str, &str); 5] = [
        ("suggested", "applied"),
        ("suggested", "auto_applied"),
        ("suggested", "rejected"),
        ("auto_applied", "applied"),
        ("auto_applied", "rejected"),
    ];
    let a = doc
        .assignments
        .iter_mut()
        .find(|a| a.fingerprint == fingerprint && from.contains(&a.status.as_str()))
        .ok_or_else(|| anyhow::anyhow!("建议不存在或已被处理"))?;
    anyhow::ensure!(
        LEGAL.contains(&(a.status.as_str(), to)),
        "非法状态转移: {} -> {to}",
        a.status
    );
    a.status = to.into();
    a.decided_at = Some(now.to_string());
    Ok(())
}

// ---------- P2b:自动应用意向日志(auto_pending)与资格 ----------

pub const IDENTIFY_OPS_FILE: &str = "identify_ops.json";

/// 一次自动应用的持久化记录(spec identify_journal 的最小形态)。回执永续渲染
/// 自此记录(含当时 R 号/seqs/目标/引文),不依赖当前指纹可定位;崩溃恢复按
/// stage 前滚;撤销按 undo 状态位推进,bool 推断不可靠。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentifyOp {
    pub op_id: String,
    pub fingerprint: String,
    pub cluster: String,
    pub seqs: Vec<u64>,
    pub target_person: String,
    pub target_name: String,
    /// 首条证据引文(回执上「因为 TA 说……」,稿变后仍可展示)。
    pub quote: String,
    /// 引文的证据类型(self_intro/addressed_reply/…);老记录缺省回退 self_intro。
    #[serde(default)]
    pub quote_type: String,
    pub created_at: String,
    /// pending → assigned → reinforced → done。
    pub stage: String,
    #[serde(default)]
    pub acknowledged: bool,
    /// 回灌未发生的原因(模型门禁/无段等;None=正常回灌)。
    #[serde(default)]
    pub reinforce_skipped: Option<String>,
    /// 撤销进度:None=未撤销;"undo_pending"→"link_cleared"→"undone"。
    #[serde(default)]
    pub undo_stage: Option<String>,
    /// 撤销时质心未能还原的原因(touched-since/superseded/…;链路已断,如实示人)。
    #[serde(default)]
    pub non_revertible: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct IdentifyOps {
    #[serde(default)]
    pub ops: Vec<IdentifyOp>,
}

pub fn load_ops(note_dir: &Path) -> IdentifyOps {
    std::fs::read_to_string(note_dir.join(IDENTIFY_OPS_FILE))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_ops(note_dir: &Path, ops: &IdentifyOps) -> anyhow::Result<()> {
    let tmp = note_dir.join(format!("{IDENTIFY_OPS_FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(ops)?)?;
    std::fs::rename(&tmp, note_dir.join(IDENTIFY_OPS_FILE))?;
    Ok(())
}

/// 自动应用资格(P2b):tier 不受开关影响,资格另算——
/// suggested + High + 库内人 + 声学正向确认存在 + z 可算时 z≥SEED_ASSIGN_Z +
/// 该簇当前全部段 person_id 空且 name 空(有手填名的簇绝不自动动,撤销无法恢复名)。
pub fn auto_apply_targets<'a>(
    idoc: &'a IdentifyDoc,
    doc: &RefinedDoc,
) -> Vec<&'a IdentifyAssignment> {
    let members = cluster_members_from_doc(doc);
    let fp_to_speaker: BTreeMap<String, String> = members
        .iter()
        .map(|(sp, seqs)| (cluster_fingerprint(seqs), sp.clone()))
        .collect();
    idoc.assignments
        .iter()
        .filter(|a| {
            a.status == "suggested"
                && a.tier == Tier::High
                && a.person_id.is_some()
                && a.acoustic.is_some()
                && a.acoustic_z.map_or(true, |z| z >= crate::diar::registry::SEED_ASSIGN_Z)
                && fp_to_speaker.get(&a.fingerprint).is_some_and(|sp| {
                    doc.paragraphs.iter().filter(|p| &p.speaker == sp).all(|p| {
                        p.person_id.is_none()
                            && p.name.as_deref().map_or(true, |n| n.trim().is_empty())
                    })
                })
        })
        .collect()
}

/// 人工确认:suggested → applied。找不到(已处理/指纹不符)报错,调用方转成
/// 「建议已失效」提示。
pub fn mark_applied(doc: &mut IdentifyDoc, fingerprint: &str, now: &str) -> anyhow::Result<()> {
    mark_transition(doc, fingerprint, &["suggested"], "applied", now)
}

/// 人工拒绝:suggested → rejected,并把「指纹|目标」写入拒绝表——同目标永不
/// 再建议,其它候选不受影响。
pub fn mark_rejected(doc: &mut IdentifyDoc, fingerprint: &str, now: &str) -> anyhow::Result<()> {
    let target = {
        let a = doc
            .assignments
            .iter()
            .find(|a| {
                a.fingerprint == fingerprint
                    && (a.status == "suggested" || a.status == "auto_applied")
            })
            .ok_or_else(|| anyhow::anyhow!("建议不存在或已被处理"))?;
        a.person_id
            .clone()
            .unwrap_or_else(|| format!("name:{}", a.new_name.clone().unwrap_or_default()))
    };
    mark_transition(doc, fingerprint, &["suggested", "auto_applied"], "rejected", now)?;
    doc.rejected.insert(rejected_key(fingerprint, &target), now.to_string());
    Ok(())
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
            emails: Vec::new(), voiceprint_quarantined: false,
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
    fn recall_excludes_quarantined_person_on_every_path() {
        // P1 既是声学最近又是 last_seen 最近——隔离后两条路都不得召回他。
        let mut p1 = person("阿隔", "mic", vec![1.0, 0.0, 0.0, 0.0], "2026-08-19");
        p1.voiceprint_quarantined = true;
        let vp = vp_with(vec![
            ("P1", p1),
            ("P2", person("阿备", "mic", vec![0.9, 0.1, 0.0, 0.0], "2026-01-01")),
        ]);
        let stats = vec![stat("R1", "mic", vec![1.0, 0.0, 0.0, 0.0], 60_000)];
        let got = recall_candidates(&stats, &vp, true, 2, 2, 10);
        let ids: Vec<&str> = got.iter().map(|c| c.person_id.as_str()).collect();
        assert!(!ids.contains(&"P1"), "隔离人物不得被任何一路召回: {ids:?}");
        assert!(ids.contains(&"P2"));
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
        let ctx = build_context("n1", &doc, &stats, &Voiceprints::default(), true, None);
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
    // ---------- 裁决测试 ----------

    fn raw(cluster: &str, person_id: Option<&str>, new_name: Option<&str>, evs: Vec<RawIdentifyEvidence>) -> RawAssignment {
        RawAssignment {
            cluster: cluster.into(),
            person_id: person_id.map(Into::into),
            new_name: new_name.map(Into::into),
            confidence: "high".into(),
            evidence: evs,
        }
    }

    fn ev(idx: usize, start: usize, end: usize, quote: &str, ty: &str) -> RawIdentifyEvidence {
        RawIdentifyEvidence { paragraph_index: idx, start, end, quote: quote.into(), r#type: ty.into() }
    }

    /// 库:P1=张伟(mic 质心与 R1 同向)。稿:R1 说「我是张伟」,R2 喊「张伟你说」。
    fn adjudicate_fixture() -> (RefinedDoc, BTreeMap<String, BTreeSet<u64>>, Vec<ClusterStat>, Voiceprints) {
        let doc = doc_with(vec![
            para("R1", &[0], "我是张伟,先说结论"),
            para("R2", &[1], "张伟你说说进展"),
            para("R1", &[2], "好,三点"),
        ]);
        let members = cluster_members_from_doc(&doc);
        let stats = vec![
            stat("R1", "mic", vec![1.0, 0.0, 0.0, 0.0], 60_000),
            stat("R2", "mic", vec![0.0, 1.0, 0.0, 0.0], 30_000),
        ];
        let vp = vp_with(vec![("P1", person("张伟", "mic", vec![1.0, 0.0, 0.0, 0.0], "2026-08-01"))]);
        (doc, members, stats, vp)
    }

    #[test]
    fn parse_skips_bad_entries_keeps_good() {
        let content = r#"{"assignments":[
            {"cluster":"R1","person_id":"P1","confidence":"high","evidence":[]},
            {"cluster":42},
            {"cluster":"R2","new_name":"李雷","confidence":"medium","evidence":[]}
        ]}"#;
        let (ok, skipped) = parse_raw_identify(content).unwrap();
        assert_eq!(ok.len(), 2);
        assert_eq!(skipped, 1);
        assert!(parse_raw_identify("{}").is_err(), "缺 assignments 整体报错");
    }

    #[test]
    fn evidence_quote_and_char_range_must_match() {
        let (doc, members, stats, vp) = adjudicate_fixture();
        // 「我是张伟」= 段0 chars [0,4)。
        let good = adjudicate(&raw("R1", Some("P1"), None, vec![ev(0, 0, 4, "我是张伟", "self_intro")]), &doc, &members, &stats, &vp, true, &BTreeMap::new());
        assert!(good.reject_reason.is_none());
        let bad = adjudicate(&raw("R1", Some("P1"), None, vec![ev(0, 0, 4, "我是李雷", "self_intro")]), &doc, &members, &stats, &vp, true, &BTreeMap::new());
        assert_eq!(bad.reject_reason, Some("evidence-mismatch"));
        let oob = adjudicate(&raw("R1", Some("P1"), None, vec![ev(0, 0, 999, "x", "self_intro")]), &doc, &members, &stats, &vp, true, &BTreeMap::new());
        assert_eq!(oob.reject_reason, Some("evidence-mismatch"));
    }

    #[test]
    fn self_intro_quote_must_contain_target_name() {
        let (doc, members, stats, mut vp) = adjudicate_fixture();
        // 库里再加个李雷:quote「我是张伟」却指认李雷 → self_intro 降级,只剩弱证据 → Low。
        vp.people.insert("P2".into(), person("李雷", "mic", vec![0.0, 0.0, 1.0, 0.0], "2026-08-02"));
        let v = adjudicate(&raw("R1", Some("P2"), None, vec![ev(0, 0, 4, "我是张伟", "self_intro")]), &doc, &members, &stats, &vp, true, &BTreeMap::new());
        assert!(v.reject_reason.is_none());
        assert_eq!(v.tier, Tier::Low, "偷梁换柱的 self_intro 只算弱证据");
    }

    #[test]
    fn addressed_reply_needs_call_and_reply_pair() {
        let (doc, members, stats, vp) = adjudicate_fixture();
        // 完整配对:他簇称呼(段1 R2 喊「张伟你说说进展」,含名)+ 本簇应答(段2 R1)。
        let paired = adjudicate(
            &raw("R1", Some("P1"), None, vec![
                ev(1, 0, 5, "张伟你说说", "addressed_reply"),
                ev(2, 0, 1, "好", "addressed_reply"),
            ]),
            &doc, &members, &stats, &vp, true, &BTreeMap::new(),
        );
        assert!(paired.reject_reason.is_none());
        assert_eq!(paired.tier, Tier::Medium, "称呼应答配对成立=中档");
        // 单条只有称呼,没有本簇应答 → 降为弱证据 → Low。
        let single = adjudicate(
            &raw("R1", Some("P1"), None, vec![ev(1, 0, 5, "张伟你说说", "addressed_reply")]),
            &doc, &members, &stats, &vp, true, &BTreeMap::new(),
        );
        assert_eq!(single.tier, Tier::Low);
    }

    #[test]
    fn conflicts_and_acoustic_gate_cap_tier() {
        let (doc, members, stats, vp) = adjudicate_fixture();
        let intro = vec![ev(0, 0, 4, "我是张伟", "self_intro")];
        // 声学同向 + self_intro → High。
        let high = adjudicate(&raw("R1", Some("P1"), None, intro.clone()), &doc, &members, &stats, &vp, true, &BTreeMap::new());
        assert_eq!(high.tier, Tier::High);
        assert!(high.acoustic.is_some());
        // 门禁关闭 → 声学 None → Medium。
        let gated = adjudicate(&raw("R1", Some("P1"), None, intro.clone()), &doc, &members, &stats, &vp, false, &BTreeMap::new());
        assert_eq!(gated.tier, Tier::Medium);
        assert!(gated.acoustic.is_none());
        // 冲突:张伟已被 R2 认领 → 上限 Medium。
        let mut taken = BTreeMap::new();
        taken.insert("R2".to_string(), "P1".to_string());
        let conflicted = adjudicate(&raw("R1", Some("P1"), None, intro), &doc, &members, &stats, &vp, true, &taken);
        assert_eq!(conflicted.tier, Tier::Medium);
    }

    #[test]
    fn structural_rejects_and_new_name_ceiling() {
        let (doc, members, stats, vp) = adjudicate_fixture();
        let both = adjudicate(&raw("R1", Some("P1"), Some("张伟"), vec![]), &doc, &members, &stats, &vp, true, &BTreeMap::new());
        assert_eq!(both.reject_reason, Some("person-xor-new-name"));
        let dangling = adjudicate(&raw("R1", Some("P99"), None, vec![]), &doc, &members, &stats, &vp, true, &BTreeMap::new());
        assert_eq!(dangling.reject_reason, Some("dangling-person"));
        let badname = adjudicate(&raw("R1", None, Some("   "), vec![]), &doc, &members, &stats, &vp, true, &BTreeMap::new());
        assert_eq!(badname.reject_reason, Some("bad-new-name"));
        let unknown_cluster = adjudicate(&raw("R9", Some("P1"), None, vec![]), &doc, &members, &stats, &vp, true, &BTreeMap::new());
        assert_eq!(unknown_cluster.reject_reason, Some("unknown-cluster"));
        // new_name:自报「我是张伟」建新人张伟——证据成立但无质心可比 → 天花板 Medium。
        let newp = adjudicate(&raw("R1", None, Some("张伟"), vec![ev(0, 0, 4, "我是张伟", "self_intro")]), &doc, &members, &stats, &vp, true, &BTreeMap::new());
        assert!(newp.reject_reason.is_none());
        assert_eq!(newp.tier, Tier::Medium);
        assert_eq!(newp.target_key, "name:张伟");
        // 未知 evidence type 丢弃后无有效证据 → reject。
        let noev = adjudicate(&raw("R1", Some("P1"), None, vec![ev(0, 0, 4, "我是张伟", "made_up")]), &doc, &members, &stats, &vp, true, &BTreeMap::new());
        assert_eq!(noev.reject_reason, Some("no-valid-evidence"));
    }
    // ---------- run_identify 合并规则测试 ----------

    struct MockExec(Vec<RawAssignment>);
    impl IdentifyExecutor for MockExec {
        fn provider(&self) -> &str { "mock" }
        fn model(&self) -> &str { "mock-m" }
        fn infer(&self, _ctx: &IdentifyContext, _log: Option<&crate::ailog::Ctx>) -> anyhow::Result<(Vec<RawAssignment>, usize)> {
            Ok((self.0.clone(), 0))
        }
    }

    #[test]
    fn run_identify_merges_rejected_and_applied_across_generations() {
        let dir = tempfile::tempdir().unwrap();
        let (doc, members, stats, vp) = adjudicate_fixture();
        let fp_r1 = cluster_fingerprint(&members["R1"]);

        // 第一轮:R1 建议张伟(P1)。
        let exec = MockExec(vec![raw("R1", Some("P1"), None, vec![ev(0, 0, 4, "我是张伟", "self_intro")])]);
        let d1 = run_identify(dir.path(), "n1", &doc, &stats, &vp, true, None, &exec, None, "t1").unwrap();
        assert_eq!(d1.assignments.len(), 1);
        assert_eq!(d1.assignments[0].status, "suggested");
        assert_eq!(d1.assignments[0].tier, Tier::High);
        assert_eq!(d1.assignments[0].fingerprint, fp_r1);

        // 用户拒绝「R1 是 P1」后重跑:同目标不再生成;换目标(new_name)可以。
        let mut d1_rejected = d1;
        d1_rejected.rejected.insert(rejected_key(&fp_r1, "P1"), "t1x".into());
        d1_rejected.assignments.clear();
        save_identify(dir.path(), &d1_rejected).unwrap();
        let d2 = run_identify(dir.path(), "n1", &doc, &stats, &vp, true, None, &exec, None, "t2").unwrap();
        assert!(d2.assignments.is_empty(), "拒过的同目标建议不再生成");
        assert!(d2.rejected.contains_key(&rejected_key(&fp_r1, "P1")), "拒绝表跨代继承");
        let exec_new = MockExec(vec![raw("R1", None, Some("张老板"), vec![ev(0, 0, 4, "我是张伟", "self_intro")])]);
        let d3 = run_identify(dir.path(), "n1", &doc, &stats, &vp, true, None, &exec_new, None, "t3").unwrap();
        // 「我是张伟」不含「张老板」→ self_intro 降级 → Low → 不落建议;
        // 换个含名证据检查放行路径:
        assert!(d3.assignments.is_empty());

        // applied 的旧条按指纹保留,且占坑挡新建议。
        let mut d4 = IdentifyDoc {
            schema_version: IDENTIFY_SCHEMA_VERSION,
            generated_at: "t4".into(), provider: "mock".into(), model: "m".into(),
            revision: 3, source_hash: "h".into(),
            assignments: vec![IdentifyAssignment {
                fingerprint: fp_r1.clone(), cluster: "R1".into(),
                person_id: Some("P1".into()), new_name: None,
                tier: Tier::High, llm_confidence: "high".into(), acoustic: None, acoustic_z: None,
                evidence: vec![], status: "applied".into(), decided_at: Some("t4".into()),
            }],
            rejected: BTreeMap::new(),
        };
        d4.rejected.clear();
        save_identify(dir.path(), &d4).unwrap();
        let d5 = run_identify(dir.path(), "n1", &doc, &stats, &vp, true, None, &exec, None, "t5").unwrap();
        assert_eq!(d5.assignments.len(), 1, "applied 保留且不重复建议");
        assert_eq!(d5.assignments[0].status, "applied");
    }

    #[test]
    fn run_identify_skips_suggestion_equal_to_existing_link() {
        let dir = tempfile::tempdir().unwrap();
        let (mut doc, _, stats, vp) = adjudicate_fixture();
        // R1 已关联 P1:同目标建议是废话,不生成。
        for p in doc.paragraphs.iter_mut().filter(|p| p.speaker == "R1") {
            p.person_id = Some("P1".into());
            p.name = Some("张伟".into());
        }
        let exec = MockExec(vec![raw("R1", Some("P1"), None, vec![ev(0, 0, 4, "我是张伟", "self_intro")])]);
        let d = run_identify(dir.path(), "n1", &doc, &stats, &vp, true, None, &exec, None, "t").unwrap();
        assert!(d.assignments.is_empty());
    }
    #[test]
    fn mark_applied_and_rejected_update_status_and_denylist() {
        let mut idoc = IdentifyDoc {
            schema_version: IDENTIFY_SCHEMA_VERSION,
            generated_at: "t".into(), provider: "mock".into(), model: "m".into(),
            revision: 1, source_hash: "h".into(),
            assignments: vec![IdentifyAssignment {
                fingerprint: "fp1".into(), cluster: "R1".into(),
                person_id: Some("P1".into()), new_name: None,
                tier: Tier::High, llm_confidence: "high".into(), acoustic: None, acoustic_z: None,
                evidence: vec![], status: "suggested".into(), decided_at: None,
            }],
            rejected: BTreeMap::new(),
        };
        assert!(mark_applied(&mut idoc, "nope", "t1").is_err());
        mark_applied(&mut idoc, "fp1", "t1").unwrap();
        assert_eq!(idoc.assignments[0].status, "applied");
        assert!(mark_applied(&mut idoc, "fp1", "t2").is_err(), "已处理不可重复");

        idoc.assignments[0].status = "suggested".into();
        mark_rejected(&mut idoc, "fp1", "t3").unwrap();
        assert_eq!(idoc.assignments[0].status, "rejected");
        assert!(idoc.rejected.contains_key(&rejected_key("fp1", "P1")));
    }
    #[test]
    fn calendar_candidates_email_first_then_name_and_prompt_cap() {
        let mut vp = vp_with(vec![
            ("P1", person("张伟", "mic", vec![1.0, 0.0, 0.0, 0.0], "2026-01-01")),
            ("P2", person("李雷", "mic", vec![0.0, 1.0, 0.0, 0.0], "2026-01-02")),
        ]);
        vp.people.get_mut("P2").unwrap().emails.push("lilei@x.com".into());
        let mut attendees: Vec<crate::store::CalendarAttendee> = vec![
            crate::store::CalendarAttendee { name: "别人".into(), email: "lilei@x.com".into(), is_me: false },
            crate::store::CalendarAttendee { name: "张伟".into(), email: String::new(), is_me: true },
        ];
        for i in 0..40 {
            attendees.push(crate::store::CalendarAttendee { name: format!("路人{i}"), email: String::new(), is_me: false });
        }
        let cal = crate::store::CalendarSnapshot {
            event_id: "ev".into(), title: "周会".into(), attendees,
            matched_at: "t".into(), match_kind: "auto".into(),
        };
        // 第④路:email 命中(P2)排最前,名字命中(P1)随后。
        let got = calendar_candidates(&cal, &vp);
        assert_eq!(got[0].person_id, "P2", "email 精确命中最优先");
        assert!(got.iter().any(|c| c.person_id == "P1"), "名字精确命中入选");
        // build_context:calendar 进 ctx,参会人 cap+is_me 标注+截断占位。
        let doc = doc_with(vec![para("R1", &[0], "开场")]);
        let ctx = build_context("n1", &doc, &[], &vp, false, Some(&cal));
        assert_eq!(ctx.candidates[0].person_id, "P2", "日历候选排最前");
        let json = serde_json::to_string(&ctx).unwrap();
        assert!(json.contains("calendar"), "calendar 序列化进 prompt 主体");
        let cc = ctx.calendar.unwrap();
        assert_eq!(cc.title, "周会");
        assert!(cc.attendees.iter().any(|a| a == "张伟(我)"), "is_me 标注");
        assert_eq!(cc.attendees.len(), CALENDAR_ATTENDEES_IN_PROMPT + 1, "cap+占位行");
        assert!(cc.attendees.last().unwrap().contains("人略"));
    }

    #[test]
    fn extract_marked_survives_noise_orphans_and_fences() {
        let (st, en) = ("<VN_IDENTIFY_ab>", "</VN_IDENTIFY_ab>");
        // 横幅噪音 + 围栏 + 末尾孤立开始标记:仍取前面的完整对。
        let out = format!(
            "banner\nthinking...\n{st}\n```json\n{{\"assignments\":[]}}\n```\n{en}\ntail {st}"
        );
        assert_eq!(extract_marked(&out, st, en).unwrap(), "{\"assignments\":[]}");
        // 缺结束标记 → None。
        assert!(extract_marked(&format!("x {st} y"), st, en).is_none());
        // 两对取最后一对。
        let two = format!("{st}old{en} mid {st}new{en}");
        assert_eq!(extract_marked(&two, st, en).unwrap(), "new");
    }

    // ---------- P2b:状态机 / 自动资格 / 合并保留 ----------

    fn ass(fp: &str, tier: Tier, person: Option<&str>, status: &str, ac: bool, z: Option<f32>) -> IdentifyAssignment {
        IdentifyAssignment {
            fingerprint: fp.into(),
            cluster: "R1".into(),
            person_id: person.map(Into::into),
            new_name: person.is_none().then(|| "新人".to_string()),
            tier,
            llm_confidence: "high".into(),
            acoustic: ac.then(|| ("mic".to_string(), 0.8)),
            acoustic_z: z,
            evidence: vec![],
            status: status.into(),
            decided_at: None,
        }
    }

    fn idoc_with(assignments: Vec<IdentifyAssignment>) -> IdentifyDoc {
        IdentifyDoc {
            schema_version: IDENTIFY_SCHEMA_VERSION,
            generated_at: "t".into(),
            provider: "mock".into(),
            model: "m".into(),
            revision: 1,
            source_hash: "h".into(),
            assignments,
            rejected: BTreeMap::new(),
        }
    }

    #[test]
    fn mark_transition_enforces_legal_moves() {
        let mut d = idoc_with(vec![ass("fp", Tier::High, Some("P1"), "suggested", true, None)]);
        mark_transition(&mut d, "fp", &["suggested"], "auto_applied", "t1").unwrap();
        assert_eq!(d.assignments[0].status, "auto_applied");
        // auto_applied → applied(确认)。
        mark_transition(&mut d, "fp", &["auto_applied"], "applied", "t2").unwrap();
        // applied 是终态:任何再转移都拒绝。
        assert!(mark_transition(&mut d, "fp", &["applied"], "rejected", "t3").is_err());
        // 撤销路径:auto_applied → rejected(mark_rejected 会带拒绝键)。
        let mut d2 = idoc_with(vec![ass("fp", Tier::High, Some("P1"), "auto_applied", true, None)]);
        mark_rejected(&mut d2, "fp", "t4").unwrap();
        assert_eq!(d2.assignments[0].status, "rejected");
        assert!(d2.rejected.contains_key(&rejected_key("fp", "P1")));
    }

    #[test]
    fn auto_apply_targets_gate_all_preconditions() {
        // 稿:R1(无关联)与 R2(有手填名)。
        let mut p2 = para("R2", &[5], "b");
        p2.name = Some("手填".into());
        let doc = doc_with(vec![para("R1", &[0, 1], "a"), p2]);
        let members = cluster_members_from_doc(&doc);
        let fp1 = cluster_fingerprint(&members["R1"]);
        let fp2 = cluster_fingerprint(&members["R2"]);
        let d = idoc_with(vec![
            ass(&fp1, Tier::High, Some("P1"), "suggested", true, Some(3.5)), // 全过
            ass(&fp2, Tier::High, Some("P1"), "suggested", true, None),      // 簇有手填名 → 拒
            ass("fp-x", Tier::High, Some("P1"), "suggested", true, None),    // 指纹失配 → 拒
        ]);
        let got: Vec<&str> = auto_apply_targets(&d, &doc).iter().map(|a| a.fingerprint.as_str()).collect();
        assert_eq!(got, vec![fp1.as_str()]);
        // z 可算但不达标 → 拒;new_name → 拒;Medium → 拒;无声学确认 → 拒。
        let d2 = idoc_with(vec![
            ass(&fp1, Tier::High, Some("P1"), "suggested", true, Some(2.0)),
        ]);
        assert!(auto_apply_targets(&d2, &doc).is_empty(), "z<3 拒");
        let d3 = idoc_with(vec![ass(&fp1, Tier::High, None, "suggested", true, None)]);
        assert!(auto_apply_targets(&d3, &doc).is_empty(), "new_name 永不自动建档");
        let d4 = idoc_with(vec![ass(&fp1, Tier::Medium, Some("P1"), "suggested", true, None)]);
        assert!(auto_apply_targets(&d4, &doc).is_empty(), "Medium 拒");
        let d5 = idoc_with(vec![ass(&fp1, Tier::High, Some("P1"), "suggested", false, None)]);
        assert!(auto_apply_targets(&d5, &doc).is_empty(), "无声学确认拒");
    }

    #[test]
    fn run_identify_merge_preserves_auto_applied() {
        let dir = tempfile::tempdir().unwrap();
        let (doc, members, stats, vp) = adjudicate_fixture();
        let fp = cluster_fingerprint(&members["R1"]);
        let mut seeded = idoc_with(vec![ass(&fp, Tier::High, Some("P1"), "auto_applied", true, None)]);
        seeded.source_hash = crate::store::source_hash(&doc.paragraphs);
        save_identify(dir.path(), &seeded).unwrap();
        let exec = MockExec(vec![]);
        let d = run_identify(dir.path(), "n1", &doc, &stats, &vp, true, None, &exec, None, "t").unwrap();
        assert_eq!(d.assignments.len(), 1, "auto_applied 回执跨代保留");
        assert_eq!(d.assignments[0].status, "auto_applied");
    }

    #[test]
    fn identify_ops_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut ops = IdentifyOps::default();
        ops.ops.push(IdentifyOp {
            op_id: "iop-1".into(),
            fingerprint: "fp".into(),
            cluster: "R1".into(),
            seqs: vec![0, 1],
            target_person: "P1".into(),
            target_name: "张伟".into(),
            quote: "我是张伟".into(),
            quote_type: "self_intro".into(),
            created_at: "t".into(),
            stage: "done".into(),
            acknowledged: false,
            reinforce_skipped: None,
            undo_stage: None,
            non_revertible: None,
        });
        save_ops(dir.path(), &ops).unwrap();
        let back = load_ops(dir.path());
        assert_eq!(back.ops.len(), 1);
        assert_eq!(back.ops[0].op_id, "iop-1");
    }

}