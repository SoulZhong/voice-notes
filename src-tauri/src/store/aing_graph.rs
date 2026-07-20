use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::refined::{RefinedDoc, RefinedParagraph};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Mention;

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
}
