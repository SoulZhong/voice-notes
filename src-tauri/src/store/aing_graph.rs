use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};

use super::refined::{RefinedDoc, RefinedParagraph};

#[allow(dead_code)] // Public contract consumed by the later HTTP and Agent write paths.
pub const GRAPH_CONTRACT_VERSION: u32 = 1;
#[allow(dead_code)] // Public contract consumed by the later HTTP and Agent write paths.
pub const CORE_PREDICATES: &[&str] = &[
    "participates_in",
    "responsible_for",
    "belongs_to",
    "uses",
    "depends_on",
    "produces",
    "assigned_to",
    "occurs_at",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[allow(dead_code)] // Public contract consumed by the later HTTP and Agent write paths.
pub struct ValidationIssue {
    pub relation_index: usize,
    pub field: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[allow(dead_code)] // Public contract consumed by the later HTTP and Agent write paths.
pub struct ValidatedGraph {
    pub relations: Vec<RelationFact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Public contract consumed by the later HTTP and Agent write paths.
pub enum PublishTier {
    Published,
    Pending,
    RawOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelationPredicate {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RelationEvidence {
    #[serde(default)]
    pub id: String,
    pub paragraph_index: usize,
    pub start: usize,
    pub end: usize,
    pub quote: String,
    #[serde(default)]
    pub source_seqs: Vec<u64>,
    #[serde(default)]
    pub source_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RelationFact {
    #[serde(default)]
    pub id: String,
    pub subject: String,
    pub predicate: RelationPredicate,
    pub object: String,
    #[serde(default)]
    pub subject_mentions: Vec<String>,
    #[serde(default)]
    pub object_mentions: Vec<String>,
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<String>,
    #[serde(default)]
    pub evidence: Vec<RelationEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GraphExtraction {
    pub contract_version: u32,
    pub provider: String,
    pub model: String,
    pub run_id: String,
    pub generated_at: String,
    pub source_hash: String,
    pub mode: String,
}

pub fn stable_id(prefix: &str, fields: &[String]) -> String {
    let mut h = Sha256::new();
    for field in fields {
        h.update((field.len() as u64).to_be_bytes());
        h.update(field.as_bytes());
    }
    format!("{prefix}{}", &format!("{:x}", h.finalize())[..24])
}

fn source_seq_fields(source_seqs: &[u64]) -> Vec<String> {
    let mut fields = vec![source_seqs.len().to_string()];
    fields.extend(source_seqs.iter().map(u64::to_string));
    fields
}

fn normalized_quote(quote: &str) -> String {
    quote.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn mention_id(
    note_id: &str,
    paragraph: &RefinedParagraph,
    entity: &str,
    start: usize,
    end: usize,
) -> String {
    let mut fields = vec![note_id.to_string()];
    fields.extend(source_seq_fields(&paragraph.source_seqs));
    fields.extend([
        paragraph.text.clone(),
        entity.to_string(),
        start.to_string(),
        end.to_string(),
    ]);
    stable_id("mn_", &fields)
}

pub fn evidence_id(
    note_id: &str,
    source_seqs: &[u64],
    start: usize,
    end: usize,
    quote: &str,
) -> String {
    let mut fields = vec![note_id.to_string()];
    fields.extend(source_seq_fields(source_seqs));
    fields.extend([start.to_string(), end.to_string(), normalized_quote(quote)]);
    stable_id("ev_", &fields)
}

pub fn relation_fact_id(note_id: &str, relation: &RelationFact) -> String {
    let mut subject_mentions = relation.subject_mentions.clone();
    subject_mentions.sort();
    let mut object_mentions = relation.object_mentions.clone();
    object_mentions.sort();
    let mut evidence_ids: Vec<_> = relation
        .evidence
        .iter()
        .map(|evidence| evidence.id.clone())
        .collect();
    evidence_ids.sort();

    let mut fields = vec![
        note_id.to_string(),
        relation.subject.clone(),
        relation.predicate.kind.clone(),
        relation.predicate.label.clone().unwrap_or_default(),
        relation.object.clone(),
        relation.confidence.to_string(),
        relation.valid_from.clone().unwrap_or_default(),
        relation.valid_to.clone().unwrap_or_default(),
    ];
    fields.push(subject_mentions.len().to_string());
    fields.extend(subject_mentions);
    fields.push(object_mentions.len().to_string());
    fields.extend(object_mentions);
    fields.push(evidence_ids.len().to_string());
    fields.extend(evidence_ids);
    stable_id("rf_", &fields)
}

pub fn source_hash(paragraphs: &[RefinedParagraph]) -> String {
    let mut h = Sha256::new();
    let mut fields = vec![paragraphs.len().to_string()];
    for paragraph in paragraphs {
        fields.extend(source_seq_fields(&paragraph.source_seqs));
        fields.push(paragraph.text.clone());
    }
    for field in fields {
        h.update((field.len() as u64).to_be_bytes());
        h.update(field.as_bytes());
    }
    format!("{:x}", h.finalize())
}

pub fn ensure_graph_ids(note_id: &str, doc: &mut RefinedDoc) {
    for paragraph in &mut doc.paragraphs {
        let identity = paragraph.clone();
        for mention in &mut paragraph.mentions {
            if mention.id.is_empty() {
                mention.id = mention_id(
                    note_id,
                    &identity,
                    &mention.entity,
                    mention.start,
                    mention.end,
                );
            }
        }
    }

    let doc_source_hash = source_hash(&doc.paragraphs);
    for relation in &mut doc.relations {
        for evidence in &mut relation.evidence {
            if evidence.id.is_empty() {
                evidence.id = evidence_id(
                    note_id,
                    &evidence.source_seqs,
                    evidence.start,
                    evidence.end,
                    &evidence.quote,
                );
            }
            if evidence.source_hash.is_empty() {
                evidence.source_hash = doc_source_hash.clone();
            }
        }
        if relation.id.is_empty() {
            relation.id = relation_fact_id(note_id, relation);
        }
    }
}

#[allow(dead_code)]
fn issue(issues: &mut Vec<ValidationIssue>, relation_index: usize, field: String, message: &str) {
    issues.push(ValidationIssue {
        relation_index,
        field,
        message: message.into(),
    });
}

#[allow(dead_code)]
fn field(relation_index: usize, suffix: &str) -> String {
    format!("relations[{relation_index}].{suffix}")
}

#[allow(dead_code)]
fn predicate_key(predicate: &RelationPredicate) -> String {
    match predicate.label.as_deref() {
        Some(label) => format!("{}\0{label}", predicate.kind),
        None => predicate.kind.clone(),
    }
}

#[allow(dead_code)] // Public contract consumed by the later HTTP and Agent write paths.
pub fn validate_graph(
    note_id: &str,
    doc: &RefinedDoc,
    relations: Vec<RelationFact>,
) -> Result<ValidatedGraph, Vec<ValidationIssue>> {
    let entity_ids: HashSet<&str> = doc
        .entities
        .iter()
        .map(|entity| entity.id.as_str())
        .collect();
    let mut normalized_mentions = Vec::new();
    for (paragraph_index, paragraph) in doc.paragraphs.iter().enumerate() {
        for mention in &paragraph.mentions {
            normalized_mentions.push((
                mention_id(
                    note_id,
                    paragraph,
                    &mention.entity,
                    mention.start,
                    mention.end,
                ),
                mention.entity.clone(),
                paragraph_index,
            ));
        }
    }
    let mention_ownership: HashMap<&str, (&str, usize)> = normalized_mentions
        .iter()
        .map(|(id, entity, paragraph_index)| (id.as_str(), (entity.as_str(), *paragraph_index)))
        .collect();
    let doc_source_hash = source_hash(&doc.paragraphs);
    let mut issues = Vec::new();
    let mut merged: BTreeMap<
        (String, String, String, Option<String>, Option<String>),
        RelationFact,
    > = BTreeMap::new();

    for (relation_index, mut relation) in relations.into_iter().enumerate() {
        let relation_issue_count = issues.len();
        let is_core = CORE_PREDICATES.contains(&relation.predicate.kind.as_str());
        if relation.predicate.kind == "custom" {
            match relation.predicate.label.as_mut() {
                Some(label) => {
                    *label = label.trim().into();
                    if label.is_empty() {
                        issue(
                            &mut issues,
                            relation_index,
                            field(relation_index, "predicate.label"),
                            "custom predicates require a non-empty label",
                        );
                    }
                }
                None => issue(
                    &mut issues,
                    relation_index,
                    field(relation_index, "predicate.label"),
                    "custom predicates require a non-empty label",
                ),
            }
        } else if !is_core {
            issue(
                &mut issues,
                relation_index,
                field(relation_index, "predicate.type"),
                "predicate must be a core predicate or custom",
            );
        }
        if !relation.confidence.is_finite() || !(0.0..=1.0).contains(&relation.confidence) {
            issue(
                &mut issues,
                relation_index,
                field(relation_index, "confidence"),
                "confidence must be between 0.0 and 1.0",
            );
        }
        if !entity_ids.contains(relation.subject.as_str()) {
            issue(
                &mut issues,
                relation_index,
                field(relation_index, "subject"),
                "subject must be an entity in this document",
            );
        }
        if !entity_ids.contains(relation.object.as_str()) {
            issue(
                &mut issues,
                relation_index,
                field(relation_index, "object"),
                "object must be an entity in this document",
            );
        }
        if relation.subject == relation.object {
            issue(
                &mut issues,
                relation_index,
                field(relation_index, "self_loop"),
                "subject and object must differ",
            );
        }
        if relation.subject_mentions.is_empty() {
            issue(
                &mut issues,
                relation_index,
                field(relation_index, "subject_mentions"),
                "subject_mentions must not be empty",
            );
        }
        if relation.object_mentions.is_empty() {
            issue(
                &mut issues,
                relation_index,
                field(relation_index, "object_mentions"),
                "object_mentions must not be empty",
            );
        }
        for (mention_index, mention_id) in relation.subject_mentions.iter().enumerate() {
            if mention_ownership
                .get(mention_id.as_str())
                .map(|(entity, _)| *entity)
                != Some(relation.subject.as_str())
            {
                issue(
                    &mut issues,
                    relation_index,
                    field(
                        relation_index,
                        &format!("subject_mentions[{mention_index}]"),
                    ),
                    "subject mention must belong to the subject entity",
                );
            }
        }
        for (mention_index, mention_id) in relation.object_mentions.iter().enumerate() {
            if mention_ownership
                .get(mention_id.as_str())
                .map(|(entity, _)| *entity)
                != Some(relation.object.as_str())
            {
                issue(
                    &mut issues,
                    relation_index,
                    field(relation_index, &format!("object_mentions[{mention_index}]")),
                    "object mention must belong to the object entity",
                );
            }
        }
        if relation.evidence.is_empty() {
            issue(
                &mut issues,
                relation_index,
                field(relation_index, "evidence"),
                "evidence must not be empty",
            );
        }

        let mut normalized_evidence = Vec::new();
        for (evidence_index, mut evidence) in relation.evidence.into_iter().enumerate() {
            let evidence_field = |name: &str| {
                field(
                    relation_index,
                    &format!("evidence[{evidence_index}].{name}"),
                )
            };
            evidence.source_seqs.sort_unstable();
            evidence.source_seqs.dedup();
            let has_nonempty_source_seqs = !evidence.source_seqs.is_empty();
            if !has_nonempty_source_seqs {
                issue(
                    &mut issues,
                    relation_index,
                    evidence_field("source_seqs"),
                    "source_seqs must be a non-empty subset of the paragraph source_seqs",
                );
            }
            let has_nonempty_span = evidence.start < evidence.end;
            if !has_nonempty_span {
                issue(
                    &mut issues,
                    relation_index,
                    evidence_field("end"),
                    "evidence start must be before end",
                );
            }
            let Some(paragraph) = doc.paragraphs.get(evidence.paragraph_index) else {
                issue(
                    &mut issues,
                    relation_index,
                    evidence_field("paragraph_index"),
                    "paragraph_index must reference a document paragraph",
                );
                continue;
            };
            let text_chars: Vec<_> = paragraph.text.chars().collect();
            let valid_span = if !has_nonempty_span {
                false
            } else if evidence.end > text_chars.len() {
                issue(
                    &mut issues,
                    relation_index,
                    evidence_field("end"),
                    "evidence end exceeds paragraph text",
                );
                false
            } else {
                true
            };
            if valid_span {
                let quote: String = text_chars[evidence.start..evidence.end].iter().collect();
                if quote != evidence.quote {
                    issue(
                        &mut issues,
                        relation_index,
                        evidence_field("quote"),
                        "evidence quote must exactly match the paragraph span",
                    );
                }
            }
            if has_nonempty_source_seqs
                && evidence
                    .source_seqs
                    .iter()
                    .any(|source_seq| !paragraph.source_seqs.contains(source_seq))
            {
                issue(
                    &mut issues,
                    relation_index,
                    evidence_field("source_seqs"),
                    "source_seqs must be a non-empty subset of the paragraph source_seqs",
                );
            }
            if valid_span
                && has_nonempty_source_seqs
                && evidence
                    .source_seqs
                    .iter()
                    .all(|source_seq| paragraph.source_seqs.contains(source_seq))
            {
                evidence.id = evidence_id(
                    note_id,
                    &evidence.source_seqs,
                    evidence.start,
                    evidence.end,
                    &evidence.quote,
                );
                evidence.source_hash = doc_source_hash.clone();
                normalized_evidence.push(evidence);
            }
        }

        if issues.len() != relation_issue_count {
            continue;
        }
        relation.subject_mentions.sort();
        relation.subject_mentions.dedup();
        relation.object_mentions.sort();
        relation.object_mentions.dedup();
        let mut evidence_by_id = BTreeMap::new();
        for evidence in normalized_evidence {
            evidence_by_id.insert(evidence.id.clone(), evidence);
        }
        relation.evidence = evidence_by_id.into_values().collect();
        let key = (
            relation.subject.clone(),
            predicate_key(&relation.predicate),
            relation.object.clone(),
            relation.valid_from.clone(),
            relation.valid_to.clone(),
        );
        match merged.get_mut(&key) {
            Some(existing) => {
                existing.subject_mentions.extend(relation.subject_mentions);
                existing.object_mentions.extend(relation.object_mentions);
                existing.evidence.extend(relation.evidence);
                existing.confidence = existing.confidence.max(relation.confidence);
            }
            None => {
                merged.insert(key, relation);
            }
        }
    }

    if !issues.is_empty() {
        return Err(issues);
    }

    let mut relations = merged
        .into_values()
        .map(|mut relation| {
            relation.subject_mentions.sort();
            relation.subject_mentions.dedup();
            relation.object_mentions.sort();
            relation.object_mentions.dedup();
            let mut evidence_by_id = BTreeMap::new();
            for evidence in relation.evidence {
                evidence_by_id.insert(evidence.id.clone(), evidence);
            }
            relation.evidence = evidence_by_id.into_values().collect();
            relation.id = relation_fact_id(note_id, &relation);
            relation
        })
        .collect::<Vec<_>>();
    relations.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(ValidatedGraph { relations })
}

#[allow(dead_code)] // Public contract consumed by the later HTTP and Agent write paths.
pub fn publish_tier(
    relation: &RelationFact,
    identity_conflict: bool,
    time_conflict: bool,
) -> PublishTier {
    if relation.confidence < 0.5 {
        PublishTier::RawOnly
    } else if relation.confidence < 0.8
        || !CORE_PREDICATES.contains(&relation.predicate.kind.as_str())
        || relation.evidence.is_empty()
        || identity_conflict
        || time_conflict
    {
        PublishTier::Pending
    } else {
        PublishTier::Published
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Entity, Mention, RefineStages};

    fn paragraph(text: &str, source_seqs: Vec<u64>) -> RefinedParagraph {
        RefinedParagraph {
            speaker: "S1".into(),
            name: None,
            person_id: None,
            start_ms: 0,
            end_ms: 1,
            text: text.into(),
            source_seqs,
            mentions: vec![Mention {
                id: String::new(),
                entity: "ent_1".into(),
                start: 0,
                end: 2,
            }],
        }
    }

    #[test]
    fn stable_ids_normalize_quotes_and_sort_relation_references() {
        assert_eq!(
            evidence_id("n1", &[1, 2], 0, 2, "  hello   world "),
            evidence_id("n1", &[1, 2], 0, 2, "hello world"),
        );

        let mut first = RelationFact {
            id: String::new(),
            subject: "A".into(),
            predicate: RelationPredicate {
                kind: "related_to".into(),
                label: None,
            },
            object: "B".into(),
            subject_mentions: vec!["mn_b".into(), "mn_a".into()],
            object_mentions: vec!["mn_d".into(), "mn_c".into()],
            confidence: 0.9,
            valid_from: None,
            valid_to: None,
            evidence: vec![
                RelationEvidence {
                    id: "ev_b".into(),
                    paragraph_index: 0,
                    start: 0,
                    end: 1,
                    quote: "A".into(),
                    source_seqs: vec![1],
                    source_hash: String::new(),
                },
                RelationEvidence {
                    id: "ev_a".into(),
                    paragraph_index: 0,
                    start: 2,
                    end: 3,
                    quote: "B".into(),
                    source_seqs: vec![1],
                    source_hash: String::new(),
                },
            ],
        };
        let first_id = relation_fact_id("n1", &first);
        first.subject_mentions.reverse();
        first.object_mentions.reverse();
        first.evidence.reverse();
        assert_eq!(first_id, relation_fact_id("n1", &first));
        assert_ne!(
            source_hash(&[paragraph("one", vec![1])]),
            source_hash(&[paragraph("two", vec![1])])
        );
    }

    fn validator_doc() -> RefinedDoc {
        RefinedDoc {
            schema_version: 2,
            generated_at: "2026-07-21T00:00:00Z".into(),
            llm_model: None,
            stages: RefineStages {
                filter: "done".into(),
                recluster: "done".into(),
                llm: "done".into(),
                entities: "done".into(),
                relations: "done".into(),
            },
            discarded_seqs: vec![],
            entities: vec![
                Entity {
                    id: "ent_zhang".into(),
                    kind: "person".into(),
                    name: "张三".into(),
                    aliases: vec![],
                },
                Entity {
                    id: "ent_rust".into(),
                    kind: "tool".into(),
                    name: "Rust".into(),
                    aliases: vec![],
                },
            ],
            graph_extraction: None,
            relations: vec![],
            paragraphs: vec![RefinedParagraph {
                speaker: "S1".into(),
                name: None,
                person_id: None,
                start_ms: 0,
                end_ms: 1,
                text: "张三使用Rust。".into(),
                source_seqs: vec![7, 8],
                mentions: vec![
                    Mention {
                        id: "untrusted-subject".into(),
                        entity: "ent_zhang".into(),
                        start: 0,
                        end: 2,
                    },
                    Mention {
                        id: "untrusted-object".into(),
                        entity: "ent_rust".into(),
                        start: 4,
                        end: 8,
                    },
                ],
            }],
        }
    }

    fn valid_fact(confidence: f64, predicate: &str) -> RelationFact {
        let doc = validator_doc();
        let paragraph = &doc.paragraphs[0];
        RelationFact {
            id: "untrusted-relation".into(),
            subject: "ent_zhang".into(),
            predicate: RelationPredicate {
                kind: predicate.into(),
                label: None,
            },
            object: "ent_rust".into(),
            subject_mentions: vec![mention_id("note-1", paragraph, "ent_zhang", 0, 2)],
            object_mentions: vec![mention_id("note-1", paragraph, "ent_rust", 4, 8)],
            confidence,
            valid_from: None,
            valid_to: None,
            evidence: vec![RelationEvidence {
                id: "untrusted-evidence".into(),
                paragraph_index: 0,
                start: 0,
                end: 8,
                quote: "张三使用Rust".into(),
                source_seqs: vec![7, 8],
                source_hash: "untrusted-hash".into(),
            }],
        }
    }

    fn fields(issues: &[ValidationIssue]) -> Vec<&str> {
        issues.iter().map(|issue| issue.field.as_str()).collect()
    }

    #[test]
    fn validator_accepts_every_core_predicate() {
        let relations = [
            "participates_in",
            "responsible_for",
            "belongs_to",
            "uses",
            "depends_on",
            "produces",
            "assigned_to",
            "occurs_at",
        ]
        .into_iter()
        .map(|predicate| valid_fact(0.8, predicate))
        .collect();

        assert_eq!(
            validate_graph("note-1", &validator_doc(), relations)
                .unwrap()
                .relations
                .len(),
            8
        );
    }

    #[test]
    fn validator_trims_custom_labels_and_recomputes_untrusted_ids() {
        let mut relation = valid_fact(0.9, "custom");
        relation.predicate.label = Some("  推动  ".into());

        let graph = validate_graph("note-1", &validator_doc(), vec![relation]).unwrap();
        let relation = &graph.relations[0];
        assert_eq!(relation.predicate.label.as_deref(), Some("推动"));
        assert!(relation.id.starts_with("rf_"));
        assert!(relation.evidence[0].id.starts_with("ev_"));
        assert_ne!(relation.evidence[0].source_hash, "untrusted-hash");
    }

    #[test]
    fn validator_returns_all_stable_field_issues() {
        let mut relation = valid_fact(1.1, "unknown");
        relation.subject = "missing".into();
        relation.object = "missing".into();
        relation.subject_mentions.clear();
        relation.object_mentions = vec!["missing-mention".into()];
        relation.evidence = vec![RelationEvidence {
            id: String::new(),
            paragraph_index: 2,
            start: 9,
            end: 9,
            quote: "wrong".into(),
            source_seqs: vec![],
            source_hash: String::new(),
        }];

        let issues = validate_graph("note-1", &validator_doc(), vec![relation]).unwrap_err();
        let fields = fields(&issues);
        assert!(fields.contains(&"relations[0].predicate.type"));
        assert!(fields.contains(&"relations[0].confidence"));
        assert!(fields.contains(&"relations[0].subject"));
        assert!(fields.contains(&"relations[0].object"));
        assert!(fields.contains(&"relations[0].subject_mentions"));
        assert!(fields.contains(&"relations[0].object_mentions[0]"));
        assert!(fields.contains(&"relations[0].evidence[0].paragraph_index"));
        assert!(fields.contains(&"relations[0].evidence[0].end"));
        assert!(fields.contains(&"relations[0].evidence[0].source_seqs"));
    }

    #[test]
    fn validator_rejects_negative_confidence_on_an_otherwise_valid_relation() {
        let issues = validate_graph("note-1", &validator_doc(), vec![valid_fact(-0.01, "uses")])
            .unwrap_err();

        assert_eq!(fields(&issues), vec!["relations[0].confidence"]);
    }

    #[test]
    fn validator_rejects_empty_source_seqs_on_an_otherwise_valid_evidence() {
        let mut relation = valid_fact(0.8, "uses");
        relation.evidence[0].source_seqs.clear();

        let issues = validate_graph("note-1", &validator_doc(), vec![relation]).unwrap_err();
        assert_eq!(
            fields(&issues),
            vec!["relations[0].evidence[0].source_seqs"]
        );
    }

    #[test]
    fn validator_rejects_invalid_mentions_and_evidence_with_unicode_offsets() {
        let mut relation = valid_fact(0.8, "uses");
        relation.object = "ent_zhang".into();
        relation.evidence = vec![RelationEvidence {
            id: String::new(),
            paragraph_index: 0,
            start: 0,
            end: 3,
            quote: "张三使用Rust".into(),
            source_seqs: vec![9],
            source_hash: String::new(),
        }];

        let issues = validate_graph("note-1", &validator_doc(), vec![relation]).unwrap_err();
        let fields = fields(&issues);
        assert!(fields.contains(&"relations[0].object_mentions[0]"));
        assert!(fields.contains(&"relations[0].self_loop"));
        assert!(fields.contains(&"relations[0].evidence[0].quote"));
        assert!(fields.contains(&"relations[0].evidence[0].source_seqs"));
    }

    #[test]
    fn validator_rejects_empty_custom_label_missing_evidence_and_out_of_bounds_span() {
        let mut relation = valid_fact(0.8, "custom");
        relation.predicate.label = Some(" \t ".into());
        relation.evidence.clear();
        let mut out_of_bounds = valid_fact(0.8, "uses");
        out_of_bounds.evidence[0].end = 99;

        let issues =
            validate_graph("note-1", &validator_doc(), vec![relation, out_of_bounds]).unwrap_err();
        let fields = fields(&issues);
        assert!(fields.contains(&"relations[0].predicate.label"));
        assert!(fields.contains(&"relations[0].evidence"));
        assert!(fields.contains(&"relations[1].evidence[0].end"));
    }

    #[test]
    fn validator_removes_duplicate_evidence_and_merges_equal_triples() {
        let mut first = valid_fact(0.8, "uses");
        first.evidence.push(first.evidence[0].clone());
        let mut second = valid_fact(0.8, "uses");
        second.evidence[0].start = 4;
        second.evidence[0].end = 8;
        second.evidence[0].quote = "Rust".into();

        let graph = validate_graph("note-1", &validator_doc(), vec![second, first]).unwrap();
        assert_eq!(graph.relations.len(), 1);
        assert_eq!(graph.relations[0].evidence.len(), 2);
        assert!(graph.relations[0]
            .evidence
            .windows(2)
            .all(|pair| pair[0].id <= pair[1].id));
    }

    #[test]
    fn validator_canonicalizes_source_seqs_before_deduplicating_evidence() {
        let forward = valid_fact(0.8, "uses");
        let mut reversed = valid_fact(0.8, "uses");
        reversed.evidence[0].source_seqs.reverse();
        let mut duplicates = forward.clone();
        duplicates.evidence.push(reversed.evidence[0].clone());

        let forward = validate_graph("note-1", &validator_doc(), vec![forward]).unwrap();
        let reversed = validate_graph("note-1", &validator_doc(), vec![reversed]).unwrap();
        let duplicates = validate_graph("note-1", &validator_doc(), vec![duplicates]).unwrap();

        assert_eq!(
            serde_json::to_vec(&forward).unwrap(),
            serde_json::to_vec(&reversed).unwrap()
        );
        assert_eq!(duplicates.relations[0].evidence.len(), 1);
        assert_eq!(duplicates.relations[0].evidence[0].source_seqs, vec![7, 8]);
    }

    #[test]
    fn validator_fixture_output_is_stable_in_relation_order() {
        #[derive(Deserialize)]
        struct Fixture {
            relations: Vec<RelationFact>,
        }

        let fixture: Fixture =
            serde_json::from_str(include_str!("../../tests/fixtures/aing_graph_valid.json"))
                .unwrap();
        let first = fixture.relations.clone();
        let mut second = fixture.relations;
        second.reverse();

        let first = serde_json::to_vec(&validate_graph("note-1", &validator_doc(), first).unwrap())
            .unwrap();
        let second =
            serde_json::to_vec(&validate_graph("note-1", &validator_doc(), second).unwrap())
                .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn validator_invalid_fixture_covers_predicate_mentions_evidence_and_confidence() {
        #[derive(Deserialize)]
        struct Fixture {
            relations: Vec<RelationFact>,
        }

        let fixture: Fixture =
            serde_json::from_str(include_str!("../../tests/fixtures/aing_graph_invalid.json"))
                .unwrap();
        let issues = validate_graph("note-1", &validator_doc(), fixture.relations).unwrap_err();
        let fields = fields(&issues);
        assert!(fields.iter().any(|field| field.contains("predicate")));
        assert!(fields.iter().any(|field| field.contains("mentions")));
        assert!(fields.iter().any(|field| field.contains("evidence")));
        assert!(fields.iter().any(|field| field.contains("confidence")));
    }

    #[test]
    fn validator_publish_tier_follows_confidence_and_conflicts() {
        assert_eq!(
            publish_tier(&valid_fact(0.80, "uses"), false, false),
            PublishTier::Published
        );
        assert_eq!(
            publish_tier(&valid_fact(0.50, "uses"), false, false),
            PublishTier::Pending
        );
        assert_eq!(
            publish_tier(&valid_fact(0.79, "uses"), false, false),
            PublishTier::Pending
        );
        assert_eq!(
            publish_tier(&valid_fact(0.99, "custom"), false, false),
            PublishTier::Pending
        );
        assert_eq!(
            publish_tier(&valid_fact(0.49, "uses"), false, false),
            PublishTier::RawOnly
        );
        assert_eq!(
            publish_tier(&valid_fact(0.99, "uses"), true, false),
            PublishTier::Pending
        );
    }
}
