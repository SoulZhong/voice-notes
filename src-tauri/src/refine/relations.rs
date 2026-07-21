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
            schema_version: crate::store::refined::REFINED_SCHEMA_VERSION,
            generated_at: "2026-07-21T09:00:00+08:00".into(),
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
