use super::overrides::{
    allocate_entity_id, allocate_split_entity_id, KnowledgeAction, KnowledgeLedger, RegistryEntity,
    UserRelation,
};
use crate::store::{Entity, RelationPredicate, VoiceprintStore, Voiceprints};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq)]
pub struct ResolverSnapshot {
    pub registry: BTreeMap<String, RegistryEntity>,
    pub redirects: BTreeMap<String, String>,
    pub mention_bindings: BTreeMap<String, String>,
    pub relation_decisions: RelationDecisions,
    legacy_ids: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RelationDecisions {
    pub confirmed: BTreeSet<String>,
    pub edited: BTreeMap<String, EditedRelation>,
    pub suppressed: Vec<SuppressedRelation>,
    pub ended: BTreeMap<String, String>,
    pub restored_operations: BTreeSet<String>,
    pub created: BTreeMap<String, UserRelation>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EditedRelation {
    pub subject_id: String,
    pub predicate: RelationPredicate,
    pub object_id: String,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SuppressedRelation {
    pub operation_id: String,
    pub subject_id: String,
    pub predicate: RelationPredicate,
    pub object_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionStatus {
    Resolved,
    New,
    PendingConflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub entity_id: Option<String>,
    pub status: ResolutionStatus,
    pub candidates: Vec<String>,
    pub reason: Option<String>,
}

impl Resolution {
    fn resolved(entity_id: String) -> Self {
        Self {
            entity_id: Some(entity_id),
            status: ResolutionStatus::Resolved,
            candidates: Vec::new(),
            reason: None,
        }
    }

    fn pending(candidates: Vec<String>, reason: impl Into<String>) -> Self {
        Self {
            entity_id: None,
            status: ResolutionStatus::PendingConflict,
            candidates,
            reason: Some(reason.into()),
        }
    }
}

pub fn replay(ledger: &KnowledgeLedger) -> anyhow::Result<ResolverSnapshot> {
    anyhow::ensure!(
        ledger.schema_version == 1,
        "unsupported knowledge ledger schema"
    );
    let mut snapshot = ResolverSnapshot {
        registry: ledger.registry.clone(),
        redirects: BTreeMap::new(),
        mention_bindings: BTreeMap::new(),
        relation_decisions: RelationDecisions::default(),
        legacy_ids: ledger.legacy_ids.clone(),
    };
    let operations_by_id: BTreeMap<_, _> = ledger
        .operations
        .iter()
        .map(|operation| (operation.id.as_str(), operation))
        .collect();
    anyhow::ensure!(
        operations_by_id.len() == ledger.operations.len(),
        "duplicate knowledge operation id"
    );

    let mut undone = BTreeSet::new();
    for operation in &ledger.operations {
        if let KnowledgeAction::Undo { operation_id } = &operation.action {
            anyhow::ensure!(
                operations_by_id.contains_key(operation_id.as_str()),
                "undo references unknown operation {operation_id}"
            );
            if let KnowledgeAction::Undo {
                operation_id: nested,
            } = &operations_by_id[operation_id.as_str()].action
            {
                undone.insert(operation_id.clone());
                undone.remove(nested);
            } else {
                undone.insert(operation_id.clone());
            }
        }
    }
    let restored: BTreeSet<String> = ledger
        .operations
        .iter()
        .filter(|operation| !undone.contains(&operation.id))
        .filter_map(|operation| match &operation.action {
            KnowledgeAction::RestoreRelation { operation_id } => Some(operation_id.clone()),
            _ => None,
        })
        .collect();

    for operation in &ledger.operations {
        if undone.contains(&operation.id)
            || restored.contains(&operation.id)
            || matches!(operation.action, KnowledgeAction::Undo { .. })
        {
            continue;
        }
        match &operation.action {
            KnowledgeAction::RenameEntity { entity_id, name } => {
                snapshot
                    .registry
                    .get_mut(entity_id)
                    .ok_or_else(|| anyhow::anyhow!("rename references unknown entity {entity_id}"))?
                    .name = name.clone();
            }
            KnowledgeAction::AddAlias { entity_id, alias } => {
                let entity = snapshot.registry.get_mut(entity_id).ok_or_else(|| {
                    anyhow::anyhow!("alias references unknown entity {entity_id}")
                })?;
                if !entity.aliases.iter().any(|value| value == alias) {
                    entity.aliases.push(alias.clone());
                }
            }
            KnowledgeAction::RemoveAlias { entity_id, alias } => {
                let entity = snapshot.registry.get_mut(entity_id).ok_or_else(|| {
                    anyhow::anyhow!("alias references unknown entity {entity_id}")
                })?;
                entity.aliases.retain(|value| value != alias);
            }
            KnowledgeAction::MergeEntity {
                source_id,
                target_id,
            } => {
                snapshot
                    .redirects
                    .insert(source_id.clone(), target_id.clone());
            }
            KnowledgeAction::BindMention {
                mention_id,
                entity_id,
            } => {
                snapshot
                    .mention_bindings
                    .insert(mention_id.clone(), entity_id.clone());
            }
            KnowledgeAction::ConfirmRelation { relation_id } => {
                snapshot
                    .relation_decisions
                    .confirmed
                    .insert(relation_id.clone());
            }
            KnowledgeAction::EditRelation {
                relation_id,
                subject_id,
                predicate,
                object_id,
                valid_from,
                valid_to,
                note,
            } => {
                snapshot.relation_decisions.edited.insert(
                    relation_id.clone(),
                    EditedRelation {
                        subject_id: subject_id.clone(),
                        predicate: predicate.clone(),
                        object_id: object_id.clone(),
                        valid_from: valid_from.clone(),
                        valid_to: valid_to.clone(),
                        note: note.clone(),
                    },
                );
            }
            KnowledgeAction::SuppressRelation {
                subject_id,
                predicate,
                object_id,
            } => {
                snapshot
                    .relation_decisions
                    .suppressed
                    .push(SuppressedRelation {
                        operation_id: operation.id.clone(),
                        subject_id: subject_id.clone(),
                        predicate: predicate.clone(),
                        object_id: object_id.clone(),
                    });
            }
            KnowledgeAction::EndRelation {
                relation_id,
                valid_to,
            } => {
                snapshot
                    .relation_decisions
                    .ended
                    .insert(relation_id.clone(), valid_to.clone());
            }
            KnowledgeAction::RestoreRelation { operation_id } => {
                snapshot
                    .relation_decisions
                    .restored_operations
                    .insert(operation_id.clone());
            }
            KnowledgeAction::CreateEntity { entity } => {
                if !snapshot
                    .registry
                    .values()
                    .any(|existing| existing == entity)
                {
                    snapshot
                        .registry
                        .insert(allocate_split_entity_id(&operation.id), entity.clone());
                }
            }
            KnowledgeAction::CreateRelation { relation } => {
                anyhow::ensure!(
                    !relation.evidence_ids.is_empty() || relation.user_assertion,
                    "created relation requires evidence or user assertion"
                );
                snapshot
                    .relation_decisions
                    .created
                    .insert(operation.id.clone(), relation.clone());
            }
            KnowledgeAction::Undo { .. } => unreachable!("undo operations are filtered above"),
        }
    }
    snapshot.relation_decisions.restored_operations = restored;
    Ok(snapshot)
}

pub fn resolve_entity(
    snapshot: &ResolverSnapshot,
    people: &Voiceprints,
    note_id: &str,
    local: &Entity,
    mention_ids: &[String],
) -> Resolution {
    let bound: Vec<String> = mention_ids
        .iter()
        .filter_map(|mention_id| snapshot.mention_bindings.get(mention_id).cloned())
        .collect();
    let mut bound = match canonicalize_candidates(snapshot, bound) {
        Ok(bound) => bound,
        Err(cycle) => return Resolution::pending(cycle, "redirect cycle detected"),
    };
    bound.sort();
    bound.dedup();
    if bound.len() > 1 {
        return Resolution::pending(bound, "mention bindings are ambiguous");
    }
    if let Some(entity_id) = bound.pop() {
        return resolved_through_redirects(snapshot, entity_id);
    }

    if local.kind == "person" {
        if let Some(person_id) = VoiceprintStore::resolve(people, &local.id) {
            return resolved_through_redirects(snapshot, person_id.to_string());
        }
    }

    if snapshot.redirects.contains_key(&local.id) {
        return resolved_through_redirects(snapshot, local.id.clone());
    }

    let name = normalize(&local.name);
    let exact_matches: Vec<String> = snapshot
        .registry
        .iter()
        .filter(|(_, entity)| entity.status == "confirmed" && entity.kind == local.kind)
        .filter(|(_, entity)| {
            normalize(&entity.name) == name
                || entity.aliases.iter().any(|alias| normalize(alias) == name)
        })
        .map(|(entity_id, _)| entity_id.clone())
        .collect();
    let mut exact_matches = match canonicalize_candidates(snapshot, exact_matches) {
        Ok(matches) => matches,
        Err(cycle) => return Resolution::pending(cycle, "redirect cycle detected"),
    };
    match exact_matches.len() {
        1 => return resolved_through_redirects(snapshot, exact_matches.remove(0)),
        count if count > 1 => {
            return Resolution::pending(exact_matches, "exact confirmed entity match is ambiguous");
        }
        _ => {}
    }

    if let Some(entity_id) = snapshot.registry_id_for_legacy(&local.id) {
        return resolved_through_redirects(snapshot, entity_id);
    }

    Resolution {
        entity_id: Some(allocate_entity_id(
            &local.kind,
            &local.name,
            note_id,
            &local.id,
        )),
        status: ResolutionStatus::New,
        candidates: Vec::new(),
        reason: None,
    }
}

impl ResolverSnapshot {
    fn registry_id_for_legacy(&self, legacy_id: &str) -> Option<String> {
        self.legacy_ids.get(legacy_id).cloned()
    }
}

fn resolved_through_redirects(snapshot: &ResolverSnapshot, entity_id: String) -> Resolution {
    match follow_redirects(snapshot, entity_id) {
        Ok(entity_id) => Resolution::resolved(entity_id),
        Err(candidates) => Resolution::pending(candidates, "redirect cycle detected"),
    }
}

fn canonicalize_candidates(
    snapshot: &ResolverSnapshot,
    candidates: Vec<String>,
) -> Result<Vec<String>, Vec<String>> {
    let mut canonical = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        canonical.push(follow_redirects(snapshot, candidate)?);
    }
    canonical.sort();
    canonical.dedup();
    Ok(canonical)
}

fn follow_redirects(snapshot: &ResolverSnapshot, entity_id: String) -> Result<String, Vec<String>> {
    let mut current = entity_id;
    let mut visited = BTreeSet::new();
    while let Some(next) = snapshot.redirects.get(&current) {
        if !visited.insert(current.clone()) || visited.contains(next) {
            let mut candidates: Vec<_> = visited.into_iter().collect();
            candidates.push(next.clone());
            candidates.sort();
            candidates.dedup();
            return Err(candidates);
        }
        current = next.clone();
    }
    Ok(current)
}

fn normalize(value: &str) -> String {
    value.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::overrides::{
        KnowledgeAction, KnowledgeLedger, KnowledgeOperation, RegistryEntity,
    };
    use crate::store::{Entity, Voiceprints};
    use serde_json::Value;

    fn registry_entity(kind: &str, name: &str, aliases: &[&str]) -> RegistryEntity {
        RegistryEntity {
            kind: kind.into(),
            name: name.into(),
            aliases: aliases.iter().map(|value| (*value).into()).collect(),
            status: "confirmed".into(),
        }
    }

    fn operation(id: &str, action: KnowledgeAction) -> KnowledgeOperation {
        KnowledgeOperation {
            id: id.into(),
            at: "2026-07-21T00:00:00Z".into(),
            before: Value::Null,
            after: Value::Null,
            action,
        }
    }

    fn local(id: &str, kind: &str, name: &str) -> Entity {
        Entity {
            id: id.into(),
            kind: kind.into(),
            name: name.into(),
            aliases: Vec::new(),
        }
    }

    #[test]
    fn replay_applies_entity_edits_without_changing_stable_id() {
        let mut ledger = KnowledgeLedger::empty();
        ledger
            .registry
            .insert("kg_1".into(), registry_entity("project", "Apollo", &[]));
        ledger.operations.extend([
            operation(
                "op_rename",
                KnowledgeAction::RenameEntity {
                    entity_id: "kg_1".into(),
                    name: "Artemis".into(),
                },
            ),
            operation(
                "op_alias",
                KnowledgeAction::AddAlias {
                    entity_id: "kg_1".into(),
                    alias: "Moonshot".into(),
                },
            ),
        ]);

        let snapshot = replay(&ledger).unwrap();

        assert_eq!(snapshot.registry["kg_1"].name, "Artemis");
        assert_eq!(snapshot.registry["kg_1"].aliases, vec!["Moonshot"]);
    }

    #[test]
    fn mention_binding_has_priority_over_all_other_matches() {
        let mut ledger = KnowledgeLedger::empty();
        ledger
            .registry
            .insert("kg_bound".into(), registry_entity("project", "Bound", &[]));
        ledger
            .registry
            .insert("kg_name".into(), registry_entity("project", "Apollo", &[]));
        ledger.operations.push(operation(
            "op_bind",
            KnowledgeAction::BindMention {
                mention_id: "mn_1".into(),
                entity_id: "kg_bound".into(),
            },
        ));
        let snapshot = replay(&ledger).unwrap();

        let resolution = resolve_entity(
            &snapshot,
            &Voiceprints::default(),
            "note-1",
            &local("e:apollo", "project", "Apollo"),
            &["mn_1".into()],
        );

        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.entity_id.as_deref(), Some("kg_bound"));
    }

    #[test]
    fn exact_confirmed_name_or_alias_requires_matching_kind() {
        let mut ledger = KnowledgeLedger::empty();
        ledger.registry.insert(
            "kg_project".into(),
            registry_entity("project", "Apollo", &["Moonshot"]),
        );
        ledger
            .registry
            .insert("kg_org".into(), registry_entity("org", "Apollo", &[]));
        let snapshot = replay(&ledger).unwrap();

        let by_alias = resolve_entity(
            &snapshot,
            &Voiceprints::default(),
            "n1",
            &local("ent_1", "project", "Moonshot"),
            &[],
        );
        assert_eq!(by_alias.entity_id.as_deref(), Some("kg_project"));
    }

    #[test]
    fn ambiguous_exact_matches_become_pending_conflict() {
        let mut ledger = KnowledgeLedger::empty();
        ledger
            .registry
            .insert("kg_a".into(), registry_entity("project", "Apollo", &[]));
        ledger.registry.insert(
            "kg_b".into(),
            registry_entity("project", "Other", &["Apollo"]),
        );
        let snapshot = replay(&ledger).unwrap();

        let resolution = resolve_entity(
            &snapshot,
            &Voiceprints::default(),
            "n1",
            &local("ent_1", "project", "Apollo"),
            &[],
        );

        assert_eq!(resolution.status, ResolutionStatus::PendingConflict);
        assert_eq!(resolution.candidates, vec!["kg_a", "kg_b"]);
    }

    #[test]
    fn exact_matches_merged_to_one_target_are_not_ambiguous() {
        let mut ledger = KnowledgeLedger::empty();
        ledger
            .registry
            .insert("kg_a".into(), registry_entity("project", "Apollo", &[]));
        ledger.registry.insert(
            "kg_b".into(),
            registry_entity("project", "Other", &["Apollo"]),
        );
        ledger.registry.insert(
            "kg_target".into(),
            registry_entity("project", "Canonical", &[]),
        );
        ledger.operations.extend([
            operation(
                "op_1",
                KnowledgeAction::MergeEntity {
                    source_id: "kg_a".into(),
                    target_id: "kg_target".into(),
                },
            ),
            operation(
                "op_2",
                KnowledgeAction::MergeEntity {
                    source_id: "kg_b".into(),
                    target_id: "kg_target".into(),
                },
            ),
        ]);
        let snapshot = replay(&ledger).unwrap();

        let resolution = resolve_entity(
            &snapshot,
            &Voiceprints::default(),
            "n1",
            &local("ent_1", "project", "Apollo"),
            &[],
        );

        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.entity_id.as_deref(), Some("kg_target"));
    }

    #[test]
    fn legacy_mapping_precedes_new_id_allocation() {
        let mut ledger = KnowledgeLedger::empty();
        ledger
            .legacy_ids
            .insert("e:apollo".into(), "kg_existing".into());
        ledger.registry.insert(
            "kg_existing".into(),
            registry_entity("project", "Unrelated", &[]),
        );
        let snapshot = replay(&ledger).unwrap();

        let resolution = resolve_entity(
            &snapshot,
            &Voiceprints::default(),
            "n1",
            &local("e:apollo", "project", "Apollo"),
            &[],
        );
        assert_eq!(resolution.entity_id.as_deref(), Some("kg_existing"));
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
    }

    #[test]
    fn unmatched_entities_get_seeded_stable_ids_without_fuzzy_matching() {
        let mut ledger = KnowledgeLedger::empty();
        ledger
            .registry
            .insert("kg_old".into(), registry_entity("project", "Apollo", &[]));
        let snapshot = replay(&ledger).unwrap();
        let entity = local("ent_1", "project", "Apoll");

        let resolution = resolve_entity(&snapshot, &Voiceprints::default(), "n1", &entity, &[]);

        assert_eq!(resolution.status, ResolutionStatus::New);
        assert_eq!(
            resolution.entity_id.as_deref(),
            Some(
                crate::graph::overrides::allocate_entity_id("project", "Apoll", "n1", "ent_1")
                    .as_str()
            )
        );
    }

    #[test]
    fn redirect_cycles_become_pending_conflicts() {
        let mut ledger = KnowledgeLedger::empty();
        ledger.operations.extend([
            operation(
                "op_1",
                KnowledgeAction::MergeEntity {
                    source_id: "kg_a".into(),
                    target_id: "kg_b".into(),
                },
            ),
            operation(
                "op_2",
                KnowledgeAction::MergeEntity {
                    source_id: "kg_b".into(),
                    target_id: "kg_a".into(),
                },
            ),
            operation(
                "op_3",
                KnowledgeAction::BindMention {
                    mention_id: "mn_1".into(),
                    entity_id: "kg_a".into(),
                },
            ),
        ]);
        let snapshot = replay(&ledger).unwrap();

        let resolution = resolve_entity(
            &snapshot,
            &Voiceprints::default(),
            "n1",
            &local("ent_1", "project", "Anything"),
            &["mn_1".into()],
        );
        assert_eq!(resolution.status, ResolutionStatus::PendingConflict);
        assert!(resolution.reason.unwrap().contains("cycle"));
    }

    #[test]
    fn replay_collects_relation_decisions_and_honors_undo() {
        let mut ledger = KnowledgeLedger::empty();
        ledger.operations.extend([
            operation(
                "op_confirm",
                KnowledgeAction::ConfirmRelation {
                    relation_id: "rf_1".into(),
                },
            ),
            operation(
                "op_undo",
                KnowledgeAction::Undo {
                    operation_id: "op_confirm".into(),
                },
            ),
            operation(
                "op_end",
                KnowledgeAction::EndRelation {
                    relation_id: "rf_2".into(),
                    valid_to: "2026-07-20".into(),
                },
            ),
        ]);

        let snapshot = replay(&ledger).unwrap();

        assert!(!snapshot.relation_decisions.confirmed.contains("rf_1"));
        assert_eq!(snapshot.relation_decisions.ended["rf_2"], "2026-07-20");
    }
}
