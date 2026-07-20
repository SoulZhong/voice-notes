use super::overrides::{
    active_operation_mask, allocate_entity_id, registry_baseline, KnowledgeAction, KnowledgeLedger,
    KnowledgeOperation, RegistryEntity, UserRelation,
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
    registry_baseline(ledger)?;
    validate_decision_history(&ledger.operations)?;
    let active = active_operation_mask(&ledger.operations)?;
    let mut snapshot = ResolverSnapshot {
        registry: ledger.registry.clone(),
        redirects: BTreeMap::new(),
        mention_bindings: BTreeMap::new(),
        relation_decisions: RelationDecisions::default(),
        legacy_ids: ledger.legacy_ids.clone(),
    };
    let restored: BTreeSet<String> = ledger
        .operations
        .iter()
        .enumerate()
        .filter(|(index, _)| active[*index])
        .filter_map(|(_, operation)| match &operation.action {
            KnowledgeAction::RestoreRelation { operation_id } => Some(operation_id.clone()),
            _ => None,
        })
        .collect();

    for (index, operation) in ledger.operations.iter().enumerate() {
        if !active[index]
            || restored.contains(&operation.id)
            || matches!(operation.action, KnowledgeAction::Undo { .. })
        {
            continue;
        }
        match &operation.action {
            KnowledgeAction::RenameEntity { .. }
            | KnowledgeAction::AddAlias { .. }
            | KnowledgeAction::RemoveAlias { .. }
            | KnowledgeAction::CreateEntity { .. } => {}
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

pub(crate) fn validate_decision_history(operations: &[KnowledgeOperation]) -> anyhow::Result<()> {
    validate_restore_targets(operations)?;
    let mut redirects = BTreeMap::new();
    let mut mention_bindings = BTreeMap::new();
    for (index, operation) in operations.iter().enumerate() {
        validate_decision_snapshot(operation)?;
        match &operation.action {
            KnowledgeAction::MergeEntity {
                source_id,
                target_id,
            } => validate_historical_decision(&mut redirects, source_id, operation, target_id)?,
            KnowledgeAction::BindMention {
                mention_id,
                entity_id,
            } => validate_historical_decision(
                &mut mention_bindings,
                mention_id,
                operation,
                entity_id,
            )?,
            KnowledgeAction::Undo { .. } => {
                let prefix = &operations[..=index];
                let active = active_operation_mask(prefix)?;
                (redirects, mention_bindings) = project_decisions(prefix, &active);
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_restore_targets(operations: &[KnowledgeOperation]) -> anyhow::Result<()> {
    let indexes: BTreeMap<&str, usize> = operations
        .iter()
        .enumerate()
        .map(|(index, operation)| (operation.id.as_str(), index))
        .collect();
    for (index, operation) in operations.iter().enumerate() {
        let KnowledgeAction::RestoreRelation { operation_id } = &operation.action else {
            continue;
        };
        let target_index = indexes.get(operation_id.as_str()).copied().ok_or_else(|| {
            anyhow::anyhow!(
                "restore operation {} references unknown operation {operation_id}",
                operation.id
            )
        })?;
        anyhow::ensure!(
            target_index < index,
            "restore operation {} must reference an earlier operation",
            operation.id
        );
        anyhow::ensure!(
            matches!(
                operations[target_index].action,
                KnowledgeAction::SuppressRelation { .. } | KnowledgeAction::EndRelation { .. }
            ),
            "restore operation {} targets an operation that cannot restore a relation",
            operation.id
        );
    }
    Ok(())
}

pub(crate) fn validate_decision_snapshot(operation: &KnowledgeOperation) -> anyhow::Result<()> {
    let expected = match &operation.action {
        KnowledgeAction::MergeEntity { target_id, .. } => Some(target_id),
        KnowledgeAction::BindMention { entity_id, .. } => Some(entity_id),
        _ => None,
    };
    let Some(expected) = expected else {
        return Ok(());
    };
    let _ = optional_string(&operation.before, operation, "before")?;
    let after = optional_string(&operation.after, operation, "after")?;
    anyhow::ensure!(
        after.as_deref() == Some(expected.as_str()),
        "operation {} after decision snapshot mismatch",
        operation.id
    );
    Ok(())
}

fn validate_historical_decision(
    decisions: &mut BTreeMap<String, String>,
    key: &str,
    operation: &KnowledgeOperation,
    expected_after: &str,
) -> anyhow::Result<()> {
    let before = optional_string(&operation.before, operation, "before")?;
    anyhow::ensure!(
        decisions.get(key) == before.as_ref(),
        "operation {} before decision snapshot mismatch",
        operation.id
    );
    decisions.insert(key.to_string(), expected_after.to_string());
    Ok(())
}

fn project_decisions(
    operations: &[KnowledgeOperation],
    active: &[bool],
) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let mut redirects = BTreeMap::new();
    let mut mention_bindings = BTreeMap::new();
    for (index, operation) in operations.iter().enumerate() {
        if !active[index] {
            continue;
        }
        match &operation.action {
            KnowledgeAction::MergeEntity {
                source_id,
                target_id,
            } => {
                redirects.insert(source_id.clone(), target_id.clone());
            }
            KnowledgeAction::BindMention {
                mention_id,
                entity_id,
            } => {
                mention_bindings.insert(mention_id.clone(), entity_id.clone());
            }
            _ => {}
        }
    }
    (redirects, mention_bindings)
}

fn optional_string(
    value: &serde_json::Value,
    operation: &KnowledgeOperation,
    field: &str,
) -> anyhow::Result<Option<String>> {
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(value) => Ok(Some(value.clone())),
        _ => anyhow::bail!(
            "operation {} {field} decision snapshot must be null or string",
            operation.id
        ),
    }
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
    let mut bound = match canonicalize_candidates(snapshot, people, bound) {
        Ok(bound) => bound,
        Err(failure) => return Resolution::pending(failure.candidates, failure.reason),
    };
    bound.sort();
    bound.dedup();
    if bound.len() > 1 {
        return Resolution::pending(bound, "mention bindings are ambiguous");
    }
    if let Some(entity_id) = bound.pop() {
        return resolved_through_redirects(snapshot, people, entity_id);
    }

    if snapshot.redirects.contains_key(&local.id) || people.redirects.contains_key(&local.id) {
        return resolved_through_redirects(snapshot, people, local.id.clone());
    }

    if local.kind == "person" {
        if let Some(person_id) = VoiceprintStore::resolve(people, &local.id) {
            return resolved_through_redirects(snapshot, people, person_id.to_string());
        }
    }

    let exact_matches = confirmed_registry_matches(&snapshot.registry, local);
    let mut exact_matches = match canonicalize_candidates(snapshot, people, exact_matches) {
        Ok(matches) => matches,
        Err(failure) => return Resolution::pending(failure.candidates, failure.reason),
    };
    match exact_matches.len() {
        1 => return resolved_through_redirects(snapshot, people, exact_matches.remove(0)),
        count if count > 1 => {
            return Resolution::pending(exact_matches, "exact confirmed entity match is ambiguous");
        }
        _ => {}
    }

    if let Some(entity_id) = snapshot
        .registry_id_for_legacy(&format!("{note_id}/{}", local.id))
        .or_else(|| snapshot.registry_id_for_legacy(&local.id))
    {
        return resolved_through_redirects(snapshot, people, entity_id);
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

/// Resolve an already-stable registry/person reference through entity and voiceprint redirects.
/// Canonical relation decisions use this so merges apply equally to model and user-authored edges.
pub fn resolve_reference_id(
    snapshot: &ResolverSnapshot,
    people: &Voiceprints,
    entity_id: &str,
) -> Resolution {
    match follow_all_redirects(snapshot, people, entity_id) {
        Ok(entity_id)
            if snapshot.registry.contains_key(&entity_id)
                || people.people.contains_key(&entity_id) =>
        {
            Resolution::resolved(entity_id)
        }
        Ok(entity_id) => Resolution::pending(vec![entity_id], "resolved target entity is missing"),
        Err(failure) => Resolution::pending(failure.candidates, failure.reason),
    }
}

pub(crate) fn confirmed_registry_matches(
    registry: &BTreeMap<String, RegistryEntity>,
    local: &Entity,
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
        .map(|(entity_id, _)| entity_id.clone())
        .collect()
}

impl ResolverSnapshot {
    fn registry_id_for_legacy(&self, legacy_id: &str) -> Option<String> {
        self.legacy_ids.get(legacy_id).cloned()
    }
}

fn resolved_through_redirects(
    snapshot: &ResolverSnapshot,
    people: &Voiceprints,
    entity_id: String,
) -> Resolution {
    resolve_reference_id(snapshot, people, &entity_id)
}

fn canonicalize_candidates(
    snapshot: &ResolverSnapshot,
    people: &Voiceprints,
    candidates: Vec<String>,
) -> Result<Vec<String>, RedirectFailure> {
    let mut canonical = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let resolution = resolve_reference_id(snapshot, people, &candidate);
        let Some(entity_id) = resolution.entity_id else {
            return Err(RedirectFailure {
                candidates: resolution.candidates,
                reason: resolution
                    .reason
                    .unwrap_or_else(|| "redirect resolution failed".into()),
            });
        };
        canonical.push(entity_id);
    }
    canonical.sort();
    canonical.dedup();
    Ok(canonical)
}

struct RedirectFailure {
    candidates: Vec<String>,
    reason: String,
}

fn follow_all_redirects(
    snapshot: &ResolverSnapshot,
    people: &Voiceprints,
    entity_id: &str,
) -> Result<String, RedirectFailure> {
    let mut current = entity_id.to_string();
    let mut visited = BTreeSet::new();
    loop {
        if !visited.insert(current.clone()) {
            let mut candidates: Vec<_> = visited.into_iter().collect();
            candidates.sort();
            return Err(RedirectFailure {
                candidates,
                reason: "redirect cycle detected".into(),
            });
        }
        let registry_next = snapshot.redirects.get(&current);
        let voiceprint_next = people.redirects.get(&current);
        let next = match (registry_next, voiceprint_next) {
            (Some(registry), Some(voiceprint)) if registry != voiceprint => {
                let mut candidates = vec![registry.clone(), voiceprint.clone()];
                candidates.sort();
                candidates.dedup();
                return Err(RedirectFailure {
                    candidates,
                    reason: "registry and voiceprint redirects disagree".into(),
                });
            }
            (Some(next), _) | (_, Some(next)) => Some(next),
            (None, None) => None,
        };
        match next {
            Some(next) => current = next.clone(),
            None => return Ok(current),
        }
    }
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

    fn entity_state(entity_id: &str, entity: Option<&RegistryEntity>) -> Value {
        serde_json::json!({"entity_id": entity_id, "entity": entity})
    }

    fn entity_operation(
        id: &str,
        before: (&str, Option<&RegistryEntity>),
        after: (&str, Option<&RegistryEntity>),
        action: KnowledgeAction,
    ) -> KnowledgeOperation {
        KnowledgeOperation {
            id: id.into(),
            at: "2026-07-21T00:00:00Z".into(),
            before: entity_state(before.0, before.1),
            after: entity_state(after.0, after.1),
            action,
        }
    }

    fn decision_operation(
        id: &str,
        before: Option<&str>,
        after: &str,
        action: KnowledgeAction,
    ) -> KnowledgeOperation {
        KnowledgeOperation {
            id: id.into(),
            at: "2026-07-21T00:00:00Z".into(),
            before: before.map_or(Value::Null, |value| Value::String(value.into())),
            after: Value::String(after.into()),
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
        let original = registry_entity("project", "Apollo", &[]);
        let renamed = registry_entity("project", "Artemis", &[]);
        let aliased = registry_entity("project", "Artemis", &["Moonshot"]);
        ledger.registry.insert("kg_1".into(), aliased.clone());
        ledger.operations.extend([
            entity_operation(
                "op_rename",
                ("kg_1", Some(&original)),
                ("kg_1", Some(&renamed)),
                KnowledgeAction::RenameEntity {
                    entity_id: "kg_1".into(),
                    name: "Artemis".into(),
                },
            ),
            entity_operation(
                "op_alias",
                ("kg_1", Some(&renamed)),
                ("kg_1", Some(&aliased)),
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
    fn direct_undo_compensates_materialized_rename() {
        let original = registry_entity("project", "Apollo", &[]);
        let renamed = registry_entity("project", "Artemis", &[]);
        let mut ledger = KnowledgeLedger::empty();
        ledger.registry.insert("kg_1".into(), original.clone());
        ledger.operations.extend([
            entity_operation(
                "op_rename",
                ("kg_1", Some(&original)),
                ("kg_1", Some(&renamed)),
                KnowledgeAction::RenameEntity {
                    entity_id: "kg_1".into(),
                    name: "Artemis".into(),
                },
            ),
            operation(
                "op_undo",
                KnowledgeAction::Undo {
                    operation_id: "op_rename".into(),
                },
            ),
        ]);

        assert_eq!(replay(&ledger).unwrap().registry["kg_1"], original);
    }

    #[test]
    fn nested_undo_parity_handles_two_and_three_levels() {
        let original = registry_entity("project", "Apollo", &[]);
        let renamed = registry_entity("project", "Artemis", &[]);
        let rename = entity_operation(
            "op_rename",
            ("kg_1", Some(&original)),
            ("kg_1", Some(&renamed)),
            KnowledgeAction::RenameEntity {
                entity_id: "kg_1".into(),
                name: "Artemis".into(),
            },
        );
        let undo_1 = operation(
            "op_undo_1",
            KnowledgeAction::Undo {
                operation_id: "op_rename".into(),
            },
        );
        let undo_2 = operation(
            "op_undo_2",
            KnowledgeAction::Undo {
                operation_id: "op_undo_1".into(),
            },
        );
        let undo_3 = operation(
            "op_undo_3",
            KnowledgeAction::Undo {
                operation_id: "op_undo_2".into(),
            },
        );

        let mut two = KnowledgeLedger::empty();
        two.registry.insert("kg_1".into(), renamed.clone());
        two.operations = vec![rename.clone(), undo_1.clone(), undo_2.clone()];
        assert_eq!(replay(&two).unwrap().registry["kg_1"], renamed);

        let mut three = KnowledgeLedger::empty();
        three.registry.insert("kg_1".into(), original.clone());
        three.operations = vec![rename, undo_1, undo_2, undo_3];
        assert_eq!(replay(&three).unwrap().registry["kg_1"], original);
    }

    #[test]
    fn non_lifo_undo_replays_create_rename_and_alias_by_field_semantics() {
        let original = registry_entity("project", "Apollo", &[]);
        let renamed = registry_entity("project", "Artemis", &[]);
        let renamed_with_alias = registry_entity("project", "Artemis", &["Moonshot"]);
        let projected = registry_entity("project", "Apollo", &["Moonshot"]);
        let mut ledger = KnowledgeLedger::empty();
        ledger.registry.insert("kg_1".into(), projected.clone());
        ledger.operations.extend([
            entity_operation(
                "op_create",
                ("kg_1", None),
                ("kg_1", Some(&original)),
                KnowledgeAction::CreateEntity {
                    entity: original.clone(),
                },
            ),
            entity_operation(
                "op_rename",
                ("kg_1", Some(&original)),
                ("kg_1", Some(&renamed)),
                KnowledgeAction::RenameEntity {
                    entity_id: "kg_1".into(),
                    name: "Artemis".into(),
                },
            ),
            entity_operation(
                "op_alias",
                ("kg_1", Some(&renamed)),
                ("kg_1", Some(&renamed_with_alias)),
                KnowledgeAction::AddAlias {
                    entity_id: "kg_1".into(),
                    alias: "Moonshot".into(),
                },
            ),
            operation(
                "op_undo_rename",
                KnowledgeAction::Undo {
                    operation_id: "op_rename".into(),
                },
            ),
        ]);

        assert_eq!(replay(&ledger).unwrap().registry["kg_1"], projected);
    }

    #[test]
    fn non_lifo_undo_keeps_later_merge_and_binding_for_the_same_key() {
        let mut ledger = KnowledgeLedger::empty();
        ledger.operations.extend([
            decision_operation(
                "op_merge_a",
                None,
                "kg_a",
                KnowledgeAction::MergeEntity {
                    source_id: "kg_source".into(),
                    target_id: "kg_a".into(),
                },
            ),
            decision_operation(
                "op_merge_b",
                Some("kg_a"),
                "kg_b",
                KnowledgeAction::MergeEntity {
                    source_id: "kg_source".into(),
                    target_id: "kg_b".into(),
                },
            ),
            decision_operation(
                "op_bind_a",
                None,
                "kg_a",
                KnowledgeAction::BindMention {
                    mention_id: "mn_1".into(),
                    entity_id: "kg_a".into(),
                },
            ),
            decision_operation(
                "op_bind_b",
                Some("kg_a"),
                "kg_b",
                KnowledgeAction::BindMention {
                    mention_id: "mn_1".into(),
                    entity_id: "kg_b".into(),
                },
            ),
            operation(
                "op_undo_merge_a",
                KnowledgeAction::Undo {
                    operation_id: "op_merge_a".into(),
                },
            ),
            operation(
                "op_undo_bind_a",
                KnowledgeAction::Undo {
                    operation_id: "op_bind_a".into(),
                },
            ),
        ]);

        let snapshot = replay(&ledger).unwrap();
        assert_eq!(snapshot.redirects["kg_source"], "kg_b");
        assert_eq!(snapshot.mention_bindings["mn_1"], "kg_b");
    }

    #[test]
    fn undo_compensates_alias_and_create_entity() {
        let original = registry_entity("project", "Apollo", &[]);
        let aliased = registry_entity("project", "Apollo", &["Moonshot"]);
        let created = registry_entity("term", "Rust", &[]);
        let mut ledger = KnowledgeLedger::empty();
        ledger.registry.insert("kg_1".into(), original.clone());
        ledger.operations.extend([
            entity_operation(
                "op_alias",
                ("kg_1", Some(&original)),
                ("kg_1", Some(&aliased)),
                KnowledgeAction::AddAlias {
                    entity_id: "kg_1".into(),
                    alias: "Moonshot".into(),
                },
            ),
            operation(
                "op_undo_alias",
                KnowledgeAction::Undo {
                    operation_id: "op_alias".into(),
                },
            ),
            entity_operation(
                "op_create",
                ("kg_created", None),
                ("kg_created", Some(&created)),
                KnowledgeAction::CreateEntity {
                    entity: created.clone(),
                },
            ),
            operation(
                "op_undo_create",
                KnowledgeAction::Undo {
                    operation_id: "op_create".into(),
                },
            ),
        ]);

        let snapshot = replay(&ledger).unwrap();
        assert_eq!(snapshot.registry["kg_1"], original);
        assert!(!snapshot.registry.contains_key("kg_created"));
    }

    #[test]
    fn undo_compensates_merge_and_mention_binding_decisions() {
        let mut ledger = KnowledgeLedger::empty();
        ledger.operations.extend([
            KnowledgeOperation {
                id: "op_merge".into(),
                at: "t".into(),
                before: Value::Null,
                after: Value::String("kg_b".into()),
                action: KnowledgeAction::MergeEntity {
                    source_id: "kg_a".into(),
                    target_id: "kg_b".into(),
                },
            },
            operation(
                "op_undo_merge",
                KnowledgeAction::Undo {
                    operation_id: "op_merge".into(),
                },
            ),
            KnowledgeOperation {
                id: "op_bind".into(),
                at: "t".into(),
                before: Value::Null,
                after: Value::String("kg_a".into()),
                action: KnowledgeAction::BindMention {
                    mention_id: "mn_1".into(),
                    entity_id: "kg_a".into(),
                },
            },
            operation(
                "op_undo_bind",
                KnowledgeAction::Undo {
                    operation_id: "op_bind".into(),
                },
            ),
        ]);

        let snapshot = replay(&ledger).unwrap();
        assert!(snapshot.redirects.is_empty());
        assert!(snapshot.mention_bindings.is_empty());
    }

    #[test]
    fn malformed_entity_snapshots_fail_closed() {
        let original = registry_entity("project", "Apollo", &[]);
        let renamed = registry_entity("project", "Artemis", &[]);
        let mut ledger = KnowledgeLedger::empty();
        ledger.registry.insert("kg_1".into(), renamed.clone());
        ledger.operations.push(KnowledgeOperation {
            id: "op_rename".into(),
            at: "t".into(),
            before: Value::Null,
            after: entity_state("kg_1", Some(&renamed)),
            action: KnowledgeAction::RenameEntity {
                entity_id: "kg_1".into(),
                name: "Artemis".into(),
            },
        });

        let error = replay(&ledger).unwrap_err();
        assert!(error.to_string().contains("op_rename"));
        assert!(error.to_string().contains("before"));
        assert_ne!(original, renamed);
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
        ledger.operations.push(decision_operation(
            "op_bind",
            None,
            "kg_bound",
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
    fn bound_mention_with_a_missing_terminal_is_pending_after_load_and_replay() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = KnowledgeLedger::empty();
        ledger.operations.push(decision_operation(
            "op_bind",
            None,
            "kg_missing",
            KnowledgeAction::BindMention {
                mention_id: "mn_1".into(),
                entity_id: "kg_missing".into(),
            },
        ));
        std::fs::write(
            root.path().join(crate::graph::overrides::KNOWLEDGE_FILE),
            serde_json::to_vec(&ledger).unwrap(),
        )
        .unwrap();
        let loaded = crate::graph::overrides::load(root.path()).unwrap();
        let snapshot = replay(&loaded).unwrap();

        let resolution = resolve_entity(
            &snapshot,
            &Voiceprints::default(),
            "n1",
            &local("ent_1", "project", "Apollo"),
            &["mn_1".into()],
        );

        assert_eq!(resolution.status, ResolutionStatus::PendingConflict);
        assert_eq!(resolution.entity_id, None);
        assert_eq!(resolution.candidates, vec!["kg_missing"]);
    }

    #[test]
    fn merge_with_a_missing_terminal_is_pending_after_load_and_replay() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = KnowledgeLedger::empty();
        ledger.operations.push(decision_operation(
            "op_merge",
            None,
            "kg_missing",
            KnowledgeAction::MergeEntity {
                source_id: "kg_source".into(),
                target_id: "kg_missing".into(),
            },
        ));
        std::fs::write(
            root.path().join(crate::graph::overrides::KNOWLEDGE_FILE),
            serde_json::to_vec(&ledger).unwrap(),
        )
        .unwrap();
        let loaded = crate::graph::overrides::load(root.path()).unwrap();
        let snapshot = replay(&loaded).unwrap();

        let resolution = resolve_entity(
            &snapshot,
            &Voiceprints::default(),
            "n1",
            &local("kg_source", "project", "Anything"),
            &[],
        );

        assert_eq!(resolution.status, ResolutionStatus::PendingConflict);
        assert_eq!(resolution.entity_id, None);
        assert_eq!(resolution.candidates, vec!["kg_missing"]);
    }

    #[test]
    fn bound_mention_accepts_and_canonicalizes_a_voiceprint_terminal() {
        let mut ledger = KnowledgeLedger::empty();
        ledger.operations.push(decision_operation(
            "op_bind",
            None,
            "P0",
            KnowledgeAction::BindMention {
                mention_id: "mn_1".into(),
                entity_id: "P0".into(),
            },
        ));
        let snapshot = replay(&ledger).unwrap();
        let people: Voiceprints = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "next_person": 2,
            "people": {"P1": {"name": "Ada"}},
            "redirects": {"P0": "P1"}
        }))
        .unwrap();

        let resolution = resolve_entity(
            &snapshot,
            &people,
            "n1",
            &local("ent_1", "person", "Ada"),
            &["mn_1".into()],
        );

        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.entity_id.as_deref(), Some("P1"));
    }

    #[test]
    fn stable_reference_composes_registry_and_voiceprint_redirects_and_detects_cross_cycles() {
        let mut ledger = KnowledgeLedger::empty();
        ledger
            .registry
            .insert("P2".into(), registry_entity("person", "Old", &[]));
        ledger
            .registry
            .insert("P1".into(), registry_entity("person", "New", &[]));
        let snapshot = replay(&ledger).unwrap();
        let people: Voiceprints = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "next_person": 3,
            "people": {"P1": {"name": "New"}},
            "redirects": {"P2": "P1"}
        }))
        .unwrap();
        assert_eq!(
            resolve_reference_id(&snapshot, &people, "P2")
                .entity_id
                .as_deref(),
            Some("P1")
        );

        let mut cyclic = KnowledgeLedger::empty();
        cyclic
            .registry
            .insert("P1".into(), registry_entity("person", "One", &[]));
        cyclic.operations.push(decision_operation(
            "merge",
            None,
            "P2",
            KnowledgeAction::MergeEntity {
                source_id: "P1".into(),
                target_id: "P2".into(),
            },
        ));
        let cyclic_snapshot = replay(&cyclic).unwrap();
        let cyclic_people: Voiceprints = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "next_person": 3,
            "people": {"P1": {"name": "One"}},
            "redirects": {"P2": "P1"}
        }))
        .unwrap();
        let conflict = resolve_reference_id(&cyclic_snapshot, &cyclic_people, "P1");
        assert_eq!(conflict.status, ResolutionStatus::PendingConflict);
        assert!(conflict.reason.as_deref().unwrap().contains("cycle"));
    }

    #[test]
    fn conflicting_registry_and_voiceprint_redirects_are_pending_everywhere() {
        let mut ledger = KnowledgeLedger::empty();
        for (id, name) in [("P2", "Old"), ("kg_x", "Registry target")] {
            ledger
                .registry
                .insert(id.into(), registry_entity("person", name, &[]));
        }
        ledger.operations.push(decision_operation(
            "merge",
            None,
            "kg_x",
            KnowledgeAction::MergeEntity {
                source_id: "P2".into(),
                target_id: "kg_x".into(),
            },
        ));
        let snapshot = replay(&ledger).unwrap();
        let people: Voiceprints = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "next_person": 3,
            "people": {"P1": {"name": "Voice target"}},
            "redirects": {"P2": "P1"}
        }))
        .unwrap();

        let reference = resolve_reference_id(&snapshot, &people, "P2");
        let mention = resolve_entity(&snapshot, &people, "n1", &local("P2", "person", "Old"), &[]);
        for resolution in [reference, mention] {
            assert_eq!(resolution.status, ResolutionStatus::PendingConflict);
            assert_eq!(resolution.entity_id, None);
            assert_eq!(resolution.candidates, vec!["P1", "kg_x"]);
        }
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
            decision_operation(
                "op_1",
                None,
                "kg_target",
                KnowledgeAction::MergeEntity {
                    source_id: "kg_a".into(),
                    target_id: "kg_target".into(),
                },
            ),
            decision_operation(
                "op_2",
                None,
                "kg_target",
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
    fn legacy_mapping_into_a_redirect_cycle_loads_as_pending_conflict() {
        let root = tempfile::tempdir().unwrap();
        let mut ledger = KnowledgeLedger::empty();
        ledger.legacy_ids.insert("e:old".into(), "kg_a".into());
        ledger.operations.extend([
            decision_operation(
                "op_merge_a",
                None,
                "kg_b",
                KnowledgeAction::MergeEntity {
                    source_id: "kg_a".into(),
                    target_id: "kg_b".into(),
                },
            ),
            decision_operation(
                "op_merge_b",
                None,
                "kg_a",
                KnowledgeAction::MergeEntity {
                    source_id: "kg_b".into(),
                    target_id: "kg_a".into(),
                },
            ),
        ]);
        std::fs::write(
            root.path().join(crate::graph::overrides::KNOWLEDGE_FILE),
            serde_json::to_vec(&ledger).unwrap(),
        )
        .unwrap();

        let loaded = crate::graph::overrides::load(root.path()).unwrap();
        let snapshot = replay(&loaded).unwrap();
        let resolution = resolve_entity(
            &snapshot,
            &Voiceprints::default(),
            "n1",
            &local("e:old", "project", "Anything"),
            &[],
        );

        assert_eq!(resolution.status, ResolutionStatus::PendingConflict);
        assert_eq!(resolution.entity_id, None);
        assert!(resolution.reason.unwrap().contains("cycle"));
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
            decision_operation(
                "op_1",
                None,
                "kg_b",
                KnowledgeAction::MergeEntity {
                    source_id: "kg_a".into(),
                    target_id: "kg_b".into(),
                },
            ),
            decision_operation(
                "op_2",
                None,
                "kg_a",
                KnowledgeAction::MergeEntity {
                    source_id: "kg_b".into(),
                    target_id: "kg_a".into(),
                },
            ),
            decision_operation(
                "op_3",
                None,
                "kg_a",
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

    #[test]
    fn undo_and_undo_of_undo_toggle_restore_relation_semantics() {
        let suppress = operation(
            "op_suppress",
            KnowledgeAction::SuppressRelation {
                subject_id: "kg_a".into(),
                predicate: RelationPredicate {
                    kind: "uses".into(),
                    label: None,
                },
                object_id: "kg_b".into(),
            },
        );
        let restore = operation(
            "op_restore",
            KnowledgeAction::RestoreRelation {
                operation_id: "op_suppress".into(),
            },
        );
        let undo_restore = operation(
            "op_undo_restore",
            KnowledgeAction::Undo {
                operation_id: "op_restore".into(),
            },
        );

        let mut direct = KnowledgeLedger::empty();
        direct.operations = vec![suppress.clone(), restore.clone(), undo_restore.clone()];
        let direct_snapshot = replay(&direct).unwrap();
        assert_eq!(direct_snapshot.relation_decisions.suppressed.len(), 1);
        assert!(direct_snapshot
            .relation_decisions
            .restored_operations
            .is_empty());

        let mut nested = KnowledgeLedger::empty();
        nested.operations = vec![
            suppress,
            restore,
            undo_restore,
            operation(
                "op_undo_undo_restore",
                KnowledgeAction::Undo {
                    operation_id: "op_undo_restore".into(),
                },
            ),
        ];
        let nested_snapshot = replay(&nested).unwrap();
        assert!(nested_snapshot.relation_decisions.suppressed.is_empty());
        assert!(nested_snapshot
            .relation_decisions
            .restored_operations
            .contains("op_suppress"));
    }

    #[test]
    fn restore_relation_accepts_an_earlier_end_decision() {
        let mut ledger = KnowledgeLedger::empty();
        ledger.operations = vec![
            operation(
                "op_end",
                KnowledgeAction::EndRelation {
                    relation_id: "rf_1".into(),
                    valid_to: "2026-07-20".into(),
                },
            ),
            operation(
                "op_restore",
                KnowledgeAction::RestoreRelation {
                    operation_id: "op_end".into(),
                },
            ),
        ];

        let snapshot = replay(&ledger).unwrap();

        assert!(!snapshot.relation_decisions.ended.contains_key("rf_1"));
        assert!(snapshot
            .relation_decisions
            .restored_operations
            .contains("op_end"));
    }
}
