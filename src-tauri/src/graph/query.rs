use super::{index, overrides, resolve};
use crate::ipc;
use crate::store::{self, RelationPredicate, VoiceprintStore};
use rusqlite::types::Value;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::Path;

const DEGRADED_MESSAGE: &str = "知识整理记录损坏，当前显示上次可用索引";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GraphFilter {
    pub entity_kinds: Vec<String>,
    pub predicate_types: Vec<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub include_history: bool,
    pub include_cooccurrence: bool,
}

struct LedgerView {
    ledger: Option<overrides::KnowledgeLedger>,
    degraded: bool,
}

pub(crate) struct ReadContext {
    connection: rusqlite::Connection,
    ledger: LedgerView,
}

#[derive(Debug)]
struct NodeRecord {
    summary: ipc::EntitySummary,
}

fn ledger_view(data_root: &Path) -> LedgerView {
    let bytes = match std::fs::read(data_root.join(overrides::KNOWLEDGE_FILE)) {
        Ok(bytes) => bytes,
        Err(_) => {
            return LedgerView {
                ledger: None,
                degraded: true,
            }
        }
    };
    match serde_json::from_slice::<overrides::KnowledgeLedger>(&bytes) {
        Ok(ledger) => LedgerView {
            degraded: !ledger_is_valid_for_read(data_root, &ledger),
            ledger: Some(ledger),
        },
        Err(_) => LedgerView {
            ledger: None,
            degraded: true,
        },
    }
}

pub(crate) fn open_read_context(data_root: &Path) -> anyhow::Result<ReadContext> {
    Ok(ReadContext {
        ledger: ledger_view(data_root),
        connection: index::open_readonly(data_root)?,
    })
}

fn canonical_set(values: &[String]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn ledger_is_valid_for_read(data_root: &Path, ledger: &overrides::KnowledgeLedger) -> bool {
    if ledger.schema_version != 1
        || ledger
            .registry
            .values()
            .any(|entity| !canonical_set(&entity.aliases))
        || ledger.operations.iter().any(|operation| {
            operation.id.is_empty()
                || matches!(
                    &operation.action,
                    overrides::KnowledgeAction::CreateEntity { entity }
                        if !canonical_set(&entity.aliases)
                )
                || matches!(
                    &operation.action,
                    overrides::KnowledgeAction::CreateRelation { relation }
                        if !canonical_set(&relation.evidence_ids)
                            || (!relation.user_assertion && relation.evidence_ids.is_empty())
                )
        })
        || overrides::active_operation_mask(&ledger.operations).is_err()
        || overrides::registry_baseline(ledger).is_err()
    {
        return false;
    }
    let Ok(snapshot) = resolve::replay(ledger) else {
        return false;
    };
    let people = VoiceprintStore::new(data_root.to_path_buf()).load();
    let mut references = BTreeSet::new();
    references.extend(ledger.registry.keys().cloned());
    references.extend(ledger.legacy_ids.values().cloned());
    references.extend(snapshot.redirects.keys().cloned());
    references.extend(snapshot.redirects.values().cloned());
    references.extend(people.redirects.keys().cloned());
    references.extend(people.redirects.values().cloned());
    references.into_iter().all(|entity_id| {
        resolve::resolve_reference_id(&snapshot, &people, &entity_id)
            .entity_id
            .is_some()
    })
}

pub(crate) fn resolve_public_entity_id(
    data_root: &Path,
    entity_id: &str,
    ledger: Option<&overrides::KnowledgeLedger>,
) -> Option<String> {
    let Some(ledger) = ledger else {
        return Some(entity_id.to_string());
    };
    let candidate = ledger
        .legacy_ids
        .get(entity_id)
        .map(String::as_str)
        .unwrap_or(entity_id);
    let snapshot = resolve::replay(ledger).ok()?;
    let people = VoiceprintStore::new(data_root.to_path_buf()).load();
    resolve::resolve_reference_id(&snapshot, &people, candidate).entity_id
}

pub(crate) fn resolve_entity_from_context(
    data_root: &Path,
    context: &ReadContext,
    entity_id: &str,
) -> Option<String> {
    resolve_public_entity_id(data_root, entity_id, context.ledger.ledger.as_ref())
}

fn normalized_values(values: &[String]) -> Vec<String> {
    let mut values = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

fn push_text_filter(sql: &mut String, params: &mut Vec<Value>, column: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    sql.push_str(" AND ");
    sql.push_str(column);
    sql.push_str(" IN (");
    sql.push_str(&vec!["?"; values.len()].join(","));
    sql.push(')');
    params.extend(values.iter().cloned().map(Value::Text));
}

fn validate_filter(filter: &GraphFilter) -> anyhow::Result<()> {
    let from = filter
        .from
        .as_deref()
        .map(chrono::DateTime::parse_from_rfc3339)
        .transpose()
        .map_err(|_| anyhow::anyhow!("from must be an RFC3339 timestamp"))?;
    let to = filter
        .to
        .as_deref()
        .map(chrono::DateTime::parse_from_rfc3339)
        .transpose()
        .map_err(|_| anyhow::anyhow!("to must be an RFC3339 timestamp"))?;
    if let (Some(from), Some(to)) = (from, to) {
        anyhow::ensure!(from <= to, "date range start must not be after its end");
    }
    Ok(())
}

fn query_nodes(
    connection: &rusqlite::Connection,
    kinds: &[String],
) -> anyhow::Result<Vec<NodeRecord>> {
    let mut sql = String::from(
        "SELECT entity.id, entity.kind, entity.name, entity.aliases, entity.is_person, \
         count(DISTINCT note_entity.note_id), \
         coalesce(sum(note_entity.mention_count), 0) \
         FROM entities entity \
         LEFT JOIN note_entities note_entity ON note_entity.entity_id = entity.id \
         WHERE 1 = 1",
    );
    let mut params = Vec::new();
    push_text_filter(&mut sql, &mut params, "entity.kind", kinds);
    sql.push_str(
        " GROUP BY entity.id, entity.kind, entity.name, entity.aliases, entity.is_person \
         ORDER BY entity.id",
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(params.iter()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;
    rows.map(|row| {
        let (id, kind, name, aliases, is_person, note_count, mention_total) = row?;
        Ok(NodeRecord {
            summary: ipc::EntitySummary {
                id,
                kind,
                name,
                aliases: serde_json::from_str(&aliases)?,
                is_person: is_person != 0,
                note_count,
                mention_total,
            },
        })
    })
    .collect()
}

fn query_semantic_edges(
    connection: &rusqlite::Connection,
    filter: &GraphFilter,
    kinds: &[String],
    predicates: &[String],
) -> anyhow::Result<Vec<ipc::SemanticEdge>> {
    let mut sql = String::from(
        "SELECT relation.id, relation.subject_id, relation.object_id, \
         relation.predicate_type, relation.predicate_label, relation.status, \
         relation.confidence, relation.origin, \
         (SELECT count(*) FROM relation_evidence evidence WHERE evidence.relation_id = relation.id), \
         relation.note_ids, relation.valid_from, relation.valid_to \
         FROM relations relation \
         JOIN entities subject ON subject.id = relation.subject_id \
         JOIN entities object ON object.id = relation.object_id WHERE 1 = 1",
    );
    let mut params = Vec::new();
    if !filter.include_history {
        sql.push_str(" AND relation.status = ?");
        params.push(Value::Text("current".into()));
    }
    push_text_filter(&mut sql, &mut params, "relation.predicate_type", predicates);
    if !kinds.is_empty() {
        push_text_filter(&mut sql, &mut params, "subject.kind", kinds);
        push_text_filter(&mut sql, &mut params, "object.kind", kinds);
    }
    if let Some(from) = &filter.from {
        sql.push_str(
            " AND (relation.valid_to IS NULL OR julianday(relation.valid_to) >= julianday(?))",
        );
        params.push(Value::Text(from.clone()));
    }
    if let Some(to) = &filter.to {
        sql.push_str(
            " AND (relation.valid_from IS NULL OR julianday(relation.valid_from) <= julianday(?))",
        );
        params.push(Value::Text(to.clone()));
    }
    sql.push_str(" ORDER BY relation.id");
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(params.iter()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, f64>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, i64>(8)?,
            row.get::<_, String>(9)?,
            row.get::<_, Option<String>>(10)?,
            row.get::<_, Option<String>>(11)?,
        ))
    })?;
    rows.map(|row| {
        let (
            id,
            subject_id,
            object_id,
            predicate_type,
            predicate_label,
            status,
            confidence,
            origin,
            evidence_count,
            note_ids,
            valid_from,
            valid_to,
        ) = row?;
        let note_ids: Vec<String> = serde_json::from_str(&note_ids)?;
        Ok(ipc::SemanticEdge {
            id,
            subject_id,
            object_id,
            predicate_type,
            predicate_label,
            status,
            confidence,
            origin,
            evidence_count,
            note_count: note_ids.len() as i64,
            valid_from,
            valid_to,
        })
    })
    .collect()
}

fn query_cooccurrence_edges(
    connection: &rusqlite::Connection,
    live_nodes: &BTreeSet<String>,
) -> anyhow::Result<Vec<ipc::EdgeRow>> {
    let mut statement = connection.prepare(
        "SELECT left_entity.entity_id, right_entity.entity_id, count(*) \
         FROM note_entities left_entity \
         JOIN note_entities right_entity \
           ON right_entity.note_id = left_entity.note_id \
          AND right_entity.entity_id > left_entity.entity_id \
         GROUP BY left_entity.entity_id, right_entity.entity_id \
         ORDER BY left_entity.entity_id, right_entity.entity_id",
    )?;
    let edges = statement
        .query_map([], |row| {
            Ok(ipc::EdgeRow {
                a: row.get(0)?,
                b: row.get(1)?,
                weight: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|edge| live_nodes.contains(&edge.a) && live_nodes.contains(&edge.b))
        .collect();
    Ok(edges)
}

pub fn semantic_graph(
    data_root: &Path,
    filter: &GraphFilter,
) -> anyhow::Result<ipc::SemanticGraphData> {
    let context = open_read_context(data_root)?;
    semantic_graph_from_context(&context, filter)
}

pub(crate) fn semantic_graph_from_context(
    context: &ReadContext,
    filter: &GraphFilter,
) -> anyhow::Result<ipc::SemanticGraphData> {
    validate_filter(filter)?;
    let kinds = normalized_values(&filter.entity_kinds);
    let predicates = normalized_values(&filter.predicate_types);
    let mut nodes = query_nodes(&context.connection, &kinds)?;
    let semantic_edges = query_semantic_edges(&context.connection, filter, &kinds, &predicates)?;
    if filter.include_history
        || !predicates.is_empty()
        || filter.from.is_some()
        || filter.to.is_some()
    {
        let admitted_endpoints = semantic_edges
            .iter()
            .flat_map(|edge| [&edge.subject_id, &edge.object_id])
            .collect::<BTreeSet<_>>();
        nodes.retain(|node| admitted_endpoints.contains(&node.summary.id));
    }
    let live_nodes = nodes
        .iter()
        .map(|node| node.summary.id.clone())
        .collect::<BTreeSet<_>>();
    let cooccurrence_edges = if filter.include_cooccurrence {
        query_cooccurrence_edges(&context.connection, &live_nodes)?
    } else {
        Vec::new()
    };
    Ok(ipc::SemanticGraphData {
        nodes: nodes.into_iter().map(|node| node.summary).collect(),
        semantic_edges,
        cooccurrence_edges,
        degraded: context.ledger.degraded,
        message: context.ledger.degraded.then(|| DEGRADED_MESSAGE.into()),
    })
}

pub fn semantic_entity_detail(
    data_root: &Path,
    entity_id: &str,
    filter: &GraphFilter,
) -> anyhow::Result<Option<ipc::SemanticEntityDetail>> {
    let context = open_read_context(data_root)?;
    semantic_entity_detail_from_context(data_root, &context, entity_id, filter)
}

fn semantic_entity_detail_from_context(
    data_root: &Path,
    context: &ReadContext,
    entity_id: &str,
    filter: &GraphFilter,
) -> anyhow::Result<Option<ipc::SemanticEntityDetail>> {
    let Some(entity_id) = resolve_entity_from_context(data_root, context, entity_id) else {
        return Ok(None);
    };
    let graph = semantic_graph_from_context(context, filter)?;
    let Some(summary) = graph.nodes.into_iter().find(|node| node.id == entity_id) else {
        return Ok(None);
    };
    let confirmed = context.connection.query_row(
        "SELECT confirmed FROM entities WHERE id = ?",
        [&entity_id],
        |row| row.get::<_, i64>(0),
    )? != 0;
    let relations = graph
        .semantic_edges
        .into_iter()
        .filter(|edge| edge.subject_id == entity_id || edge.object_id == entity_id)
        .collect();
    Ok(Some(ipc::SemanticEntityDetail {
        id: summary.id,
        kind: summary.kind,
        name: summary.name,
        aliases: summary.aliases,
        confirmed,
        is_person: summary.is_person,
        note_count: summary.note_count,
        mention_total: summary.mention_total,
        relations,
        degraded: graph.degraded,
        message: graph.message,
    }))
}

fn relation_row(
    connection: &rusqlite::Connection,
    relation_id: &str,
) -> anyhow::Result<
    Option<(
        ipc::SemanticEdge,
        Option<String>,
        Option<String>,
        Vec<String>,
    )>,
> {
    let row = connection.query_row(
        "SELECT relation.id, relation.subject_id, relation.object_id, relation.predicate_type, \
         relation.predicate_label, relation.status, relation.confidence, relation.origin, \
         (SELECT count(*) FROM relation_evidence evidence WHERE evidence.relation_id = relation.id), \
         relation.note_ids, relation.valid_from, relation.valid_to, relation.provider, relation.model \
         FROM relations relation WHERE relation.id = ?",
        [relation_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, f64>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, Option<String>>(13)?,
            ))
        },
    );
    let row = match row {
        Ok(row) => row,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let note_ids: Vec<String> = serde_json::from_str(&row.9)?;
    Ok(Some((
        ipc::SemanticEdge {
            id: row.0,
            subject_id: row.1,
            object_id: row.2,
            predicate_type: row.3,
            predicate_label: row.4,
            status: row.5,
            confidence: row.6,
            origin: row.7,
            evidence_count: row.8,
            note_count: note_ids.len() as i64,
            valid_from: row.10,
            valid_to: row.11,
        },
        row.12,
        row.13,
        note_ids,
    )))
}

pub fn relation_detail(
    data_root: &Path,
    relation_id: &str,
) -> anyhow::Result<Option<ipc::RelationDetail>> {
    let connection = index::open_readonly(data_root)?;
    let Some((relation, provider, model, note_ids)) = relation_row(&connection, relation_id)?
    else {
        return Ok(None);
    };
    let mut statement = connection.prepare(
        "SELECT id, note_id, paragraph_index, start_offset, end_offset, quote, source_seqs, \
         source_hash, subject_mentions, object_mentions \
         FROM relation_evidence WHERE relation_id = ? ORDER BY id",
    )?;
    let rows = statement.query_map([relation_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, String>(9)?,
        ))
    })?;
    let evidence = rows
        .map(|row| {
            let row = row?;
            Ok(ipc::RelationEvidence {
                id: row.0,
                note_id: row.1,
                paragraph_index: row.2,
                start_offset: row.3,
                end_offset: row.4,
                quote: row.5,
                source_seqs: serde_json::from_str(&row.6)?,
                source_hash: row.7,
                subject_mentions: serde_json::from_str(&row.8)?,
                object_mentions: serde_json::from_str(&row.9)?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Some(ipc::RelationDetail {
        relation,
        provider,
        model,
        note_ids,
        evidence,
    }))
}

fn pending_payload_string<'a>(
    payload: &'a serde_json::Value,
    pointers: &[&str],
) -> Option<&'a str> {
    pointers
        .iter()
        .find_map(|pointer| payload.pointer(pointer).and_then(serde_json::Value::as_str))
}

fn pending_matches_filter(
    payload: &serde_json::Value,
    filter: &GraphFilter,
    kinds: &[String],
    predicates: &[String],
) -> bool {
    if !predicates.is_empty() {
        if let Some(predicate) = pending_payload_string(
            payload,
            &[
                "/predicate_type",
                "/predicate/kind",
                "/relation/predicate_type",
                "/relation/predicate/kind",
            ],
        ) {
            if !predicates.iter().any(|allowed| allowed == predicate) {
                return false;
            }
        }
    }
    if !kinds.is_empty() {
        for pointer in [
            "/entity_kind",
            "/subject_kind",
            "/object_kind",
            "/relation/subject_kind",
            "/relation/object_kind",
        ] {
            if let Some(kind) = payload.pointer(pointer).and_then(serde_json::Value::as_str) {
                if !kinds.iter().any(|allowed| allowed == kind) {
                    return false;
                }
            }
        }
    }
    if !filter.include_history {
        if pending_payload_string(payload, &["/status", "/relation/status"])
            .is_some_and(|status| status == "historical")
        {
            return false;
        }
    }
    if let (Some(from), Some(valid_to)) = (
        filter
            .from
            .as_deref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok()),
        pending_payload_string(payload, &["/valid_to", "/relation/valid_to"])
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok()),
    ) {
        if valid_to < from {
            return false;
        }
    }
    if let (Some(to), Some(valid_from)) = (
        filter
            .to
            .as_deref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok()),
        pending_payload_string(payload, &["/valid_from", "/relation/valid_from"])
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok()),
    ) {
        if valid_from > to {
            return false;
        }
    }
    true
}

pub fn pending_review(
    data_root: &Path,
    filter: &GraphFilter,
) -> anyhow::Result<Vec<ipc::PendingReviewItem>> {
    let context = open_read_context(data_root)?;
    pending_review_from_context(&context, filter)
}

fn pending_review_from_context(
    context: &ReadContext,
    filter: &GraphFilter,
) -> anyhow::Result<Vec<ipc::PendingReviewItem>> {
    validate_filter(filter)?;
    let predicates = normalized_values(&filter.predicate_types);
    let kinds = normalized_values(&filter.entity_kinds);
    let mut statement = context.connection.prepare(
        "SELECT id, kind, note_id, relation_id, payload FROM pending_review ORDER BY kind, id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    rows.map(|row| {
        let (id, kind, note_id, relation_id, payload) = row?;
        Ok(ipc::PendingReviewItem {
            id,
            kind,
            note_id,
            relation_id,
            payload: serde_json::from_str(&payload)?,
        })
    })
    .collect::<anyhow::Result<Vec<_>>>()
    .map(|rows| {
        rows.into_iter()
            .filter(|row| pending_matches_filter(&row.payload, filter, &kinds, &predicates))
            .collect()
    })
}

pub fn entity_mentions(
    data_root: &Path,
    entity_id: &str,
) -> anyhow::Result<Vec<ipc::MentionEvidence>> {
    let context = open_read_context(data_root)?;
    let Some(entity_id) = resolve_entity_from_context(data_root, &context, entity_id) else {
        return Ok(Vec::new());
    };
    let mut statement = context.connection.prepare(
        "SELECT id, note_id, entity_id, paragraph_index, start_offset, end_offset, quote \
         FROM entity_mentions WHERE entity_id = ? \
         ORDER BY note_id, paragraph_index, start_offset, id",
    )?;
    let mentions = statement
        .query_map([entity_id], |row| {
            Ok(ipc::MentionEvidence {
                id: row.get(0)?,
                note_id: row.get(1)?,
                entity_id: row.get(2)?,
                paragraph_index: row.get(3)?,
                start_offset: row.get(4)?,
                end_offset: row.get(5)?,
                quote: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(mentions)
}

fn operation_result(
    operation_id: String,
    entity_id: Option<String>,
) -> ipc::KnowledgeMutationResult {
    ipc::KnowledgeMutationResult {
        operation_id,
        entity_id,
        rebuild_state: "committed".into(),
    }
}

fn operation_identity(
    ledger: &overrides::KnowledgeLedger,
    at: &str,
    kind: &str,
    payload: &serde_json::Value,
) -> String {
    let mut nonce = 0_u64;
    loop {
        let id = store::stable_id(
            "op_",
            &[
                at.into(),
                ledger.operations.len().to_string(),
                kind.into(),
                payload.to_string(),
                nonce.to_string(),
            ],
        );
        if ledger.operations.iter().all(|operation| operation.id != id) {
            return id;
        }
        nonce = nonce
            .checked_add(1)
            .expect("knowledge operation identity space exhausted");
    }
}

fn next_operation_time(ledger: &overrides::KnowledgeLedger) -> anyhow::Result<String> {
    let now = chrono::Utc::now();
    let latest = ledger
        .operations
        .iter()
        .filter_map(|operation| chrono::DateTime::parse_from_rfc3339(&operation.at).ok())
        .map(|timestamp| timestamp.with_timezone(&chrono::Utc))
        .max();
    let timestamp = match latest {
        Some(latest) if latest >= now => latest
            .checked_add_signed(chrono::Duration::nanoseconds(1))
            .ok_or_else(|| anyhow::anyhow!("知识操作时间戳已耗尽"))?,
        _ => now,
    };
    Ok(timestamp.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
}

fn entity_state(
    entity_id: &str,
    entity: Option<overrides::RegistryEntity>,
) -> anyhow::Result<serde_json::Value> {
    Ok(serde_json::to_value(overrides::RegistryState {
        entity_id: entity_id.into(),
        entity,
    })?)
}

fn canonical_mutation_entity(
    data_root: &Path,
    ledger: &overrides::KnowledgeLedger,
    entity_id: &str,
) -> anyhow::Result<String> {
    let snapshot = resolve::replay(ledger)?;
    let people = VoiceprintStore::new(data_root.to_path_buf()).load();
    let candidate = ledger
        .legacy_ids
        .get(entity_id)
        .map(String::as_str)
        .unwrap_or(entity_id);
    let resolution = resolve::resolve_reference_id(&snapshot, &people, candidate);
    resolution
        .entity_id
        .ok_or_else(|| anyhow::anyhow!("实体不存在或重定向冲突"))
}

fn require_nonempty(value: &str, message: &str) -> anyhow::Result<String> {
    let value = value.trim();
    anyhow::ensure!(!value.is_empty(), "{message}");
    Ok(value.into())
}

fn normalized_predicate(mut predicate: RelationPredicate) -> anyhow::Result<RelationPredicate> {
    predicate.kind = predicate.kind.trim().into();
    if predicate.kind == "custom" {
        predicate.label = Some(require_nonempty(
            predicate.label.as_deref().unwrap_or_default(),
            "自定义关系必须填写名称",
        )?);
    } else {
        anyhow::ensure!(
            store::aing_graph::CORE_PREDICATES.contains(&predicate.kind.as_str()),
            "关系类型无效"
        );
        predicate.label = None;
    }
    Ok(predicate)
}

fn validate_interval(from: Option<&str>, to: Option<&str>) -> anyhow::Result<()> {
    let from = from
        .map(chrono::DateTime::parse_from_rfc3339)
        .transpose()
        .map_err(|_| anyhow::anyhow!("关系时间必须使用 RFC3339 格式"))?;
    let to = to
        .map(chrono::DateTime::parse_from_rfc3339)
        .transpose()
        .map_err(|_| anyhow::anyhow!("关系时间必须使用 RFC3339 格式"))?;
    if let (Some(from), Some(to)) = (from, to) {
        anyhow::ensure!(from <= to, "关系开始时间不能晚于结束时间");
    }
    Ok(())
}

fn relation_exists(data_root: &Path, relation_id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(relation_id.starts_with("cr_"), "关系 ID 必须是公开稳定 ID");
    let connection = index::open_readonly(data_root)?;
    let exists = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM relations WHERE id = ?)",
        [relation_id],
        |row| row.get::<_, bool>(0),
    )?;
    anyhow::ensure!(exists, "关系不存在");
    Ok(())
}

fn evidence_ids_exist(data_root: &Path, evidence_ids: &[String]) -> anyhow::Result<()> {
    if evidence_ids.is_empty() {
        return Ok(());
    }
    let connection = index::open_readonly(data_root)?;
    let mut statement =
        connection.prepare("SELECT EXISTS(SELECT 1 FROM relation_evidence WHERE id = ?)")?;
    for evidence_id in evidence_ids {
        anyhow::ensure!(
            statement.query_row([evidence_id], |row| row.get::<_, bool>(0))?,
            "关系证据不存在"
        );
    }
    Ok(())
}

fn push_operation(
    ledger: &mut overrides::KnowledgeLedger,
    id: String,
    at: &str,
    before: serde_json::Value,
    after: serde_json::Value,
    action: overrides::KnowledgeAction,
) {
    ledger.operations.push(overrides::KnowledgeOperation {
        id,
        at: at.into(),
        before,
        after,
        action,
    });
}

pub(crate) fn apply_operation(
    data_root: &Path,
    input: &ipc::KnowledgeOperationInput,
) -> anyhow::Result<ipc::KnowledgeMutationResult> {
    // Explicit load is the fail-closed gate. In particular, `update` is never reached for a
    // corrupt ledger, so no rebuild can be queued by its caller.
    let initial_ledger = overrides::load(data_root)?;
    match input {
        ipc::KnowledgeOperationInput::ConfirmRelation { relation_id }
        | ipc::KnowledgeOperationInput::EditRelation { relation_id, .. }
        | ipc::KnowledgeOperationInput::EndRelation { relation_id, .. } => {
            relation_exists(data_root, relation_id)?;
        }
        ipc::KnowledgeOperationInput::CreateRelation {
            evidence_ids,
            user_assertion,
            ..
        } => {
            anyhow::ensure!(
                *user_assertion || !evidence_ids.is_empty(),
                "关系必须有证据或明确标记为用户声明"
            );
            evidence_ids_exist(data_root, evidence_ids)?;
        }
        ipc::KnowledgeOperationInput::BindMention {
            mention_id,
            entity_id,
        } => {
            canonical_mutation_entity(data_root, &initial_ledger, entity_id)?;
            let connection = index::open_readonly(data_root)?;
            let exists = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM entity_mentions WHERE id = ?)",
                [mention_id],
                |row| row.get::<_, bool>(0),
            )?;
            anyhow::ensure!(exists, "提及不存在");
        }
        _ => {}
    }
    overrides::update(data_root, |ledger| {
        let at = next_operation_time(ledger)?;
        let (kind, payload) = match input {
            ipc::KnowledgeOperationInput::RenameEntity { entity_id, name } => {
                ("rename_entity", serde_json::json!([entity_id, name]))
            }
            ipc::KnowledgeOperationInput::AddAlias { entity_id, alias } => {
                ("add_alias", serde_json::json!([entity_id, alias]))
            }
            ipc::KnowledgeOperationInput::RemoveAlias { entity_id, alias } => {
                ("remove_alias", serde_json::json!([entity_id, alias]))
            }
            ipc::KnowledgeOperationInput::BindMention {
                mention_id,
                entity_id,
            } => ("bind_mention", serde_json::json!([mention_id, entity_id])),
            ipc::KnowledgeOperationInput::ConfirmRelation { relation_id } => {
                ("confirm_relation", serde_json::json!([relation_id]))
            }
            ipc::KnowledgeOperationInput::EditRelation { relation_id, .. } => {
                ("edit_relation", serde_json::json!([relation_id]))
            }
            ipc::KnowledgeOperationInput::SuppressRelation {
                subject_id,
                object_id,
                ..
            } => (
                "suppress_relation",
                serde_json::json!([subject_id, object_id]),
            ),
            ipc::KnowledgeOperationInput::EndRelation {
                relation_id,
                valid_to,
            } => ("end_relation", serde_json::json!([relation_id, valid_to])),
            ipc::KnowledgeOperationInput::RestoreRelation { operation_id } => {
                ("restore_relation", serde_json::json!([operation_id]))
            }
            ipc::KnowledgeOperationInput::CreateEntity { kind, name, .. } => {
                ("create_entity", serde_json::json!([kind, name]))
            }
            ipc::KnowledgeOperationInput::CreateRelation {
                subject_id,
                object_id,
                ..
            } => (
                "create_relation",
                serde_json::json!([subject_id, object_id]),
            ),
        };
        let operation_id = operation_identity(ledger, &at, kind, &payload);
        let mut result_entity_id = None;
        match input.clone() {
            ipc::KnowledgeOperationInput::RenameEntity { entity_id, name } => {
                let entity_id = canonical_mutation_entity(data_root, ledger, &entity_id)?;
                let name = require_nonempty(&name, "实体名称不能为空")?;
                let before = ledger
                    .registry
                    .get(&entity_id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("实体不支持改名"))?;
                anyhow::ensure!(before.name != name, "实体名称没有变化");
                let mut after = before.clone();
                after.name = name.clone();
                ledger.registry.insert(entity_id.clone(), after.clone());
                push_operation(
                    ledger,
                    operation_id.clone(),
                    &at,
                    entity_state(&entity_id, Some(before))?,
                    entity_state(&entity_id, Some(after))?,
                    overrides::KnowledgeAction::RenameEntity { entity_id, name },
                );
            }
            ipc::KnowledgeOperationInput::AddAlias { entity_id, alias } => {
                let entity_id = canonical_mutation_entity(data_root, ledger, &entity_id)?;
                let alias = require_nonempty(&alias, "别名不能为空")?;
                let before = ledger
                    .registry
                    .get(&entity_id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("实体不支持别名修改"))?;
                anyhow::ensure!(!before.aliases.contains(&alias), "别名已存在");
                let mut after = before.clone();
                after.aliases.push(alias.clone());
                after.aliases.sort();
                after.aliases.dedup();
                ledger.registry.insert(entity_id.clone(), after.clone());
                push_operation(
                    ledger,
                    operation_id.clone(),
                    &at,
                    entity_state(&entity_id, Some(before))?,
                    entity_state(&entity_id, Some(after))?,
                    overrides::KnowledgeAction::AddAlias { entity_id, alias },
                );
            }
            ipc::KnowledgeOperationInput::RemoveAlias { entity_id, alias } => {
                let entity_id = canonical_mutation_entity(data_root, ledger, &entity_id)?;
                let alias = require_nonempty(&alias, "别名不能为空")?;
                let before = ledger
                    .registry
                    .get(&entity_id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("实体不支持别名修改"))?;
                anyhow::ensure!(before.aliases.contains(&alias), "别名不存在");
                let mut after = before.clone();
                after.aliases.retain(|value| value != &alias);
                ledger.registry.insert(entity_id.clone(), after.clone());
                push_operation(
                    ledger,
                    operation_id.clone(),
                    &at,
                    entity_state(&entity_id, Some(before))?,
                    entity_state(&entity_id, Some(after))?,
                    overrides::KnowledgeAction::RemoveAlias { entity_id, alias },
                );
            }
            ipc::KnowledgeOperationInput::BindMention {
                mention_id,
                entity_id,
            } => {
                let entity_id = canonical_mutation_entity(data_root, ledger, &entity_id)?;
                let snapshot = resolve::replay(ledger)?;
                let before = snapshot.mention_bindings.get(&mention_id).cloned();
                anyhow::ensure!(before.as_deref() != Some(&entity_id), "提及绑定没有变化");
                push_operation(
                    ledger,
                    operation_id.clone(),
                    &at,
                    before.map_or(serde_json::Value::Null, serde_json::Value::String),
                    serde_json::Value::String(entity_id.clone()),
                    overrides::KnowledgeAction::BindMention {
                        mention_id,
                        entity_id,
                    },
                );
            }
            ipc::KnowledgeOperationInput::ConfirmRelation { relation_id } => push_operation(
                ledger,
                operation_id.clone(),
                &at,
                serde_json::Value::Null,
                serde_json::Value::Null,
                overrides::KnowledgeAction::ConfirmRelation { relation_id },
            ),
            ipc::KnowledgeOperationInput::EditRelation {
                relation_id,
                subject_id,
                predicate,
                object_id,
                valid_from,
                valid_to,
                note,
            } => {
                let subject_id = canonical_mutation_entity(data_root, ledger, &subject_id)?;
                let object_id = canonical_mutation_entity(data_root, ledger, &object_id)?;
                anyhow::ensure!(subject_id != object_id, "关系两端不能是同一实体");
                let predicate = normalized_predicate(predicate)?;
                validate_interval(valid_from.as_deref(), valid_to.as_deref())?;
                push_operation(
                    ledger,
                    operation_id.clone(),
                    &at,
                    serde_json::Value::Null,
                    serde_json::Value::Null,
                    overrides::KnowledgeAction::EditRelation {
                        relation_id,
                        subject_id,
                        predicate,
                        object_id,
                        valid_from,
                        valid_to,
                        note: note
                            .map(|value| value.trim().into())
                            .filter(|value: &String| !value.is_empty()),
                    },
                );
            }
            ipc::KnowledgeOperationInput::SuppressRelation {
                subject_id,
                predicate,
                object_id,
            } => {
                let subject_id = canonical_mutation_entity(data_root, ledger, &subject_id)?;
                let object_id = canonical_mutation_entity(data_root, ledger, &object_id)?;
                anyhow::ensure!(subject_id != object_id, "关系两端不能是同一实体");
                push_operation(
                    ledger,
                    operation_id.clone(),
                    &at,
                    serde_json::Value::Null,
                    serde_json::Value::Null,
                    overrides::KnowledgeAction::SuppressRelation {
                        subject_id,
                        predicate: normalized_predicate(predicate)?,
                        object_id,
                    },
                );
            }
            ipc::KnowledgeOperationInput::EndRelation {
                relation_id,
                valid_to,
            } => {
                validate_interval(None, Some(&valid_to))?;
                push_operation(
                    ledger,
                    operation_id.clone(),
                    &at,
                    serde_json::Value::Null,
                    serde_json::Value::Null,
                    overrides::KnowledgeAction::EndRelation {
                        relation_id,
                        valid_to,
                    },
                );
            }
            ipc::KnowledgeOperationInput::RestoreRelation {
                operation_id: target,
            } => {
                let target_operation = ledger
                    .operations
                    .iter()
                    .find(|operation| operation.id == target)
                    .ok_or_else(|| anyhow::anyhow!("要恢复的操作不存在"))?;
                anyhow::ensure!(
                    matches!(
                        target_operation.action,
                        overrides::KnowledgeAction::SuppressRelation { .. }
                            | overrides::KnowledgeAction::EndRelation { .. }
                    ),
                    "该操作不能恢复关系"
                );
                push_operation(
                    ledger,
                    operation_id.clone(),
                    &at,
                    serde_json::Value::Null,
                    serde_json::Value::Null,
                    overrides::KnowledgeAction::RestoreRelation {
                        operation_id: target,
                    },
                );
            }
            ipc::KnowledgeOperationInput::CreateEntity {
                kind,
                name,
                mut aliases,
            } => {
                let kind = require_nonempty(&kind, "实体类型不能为空")?;
                let name = require_nonempty(&name, "实体名称不能为空")?;
                aliases = normalized_values(&aliases);
                let entity = overrides::RegistryEntity {
                    kind,
                    name,
                    aliases,
                    status: "confirmed".into(),
                };
                let entity_id = overrides::allocate_entity_id(
                    &entity.kind,
                    &entity.name,
                    "manual",
                    &operation_id,
                );
                anyhow::ensure!(!ledger.registry.contains_key(&entity_id), "实体已存在");
                ledger.registry.insert(entity_id.clone(), entity.clone());
                push_operation(
                    ledger,
                    operation_id.clone(),
                    &at,
                    entity_state(&entity_id, None)?,
                    entity_state(&entity_id, Some(entity.clone()))?,
                    overrides::KnowledgeAction::CreateEntity { entity },
                );
                result_entity_id = Some(entity_id);
            }
            ipc::KnowledgeOperationInput::CreateRelation {
                subject_id,
                predicate,
                object_id,
                valid_from,
                valid_to,
                note,
                mut evidence_ids,
                user_assertion,
            } => {
                let subject_id = canonical_mutation_entity(data_root, ledger, &subject_id)?;
                let object_id = canonical_mutation_entity(data_root, ledger, &object_id)?;
                anyhow::ensure!(subject_id != object_id, "关系两端不能是同一实体");
                validate_interval(valid_from.as_deref(), valid_to.as_deref())?;
                evidence_ids.sort();
                evidence_ids.dedup();
                let relation = overrides::UserRelation {
                    subject_id,
                    predicate: normalized_predicate(predicate)?,
                    object_id,
                    valid_from,
                    valid_to,
                    note: note
                        .map(|value| value.trim().into())
                        .filter(|value: &String| !value.is_empty()),
                    evidence_ids,
                    user_assertion,
                };
                push_operation(
                    ledger,
                    operation_id.clone(),
                    &at,
                    serde_json::Value::Null,
                    serde_json::Value::Null,
                    overrides::KnowledgeAction::CreateRelation { relation },
                );
            }
        }
        Ok(operation_result(operation_id, result_entity_id))
    })
}

pub(crate) fn split_operation(
    data_root: &Path,
    request: &ipc::SplitEntityRequest,
) -> anyhow::Result<ipc::KnowledgeMutationResult> {
    let ledger = overrides::load(data_root)?;
    let source_id = canonical_mutation_entity(data_root, &ledger, &request.entity_id)?;
    let all_mentions = entity_mentions(data_root, &source_id)?;
    let mention_owners = all_mentions
        .into_iter()
        .map(|mention| (mention.id, mention.entity_id))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut mention_ids = request.mention_ids.clone();
    mention_ids.sort();
    mention_ids.dedup();
    anyhow::ensure!(!mention_ids.is_empty(), "至少选择一条提及");
    anyhow::ensure!(
        mention_ids.len() == request.mention_ids.len(),
        "提及 ID 不能重复"
    );
    anyhow::ensure!(
        mention_ids
            .iter()
            .all(|mention_id| mention_owners.get(mention_id) == Some(&source_id)),
        "只能拆分该实体当前拥有的稳定提及"
    );
    let source = ledger
        .registry
        .get(&source_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("该实体不支持拆分"))?;
    let name = require_nonempty(&request.name, "新实体名称不能为空")?;
    let kind = request.kind.clone().unwrap_or(source.kind);
    let kind = require_nonempty(&kind, "新实体类型不能为空")?;
    let aliases = normalized_values(&request.aliases);

    overrides::update(data_root, |ledger| {
        let snapshot = resolve::replay(ledger)?;
        anyhow::ensure!(
            mention_ids
                .iter()
                .all(|mention_id| !snapshot.mention_bindings.contains_key(mention_id)),
            "所选提及已经被拆分"
        );
        let at = next_operation_time(ledger)?;
        let root_operation_id = operation_identity(
            ledger,
            &at,
            "split_entity",
            &serde_json::json!([source_id, mention_ids]),
        );
        let entity_id = overrides::allocate_split_entity_id(&root_operation_id);
        anyhow::ensure!(
            !ledger.registry.contains_key(&entity_id),
            "拆分实体 ID 冲突"
        );
        let entity = overrides::RegistryEntity {
            kind,
            name,
            aliases,
            status: "confirmed".into(),
        };
        ledger.registry.insert(entity_id.clone(), entity.clone());
        push_operation(
            ledger,
            root_operation_id.clone(),
            &at,
            entity_state(&entity_id, None)?,
            entity_state(&entity_id, Some(entity.clone()))?,
            overrides::KnowledgeAction::CreateEntity { entity },
        );
        for mention_id in &mention_ids {
            let bind_id = split_bind_operation_id(&root_operation_id, mention_id);
            anyhow::ensure!(
                ledger
                    .operations
                    .iter()
                    .all(|operation| operation.id != bind_id),
                "拆分子操作 ID 冲突"
            );
            let bind_at = next_operation_time(ledger)?;
            push_operation(
                ledger,
                bind_id,
                &bind_at,
                serde_json::Value::Null,
                serde_json::Value::String(entity_id.clone()),
                overrides::KnowledgeAction::BindMention {
                    mention_id: mention_id.clone(),
                    entity_id: entity_id.clone(),
                },
            );
        }
        Ok(operation_result(root_operation_id, Some(entity_id)))
    })
}

pub(crate) fn merge_operation(
    data_root: &Path,
    source_id: &str,
    target_id: &str,
) -> anyhow::Result<ipc::KnowledgeMutationResult> {
    overrides::load(data_root)?;
    overrides::update(data_root, |ledger| {
        let source_id = canonical_mutation_entity(data_root, ledger, source_id)?;
        let target_id = canonical_mutation_entity(data_root, ledger, target_id)?;
        anyhow::ensure!(source_id != target_id, "不能合并同一个实体或形成重定向环");
        let snapshot = resolve::replay(ledger)?;
        let before = snapshot.redirects.get(&source_id).cloned();
        let at = next_operation_time(ledger)?;
        let operation_id = operation_identity(
            ledger,
            &at,
            "merge_entity",
            &serde_json::json!([source_id, target_id]),
        );
        push_operation(
            ledger,
            operation_id.clone(),
            &at,
            before.map_or(serde_json::Value::Null, serde_json::Value::String),
            serde_json::Value::String(target_id.clone()),
            overrides::KnowledgeAction::MergeEntity {
                source_id,
                target_id,
            },
        );
        Ok(operation_result(operation_id, None))
    })
}

fn project_registry_after_undo(
    baseline: &std::collections::BTreeMap<String, overrides::RegistryEntity>,
    ledger: &overrides::KnowledgeLedger,
) -> anyhow::Result<std::collections::BTreeMap<String, overrides::RegistryEntity>> {
    let active = overrides::active_operation_mask(&ledger.operations)?;
    let mut registry = baseline.clone();
    for (index, operation) in ledger.operations.iter().enumerate() {
        if !active[index] {
            continue;
        }
        match &operation.action {
            overrides::KnowledgeAction::RenameEntity { entity_id, name } => {
                registry
                    .get_mut(entity_id)
                    .ok_or_else(|| anyhow::anyhow!("改名目标不存在"))?
                    .name = name.clone();
            }
            overrides::KnowledgeAction::AddAlias { entity_id, alias } => {
                let entity = registry
                    .get_mut(entity_id)
                    .ok_or_else(|| anyhow::anyhow!("别名目标不存在"))?;
                entity.aliases.push(alias.clone());
                entity.aliases.sort();
                entity.aliases.dedup();
            }
            overrides::KnowledgeAction::RemoveAlias { entity_id, alias } => {
                registry
                    .get_mut(entity_id)
                    .ok_or_else(|| anyhow::anyhow!("别名目标不存在"))?
                    .aliases
                    .retain(|value| value != alias);
            }
            overrides::KnowledgeAction::CreateEntity { entity } => {
                let state: overrides::RegistryState =
                    serde_json::from_value(operation.after.clone())?;
                registry.insert(state.entity_id, entity.clone());
            }
            _ => {}
        }
    }
    Ok(registry)
}

fn split_bind_operation_id(root_operation_id: &str, mention_id: &str) -> String {
    store::stable_id(
        "op_",
        &[
            root_operation_id.into(),
            "split_bind".into(),
            mention_id.into(),
        ],
    )
}

fn undo_child_operation_id(root_operation_id: &str, target_id: &str) -> String {
    store::stable_id(
        "op_",
        &[
            root_operation_id.into(),
            "group_undo".into(),
            target_id.into(),
        ],
    )
}

fn split_batch_indices(
    ledger: &overrides::KnowledgeLedger,
    target_index: usize,
) -> anyhow::Result<Option<Vec<usize>>> {
    let target = &ledger.operations[target_index];
    let overrides::KnowledgeAction::CreateEntity { .. } = &target.action else {
        return Ok(None);
    };
    let state: overrides::RegistryState = serde_json::from_value(target.after.clone())?;
    if state.entity_id != overrides::allocate_split_entity_id(&target.id) {
        return Ok(None);
    }
    let mut targets = vec![target_index];
    for (index, operation) in ledger.operations.iter().enumerate().skip(target_index + 1) {
        let overrides::KnowledgeAction::BindMention {
            mention_id,
            entity_id,
        } = &operation.action
        else {
            break;
        };
        if entity_id != &state.entity_id
            || operation.id != split_bind_operation_id(&target.id, mention_id)
        {
            break;
        }
        targets.push(index);
    }
    Ok((targets.len() > 1).then_some(targets))
}

fn operation_group_indices(
    ledger: &overrides::KnowledgeLedger,
    target_index: usize,
) -> anyhow::Result<Vec<usize>> {
    operation_group_indices_at_depth(ledger, target_index, 0)
}

fn operation_group_indices_at_depth(
    ledger: &overrides::KnowledgeLedger,
    target_index: usize,
    depth: usize,
) -> anyhow::Result<Vec<usize>> {
    anyhow::ensure!(depth <= ledger.operations.len(), "知识操作分组形成递归环");
    if let Some(targets) = split_batch_indices(ledger, target_index)? {
        return Ok(targets);
    }
    let target = &ledger.operations[target_index];
    if let overrides::KnowledgeAction::Undo { operation_id } = &target.action {
        let undone_index = ledger
            .operations
            .iter()
            .position(|operation| operation.id == *operation_id)
            .ok_or_else(|| anyhow::anyhow!("撤销分组引用未知操作"))?;
        anyhow::ensure!(undone_index < target_index, "撤销分组必须引用更早的操作");
        let undone_group = operation_group_indices_at_depth(ledger, undone_index, depth + 1)?;
        if undone_group.len() > 1 {
            let mut group = Vec::with_capacity(undone_group.len());
            for (offset, undone_member) in undone_group.iter().enumerate() {
                let index = target_index + offset;
                let operation = ledger
                    .operations
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("撤销分组不完整"))?;
                let expected_target = &ledger.operations[*undone_member].id;
                anyhow::ensure!(
                    matches!(
                        &operation.action,
                        overrides::KnowledgeAction::Undo { operation_id }
                            if operation_id == expected_target
                    ),
                    "撤销分组目标不连续"
                );
                if offset > 0 {
                    anyhow::ensure!(
                        operation.id == undo_child_operation_id(&target.id, expected_target),
                        "撤销分组子操作 ID 无效"
                    );
                }
                group.push(index);
            }
            return Ok(group);
        }
    }
    Ok(vec![target_index])
}

fn enclosing_group_root(
    ledger: &overrides::KnowledgeLedger,
    target_index: usize,
) -> anyhow::Result<Option<usize>> {
    for root_index in 0..target_index {
        let group = operation_group_indices(ledger, root_index)?;
        if group.len() > 1 && group.contains(&target_index) {
            return Ok(Some(root_index));
        }
    }
    Ok(None)
}

pub(crate) fn undo_operation(
    data_root: &Path,
    target_operation_id: &str,
) -> anyhow::Result<ipc::KnowledgeMutationResult> {
    overrides::load(data_root)?;
    overrides::update(data_root, |ledger| {
        let target_index = ledger
            .operations
            .iter()
            .position(|operation| operation.id == target_operation_id)
            .ok_or_else(|| anyhow::anyhow!("要撤销的操作不存在"))?;
        anyhow::ensure!(
            ledger.operations[target_index].at != "legacy_bootstrap",
            "系统迁移操作不能撤销"
        );
        anyhow::ensure!(
            enclosing_group_root(ledger, target_index)?.is_none(),
            "分组子操作不能单独撤销"
        );
        let target_indices = operation_group_indices(ledger, target_index)?;
        let targets = target_indices
            .iter()
            .map(|index| ledger.operations[*index].id.clone())
            .collect::<Vec<_>>();
        let baseline = overrides::registry_baseline(ledger)?;
        let root_at = next_operation_time(ledger)?;
        let root_operation_id = operation_identity(
            ledger,
            &root_at,
            "undo_group",
            &serde_json::to_value(&targets)?,
        );
        let mut first_operation_id = None;
        for (offset, target) in targets.into_iter().enumerate() {
            let operation_id = if offset == 0 {
                root_operation_id.clone()
            } else {
                undo_child_operation_id(&root_operation_id, &target)
            };
            anyhow::ensure!(
                ledger
                    .operations
                    .iter()
                    .all(|operation| operation.id != operation_id),
                "撤销操作 ID 冲突"
            );
            let at = if offset == 0 {
                root_at.clone()
            } else {
                next_operation_time(ledger)?
            };
            first_operation_id.get_or_insert_with(|| operation_id.clone());
            push_operation(
                ledger,
                operation_id,
                &at,
                serde_json::Value::Null,
                serde_json::Value::Null,
                overrides::KnowledgeAction::Undo {
                    operation_id: target,
                },
            );
        }
        let first_operation_id =
            first_operation_id.ok_or_else(|| anyhow::anyhow!("没有可撤销的操作"))?;
        ledger.registry = project_registry_after_undo(&baseline, ledger)?;
        Ok(operation_result(first_operation_id, None))
    })
}

#[cfg(test)]
mod tests {
    use super::{
        apply_operation, entity_mentions, merge_operation, open_read_context, pending_review,
        pending_review_from_context, relation_detail, semantic_entity_detail,
        semantic_entity_detail_from_context, semantic_graph, split_operation, undo_operation,
        GraphFilter,
    };
    use crate::graph::canonical::{
        CanonicalEntity, CanonicalEvidence, CanonicalGraph, CanonicalMention, CanonicalRelation,
        PendingItem, RelationOrigin, RelationStatus,
    };
    use crate::graph::{index, overrides};
    use crate::store::RelationPredicate;
    use std::collections::BTreeMap;

    fn entity(id: &str, kind: &str, name: &str) -> CanonicalEntity {
        CanonicalEntity {
            id: id.into(),
            kind: kind.into(),
            name: name.into(),
            aliases: Vec::new(),
            confirmed: true,
        }
    }

    fn evidence(id: &str, note_id: &str, paragraph_index: usize) -> CanonicalEvidence {
        CanonicalEvidence {
            id: id.into(),
            note_id: note_id.into(),
            paragraph_index,
            start: 0,
            end: 5,
            quote: format!("quote-{id}"),
            source_seqs: vec![1],
            source_hash: "hash".into(),
            subject_mentions: vec![format!("m-subject-{id}")],
            object_mentions: vec![format!("m-object-{id}")],
        }
    }

    fn relation(
        id: &str,
        subject_id: &str,
        predicate_type: &str,
        object_id: &str,
        status: RelationStatus,
        valid_from: Option<&str>,
        valid_to: Option<&str>,
        evidence_rows: Vec<CanonicalEvidence>,
    ) -> CanonicalRelation {
        CanonicalRelation {
            id: id.into(),
            subject_id: subject_id.into(),
            predicate: RelationPredicate {
                kind: predicate_type.into(),
                label: None,
            },
            object_id: object_id.into(),
            confidence: 0.9,
            valid_from: valid_from.map(Into::into),
            valid_to: valid_to.map(Into::into),
            status,
            origin: RelationOrigin::Model,
            provider: Some("fixture".into()),
            model: Some("fixture-model".into()),
            note_ids: evidence_rows
                .iter()
                .map(|row| row.note_id.clone())
                .collect(),
            evidence: evidence_rows,
        }
    }

    fn install_fixture(root: &std::path::Path) {
        let entities = BTreeMap::from([
            ("kg_a".into(), entity("kg_a", "person", "Alice")),
            ("kg_b".into(), entity("kg_b", "project", "Beacon")),
            ("kg_c".into(), entity("kg_c", "org", "Council")),
        ]);
        let mentions = vec![
            CanonicalMention {
                id: "m_a_2".into(),
                note_id: "note-2".into(),
                entity_id: "kg_a".into(),
                paragraph_index: 1,
                start: 2,
                end: 7,
                quote: "Alice".into(),
            },
            CanonicalMention {
                id: "m_a_1".into(),
                note_id: "note-1".into(),
                entity_id: "kg_a".into(),
                paragraph_index: 0,
                start: 0,
                end: 5,
                quote: "Alice".into(),
            },
            CanonicalMention {
                id: "m_b_1".into(),
                note_id: "note-1".into(),
                entity_id: "kg_b".into(),
                paragraph_index: 0,
                start: 10,
                end: 16,
                quote: "Beacon".into(),
            },
        ];
        let relations = vec![
            relation(
                "cr_current",
                "kg_a",
                "responsible_for",
                "kg_b",
                RelationStatus::Current,
                Some("2026-01-01T00:00:00Z"),
                None,
                vec![evidence("ev_z", "note-2", 1), evidence("ev_a", "note-1", 0)],
            ),
            relation(
                "cr_history",
                "kg_a",
                "member_of",
                "kg_c",
                RelationStatus::Historical,
                Some("2024-01-01T00:00:00Z"),
                Some("2025-01-01T00:00:00Z"),
                vec![evidence("ev_history", "note-1", 0)],
            ),
        ];
        let pending = vec![
            PendingItem::RelationReview {
                note_id: "note-1".into(),
                relation_id: "cr_current".into(),
            },
            PendingItem::IdentityConflict {
                note_id: "note-2".into(),
                local_entity_id: "ent_x".into(),
                candidates: vec!["kg_a".into(), "kg_c".into()],
                reason: "ambiguous".into(),
            },
        ];
        index::rebuild_atomic(
            root,
            &CanonicalGraph {
                entities,
                mentions,
                relations,
                pending,
            },
        )
        .unwrap();

        let ledger = overrides::KnowledgeLedger {
            schema_version: 1,
            registry: BTreeMap::from([
                (
                    "kg_a".into(),
                    overrides::RegistryEntity {
                        kind: "person".into(),
                        name: "Alice".into(),
                        aliases: Vec::new(),
                        status: "confirmed".into(),
                    },
                ),
                (
                    "kg_b".into(),
                    overrides::RegistryEntity {
                        kind: "project".into(),
                        name: "Beacon".into(),
                        aliases: Vec::new(),
                        status: "confirmed".into(),
                    },
                ),
                (
                    "kg_c".into(),
                    overrides::RegistryEntity {
                        kind: "org".into(),
                        name: "Council".into(),
                        aliases: Vec::new(),
                        status: "confirmed".into(),
                    },
                ),
            ]),
            legacy_ids: BTreeMap::from([("e:alice".into(), "kg_a".into())]),
            operations: Vec::new(),
        };
        std::fs::write(
            root.join(overrides::KNOWLEDGE_FILE),
            serde_json::to_vec(&ledger).unwrap(),
        )
        .unwrap();
    }

    fn all_filter() -> GraphFilter {
        GraphFilter {
            entity_kinds: Vec::new(),
            predicate_types: Vec::new(),
            from: None,
            to: None,
            include_history: true,
            include_cooccurrence: true,
        }
    }

    #[test]
    fn semantic_graph_applies_current_history_date_predicate_and_entity_filters() {
        let root = tempfile::tempdir().unwrap();
        install_fixture(root.path());

        let current = semantic_graph(root.path(), &GraphFilter::default()).unwrap();
        assert_eq!(current.semantic_edges.len(), 1);
        assert_eq!(current.semantic_edges[0].id, "cr_current");

        let dated = semantic_graph(
            root.path(),
            &GraphFilter {
                entity_kinds: vec!["person".into(), "org".into()],
                predicate_types: vec!["member_of".into()],
                from: Some("2024-06-01T00:00:00Z".into()),
                to: Some("2025-06-01T00:00:00Z".into()),
                include_history: true,
                include_cooccurrence: false,
            },
        )
        .unwrap();
        assert_eq!(
            dated
                .nodes
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            ["kg_a", "kg_c"]
        );
        assert_eq!(
            dated
                .semantic_edges
                .iter()
                .map(|edge| edge.id.as_str())
                .collect::<Vec<_>>(),
            ["cr_history"]
        );
        assert!(dated.cooccurrence_edges.is_empty());

        let predicate_only = semantic_graph(
            root.path(),
            &GraphFilter {
                predicate_types: vec!["member_of".into()],
                include_history: true,
                ..GraphFilter::default()
            },
        )
        .unwrap();
        assert_eq!(
            predicate_only
                .nodes
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            ["kg_a", "kg_c"]
        );
    }

    #[test]
    fn relation_evidence_mentions_pending_and_legacy_redirects_are_stable() {
        let root = tempfile::tempdir().unwrap();
        install_fixture(root.path());

        let detail = relation_detail(root.path(), "cr_current").unwrap().unwrap();
        assert_eq!(
            detail
                .evidence
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["ev_a", "ev_z"]
        );
        let mentions = entity_mentions(root.path(), "e:alice").unwrap();
        assert_eq!(
            mentions
                .iter()
                .map(|row| (row.note_id.as_str(), row.id.as_str()))
                .collect::<Vec<_>>(),
            [("note-1", "m_a_1"), ("note-2", "m_a_2")]
        );
        let entity = semantic_entity_detail(root.path(), "e:alice", &all_filter())
            .unwrap()
            .unwrap();
        assert_eq!(entity.id, "kg_a");
        assert_eq!(entity.relations.len(), 2);
        let pending = pending_review(root.path(), &all_filter()).unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|row| row.kind.as_str())
                .collect::<Vec<_>>(),
            ["identity_conflict", "relation_review"]
        );
    }

    #[test]
    fn pending_review_keeps_relation_pending_without_a_published_relation_row() {
        let root = tempfile::tempdir().unwrap();
        let pending = vec![
            PendingItem::RelationReview {
                note_id: "note-review".into(),
                relation_id: "cr_missing_review".into(),
            },
            PendingItem::StaleEvidence {
                note_id: "note-stale".into(),
                relation_id: "cr_missing_stale".into(),
                evidence_id: "ev_stale".into(),
            },
            PendingItem::SplitConflict {
                note_id: "note-split".into(),
                relation_id: "cr_missing_split".into(),
                evidence_id: "ev_split".into(),
            },
        ];
        index::rebuild_atomic(
            root.path(),
            &CanonicalGraph {
                entities: BTreeMap::new(),
                mentions: Vec::new(),
                relations: Vec::new(),
                pending,
            },
        )
        .unwrap();
        std::fs::write(
            root.path().join(overrides::KNOWLEDGE_FILE),
            serde_json::to_vec(&overrides::KnowledgeLedger::empty()).unwrap(),
        )
        .unwrap();

        let rows = pending_review(root.path(), &GraphFilter::default()).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.kind.as_str()).collect::<Vec<_>>(),
            ["relation_review", "split_conflict", "stale_evidence"]
        );
        let conservatively_filtered = pending_review(
            root.path(),
            &GraphFilter {
                entity_kinds: vec!["person".into()],
                predicate_types: vec!["uses".into()],
                from: Some("2026-01-01T00:00:00Z".into()),
                ..GraphFilter::default()
            },
        )
        .unwrap();
        assert_eq!(conservatively_filtered.len(), 3);
    }

    #[test]
    fn held_read_context_never_mixes_replaced_index_versions() {
        let root = tempfile::tempdir().unwrap();
        install_fixture(root.path());
        let context = open_read_context(root.path()).unwrap();

        index::rebuild_atomic(
            root.path(),
            &CanonicalGraph {
                entities: BTreeMap::from([(
                    "kg_new".into(),
                    entity("kg_new", "project", "New snapshot"),
                )]),
                mentions: Vec::new(),
                relations: Vec::new(),
                pending: vec![PendingItem::InvalidDocument {
                    note_id: "new-note".into(),
                    message: "new snapshot".into(),
                }],
            },
        )
        .unwrap();

        let old_detail = semantic_entity_detail_from_context(
            root.path(),
            &context,
            "kg_a",
            &GraphFilter::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(old_detail.name, "Alice");
        assert_eq!(
            pending_review_from_context(&context, &GraphFilter::default())
                .unwrap()
                .len(),
            2
        );
        assert!(
            semantic_entity_detail(root.path(), "kg_a", &GraphFilter::default())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            semantic_graph(root.path(), &GraphFilter::default())
                .unwrap()
                .nodes[0]
                .id,
            "kg_new"
        );
    }

    #[test]
    fn corrupt_ledger_keeps_last_index_readable_and_marks_it_degraded() {
        let root = tempfile::tempdir().unwrap();
        install_fixture(root.path());
        std::fs::write(root.path().join(overrides::KNOWLEDGE_FILE), b"not json").unwrap();

        let graph = semantic_graph(root.path(), &GraphFilter::default()).unwrap();
        assert_eq!(graph.semantic_edges.len(), 1);
        assert!(graph.degraded);
        assert_eq!(
            graph.message.as_deref(),
            Some("知识整理记录损坏，当前显示上次可用索引")
        );
    }

    #[test]
    fn date_overlap_compares_rfc3339_instants_not_text_offsets() {
        let root = tempfile::tempdir().unwrap();
        install_fixture(root.path());
        let graph = semantic_graph(
            root.path(),
            &GraphFilter {
                to: Some("2026-01-01T01:00:00+08:00".into()),
                ..GraphFilter::default()
            },
        )
        .unwrap();
        assert!(graph.semantic_edges.is_empty());
    }

    #[test]
    fn noncanonical_ledger_is_also_reported_as_degraded() {
        let root = tempfile::tempdir().unwrap();
        install_fixture(root.path());
        let path = root.path().join(overrides::KNOWLEDGE_FILE);
        let mut ledger: overrides::KnowledgeLedger =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        ledger.registry.get_mut("kg_a").unwrap().aliases = vec!["z".into(), "a".into()];
        std::fs::write(&path, serde_json::to_vec(&ledger).unwrap()).unwrap();

        assert!(
            semantic_graph(root.path(), &GraphFilter::default())
                .unwrap()
                .degraded
        );
    }

    #[test]
    fn redirect_cycle_without_a_legacy_mapping_degrades_and_never_falls_back() {
        let root = tempfile::tempdir().unwrap();
        install_fixture(root.path());
        let ledger_path = root.path().join(overrides::KNOWLEDGE_FILE);
        let mut ledger: overrides::KnowledgeLedger =
            serde_json::from_slice(&std::fs::read(&ledger_path).unwrap()).unwrap();
        ledger.legacy_ids.clear();
        std::fs::write(&ledger_path, serde_json::to_vec(&ledger).unwrap()).unwrap();
        merge_operation(root.path(), "kg_a", "kg_b").unwrap();
        std::fs::write(
            root.path().join("voiceprints.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "people": {"kg_a": {"name": "Alice"}, "kg_b": {"name": "Beacon"}},
                "redirects": {"kg_b": "kg_a"}
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(
            semantic_graph(root.path(), &GraphFilter::default())
                .unwrap()
                .degraded
        );
        assert!(
            semantic_entity_detail(root.path(), "kg_a", &GraphFilter::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn legacy_mapping_into_a_cross_redirect_cycle_degrades_and_never_falls_back() {
        let root = tempfile::tempdir().unwrap();
        install_fixture(root.path());
        merge_operation(root.path(), "kg_a", "kg_b").unwrap();
        std::fs::write(
            root.path().join("voiceprints.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "people": {"kg_a": {"name": "Alice"}, "kg_b": {"name": "Beacon"}},
                "redirects": {"kg_b": "kg_a"}
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(
            semantic_graph(root.path(), &GraphFilter::default())
                .unwrap()
                .degraded
        );
        assert!(
            semantic_entity_detail(root.path(), "e:alice", &GraphFilter::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn entity_mutation_appends_one_typed_operation_with_valid_snapshots() {
        let root = tempfile::tempdir().unwrap();
        install_fixture(root.path());
        let result = apply_operation(
            root.path(),
            &crate::ipc::KnowledgeOperationInput::RenameEntity {
                entity_id: "e:alice".into(),
                name: "Alicia".into(),
            },
        )
        .unwrap();

        let ledger = overrides::load(root.path()).unwrap();
        assert_eq!(ledger.operations.len(), 1);
        assert_eq!(ledger.operations[0].id, result.operation_id);
        assert!(matches!(
            &ledger.operations[0].action,
            overrides::KnowledgeAction::RenameEntity { entity_id, name }
                if entity_id == "kg_a" && name == "Alicia"
        ));
        assert_eq!(ledger.registry["kg_a"].name, "Alicia");
        assert_ne!(ledger.operations[0].before, ledger.operations[0].after);
    }

    #[test]
    fn relation_mutation_compares_rfc3339_instants_in_both_offset_directions() {
        let root = tempfile::tempdir().unwrap();
        install_fixture(root.path());
        let create = |valid_from: &str, valid_to: &str| {
            apply_operation(
                root.path(),
                &crate::ipc::KnowledgeOperationInput::CreateRelation {
                    subject_id: "kg_a".into(),
                    predicate: RelationPredicate {
                        kind: "uses".into(),
                        label: None,
                    },
                    object_id: "kg_b".into(),
                    valid_from: Some(valid_from.into()),
                    valid_to: Some(valid_to.into()),
                    note: None,
                    evidence_ids: Vec::new(),
                    user_assertion: true,
                },
            )
        };

        create("2026-01-01T01:00:00+08:00", "2025-12-31T18:00:00Z").unwrap();
        assert!(create("2025-12-31T23:00:00-08:00", "2026-01-01T01:00:00Z",).is_err());
        assert_eq!(overrides::load(root.path()).unwrap().operations.len(), 1);
    }

    #[test]
    fn split_atomically_creates_one_entity_and_binds_only_selected_stable_mentions() {
        let root = tempfile::tempdir().unwrap();
        install_fixture(root.path());
        let result = split_operation(
            root.path(),
            &crate::ipc::SplitEntityRequest {
                entity_id: "kg_a".into(),
                name: "Alice (other)".into(),
                kind: None,
                aliases: Vec::new(),
                mention_ids: vec!["m_a_2".into()],
            },
        )
        .unwrap();
        let split_id = result.entity_id.unwrap();
        let ledger = overrides::load(root.path()).unwrap();
        assert_eq!(ledger.operations.len(), 2);
        assert!(matches!(
            ledger.operations[0].action,
            overrides::KnowledgeAction::CreateEntity { .. }
        ));
        assert!(matches!(
            &ledger.operations[1].action,
            overrides::KnowledgeAction::BindMention { mention_id, entity_id }
                if mention_id == "m_a_2" && entity_id == &split_id
        ));
        assert!(!ledger.operations.iter().any(|operation| matches!(
            &operation.action,
            overrides::KnowledgeAction::BindMention { mention_id, .. } if mention_id == "m_a_1"
        )));
        assert_ne!(ledger.operations[0].at, ledger.operations[1].at);
        assert_ne!(ledger.operations[0].id, ledger.operations[1].id);
    }

    #[test]
    fn split_child_cannot_be_undone_directly() {
        let root = tempfile::tempdir().unwrap();
        install_fixture(root.path());
        split_operation(
            root.path(),
            &crate::ipc::SplitEntityRequest {
                entity_id: "kg_a".into(),
                name: "Alice (other)".into(),
                kind: None,
                aliases: Vec::new(),
                mention_ids: vec!["m_a_2".into()],
            },
        )
        .unwrap();
        let child_id = overrides::load(root.path()).unwrap().operations[1]
            .id
            .clone();
        assert!(undo_operation(root.path(), &child_id).is_err());
    }

    #[test]
    fn split_undo_ignores_an_unrelated_bind_with_the_same_wall_timestamp() {
        let root = tempfile::tempdir().unwrap();
        install_fixture(root.path());
        let split = split_operation(
            root.path(),
            &crate::ipc::SplitEntityRequest {
                entity_id: "kg_a".into(),
                name: "Alice (other)".into(),
                kind: None,
                aliases: Vec::new(),
                mention_ids: vec!["m_a_2".into()],
            },
        )
        .unwrap();
        apply_operation(
            root.path(),
            &crate::ipc::KnowledgeOperationInput::BindMention {
                mention_id: "m_a_1".into(),
                entity_id: split.entity_id.clone().unwrap(),
            },
        )
        .unwrap();
        let ledger_path = root.path().join(overrides::KNOWLEDGE_FILE);
        let mut ledger: overrides::KnowledgeLedger =
            serde_json::from_slice(&std::fs::read(&ledger_path).unwrap()).unwrap();
        ledger.operations[2].at = ledger.operations[0].at.clone();
        std::fs::write(&ledger_path, serde_json::to_vec(&ledger).unwrap()).unwrap();

        undo_operation(root.path(), &split.operation_id).unwrap();
        let ledger = overrides::load(root.path()).unwrap();
        let active = overrides::active_operation_mask(&ledger.operations).unwrap();
        assert_eq!(&active[..3], &[false, false, true]);
    }

    #[test]
    fn multiple_split_batches_remain_independent() {
        let root = tempfile::tempdir().unwrap();
        install_fixture(root.path());
        let first = split_operation(
            root.path(),
            &crate::ipc::SplitEntityRequest {
                entity_id: "kg_a".into(),
                name: "Alice one".into(),
                kind: None,
                aliases: Vec::new(),
                mention_ids: vec!["m_a_2".into()],
            },
        )
        .unwrap();
        let second = split_operation(
            root.path(),
            &crate::ipc::SplitEntityRequest {
                entity_id: "kg_a".into(),
                name: "Alice two".into(),
                kind: None,
                aliases: Vec::new(),
                mention_ids: vec!["m_a_1".into()],
            },
        )
        .unwrap();

        undo_operation(root.path(), &first.operation_id).unwrap();
        let ledger = overrides::load(root.path()).unwrap();
        let active = overrides::active_operation_mask(&ledger.operations).unwrap();
        assert_eq!(&active[..4], &[false, false, true, true]);
        assert!(ledger
            .registry
            .contains_key(second.entity_id.as_deref().unwrap()));
    }

    #[test]
    fn split_operation_id_undoes_and_redoes_the_whole_atomic_batch() {
        let root = tempfile::tempdir().unwrap();
        install_fixture(root.path());
        let split = split_operation(
            root.path(),
            &crate::ipc::SplitEntityRequest {
                entity_id: "kg_a".into(),
                name: "Alice (other)".into(),
                kind: None,
                aliases: Vec::new(),
                mention_ids: vec!["m_a_2".into()],
            },
        )
        .unwrap();
        let undo = undo_operation(root.path(), &split.operation_id).unwrap();
        let ledger = overrides::load(root.path()).unwrap();
        let active = overrides::active_operation_mask(&ledger.operations).unwrap();
        assert_eq!(&active[..2], &[false, false]);
        assert!(!ledger
            .registry
            .contains_key(split.entity_id.as_deref().unwrap()));

        undo_operation(root.path(), &undo.operation_id).unwrap();
        let ledger = overrides::load(root.path()).unwrap();
        let active = overrides::active_operation_mask(&ledger.operations).unwrap();
        assert_eq!(&active[..2], &[true, true]);
        assert!(ledger
            .registry
            .contains_key(split.entity_id.as_deref().unwrap()));
    }

    #[test]
    fn merge_rejects_self_cycle_and_missing_endpoints() {
        let root = tempfile::tempdir().unwrap();
        install_fixture(root.path());
        assert!(merge_operation(root.path(), "kg_a", "kg_a").is_err());
        assert!(merge_operation(root.path(), "kg_missing", "kg_b").is_err());
        merge_operation(root.path(), "kg_a", "kg_b").unwrap();
        assert!(merge_operation(root.path(), "kg_b", "kg_a").is_err());
    }

    #[test]
    fn undo_targets_existing_operations_and_preserves_nested_parity() {
        let root = tempfile::tempdir().unwrap();
        install_fixture(root.path());
        let rename = apply_operation(
            root.path(),
            &crate::ipc::KnowledgeOperationInput::RenameEntity {
                entity_id: "kg_a".into(),
                name: "Alicia".into(),
            },
        )
        .unwrap();
        let first_undo = undo_operation(root.path(), &rename.operation_id).unwrap();
        assert_eq!(
            overrides::load(root.path()).unwrap().registry["kg_a"].name,
            "Alice"
        );
        undo_operation(root.path(), &first_undo.operation_id).unwrap();
        assert_eq!(
            overrides::load(root.path()).unwrap().registry["kg_a"].name,
            "Alicia"
        );
        assert!(undo_operation(root.path(), "op_missing").is_err());
    }

    #[test]
    fn corrupt_ledger_blocks_mutation_without_changing_bytes() {
        let root = tempfile::tempdir().unwrap();
        install_fixture(root.path());
        let path = root.path().join(overrides::KNOWLEDGE_FILE);
        std::fs::write(&path, b"not json").unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(apply_operation(
            root.path(),
            &crate::ipc::KnowledgeOperationInput::RenameEntity {
                entity_id: "kg_a".into(),
                name: "Alicia".into(),
            },
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
