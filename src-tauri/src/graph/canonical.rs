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

struct ModelRelationGroup {
    relation: CanonicalRelation,
    raw_ids: BTreeSet<String>,
}

pub fn reconcile_registry(data_root: &Path) -> anyhow::Result<KnowledgeLedger> {
    overrides::update(data_root, |ledger| {
        let documents = scan_documents(data_root)?;
        for document in documents.into_iter().filter_map(|document| document.ok()) {
            for local in &document.doc.entities {
                if local.name.trim().is_empty() {
                    continue;
                }
                let legacy_key = local_key(&document.note_id, &local.id);
                if ledger.legacy_ids.contains_key(&legacy_key) {
                    continue;
                }
                let candidates = resolve::confirmed_registry_matches(&ledger.registry, local);
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
    now: DateTime<FixedOffset>,
) -> anyhow::Result<CanonicalGraph> {
    let snapshot = resolve::replay(ledger)?;
    let people = VoiceprintStore::new(data_root.to_path_buf()).load();
    let documents = scan_documents(data_root)?;
    let mut entities = BTreeMap::new();
    let mut pending = Vec::new();
    for (id, entity) in &snapshot.registry {
        let resolution = resolve::resolve_reference_id(&snapshot, &people, id);
        let Some(canonical_id) = resolution.entity_id else {
            pending.push(PendingItem::IdentityConflict {
                note_id: String::new(),
                local_entity_id: id.clone(),
                candidates: resolution.candidates,
                reason: resolution
                    .reason
                    .unwrap_or_else(|| "registry redirect conflict".into()),
            });
            continue;
        };
        if canonical_id != *id {
            if snapshot.registry.contains_key(&canonical_id) {
                continue;
            }
            if let Some(person) = people.people.get(&canonical_id) {
                entities
                    .entry(canonical_id.clone())
                    .or_insert_with(|| CanonicalEntity {
                        id: canonical_id,
                        kind: "person".into(),
                        name: person.name.clone(),
                        aliases: Vec::new(),
                        confirmed: true,
                    });
                continue;
            }
        }
        entities.insert(
            canonical_id.clone(),
            CanonicalEntity {
                id: canonical_id,
                kind: entity.kind.clone(),
                name: entity.name.clone(),
                aliases: entity.aliases.clone(),
                confirmed: entity.status == "confirmed",
            },
        );
    }
    let mut mentions = Vec::new();
    for document in &documents {
        let document = match document {
            Ok(document) => document,
            Err(invalid) => {
                pending.push(PendingItem::InvalidDocument {
                    note_id: invalid.note_id.clone(),
                    message: invalid.message.clone(),
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
                if !entities.contains_key(&entity_id) {
                    if let Some(person) = people.people.get(&entity_id) {
                        entities.insert(
                            entity_id.clone(),
                            CanonicalEntity {
                                id: entity_id.clone(),
                                kind: "person".into(),
                                name: person.name.clone(),
                                aliases: Vec::new(),
                                confirmed: true,
                            },
                        );
                    } else {
                        pending.push(PendingItem::IdentityConflict {
                            note_id: document.note_id.clone(),
                            local_entity_id: local.id.clone(),
                            candidates: vec![entity_id],
                            reason: "resolved mention target is absent from the canonical registry"
                                .into(),
                        });
                        continue;
                    }
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
    let mut model_groups: BTreeMap<(RelationKey, String), ModelRelationGroup> = BTreeMap::new();
    let mut relation_groups: BTreeMap<(RelationKey, String), CanonicalRelation> = BTreeMap::new();
    let evidence_catalog = collect_evidence_catalog(&documents);
    for document in documents
        .iter()
        .filter_map(|document| document.as_ref().ok())
    {
        for relation in &document.doc.relations {
            project_model_relation(
                &document,
                relation,
                &mention_index,
                &mut model_groups,
                &mut pending,
                now,
            );
        }
    }
    project_model_groups(
        model_groups,
        &snapshot,
        &people,
        &mention_index,
        &mut relation_groups,
        &mut pending,
        now,
    );
    project_created_relations(
        &snapshot,
        &people,
        &evidence_catalog,
        &mention_index,
        &mut relation_groups,
        &mut pending,
        now,
    );
    let mut candidates = Vec::new();
    for mut relation in relation_groups.into_values() {
        if let Err(item) =
            prepare_final_relation(&mut relation, &mention_index, &mut entities, &people)
        {
            pending.push(item);
            continue;
        }
        candidates.push(relation);
    }
    let (conflicting_ids, time_pending) = find_time_conflicts(&candidates);
    pending.extend(time_pending);
    let mut relations = Vec::new();
    for relation in candidates {
        let time_conflict = conflicting_ids.contains(&relation.id);
        let tier = canonical_publish_tier(&relation, time_conflict);
        let human_decision = matches!(
            relation.origin,
            RelationOrigin::Confirmed | RelationOrigin::Manual | RelationOrigin::UserAssertion
        );
        let tier = if human_decision && !time_conflict && tier == PublishTier::Pending {
            PublishTier::Published
        } else {
            tier
        };
        match tier {
            PublishTier::RawOnly => continue,
            PublishTier::Pending if !time_conflict => {
                pending.push(PendingItem::RelationReview {
                    note_id: relation.note_ids.first().cloned().unwrap_or_default(),
                    relation_id: relation.id.clone(),
                });
            }
            PublishTier::Pending | PublishTier::Published => {}
        }
        relations.push(relation);
    }
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
    mentions: &BTreeMap<(String, String), CanonicalMention>,
    groups: &mut BTreeMap<(RelationKey, String), ModelRelationGroup>,
    pending: &mut Vec<PendingItem>,
    now: DateTime<FixedOffset>,
) {
    if source.confidence < 0.5 {
        return;
    }
    let resolved = resolve_evidence(document, source);
    for evidence_id in resolved.stale {
        pending.push(PendingItem::StaleEvidence {
            note_id: document.note_id.clone(),
            relation_id: source.id.clone(),
            evidence_id,
        });
    }
    if resolved.valid.is_empty() {
        return;
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
    for evidence in resolved.valid {
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

    for ((subject_id, object_id), mut evidence) in split {
        let predicate = match normalized_predicate(&source.predicate) {
            Ok(predicate) => predicate,
            Err(()) => {
                pending.push(PendingItem::RelationReview {
                    note_id: document.note_id.clone(),
                    relation_id: source.id.clone(),
                });
                continue;
            }
        };
        canonicalize_evidence(&mut evidence);
        let relation_id = canonical_relation_id(
            &subject_id,
            &predicate,
            &object_id,
            source.valid_from.as_deref(),
            source.valid_to.as_deref(),
        );
        let extraction = document.doc.graph_extraction.as_ref();
        insert_model_relation(
            groups,
            CanonicalRelation {
                id: relation_id,
                subject_id,
                predicate,
                object_id,
                confidence: source.confidence,
                valid_from: source.valid_from.clone(),
                valid_to: source.valid_to.clone(),
                status: relation_status(source.valid_to.as_deref(), now),
                origin: RelationOrigin::Model,
                provider: extraction.map(|value| value.provider.clone()),
                model: extraction.map(|value| value.model.clone()),
                note_ids: vec![document.note_id.clone()],
                evidence,
            },
            source.id.clone(),
        );
    }
}

struct EvidenceResolution {
    valid: Vec<CanonicalEvidence>,
    stale: Vec<String>,
}

fn resolve_evidence(document: &ScannedDocument, relation: &RelationFact) -> EvidenceResolution {
    let mut valid = Vec::new();
    let mut stale = Vec::new();
    let current_source_hash = store::source_hash(&document.doc.paragraphs);
    for evidence in &relation.evidence {
        let location = document
            .doc
            .paragraphs
            .get(evidence.paragraph_index)
            .and_then(|paragraph| {
                (char_slice(&paragraph.text, evidence.start, evidence.end).as_deref()
                    == Some(evidence.quote.as_str())
                    && evidence.source_hash == current_source_hash
                    && evidence
                        .source_seqs
                        .iter()
                        .all(|seq| paragraph.source_seqs.contains(seq)))
                .then_some((evidence.paragraph_index, evidence.start, evidence.end))
            })
            .or_else(|| unique_evidence_location(&document.doc, evidence));
        let Some((paragraph_index, start, end)) = location else {
            stale.push(evidence.id.clone());
            continue;
        };
        valid.push(CanonicalEvidence {
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
    EvidenceResolution { valid, stale }
}

fn collect_evidence_catalog(
    documents: &[Result<ScannedDocument, InvalidDocument>],
) -> BTreeMap<String, CanonicalEvidence> {
    let mut catalog = BTreeMap::new();
    for document in documents
        .iter()
        .filter_map(|document| document.as_ref().ok())
    {
        for relation in &document.doc.relations {
            for evidence in resolve_evidence(document, relation).valid {
                catalog.entry(evidence.id.clone()).or_insert(evidence);
            }
        }
    }
    catalog
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
                && normalized_predicate(&decision.predicate).as_ref() == Ok(predicate)
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
    let (key, temporal) = relation_group_key(&relation);
    match groups.get_mut(&(key.clone(), temporal.clone())) {
        Some(existing) => merge_relation(existing, relation),
        None => {
            groups.insert((key, temporal), relation);
        }
    }
}

fn insert_model_relation(
    groups: &mut BTreeMap<(RelationKey, String), ModelRelationGroup>,
    relation: CanonicalRelation,
    raw_id: String,
) {
    let (key, temporal) = relation_group_key(&relation);
    match groups.get_mut(&(key.clone(), temporal.clone())) {
        Some(existing) => {
            merge_relation(&mut existing.relation, relation);
            existing.raw_ids.insert(raw_id);
        }
        None => {
            groups.insert(
                (key, temporal),
                ModelRelationGroup {
                    relation,
                    raw_ids: BTreeSet::from([raw_id]),
                },
            );
        }
    }
}

fn relation_group_key(relation: &CanonicalRelation) -> (RelationKey, String) {
    (
        RelationKey {
            subject_id: relation.subject_id.clone(),
            predicate_type: relation.predicate.kind.clone(),
            predicate_label: relation.predicate.label.clone(),
            object_id: relation.object_id.clone(),
        },
        format!(
            "{}\0{}",
            relation.valid_from.as_deref().unwrap_or_default(),
            relation.valid_to.as_deref().unwrap_or_default()
        ),
    )
}

fn merge_relation(existing: &mut CanonicalRelation, relation: CanonicalRelation) {
    existing.confidence = existing.confidence.max(relation.confidence);
    existing.note_ids.extend(relation.note_ids);
    existing.note_ids.sort();
    existing.note_ids.dedup();
    existing.evidence.extend(relation.evidence);
    canonicalize_evidence(&mut existing.evidence);
    if relation.origin as u8 > existing.origin as u8 {
        existing.origin = relation.origin;
    }
}

fn project_model_groups(
    groups: BTreeMap<(RelationKey, String), ModelRelationGroup>,
    snapshot: &resolve::ResolverSnapshot,
    people: &store::Voiceprints,
    mentions: &BTreeMap<(String, String), CanonicalMention>,
    output: &mut BTreeMap<(RelationKey, String), CanonicalRelation>,
    pending: &mut Vec<PendingItem>,
    now: DateTime<FixedOffset>,
) {
    for mut group in groups.into_values() {
        let mut aliases = historical_relation_aliases(&group.relation, snapshot, people);
        aliases.extend(group.raw_ids);
        if !apply_relation_events(
            &mut group.relation,
            &mut aliases,
            snapshot,
            people,
            mentions,
            pending,
            now,
        ) {
            continue;
        }
        if is_suppressed(
            snapshot,
            people,
            &group.relation.subject_id,
            &group.relation.predicate,
            &group.relation.object_id,
        ) {
            continue;
        }
        insert_relation(output, group.relation);
    }
}

fn apply_relation_events(
    relation: &mut CanonicalRelation,
    aliases: &mut BTreeSet<String>,
    snapshot: &resolve::ResolverSnapshot,
    people: &store::Voiceprints,
    mentions: &BTreeMap<(String, String), CanonicalMention>,
    pending: &mut Vec<PendingItem>,
    now: DateTime<FixedOffset>,
) -> bool {
    aliases.insert(relation.id.clone());
    for event in &snapshot.relation_decisions.events {
        match event {
            resolve::RelationEvent::Confirm { relation_id } if aliases.contains(relation_id) => {
                if relation.origin == RelationOrigin::Model {
                    relation.origin = RelationOrigin::Confirmed;
                }
            }
            resolve::RelationEvent::Edit { relation_id, edit } if aliases.contains(relation_id) => {
                let subject = resolve::resolve_reference_id(snapshot, people, &edit.subject_id);
                let object = resolve::resolve_reference_id(snapshot, people, &edit.object_id);
                let (Some(subject_id), Some(object_id)) = (subject.entity_id, object.entity_id)
                else {
                    pending.push(PendingItem::IdentityConflict {
                        note_id: relation.note_ids.first().cloned().unwrap_or_default(),
                        local_entity_id: relation_id.clone(),
                        candidates: subject
                            .candidates
                            .into_iter()
                            .chain(object.candidates)
                            .collect(),
                        reason: "edited relation references an unresolved entity".into(),
                    });
                    return false;
                };
                relation.subject_id = subject_id;
                relation.predicate = match normalized_predicate(&edit.predicate) {
                    Ok(predicate) => predicate,
                    Err(()) => {
                        pending.push(PendingItem::RelationReview {
                            note_id: relation.note_ids.first().cloned().unwrap_or_default(),
                            relation_id: relation_id.clone(),
                        });
                        return false;
                    }
                };
                relation.object_id = object_id;
                relation.valid_from = edit.valid_from.clone();
                relation.valid_to = edit.valid_to.clone();
                relation.origin = RelationOrigin::Manual;
                redirect_evidence_mentions(relation, mentions);
                relation.id = canonical_relation_id(
                    &relation.subject_id,
                    &relation.predicate,
                    &relation.object_id,
                    relation.valid_from.as_deref(),
                    relation.valid_to.as_deref(),
                );
                aliases.extend(historical_relation_aliases(relation, snapshot, people));
                aliases.insert(relation.id.clone());
            }
            resolve::RelationEvent::End {
                relation_id,
                valid_to,
            } if aliases.contains(relation_id) => {
                relation.valid_to = Some(valid_to.clone());
                relation.id = canonical_relation_id(
                    &relation.subject_id,
                    &relation.predicate,
                    &relation.object_id,
                    relation.valid_from.as_deref(),
                    relation.valid_to.as_deref(),
                );
                aliases.insert(relation.id.clone());
            }
            _ => {}
        }
    }
    relation.status = relation_status(relation.valid_to.as_deref(), now);
    true
}

fn redirect_evidence_mentions(
    relation: &mut CanonicalRelation,
    mentions: &BTreeMap<(String, String), CanonicalMention>,
) {
    for evidence in &mut relation.evidence {
        let mut mention_ids = evidence.subject_mentions.clone();
        mention_ids.extend(evidence.object_mentions.clone());
        mention_ids.sort();
        mention_ids.dedup();
        evidence.subject_mentions = mention_ids
            .iter()
            .filter(|mention_id| {
                mentions
                    .get(&(evidence.note_id.clone(), (*mention_id).clone()))
                    .is_some_and(|mention| mention.entity_id == relation.subject_id)
            })
            .cloned()
            .collect();
        evidence.object_mentions = mention_ids
            .iter()
            .filter(|mention_id| {
                mentions
                    .get(&(evidence.note_id.clone(), (*mention_id).clone()))
                    .is_some_and(|mention| mention.entity_id == relation.object_id)
            })
            .cloned()
            .collect();
    }
}

fn historical_relation_aliases(
    relation: &CanonicalRelation,
    snapshot: &resolve::ResolverSnapshot,
    people: &store::Voiceprints,
) -> BTreeSet<String> {
    let subjects = historical_endpoint_aliases(&relation.subject_id, snapshot, people);
    let objects = historical_endpoint_aliases(&relation.object_id, snapshot, people);
    subjects
        .iter()
        .flat_map(|subject| {
            objects.iter().map(move |object| {
                canonical_relation_id(
                    subject,
                    &relation.predicate,
                    object,
                    relation.valid_from.as_deref(),
                    relation.valid_to.as_deref(),
                )
            })
        })
        .collect()
}

fn historical_endpoint_aliases(
    terminal: &str,
    snapshot: &resolve::ResolverSnapshot,
    people: &store::Voiceprints,
) -> BTreeSet<String> {
    let mut candidates = BTreeSet::from([terminal.to_string()]);
    candidates.extend(snapshot.registry.keys().cloned());
    candidates.extend(snapshot.redirects.keys().cloned());
    candidates.extend(snapshot.redirects.values().cloned());
    candidates.extend(people.people.keys().cloned());
    candidates.extend(people.redirects.keys().cloned());
    candidates.extend(people.redirects.values().cloned());
    let mut aliases: BTreeSet<_> = candidates
        .into_iter()
        .filter(|candidate| {
            resolve::resolve_reference_id(snapshot, people, candidate)
                .entity_id
                .as_deref()
                == Some(terminal)
        })
        .collect();
    let mut edges = snapshot.redirect_history.clone();
    edges.extend(
        people
            .redirects
            .iter()
            .map(|(source, target)| (source.clone(), target.clone())),
    );
    loop {
        let before = aliases.len();
        for (source, target) in &edges {
            if aliases.contains(source) || aliases.contains(target) {
                aliases.insert(source.clone());
                aliases.insert(target.clone());
            }
        }
        if aliases.len() == before {
            return aliases;
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
        let predicate = match normalized_predicate(&relation.predicate) {
            Ok(predicate) => predicate,
            Err(()) => {
                pending.push(PendingItem::RelationReview {
                    note_id: String::new(),
                    relation_id: canonical_relation_id(
                        &relation.subject_id,
                        &relation.predicate,
                        &relation.object_id,
                        relation.valid_from.as_deref(),
                        relation.valid_to.as_deref(),
                    ),
                });
                continue;
            }
        };
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
                    &predicate,
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
        let valid_from = relation.valid_from.clone();
        let valid_to = relation.valid_to.clone();
        let original_id = canonical_relation_id(
            &relation.subject_id,
            &predicate,
            &relation.object_id,
            valid_from.as_deref(),
            valid_to.as_deref(),
        );
        let id = canonical_relation_id(
            &subject_id,
            &predicate,
            &object_id,
            valid_from.as_deref(),
            valid_to.as_deref(),
        );
        let mut note_ids: Vec<_> = evidence.iter().map(|item| item.note_id.clone()).collect();
        note_ids.sort();
        note_ids.dedup();
        let mut canonical = CanonicalRelation {
            id,
            subject_id,
            predicate,
            object_id,
            confidence: 1.0,
            valid_from,
            valid_to: valid_to.clone(),
            status: relation_status(valid_to.as_deref(), now),
            origin: if relation.user_assertion {
                RelationOrigin::UserAssertion
            } else {
                RelationOrigin::Manual
            },
            provider: None,
            model: None,
            note_ids,
            evidence,
        };
        let mut aliases = historical_relation_aliases(&canonical, snapshot, people);
        aliases.insert(original_id);
        if !apply_relation_events(
            &mut canonical,
            &mut aliases,
            snapshot,
            people,
            mentions,
            pending,
            now,
        ) {
            continue;
        }
        if is_suppressed(
            snapshot,
            people,
            &canonical.subject_id,
            &canonical.predicate,
            &canonical.object_id,
        ) {
            continue;
        }
        insert_relation(groups, canonical);
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

type RelationInterval = (Option<DateTime<FixedOffset>>, Option<DateTime<FixedOffset>>);

fn find_time_conflicts(relations: &[CanonicalRelation]) -> (BTreeSet<String>, Vec<PendingItem>) {
    let mut conflicts = BTreeSet::new();
    let mut relation_ids = BTreeSet::new();
    for left in 0..relations.len() {
        if relations[left].status != RelationStatus::Current {
            continue;
        }
        if !matches!(
            relations[left].predicate.kind.as_str(),
            "responsible_for" | "assigned_to"
        ) {
            continue;
        }
        let Ok(left_interval) = relation_interval(&relations[left]) else {
            continue;
        };
        for right in left + 1..relations.len() {
            if relations[right].status != RelationStatus::Current {
                continue;
            }
            let Ok(right_interval) = relation_interval(&relations[right]) else {
                continue;
            };
            if relations[left].subject_id == relations[right].subject_id
                && relations[left].predicate == relations[right].predicate
                && relations[left].object_id != relations[right].object_id
                && intervals_overlap(&left_interval, &right_interval)
            {
                let mut ids = vec![relations[left].id.clone(), relations[right].id.clone()];
                ids.sort();
                relation_ids.extend(ids.iter().cloned());
                conflicts.insert(ids);
            }
        }
    }
    (
        relation_ids,
        conflicts
            .into_iter()
            .map(|relation_ids| PendingItem::TimeConflict { relation_ids })
            .collect(),
    )
}

fn relation_interval(relation: &CanonicalRelation) -> anyhow::Result<RelationInterval> {
    let parse = |value: &Option<String>, field: &str| -> anyhow::Result<_> {
        value
            .as_deref()
            .map(DateTime::parse_from_rfc3339)
            .transpose()
            .map_err(|error| anyhow::anyhow!("invalid {field}: {error}"))
    };
    let start = parse(&relation.valid_from, "valid_from")?;
    let end = parse(&relation.valid_to, "valid_to")?;
    anyhow::ensure!(
        !matches!((start, end), (Some(start), Some(end)) if end <= start),
        "valid_to must be later than valid_from"
    );
    Ok((start, end))
}

fn intervals_overlap(left: &RelationInterval, right: &RelationInterval) -> bool {
    left.1
        .is_none_or(|left_end| right.0.is_none_or(|right_start| right_start < left_end))
        && right
            .1
            .is_none_or(|right_end| left.0.is_none_or(|left_start| left_start < right_end))
}

fn prepare_final_relation(
    relation: &mut CanonicalRelation,
    mentions: &BTreeMap<(String, String), CanonicalMention>,
    entities: &mut BTreeMap<String, CanonicalEntity>,
    people: &store::Voiceprints,
) -> Result<(), PendingItem> {
    let note_id = relation.note_ids.first().cloned().unwrap_or_default();
    let Ok(predicate) = normalized_predicate(&relation.predicate) else {
        return Err(PendingItem::RelationReview {
            note_id,
            relation_id: relation.id.clone(),
        });
    };
    relation.predicate = predicate;
    relation.id = canonical_relation_id(
        &relation.subject_id,
        &relation.predicate,
        &relation.object_id,
        relation.valid_from.as_deref(),
        relation.valid_to.as_deref(),
    );
    let endpoints_exist = [&relation.subject_id, &relation.object_id]
        .into_iter()
        .all(|entity_id| materialize_endpoint_entity(entity_id, entities, people));
    if !endpoints_exist
        || relation.subject_id == relation.object_id
        || relation_interval(relation).is_err()
    {
        return Err(PendingItem::RelationReview {
            note_id,
            relation_id: relation.id.clone(),
        });
    }
    if relation.origin == RelationOrigin::UserAssertion {
        return Ok(());
    }
    if relation.evidence.is_empty() {
        return Err(PendingItem::RelationReview {
            note_id,
            relation_id: relation.id.clone(),
        });
    }
    for evidence in &relation.evidence {
        let valid_subject = !evidence.subject_mentions.is_empty()
            && evidence.subject_mentions.iter().all(|mention_id| {
                mentions
                    .get(&(evidence.note_id.clone(), mention_id.clone()))
                    .is_some_and(|mention| mention.entity_id == relation.subject_id)
            });
        let valid_object = !evidence.object_mentions.is_empty()
            && evidence.object_mentions.iter().all(|mention_id| {
                mentions
                    .get(&(evidence.note_id.clone(), mention_id.clone()))
                    .is_some_and(|mention| mention.entity_id == relation.object_id)
            });
        if !valid_subject || !valid_object {
            return Err(PendingItem::SplitConflict {
                note_id: evidence.note_id.clone(),
                relation_id: relation.id.clone(),
                evidence_id: evidence.id.clone(),
            });
        }
    }
    Ok(())
}

fn normalized_predicate(predicate: &RelationPredicate) -> Result<RelationPredicate, ()> {
    if predicate.kind == "custom" {
        let label = predicate
            .label
            .as_deref()
            .map(str::trim)
            .filter(|label| !label.is_empty());
        return label
            .map(|label| RelationPredicate {
                kind: "custom".into(),
                label: Some(label.into()),
            })
            .ok_or(());
    }
    store::aing_graph::CORE_PREDICATES
        .contains(&predicate.kind.as_str())
        .then(|| predicate.clone())
        .ok_or(())
}

fn materialize_endpoint_entity(
    entity_id: &str,
    entities: &mut BTreeMap<String, CanonicalEntity>,
    people: &store::Voiceprints,
) -> bool {
    if entities.contains_key(entity_id) {
        return true;
    }
    let Some(person) = people.people.get(entity_id) else {
        return false;
    };
    entities.insert(
        entity_id.into(),
        CanonicalEntity {
            id: entity_id.into(),
            kind: "person".into(),
            name: person.name.clone(),
            aliases: Vec::new(),
            confirmed: true,
        },
    );
    true
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

fn scan_documents(
    data_root: &Path,
) -> anyhow::Result<Vec<Result<ScannedDocument, InvalidDocument>>> {
    let notes = data_root.join("notes");
    let root_metadata = match std::fs::symlink_metadata(&notes) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        !root_metadata.file_type().is_symlink() && root_metadata.is_dir(),
        "notes root must be a regular directory"
    );
    let entries = match std::fs::read_dir(&notes) {
        Ok(entries) => entries,
        Err(error) => return Err(error.into()),
    };
    let mut note_entries = Vec::new();
    for entry in entries {
        let entry = entry?;
        let note_id = note_id_from_file_name(entry.file_name())?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            note_entries.push(Err(InvalidDocument {
                note_id,
                message: "note directory must not be a symbolic link".into(),
            }));
        } else if file_type.is_dir() {
            match std::fs::symlink_metadata(entry.path().join(store::AING_DOC_FILE)) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                _ => note_entries.push(load_document(entry.path(), note_id)),
            }
        }
    }
    note_entries.sort_by(|left, right| document_note_id(left).cmp(document_note_id(right)));
    Ok(note_entries)
}

fn note_id_from_file_name(file_name: std::ffi::OsString) -> anyhow::Result<String> {
    file_name.into_string().map_err(|file_name| {
        anyhow::anyhow!("note directory name is not valid UTF-8: {:?}", file_name)
    })
}

fn load_document(note_dir: PathBuf, note_id: String) -> Result<ScannedDocument, InvalidDocument> {
    if let Err(error) = store::validate_note_id(&note_id) {
        return Err(InvalidDocument {
            note_id,
            message: error.to_string(),
        });
    }
    let path = note_dir.join(store::AING_DOC_FILE);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(InvalidDocument {
                note_id,
                message: "aing.json is missing".into(),
            })
        }
        Err(error) => {
            return Err(InvalidDocument {
                note_id,
                message: error.to_string(),
            })
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(InvalidDocument {
            note_id,
            message: "aing.json must be a regular file".into(),
        });
    }
    let bytes = std::fs::read(&path).map_err(|error| InvalidDocument {
        note_id: note_id.clone(),
        message: error.to_string(),
    })?;
    let mut doc: RefinedDoc = serde_json::from_slice(&bytes).map_err(|error| InvalidDocument {
        note_id: note_id.clone(),
        message: error.to_string(),
    })?;
    store::ensure_graph_ids(&note_id, &mut doc);
    normalize_document_graph(&note_id, &mut doc).map_err(|issues| InvalidDocument {
        note_id: note_id.clone(),
        message: issues
            .into_iter()
            .map(|issue| format!("{}: {}", issue.field, issue.message))
            .collect::<Vec<_>>()
            .join("; "),
    })?;
    Ok(ScannedDocument { note_id, doc })
}

// Consume Task 2 semantic/ID normalization, but restore the original provenance fields afterward
// so Task 4 remains solely responsible for current/relocated/ambiguous/stale classification.
fn normalize_document_graph(
    note_id: &str,
    doc: &mut RefinedDoc,
) -> Result<(), Vec<store::aing_graph::ValidationIssue>> {
    let mut mention_ids: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for paragraph in &mut doc.paragraphs {
        let identity = paragraph.clone();
        for mention in &mut paragraph.mentions {
            let original = mention.id.clone();
            let normalized = store::mention_id(
                note_id,
                &identity,
                &mention.entity,
                mention.start,
                mention.end,
            );
            mention_ids
                .entry((original, mention.entity.clone()))
                .or_default()
                .push(normalized.clone());
            mention.id = normalized;
        }
    }

    let mut validation_doc = doc.clone();
    let mut validation_relations = doc.relations.clone();
    let mut original_evidence = BTreeMap::new();
    let mut synthetic_index = 0_u64;
    for (relation_index, relation) in validation_relations.iter_mut().enumerate() {
        relation.subject_mentions = normalize_mention_references(
            &relation.subject_mentions,
            &relation.subject,
            &mention_ids,
        );
        relation.object_mentions =
            normalize_mention_references(&relation.object_mentions, &relation.object, &mention_ids);
        for (evidence_index, evidence) in relation.evidence.iter_mut().enumerate() {
            let original = evidence.clone();
            let location = evidence_location_for_validation(doc, evidence);
            if let Some((paragraph_index, start, end)) = location {
                evidence.paragraph_index = paragraph_index;
                evidence.start = start;
                evidence.end = end;
            } else {
                synthetic_index += 1;
                let quote = format!("semantic evidence {relation_index} {evidence_index}");
                let source_seq = u64::MAX - synthetic_index;
                let paragraph_index = validation_doc.paragraphs.len();
                validation_doc.paragraphs.push(store::RefinedParagraph {
                    speaker: "validation".into(),
                    name: None,
                    person_id: None,
                    start_ms: 0,
                    end_ms: 0,
                    text: quote.clone(),
                    source_seqs: vec![source_seq],
                    mentions: Vec::new(),
                });
                evidence.paragraph_index = paragraph_index;
                evidence.start = 0;
                evidence.end = quote.chars().count();
                evidence.quote = quote;
                evidence.source_seqs = vec![source_seq];
            }
            evidence.source_seqs.sort_unstable();
            evidence.source_seqs.dedup();
            let normalized_id = store::evidence_id(
                note_id,
                &evidence.source_seqs,
                evidence.start,
                evidence.end,
                &evidence.quote,
            );
            original_evidence.insert(normalized_id, original);
        }
    }

    let mut validated =
        store::aing_graph::validate_graph(note_id, &validation_doc, validation_relations)?;
    for relation in &mut validated.relations {
        for evidence in &mut relation.evidence {
            if let Some(original) = original_evidence.get(&evidence.id) {
                let normalized_id = evidence.id.clone();
                *evidence = original.clone();
                evidence.id = normalized_id;
                evidence.source_seqs.sort_unstable();
                evidence.source_seqs.dedup();
            }
        }
    }
    doc.relations = validated.relations;
    Ok(())
}

fn normalize_mention_references(
    references: &[String],
    entity_id: &str,
    mention_ids: &BTreeMap<(String, String), Vec<String>>,
) -> Vec<String> {
    let mut normalized: Vec<_> = references
        .iter()
        .flat_map(|reference| {
            mention_ids
                .get(&(reference.clone(), entity_id.to_string()))
                .cloned()
                .unwrap_or_else(|| vec![reference.clone()])
        })
        .collect();
    normalized.sort();
    normalized.dedup();
    normalized
}

fn first_evidence_match(
    doc: &RefinedDoc,
    quote: &str,
    source_seqs: &[u64],
) -> Option<(usize, usize, usize)> {
    if quote.is_empty() || source_seqs.is_empty() {
        return None;
    }
    for (paragraph_index, paragraph) in doc.paragraphs.iter().enumerate() {
        if !source_seqs
            .iter()
            .all(|seq| paragraph.source_seqs.contains(seq))
        {
            continue;
        }
        let text: Vec<_> = paragraph.text.chars().collect();
        let quote: Vec<_> = quote.chars().collect();
        if quote.len() > text.len() {
            continue;
        }
        for start in 0..=text.len() - quote.len() {
            if text[start..start + quote.len()] == quote {
                return Some((paragraph_index, start, start + quote.len()));
            }
        }
    }
    None
}

fn evidence_location_for_validation(
    doc: &RefinedDoc,
    evidence: &store::RelationEvidence,
) -> Option<(usize, usize, usize)> {
    doc.paragraphs
        .get(evidence.paragraph_index)
        .filter(|paragraph| {
            !evidence.source_seqs.is_empty()
                && evidence
                    .source_seqs
                    .iter()
                    .all(|seq| paragraph.source_seqs.contains(seq))
                && char_slice(&paragraph.text, evidence.start, evidence.end).as_deref()
                    == Some(evidence.quote.as_str())
        })
        .map(|_| (evidence.paragraph_index, evidence.start, evidence.end))
        .or_else(|| first_evidence_match(doc, &evidence.quote, &evidence.source_seqs))
}

fn document_note_id(document: &Result<ScannedDocument, InvalidDocument>) -> &str {
    match document {
        Ok(document) => &document.note_id,
        Err(document) => &document.note_id,
    }
}

fn local_key(note_id: &str, local_id: &str) -> String {
    format!("{note_id}/{local_id}")
}

fn is_person_id(id: &str) -> bool {
    id.strip_prefix('P').is_some_and(|suffix| {
        !suffix.is_empty() && suffix.chars().all(|char| char.is_ascii_digit())
    })
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

    fn push_user_relation(
        ledger: &mut KnowledgeLedger,
        id: &str,
        relation: super::super::overrides::UserRelation,
    ) {
        ledger.operations.push(KnowledgeOperation {
            id: id.into(),
            at: "2026-07-21T00:00:00Z".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::CreateRelation { relation },
        });
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
        ledger.registry.insert(
            "kg_other".into(),
            RegistryEntity {
                kind: "project".into(),
                name: "Other".into(),
                aliases: Vec::new(),
                status: "confirmed".into(),
            },
        );
        for (id, object_id) in [("op_user_1", project_id), ("op_user_2", "kg_other".into())] {
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

        moved.relations[0].evidence[0].paragraph_index = 0;
        moved.relations[0].evidence[0].start = 0;
        moved.relations[0].evidence[0].end = 17;
        write_note(root.path(), "n1", &moved);
        let wrong_fast_path = build_canonical_graph(root.path(), &ledger, now).unwrap();
        assert!(wrong_fast_path.relations.is_empty());
        assert!(wrong_fast_path
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
            ("kg_other", "Other"),
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
                    object_id: "kg_other".into(),
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
            vec![&"kg_other".to_string(), &"kg_target".to_string()]
        );
        assert_eq!(graph.relations[0].subject_id, "kg_target");
    }

    #[test]
    fn registry_entities_redirected_to_voiceprints_use_the_terminal_person() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("voiceprints.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "next_person": 3,
                "people": {"P1": {"name": "Canonical person"}},
                "redirects": {"P2": "P1"}
            }))
            .unwrap(),
        )
        .unwrap();
        let mut ledger = KnowledgeLedger::empty();
        ledger.registry.insert(
            "P2".into(),
            RegistryEntity {
                kind: "person".into(),
                name: "Stale person".into(),
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

        assert_eq!(
            graph.entities.keys().collect::<Vec<_>>(),
            vec![&"P1".to_string()]
        );
        assert_eq!(graph.entities["P1"].name, "Canonical person");
    }

    #[test]
    fn final_tiering_happens_after_grouping_and_raw_failures_stay_silent() {
        let root = tempfile::tempdir().unwrap();
        let mut mixed = relation_doc("n1", 0.6);
        let mut high = mixed.relations[0].clone();
        high.confidence = 0.9;
        high.id.clear();
        mixed.relations.push(high);
        ensure_graph_ids("n1", &mut mixed);
        write_note(root.path(), "n1", &mixed);
        let ledger = reconcile_registry(root.path()).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap();
        let graph = build_canonical_graph(root.path(), &ledger, now).unwrap();
        assert_eq!(graph.relations.len(), 1);
        assert_eq!(graph.relations[0].confidence, 0.9);
        assert!(
            graph.pending.is_empty(),
            "the grouped 0.9 fact is published once"
        );

        let raw_root = tempfile::tempdir().unwrap();
        let mut raw = relation_doc("raw", 0.4);
        raw.relations[0].evidence[0].paragraph_index = 99;
        raw.relations[0].evidence[0].source_hash = "stale".into();
        write_note(raw_root.path(), "raw", &raw);
        let raw_ledger = reconcile_registry(raw_root.path()).unwrap();
        let raw_graph = build_canonical_graph(raw_root.path(), &raw_ledger, now).unwrap();
        assert!(raw_graph.relations.is_empty());
        assert!(
            raw_graph.pending.is_empty(),
            "RawOnly never leaks stale/split pending"
        );
    }

    #[test]
    fn created_relation_end_restore_and_undo_recompute_temporal_identity() {
        let root = tempfile::tempdir().unwrap();
        write_note(root.path(), "n1", &relation_doc("n1", 0.4));
        let mut ledger = reconcile_registry(root.path()).unwrap();
        let subject_id = ledger.legacy_ids["n1/P1"].clone();
        let object_id = ledger.legacy_ids["n1/ent_project"].clone();
        let predicate = RelationPredicate {
            kind: "uses".into(),
            label: None,
        };
        let pre_end_id = canonical_relation_id(&subject_id, &predicate, &object_id, None, None);
        ledger.operations.push(KnowledgeOperation {
            id: "op_create_temporal".into(),
            at: "t1".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::CreateRelation {
                relation: super::super::overrides::UserRelation {
                    subject_id: subject_id.clone(),
                    predicate: predicate.clone(),
                    object_id: object_id.clone(),
                    valid_from: None,
                    valid_to: None,
                    note: None,
                    evidence_ids: Vec::new(),
                    user_assertion: true,
                },
            },
        });
        ledger.operations.push(KnowledgeOperation {
            id: "op_end_created".into(),
            at: "t2".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::EndRelation {
                relation_id: pre_end_id,
                valid_to: "2026-07-20T00:00:00+08:00".into(),
            },
        });
        let now = DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap();
        let ended = build_canonical_graph(root.path(), &ledger, now).unwrap();
        assert_eq!(ended.relations[0].status, RelationStatus::Historical);

        ledger.operations.push(KnowledgeOperation {
            id: "op_restore_created".into(),
            at: "t3".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::RestoreRelation {
                operation_id: "op_end_created".into(),
            },
        });
        let restored = build_canonical_graph(root.path(), &ledger, now).unwrap();
        assert_eq!(restored.relations[0].status, RelationStatus::Current);
        ledger.operations.push(KnowledgeOperation {
            id: "op_undo_restore_created".into(),
            at: "t4".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::Undo {
                operation_id: "op_restore_created".into(),
            },
        });
        assert_eq!(
            build_canonical_graph(root.path(), &ledger, now)
                .unwrap()
                .relations[0]
                .status,
            RelationStatus::Historical
        );
    }

    #[test]
    fn final_relation_validation_rejects_self_loops_and_unrelated_edited_evidence() {
        let root = tempfile::tempdir().unwrap();
        let mut document = relation_doc("n1", 0.9);
        document
            .entities
            .push(entity("ent_other", "term", "Other", &[]));
        write_note(root.path(), "n1", &document);
        let mut ledger = reconcile_registry(root.path()).unwrap();
        let source_id = document.relations[0].id.clone();
        let person_id = ledger.legacy_ids["n1/P1"].clone();
        let other_id = ledger.legacy_ids["n1/ent_other"].clone();
        ledger.operations.push(KnowledgeOperation {
            id: "op_edit_unrelated".into(),
            at: "t".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::EditRelation {
                relation_id: source_id,
                subject_id: person_id.clone(),
                predicate: RelationPredicate {
                    kind: "uses".into(),
                    label: None,
                },
                object_id: other_id,
                valid_from: None,
                valid_to: None,
                note: None,
            },
        });
        let now = DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap();
        let unrelated = build_canonical_graph(root.path(), &ledger, now).unwrap();
        assert!(unrelated.relations.is_empty());
        assert!(unrelated
            .pending
            .iter()
            .any(|item| matches!(item, PendingItem::SplitConflict { .. })));

        let mut self_loop = KnowledgeLedger::empty();
        self_loop.registry.insert(
            "kg_a".into(),
            RegistryEntity {
                kind: "term".into(),
                name: "A".into(),
                aliases: Vec::new(),
                status: "confirmed".into(),
            },
        );
        self_loop.operations.push(KnowledgeOperation {
            id: "op_self".into(),
            at: "t".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::CreateRelation {
                relation: super::super::overrides::UserRelation {
                    subject_id: "kg_a".into(),
                    predicate: RelationPredicate {
                        kind: "uses".into(),
                        label: None,
                    },
                    object_id: "kg_a".into(),
                    valid_from: None,
                    valid_to: None,
                    note: None,
                    evidence_ids: Vec::new(),
                    user_assertion: true,
                },
            },
        });
        let invalid = build_canonical_graph(root.path(), &self_loop, now).unwrap();
        assert!(invalid.relations.is_empty());
        assert!(invalid
            .pending
            .iter()
            .any(|item| matches!(item, PendingItem::RelationReview { .. })));
    }

    #[test]
    fn alias_ambiguity_and_unreconciled_new_ids_never_create_dangling_mentions() {
        let root = tempfile::tempdir().unwrap();
        write_note(
            root.path(),
            "n1",
            &doc(
                vec![entity("ent_1", "project", "X", &["A", "B"])],
                "X",
                vec![mention("ent_1", 0, 1)],
            ),
        );
        let mut ledger = KnowledgeLedger::empty();
        for (id, name) in [("kg_a", "A"), ("kg_b", "B")] {
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
        let graph = build_canonical_graph(
            root.path(),
            &ledger,
            DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();
        assert!(graph.mentions.is_empty());
        assert!(graph.pending.iter().any(
            |item| matches!(item, PendingItem::IdentityConflict { candidates, .. } if candidates == &["kg_a", "kg_b"])
        ));

        let dangling = tempfile::tempdir().unwrap();
        write_note(
            dangling.path(),
            "n1",
            &doc(
                vec![entity("ent_new", "term", "New", &[])],
                "New",
                vec![mention("ent_new", 0, 3)],
            ),
        );
        let graph = build_canonical_graph(
            dangling.path(),
            &KnowledgeLedger::empty(),
            DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();
        assert!(graph.mentions.is_empty());
        assert!(graph
            .pending
            .iter()
            .any(|item| matches!(item, PendingItem::IdentityConflict { .. })));
    }

    #[test]
    fn temporal_conflicts_require_real_interval_overlap_and_invalid_times_are_pending() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = KnowledgeLedger::empty();
        for id in ["kg_person", "kg_a", "kg_b"] {
            ledger.registry.insert(
                id.into(),
                RegistryEntity {
                    kind: "term".into(),
                    name: id.into(),
                    aliases: Vec::new(),
                    status: "confirmed".into(),
                },
            );
        }
        for (id, object, from, to) in [
            (
                "op_future_a",
                "kg_a",
                "2026-08-01T00:00:00+08:00",
                "2026-08-10T00:00:00+08:00",
            ),
            (
                "op_future_b",
                "kg_b",
                "2026-08-11T00:00:00+08:00",
                "2026-08-20T00:00:00+08:00",
            ),
        ] {
            ledger.operations.push(KnowledgeOperation {
                id: id.into(),
                at: "t".into(),
                before: serde_json::Value::Null,
                after: serde_json::Value::Null,
                action: KnowledgeAction::CreateRelation {
                    relation: super::super::overrides::UserRelation {
                        subject_id: "kg_person".into(),
                        predicate: RelationPredicate {
                            kind: "assigned_to".into(),
                            label: None,
                        },
                        object_id: object.into(),
                        valid_from: Some(from.into()),
                        valid_to: Some(to.into()),
                        note: None,
                        evidence_ids: Vec::new(),
                        user_assertion: true,
                    },
                },
            });
        }
        let now = DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap();
        let graph = build_canonical_graph(root.path(), &ledger, now).unwrap();
        assert!(!graph
            .pending
            .iter()
            .any(|item| matches!(item, PendingItem::TimeConflict { .. })));

        ledger.operations.push(KnowledgeOperation {
            id: "op_invalid_time".into(),
            at: "t".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::CreateRelation {
                relation: super::super::overrides::UserRelation {
                    subject_id: "kg_a".into(),
                    predicate: RelationPredicate {
                        kind: "uses".into(),
                        label: None,
                    },
                    object_id: "kg_b".into(),
                    valid_from: Some("not-a-time".into()),
                    valid_to: None,
                    note: None,
                    evidence_ids: Vec::new(),
                    user_assertion: true,
                },
            },
        });
        let invalid = build_canonical_graph(root.path(), &ledger, now).unwrap();
        assert_eq!(invalid.relations.len(), 2);
        assert!(invalid
            .pending
            .iter()
            .any(|item| matches!(item, PendingItem::RelationReview { .. })));
    }

    #[test]
    fn unreferenced_registry_cycles_and_missing_terminals_are_explicit_identity_conflicts() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = KnowledgeLedger::empty();
        for id in ["kg_a", "kg_b", "kg_source"] {
            ledger.registry.insert(
                id.into(),
                RegistryEntity {
                    kind: "term".into(),
                    name: id.into(),
                    aliases: Vec::new(),
                    status: "confirmed".into(),
                },
            );
        }
        for (id, source, target) in [
            ("m1", "kg_a", "kg_b"),
            ("m2", "kg_b", "kg_a"),
            ("m3", "kg_source", "kg_missing"),
        ] {
            ledger.operations.push(KnowledgeOperation {
                id: id.into(),
                at: "t".into(),
                before: serde_json::Value::Null,
                after: serde_json::Value::String(target.into()),
                action: KnowledgeAction::MergeEntity {
                    source_id: source.into(),
                    target_id: target.into(),
                },
            });
        }
        let graph = build_canonical_graph(
            root.path(),
            &ledger,
            DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();
        assert!(graph.entities.is_empty());
        assert_eq!(
            graph
                .pending
                .iter()
                .filter(|item| matches!(item, PendingItem::IdentityConflict { .. }))
                .count(),
            3
        );
        assert!(graph.pending.iter().any(|item| matches!(
            item,
            PendingItem::IdentityConflict { local_entity_id, candidates, .. }
                if local_entity_id == "kg_source" && candidates == &["kg_missing"]
        )));
    }

    #[test]
    fn time_conflicts_ignore_rejected_and_historical_relations() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = KnowledgeLedger::empty();
        for id in ["kg_a", "kg_b", "kg_c"] {
            ledger.registry.insert(
                id.into(),
                RegistryEntity {
                    kind: "project".into(),
                    name: id.into(),
                    aliases: Vec::new(),
                    status: "confirmed".into(),
                },
            );
        }
        for (id, object_id, valid_to) in [
            ("self_loop", "kg_a", None),
            ("historical", "kg_b", Some("2026-07-20T00:00:00Z")),
            ("current", "kg_c", None),
        ] {
            push_user_relation(
                &mut ledger,
                id,
                super::super::overrides::UserRelation {
                    subject_id: "kg_a".into(),
                    predicate: RelationPredicate {
                        kind: "responsible_for".into(),
                        label: None,
                    },
                    object_id: object_id.into(),
                    valid_from: None,
                    valid_to: valid_to.map(str::to_string),
                    note: None,
                    evidence_ids: Vec::new(),
                    user_assertion: true,
                },
            );
        }

        let graph = build_canonical_graph(
            root.path(),
            &ledger,
            DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();

        assert_eq!(graph.relations.len(), 2);
        assert!(!graph
            .pending
            .iter()
            .any(|item| matches!(item, PendingItem::TimeConflict { .. })));
        let published_ids: BTreeSet<_> = graph
            .relations
            .iter()
            .map(|relation| relation.id.as_str())
            .collect();
        assert!(graph.pending.iter().all(|item| match item {
            PendingItem::RelationReview { relation_id, .. }
            | PendingItem::StaleEvidence { relation_id, .. }
            | PendingItem::SplitConflict { relation_id, .. } =>
                !published_ids.contains(relation_id.as_str()),
            PendingItem::TimeConflict { relation_ids } => relation_ids
                .iter()
                .all(|id| published_ids.contains(id.as_str())),
            _ => true,
        }));
    }

    #[test]
    fn successful_user_assertions_materialize_voiceprint_terminal_entities() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("voiceprints.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "next_person": 2,
                "people": {"P1": {"name": "Alice"}}
            }))
            .unwrap(),
        )
        .unwrap();
        let mut ledger = KnowledgeLedger::empty();
        ledger.registry.insert(
            "kg_project".into(),
            RegistryEntity {
                kind: "project".into(),
                name: "Apollo".into(),
                aliases: Vec::new(),
                status: "confirmed".into(),
            },
        );
        push_user_relation(
            &mut ledger,
            "create",
            super::super::overrides::UserRelation {
                subject_id: "P1".into(),
                predicate: RelationPredicate {
                    kind: "uses".into(),
                    label: None,
                },
                object_id: "kg_project".into(),
                valid_from: None,
                valid_to: None,
                note: None,
                evidence_ids: Vec::new(),
                user_assertion: true,
            },
        );

        let graph = build_canonical_graph(
            root.path(),
            &ledger,
            DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();
        assert_eq!(graph.relations.len(), 1);
        assert_eq!(graph.entities["P1"].name, "Alice");
        assert!(graph.relations.iter().all(|relation| {
            graph.entities.contains_key(&relation.subject_id)
                && graph.entities.contains_key(&relation.object_id)
        }));
    }

    #[test]
    fn created_relation_end_survives_later_endpoint_redirect() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = KnowledgeLedger::empty();
        for id in ["kg_old", "kg_new", "kg_project"] {
            ledger.registry.insert(
                id.into(),
                RegistryEntity {
                    kind: "project".into(),
                    name: id.into(),
                    aliases: Vec::new(),
                    status: "confirmed".into(),
                },
            );
        }
        let predicate = RelationPredicate {
            kind: "uses".into(),
            label: None,
        };
        let original_id = canonical_relation_id("kg_old", &predicate, "kg_project", None, None);
        push_user_relation(
            &mut ledger,
            "create",
            super::super::overrides::UserRelation {
                subject_id: "kg_old".into(),
                predicate: predicate.clone(),
                object_id: "kg_project".into(),
                valid_from: None,
                valid_to: None,
                note: None,
                evidence_ids: Vec::new(),
                user_assertion: true,
            },
        );
        ledger.operations.push(KnowledgeOperation {
            id: "end".into(),
            at: "2026-07-20T00:00:00Z".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::EndRelation {
                relation_id: original_id,
                valid_to: "2026-07-20T00:00:00Z".into(),
            },
        });
        ledger.operations.push(KnowledgeOperation {
            id: "merge".into(),
            at: "2026-07-21T00:00:00Z".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::String("kg_new".into()),
            action: KnowledgeAction::MergeEntity {
                source_id: "kg_old".into(),
                target_id: "kg_new".into(),
            },
        });

        let graph = build_canonical_graph(
            root.path(),
            &ledger,
            DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();
        assert_eq!(graph.relations[0].subject_id, "kg_new");
        assert_eq!(graph.relations[0].status, RelationStatus::Historical);
        assert_eq!(
            graph.relations[0].id,
            canonical_relation_id(
                "kg_new",
                &predicate,
                "kg_project",
                None,
                Some("2026-07-20T00:00:00Z")
            )
        );
    }

    #[test]
    fn user_assertions_still_validate_and_normalize_predicates() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = KnowledgeLedger::empty();
        for id in ["kg_a", "kg_b"] {
            ledger.registry.insert(
                id.into(),
                RegistryEntity {
                    kind: "term".into(),
                    name: id.into(),
                    aliases: Vec::new(),
                    status: "confirmed".into(),
                },
            );
        }
        for (id, kind, label) in [
            ("unknown", "invented", None),
            ("blank_custom", "custom", Some("   ")),
            ("trimmed_custom", "custom", Some("  works with  ")),
        ] {
            push_user_relation(
                &mut ledger,
                id,
                super::super::overrides::UserRelation {
                    subject_id: "kg_a".into(),
                    predicate: RelationPredicate {
                        kind: kind.into(),
                        label: label.map(str::to_string),
                    },
                    object_id: "kg_b".into(),
                    valid_from: None,
                    valid_to: None,
                    note: None,
                    evidence_ids: Vec::new(),
                    user_assertion: true,
                },
            );
        }

        let graph = build_canonical_graph(
            root.path(),
            &ledger,
            DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();
        assert_eq!(graph.relations.len(), 1);
        assert_eq!(graph.relations[0].predicate.kind, "custom");
        assert_eq!(
            graph.relations[0].predicate.label.as_deref(),
            Some("works with")
        );
        assert_eq!(
            graph
                .pending
                .iter()
                .filter(|item| matches!(item, PendingItem::RelationReview { .. }))
                .count(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn notes_root_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write_note(outside.path(), "escaped", &relation_doc("escaped", 0.9));
        symlink(outside.path().join("notes"), root.path().join("notes")).unwrap();

        assert!(build_canonical_graph(
            root.path(),
            &KnowledgeLedger::empty(),
            DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_note_names_fail_closed_without_lossy_identity() {
        use std::os::unix::ffi::OsStringExt;
        let file_name = std::ffi::OsString::from_vec(b"bad\xff".to_vec());
        let error = note_id_from_file_name(file_name).unwrap_err();
        assert!(error.to_string().contains("UTF-8"));
    }

    #[test]
    fn semantically_invalid_task2_documents_are_invalid_documents() {
        let root = tempfile::tempdir().unwrap();
        let mut invalid = relation_doc("invalid", 0.9);
        invalid.relations[0].predicate.kind = "invented".into();
        write_note(root.path(), "invalid", &invalid);
        let graph = build_canonical_graph(
            root.path(),
            &KnowledgeLedger::empty(),
            DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();
        assert!(graph.relations.is_empty());
        assert!(graph.pending.iter().any(|item| matches!(
            item,
            PendingItem::InvalidDocument { note_id, message }
                if note_id == "invalid" && message.contains("predicate")
        )));
    }

    #[test]
    fn stale_no_match_is_relation_pending_but_raw_only_is_silent() {
        let now = DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap();
        for (note_id, confidence, expect_pending) in [("pending", 0.9, true), ("raw", 0.4, false)] {
            let root = tempfile::tempdir().unwrap();
            let mut document = relation_doc(note_id, confidence);
            let evidence = &mut document.relations[0].evidence[0];
            evidence.paragraph_index = 99;
            evidence.start = 99;
            evidence.end = 120;
            evidence.quote = "quote absent from every paragraph".into();
            evidence.source_hash = "stale".into();
            write_note(root.path(), note_id, &document);
            let ledger = reconcile_registry(root.path()).unwrap();
            let graph = build_canonical_graph(root.path(), &ledger, now).unwrap();
            assert!(graph.relations.is_empty());
            assert_eq!(
                graph
                    .pending
                    .iter()
                    .any(|item| matches!(item, PendingItem::StaleEvidence { .. })),
                expect_pending
            );
            assert!(!graph
                .pending
                .iter()
                .any(|item| matches!(item, PendingItem::InvalidDocument { .. })));
            if !expect_pending {
                assert!(graph.pending.is_empty());
            }
        }
    }

    #[test]
    fn visible_and_raw_alias_decisions_apply_once_to_a_multi_note_group() {
        let root = tempfile::tempdir().unwrap();
        let first = relation_doc("n1", 0.6);
        let second = relation_doc("n2", 0.6);
        let second_raw_id = second.relations[0].id.clone();
        write_note(root.path(), "n1", &first);
        write_note(root.path(), "n2", &second);
        let mut ledger = reconcile_registry(root.path()).unwrap();
        let first_project = ledger.legacy_ids["n1/ent_project"].clone();
        let second_project = ledger.legacy_ids["n2/ent_project"].clone();
        ledger.operations.push(KnowledgeOperation {
            id: "merge_note_projects".into(),
            at: "2026-07-21T00:00:00Z".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::String(first_project.clone()),
            action: KnowledgeAction::MergeEntity {
                source_id: second_project,
                target_id: first_project,
            },
        });
        let now = DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap();
        let undecided = build_canonical_graph(root.path(), &ledger, now).unwrap();
        assert_eq!(undecided.relations.len(), 1, "{:?}", undecided.relations);
        assert_eq!(undecided.relations[0].note_ids, vec!["n1", "n2"]);
        let visible_id = undecided.relations[0].id.clone();

        for (operation_id, decision_id) in [
            ("confirm_visible", visible_id),
            ("confirm_raw", second_raw_id),
        ] {
            let mut confirmed = ledger.clone();
            confirmed.operations.push(KnowledgeOperation {
                id: operation_id.into(),
                at: "2026-07-21T00:00:00Z".into(),
                before: serde_json::Value::Null,
                after: serde_json::Value::Null,
                action: KnowledgeAction::ConfirmRelation {
                    relation_id: decision_id,
                },
            });
            let graph = build_canonical_graph(root.path(), &confirmed, now).unwrap();
            assert_eq!(graph.relations.len(), 1);
            assert_eq!(graph.relations[0].origin, RelationOrigin::Confirmed);
            assert_eq!(graph.relations[0].note_ids, vec!["n1", "n2"]);
            assert!(!graph
                .pending
                .iter()
                .any(|item| matches!(item, PendingItem::RelationReview { .. })));
        }

        let project_id = ledger.legacy_ids["n1/ent_project"].clone();
        let person_id = ledger.legacy_ids["n1/P1"].clone();
        let mut edited = ledger;
        edited.operations.push(KnowledgeOperation {
            id: "edit_visible".into(),
            at: "2026-07-21T00:00:00Z".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::EditRelation {
                relation_id: undecided.relations[0].id.clone(),
                subject_id: project_id,
                predicate: RelationPredicate {
                    kind: "uses".into(),
                    label: None,
                },
                object_id: person_id,
                valid_from: None,
                valid_to: None,
                note: None,
            },
        });
        let graph = build_canonical_graph(root.path(), &edited, now).unwrap();
        assert_eq!(graph.relations.len(), 1);
        assert_eq!(graph.relations[0].origin, RelationOrigin::Manual);
        assert_eq!(graph.relations[0].note_ids, vec!["n1", "n2"]);
        assert!(!graph
            .pending
            .iter()
            .any(|item| matches!(item, PendingItem::RelationReview { .. })));
    }

    #[test]
    fn end_matches_historical_visible_ids_across_later_redirects_and_restore_parity() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = KnowledgeLedger::empty();
        for id in ["kg_a", "kg_b", "kg_mid", "kg_c"] {
            ledger.registry.insert(
                id.into(),
                RegistryEntity {
                    kind: "term".into(),
                    name: id.into(),
                    aliases: Vec::new(),
                    status: "confirmed".into(),
                },
            );
        }
        let predicate = RelationPredicate {
            kind: "uses".into(),
            label: None,
        };
        push_user_relation(
            &mut ledger,
            "create",
            super::super::overrides::UserRelation {
                subject_id: "kg_a".into(),
                predicate: predicate.clone(),
                object_id: "kg_b".into(),
                valid_from: None,
                valid_to: None,
                note: None,
                evidence_ids: Vec::new(),
                user_assertion: true,
            },
        );
        ledger.operations.push(KnowledgeOperation {
            id: "merge_b_mid".into(),
            at: "t1".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::String("kg_mid".into()),
            action: KnowledgeAction::MergeEntity {
                source_id: "kg_b".into(),
                target_id: "kg_mid".into(),
            },
        });
        let intermediate_id = canonical_relation_id("kg_a", &predicate, "kg_mid", None, None);
        ledger.operations.push(KnowledgeOperation {
            id: "end_mid".into(),
            at: "t2".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::EndRelation {
                relation_id: intermediate_id,
                valid_to: "2026-07-20T00:00:00Z".into(),
            },
        });
        ledger.operations.push(KnowledgeOperation {
            id: "merge_mid_c".into(),
            at: "t3".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::String("kg_c".into()),
            action: KnowledgeAction::MergeEntity {
                source_id: "kg_mid".into(),
                target_id: "kg_c".into(),
            },
        });
        let now = DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap();
        let ended = build_canonical_graph(root.path(), &ledger, now).unwrap();
        assert_eq!(ended.relations[0].object_id, "kg_c");
        assert_eq!(ended.relations[0].status, RelationStatus::Historical);

        ledger.operations.push(KnowledgeOperation {
            id: "restore".into(),
            at: "t4".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::RestoreRelation {
                operation_id: "end_mid".into(),
            },
        });
        assert_eq!(
            build_canonical_graph(root.path(), &ledger, now)
                .unwrap()
                .relations[0]
                .status,
            RelationStatus::Current
        );
        ledger.operations.push(KnowledgeOperation {
            id: "undo_restore".into(),
            at: "t5".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::Undo {
                operation_id: "restore".into(),
            },
        });
        assert_eq!(
            build_canonical_graph(root.path(), &ledger, now)
                .unwrap()
                .relations[0]
                .status,
            RelationStatus::Historical
        );
    }

    #[test]
    fn model_visible_end_survives_a_later_endpoint_merge() {
        let root = tempfile::tempdir().unwrap();
        write_note(root.path(), "n1", &relation_doc("n1", 0.9));
        let mut ledger = reconcile_registry(root.path()).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap();
        let visible_id = build_canonical_graph(root.path(), &ledger, now)
            .unwrap()
            .relations[0]
            .id
            .clone();
        let old_project = ledger.legacy_ids["n1/ent_project"].clone();
        ledger.registry.insert(
            "kg_new_project".into(),
            RegistryEntity {
                kind: "project".into(),
                name: "New project".into(),
                aliases: Vec::new(),
                status: "confirmed".into(),
            },
        );
        ledger.operations.push(KnowledgeOperation {
            id: "end_visible".into(),
            at: "t1".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::EndRelation {
                relation_id: visible_id,
                valid_to: "2026-07-20T00:00:00Z".into(),
            },
        });
        ledger.operations.push(KnowledgeOperation {
            id: "merge_after_end".into(),
            at: "t2".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::String("kg_new_project".into()),
            action: KnowledgeAction::MergeEntity {
                source_id: old_project,
                target_id: "kg_new_project".into(),
            },
        });

        let graph = build_canonical_graph(root.path(), &ledger, now).unwrap();
        assert_eq!(graph.relations.len(), 1);
        assert_eq!(graph.relations[0].object_id, "kg_new_project");
        assert_eq!(graph.relations[0].status, RelationStatus::Historical);
    }

    #[test]
    fn task2_normalized_ids_replace_forged_relation_and_evidence_ids() {
        let root = tempfile::tempdir().unwrap();
        let mut forged = split_relation_doc("n1");
        forged.relations[0].id = "rf_forged".into();
        for evidence in &mut forged.relations[0].evidence {
            evidence.id = "ev_forged".into();
        }
        let expected =
            store::aing_graph::validate_graph("n1", &forged, forged.relations.clone()).unwrap();
        let expected_raw_id = expected.relations[0].id.clone();
        assert_ne!(expected_raw_id, "rf_forged");
        assert_eq!(
            expected.relations[0]
                .evidence
                .iter()
                .map(|evidence| &evidence.id)
                .collect::<BTreeSet<_>>()
                .len(),
            2
        );
        write_note(root.path(), "n1", &forged);
        let mut ledger = reconcile_registry(root.path()).unwrap();
        ledger.operations.push(KnowledgeOperation {
            id: "confirm_normalized".into(),
            at: "t".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::ConfirmRelation {
                relation_id: expected_raw_id,
            },
        });
        let graph = build_canonical_graph(
            root.path(),
            &ledger,
            DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();
        assert_eq!(graph.relations.len(), 1);
        assert_eq!(graph.relations[0].origin, RelationOrigin::Confirmed);
        assert_eq!(graph.relations[0].evidence.len(), 2);
        assert_eq!(
            graph.relations[0]
                .evidence
                .iter()
                .map(|evidence| &evidence.id)
                .collect::<BTreeSet<_>>()
                .len(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn note_symlinks_are_rejected_as_invalid_documents() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_note = outside.path().join("escaped");
        std::fs::create_dir_all(&outside_note).unwrap();
        write_refined_atomic(&outside_note, &relation_doc("escaped", 0.9)).unwrap();
        std::fs::create_dir_all(root.path().join("notes")).unwrap();
        symlink(&outside_note, root.path().join("notes").join("escaped")).unwrap();
        let graph = build_canonical_graph(
            root.path(),
            &KnowledgeLedger::empty(),
            DateTime::parse_from_rfc3339("2026-07-21T12:00:00+08:00").unwrap(),
        )
        .unwrap();
        assert!(graph.relations.is_empty());
        assert!(graph.pending.iter().any(|item| matches!(item, PendingItem::InvalidDocument { note_id, .. } if note_id == "escaped")));
    }
}
