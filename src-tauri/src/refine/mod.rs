//! 会后 Aing 管线编排:过滤(A3)→重聚类(A1)→段落化,可选 LLM Aing(A2)。
//! 原始三文件只读;一切产物写 refined.json。

pub mod agent;
pub mod backfill;
pub mod filter;
pub mod llm;
pub mod recluster;
pub mod relations;

use crate::diar::registry::SeedCluster;
use crate::diar::SpeakerEmbedder;
use crate::store::{
    Entity, GraphExtraction, Mention, RefineStages, RefinedDoc, RefinedParagraph, SegmentRecord,
    SpeakerMeta,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use unicode_casefold::UnicodeCaseFold;

/// 同说话人合并段落的时长上限(对齐豆包排版粒度)。
pub const MAX_PARA_MS: u64 = 60_000;

/// 关系回退不是两个顶层字段的拷贝，而是一份可独立解释的最小图谱快照。
/// 端点实体和关系显式引用的 mentions 与旧关系共存亡，避免把旧 `ent_N`
/// 静默重定向到本轮新抽取出的同号实体。
#[derive(Clone, Default)]
struct GraphFallbackSnapshot {
    graph_extraction: Option<GraphExtraction>,
    relations: Vec<crate::store::RelationFact>,
    entities: Vec<Entity>,
    mentions: Vec<(usize, Mention)>,
}

impl GraphFallbackSnapshot {
    fn capture(doc: &RefinedDoc) -> Self {
        let mut endpoint_ids = BTreeSet::new();
        let mut referenced_mentions: BTreeMap<&str, &str> = BTreeMap::new();
        for relation in &doc.relations {
            endpoint_ids.insert(relation.subject.as_str());
            endpoint_ids.insert(relation.object.as_str());
            for id in &relation.subject_mentions {
                referenced_mentions.insert(id, relation.subject.as_str());
            }
            for id in &relation.object_mentions {
                referenced_mentions.insert(id, relation.object.as_str());
            }
        }

        let entities: Vec<_> = doc
            .entities
            .iter()
            .filter(|entity| endpoint_ids.contains(entity.id.as_str()))
            .cloned()
            .collect();
        let mentions: Vec<_> = doc
            .paragraphs
            .iter()
            .enumerate()
            .flat_map(|(paragraph_index, paragraph)| {
                paragraph
                    .mentions
                    .iter()
                    .filter(|mention| referenced_mentions.contains_key(mention.id.as_str()))
                    .cloned()
                    .map(move |mention| (paragraph_index, mention))
            })
            .collect();

        let entity_ids: BTreeSet<_> = entities.iter().map(|entity| entity.id.as_str()).collect();
        let all_mention_owners: BTreeMap<_, _> = doc
            .paragraphs
            .iter()
            .flat_map(|paragraph| &paragraph.mentions)
            .map(|mention| (mention.id.as_str(), mention.entity.as_str()))
            .collect();
        let captured_mention_owners: BTreeMap<_, _> = mentions
            .iter()
            .map(|(_, mention)| (mention.id.as_str(), mention.entity.as_str()))
            .collect();
        let coherent = endpoint_ids.iter().all(|id| entity_ids.contains(id))
            && referenced_mentions
                .iter()
                .all(|(id, owner)| captured_mention_owners.get(*id).copied() == Some(*owner))
            && doc.relations.iter().all(|relation| {
                !relation.subject_mentions.is_empty()
                    && !relation.object_mentions.is_empty()
                    && relation.subject_mentions.iter().all(|id| {
                        all_mention_owners.get(id.as_str()).copied()
                            == Some(relation.subject.as_str())
                    })
                    && relation.object_mentions.iter().all(|id| {
                        all_mention_owners.get(id.as_str()).copied()
                            == Some(relation.object.as_str())
                    })
                    && !relation.evidence.is_empty()
                    && relation.evidence.iter().all(|evidence| {
                        !evidence.quote.is_empty()
                            && !evidence.source_seqs.is_empty()
                            && evidence.start < evidence.end
                    })
            });

        if !coherent {
            return Self::default();
        }
        Self {
            graph_extraction: doc.graph_extraction.clone(),
            relations: doc.relations.clone(),
            entities,
            mentions,
        }
    }

    fn restore(self, note_id: &str, doc: &mut RefinedDoc) {
        if !self.relations.is_empty() && doc.paragraphs.is_empty() {
            // 没有段落就无处保存关系引用的 mention；清空比制造悬空引用安全。
            doc.graph_extraction = None;
            doc.relations.clear();
            doc.graph_support_mentions.clear();
            return;
        }

        let support_by_id: BTreeMap<_, _> = self
            .entities
            .iter()
            .map(|entity| (entity.id.clone(), entity.clone()))
            .collect();
        let mut used_ids: BTreeSet<_> = support_by_id.keys().cloned().collect();
        let mut remapped_ids = BTreeMap::new();
        let mut next_local_id = 1usize;

        for entity in &mut doc.entities {
            let reuse = self.entities.iter().find(|support| {
                entity_key(&support.name) == entity_key(&entity.name)
                    && entity_key(&support.kind) == entity_key(&entity.kind)
            });
            let desired = if let Some(support) = reuse {
                support.id.clone()
            } else if !entity.id.is_empty() && !used_ids.contains(&entity.id) {
                entity.id.clone()
            } else {
                loop {
                    let candidate = format!("ent_{next_local_id}");
                    next_local_id += 1;
                    if !used_ids.contains(&candidate) {
                        break candidate;
                    }
                }
            };
            if entity.id != desired {
                remapped_ids.insert(entity.id.clone(), desired.clone());
                entity.id = desired.clone();
            }
            used_ids.insert(desired);
        }

        for paragraph in &mut doc.paragraphs {
            for mention in &mut paragraph.mentions {
                if let Some(new_id) = remapped_ids.get(&mention.entity) {
                    mention.entity = new_id.clone();
                    mention.id.clear();
                }
            }
        }

        let mut merged_entities = support_by_id;
        for entity in std::mem::take(&mut doc.entities) {
            if let Some(existing) = merged_entities.get_mut(&entity.id) {
                merge_entity_aliases(existing, &entity);
            } else {
                merged_entities.insert(entity.id.clone(), entity);
            }
        }
        doc.entities = merged_entities.into_values().collect();

        // 先让本轮 mentions 在重映射后的实体 id 上生成自己的稳定 id。
        doc.graph_extraction = None;
        doc.relations.clear();
        crate::store::ensure_graph_ids(note_id, doc);

        let mut support_mention_ids: BTreeSet<_> = doc.graph_support_mentions.drain(..).collect();
        for (old_index, mention) in self.mentions {
            let safe_index = old_index.min(doc.paragraphs.len() - 1);
            let fresh_match = doc.paragraphs.get_mut(old_index).and_then(|paragraph| {
                paragraph.mentions.iter_mut().find(|current| {
                    current.entity == mention.entity
                        && current.start == mention.start
                        && current.end == mention.end
                })
            });
            if let Some(current) = fresh_match {
                // 必须同时匹配实体归属、原段落和区间；stable id 相同本身不足以证明
                // occurrence 已回到当前正文。命中后沿用旧关系引用 id，并提升回 live。
                current.id = mention.id.clone();
                support_mention_ids.remove(&mention.id);
                continue;
            }
            for paragraph in &mut doc.paragraphs {
                paragraph
                    .mentions
                    .retain(|current| current.id != mention.id);
            }
            support_mention_ids.insert(mention.id.clone());
            doc.paragraphs[safe_index].mentions.push(mention);
        }
        for paragraph in &mut doc.paragraphs {
            paragraph.mentions.sort_by(|left, right| {
                left.start
                    .cmp(&right.start)
                    .then(left.end.cmp(&right.end))
                    .then(left.entity.cmp(&right.entity))
                    .then(left.id.cmp(&right.id))
            });
        }

        doc.graph_extraction = self.graph_extraction;
        doc.relations = self.relations;
        crate::store::ensure_graph_ids(note_id, doc);
        let actual_mentions: BTreeSet<_> = doc
            .paragraphs
            .iter()
            .flat_map(|paragraph| &paragraph.mentions)
            .map(|mention| mention.id.as_str())
            .collect();
        doc.graph_support_mentions = support_mention_ids
            .into_iter()
            .filter(|id| actual_mentions.contains(id.as_str()))
            .collect();
    }
}

fn merge_entity_aliases(existing: &mut Entity, current: &Entity) {
    for alias in current.aliases.iter().chain(std::iter::once(&current.name)) {
        let key = entity_key(alias);
        if key != entity_key(&existing.name)
            && !existing
                .aliases
                .iter()
                .any(|candidate| entity_key(candidate) == key)
        {
            existing.aliases.push(alias.clone());
        }
    }
    existing.aliases.sort_by_key(|alias| entity_key(alias));
}

pub fn run_local(
    note_dir: &Path,
    segs: &[SegmentRecord],
    speakers: &BTreeMap<String, SpeakerMeta>,
    embedder: Option<&mut dyn SpeakerEmbedder>,
    seeds: &[SeedCluster],
    generated_at: &str,
) -> anyhow::Result<RefinedDoc> {
    let discarded = filter::discarded_seqs(segs);
    let kept: Vec<&SegmentRecord> = segs
        .iter()
        .filter(|s| !discarded.contains(&s.seq))
        .collect();

    let inputs: Vec<recluster::SegInput> = kept
        .iter()
        .map(|s| recluster::SegInput {
            seq: s.seq,
            start_ms: s.start_ms,
            end_ms: s.end_ms,
            source: s.source.clone(),
            old_speaker: s.speaker.clone(),
        })
        .collect();

    let (assign, recluster_state) = match embedder {
        Some(e) => match embed_all(note_dir, &kept, e) {
            Ok(embs) => (recluster::recluster(&inputs, &embs, seeds), "done"),
            Err(err) => {
                eprintln!("refine: 嵌入失败,重聚类降级: {err}");
                (fallback_assign(&inputs), "failed")
            }
        },
        None => (fallback_assign(&inputs), "skipped"),
    };

    let paragraphs = build_paragraphs(segs, &discarded, &assign, speakers);
    let mut doc = RefinedDoc {
        schema_version: crate::store::refined::REFINED_SCHEMA_VERSION,
        generated_at: generated_at.to_string(),
        llm_model: None,
        stages: RefineStages {
            filter: "done".into(),
            recluster: recluster_state.into(),
            llm: "off".into(),
            entities: "off".into(),
            relations: "off".into(),
        },
        discarded_seqs: discarded,
        entities: vec![],
        graph_extraction: None,
        relations: vec![],
        graph_support_mentions: vec![],
        paragraphs,
    };
    let note_lock = crate::store::notelock::NoteLock::acquire(note_dir)?
        .ok_or_else(|| anyhow::anyhow!("笔记正被其它进程写入，无法提交本地 Aing"))?;
    let note_id = note_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("修订稿目录缺少有效笔记 id"))?;
    if let Some(previous) = crate::store::refined::load_refined_locked(note_dir, &note_lock) {
        GraphFallbackSnapshot::capture(&previous).restore(note_id, &mut doc);
    }
    crate::store::ensure_graph_ids(note_id, &mut doc);
    crate::store::refined::write_refined_atomic_locked(note_dir, &doc, &note_lock)?;
    Ok(doc)
}

/// 时间轴 ms 区间 → 该轨文件内的 PCM 样本下标区间(16kHz 单声道,1ms=16 样本)。
///
/// F2 修复:每条轨道的文件内 0 点并不总是笔记时间轴的 0 点——续录/中途授权的轨道
/// audio.json 里记着非零 offset_ms(不变式:文件内毫秒 + offset_ms == 段时间轴毫秒,
/// 见 store/audio.rs `TrackMeta::offset_ms` 文档)。故切片前须先把时间轴 ms 减掉该轨
/// offset_ms 换算回文件内 ms,而不能假设轨道 0 时刻就是笔记 0 点。
/// 越界一律 clamp 到 pcm_len;换算后 end<=start(轨道 offset 之后、或段整体早于该轨
/// 出现时刻)返回 None,调用方按"该段在这轨没有可用音频"处理。
fn slice_range(
    start_ms: u64,
    end_ms: u64,
    offset_ms: u64,
    pcm_len: usize,
) -> Option<(usize, usize)> {
    let a = ((start_ms.saturating_sub(offset_ms)) as usize * 16).min(pcm_len);
    let b = ((end_ms.saturating_sub(offset_ms)) as usize * 16).min(pcm_len);
    if b <= a {
        None
    } else {
        Some((a, b))
    }
}

/// 按 source 分组、每次只惰性加载一轨全场 PCM,算完该轨全部段的嵌入即整轨 drop
/// (F3 修复:避免双轨全场 f32 同时常驻内存——3h 双轨会议可达 ~1.4GB+)。
/// 结果按 kept 的原始下标回填,输出顺序/长度与 kept 保持一一对应,行为不变。
fn embed_all(
    note_dir: &Path,
    kept: &[&SegmentRecord],
    embedder: &mut dyn SpeakerEmbedder,
) -> anyhow::Result<Vec<Option<Vec<f32>>>> {
    let audio_meta = crate::store::audio::load_audio_meta(note_dir);
    let mut out: Vec<Option<Vec<f32>>> = vec![None; kept.len()];

    let mut by_source: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, s) in kept.iter().enumerate() {
        by_source.entry(s.source.as_str()).or_default().push(i);
    }

    for (source, idxs) in by_source {
        // 该轨全是短段(< MIN_EMBED_MS)时无需读它的 PCM,省一次磁盘 I/O。
        if !idxs.iter().any(|&i| {
            let s = kept[i];
            s.end_ms.saturating_sub(s.start_ms) >= recluster::MIN_EMBED_MS
        }) {
            continue;
        }
        let offset_ms = audio_meta
            .tracks
            .get(source)
            .map(|t| t.offset_ms)
            .unwrap_or(0);
        let pcm = crate::store::transcode::track_pcm(note_dir, source)?;
        for &i in &idxs {
            let s = kept[i];
            let dur = s.end_ms.saturating_sub(s.start_ms);
            if dur < recluster::MIN_EMBED_MS {
                continue;
            }
            if let Some((a, b)) = slice_range(s.start_ms, s.end_ms, offset_ms, pcm.len()) {
                out[i] = embedder.embed(&pcm[a..b]).ok();
            }
        }
        // `pcm` 在此 for 迭代结束时被 drop:下一轮换轨才会再分配一份全场 PCM,
        // 任意时刻至多一轨全场 PCM 常驻内存。
    }
    Ok(out)
}

fn fallback_assign(inputs: &[recluster::SegInput]) -> Vec<recluster::Assignment> {
    inputs
        .iter()
        .map(|i| recluster::Assignment {
            seq: i.seq,
            speaker: i.old_speaker.clone().unwrap_or_else(|| "R1".into()),
            name: None,
            person: None,
        })
        .collect()
}

pub(crate) fn build_paragraphs(
    segs: &[SegmentRecord],
    discarded: &[u64],
    assign: &[recluster::Assignment],
    speakers: &BTreeMap<String, SpeakerMeta>,
) -> Vec<RefinedParagraph> {
    let by_seq: BTreeMap<u64, &recluster::Assignment> = assign.iter().map(|a| (a.seq, a)).collect();
    let mut out: Vec<RefinedParagraph> = Vec::new();
    for s in segs {
        if discarded.contains(&s.seq) {
            continue;
        }
        let Some(a) = by_seq.get(&s.seq) else {
            continue;
        };
        let old_meta = s.speaker.as_ref().and_then(|old| speakers.get(old));
        let name = a.name.clone().or_else(|| {
            old_meta
                .filter(|m| !m.name.is_empty())
                .map(|m| m.name.clone())
        });
        // 人物关联:重聚类种子命中优先;降级路径(沿用旧 S 标签)继承该说话人在
        // speakers.json 里已有的关联——与 name 同一套兜底逻辑。
        let person_id = a
            .person
            .clone()
            .or_else(|| old_meta.and_then(|m| m.person_id.clone()));
        let merge = out.last().map_or(false, |p: &RefinedParagraph| {
            p.speaker == a.speaker && s.end_ms.saturating_sub(p.start_ms) <= MAX_PARA_MS
        });
        if merge {
            let p = out.last_mut().unwrap();
            p.text.push_str(&s.text);
            p.end_ms = s.end_ms;
            p.source_seqs.push(s.seq);
        } else {
            out.push(RefinedParagraph {
                speaker: a.speaker.clone(),
                name,
                person_id,
                start_ms: s.start_ms,
                end_ms: s.end_ms,
                text: s.text.clone(),
                source_seqs: vec![s.seq],
                mentions: vec![],
            });
        }
    }
    out
}

/// F4 修复:落盘失败时把 `doc.stages.llm` 降级为 "failed",不留在 polish 算出的
/// "done"/"partial"——否则内存态(以及 lib.rs 随后 `emit("llm", &doc.stages.llm)`
/// 发出的事件)会报告"完成",但盘上 refined.json 因写失败仍是旧值(通常 "off"),
/// 前后端状态互相矛盾。落盘失败一律视为"本轮 Aing 没能生效"，与其它阶段"没能
/// 落成盘就不算完成"的语义看齐;错误照样返回给调用方记日志,不吞掉。
pub fn run_llm(
    note_dir: &Path,
    doc: &mut RefinedDoc,
    cfg: &llm::LlmConfig,
    llm_model: &str,
    log: Option<&crate::ailog::Ctx>,
) -> anyhow::Result<()> {
    let fail_in_memory = |doc: &mut RefinedDoc| {
        doc.stages.llm = "failed".into();
        doc.stages.entities = "failed".into();
        doc.stages.relations = "failed".into();
    };
    let aing_path = note_dir.join(crate::store::AING_DOC_FILE);
    // HTTP 必须以调用开始时盘上的整份人读真值为基线。原始 bytes 即 CAS revision：
    // 原子 rename 保证读到完整版本，提交时逐字节比较可捕获任意字段的并发改动。
    let original_revision = match std::fs::read(&aing_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // 兼容仅有 legacy refined.json 的旧笔记：先走受锁保护的一次性迁移。
            if crate::store::load_refined(note_dir).is_none() {
                fail_in_memory(doc);
                anyhow::bail!("HTTP Aing 前无法加载最新整份修订稿");
            }
            match std::fs::read(&aing_path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    fail_in_memory(doc);
                    return Err(error.into());
                }
            }
        }
        Err(error) => {
            fail_in_memory(doc);
            return Err(error.into());
        }
    };
    let mut latest: RefinedDoc = match serde_json::from_slice(&original_revision) {
        Ok(doc) => doc,
        Err(error) => {
            fail_in_memory(doc);
            return Err(error.into());
        }
    };
    let note_id = note_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("修订稿目录缺少有效笔记 id"))?;
    crate::store::ensure_graph_ids(note_id, &mut latest);
    *doc = latest;
    let fallback_graph = GraphFallbackSnapshot::capture(doc);

    let (text_outcome, raw_entities, raw_relations) = llm::polish(cfg, &mut doc.paragraphs, log);
    let relations_complete = text_outcome.relations_complete();
    let state = match &text_outcome {
        llm::LlmOutcome::Done | llm::LlmOutcome::DoneWithRelationErrors => "done",
        llm::LlmOutcome::Partial(_) => "partial",
        llm::LlmOutcome::Failed => "failed",
    };
    doc.stages.llm = state.into();
    doc.llm_model = Some(llm_model.to_string());
    // 实体维度:与文本同一批调用产出,stages.entities 跟随 state;实体环节绝不回退修订文本。
    fill_entities(doc, raw_entities, state);
    // materialize 会引用 mention id；必须先写回当前内存 doc，而不是只依赖 writer
    // 对 clone 的落盘修复，否则调用者内存态与 reload 后状态不一致。
    crate::store::ensure_graph_ids(note_id, doc);

    let note_lock = match crate::store::notelock::NoteLock::acquire(note_dir) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            fail_in_memory(doc);
            anyhow::bail!("笔记正被其它进程写入,无法提交 HTTP Aing");
        }
        Err(error) => {
            fail_in_memory(doc);
            return Err(error.into());
        }
    };
    let current_revision = match std::fs::read(&aing_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            fail_in_memory(doc);
            return Err(error.into());
        }
    };
    if current_revision != original_revision {
        if let Ok(mut concurrent) = serde_json::from_slice::<RefinedDoc>(&current_revision) {
            crate::store::ensure_graph_ids(note_id, &mut concurrent);
            *doc = concurrent;
        }
        anyhow::bail!("HTTP Aing 期间 aing.json 已变化，请基于最新版本重试");
    }

    // CAS 成功后在锁内完成 materialize + 整份提交。成功替换关系和 extraction；任意
    // parse/materialize/validator 失败只恢复同一原始整份基线里的旧图谱字段。
    if relations_complete {
        match relations::materialize(note_id, doc, raw_relations) {
            Ok(graph) => {
                let source_hash = crate::store::source_hash(&doc.paragraphs);
                let extraction = GraphExtraction {
                    contract_version: crate::store::aing_graph::GRAPH_CONTRACT_VERSION,
                    provider: "openai".into(),
                    model: llm_model.to_string(),
                    run_id: crate::store::stable_id(
                        "run_",
                        &[
                            note_id.to_string(),
                            llm_model.to_string(),
                            doc.generated_at.clone(),
                            source_hash.clone(),
                        ],
                    ),
                    generated_at: doc.generated_at.clone(),
                    source_hash,
                    mode: "http".into(),
                };
                relations::apply_validated_graph(doc, extraction, graph);
            }
            Err(issues) => {
                for issue in issues {
                    eprintln!("refine relations: {}: {}", issue.field, issue.message);
                }
                fallback_graph.clone().restore(note_id, doc);
                doc.stages.relations = "failed".into();
            }
        }
    } else {
        fallback_graph.restore(note_id, doc);
        doc.stages.relations = "failed".into();
    }
    if let Err(e) = crate::store::refined::write_refined_atomic_locked(note_dir, doc, &note_lock) {
        fail_in_memory(doc);
        return Err(e);
    }
    Ok(())
}

/// 把原始实体落进 doc:解析规范实体 + 逐段算 mention,`stages.entities` 置为 text_state
/// (与文本 outcome 同源——同一批调用产出)。抽成纯函数便于无网络单测,也隔离实体环节:
/// 无论实体多寡,都不触碰 doc.paragraphs[].text(修订文本已由 polish 定稿)。
pub(crate) fn fill_entities(doc: &mut RefinedDoc, raw: Vec<llm::RawEntity>, text_state: &str) {
    doc.stages.entities = text_state.into();
    doc.graph_support_mentions.clear();
    let entities = resolve_note_entities(raw);
    let mentions = compute_mentions(&doc.paragraphs, &entities);
    for (p, m) in doc.paragraphs.iter_mut().zip(mentions) {
        p.mentions = m;
    }
    doc.entities = entities;
}

/// 大模型原始实体 → 本篇规范实体:按规范名(trim + Unicode full default case fold)
/// 去重并合并别名,
/// 按首次出现顺序分配局部 id `ent_N`。首现的原名即规范名。**不做全局 person 归并**
/// (跨笔记/声纹匹配是 Plan 4 的解析层)。
pub(crate) fn entity_key(value: &str) -> String {
    value.trim().case_fold().collect()
}

pub(crate) fn resolve_note_entities(raw: Vec<llm::RawEntity>) -> Vec<Entity> {
    let mut out: Vec<Entity> = Vec::new();
    for r in raw {
        let name = r.name.trim();
        if name.is_empty() {
            continue;
        }
        let key = entity_key(name);
        if let Some(e) = out.iter_mut().find(|e| entity_key(&e.name) == key) {
            for a in r.aliases {
                let a = a.trim().to_string();
                if !a.is_empty()
                    && entity_key(&a) != key
                    && !e.aliases.iter().any(|x| entity_key(x) == entity_key(&a))
                {
                    e.aliases.push(a);
                }
            }
        } else {
            let id = format!("ent_{}", out.len() + 1);
            let mut aliases: Vec<String> = Vec::new();
            for a in r.aliases {
                let a = a.trim().to_string();
                if !a.is_empty()
                    && entity_key(&a) != key
                    && !aliases.iter().any(|x| entity_key(x) == entity_key(&a))
                {
                    aliases.push(a);
                }
            }
            out.push(Entity {
                id,
                kind: r.kind.trim().to_string(),
                name: name.to_string(),
                aliases,
            });
        }
    }
    out
}

/// 在 hay 中找 needle 的所有非重叠出现,返回 char 下标半开区间 [start,end)。
/// 空 needle 返回空。用于把实体名/别名映射到修订后正文的高亮区间。
fn find_char_spans(hay: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let needle_chars = needle.chars().count();
    let mut spans = Vec::new();
    let mut byte = 0usize;
    while let Some(pos) = hay[byte..].find(needle) {
        let abs = byte + pos;
        let char_start = hay[..abs].chars().count();
        spans.push((char_start, char_start + needle_chars));
        byte = abs + needle.len(); // 非重叠推进
    }
    spans
}

/// 对每段,在修订后 `text` 里按各实体 name+aliases 子串搜索,产出提及区间。
/// 单段内所有实体的命中合一起按 (start 升序, 长度降序) 贪心去重叠,保证高亮不交叠、
/// 长匹配优先(别名「灯塔」与全名「灯塔计划」重叠时留全名)。返回与 paragraphs 逐段对齐。
pub(crate) fn compute_mentions(
    paragraphs: &[RefinedParagraph],
    entities: &[Entity],
) -> Vec<Vec<Mention>> {
    paragraphs
        .iter()
        .map(|p| {
            // 收集 (start, end, entity_id)
            let mut hits: Vec<(usize, usize, &str)> = Vec::new();
            for e in entities {
                for needle in std::iter::once(&e.name).chain(e.aliases.iter()) {
                    for (s, en) in find_char_spans(&p.text, needle) {
                        hits.push((s, en, e.id.as_str()));
                    }
                }
            }
            // start 升序、长度降序;贪心保留不与已选重叠者
            hits.sort_by(|a, b| a.0.cmp(&b.0).then((b.1 - b.0).cmp(&(a.1 - a.0))));
            let mut chosen: Vec<Mention> = Vec::new();
            let mut last_end = 0usize;
            let mut first = true;
            for (s, en, id) in hits {
                if first || s >= last_end {
                    chosen.push(Mention {
                        id: String::new(),
                        entity: id.to_string(),
                        start: s,
                        end: en,
                    });
                    last_end = en;
                    first = false;
                }
            }
            chosen
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;
    use crate::store::{
        GraphExtraction, RelationEvidence, RelationFact, RelationPredicate, SegmentRecord,
        SpeakerMeta,
    };
    use std::collections::BTreeMap;
    use std::io::{Read, Write};

    fn para(text: &str) -> crate::store::RefinedParagraph {
        crate::store::RefinedParagraph {
            speaker: "R1".into(),
            name: None,
            person_id: None,
            start_ms: 0,
            end_ms: 1000,
            text: text.into(),
            source_seqs: vec![0],
            mentions: vec![],
        }
    }

    fn doc_with(texts: &[&str]) -> RefinedDoc {
        RefinedDoc {
            schema_version: crate::store::refined::REFINED_SCHEMA_VERSION,
            generated_at: "t".into(),
            llm_model: None,
            stages: RefineStages {
                filter: "done".into(),
                recluster: "done".into(),
                llm: "off".into(),
                entities: "off".into(),
                relations: "off".into(),
            },
            discarded_seqs: vec![],
            entities: vec![],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            paragraphs: texts.iter().map(|t| para(t)).collect(),
        }
    }

    fn valid_prior_doc_with_texts(note_id: &str, texts: &[&str]) -> RefinedDoc {
        assert!(texts
            .first()
            .is_some_and(|text| text.starts_with("张三负责灯塔计划")));
        let mut doc = doc_with(texts);
        doc.generated_at = "2026-07-20T09:00:00+08:00".into();
        doc.entities = vec![
            store::Entity {
                id: "ent_1".into(),
                kind: "person".into(),
                name: "张三".into(),
                aliases: vec![],
            },
            store::Entity {
                id: "ent_2".into(),
                kind: "project".into(),
                name: "灯塔计划".into(),
                aliases: vec![],
            },
        ];
        doc.paragraphs[0].mentions = vec![
            store::Mention {
                id: String::new(),
                entity: "ent_1".into(),
                start: 0,
                end: 2,
            },
            store::Mention {
                id: String::new(),
                entity: "ent_2".into(),
                start: 4,
                end: 8,
            },
        ];
        store::ensure_graph_ids(note_id, &mut doc);
        let relation = RelationFact {
            id: String::new(),
            subject: "ent_1".into(),
            predicate: RelationPredicate {
                kind: "responsible_for".into(),
                label: None,
            },
            object: "ent_2".into(),
            subject_mentions: vec![doc.paragraphs[0].mentions[0].id.clone()],
            object_mentions: vec![doc.paragraphs[0].mentions[1].id.clone()],
            confidence: 0.9,
            valid_from: None,
            valid_to: None,
            evidence: vec![RelationEvidence {
                id: String::new(),
                paragraph_index: 0,
                start: 0,
                end: 8,
                quote: "张三负责灯塔计划".into(),
                source_seqs: vec![0],
                source_hash: String::new(),
            }],
        };
        doc.relations = store::aing_graph::validate_graph(note_id, &doc, vec![relation])
            .expect("prior fixture 必须先通过 Task 2 validator")
            .relations;
        doc.graph_extraction = Some(GraphExtraction {
            contract_version: crate::store::aing_graph::GRAPH_CONTRACT_VERSION,
            provider: "old-provider".into(),
            model: "old-model".into(),
            run_id: "old-run".into(),
            generated_at: doc.generated_at.clone(),
            source_hash: store::source_hash(&doc.paragraphs),
            mode: "http".into(),
        });
        doc.stages.entities = "done".into();
        doc.stages.relations = "done".into();
        doc
    }

    fn valid_prior_doc(note_id: &str) -> RefinedDoc {
        valid_prior_doc_with_texts(note_id, &["张三负责灯塔计划"])
    }

    fn assert_fallback_dependencies(doc: &RefinedDoc) {
        assert_eq!(doc.relations.len(), 1);
        let relation = &doc.relations[0];
        let entities: BTreeMap<_, _> = doc
            .entities
            .iter()
            .map(|entity| (entity.id.as_str(), entity.name.as_str()))
            .collect();
        assert_eq!(entities.get(relation.subject.as_str()), Some(&"张三"));
        assert_eq!(entities.get(relation.object.as_str()), Some(&"灯塔计划"));
        let mention_owners: BTreeMap<_, _> = doc
            .paragraphs
            .iter()
            .flat_map(|paragraph| &paragraph.mentions)
            .map(|mention| (mention.id.as_str(), mention.entity.as_str()))
            .collect();
        for mention in &relation.subject_mentions {
            assert_eq!(
                mention_owners.get(mention.as_str()),
                Some(&relation.subject.as_str())
            );
        }
        for mention in &relation.object_mentions {
            assert_eq!(
                mention_owners.get(mention.as_str()),
                Some(&relation.object.as_str())
            );
        }
    }

    fn entity_id<'a>(doc: &'a RefinedDoc, name: &str) -> &'a str {
        doc.entities
            .iter()
            .find(|entity| entity.name == name)
            .map(|entity| entity.id.as_str())
            .unwrap_or_else(|| panic!("missing entity {name}"))
    }

    fn assert_fallback_commit(
        root: &std::path::Path,
        note_id: &str,
        doc: &RefinedDoc,
        baseline_graph: &[u8],
        new_entity_names: &[&str],
    ) {
        assert_eq!(graph_bytes(doc), baseline_graph);
        assert_fallback_dependencies(doc);
        assert_eq!(entity_id(doc, "张三"), "ent_1");
        assert_eq!(entity_id(doc, "灯塔计划"), "ent_2");
        let support: BTreeSet<_> = doc
            .graph_support_mentions
            .iter()
            .map(String::as_str)
            .collect();
        for name in new_entity_names {
            let id = entity_id(doc, name);
            assert!(id != "ent_1" && id != "ent_2", "{name} 不能重定向旧端点 id");
            assert!(doc
                .paragraphs
                .iter()
                .flat_map(|paragraph| &paragraph.mentions)
                .any(|mention| mention.entity == id && !support.contains(mention.id.as_str())));
        }
        let persisted = store::load_refined(&root.join("notes").join(note_id)).unwrap();
        assert_eq!(
            serde_json::to_value(doc).unwrap(),
            serde_json::to_value(&persisted).unwrap(),
            "HTTP 返回内存态必须与落盘完全一致"
        );
        assert_task4_stale_not_invalid(root, note_id, doc);
    }

    fn note_dir(root: &std::path::Path, note_id: &str) -> std::path::PathBuf {
        let dir = root.join("notes").join(note_id);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn assert_task4_stale_not_invalid(root: &std::path::Path, note_id: &str, source: &RefinedDoc) {
        let ledger = crate::graph::canonical::reconcile_registry(root).unwrap();
        let graph = crate::graph::canonical::build_canonical_graph(
            root,
            &ledger,
            chrono::DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();
        assert!(
            graph.relations.is_empty(),
            "旧证据已 stale，不应发布或重定向到新实体"
        );
        assert!(graph.pending.iter().any(|item| matches!(
            item,
            crate::graph::canonical::PendingItem::StaleEvidence { note_id: stale, .. }
                if stale == note_id
        )));
        assert!(!graph.pending.iter().any(|item| matches!(
            item,
            crate::graph::canonical::PendingItem::InvalidDocument { note_id: invalid, .. }
                if invalid == note_id
        )));
        let support: BTreeSet<_> = source
            .graph_support_mentions
            .iter()
            .map(String::as_str)
            .collect();
        let local_names: BTreeMap<_, _> = source
            .entities
            .iter()
            .map(|entity| (entity.id.as_str(), entity.name.as_str()))
            .collect();
        let mut expected_live = Vec::new();
        for (paragraph_index, paragraph) in source.paragraphs.iter().enumerate() {
            for mention in &paragraph.mentions {
                if !support.contains(mention.id.as_str()) {
                    expected_live.push((
                        paragraph_index,
                        mention.start,
                        mention.end,
                        local_names[mention.entity.as_str()].to_string(),
                    ));
                }
            }
        }
        expected_live.sort();
        let mut actual_live: Vec<_> = graph
            .mentions
            .iter()
            .filter(|mention| mention.note_id == note_id)
            .map(|mention| {
                (
                    mention.paragraph_index,
                    mention.start,
                    mention.end,
                    graph.entities[&mention.entity_id].name.clone(),
                )
            })
            .collect();
        actual_live.sort();
        assert_eq!(
            actual_live, expected_live,
            "support mention 不能泄漏为 live mention"
        );
        let stats = crate::graph::index::rebuild_atomic(root, &graph).unwrap();
        assert_eq!(
            stats.mentions,
            expected_live.len(),
            "support mention 不能进入 entity_mentions 索引表"
        );
    }

    fn graph_bytes(doc: &RefinedDoc) -> Vec<u8> {
        serde_json::to_vec(&(doc.graph_extraction.clone(), doc.relations.clone())).unwrap()
    }

    fn mock_server(body: String) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let count = stream.read(&mut chunk).unwrap_or(0);
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.trim()
                            .eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        format!("http://{addr}")
    }

    /// `Some(body)` 返回一个 200；`None` 在读完请求后直接断开，用来稳定制造
    /// 传输级失败。每项对应一个分块请求。
    fn sequence_server(responses: Vec<Option<String>>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for body in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let count = stream.read(&mut chunk).unwrap_or(0);
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..count]);
                    let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if request.len() >= header_end + 4 + content_length {
                        break;
                    }
                }
                if let Some(body) = body {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    stream.write_all(response.as_bytes()).unwrap();
                }
            }
        });
        format!("http://{addr}")
    }

    fn barrier_server(
        body: String,
    ) -> (
        String,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (requested_tx, requested_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let count = stream.read(&mut chunk).unwrap_or(0);
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.trim()
                            .eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            requested_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{addr}"), requested_rx, release_tx)
    }

    fn chat_response(content: serde_json::Value) -> String {
        serde_json::json!({
            "choices": [{"message": {"content": content.to_string()}}]
        })
        .to_string()
    }

    fn relation_content(quote: &str, include_object_entity: bool) -> serde_json::Value {
        let mut entities = vec![serde_json::json!({
            "name": "张三", "kind": "person", "aliases": []
        })];
        if include_object_entity {
            entities.push(serde_json::json!({
                "name": "灯塔计划", "kind": "project", "aliases": []
            }));
        }
        serde_json::json!({
            "glossary": {},
            "texts": ["🙂张三负责灯塔计划"],
            "entities": entities,
            "relations": [{
                "subject": "张三",
                "predicate": {"type": "responsible_for", "label": null},
                "object": "灯塔计划",
                "confidence": 0.92,
                "valid_from": null,
                "valid_to": null,
                "evidence": [{
                    "paragraph_index": 0,
                    "start": 1,
                    "end": 9,
                    "quote": quote
                }]
            }]
        })
    }

    fn changed_relation_content(quote: &str, include_object_entity: bool) -> serde_json::Value {
        let mut entities = vec![serde_json::json!({
            "name": "张三", "kind": "person", "aliases": ["老张"]
        })];
        if include_object_entity {
            entities.push(serde_json::json!({
                "name": "火星计划", "kind": "project", "aliases": []
            }));
        }
        serde_json::json!({
            "glossary": {},
            "texts": ["张三负责火星计划"],
            "entities": entities,
            "relations": [{
                "subject": "张三",
                "predicate": {"type": "responsible_for", "label": null},
                "object": "火星计划",
                "confidence": 0.92,
                "valid_from": null,
                "valid_to": null,
                "evidence": [{
                    "paragraph_index": 0,
                    "start": 0,
                    "end": 8,
                    "quote": quote
                }]
            }]
        })
    }

    fn changed_entities_content(
        text: &str,
        relations: Option<serde_json::Value>,
    ) -> serde_json::Value {
        let mut content = serde_json::json!({
            "glossary": {},
            "texts": [text],
            "entities": [
                {"name": "李四", "kind": "person", "aliases": []},
                {"name": "火星计划", "kind": "project", "aliases": []}
            ]
        });
        if let Some(relations) = relations {
            content["relations"] = relations;
        }
        content
    }

    #[test]
    fn resolve_dedups_by_name_case_insensitive_and_merges_aliases() {
        let raw = vec![
            llm::RawEntity {
                name: "灯塔计划".into(),
                kind: "project".into(),
                aliases: vec!["Lighthouse".into()],
            },
            llm::RawEntity {
                name: "灯塔计划".into(),
                kind: "project".into(),
                aliases: vec!["灯塔".into()],
            },
            llm::RawEntity {
                name: "Acme".into(),
                kind: "org".into(),
                aliases: vec![],
            },
            llm::RawEntity {
                name: "acme".into(),
                kind: "org".into(),
                aliases: vec!["ACME 公司".into()],
            },
        ];
        let ents = resolve_note_entities(raw);
        assert_eq!(ents.len(), 2, "灯塔计划 与 Acme 各归一为一个");
        assert_eq!(ents[0].id, "ent_1");
        assert_eq!(ents[0].name, "灯塔计划");
        // 合并别名(去重、顺序稳定)
        assert!(ents[0].aliases.contains(&"Lighthouse".to_string()));
        assert!(ents[0].aliases.contains(&"灯塔".to_string()));
        assert_eq!(ents[1].id, "ent_2");
        assert_eq!(ents[1].name, "Acme", "首现写法为规范名");
        assert!(ents[1].aliases.contains(&"ACME 公司".to_string()));
    }

    #[test]
    fn resolve_uses_full_unicode_default_case_folding() {
        let ents = resolve_note_entities(vec![
            llm::RawEntity {
                name: "Straße".into(),
                kind: "place".into(),
                aliases: vec!["Fußweg".into()],
            },
            llm::RawEntity {
                name: "STRASSE".into(),
                kind: "place".into(),
                aliases: vec!["FUSSWEG".into()],
            },
        ]);

        assert_eq!(ents.len(), 1, "ß/SS 必须按 Unicode full case folding 归一");
        assert_eq!(ents[0].name, "Straße");
        assert_eq!(ents[0].aliases, vec!["Fußweg"]);
    }

    #[test]
    fn compute_mentions_finds_name_and_alias_by_char_offset() {
        let ents = vec![store::Entity {
            id: "ent_1".into(),
            kind: "project".into(),
            name: "灯塔计划".into(),
            aliases: vec!["Lighthouse".into()],
        }];
        // 段落0:开头是「灯塔计划」(char 0..4);段落1:含别名 Lighthouse
        let ps = vec![
            para("灯塔计划下周启动"),
            para("我们叫它 Lighthouse 吧"), // "我们叫它 " 是 5 个 char(含空格),Lighthouse 从 char 5 起
        ];
        let ms = compute_mentions(&ps, &ents);
        assert_eq!(
            ms[0],
            vec![store::Mention {
                id: String::new(),
                entity: "ent_1".into(),
                start: 0,
                end: 4
            }]
        );
        assert_eq!(
            ms[1],
            vec![store::Mention {
                id: String::new(),
                entity: "ent_1".into(),
                start: 5,
                end: 15
            }]
        );
    }

    #[test]
    fn compute_mentions_non_overlapping_and_empty_when_absent() {
        let ents = vec![store::Entity {
            id: "ent_1".into(),
            kind: "term".into(),
            name: "AB".into(),
            aliases: vec![],
        }];
        let ps = vec![para("无关文本")];
        assert!(compute_mentions(&ps, &ents)[0].is_empty());
    }

    /// 假嵌入器:按调用顺序(=段 seq 序)依次返回预置方向向量。
    struct SeqEmbedder {
        dirs: Vec<[f32; 3]>,
        i: usize,
    }
    impl crate::diar::SpeakerEmbedder for SeqEmbedder {
        fn embed(&mut self, _s: &[f32]) -> anyhow::Result<Vec<f32>> {
            let d = self.dirs[self.i.min(self.dirs.len() - 1)];
            self.i += 1;
            Ok(vec![d[0], d[1], d[2]])
        }
    }

    fn seg(seq: u64, source: &str, text: &str, start: u64, end: u64, spk: &str) -> SegmentRecord {
        SegmentRecord {
            seq,
            source: source.into(),
            text: text.into(),
            start_ms: start,
            end_ms: end,
            speaker: Some(spk.into()),
            rms: Some(0.02),
        }
    }

    fn write_wav(dir: &std::path::Path, name: &str, secs: u32) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(dir.join(name), spec).unwrap();
        for _ in 0..(16000 * secs) {
            w.write_sample(2000i16).unwrap();
        }
        w.finalize().unwrap();
    }

    #[test]
    fn run_local_filters_reclusters_and_builds_paragraphs() {
        let dir = tempfile::tempdir().unwrap();
        write_wav(dir.path(), "mic.wav", 30);
        let segs = vec![
            seg(0, "mic", "大家好,今天讲三点。", 0, 5000, "S1"),
            seg(1, "mic", "噪声。", 5000, 5600, "S9"), // 应被过滤(合成占位:≤2 有效字短段)
            seg(2, "mic", "第一点是架构。", 6000, 11_000, "S2"), // 与 seq0 同人(嵌入同向)
            seg(3, "mic", "我有个问题。", 12_000, 20_000, "S3"), // 另一人(≥MIN_CLUSTER_MS,不被判碎片吞并)
        ];
        let a = [1.0, 0.0, 0.0];
        let b = [0.0, 1.0, 0.0];
        let mut e = SeqEmbedder {
            dirs: vec![a, a, b],
            i: 0,
        };
        let doc = run_local(
            dir.path(),
            &segs,
            &BTreeMap::new(),
            Some(&mut e),
            &[],
            "2026-07-06T15:00:00+08:00",
        )
        .unwrap();
        assert_eq!(doc.discarded_seqs, vec![1]);
        assert_eq!(doc.stages.filter, "done");
        assert_eq!(doc.stages.recluster, "done");
        assert_eq!(doc.stages.llm, "off");
        assert_eq!(doc.paragraphs.len(), 2, "seq0+seq2 并段,seq3 独立");
        assert_eq!(doc.paragraphs[0].source_seqs, vec![0, 2]);
        assert_ne!(doc.paragraphs[0].speaker, doc.paragraphs[1].speaker);
        assert!(
            crate::store::load_refined(dir.path()).is_some(),
            "run_local 已落盘"
        );
    }

    #[test]
    fn run_local_without_embedder_skips_recluster_keeps_old_labels() {
        let dir = tempfile::tempdir().unwrap();
        let mut speakers = BTreeMap::new();
        speakers.insert(
            "S1".into(),
            SpeakerMeta {
                name: "老板".into(),
                sources: vec!["mic".into()],
                centroid: None,
                count: 1,
                person_id: Some("P2".into()),
            },
        );
        let segs = vec![seg(0, "mic", "就这样定了。", 0, 4000, "S1")];
        let doc = run_local(dir.path(), &segs, &speakers, None, &[], "t").unwrap();
        assert_eq!(doc.stages.recluster, "skipped");
        assert_eq!(doc.paragraphs[0].speaker, "S1");
        assert_eq!(
            doc.paragraphs[0].name.as_deref(),
            Some("老板"),
            "旧标签沿用用户改名"
        );
        assert_eq!(
            doc.paragraphs[0].person_id.as_deref(),
            Some("P2"),
            "降级路径继承既有人物关联"
        );
    }

    #[test]
    fn paragraphs_split_at_max_duration() {
        let segs: Vec<SegmentRecord> = (0..5)
            .map(|i| seg(i, "mic", "内容。", i * 20_000, (i + 1) * 20_000, "S1"))
            .collect();
        let assign: Vec<_> = (0..5)
            .map(|i| recluster::Assignment {
                seq: i,
                speaker: "R1".into(),
                name: None,
                person: None,
            })
            .collect();
        let ps = build_paragraphs(&segs, &[], &assign, &BTreeMap::new());
        assert!(ps.len() >= 2, "100s 同人内容必须按 MAX_PARA_MS 切段");
        assert!(ps
            .iter()
            .all(|p| p.end_ms - p.start_ms <= MAX_PARA_MS + 20_000));
    }

    #[test]
    fn slice_range_covers_offset_bounds_and_inversion() {
        // offset=0:直接按 ms*16 换算,不做任何偏移。
        assert_eq!(
            slice_range(1000, 3000, 0, 1_000_000),
            Some((16_000, 48_000))
        );
        // offset=60_000(续录/中途授权轨道,从第 60s 才出现):时间轴 ms 须先减掉 offset
        // 才是文件内 ms,直接拿时间轴 ms 当文件内 ms 用是 F2 的 bug。
        assert_eq!(
            slice_range(61_000, 63_000, 60_000, 1_000_000),
            Some((16_000, 48_000))
        );
        // 越界:换算后终点超过 pcm 实际长度,clamp 到 pcm_len;clamp 后 start==end(=pcm_len)
        // 时说明这段完全落在文件外,必须 None。
        assert_eq!(slice_range(0, 1_000_000, 0, 1_000), Some((0, 1_000)));
        assert_eq!(
            slice_range(100_000, 200_000, 0, 1_000),
            None,
            "clamp 后 start==end→None"
        );
        // 倒置:end<=start(含 offset 换算后倒置)一律 None,绝不倒切片。
        assert_eq!(slice_range(3000, 1000, 0, 1_000_000), None);
    }

    #[test]
    fn embed_all_accounts_for_track_offset_ms() {
        // F2 回归:轨道文件的 0 点不总是笔记时间轴的 0 点——续录/中途授权的轨道
        // audio.json 记着非零 offset_ms。mic.wav 只覆盖轨道出现之后的 10s
        // (文件内 0ms == 时间轴 60_000ms),文件内第 1~3 秒写入区别于其余静音的
        // marker 采样值,供嵌入器捕获校验 embed_all 是否先减掉 offset 再切片。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("audio.json"),
            r#"{"schema_version":1,"tracks":{"mic":{"offset_ms":60000}}}"#,
        )
        .unwrap();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(dir.path().join("mic.wav"), spec).unwrap();
        for ms in 0..10_000u32 {
            let v: i16 = if (1000..3000).contains(&ms) { 5000 } else { 0 };
            for _ in 0..16 {
                w.write_sample(v).unwrap();
            }
        }
        w.finalize().unwrap();

        // 段时间轴 61_000..63_000ms == 文件内 1000..3000ms(marker 区间)。
        let s = seg(0, "mic", "第一点。", 61_000, 63_000, "S1");
        let kept: Vec<&SegmentRecord> = vec![&s];

        struct CaptureEmbedder(Vec<f32>);
        impl crate::diar::SpeakerEmbedder for CaptureEmbedder {
            fn embed(&mut self, s: &[f32]) -> anyhow::Result<Vec<f32>> {
                self.0.push(s.first().copied().unwrap_or(0.0));
                Ok(vec![0.0])
            }
        }
        let mut e = CaptureEmbedder(Vec::new());
        let out = embed_all(dir.path(), &kept, &mut e).unwrap();
        assert!(
            out[0].is_some(),
            "offset 换算后应落在轨道有效范围内,必须产出嵌入"
        );
        assert_eq!(
            e.0[0],
            5000.0 / 32768.0,
            "切片起点须是文件内 1000ms 处的 marker 采样,而非误用时间轴 61_000ms 直接当文件内 ms"
        );
    }

    #[test]
    fn run_llm_success_sets_stage_done_and_persists() {
        // 空段落 → llm::polish 内部 chunk_indices 为空,直接判 Done,不发起网络请求;
        // 验证正常路径下 stages.llm/llm_model 按预期置位且成功落盘。
        let dir = tempfile::tempdir().unwrap();
        let mut doc = RefinedDoc {
            schema_version: crate::store::refined::REFINED_SCHEMA_VERSION,
            generated_at: "t".into(),
            llm_model: None,
            stages: RefineStages {
                filter: "done".into(),
                recluster: "done".into(),
                llm: "off".into(),
                entities: "off".into(),
                relations: "off".into(),
            },
            discarded_seqs: vec![],
            entities: vec![],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            paragraphs: vec![],
        };
        let cfg = llm::LlmConfig {
            base_url: "http://127.0.0.1:1".into(),
            model: "m".into(),
            api_key: "k".into(),
        };
        store::write_refined_atomic(dir.path(), &doc).unwrap();
        run_llm(dir.path(), &mut doc, &cfg, "m", None).expect("空段落路径不应触网,写盘应成功");
        assert_eq!(doc.stages.llm, "done");
        assert_eq!(doc.llm_model.as_deref(), Some("m"));
        assert!(crate::store::load_refined(dir.path()).is_some(), "已落盘");
    }

    #[test]
    fn run_llm_write_failure_downgrades_stage_to_failed() {
        // F4 回归:落盘失败(此处用不存在的目录模拟——std::fs::write 必然 ENOENT)时,
        // stages.llm 不能停留在 polish 算出的 "done"/"partial"(此处为空段落→Done),
        // 必须降级为 "failed",否则内存态与随后的 emit 会跟磁盘上的旧值(通常 "off")
        // 互相矛盾。错误仍须原样返回给调用方。
        let base = tempfile::tempdir().unwrap();
        let missing_dir = base.path().join("does-not-exist");
        let mut doc = RefinedDoc {
            schema_version: crate::store::refined::REFINED_SCHEMA_VERSION,
            generated_at: "t".into(),
            llm_model: None,
            stages: RefineStages {
                filter: "done".into(),
                recluster: "done".into(),
                llm: "off".into(),
                entities: "off".into(),
                relations: "off".into(),
            },
            discarded_seqs: vec![],
            entities: vec![],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            paragraphs: vec![],
        };
        let cfg = llm::LlmConfig {
            base_url: "http://127.0.0.1:1".into(),
            model: "m".into(),
            api_key: "k".into(),
        };
        let err = run_llm(&missing_dir, &mut doc, &cfg, "m", None);
        assert!(err.is_err(), "目录不存在,写盘必须失败");
        assert_eq!(
            doc.stages.llm, "failed",
            "写盘失败必须把内存态降级为 failed"
        );
    }

    #[test]
    fn fill_entities_populates_entities_and_mentions() {
        let mut doc = doc_with(&["灯塔计划下周启动"]);
        let raw = vec![llm::RawEntity {
            name: "灯塔计划".into(),
            kind: "project".into(),
            aliases: vec![],
        }];
        fill_entities(&mut doc, raw, "done");
        assert_eq!(doc.stages.entities, "done");
        assert_eq!(doc.entities.len(), 1);
        assert_eq!(doc.entities[0].id, "ent_1");
        assert_eq!(
            doc.paragraphs[0].mentions,
            vec![store::Mention {
                id: String::new(),
                entity: "ent_1".into(),
                start: 0,
                end: 4
            }]
        );
    }

    #[test]
    fn fill_entities_empty_raw_sets_stage_but_no_entities() {
        let mut doc = doc_with(&["你好"]);
        fill_entities(&mut doc, vec![], "done");
        assert_eq!(doc.stages.entities, "done", "成功抽取但无实体也是 done");
        assert!(doc.entities.is_empty());
        assert!(doc.paragraphs[0].mentions.is_empty());
    }

    #[test]
    fn fill_entities_follows_text_state_on_failure() {
        let mut doc = doc_with(&["原文"]);
        fill_entities(&mut doc, vec![], "failed"); // 文本失败 → 实体也 failed、空
        assert_eq!(doc.stages.entities, "failed");
        assert!(doc.entities.is_empty());
    }

    #[test]
    fn run_llm_wires_entities_stage_no_network() {
        // 空段落 → polish 早退 Done 不触网(沿用现有 run_llm 测试同款零网络路径),
        // 验证 run_llm 确实调了 fill_entities:stages.entities 被置位(done)、entities 空。
        let dir = tempfile::tempdir().unwrap();
        let mut doc = doc_with(&[]);
        let cfg = llm::LlmConfig {
            base_url: "http://127.0.0.1:1".into(),
            model: "m".into(),
            api_key: "k".into(),
        };
        store::write_refined_atomic(dir.path(), &doc).unwrap();
        run_llm(dir.path(), &mut doc, &cfg, "m", None).unwrap();
        assert_eq!(doc.stages.llm, "done");
        assert_eq!(
            doc.stages.entities, "done",
            "run_llm 应经 fill_entities 置位 stages.entities"
        );
        let reloaded = crate::store::load_refined(dir.path()).unwrap();
        assert_eq!(reloaded.stages.entities, "done", "落盘也带上 entities 阶段");
    }

    #[test]
    fn run_llm_materializes_and_persists_http_relations_with_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let mut doc = doc_with(&["🙂张三负则灯塔计划"]);
        doc.paragraphs[0].source_seqs = vec![42, 41];
        store::write_refined_atomic(dir.path(), &doc).unwrap();
        let base = mock_server(chat_response(relation_content("张三负责灯塔计划", true)));
        let cfg = llm::LlmConfig {
            base_url: base,
            model: "model-v1".into(),
            api_key: "k".into(),
        };

        run_llm(dir.path(), &mut doc, &cfg, "model-v1", None).unwrap();

        assert_eq!(doc.paragraphs[0].text, "🙂张三负责灯塔计划");
        assert_eq!(doc.stages.llm, "done");
        assert_eq!(doc.stages.entities, "done");
        assert_eq!(doc.stages.relations, "done");
        assert_eq!(doc.relations.len(), 1);
        let relation = &doc.relations[0];
        assert!(relation.id.starts_with("rf_"));
        assert_eq!(relation.subject, "ent_1");
        assert_eq!(relation.object, "ent_2");
        assert_eq!(relation.evidence[0].quote, "张三负责灯塔计划");
        assert_eq!(relation.evidence[0].source_seqs, vec![41, 42]);
        assert!(relation.subject_mentions[0].starts_with("mn_"));
        assert!(relation.object_mentions[0].starts_with("mn_"));
        assert_eq!(
            doc.paragraphs[0].mentions[0].id, relation.subject_mentions[0],
            "materialize 前必须把 mention id 写回内存 doc"
        );
        assert_eq!(
            doc.paragraphs[0].mentions[1].id,
            relation.object_mentions[0]
        );
        let extraction = doc.graph_extraction.as_ref().unwrap();
        assert_eq!(
            extraction.contract_version,
            crate::store::aing_graph::GRAPH_CONTRACT_VERSION
        );
        assert_eq!(extraction.provider, "openai");
        assert_eq!(extraction.model, "model-v1");
        assert_eq!(extraction.mode, "http");
        assert_eq!(extraction.source_hash, store::source_hash(&doc.paragraphs));
        assert!(extraction.run_id.starts_with("run_"));

        let persisted = store::load_refined(dir.path()).unwrap();
        assert_eq!(graph_bytes(&persisted), graph_bytes(&doc));
        assert_eq!(persisted.stages.relations, "done");
        assert_eq!(
            serde_json::to_value(&persisted).unwrap(),
            serde_json::to_value(&doc).unwrap(),
            "成功返回的内存 doc 必须与 reload 后逐字段一致"
        );
    }

    #[test]
    fn run_llm_polishes_the_latest_full_disk_document() {
        let dir = tempfile::tempdir().unwrap();
        let mut disk = doc_with(&["盘上原稿"]);
        disk.paragraphs[0].speaker = "disk-speaker".into();
        disk.paragraphs[0].name = Some("盘上姓名".into());
        disk.paragraphs[0].person_id = Some("P9".into());
        disk.discarded_seqs = vec![7, 8];
        store::write_refined_atomic(dir.path(), &disk).unwrap();

        let mut stale_caller = doc_with(&["调用方旧稿"]);
        stale_caller.paragraphs[0].speaker = "stale-speaker".into();
        let content = serde_json::json!({
            "glossary": {}, "texts": ["盘上润色稿"], "entities": [], "relations": []
        });
        let cfg = llm::LlmConfig {
            base_url: mock_server(chat_response(content)),
            model: "model-latest".into(),
            api_key: "k".into(),
        };

        run_llm(dir.path(), &mut stale_caller, &cfg, "model-latest", None).unwrap();

        assert_eq!(stale_caller.paragraphs[0].text, "盘上润色稿");
        assert_eq!(stale_caller.paragraphs[0].speaker, "disk-speaker");
        assert_eq!(stale_caller.paragraphs[0].name.as_deref(), Some("盘上姓名"));
        assert_eq!(stale_caller.paragraphs[0].person_id.as_deref(), Some("P9"));
        assert_eq!(stale_caller.discarded_seqs, vec![7, 8]);
    }

    #[test]
    fn concurrent_writer_during_http_causes_whole_document_cas_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let base = doc_with(&["HTTP 前原稿"]);
        store::write_refined_atomic(dir.path(), &base).unwrap();
        let content = serde_json::json!({
            "glossary": {}, "texts": ["HTTP 候选稿"], "entities": [], "relations": []
        });
        let (base_url, requested, release) = barrier_server(chat_response(content));
        let cfg = llm::LlmConfig {
            base_url,
            model: "model-cas".into(),
            api_key: "k".into(),
        };
        let note_dir = dir.path().to_path_buf();
        let mut caller = doc_with(&["调用方旧稿"]);
        let worker = std::thread::spawn(move || {
            let result = run_llm(&note_dir, &mut caller, &cfg, "model-cas", None);
            (result, caller)
        });

        requested
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("HTTP 请求应到达屏障");
        let mut concurrent = base;
        concurrent.generated_at = "并发版本".into();
        concurrent.discarded_seqs = vec![99];
        concurrent.paragraphs[0].speaker = "concurrent-speaker".into();
        concurrent.paragraphs[0].text = "并发写者真值".into();
        store::write_refined_atomic(dir.path(), &concurrent).unwrap();
        let concurrent_bytes = std::fs::read(dir.path().join(store::AING_DOC_FILE)).unwrap();
        release.send(()).unwrap();

        let (result, returned) = worker.join().unwrap();
        let error = result.expect_err("revision 改变必须拒绝整个 HTTP 候选稿");
        assert!(
            error.to_string().contains("已变化"),
            "错误应明确提示 CAS 冲突: {error:#}"
        );
        assert_eq!(
            std::fs::read(dir.path().join(store::AING_DOC_FILE)).unwrap(),
            concurrent_bytes,
            "不能只合并图谱字段或覆盖并发写者的任意整文字段"
        );
        let latest = store::load_refined(dir.path()).unwrap();
        assert_eq!(
            serde_json::to_vec(&returned).unwrap(),
            serde_json::to_vec(&latest).unwrap(),
            "冲突后内存态也应回到最新整份盘上真值"
        );
    }

    #[test]
    fn invalid_quote_or_absent_entity_preserves_prior_graph_but_keeps_text_and_entities() {
        for (case, quote, include_object_entity) in [
            ("invalid-quote", "张三拥有火星计划", true),
            ("absent-entity", "张三负责火星计划", false),
        ] {
            let root = tempfile::tempdir().unwrap();
            let note_id = format!("fallback-{case}");
            let dir = note_dir(root.path(), &note_id);
            let mut doc = valid_prior_doc(&note_id);
            store::write_refined_atomic(&dir, &doc).unwrap();
            let baseline = store::load_refined(&dir).unwrap();
            let baseline_graph = graph_bytes(&baseline);
            doc = baseline;
            let base = mock_server(chat_response(changed_relation_content(
                quote,
                include_object_entity,
            )));
            let cfg = llm::LlmConfig {
                base_url: base,
                model: "model-v2".into(),
                api_key: "k".into(),
            };

            run_llm(&dir, &mut doc, &cfg, "model-v2", None).unwrap();

            assert_eq!(doc.paragraphs[0].text, "张三负责火星计划");
            assert_eq!(doc.stages.llm, "done");
            assert_eq!(doc.stages.entities, "done");
            assert_eq!(doc.stages.relations, "failed");
            assert_eq!(
                entity_id(&doc, "张三"),
                "ent_1",
                "同名同 kind 必须复用旧 id"
            );
            let new_entities: &[&str] = if include_object_entity {
                &["火星计划"]
            } else {
                &[]
            };
            assert_fallback_commit(root.path(), &note_id, &doc, &baseline_graph, new_entities);
        }
    }

    #[test]
    fn missing_or_malformed_relations_preserve_new_text_entities_and_coherent_prior_graph() {
        for (case, relations) in [
            ("missing", None),
            ("malformed", Some(serde_json::json!({}))),
        ] {
            let root = tempfile::tempdir().unwrap();
            let note_id = format!("fallback-{case}");
            let dir = note_dir(root.path(), &note_id);
            let mut doc = valid_prior_doc(&note_id);
            store::write_refined_atomic(&dir, &doc).unwrap();
            let baseline = store::load_refined(&dir).unwrap();
            let baseline_graph = graph_bytes(&baseline);
            doc = baseline;
            let response = changed_entities_content("李四负责火星计划", relations);
            let cfg = llm::LlmConfig {
                base_url: mock_server(chat_response(response)),
                model: "model-v2".into(),
                api_key: "k".into(),
            };

            run_llm(&dir, &mut doc, &cfg, "model-v2", None).unwrap();

            assert_eq!(doc.paragraphs[0].text, "李四负责火星计划");
            assert_eq!(doc.stages.llm, "done");
            assert_eq!(doc.stages.entities, "done");
            assert_eq!(doc.stages.relations, "failed");
            assert_fallback_commit(
                root.path(),
                &note_id,
                &doc,
                &baseline_graph,
                &["李四", "火星计划"],
            );
            let support: BTreeSet<_> = doc
                .graph_support_mentions
                .iter()
                .map(String::as_str)
                .collect();
            assert!(doc.relations[0]
                .subject_mentions
                .iter()
                .chain(&doc.relations[0].object_mentions)
                .all(|mention| support.contains(mention.as_str())));
        }
    }

    #[test]
    fn unchanged_text_fallback_reuses_fresh_live_mentions_without_invalid_document() {
        let root = tempfile::tempdir().unwrap();
        let note_id = "fallback-unchanged";
        let dir = note_dir(root.path(), note_id);
        let mut doc = valid_prior_doc(note_id);
        store::write_refined_atomic(&dir, &doc).unwrap();
        let missing = serde_json::json!({
            "glossary": {},
            "texts": ["张三负责灯塔计划"],
            "entities": [
                {"name": "张三", "kind": "person", "aliases": []},
                {"name": "灯塔计划", "kind": "project", "aliases": []}
            ]
        });
        let cfg = llm::LlmConfig {
            base_url: mock_server(chat_response(missing)),
            model: "model-v2".into(),
            api_key: "k".into(),
        };

        run_llm(&dir, &mut doc, &cfg, "model-v2", None).unwrap();

        assert_fallback_dependencies(&doc);
        assert!(doc.graph_support_mentions.is_empty());
        let ledger = crate::graph::canonical::reconcile_registry(root.path()).unwrap();
        let graph = crate::graph::canonical::build_canonical_graph(
            root.path(),
            &ledger,
            chrono::DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();
        assert!(!graph.pending.iter().any(|item| matches!(
            item,
            crate::graph::canonical::PendingItem::InvalidDocument { note_id: invalid, .. }
                if invalid == note_id
        )));
        let mut quotes: Vec<_> = graph
            .mentions
            .iter()
            .filter(|mention| mention.note_id == note_id)
            .map(|mention| mention.quote.as_str())
            .collect();
        quotes.sort();
        assert_eq!(quotes, vec!["张三", "灯塔计划"]);
    }

    #[test]
    fn fallback_support_mentions_promote_when_fresh_occurrences_return() {
        let root = tempfile::tempdir().unwrap();
        let note_id = "fallback-support-promotion";
        let dir = note_dir(root.path(), note_id);
        let mut doc = valid_prior_doc(note_id);
        let relation_mentions: BTreeSet<_> = doc.relations[0]
            .subject_mentions
            .iter()
            .chain(&doc.relations[0].object_mentions)
            .cloned()
            .collect();
        store::write_refined_atomic(&dir, &doc).unwrap();
        let changed = changed_entities_content("李四负责火星计划", None);
        let restored = serde_json::json!({
            "glossary": {},
            "texts": ["张三负责灯塔计划"],
            "entities": [
                {"name": "张三", "kind": "person", "aliases": []},
                {"name": "灯塔计划", "kind": "project", "aliases": []}
            ]
        });
        let cfg = llm::LlmConfig {
            base_url: sequence_server(vec![
                Some(chat_response(changed)),
                Some(chat_response(restored)),
            ]),
            model: "model-v2".into(),
            api_key: "k".into(),
        };

        run_llm(&dir, &mut doc, &cfg, "model-changed", None).unwrap();
        assert_eq!(
            doc.graph_support_mentions
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
            relation_mentions,
            "正文不再包含旧 occurrences 时，关系端点必须降级为 support"
        );

        run_llm(&dir, &mut doc, &cfg, "model-restored", None).unwrap();

        assert_fallback_dependencies(&doc);
        assert!(
            doc.graph_support_mentions.is_empty(),
            "相同实体、段落和区间重新成为 fresh occurrence 后必须提升回 live"
        );
        let all_mentions: Vec<_> = doc
            .paragraphs
            .iter()
            .enumerate()
            .flat_map(|(paragraph_index, paragraph)| {
                paragraph
                    .mentions
                    .iter()
                    .map(move |mention| (paragraph_index, mention))
            })
            .collect();
        assert_eq!(all_mentions.len(), 2);
        assert!(all_mentions.iter().all(|(paragraph_index, mention)| {
            *paragraph_index == 0
                && relation_mentions.contains(&mention.id)
                && ((mention.entity == "ent_1" && mention.start == 0 && mention.end == 2)
                    || (mention.entity == "ent_2" && mention.start == 4 && mention.end == 8))
        }));

        let ledger = crate::graph::canonical::reconcile_registry(root.path()).unwrap();
        let graph = crate::graph::canonical::build_canonical_graph(
            root.path(),
            &ledger,
            chrono::DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();
        let note_mentions: Vec<_> = graph
            .mentions
            .iter()
            .filter(|mention| mention.note_id == note_id)
            .collect();
        assert_eq!(note_mentions.len(), 2);
        assert_eq!(
            note_mentions
                .iter()
                .map(|mention| mention.id.clone())
                .collect::<BTreeSet<_>>(),
            relation_mentions,
            "两个旧端点应各提升为唯一 live canonical mention"
        );
        assert_eq!(graph.relations.len(), 1);
        let stats = crate::graph::index::rebuild_atomic(root.path(), &graph).unwrap();
        assert_eq!(stats.mentions, 2);
        assert_eq!(stats.relations, 1);
    }

    #[test]
    fn explicit_empty_relations_clear_fallback_graph_and_only_its_dependencies() {
        let root = tempfile::tempdir().unwrap();
        let note_id = "fallback-explicit-clear";
        let dir = note_dir(root.path(), note_id);
        let mut doc = valid_prior_doc(note_id);
        store::write_refined_atomic(&dir, &doc).unwrap();
        let missing = changed_entities_content("李四负责火星计划", None);
        let missing_again = changed_entities_content("李四负责火星计划二次", None);
        let explicit_empty = serde_json::json!({
            "glossary": {},
            "texts": ["王五启动金星计划"],
            "entities": [
                {"name": "王五", "kind": "person", "aliases": []},
                {"name": "金星计划", "kind": "project", "aliases": []}
            ],
            "relations": []
        });
        let base_url = sequence_server(vec![
            Some(chat_response(missing)),
            Some(chat_response(missing_again)),
            Some(chat_response(explicit_empty)),
        ]);
        let cfg = llm::LlmConfig {
            base_url,
            model: "model-v2".into(),
            api_key: "k".into(),
        };
        run_llm(&dir, &mut doc, &cfg, "model-v2", None).unwrap();
        assert_fallback_dependencies(&doc);
        let support_once = doc.graph_support_mentions.clone();
        assert!(!support_once.is_empty());

        run_llm(&dir, &mut doc, &cfg, "model-v2-retry", None).unwrap();
        assert_fallback_dependencies(&doc);
        assert_eq!(doc.graph_support_mentions, support_once);
        assert!(doc
            .graph_support_mentions
            .windows(2)
            .all(|ids| ids[0] < ids[1]));

        run_llm(&dir, &mut doc, &cfg, "model-v3", None).unwrap();

        assert_eq!(doc.paragraphs[0].text, "王五启动金星计划");
        assert_eq!(doc.stages.relations, "done");
        assert!(doc.relations.is_empty(), "显式 [] 是成功替换，不保留旧事实");
        assert_eq!(doc.graph_extraction.as_ref().unwrap().model, "model-v3");
        assert_eq!(entity_id(&doc, "王五"), "ent_1");
        assert_eq!(entity_id(&doc, "金星计划"), "ent_2");
        assert!(!doc
            .entities
            .iter()
            .any(|entity| entity.name == "张三" || entity.name == "灯塔计划"));
        assert!(doc.graph_support_mentions.is_empty());
        assert!(doc
            .paragraphs
            .iter()
            .flat_map(|paragraph| &paragraph.mentions)
            .all(|mention| doc
                .entities
                .iter()
                .any(|entity| entity.id == mention.entity)));
        assert_eq!(
            serde_json::to_value(&doc).unwrap(),
            serde_json::to_value(store::load_refined(&dir).unwrap()).unwrap()
        );
    }

    #[test]
    fn non_string_text_item_marks_relations_incomplete_and_preserves_prior_graph() {
        let dir = tempfile::tempdir().unwrap();
        let note_id = dir.path().file_name().unwrap().to_str().unwrap();
        let mut doc = valid_prior_doc(note_id);
        store::write_refined_atomic(dir.path(), &doc).unwrap();
        let baseline = store::load_refined(dir.path()).unwrap();
        let baseline_graph = graph_bytes(&baseline);
        doc = baseline;
        let bad = serde_json::json!({
            "glossary": {}, "texts": [null], "entities": [], "relations": []
        });
        let cfg = llm::LlmConfig {
            base_url: mock_server(chat_response(bad)),
            model: "model-strict".into(),
            api_key: "k".into(),
        };

        run_llm(dir.path(), &mut doc, &cfg, "model-strict", None).unwrap();

        assert_eq!(doc.paragraphs[0].text, "张三负责灯塔计划");
        assert_eq!(doc.stages.llm, "partial");
        assert_eq!(doc.stages.relations, "failed");
        assert_eq!(graph_bytes(&doc), baseline_graph);
        assert_fallback_dependencies(&doc);
        assert_eq!(
            serde_json::to_value(&doc).unwrap(),
            serde_json::to_value(store::load_refined(dir.path()).unwrap()).unwrap()
        );
    }

    #[test]
    fn multi_chunk_valid_relation_then_explicit_empty_keeps_the_valid_fact() {
        let dir = tempfile::tempdir().unwrap();
        let first_text = format!("张三负责灯塔计划{}", "甲".repeat(llm::CHUNK_CHARS));
        let second_text = format!("第二段{}", "乙".repeat(llm::CHUNK_CHARS));
        let mut doc = doc_with(&[&first_text, &second_text]);
        assert_eq!(llm::chunk_indices(&doc.paragraphs).len(), 2);
        store::write_refined_atomic(dir.path(), &doc).unwrap();
        let first = serde_json::json!({
            "glossary": {},
            "texts": [first_text],
            "entities": [
                {"name": "张三", "kind": "person", "aliases": []},
                {"name": "灯塔计划", "kind": "project", "aliases": []}
            ],
            "relations": [{
                "subject": "张三",
                "predicate": {"type": "responsible_for", "label": null},
                "object": "灯塔计划",
                "confidence": 0.9,
                "valid_from": null,
                "valid_to": null,
                "evidence": [{
                    "paragraph_index": 0, "start": 0, "end": 8,
                    "quote": "张三负责灯塔计划"
                }]
            }]
        });
        let second = serde_json::json!({
            "glossary": {}, "texts": [second_text], "entities": [], "relations": []
        });
        let cfg = llm::LlmConfig {
            base_url: sequence_server(vec![
                Some(chat_response(first)),
                Some(chat_response(second)),
            ]),
            model: "model-multi".into(),
            api_key: "k".into(),
        };

        run_llm(dir.path(), &mut doc, &cfg, "model-multi", None).unwrap();

        assert_eq!(doc.stages.relations, "done");
        assert_eq!(doc.relations.len(), 1);
        assert_eq!(doc.relations[0].evidence[0].paragraph_index, 0);
        assert_eq!(doc.relations[0].evidence[0].quote, "张三负责灯塔计划");
    }

    #[test]
    fn any_later_chunk_failure_preserves_the_entire_prior_graph() {
        for failure in [
            "network",
            "content",
            "missing-relations",
            "malformed-relations",
        ] {
            let root = tempfile::tempdir().unwrap();
            let note_id = format!("fallback-later-{failure}");
            let dir = note_dir(root.path(), &note_id);
            let first_prior = format!("张三负责灯塔计划{}", "甲".repeat(llm::CHUNK_CHARS));
            let second_prior = format!("旧第二段{}", "乙".repeat(llm::CHUNK_CHARS));
            let first_revised = format!("李四负责火星计划{}", "甲".repeat(llm::CHUNK_CHARS));
            let second_revised = format!("新第二段{}", "乙".repeat(llm::CHUNK_CHARS));
            let mut doc = valid_prior_doc_with_texts(&note_id, &[&first_prior, &second_prior]);
            assert_eq!(llm::chunk_indices(&doc.paragraphs).len(), 2);
            store::write_refined_atomic(&dir, &doc).unwrap();
            let baseline = store::load_refined(&dir).unwrap();
            let baseline_graph = graph_bytes(&baseline);
            doc = baseline;

            let first = serde_json::json!({
                "glossary": {},
                "texts": [first_revised],
                "entities": [
                    {"name": "李四", "kind": "person", "aliases": []},
                    {"name": "火星计划", "kind": "project", "aliases": []}
                ],
                "relations": [{
                    "subject": "李四",
                    "predicate": {"type": "responsible_for", "label": null},
                    "object": "火星计划",
                    "confidence": 0.9,
                    "valid_from": null,
                    "valid_to": null,
                    "evidence": [{
                        "paragraph_index": 0, "start": 0, "end": 8,
                        "quote": "李四负责火星计划"
                    }]
                }]
            });
            let second = match failure {
                "network" => None,
                "content" => Some(chat_response(serde_json::json!({
                    "glossary": {}, "texts": [null], "entities": [], "relations": []
                }))),
                "missing-relations" => Some(chat_response(serde_json::json!({
                    "glossary": {}, "texts": [second_revised], "entities": []
                }))),
                "malformed-relations" => Some(chat_response(serde_json::json!({
                    "glossary": {}, "texts": [second_revised], "entities": [], "relations": {}
                }))),
                _ => unreachable!(),
            };
            let cfg = llm::LlmConfig {
                base_url: sequence_server(vec![Some(chat_response(first)), second]),
                model: "model-multi-failure".into(),
                api_key: "k".into(),
            };

            run_llm(&dir, &mut doc, &cfg, "model-multi-failure", None).unwrap();

            assert_eq!(doc.stages.relations, "failed", "failure={failure}");
            assert_eq!(doc.paragraphs[0].text, first_revised, "failure={failure}");
            assert_fallback_commit(
                root.path(),
                &note_id,
                &doc,
                &baseline_graph,
                &["李四", "火星计划"],
            );
        }
    }

    #[test]
    fn run_local_carries_prior_graph_snapshot_before_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let dir = note_dir(root.path(), "local-fallback");
        let previous = valid_prior_doc("local-fallback");
        store::write_refined_atomic(&dir, &previous).unwrap();
        let baseline = store::load_refined(&dir).unwrap();
        let baseline_graph = graph_bytes(&baseline);
        let segments = vec![seg(7, "mic", "李四负责火星计划", 0, 4000, "S1")];

        let doc = run_local(&dir, &segments, &BTreeMap::new(), None, &[], "new-time").unwrap();

        assert_eq!(doc.paragraphs[0].text, "李四负责火星计划");
        assert_eq!(graph_bytes(&doc), baseline_graph);
        assert_fallback_dependencies(&doc);
        assert_eq!(
            serde_json::to_value(&doc).unwrap(),
            serde_json::to_value(store::load_refined(&dir).unwrap()).unwrap()
        );
        assert_eq!(
            graph_bytes(&store::load_refined(&dir).unwrap()),
            baseline_graph
        );
        assert_task4_stale_not_invalid(root.path(), "local-fallback", &doc);
    }

    #[test]
    fn fallback_snapshot_keeps_paragraph_indexes_safe_or_drops_unhostable_graph() {
        let prior = valid_prior_doc("paragraph-safety");
        let mut snapshot = GraphFallbackSnapshot::capture(&prior);
        for (paragraph_index, _) in &mut snapshot.mentions {
            *paragraph_index = usize::MAX;
        }
        let mut shortened = doc_with(&["李四负责火星计划"]);
        fill_entities(
            &mut shortened,
            vec![
                llm::RawEntity {
                    name: "李四".into(),
                    kind: "person".into(),
                    aliases: vec![],
                },
                llm::RawEntity {
                    name: "火星计划".into(),
                    kind: "project".into(),
                    aliases: vec![],
                },
            ],
            "done",
        );
        snapshot.restore("paragraph-safety", &mut shortened);
        assert_fallback_dependencies(&shortened);
        assert!(shortened.paragraphs[0]
            .mentions
            .iter()
            .any(|mention| mention.entity == "ent_1"));
        assert!(shortened.paragraphs[0]
            .mentions
            .iter()
            .any(|mention| mention.entity == "ent_2"));

        let mut empty = doc_with(&[]);
        GraphFallbackSnapshot::capture(&prior).restore("paragraph-safety", &mut empty);
        assert!(empty.graph_extraction.is_none());
        assert!(empty.relations.is_empty());
        assert!(empty.entities.is_empty());
    }

    #[test]
    fn note_lock_prevents_run_local_and_run_llm_from_overwriting_the_note() {
        let dir = tempfile::tempdir().unwrap();
        let previous = doc_with(&["锁内旧稿"]);
        store::write_refined_atomic(dir.path(), &previous).unwrap();
        let baseline = std::fs::read(dir.path().join(store::AING_DOC_FILE)).unwrap();
        let lock = crate::store::notelock::NoteLock::try_exclusive(dir.path())
            .unwrap()
            .unwrap();

        let segments = vec![seg(7, "mic", "不应写入。", 0, 4000, "S1")];
        let local_result = run_local(
            dir.path(),
            &segments,
            &BTreeMap::new(),
            None,
            &[],
            "new-time",
        );
        assert!(
            local_result.is_err(),
            "run_local 拿不到锁时必须显式报告未提交，调用方不得继续 HTTP"
        );
        assert_eq!(
            std::fs::read(dir.path().join(store::AING_DOC_FILE)).unwrap(),
            baseline,
            "run_local 必须服从 note lock"
        );

        let mut doc = store::load_refined(dir.path()).unwrap();
        let explicit_empty = serde_json::json!({
            "glossary": {}, "texts": ["也不应写入"], "entities": [], "relations": []
        });
        let cfg = llm::LlmConfig {
            base_url: mock_server(chat_response(explicit_empty)),
            model: "model-v3".into(),
            api_key: "k".into(),
        };
        let result = run_llm(dir.path(), &mut doc, &cfg, "model-v3", None);
        assert!(result.is_err(), "拿不到 note lock 必须显式报告未提交");
        assert_eq!(
            std::fs::read(dir.path().join(store::AING_DOC_FILE)).unwrap(),
            baseline,
            "run_llm 必须服从 note lock"
        );
        drop(lock);
    }

    fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            let to = dst.join(entry.file_name());
            if ty.is_dir() {
                copy_dir_all(&entry.path(), &to)?;
            } else {
                std::fs::copy(entry.path(), &to)?;
            }
        }
        Ok(())
    }

    /// golden 校准工具(长期保留,非一次性):对真实会议样本跑本地 Aing 管线,
    /// 产物落在临时工作目录下供 scripts/refine_golden.py 核验聚类/过滤指标。
    /// 源目录只读——先整份拷到 `$TMPDIR/vn-golden-work/<note_id>`,绝不碰用户原始数据。
    ///
    /// 用法(在 src-tauri/ 下):
    /// ```text
    /// VN_MODELS=<模型目录> VN_GOLDEN_NOTE=<会话目录> \
    ///   cargo test --lib refine::tests::golden_generate_refined -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn golden_generate_refined() {
        let src = std::env::var("VN_GOLDEN_NOTE").expect("设置 VN_GOLDEN_NOTE 为 golden 会话目录");
        let src = std::path::PathBuf::from(src);
        let note_id = src
            .file_name()
            .expect("VN_GOLDEN_NOTE 需指向具体会话目录")
            .to_string_lossy()
            .to_string();

        let work_root = std::env::temp_dir().join("vn-golden-work");
        let _ = std::fs::remove_dir_all(&work_root);
        let dst = work_root.join(&note_id);
        copy_dir_all(&src, &dst).expect("拷贝 golden 会话目录到临时工作目录失败");

        let note = crate::store::NoteStore::new(work_root.clone())
            .load(&note_id)
            .expect("加载拷贝目录的 segments/speakers 失败");

        let model_path =
            crate::models::root().join("3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx");
        let mut embedder = crate::diar::SherpaEmbedder::new(&model_path)
            .expect("加载声纹模型失败(设置 VN_MODELS 指向模型目录)");

        let doc = run_local(
            &dst,
            &note.segments,
            &note.speakers,
            Some(&mut embedder as &mut dyn crate::diar::SpeakerEmbedder),
            &[],
            "golden",
        )
        .unwrap();

        let labels: std::collections::BTreeSet<&str> =
            doc.paragraphs.iter().map(|p| p.speaker.as_str()).collect();
        println!("聚类标签数: {}", labels.len());
        println!("段落数: {}", doc.paragraphs.len());
        println!("discarded 数: {}", doc.discarded_seqs.len());
        println!("工作目录: {}", dst.display());

        assert!(
            crate::store::load_refined(&dst).is_some(),
            "refined.json 应已生成"
        );
    }
}
