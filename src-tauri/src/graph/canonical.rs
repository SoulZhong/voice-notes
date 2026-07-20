use super::overrides::{
    self, KnowledgeAction, KnowledgeLedger, KnowledgeOperation, RegistryEntity, RegistryState,
};
use super::resolve::{self, ResolutionStatus};
use crate::store::aing_graph::PublishTier;
use crate::store::{self, RefinedDoc, RelationFact, RelationPredicate, VoiceprintStore};
use chrono::{DateTime, FixedOffset};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CanonicalEntity {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CanonicalMention {
    pub id: String,
    pub note_id: String,
    pub entity_id: String,
    pub paragraph_index: usize,
    pub start: usize,
    pub end: usize,
    pub quote: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CanonicalEvidence {
    pub id: String,
    pub note_id: String,
    pub paragraph_index: usize,
    pub start: usize,
    pub end: usize,
    pub quote: String,
    pub source_seqs: Vec<u64>,
    pub source_hash: String,
    pub subject_mentions: Vec<String>,
    pub object_mentions: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationStatus {
    Current,
    Historical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationOrigin {
    Model,
    Confirmed,
    Manual,
    UserAssertion,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CanonicalRelation {
    pub id: String,
    pub subject_id: String,
    pub predicate: RelationPredicate,
    pub object_id: String,
    pub confidence: f64,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub status: RelationStatus,
    pub origin: RelationOrigin,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub note_ids: Vec<String>,
    pub evidence: Vec<CanonicalEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingItem {
    InvalidDocument {
        note_id: String,
        message: String,
    },
    IdentityConflict {
        note_id: String,
        local_entity_id: String,
        candidates: Vec<String>,
        reason: String,
    },
    StaleEvidence {
        note_id: String,
        relation_id: String,
        evidence_id: String,
    },
    SplitConflict {
        note_id: String,
        relation_id: String,
        evidence_id: String,
    },
    RelationReview {
        note_id: String,
        relation_id: String,
    },
    TimeConflict {
        relation_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CanonicalGraph {
    pub entities: BTreeMap<String, CanonicalEntity>,
    pub mentions: Vec<CanonicalMention>,
    pub relations: Vec<CanonicalRelation>,
    pub pending: Vec<PendingItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RelationKey {
    subject_id: String,
    predicate_type: String,
    predicate_label: Option<String>,
    object_id: String,
}

pub fn reconcile_registry(data_root: &Path) -> anyhow::Result<KnowledgeLedger> {
    overrides::update(data_root, |ledger| {
        let documents = scan_documents(data_root);
        for document in documents.into_iter().filter_map(|document| document.ok()) {
            for local in &document.doc.entities {
                if local.name.trim().is_empty() {
                    continue;
                }
                let legacy_key = local_key(&document.note_id, &local.id);
                if ledger.legacy_ids.contains_key(&legacy_key) {
                    continue;
                }
                let candidates = registry_matches(&ledger.registry, local);
                if candidates.len() > 1 {
                    // Ambiguity is a governance decision: leave it unbound for the pure builder
                    // to surface instead of manufacturing a third identity.
                    continue;
                }
                let entity_id = if candidates.len() == 1 {
                    candidates[0].clone()
                } else if local.kind == "person" && is_person_id(&local.id) {
                    local.id.clone()
                } else {
                    overrides::allocate_entity_id(
                        &local.kind,
                        &local.name,
                        &document.note_id,
                        &local.id,
                    )
                };
                if !ledger.registry.contains_key(&entity_id) {
                    let mut aliases = local.aliases.clone();
                    aliases.sort();
                    aliases.dedup();
                    let entity = RegistryEntity {
                        kind: local.kind.clone(),
                        name: local.name.clone(),
                        aliases,
                        status: "model".into(),
                    };
                    let operation_id =
                        store::stable_id("op_", &["canonical_seed".into(), entity_id.clone()]);
                    ledger.registry.insert(entity_id.clone(), entity.clone());
                    ledger.operations.push(KnowledgeOperation {
                        id: operation_id,
                        at: document.doc.generated_at.clone(),
                        before: serde_json::to_value(RegistryState {
                            entity_id: entity_id.clone(),
                            entity: None,
                        })?,
                        after: serde_json::to_value(RegistryState {
                            entity_id: entity_id.clone(),
                            entity: Some(entity.clone()),
                        })?,
                        action: KnowledgeAction::CreateEntity { entity },
                    });
                }
                ledger.legacy_ids.insert(legacy_key, entity_id);
            }
        }
        Ok(())
    })?;
    overrides::load(data_root).map_err(anyhow::Error::from)
}

pub fn build_canonical_graph(
    data_root: &Path,
    ledger: &KnowledgeLedger,
    _now: DateTime<FixedOffset>,
) -> anyhow::Result<CanonicalGraph> {
    let snapshot = resolve::replay(ledger)?;
    let people = VoiceprintStore::new(data_root.to_path_buf()).load();
    let entities = snapshot
        .registry
        .iter()
        .filter(|(id, _)| !snapshot.redirects.contains_key(*id))
        .filter_map(|(id, entity)| {
            let canonical_id = resolve::resolve_reference_id(&snapshot, &people, id).entity_id?;
            Some((
                canonical_id.clone(),
                CanonicalEntity {
                    id: canonical_id,
                    kind: entity.kind.clone(),
                    name: entity.name.clone(),
                    aliases: entity.aliases.clone(),
                    confirmed: entity.status == "confirmed",
                },
            ))
        })
        .collect();
    let mut mentions = Vec::new();
    let mut pending = Vec::new();
    for document in scan_documents(data_root) {
        let document = match document {
            Ok(document) => document,
            Err(invalid) => {
                pending.push(PendingItem::InvalidDocument {
                    note_id: invalid.note_id,
                    message: invalid.message,
                });
                continue;
            }
        };
        let locals: BTreeMap<_, _> = document
            .doc
            .entities
            .iter()
            .map(|entity| (entity.id.as_str(), entity))
            .collect();
        for (paragraph_index, paragraph) in document.doc.paragraphs.iter().enumerate() {
            for mention in &paragraph.mentions {
                let Some(local) = locals.get(mention.entity.as_str()) else {
                    pending.push(PendingItem::IdentityConflict {
                        note_id: document.note_id.clone(),
                        local_entity_id: mention.entity.clone(),
                        candidates: Vec::new(),
                        reason: "mention references a missing local entity".into(),
                    });
                    continue;
                };
                let resolution = resolve::resolve_entity(
                    &snapshot,
                    &people,
                    &document.note_id,
                    local,
                    std::slice::from_ref(&mention.id),
                );
                let Some(entity_id) = resolution.entity_id else {
                    pending.push(PendingItem::IdentityConflict {
                        note_id: document.note_id.clone(),
                        local_entity_id: local.id.clone(),
                        candidates: resolution.candidates,
                        reason: resolution
                            .reason
                            .unwrap_or_else(|| "identity conflict".into()),
                    });
                    continue;
                };
                if resolution.status == ResolutionStatus::PendingConflict {
                    continue;
                }
                let Some(quote) = char_slice(&paragraph.text, mention.start, mention.end) else {
                    pending.push(PendingItem::IdentityConflict {
                        note_id: document.note_id.clone(),
                        local_entity_id: local.id.clone(),
                        candidates: Vec::new(),
                        reason: "mention range is outside paragraph".into(),
                    });
                    continue;
                };
                mentions.push(CanonicalMention {
                    id: mention.id.clone(),
                    note_id: document.note_id.clone(),
                    entity_id,
                    paragraph_index,
                    start: mention.start,
                    end: mention.end,
                    quote,
                });
            }
        }
    }
    mentions.sort_by(|left, right| left.id.cmp(&right.id));
    let mention_index: BTreeMap<_, _> = mentions
        .iter()
        .map(|mention| {
            (
                (mention.note_id.clone(), mention.id.clone()),
                mention.clone(),
            )
        })
        .collect();
    let mut relation_groups: BTreeMap<(RelationKey, String), CanonicalRelation> = BTreeMap::new();
    let mut evidence_catalog = BTreeMap::new();
    for document in scan_documents(data_root).into_iter().filter_map(Result::ok) {
        for relation in &document.doc.relations {
            project_model_relation(
                &document,
                relation,
                &snapshot,
                &people,
                &mention_index,
                &mut relation_groups,
                &mut evidence_catalog,
                &mut pending,
                _now,
            );
        }
    }
    project_created_relations(
        &snapshot,
        &people,
        &evidence_catalog,
        &mention_index,
        &mut relation_groups,
        &mut pending,
        _now,
    );
    let mut relations: Vec<_> = relation_groups.into_values().collect();
    mark_time_conflicts(&mut relations, &mut pending, _now);
    relations.sort_by(|left, right| left.id.cmp(&right.id));
    pending.sort_by_key(pending_sort_key);
    Ok(CanonicalGraph {
        entities,
        mentions,
        relations,
        pending,
    })
}

fn project_model_relation(
    document: &ScannedDocument,
    source: &RelationFact,
    snapshot: &resolve::ResolverSnapshot,
    people: &store::Voiceprints,
    mentions: &BTreeMap<(String, String), CanonicalMention>,
    groups: &mut BTreeMap<(RelationKey, String), CanonicalRelation>,
    evidence_catalog: &mut BTreeMap<String, CanonicalEvidence>,
    pending: &mut Vec<PendingItem>,
    now: DateTime<FixedOffset>,
) {
    let resolved_evidence = match resolve_evidence(document, source, pending) {
        Some(evidence) => evidence,
        None => return,
    };
    for evidence in &resolved_evidence {
        evidence_catalog.insert(evidence.id.clone(), evidence.clone());
    }
    let subject_mentions: Vec<_> = source
        .subject_mentions
        .iter()
        .filter_map(|id| mentions.get(&(document.note_id.clone(), id.clone())))
        .collect();
    let object_mentions: Vec<_> = source
        .object_mentions
        .iter()
        .filter_map(|id| mentions.get(&(document.note_id.clone(), id.clone())))
        .collect();
    if subject_mentions.is_empty() || object_mentions.is_empty() {
        pending.push(PendingItem::SplitConflict {
            note_id: document.note_id.clone(),
            relation_id: source.id.clone(),
            evidence_id: String::new(),
        });
        return;
    }

    let mut split: BTreeMap<(String, String), Vec<CanonicalEvidence>> = BTreeMap::new();
    for evidence in resolved_evidence {
        let subject_ids: BTreeSet<_> = subject_mentions
            .iter()
            .filter(|mention| evidence_contains(&evidence, mention))
            .map(|mention| mention.entity_id.clone())
            .collect();
        let object_ids: BTreeSet<_> = object_mentions
            .iter()
            .filter(|mention| evidence_contains(&evidence, mention))
            .map(|mention| mention.entity_id.clone())
            .collect();
        if subject_ids.is_empty() || object_ids.is_empty() {
            pending.push(PendingItem::SplitConflict {
                note_id: document.note_id.clone(),
                relation_id: source.id.clone(),
                evidence_id: evidence.id.clone(),
            });
            continue;
        }
        for subject_id in &subject_ids {
            for object_id in &object_ids {
                let mut evidence = evidence.clone();
                evidence.subject_mentions = subject_mentions
                    .iter()
                    .filter(|mention| {
                        mention.entity_id == *subject_id && evidence_contains(&evidence, mention)
                    })
                    .map(|mention| mention.id.clone())
                    .collect();
                evidence.object_mentions = object_mentions
                    .iter()
                    .filter(|mention| {
                        mention.entity_id == *object_id && evidence_contains(&evidence, mention)
                    })
                    .map(|mention| mention.id.clone())
                    .collect();
                split
                    .entry((subject_id.clone(), object_id.clone()))
                    .or_default()
                    .push(evidence);
            }
        }
    }

    for ((mut subject_id, mut object_id), mut evidence) in split {
        let mut predicate = source.predicate.clone();
        let mut valid_from = source.valid_from.clone();
        let mut valid_to = source.valid_to.clone();
        let mut origin = if snapshot.relation_decisions.confirmed.contains(&source.id) {
            RelationOrigin::Confirmed
        } else {
            RelationOrigin::Model
        };
        if let Some(edit) = snapshot.relation_decisions.edited.get(&source.id) {
            let subject = resolve::resolve_reference_id(snapshot, people, &edit.subject_id);
            let object = resolve::resolve_reference_id(snapshot, people, &edit.object_id);
            let (Some(edited_subject), Some(edited_object)) = (subject.entity_id, object.entity_id)
            else {
                pending.push(PendingItem::IdentityConflict {
                    note_id: document.note_id.clone(),
                    local_entity_id: source.id.clone(),
                    candidates: subject
                        .candidates
                        .into_iter()
                        .chain(object.candidates)
                        .collect(),
                    reason: "edited relation references an unresolved entity".into(),
                });
                continue;
            };
            subject_id = edited_subject;
            predicate = edit.predicate.clone();
            object_id = edited_object;
            valid_from = edit.valid_from.clone();
            valid_to = edit.valid_to.clone();
            origin = RelationOrigin::Manual;
            for item in &mut evidence {
                let mut mention_ids = item.subject_mentions.clone();
                mention_ids.extend(item.object_mentions.clone());
                mention_ids.sort();
                mention_ids.dedup();
                item.subject_mentions = mention_ids
                    .iter()
                    .filter(|mention_id| {
                        mentions
                            .get(&(document.note_id.clone(), (*mention_id).clone()))
                            .is_some_and(|mention| mention.entity_id == subject_id)
                    })
                    .cloned()
                    .collect();
                item.object_mentions = mention_ids
                    .iter()
                    .filter(|mention_id| {
                        mentions
                            .get(&(document.note_id.clone(), (*mention_id).clone()))
                            .is_some_and(|mention| mention.entity_id == object_id)
                    })
                    .cloned()
                    .collect();
            }
        }
        if is_suppressed(snapshot, people, &subject_id, &predicate, &object_id) {
            continue;
        }
        let pre_end_relation_id = canonical_relation_id(
            &subject_id,
            &predicate,
            &object_id,
            valid_from.as_deref(),
            valid_to.as_deref(),
        );
        if let Some(ended) = snapshot
            .relation_decisions
            .ended
            .get(&source.id)
            .or_else(|| snapshot.relation_decisions.ended.get(&pre_end_relation_id))
        {
            valid_to = Some(ended.clone());
        }
        canonicalize_evidence(&mut evidence);
        let relation_id = canonical_relation_id(
            &subject_id,
            &predicate,
            &object_id,
            valid_from.as_deref(),
            valid_to.as_deref(),
        );
        let fact = RelationFact {
            id: source.id.clone(),
            subject: subject_id.clone(),
            predicate: predicate.clone(),
            object: object_id.clone(),
            subject_mentions: evidence
                .iter()
                .flat_map(|item| item.subject_mentions.clone())
                .collect(),
            object_mentions: evidence
                .iter()
                .flat_map(|item| item.object_mentions.clone())
                .collect(),
            confidence: source.confidence,
            valid_from: valid_from.clone(),
            valid_to: valid_to.clone(),
            evidence: evidence
                .iter()
                .map(|item| store::RelationEvidence {
                    id: item.id.clone(),
                    paragraph_index: item.paragraph_index,
                    start: item.start,
                    end: item.end,
                    quote: item.quote.clone(),
                    source_seqs: item.source_seqs.clone(),
                    source_hash: item.source_hash.clone(),
                })
                .collect(),
        };
        match store::aing_graph::publish_tier(&fact, false, false) {
            PublishTier::RawOnly => continue,
            PublishTier::Pending => pending.push(PendingItem::RelationReview {
                note_id: document.note_id.clone(),
                relation_id: relation_id.clone(),
            }),
            PublishTier::Published => {}
        }
        let extraction = document.doc.graph_extraction.as_ref();
        insert_relation(
            groups,
            CanonicalRelation {
                id: relation_id,
                subject_id,
                predicate,
                object_id,
                confidence: source.confidence,
                valid_from,
                valid_to: valid_to.clone(),
                status: relation_status(valid_to.as_deref(), now),
                origin,
                provider: extraction.map(|value| value.provider.clone()),
                model: extraction.map(|value| value.model.clone()),
                note_ids: vec![document.note_id.clone()],
                evidence,
            },
        );
    }
}

fn resolve_evidence(
    document: &ScannedDocument,
    relation: &RelationFact,
    pending: &mut Vec<PendingItem>,
) -> Option<Vec<CanonicalEvidence>> {
    let mut result = Vec::new();
    for evidence in &relation.evidence {
        let location = document
            .doc
            .paragraphs
            .get(evidence.paragraph_index)
            .and_then(|paragraph| {
                (char_slice(&paragraph.text, evidence.start, evidence.end).as_deref()
                    == Some(evidence.quote.as_str())
                    && evidence
                        .source_seqs
                        .iter()
                        .all(|seq| paragraph.source_seqs.contains(seq)))
                .then_some((evidence.paragraph_index, evidence.start, evidence.end))
            })
            .or_else(|| unique_evidence_location(&document.doc, evidence));
        let Some((paragraph_index, start, end)) = location else {
            pending.push(PendingItem::StaleEvidence {
                note_id: document.note_id.clone(),
                relation_id: relation.id.clone(),
                evidence_id: evidence.id.clone(),
            });
            continue;
        };
        result.push(CanonicalEvidence {
            id: evidence.id.clone(),
            note_id: document.note_id.clone(),
            paragraph_index,
            start,
            end,
            quote: evidence.quote.clone(),
            source_seqs: evidence.source_seqs.clone(),
            source_hash: evidence.source_hash.clone(),
            subject_mentions: Vec::new(),
            object_mentions: Vec::new(),
        });
    }
    (!result.is_empty()).then_some(result)
}

fn unique_evidence_location(
    doc: &RefinedDoc,
    evidence: &store::RelationEvidence,
) -> Option<(usize, usize, usize)> {
    if evidence.source_hash != store::source_hash(&doc.paragraphs) {
        return None;
    }
    let mut matches = Vec::new();
    for (paragraph_index, paragraph) in doc.paragraphs.iter().enumerate() {
        if !evidence
            .source_seqs
            .iter()
            .all(|seq| paragraph.source_seqs.contains(seq))
        {
            continue;
        }
        let text: Vec<_> = paragraph.text.chars().collect();
        let quote: Vec<_> = evidence.quote.chars().collect();
        if quote.is_empty() || quote.len() > text.len() {
            continue;
        }
        for start in 0..=text.len() - quote.len() {
            if text[start..start + quote.len()] == quote {
                matches.push((paragraph_index, start, start + quote.len()));
            }
        }
    }
    (matches.len() == 1).then(|| matches[0])
}

fn evidence_contains(evidence: &CanonicalEvidence, mention: &CanonicalMention) -> bool {
    evidence.note_id == mention.note_id
        && evidence.paragraph_index == mention.paragraph_index
        && evidence.start <= mention.start
        && mention.end <= evidence.end
}

fn is_suppressed(
    snapshot: &resolve::ResolverSnapshot,
    people: &store::Voiceprints,
    subject_id: &str,
    predicate: &RelationPredicate,
    object_id: &str,
) -> bool {
    snapshot
        .relation_decisions
        .suppressed
        .iter()
        .any(|decision| {
            let decision_subject =
                resolve::resolve_reference_id(snapshot, people, &decision.subject_id).entity_id;
            let decision_object =
                resolve::resolve_reference_id(snapshot, people, &decision.object_id).entity_id;
            decision_subject.as_deref() == Some(subject_id)
                && decision.predicate == *predicate
                && decision_object.as_deref() == Some(object_id)
        })
}

fn canonical_relation_id(
    subject_id: &str,
    predicate: &RelationPredicate,
    object_id: &str,
    valid_from: Option<&str>,
    valid_to: Option<&str>,
) -> String {
    store::stable_id(
        "cr_",
        &[
            subject_id.into(),
            predicate.kind.clone(),
            predicate.label.clone().unwrap_or_default(),
            object_id.into(),
            valid_from.unwrap_or_default().into(),
            valid_to.unwrap_or_default().into(),
        ],
    )
}

fn relation_status(valid_to: Option<&str>, now: DateTime<FixedOffset>) -> RelationStatus {
    valid_to
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .filter(|valid_to| *valid_to <= now)
        .map_or(RelationStatus::Current, |_| RelationStatus::Historical)
}

fn insert_relation(
    groups: &mut BTreeMap<(RelationKey, String), CanonicalRelation>,
    relation: CanonicalRelation,
) {
    let key = RelationKey {
        subject_id: relation.subject_id.clone(),
        predicate_type: relation.predicate.kind.clone(),
        predicate_label: relation.predicate.label.clone(),
        object_id: relation.object_id.clone(),
    };
    let temporal = format!(
        "{}\0{}",
        relation.valid_from.as_deref().unwrap_or_default(),
        relation.valid_to.as_deref().unwrap_or_default()
    );
    match groups.get_mut(&(key.clone(), temporal.clone())) {
        Some(existing) => {
            existing.confidence = existing.confidence.max(relation.confidence);
            existing.note_ids.extend(relation.note_ids);
            existing.note_ids.sort();
            existing.note_ids.dedup();
            existing.evidence.extend(relation.evidence);
            existing
                .evidence
                .sort_by(|left, right| left.id.cmp(&right.id));
            existing
                .evidence
                .dedup_by(|left, right| left.id == right.id);
            if relation.origin as u8 > existing.origin as u8 {
                existing.origin = relation.origin;
            }
        }
        None => {
            groups.insert((key, temporal), relation);
        }
    }
}

fn project_created_relations(
    snapshot: &resolve::ResolverSnapshot,
    people: &store::Voiceprints,
    evidence_by_id: &BTreeMap<String, CanonicalEvidence>,
    mentions: &BTreeMap<(String, String), CanonicalMention>,
    groups: &mut BTreeMap<(RelationKey, String), CanonicalRelation>,
    pending: &mut Vec<PendingItem>,
    now: DateTime<FixedOffset>,
) {
    for relation in snapshot.relation_decisions.created.values() {
        let subject = resolve::resolve_reference_id(snapshot, people, &relation.subject_id);
        let object = resolve::resolve_reference_id(snapshot, people, &relation.object_id);
        let (Some(subject_id), Some(object_id)) = (subject.entity_id, object.entity_id) else {
            pending.push(PendingItem::IdentityConflict {
                note_id: String::new(),
                local_entity_id: String::new(),
                candidates: subject
                    .candidates
                    .into_iter()
                    .chain(object.candidates)
                    .collect(),
                reason: "created relation references an unresolved entity".into(),
            });
            continue;
        };
        if is_suppressed(
            snapshot,
            people,
            &subject_id,
            &relation.predicate,
            &object_id,
        ) {
            continue;
        }
        let mut evidence: Vec<_> = relation
            .evidence_ids
            .iter()
            .filter_map(|id| evidence_by_id.get(id).cloned())
            .collect();
        if evidence.len() != relation.evidence_ids.len() && !relation.user_assertion {
            pending.push(PendingItem::RelationReview {
                note_id: String::new(),
                relation_id: canonical_relation_id(
                    &subject_id,
                    &relation.predicate,
                    &object_id,
                    relation.valid_from.as_deref(),
                    relation.valid_to.as_deref(),
                ),
            });
            continue;
        }
        for item in &mut evidence {
            item.subject_mentions = mentions
                .values()
                .filter(|mention| {
                    mention.entity_id == subject_id && evidence_contains(item, mention)
                })
                .map(|mention| mention.id.clone())
                .collect();
            item.object_mentions = mentions
                .values()
                .filter(|mention| {
                    mention.entity_id == object_id && evidence_contains(item, mention)
                })
                .map(|mention| mention.id.clone())
                .collect();
        }
        canonicalize_evidence(&mut evidence);
        let id = canonical_relation_id(
            &subject_id,
            &relation.predicate,
            &object_id,
            relation.valid_from.as_deref(),
            relation.valid_to.as_deref(),
        );
        let fact = RelationFact {
            id: id.clone(),
            subject: subject_id.clone(),
            predicate: relation.predicate.clone(),
            object: object_id.clone(),
            subject_mentions: Vec::new(),
            object_mentions: Vec::new(),
            confidence: 1.0,
            valid_from: relation.valid_from.clone(),
            valid_to: relation.valid_to.clone(),
            evidence: evidence
                .iter()
                .map(|item| store::RelationEvidence {
                    id: item.id.clone(),
                    paragraph_index: item.paragraph_index,
                    start: item.start,
                    end: item.end,
                    quote: item.quote.clone(),
                    source_seqs: item.source_seqs.clone(),
                    source_hash: item.source_hash.clone(),
                })
                .collect(),
        };
        match store::aing_graph::publish_tier(&fact, false, false) {
            PublishTier::RawOnly => continue,
            PublishTier::Pending if !relation.user_assertion => {
                pending.push(PendingItem::RelationReview {
                    note_id: evidence
                        .first()
                        .map(|item| item.note_id.clone())
                        .unwrap_or_default(),
                    relation_id: id.clone(),
                });
            }
            PublishTier::Pending | PublishTier::Published => {}
        }
        let mut note_ids: Vec<_> = evidence.iter().map(|item| item.note_id.clone()).collect();
        note_ids.sort();
        note_ids.dedup();
        insert_relation(
            groups,
            CanonicalRelation {
                id,
                subject_id,
                predicate: relation.predicate.clone(),
                object_id,
                confidence: 1.0,
                valid_from: relation.valid_from.clone(),
                valid_to: relation.valid_to.clone(),
                status: relation_status(relation.valid_to.as_deref(), now),
                origin: if relation.user_assertion {
                    RelationOrigin::UserAssertion
                } else {
                    RelationOrigin::Manual
                },
                provider: None,
                model: None,
                note_ids,
                evidence,
            },
        );
    }
}

fn canonicalize_evidence(evidence: &mut Vec<CanonicalEvidence>) {
    for item in evidence.iter_mut() {
        item.source_seqs.sort_unstable();
        item.source_seqs.dedup();
        item.subject_mentions.sort();
        item.subject_mentions.dedup();
        item.object_mentions.sort();
        item.object_mentions.dedup();
    }
    evidence.sort_by(|left, right| left.id.cmp(&right.id));
    evidence.dedup_by(|left, right| left.id == right.id);
}

fn mark_time_conflicts(
    relations: &mut [CanonicalRelation],
    pending: &mut Vec<PendingItem>,
    _now: DateTime<FixedOffset>,
) {
    let mut conflicts = BTreeSet::new();
    for left in 0..relations.len() {
        if relations[left].status != RelationStatus::Current
            || !matches!(
                relations[left].predicate.kind.as_str(),
                "responsible_for" | "assigned_to"
            )
        {
            continue;
        }
        for right in left + 1..relations.len() {
            if relations[right].status == RelationStatus::Current
                && relations[left].subject_id == relations[right].subject_id
                && relations[left].predicate == relations[right].predicate
                && relations[left].object_id != relations[right].object_id
            {
                // The shared Task 2 policy is the final authority for conflict tiering.
                let _ = canonical_publish_tier(&relations[left], true);
                let _ = canonical_publish_tier(&relations[right], true);
                let mut ids = vec![relations[left].id.clone(), relations[right].id.clone()];
                ids.sort();
                conflicts.insert(ids);
            }
        }
    }
    pending.extend(
        conflicts
            .into_iter()
            .map(|relation_ids| PendingItem::TimeConflict { relation_ids }),
    );
}

fn canonical_publish_tier(relation: &CanonicalRelation, time_conflict: bool) -> PublishTier {
    let fact = RelationFact {
        id: relation.id.clone(),
        subject: relation.subject_id.clone(),
        predicate: relation.predicate.clone(),
        object: relation.object_id.clone(),
        subject_mentions: relation
            .evidence
            .iter()
            .flat_map(|evidence| evidence.subject_mentions.clone())
            .collect(),
        object_mentions: relation
            .evidence
            .iter()
            .flat_map(|evidence| evidence.object_mentions.clone())
            .collect(),
        confidence: relation.confidence,
        valid_from: relation.valid_from.clone(),
        valid_to: relation.valid_to.clone(),
        evidence: relation
            .evidence
            .iter()
            .map(|evidence| store::RelationEvidence {
                id: evidence.id.clone(),
                paragraph_index: evidence.paragraph_index,
                start: evidence.start,
                end: evidence.end,
                quote: evidence.quote.clone(),
                source_seqs: evidence.source_seqs.clone(),
                source_hash: evidence.source_hash.clone(),
            })
            .collect(),
    };
    store::aing_graph::publish_tier(&fact, false, time_conflict)
}

struct ScannedDocument {
    note_id: String,
    doc: RefinedDoc,
}

struct InvalidDocument {
    note_id: String,
    message: String,
}

fn scan_documents(data_root: &Path) -> Vec<Result<ScannedDocument, InvalidDocument>> {
    let notes = data_root.join("notes");
    let mut paths: Vec<PathBuf> = match std::fs::read_dir(&notes) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path().join(store::AING_DOC_FILE))
            .filter(|path| path.is_file())
            .collect(),
        Err(_) => Vec::new(),
    };
    paths.sort_by(|left, right| {
        note_id_for_path(left)
            .unwrap_or_default()
            .cmp(&note_id_for_path(right).unwrap_or_default())
    });
    paths
        .into_iter()
        .map(|path| {
            let note_id = note_id_for_path(&path).unwrap_or_default();
            if let Err(error) = store::validate_note_id(&note_id) {
                return Err(InvalidDocument {
                    note_id,
                    message: error.to_string(),
                });
            }
            let bytes = std::fs::read(&path).map_err(|error| InvalidDocument {
                note_id: note_id.clone(),
                message: error.to_string(),
            })?;
            let mut doc: RefinedDoc =
                serde_json::from_slice(&bytes).map_err(|error| InvalidDocument {
                    note_id: note_id.clone(),
                    message: error.to_string(),
                })?;
            store::ensure_graph_ids(&note_id, &mut doc);
            Ok(ScannedDocument { note_id, doc })
        })
        .collect()
}

fn note_id_for_path(path: &Path) -> Option<String> {
    path.parent()?.file_name()?.to_str().map(str::to_string)
}

fn local_key(note_id: &str, local_id: &str) -> String {
    format!("{note_id}/{local_id}")
}

fn is_person_id(id: &str) -> bool {
    id.strip_prefix('P').is_some_and(|suffix| {
        !suffix.is_empty() && suffix.chars().all(|char| char.is_ascii_digit())
    })
}

fn registry_matches(
    registry: &BTreeMap<String, RegistryEntity>,
    local: &store::Entity,
) -> Vec<String> {
    let mut local_names = BTreeSet::from([normalize(&local.name)]);
    local_names.extend(local.aliases.iter().map(|alias| normalize(alias)));
    registry
        .iter()
        .filter(|(_, entity)| entity.status == "confirmed" && entity.kind == local.kind)
        .filter(|(_, entity)| {
            std::iter::once(&entity.name)
                .chain(entity.aliases.iter())
                .map(|name| normalize(name))
                .any(|name| local_names.contains(&name))
        })
        .map(|(id, _)| id.clone())
        .collect()
}

fn normalize(value: &str) -> String {
    value.trim().to_lowercase()
}

fn char_slice(text: &str, start: usize, end: usize) -> Option<String> {
    let chars: Vec<_> = text.chars().collect();
    (start < end && end <= chars.len()).then(|| chars[start..end].iter().collect())
}

fn pending_sort_key(item: &PendingItem) -> String {
    serde_json::to_string(item).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{
        ensure_graph_ids, write_refined_atomic, Entity, GraphExtraction, Mention, RefineStages,
        RefinedDoc, RefinedParagraph, RelationEvidence, RelationFact,
    };
    use chrono::DateTime;

    fn doc(entities: Vec<Entity>, text: &str, mentions: Vec<Mention>) -> RefinedDoc {
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
            discarded_seqs: Vec::new(),
            entities,
            graph_extraction: Some(GraphExtraction {
                contract_version: 1,
                provider: "fixture".into(),
                model: "fixture-v1".into(),
                run_id: "run-1".into(),
                generated_at: "2026-07-21T00:00:00Z".into(),
                source_hash: String::new(),
                mode: "done".into(),
            }),
            relations: Vec::new(),
            paragraphs: vec![RefinedParagraph {
                speaker: "S1".into(),
                name: None,
                person_id: None,
                start_ms: 0,
                end_ms: 1,
                text: text.into(),
                source_seqs: vec![1],
                mentions,
            }],
        }
    }

    fn entity(id: &str, kind: &str, name: &str, aliases: &[&str]) -> Entity {
        Entity {
            id: id.into(),
            kind: kind.into(),
            name: name.into(),
            aliases: aliases.iter().map(|value| (*value).into()).collect(),
        }
    }

    fn mention(entity: &str, start: usize, end: usize) -> Mention {
        Mention {
            id: String::new(),
            entity: entity.into(),
            start,
            end,
        }
    }

    fn write_note(root: &Path, note_id: &str, doc: &RefinedDoc) {
        let note_dir = root.join("notes").join(note_id);
        std::fs::create_dir_all(&note_dir).unwrap();
        write_refined_atomic(&note_dir, doc).unwrap();
    }

    fn relation_doc(note_id: &str, confidence: f64) -> RefinedDoc {
        let mut document = doc(
            vec![
                entity("P1", "person", "Alice", &[]),
                entity("ent_project", "project", "Apollo", &[]),
            ],
            "Alice owns Apollo",
            vec![mention("P1", 0, 5), mention("ent_project", 11, 17)],
        );
        ensure_graph_ids(note_id, &mut document);
        document.relations.push(RelationFact {
            id: String::new(),
            subject: "P1".into(),
            predicate: RelationPredicate {
                kind: "responsible_for".into(),
                label: None,
            },
            object: "ent_project".into(),
            subject_mentions: vec![document.paragraphs[0].mentions[0].id.clone()],
            object_mentions: vec![document.paragraphs[0].mentions[1].id.clone()],
            confidence,
            valid_from: None,
            valid_to: None,
            evidence: vec![RelationEvidence {
                id: String::new(),
                paragraph_index: 0,
                start: 0,
                end: 17,
                quote: "Alice owns Apollo".into(),
                source_seqs: vec![1],
                source_hash: String::new(),
            }],
        });
        ensure_graph_ids(note_id, &mut document);
        document
    }

    fn split_relation_doc(note_id: &str) -> RefinedDoc {
        let mut document = doc(
            vec![
                entity("ent_team", "project", "Team", &[]),
                entity("ent_other", "project", "Other Team", &[]),
                entity("ent_product", "term", "App", &[]),
            ],
            "Team owns App Team owns App",
            vec![
                mention("ent_team", 0, 4),
                mention("ent_product", 10, 13),
                mention("ent_team", 14, 18),
                mention("ent_product", 24, 27),
            ],
        );
        ensure_graph_ids(note_id, &mut document);
        document.relations.push(RelationFact {
            id: String::new(),
            subject: "ent_team".into(),
            predicate: RelationPredicate {
                kind: "responsible_for".into(),
                label: None,
            },
            object: "ent_product".into(),
            subject_mentions: vec![
                document.paragraphs[0].mentions[0].id.clone(),
                document.paragraphs[0].mentions[2].id.clone(),
            ],
            object_mentions: vec![
                document.paragraphs[0].mentions[1].id.clone(),
                document.paragraphs[0].mentions[3].id.clone(),
            ],
            confidence: 0.9,
            valid_from: None,
            valid_to: None,
            evidence: vec![
                RelationEvidence {
                    id: String::new(),
                    paragraph_index: 0,
                    start: 0,
                    end: 13,
                    quote: "Team owns App".into(),
                    source_seqs: vec![1],
                    source_hash: String::new(),
                },
                RelationEvidence {
                    id: String::new(),
                    paragraph_index: 0,
                    start: 14,
                    end: 27,
                    quote: "Team owns App".into(),
                    source_seqs: vec![1],
                    source_hash: String::new(),
                },
            ],
        });
        ensure_graph_ids(note_id, &mut document);
        document
    }

    #[test]
    fn empty_library_builds_a_deterministic_empty_graph() {
        let root = tempfile::tempdir().unwrap();
        let ledger = reconcile_registry(root.path()).unwrap();
        let graph = build_canonical_graph(
            root.path(),
            &ledger,
            DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();

        assert!(graph.entities.is_empty());
        assert!(graph.mentions.is_empty());
        assert!(graph.relations.is_empty());
        assert!(graph.pending.is_empty());
    }

    #[test]
    fn registry_and_projection_are_independent_of_directory_creation_order() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let alpha = doc(
            vec![
                entity("ent_project", "project", "Apollo", &["Moonshot"]),
                entity("ent_org", "org", "Apollo", &[]),
            ],
            "Apollo",
            vec![mention("ent_project", 0, 6)],
        );
        let beta = doc(
            vec![entity("ent_alias", "project", "Moonshot", &[])],
            "Moonshot",
            vec![mention("ent_alias", 0, 8)],
        );
        write_note(first.path(), "n2", &beta);
        write_note(first.path(), "n1", &alpha);
        write_note(second.path(), "n1", &alpha);
        write_note(second.path(), "n2", &beta);

        let first_ledger = reconcile_registry(first.path()).unwrap();
        let second_ledger = reconcile_registry(second.path()).unwrap();
        assert_eq!(first_ledger.registry, second_ledger.registry);
        assert_eq!(first_ledger.legacy_ids, second_ledger.legacy_ids);

        let now = DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap();
        let first_graph = build_canonical_graph(first.path(), &first_ledger, now).unwrap();
        let second_graph = build_canonical_graph(second.path(), &second_ledger, now).unwrap();
        assert_eq!(
            serde_json::to_value(&first_graph).unwrap(),
            serde_json::to_value(&second_graph).unwrap()
        );
        assert_eq!(first_graph.entities.len(), 3);
        assert_eq!(first_graph.mentions.len(), 2);
        assert_eq!(
            first_graph
                .mentions
                .iter()
                .map(|mention| mention.entity_id.as_str())
                .collect::<BTreeSet<_>>()
                .len(),
            2,
            "unconfirmed model entities are not auto-merged by an alias"
        );
        assert!(first_graph.pending.is_empty());
    }

    #[test]
    fn confirmed_alias_matches_exactly_within_the_same_kind() {
        let root = tempfile::tempdir().unwrap();
        let document = doc(
            vec![entity("ent_alias", "project", "Moonshot", &[])],
            "Moonshot",
            vec![mention("ent_alias", 0, 8)],
        );
        write_note(root.path(), "n1", &document);
        let mut ledger = KnowledgeLedger::empty();
        ledger.registry.insert(
            "kg_project".into(),
            RegistryEntity {
                kind: "project".into(),
                name: "Apollo".into(),
                aliases: vec!["Moonshot".into()],
                status: "confirmed".into(),
            },
        );
        ledger.registry.insert(
            "kg_org".into(),
            RegistryEntity {
                kind: "org".into(),
                name: "Moonshot".into(),
                aliases: Vec::new(),
                status: "confirmed".into(),
            },
        );

        let graph = build_canonical_graph(
            root.path(),
            &ledger,
            DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();
        assert_eq!(graph.mentions[0].entity_id, "kg_project");
    }

    #[test]
    fn ambiguous_confirmed_match_stays_pending_without_allocating_a_bogus_entity() {
        let root = tempfile::tempdir().unwrap();
        write_note(
            root.path(),
            "n1",
            &doc(
                vec![entity("ent_1", "project", "Apollo", &[])],
                "Apollo",
                vec![mention("ent_1", 0, 6)],
            ),
        );
        let mut ledger = KnowledgeLedger::empty();
        for id in ["kg_a", "kg_b"] {
            ledger.registry.insert(
                id.into(),
                RegistryEntity {
                    kind: "project".into(),
                    name: "Apollo".into(),
                    aliases: Vec::new(),
                    status: "confirmed".into(),
                },
            );
        }
        std::fs::write(
            root.path().join(super::super::overrides::KNOWLEDGE_FILE),
            serde_json::to_vec(&ledger).unwrap(),
        )
        .unwrap();

        let ledger = reconcile_registry(root.path()).unwrap();
        let graph = build_canonical_graph(
            root.path(),
            &ledger,
            DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();
        assert_eq!(graph.entities.len(), 2);
        assert!(graph.mentions.is_empty());
        assert!(graph.pending.iter().any(
            |item| matches!(item, PendingItem::IdentityConflict { candidates, .. } if candidates == &["kg_a", "kg_b"])
        ));
    }

    #[test]
    fn relation_tiering_and_suppression_use_the_stable_triple_not_evidence_ids() {
        let root = tempfile::tempdir().unwrap();
        let high = relation_doc("n1", 0.9);
        let source_relation_id = high.relations[0].id.clone();
        write_note(root.path(), "n1", &high);
        let mut ledger = reconcile_registry(root.path()).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap();

        let graph = build_canonical_graph(root.path(), &ledger, now).unwrap();
        assert_eq!(graph.relations.len(), 1);
        assert!(graph.pending.is_empty());
        assert_eq!(graph.relations[0].origin, RelationOrigin::Model);

        let subject_id = graph.relations[0].subject_id.clone();
        let object_id = graph.relations[0].object_id.clone();
        ledger.operations.push(KnowledgeOperation {
            id: "op_suppress".into(),
            at: "2026-07-21T01:00:00Z".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::SuppressRelation {
                subject_id,
                predicate: RelationPredicate {
                    kind: "responsible_for".into(),
                    label: None,
                },
                object_id,
            },
        });

        let mut rerun = high;
        rerun.relations[0].id = source_relation_id;
        rerun.relations[0].evidence[0].id = "ev_new_model_run".into();
        write_note(root.path(), "n1", &rerun);
        let suppressed = build_canonical_graph(root.path(), &ledger, now).unwrap();
        assert!(suppressed.relations.is_empty());
        assert!(suppressed.pending.is_empty());
    }

    #[test]
    fn publish_tier_keeps_pending_but_drops_raw_only() {
        let root = tempfile::tempdir().unwrap();
        write_note(root.path(), "n1", &relation_doc("n1", 0.6));
        write_note(root.path(), "n2", &relation_doc("n2", 0.4));
        let ledger = reconcile_registry(root.path()).unwrap();
        let graph = build_canonical_graph(
            root.path(),
            &ledger,
            DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();

        assert_eq!(graph.relations.len(), 1);
        assert!(
            matches!(graph.pending.as_slice(), [PendingItem::RelationReview { note_id, .. }] if note_id == "n1")
        );
    }

    #[test]
    fn selected_mention_binding_splits_evidence_and_undo_rejoins_it() {
        let root = tempfile::tempdir().unwrap();
        let document = split_relation_doc("n1");
        let rebound_mention = document.paragraphs[0].mentions[2].id.clone();
        write_note(root.path(), "n1", &document);
        let mut ledger = reconcile_registry(root.path()).unwrap();
        let other_id = ledger.legacy_ids["n1/ent_other"].clone();
        ledger.operations.push(KnowledgeOperation {
            id: "op_bind_split".into(),
            at: "2026-07-21T01:00:00Z".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::String(other_id),
            action: KnowledgeAction::BindMention {
                mention_id: rebound_mention,
                entity_id: ledger.legacy_ids["n1/ent_other"].clone(),
            },
        });
        let now = DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap();

        let split = build_canonical_graph(root.path(), &ledger, now).unwrap();
        assert_eq!(split.relations.len(), 2);
        assert!(split
            .relations
            .iter()
            .all(|relation| relation.evidence.len() == 1));

        ledger.operations.push(KnowledgeOperation {
            id: "op_undo_split".into(),
            at: "2026-07-21T02:00:00Z".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::Undo {
                operation_id: "op_bind_split".into(),
            },
        });
        let joined = build_canonical_graph(root.path(), &ledger, now).unwrap();
        assert_eq!(joined.relations.len(), 1);
        assert_eq!(joined.relations[0].evidence.len(), 2);
    }

    #[test]
    fn relation_decisions_confirm_edit_suppress_restore_and_end_in_file_order() {
        let root = tempfile::tempdir().unwrap();
        let document = relation_doc("n1", 0.9);
        let source_id = document.relations[0].id.clone();
        write_note(root.path(), "n1", &document);
        let base = reconcile_registry(root.path()).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap();
        let initial = build_canonical_graph(root.path(), &base, now).unwrap();
        let original = &initial.relations[0];

        let mut confirmed = base.clone();
        confirmed.operations.push(KnowledgeOperation {
            id: "op_confirm".into(),
            at: "t1".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::ConfirmRelation {
                relation_id: source_id.clone(),
            },
        });
        assert_eq!(
            build_canonical_graph(root.path(), &confirmed, now)
                .unwrap()
                .relations[0]
                .origin,
            RelationOrigin::Confirmed
        );

        let edited_predicate = RelationPredicate {
            kind: "uses".into(),
            label: None,
        };
        confirmed.operations.push(KnowledgeOperation {
            id: "op_edit".into(),
            at: "t2".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::EditRelation {
                relation_id: source_id.clone(),
                subject_id: original.object_id.clone(),
                predicate: edited_predicate.clone(),
                object_id: original.subject_id.clone(),
                valid_from: Some("2026-07-01T00:00:00+08:00".into()),
                valid_to: None,
                note: Some("direction corrected".into()),
            },
        });
        let edited = build_canonical_graph(root.path(), &confirmed, now).unwrap();
        assert_eq!(edited.relations[0].origin, RelationOrigin::Manual);
        assert_eq!(edited.relations[0].predicate, edited_predicate);
        assert_eq!(edited.relations[0].subject_id, original.object_id);
        assert_eq!(
            edited.relations[0].evidence[0].subject_mentions,
            original.evidence[0].object_mentions
        );
        assert_eq!(
            edited.relations[0].evidence[0].object_mentions,
            original.evidence[0].subject_mentions
        );

        confirmed.operations.push(KnowledgeOperation {
            id: "op_suppress_edit".into(),
            at: "t3".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::SuppressRelation {
                subject_id: original.object_id.clone(),
                predicate: edited_predicate,
                object_id: original.subject_id.clone(),
            },
        });
        assert!(build_canonical_graph(root.path(), &confirmed, now)
            .unwrap()
            .relations
            .is_empty());
        confirmed.operations.push(KnowledgeOperation {
            id: "op_restore".into(),
            at: "t4".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::RestoreRelation {
                operation_id: "op_suppress_edit".into(),
            },
        });
        assert_eq!(
            build_canonical_graph(root.path(), &confirmed, now)
                .unwrap()
                .relations
                .len(),
            1
        );

        let ended_at = "2026-07-20T00:00:00+08:00";
        let mut ended = base;
        ended.operations.push(KnowledgeOperation {
            id: "op_end".into(),
            at: "t5".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::EndRelation {
                relation_id: source_id,
                valid_to: ended_at.into(),
            },
        });
        let ended = build_canonical_graph(root.path(), &ended, now).unwrap();
        assert_eq!(ended.relations[0].status, RelationStatus::Historical);
        assert_eq!(
            ended.relations[0].id,
            canonical_relation_id(
                &ended.relations[0].subject_id,
                &ended.relations[0].predicate,
                &ended.relations[0].object_id,
                None,
                Some(ended_at)
            )
        );
    }

    #[test]
    fn user_assertions_are_canonical_and_overlapping_exclusive_relations_are_pending() {
        let root = tempfile::tempdir().unwrap();
        write_note(root.path(), "n1", &relation_doc("n1", 0.4));
        let mut ledger = reconcile_registry(root.path()).unwrap();
        let person_id = ledger.legacy_ids["n1/P1"].clone();
        let project_id = ledger.legacy_ids["n1/ent_project"].clone();
        for (id, object_id) in [("op_user_1", project_id), ("op_user_2", person_id.clone())] {
            ledger.operations.push(KnowledgeOperation {
                id: id.into(),
                at: "2026-07-21T00:00:00Z".into(),
                before: serde_json::Value::Null,
                after: serde_json::Value::Null,
                action: KnowledgeAction::CreateRelation {
                    relation: super::super::overrides::UserRelation {
                        subject_id: person_id.clone(),
                        predicate: RelationPredicate {
                            kind: "responsible_for".into(),
                            label: None,
                        },
                        object_id,
                        valid_from: None,
                        valid_to: None,
                        note: Some("explicit assertion".into()),
                        evidence_ids: Vec::new(),
                        user_assertion: true,
                    },
                },
            });
        }

        let graph = build_canonical_graph(
            root.path(),
            &ledger,
            DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();
        assert_eq!(graph.relations.len(), 2);
        assert!(graph
            .relations
            .iter()
            .all(|relation| relation.origin == RelationOrigin::UserAssertion));
        assert!(graph
            .pending
            .iter()
            .any(|item| matches!(item, PendingItem::TimeConflict { .. })));
    }

    #[test]
    fn user_created_relation_can_promote_evidence_from_a_raw_only_model_fact() {
        let root = tempfile::tempdir().unwrap();
        let raw = relation_doc("n1", 0.4);
        let evidence_id = raw.relations[0].evidence[0].id.clone();
        write_note(root.path(), "n1", &raw);
        let mut ledger = reconcile_registry(root.path()).unwrap();
        let person_id = ledger.legacy_ids["n1/P1"].clone();
        let project_id = ledger.legacy_ids["n1/ent_project"].clone();
        ledger.operations.push(KnowledgeOperation {
            id: "op_evidenced_relation".into(),
            at: "2026-07-21T00:00:00Z".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::CreateRelation {
                relation: super::super::overrides::UserRelation {
                    subject_id: project_id,
                    predicate: RelationPredicate {
                        kind: "uses".into(),
                        label: None,
                    },
                    object_id: person_id,
                    valid_from: None,
                    valid_to: None,
                    note: None,
                    evidence_ids: vec![evidence_id],
                    user_assertion: false,
                },
            },
        });

        let graph = build_canonical_graph(
            root.path(),
            &ledger,
            DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();
        assert_eq!(graph.relations.len(), 1);
        assert_eq!(graph.relations[0].origin, RelationOrigin::Manual);
        assert_eq!(graph.relations[0].evidence.len(), 1);
        assert_eq!(graph.relations[0].evidence[0].subject_mentions.len(), 1);
        assert_eq!(graph.relations[0].evidence[0].object_mentions.len(), 1);
        assert!(graph.pending.is_empty());
    }

    #[test]
    fn stale_evidence_relocates_only_when_the_provenance_quote_is_unique() {
        let root = tempfile::tempdir().unwrap();
        let mut moved = relation_doc("n1", 0.9);
        moved.relations[0].evidence[0].paragraph_index = 99;
        moved.relations[0].evidence[0].start = 99;
        moved.relations[0].evidence[0].end = 116;
        write_note(root.path(), "n1", &moved);
        let ledger = reconcile_registry(root.path()).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap();

        let relocated = build_canonical_graph(root.path(), &ledger, now).unwrap();
        assert_eq!(relocated.relations.len(), 1);
        assert_eq!(relocated.relations[0].evidence[0].paragraph_index, 0);
        assert_eq!(relocated.relations[0].evidence[0].start, 0);

        moved.paragraphs[0].text = "Alice owns Apollo Alice owns Apollo".into();
        write_note(root.path(), "n1", &moved);
        let ambiguous = build_canonical_graph(root.path(), &ledger, now).unwrap();
        assert!(ambiguous.relations.is_empty());
        assert!(ambiguous
            .pending
            .iter()
            .any(|item| matches!(item, PendingItem::StaleEvidence { .. })));

        moved.paragraphs[0].text = "Alice owns Apollo".into();
        moved.relations[0].evidence[0].source_hash = "different-source".into();
        write_note(root.path(), "n1", &moved);
        let wrong_source = build_canonical_graph(root.path(), &ledger, now).unwrap();
        assert!(wrong_source.relations.is_empty());
        assert!(wrong_source
            .pending
            .iter()
            .any(|item| matches!(item, PendingItem::StaleEvidence { .. })));
    }

    #[test]
    fn unreadable_aing_document_is_visible_as_pending_without_blocking_valid_notes() {
        let root = tempfile::tempdir().unwrap();
        write_note(root.path(), "good", &relation_doc("good", 0.9));
        let broken_dir = root.path().join("notes").join("broken");
        std::fs::create_dir_all(&broken_dir).unwrap();
        std::fs::write(broken_dir.join(crate::store::AING_DOC_FILE), b"{not-json").unwrap();

        let ledger = reconcile_registry(root.path()).unwrap();
        let graph = build_canonical_graph(
            root.path(),
            &ledger,
            DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();
        assert_eq!(graph.relations.len(), 1);
        assert!(graph.pending.iter().any(
            |item| matches!(item, PendingItem::InvalidDocument { note_id, .. } if note_id == "broken")
        ));
    }

    #[test]
    fn multi_hop_merge_projects_only_the_terminal_entity() {
        let root = tempfile::tempdir().unwrap();
        write_note(
            root.path(),
            "n1",
            &doc(
                vec![entity("ent_source", "project", "Old", &[])],
                "Old",
                vec![mention("ent_source", 0, 3)],
            ),
        );
        let mut ledger = KnowledgeLedger::empty();
        for (id, name) in [
            ("kg_source", "Old"),
            ("kg_mid", "Middle"),
            ("kg_target", "New"),
        ] {
            ledger.registry.insert(
                id.into(),
                RegistryEntity {
                    kind: "project".into(),
                    name: name.into(),
                    aliases: Vec::new(),
                    status: "confirmed".into(),
                },
            );
        }
        ledger
            .legacy_ids
            .insert("n1/ent_source".into(), "kg_source".into());
        for (id, source, target) in [
            ("op_merge_1", "kg_source", "kg_mid"),
            ("op_merge_2", "kg_mid", "kg_target"),
        ] {
            ledger.operations.push(KnowledgeOperation {
                id: id.into(),
                at: "2026-07-21T00:00:00Z".into(),
                before: serde_json::Value::Null,
                after: serde_json::Value::String(target.into()),
                action: KnowledgeAction::MergeEntity {
                    source_id: source.into(),
                    target_id: target.into(),
                },
            });
        }
        ledger.operations.push(KnowledgeOperation {
            id: "op_relation_before_merge".into(),
            at: "2026-07-21T01:00:00Z".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::CreateRelation {
                relation: super::super::overrides::UserRelation {
                    subject_id: "kg_source".into(),
                    predicate: RelationPredicate {
                        kind: "uses".into(),
                        label: None,
                    },
                    object_id: "kg_target".into(),
                    valid_from: None,
                    valid_to: None,
                    note: None,
                    evidence_ids: Vec::new(),
                    user_assertion: true,
                },
            },
        });

        let graph = build_canonical_graph(
            root.path(),
            &ledger,
            DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();
        assert_eq!(graph.mentions[0].entity_id, "kg_target");
        assert_eq!(
            graph.entities.keys().collect::<Vec<_>>(),
            vec![&"kg_target".to_string()]
        );
        assert_eq!(graph.relations[0].subject_id, "kg_target");
    }
}
