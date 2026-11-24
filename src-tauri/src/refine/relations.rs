use crate::refine::llm::RawRelation;
use crate::store::aing_graph::{ValidatedGraph, ValidationIssue};
use crate::store::{self, GraphExtraction, RefinedDoc, RelationEvidence, RelationFact};

fn issue(
    issues: &mut Vec<ValidationIssue>,
    relation_index: usize,
    field: impl Into<String>,
    message: impl Into<String>,
) {
    issues.push(ValidationIssue {
        relation_index,
        field: field.into(),
        message: message.into(),
    });
}

fn relation_field(relation_index: usize, suffix: &str) -> String {
    format!("relations[{relation_index}].{suffix}")
}

fn resolve_entity<'a>(
    doc: &'a RefinedDoc,
    name: &str,
    relation_index: usize,
    endpoint: &str,
    issues: &mut Vec<ValidationIssue>,
) -> Option<&'a crate::store::Entity> {
    let key = super::entity_key(name);
    let matches = doc
        .entities
        .iter()
        .filter(|entity| {
            super::entity_key(&entity.name) == key
                || entity
                    .aliases
                    .iter()
                    .any(|alias| super::entity_key(alias) == key)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [entity] => Some(*entity),
        [] => {
            issue(
                issues,
                relation_index,
                relation_field(relation_index, endpoint),
                format!("{endpoint} entity is missing"),
            );
            None
        }
        _ => {
            issue(
                issues,
                relation_index,
                relation_field(relation_index, endpoint),
                format!("{endpoint} entity name or alias is ambiguous"),
            );
            None
        }
    }
}

fn overlaps(start: usize, end: usize, mention_start: usize, mention_end: usize) -> bool {
    mention_start < end && start < mention_end
}

/// 把模型使用的规范名/别名解析为本篇局部实体，再从 exact evidence span 中取得
/// 两端 mention。模型没有可提交的 ID；所有 ID、source seq/hash 都由本地重算，最后
/// 以 Task 2 validator 的归一结果作为唯一返回值。
pub fn materialize(
    note_id: &str,
    doc: &RefinedDoc,
    raw: Vec<RawRelation>,
) -> Result<ValidatedGraph, Vec<ValidationIssue>> {
    let mut issues = Vec::new();
    let mut relations = Vec::with_capacity(raw.len());

    for (relation_index, raw_relation) in raw.into_iter().enumerate() {
        let subject = resolve_entity(
            doc,
            &raw_relation.subject,
            relation_index,
            "subject",
            &mut issues,
        );
        let object = resolve_entity(
            doc,
            &raw_relation.object,
            relation_index,
            "object",
            &mut issues,
        );
        let (Some(subject), Some(object)) = (subject, object) else {
            continue;
        };

        let mut subject_mentions = Vec::new();
        let mut object_mentions = Vec::new();
        let mut evidence_rows = Vec::with_capacity(raw_relation.evidence.len());
        for (evidence_index, raw_evidence) in raw_relation.evidence.into_iter().enumerate() {
            let prefix = format!("evidence[{evidence_index}]");
            let Some(paragraph) = doc.paragraphs.get(raw_evidence.paragraph_index) else {
                issue(
                    &mut issues,
                    relation_index,
                    relation_field(relation_index, &format!("{prefix}.paragraph_index")),
                    "paragraph_index must reference an absolute document paragraph",
                );
                continue;
            };

            let chars = paragraph.text.chars().collect::<Vec<_>>();
            if raw_evidence.start >= raw_evidence.end || raw_evidence.end > chars.len() {
                issue(
                    &mut issues,
                    relation_index,
                    relation_field(relation_index, &format!("{prefix}.end")),
                    "evidence span must be a non-empty Unicode scalar range in the paragraph",
                );
                continue;
            }
            let exact_quote = chars[raw_evidence.start..raw_evidence.end]
                .iter()
                .collect::<String>();
            if exact_quote != raw_evidence.quote {
                issue(
                    &mut issues,
                    relation_index,
                    relation_field(relation_index, &format!("{prefix}.quote")),
                    "evidence quote must exactly match the Unicode scalar span",
                );
                continue;
            }

            let evidence_subject_mentions = paragraph
                .mentions
                .iter()
                .filter(|mention| {
                    mention.entity == subject.id
                        && overlaps(
                            raw_evidence.start,
                            raw_evidence.end,
                            mention.start,
                            mention.end,
                        )
                })
                .map(|mention| {
                    store::mention_id(
                        note_id,
                        paragraph,
                        &mention.entity,
                        mention.start,
                        mention.end,
                    )
                })
                .collect::<Vec<_>>();
            let evidence_object_mentions = paragraph
                .mentions
                .iter()
                .filter(|mention| {
                    mention.entity == object.id
                        && overlaps(
                            raw_evidence.start,
                            raw_evidence.end,
                            mention.start,
                            mention.end,
                        )
                })
                .map(|mention| {
                    store::mention_id(
                        note_id,
                        paragraph,
                        &mention.entity,
                        mention.start,
                        mention.end,
                    )
                })
                .collect::<Vec<_>>();
            if evidence_subject_mentions.is_empty() {
                issue(
                    &mut issues,
                    relation_index,
                    relation_field(relation_index, &format!("{prefix}.subject_mentions")),
                    "evidence span must overlap a mention owned by the subject entity",
                );
            }
            if evidence_object_mentions.is_empty() {
                issue(
                    &mut issues,
                    relation_index,
                    relation_field(relation_index, &format!("{prefix}.object_mentions")),
                    "evidence span must overlap a mention owned by the object entity",
                );
            }
            subject_mentions.extend(evidence_subject_mentions);
            object_mentions.extend(evidence_object_mentions);
            evidence_rows.push(RelationEvidence {
                id: String::new(),
                paragraph_index: raw_evidence.paragraph_index,
                start: raw_evidence.start,
                end: raw_evidence.end,
                quote: raw_evidence.quote,
                source_seqs: paragraph.source_seqs.clone(),
                source_hash: String::new(),
            });
        }

        relations.push(RelationFact {
            id: String::new(),
            subject: subject.id.clone(),
            predicate: raw_relation.predicate,
            object: object.id.clone(),
            subject_mentions,
            object_mentions,
            confidence: raw_relation.confidence,
            valid_from: raw_relation.valid_from,
            valid_to: raw_relation.valid_to,
            evidence: evidence_rows,
        });
    }

    if !issues.is_empty() {
        return Err(issues);
    }
    store::aing_graph::validate_graph(note_id, doc, relations)
}

/// 宽松物化的结果:入库的图谱 + 一路上丢掉了什么。
pub struct LenientGraph {
    pub graph: ValidatedGraph,
    /// 被丢弃的关系/证据及原因(`relation_index` 是模型原始输出里的下标)。
    pub issues: Vec<ValidationIssue>,
    /// 模型一共提了几条关系。`proposed > 0` 而 `graph.relations` 为空 = 全军覆没。
    pub proposed: usize,
}

/// 实体 `entity_id` 在 `paragraph` 里的 mention id。`span` 为 Some 时只取与该区间
/// 相交的;None 取整段的。
fn mention_ids(
    note_id: &str,
    paragraph: &crate::store::RefinedParagraph,
    entity_id: &str,
    span: Option<(usize, usize)>,
) -> Vec<String> {
    paragraph
        .mentions
        .iter()
        .filter(|mention| mention.entity == entity_id)
        .filter(|mention| span.is_none_or(|(start, end)| overlaps(start, end, mention.start, mention.end)))
        .map(|mention| store::mention_id(note_id, paragraph, &mention.entity, mention.start, mention.end))
        .collect()
}

/// HTTP 路径的宽松物化。与 [`materialize`] 的三点不同(2026-11-24,#849):
///
/// 1. **引文位置由程序定**。模型报的 `start/end` 只当提示,对不上就按 `quote` 在段里
///    查找(见 [`super::quote`]),找到的区间与原文切片回填。盘上 338 篇笔记的关系
///    此前为 0 条,绝大多数死在「下标对不上」。
/// 2. **坏一条丢一条**。任何一条关系不合格只丢它自己,其余照常入库;旧行为是一条
///    出错整篇作废。
/// 3. **mention 就近兜底**。引文区间没盖住主语/宾语的 mention 时(引文里用了代词
///    或没把人名抄全),退到**同一段**里该实体的 mention。实体归属仍由模型给的
///    主宾语名解析,引文仍逐字存在,只是不再要求两端都落在引文区间之内。底线不放:
///    至少一条证据所在的段落要同时提到主语和宾语;两端都没提到的段落不留作证据。
///
/// Agent / MCP 写回路径仍走严格的 [`materialize`]:那边的调用方拿得到最终段落,
/// 而且提交被拒后可以改了重交。
pub fn materialize_lenient(note_id: &str, doc: &RefinedDoc, raw: Vec<RawRelation>) -> LenientGraph {
    let proposed = raw.len();
    let mut issues = Vec::new();
    // (模型原始下标, 候选关系)
    let mut candidates: Vec<(usize, RelationFact)> = Vec::new();

    for (relation_index, raw_relation) in raw.into_iter().enumerate() {
        let subject = resolve_entity(doc, &raw_relation.subject, relation_index, "subject", &mut issues);
        let object = resolve_entity(doc, &raw_relation.object, relation_index, "object", &mut issues);
        let (Some(subject), Some(object)) = (subject, object) else {
            continue;
        };

        let mut subject_mentions = Vec::new();
        let mut object_mentions = Vec::new();
        let mut evidence_rows: Vec<RelationEvidence> = Vec::new();
        // 是否至少有一条证据所在的段落同时提到了主语和宾语。
        let mut co_mentioned = false;
        let mut located_any = false;
        for (evidence_index, raw_evidence) in raw_relation.evidence.into_iter().enumerate() {
            let located = super::quote::locate_in_paragraphs(
                doc.paragraphs.iter().map(|paragraph| paragraph.text.as_str()),
                raw_evidence.paragraph_index,
                &raw_evidence.quote,
                Some((raw_evidence.start, raw_evidence.end)),
            );
            let Some((paragraph_index, start, end)) = located else {
                issue(
                    &mut issues,
                    relation_index,
                    relation_field(relation_index, &format!("evidence[{evidence_index}].quote")),
                    "evidence quote was not found in the document",
                );
                continue;
            };
            located_any = true;
            let paragraph = &doc.paragraphs[paragraph_index];
            // 每端先取引文区间内的 mention;区间没盖住就退到**同一段**里的。
            let endpoint_mentions = |entity_id: &str| {
                let in_span = mention_ids(note_id, paragraph, entity_id, Some((start, end)));
                if in_span.is_empty() {
                    mention_ids(note_id, paragraph, entity_id, None)
                } else {
                    in_span
                }
            };
            let row_subject = endpoint_mentions(&subject.id);
            let row_object = endpoint_mentions(&object.id);
            // 这段两端都没提到:这条引文跟这条关系对不上,不留作证据。
            if row_subject.is_empty() && row_object.is_empty() {
                issue(
                    &mut issues,
                    relation_index,
                    relation_field(relation_index, &format!("evidence[{evidence_index}]")),
                    "evidence paragraph mentions neither the subject nor the object",
                );
                continue;
            }
            co_mentioned |= !row_subject.is_empty() && !row_object.is_empty();
            subject_mentions.extend(row_subject);
            object_mentions.extend(row_object);
            evidence_rows.push(RelationEvidence {
                id: String::new(),
                paragraph_index,
                start,
                end,
                quote: super::quote::slice(&paragraph.text, start, end),
                source_seqs: paragraph.source_seqs.clone(),
                source_hash: String::new(),
            });
        }
        if !located_any {
            issue(
                &mut issues,
                relation_index,
                relation_field(relation_index, "evidence"),
                "no evidence could be located in the document",
            );
            continue;
        }
        // 主语和宾语必须在**同一条证据的同一段**里共同出现过至少一次。只靠"证据 A 的
        // 段落提到主语、证据 B 的段落提到宾语"拼不出一条关系。
        if !co_mentioned {
            let endpoint = if subject_mentions.is_empty() { "subject_mentions" } else { "object_mentions" };
            issue(
                &mut issues,
                relation_index,
                relation_field(relation_index, endpoint),
                "no single evidence paragraph mentions both the subject and the object",
            );
            continue;
        }
        subject_mentions.sort();
        subject_mentions.dedup();
        object_mentions.sort();
        object_mentions.dedup();
        candidates.push((
            relation_index,
            RelationFact {
                id: String::new(),
                subject: subject.id.clone(),
                predicate: raw_relation.predicate,
                object: object.id.clone(),
                subject_mentions,
                object_mentions,
                confidence: raw_relation.confidence,
                valid_from: raw_relation.valid_from,
                valid_to: raw_relation.valid_to,
                evidence: evidence_rows,
            },
        ));
    }

    // 交给统一 validator。它按候选列表里的位置报错:剔掉被点名的再验,直到通过。
    // 每轮候选数严格减少,必然终止;空集合恒合法。
    loop {
        let facts = candidates.iter().map(|(_, fact)| fact.clone()).collect();
        match store::aing_graph::validate_graph(note_id, doc, facts) {
            Ok(graph) => return LenientGraph { graph, issues, proposed },
            Err(found) => {
                let bad: std::collections::BTreeSet<usize> =
                    found.iter().map(|found| found.relation_index).collect();
                for mut found in found {
                    if let Some((original, _)) = candidates.get(found.relation_index) {
                        found.relation_index = *original;
                    }
                    issues.push(found);
                }
                let before = candidates.len();
                let mut position = 0usize;
                candidates.retain(|_| {
                    let keep = !bad.contains(&position);
                    position += 1;
                    keep
                });
                if candidates.len() == before {
                    // validator 报了错却没点到任何候选(不该发生):全丢,保证终止。
                    candidates.clear();
                }
            }
        }
    }
}

pub fn apply_validated_graph(
    doc: &mut RefinedDoc,
    extraction: GraphExtraction,
    graph: ValidatedGraph,
) {
    doc.relations = graph.relations;
    doc.graph_extraction = Some(extraction);
    doc.graph_support_mentions.clear();
    doc.stages.relations = "done".into();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refine::llm::{RawEvidence, RawRelation};
    use crate::store::{
        mention_id, Entity, Mention, RefineStages, RefinedDoc, RefinedParagraph, RelationPredicate,
    };

    fn entity(id: &str, name: &str, aliases: &[&str]) -> Entity {
        Entity {
            id: id.into(),
            kind: if id == "ent_1" { "person" } else { "project" }.into(),
            name: name.into(),
            aliases: aliases.iter().map(|alias| (*alias).into()).collect(),
        }
    }

    fn doc(text: &str, entities: Vec<Entity>, mentions: Vec<Mention>) -> RefinedDoc {
        RefinedDoc {
            llm_failed_paragraphs: Vec::new(),
            schema_version: crate::store::refined::REFINED_SCHEMA_VERSION,
            generated_at: "2026-07-21T09:00:00+08:00".into(),
            written_at: String::new(),
            writer_pid: 0,
            finished_at: String::new(),
            writer_run: String::new(),
            llm_model: Some("model-v1".into()),
            stages: RefineStages {
                filter: "done".into(),
                recluster: "done".into(),
                llm: "done".into(),
                entities: "done".into(),
                relations: "off".into(),
            },
            discarded_seqs: vec![],
            entities,
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            revision: 0,
            stale: false,
            paragraphs: vec![RefinedParagraph {
                speaker: "R1".into(),
                name: None,
                person_id: None,
                start_ms: 0,
                end_ms: 1000,
                text: text.into(),
                source_seqs: vec![9, 7, 9],
                mentions,
            }],
        }
    }

    fn raw(subject: &str, object: &str, start: usize, end: usize, quote: &str) -> RawRelation {
        RawRelation {
            subject: subject.into(),
            predicate: RelationPredicate {
                kind: "responsible_for".into(),
                label: None,
            },
            object: object.into(),
            confidence: 0.92,
            valid_from: None,
            valid_to: None,
            evidence: vec![RawEvidence {
                paragraph_index: 0,
                start,
                end,
                quote: quote.into(),
            }],
        }
    }

    fn two_entity_doc(text: &str, subject: (usize, usize), object: (usize, usize)) -> RefinedDoc {
        doc(
            text,
            vec![entity("ent_1", "张三", &[]), entity("ent_2", "灯塔计划", &[])],
            vec![
                Mention { id: String::new(), entity: "ent_1".into(), start: subject.0, end: subject.1 },
                Mention { id: String::new(), entity: "ent_2".into(), start: object.0, end: object.1 },
            ],
        )
    }

    /// #849 的核心回归:模型抄对了引文、数错了下标——以前整篇作废,现在照常入库。
    #[test]
    fn lenient_recovers_relations_whose_offsets_are_wrong() {
        let doc = two_entity_doc("今天定了,张三负责灯塔计划。", (5, 7), (9, 13));
        // 严格路径仍然拒绝(Agent/MCP 写回的契约不变)。
        assert!(materialize("note-1", &doc, vec![raw("张三", "灯塔计划", 0, 8, "张三负责灯塔计划")]).is_err());

        for (start, end) in [(0, 8), (0, 0), (40, 90)] {
            let lenient = materialize_lenient(
                "note-1",
                &doc,
                vec![raw("张三", "灯塔计划", start, end, "张三负责灯塔计划")],
            );
            assert_eq!(lenient.proposed, 1);
            assert_eq!(lenient.graph.relations.len(), 1, "下标 ({start},{end}) 应被纠正: {:?}", lenient.issues);
            let evidence = &lenient.graph.relations[0].evidence[0];
            assert_eq!((evidence.start, evidence.end), (5, 13));
            assert_eq!(evidence.quote, "张三负责灯塔计划");
        }
    }

    #[test]
    fn lenient_keeps_good_relations_when_others_are_bad() {
        let doc = two_entity_doc("张三负责灯塔计划。", (0, 2), (4, 8));
        let lenient = materialize_lenient(
            "note-1",
            &doc,
            vec![
                raw("张三", "灯塔计划", 0, 0, "张三负责灯塔计划"),
                raw("李四", "灯塔计划", 0, 0, "张三负责灯塔计划"), // 主语不是本篇实体
                raw("张三", "灯塔计划", 0, 0, "张三已经放弃灯塔计划"), // 引文是编的
                raw("张三", "张三", 0, 0, "张三负责灯塔计划"),     // 自环,validator 拒
            ],
        );
        assert_eq!(lenient.proposed, 4);
        assert_eq!(lenient.graph.relations.len(), 1, "{:?}", lenient.issues);
        let flagged: std::collections::BTreeSet<usize> =
            lenient.issues.iter().map(|issue| issue.relation_index).collect();
        assert_eq!(flagged, [1, 2, 3].into_iter().collect(), "问题记在模型原始下标上");
    }

    #[test]
    fn lenient_reports_total_loss_as_empty_with_proposals() {
        let doc = two_entity_doc("张三负责灯塔计划。", (0, 2), (4, 8));
        let lenient = materialize_lenient(
            "note-1",
            &doc,
            vec![raw("张三", "灯塔计划", 0, 0, "完全没有这句话的引文")],
        );
        assert_eq!((lenient.proposed, lenient.graph.relations.len()), (1, 0));
        assert!(!lenient.issues.is_empty());
        // 模型说没有关系:空集合、零问题。
        let empty = materialize_lenient("note-1", &doc, vec![]);
        assert_eq!((empty.proposed, empty.graph.relations.len(), empty.issues.len()), (0, 0, 0));
    }

    /// 引文没把宾语名字抄进来(只抄了「张三负责」):退到同段里该实体的 mention。
    #[test]
    fn lenient_falls_back_to_same_paragraph_mentions() {
        let doc = two_entity_doc("张三负责灯塔计划。", (0, 2), (4, 8));
        let lenient =
            materialize_lenient("note-1", &doc, vec![raw("张三", "灯塔计划", 0, 0, "张三负责")]);
        assert_eq!(lenient.graph.relations.len(), 1, "{:?}", lenient.issues);
        let fact = &lenient.graph.relations[0];
        assert_eq!(fact.object_mentions, vec![mention_id("note-1", &doc.paragraphs[0], "ent_2", 4, 8)]);
        assert_eq!(fact.evidence[0].quote, "张三负责");
    }

    /// 同段里根本没提到宾语:没有落点,丢这一条。
    #[test]
    fn lenient_drops_relations_with_no_mention_anywhere_near_the_evidence() {
        let doc = doc(
            "张三负责这件事。",
            vec![entity("ent_1", "张三", &[]), entity("ent_2", "灯塔计划", &[])],
            vec![Mention { id: String::new(), entity: "ent_1".into(), start: 0, end: 2 }],
        );
        let lenient =
            materialize_lenient("note-1", &doc, vec![raw("张三", "灯塔计划", 0, 0, "张三负责这件事")]);
        assert_eq!(lenient.graph.relations.len(), 0);
        assert!(lenient.issues.iter().any(|issue| issue.field.ends_with("object_mentions")));
    }

    /// 审查发现:不能拿"证据 A 的段落提到主语、证据 B 的段落提到宾语"拼出一条关系。
    #[test]
    fn lenient_requires_one_paragraph_mentioning_both_endpoints() {
        let mut split = two_entity_doc("张三今天请假了。", (0, 2), (0, 0));
        split.paragraphs[0].mentions.truncate(1); // 段 0 只提到张三
        let mut second = split.paragraphs[0].clone();
        second.text = "灯塔计划下周上线。".into();
        second.source_seqs = vec![11];
        second.mentions = vec![Mention { id: String::new(), entity: "ent_2".into(), start: 0, end: 4 }];
        split.paragraphs.push(second);
        let mut relation = raw("张三", "灯塔计划", 0, 0, "张三今天请假了");
        relation.evidence.push(RawEvidence {
            paragraph_index: 1,
            start: 0,
            end: 0,
            quote: "灯塔计划下周上线".into(),
        });
        let lenient = materialize_lenient("note-1", &split, vec![relation]);
        assert_eq!(lenient.graph.relations.len(), 0, "两段各提一端,不构成关系");
        assert!(lenient
            .issues
            .iter()
            .any(|issue| issue.message.contains("mentions both the subject and the object")));
    }

    #[test]
    fn materializes_unicode_evidence_mentions_and_consumes_validator_normalization() {
        let mut doc = doc(
            "🙂张三负责灯塔计划",
            vec![
                entity("ent_1", "张三", &[]),
                entity("ent_2", "灯塔计划", &[]),
            ],
            vec![
                Mention {
                    id: String::new(),
                    entity: "ent_1".into(),
                    start: 1,
                    end: 3,
                },
                Mention {
                    id: String::new(),
                    entity: "ent_2".into(),
                    start: 5,
                    end: 9,
                },
            ],
        );
        let expected_subject = mention_id("note-1", &doc.paragraphs[0], "ent_1", 1, 3);
        let expected_object = mention_id("note-1", &doc.paragraphs[0], "ent_2", 5, 9);

        let graph = materialize(
            "note-1",
            &doc,
            vec![raw("张三", "灯塔计划", 1, 9, "张三负责灯塔计划")],
        )
        .unwrap();

        assert_eq!(graph.relations.len(), 1);
        let fact = &graph.relations[0];
        assert!(fact.id.starts_with("rf_"));
        assert_eq!(fact.subject, "ent_1");
        assert_eq!(fact.object, "ent_2");
        assert_eq!(fact.subject_mentions, vec![expected_subject]);
        assert_eq!(fact.object_mentions, vec![expected_object]);
        assert_eq!(fact.evidence[0].quote, "张三负责灯塔计划");
        assert_eq!((fact.evidence[0].start, fact.evidence[0].end), (1, 9));
        assert_eq!(
            fact.evidence[0].source_seqs,
            vec![7, 9],
            "必须消费 validator 的排序去重结果"
        );
        assert!(fact.evidence[0].source_hash.len() > 20);

        let extraction = crate::store::GraphExtraction {
            contract_version: crate::store::aing_graph::GRAPH_CONTRACT_VERSION,
            provider: "openai".into(),
            model: "model-v1".into(),
            run_id: "run-1".into(),
            generated_at: doc.generated_at.clone(),
            source_hash: crate::store::source_hash(&doc.paragraphs),
            mode: "http".into(),
        };
        doc.graph_support_mentions = vec!["mn_old_support".into()];
        apply_validated_graph(&mut doc, extraction.clone(), graph);
        assert_eq!(doc.stages.relations, "done");
        assert_eq!(doc.graph_extraction, Some(extraction));
        assert_eq!(doc.relations.len(), 1);
        assert!(doc.graph_support_mentions.is_empty());
    }

    #[test]
    fn resolves_case_folded_canonical_names_and_aliases() {
        let doc = doc(
            "ACME 负责 Lighthouse",
            vec![
                entity("ent_1", "Acme", &["ACME"]),
                entity("ent_2", "灯塔计划", &["Lighthouse"]),
            ],
            vec![
                Mention {
                    id: String::new(),
                    entity: "ent_1".into(),
                    start: 0,
                    end: 4,
                },
                Mention {
                    id: String::new(),
                    entity: "ent_2".into(),
                    start: 8,
                    end: 18,
                },
            ],
        );

        let graph = materialize(
            "note-1",
            &doc,
            vec![raw("acme", "lighthouse", 0, 18, "ACME 负责 Lighthouse")],
        )
        .unwrap();

        assert_eq!(graph.relations[0].subject, "ent_1");
        assert_eq!(graph.relations[0].object, "ent_2");
    }

    #[test]
    fn resolves_full_unicode_case_folded_endpoints() {
        let doc = doc(
            "Straße 负责 Projekt",
            vec![
                entity("ent_1", "Straße", &[]),
                entity("ent_2", "Projekt", &[]),
            ],
            vec![
                Mention {
                    id: String::new(),
                    entity: "ent_1".into(),
                    start: 0,
                    end: 6,
                },
                Mention {
                    id: String::new(),
                    entity: "ent_2".into(),
                    start: 10,
                    end: 17,
                },
            ],
        );

        let graph = materialize(
            "note-1",
            &doc,
            vec![raw("STRASSE", "projekt", 0, 17, "Straße 负责 Projekt")],
        )
        .unwrap();

        assert_eq!(graph.relations[0].subject, "ent_1");
    }

    #[test]
    fn unicode_case_fold_collisions_are_ambiguous() {
        let doc = doc(
            "Straße 负责 Projekt",
            vec![
                entity("ent_1", "Straße", &[]),
                entity("ent_2", "STRASSE", &[]),
                entity("ent_3", "Projekt", &[]),
            ],
            vec![
                Mention {
                    id: String::new(),
                    entity: "ent_1".into(),
                    start: 0,
                    end: 6,
                },
                Mention {
                    id: String::new(),
                    entity: "ent_2".into(),
                    start: 0,
                    end: 6,
                },
                Mention {
                    id: String::new(),
                    entity: "ent_3".into(),
                    start: 10,
                    end: 17,
                },
            ],
        );

        let issues = materialize(
            "note-1",
            &doc,
            vec![raw("strasse", "Projekt", 0, 17, "Straße 负责 Projekt")],
        )
        .unwrap_err();

        assert!(issues.iter().any(|issue| {
            issue.field == "relations[0].subject" && issue.message.contains("ambiguous")
        }));
    }

    #[test]
    fn rejects_ambiguous_and_missing_entity_names() {
        let doc = doc(
            "Owner 负责灯塔计划",
            vec![
                entity("ent_1", "张三", &["Owner"]),
                entity("ent_2", "李四", &["owner"]),
            ],
            vec![],
        );

        let ambiguous = materialize(
            "note-1",
            &doc,
            vec![raw("OWNER", "李四", 0, 14, "Owner 负责灯塔计划")],
        )
        .unwrap_err();
        assert!(ambiguous.iter().any(|issue| {
            issue.field == "relations[0].subject" && issue.message.contains("ambiguous")
        }));

        let missing = materialize(
            "note-1",
            &doc,
            vec![raw("不存在", "李四", 0, 14, "Owner 负责灯塔计划")],
        )
        .unwrap_err();
        assert!(missing.iter().any(|issue| {
            issue.field == "relations[0].subject" && issue.message.contains("missing")
        }));
    }

    #[test]
    fn rejects_invalid_quote_and_requires_both_endpoint_mentions_in_each_evidence_span() {
        let doc = doc(
            "张三负责灯塔计划",
            vec![
                entity("ent_1", "张三", &[]),
                entity("ent_2", "灯塔计划", &[]),
            ],
            vec![
                Mention {
                    id: String::new(),
                    entity: "ent_1".into(),
                    start: 0,
                    end: 2,
                },
                Mention {
                    id: String::new(),
                    entity: "ent_2".into(),
                    start: 4,
                    end: 8,
                },
            ],
        );

        let quote_issues = materialize(
            "note-1",
            &doc,
            vec![raw("张三", "灯塔计划", 0, 8, "张三拥有灯塔计划")],
        )
        .unwrap_err();
        assert!(quote_issues
            .iter()
            .any(|issue| issue.field.ends_with("evidence[0].quote")));

        let ownership_issues =
            materialize("note-1", &doc, vec![raw("张三", "灯塔计划", 0, 2, "张三")]).unwrap_err();
        assert!(ownership_issues
            .iter()
            .any(|issue| { issue.field.ends_with("evidence[0].object_mentions") }));
    }

    #[test]
    fn explicit_empty_relation_set_is_valid() {
        let doc = doc("没有关系", vec![], vec![]);
        assert!(materialize("note-1", &doc, vec![])
            .unwrap()
            .relations
            .is_empty());
    }
}
