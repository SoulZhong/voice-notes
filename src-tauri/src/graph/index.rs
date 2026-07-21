use super::canonical::{self, CanonicalGraph, PendingItem};
use super::overrides;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const GRAPH_SCHEMA_VERSION: u32 = 2;

const NEXT_FILE: &str = "graph.sqlite.next";
const PREVIOUS_FILE: &str = "graph.sqlite.previous";
const STATUS_ERROR: &str = "semantic graph rebuild failed";

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;
CREATE TABLE entities (
  id         TEXT PRIMARY KEY,
  kind       TEXT NOT NULL,
  name       TEXT NOT NULL,
  aliases    TEXT NOT NULL,
  confirmed  INTEGER NOT NULL,
  is_person  INTEGER NOT NULL,
  updated_at TEXT
);
CREATE TABLE note_entities (
  note_id       TEXT NOT NULL,
  entity_id     TEXT NOT NULL REFERENCES entities(id),
  mention_count INTEGER NOT NULL CHECK (mention_count >= 0),
  PRIMARY KEY (note_id, entity_id)
);
CREATE INDEX idx_note_entities_entity ON note_entities(entity_id, note_id);
CREATE TABLE entity_mentions (
  id              TEXT PRIMARY KEY,
  note_id         TEXT NOT NULL,
  entity_id       TEXT NOT NULL REFERENCES entities(id),
  paragraph_index INTEGER NOT NULL CHECK (paragraph_index >= 0),
  start_offset    INTEGER NOT NULL CHECK (start_offset >= 0),
  end_offset      INTEGER NOT NULL CHECK (end_offset >= start_offset),
  quote           TEXT NOT NULL
);
CREATE INDEX idx_entity_mentions_entity ON entity_mentions(entity_id, note_id, paragraph_index, start_offset, id);
CREATE TABLE relations (
  id              TEXT PRIMARY KEY,
  subject_id      TEXT NOT NULL REFERENCES entities(id),
  predicate_type  TEXT NOT NULL,
  predicate_label TEXT,
  object_id       TEXT NOT NULL REFERENCES entities(id),
  confidence      REAL NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),
  valid_from      TEXT,
  valid_to        TEXT,
  status          TEXT NOT NULL CHECK (status IN ('current', 'historical')),
  origin          TEXT NOT NULL CHECK (origin IN ('model', 'confirmed', 'manual', 'user_assertion')),
  provider        TEXT,
  model           TEXT,
  note_ids        TEXT NOT NULL
);
CREATE INDEX idx_relations_subject ON relations(subject_id, status, id);
CREATE INDEX idx_relations_object ON relations(object_id, status, id);
CREATE INDEX idx_relations_predicate ON relations(predicate_type, status, id);
CREATE TABLE relation_evidence (
  relation_id      TEXT NOT NULL REFERENCES relations(id) ON DELETE CASCADE,
  id               TEXT NOT NULL,
  note_id          TEXT NOT NULL,
  paragraph_index  INTEGER NOT NULL CHECK (paragraph_index >= 0),
  start_offset     INTEGER NOT NULL CHECK (start_offset >= 0),
  end_offset       INTEGER NOT NULL CHECK (end_offset >= start_offset),
  quote            TEXT NOT NULL,
  source_seqs      TEXT NOT NULL,
  source_hash      TEXT NOT NULL,
  subject_mentions TEXT NOT NULL,
  object_mentions  TEXT NOT NULL,
  PRIMARY KEY (relation_id, id)
);
CREATE INDEX idx_relation_evidence_note ON relation_evidence(note_id, relation_id, id);
CREATE TABLE pending_review (
  id          TEXT PRIMARY KEY,
  kind        TEXT NOT NULL,
  note_id     TEXT,
  relation_id TEXT,
  payload     TEXT NOT NULL
);
CREATE INDEX idx_pending_review_kind ON pending_review(kind, id);
CREATE TABLE graph_meta (
  schema_version INTEGER NOT NULL CHECK (schema_version = 2),
  build_id       TEXT NOT NULL,
  ledger_digest  TEXT NOT NULL,
  CHECK (schema_version = 2)
);
"#;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BuildStats {
    pub entities: usize,
    pub mentions: usize,
    pub relations: usize,
    pub evidence: usize,
    pub pending: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexStatus {
    pub state: String,
    pub error: Option<String>,
    pub stats: Option<BuildStats>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildStage {
    AfterSchema,
    BeforeCommit,
    AfterValidation,
    BeforeBackup,
    BeforeReplace,
}

enum CandidateOutcome {
    Replaced(BuildStats),
    Retry,
}

struct NextFile {
    path: PathBuf,
    replaced: bool,
}

impl NextFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            replaced: false,
        }
    }
}

impl Drop for NextFile {
    fn drop(&mut self) {
        if !self.replaced {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn normalized_graph(canonical: &CanonicalGraph) -> CanonicalGraph {
    let mut graph = canonical.clone();
    for entity in graph.entities.values_mut() {
        entity.aliases.sort();
        entity.aliases.dedup();
    }
    graph.mentions.sort_by(|left, right| left.id.cmp(&right.id));
    for relation in &mut graph.relations {
        relation.note_ids.sort();
        relation.note_ids.dedup();
        relation
            .evidence
            .sort_by(|left, right| left.id.cmp(&right.id));
        for evidence in &mut relation.evidence {
            evidence.source_seqs.sort_unstable();
            evidence.source_seqs.dedup();
            evidence.subject_mentions.sort();
            evidence.subject_mentions.dedup();
            evidence.object_mentions.sort();
            evidence.object_mentions.dedup();
        }
    }
    graph
        .relations
        .sort_by(|left, right| left.id.cmp(&right.id));
    graph
        .pending
        .sort_by_cached_key(|item| serde_json::to_string(item).unwrap_or_else(|_| String::new()));
    graph
}

fn stable_digest(prefix: &str, bytes: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(bytes);
    format!("{prefix}{}", &hex::encode(hash.finalize())[..24])
}

fn graph_build_id(graph: &CanonicalGraph) -> anyhow::Result<String> {
    Ok(stable_digest("build_", &serde_json::to_vec(graph)?))
}

fn ledger_digest(ledger: &overrides::KnowledgeLedger) -> anyhow::Result<String> {
    Ok(stable_digest("ledger_", &serde_json::to_vec(ledger)?))
}

fn enum_name(value: &impl Serialize) -> anyhow::Result<String> {
    let encoded = serde_json::to_string(value)?;
    Ok(encoded.trim_matches('"').to_string())
}

fn pending_columns(item: &PendingItem) -> (&'static str, Option<&str>, Option<&str>) {
    match item {
        PendingItem::InvalidDocument { note_id, .. } => ("invalid_document", Some(note_id), None),
        PendingItem::IdentityConflict { note_id, .. } => ("identity_conflict", Some(note_id), None),
        PendingItem::StaleEvidence {
            note_id,
            relation_id,
            ..
        } => ("stale_evidence", Some(note_id), Some(relation_id)),
        PendingItem::SplitConflict {
            note_id,
            relation_id,
            ..
        } => ("split_conflict", Some(note_id), Some(relation_id)),
        PendingItem::RelationReview {
            note_id,
            relation_id,
        } => ("relation_review", Some(note_id), Some(relation_id)),
        PendingItem::TimeConflict { .. } => ("time_conflict", None, None),
    }
}

fn insert_graph(
    transaction: &rusqlite::Transaction<'_>,
    graph: &CanonicalGraph,
    ledger_digest: &str,
) -> anyhow::Result<BuildStats> {
    for entity in graph.entities.values() {
        transaction.execute(
            "INSERT INTO entities(id, kind, name, aliases, confirmed, is_person, updated_at) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, NULL)",
            rusqlite::params![
                entity.id,
                entity.kind,
                entity.name,
                serde_json::to_string(&entity.aliases)?,
                entity.confirmed as i64,
                (entity.kind == "person") as i64,
            ],
        )?;
    }

    let mut note_entities = BTreeMap::<(String, String), usize>::new();
    for mention in &graph.mentions {
        transaction.execute(
            "INSERT INTO entity_mentions(\
                id, note_id, entity_id, paragraph_index, start_offset, end_offset, quote\
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                mention.id,
                mention.note_id,
                mention.entity_id,
                mention.paragraph_index,
                mention.start,
                mention.end,
                mention.quote,
            ],
        )?;
        *note_entities
            .entry((mention.note_id.clone(), mention.entity_id.clone()))
            .or_default() += 1;
    }
    for ((note_id, entity_id), mention_count) in note_entities {
        transaction.execute(
            "INSERT INTO note_entities(note_id, entity_id, mention_count) VALUES(?1, ?2, ?3)",
            rusqlite::params![note_id, entity_id, mention_count],
        )?;
    }

    let mut evidence_count = 0;
    for relation in &graph.relations {
        transaction.execute(
            "INSERT INTO relations(\
                id, subject_id, predicate_type, predicate_label, object_id, confidence, \
                valid_from, valid_to, status, origin, provider, model, note_ids\
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                relation.id,
                relation.subject_id,
                relation.predicate.kind,
                relation.predicate.label,
                relation.object_id,
                relation.confidence,
                relation.valid_from,
                relation.valid_to,
                enum_name(&relation.status)?,
                enum_name(&relation.origin)?,
                relation.provider,
                relation.model,
                serde_json::to_string(&relation.note_ids)?,
            ],
        )?;
        for evidence in &relation.evidence {
            transaction.execute(
                "INSERT INTO relation_evidence(\
                    relation_id, id, note_id, paragraph_index, start_offset, end_offset, quote, \
                    source_seqs, source_hash, subject_mentions, object_mentions\
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    relation.id,
                    evidence.id,
                    evidence.note_id,
                    evidence.paragraph_index,
                    evidence.start,
                    evidence.end,
                    evidence.quote,
                    serde_json::to_string(&evidence.source_seqs)?,
                    evidence.source_hash,
                    serde_json::to_string(&evidence.subject_mentions)?,
                    serde_json::to_string(&evidence.object_mentions)?,
                ],
            )?;
            evidence_count += 1;
        }
    }

    for (ordinal, item) in graph.pending.iter().enumerate() {
        let payload = serde_json::to_string(item)?;
        let id = stable_digest("pending_", format!("{}:{payload}", ordinal).as_bytes());
        let (kind, note_id, relation_id) = pending_columns(item);
        transaction.execute(
            "INSERT INTO pending_review(id, kind, note_id, relation_id, payload) \
             VALUES(?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, kind, note_id, relation_id, payload],
        )?;
    }

    transaction.execute(
        "INSERT INTO graph_meta(schema_version, build_id, ledger_digest) VALUES(?1, ?2, ?3)",
        rusqlite::params![GRAPH_SCHEMA_VERSION, graph_build_id(graph)?, ledger_digest],
    )?;

    Ok(BuildStats {
        entities: graph.entities.len(),
        mentions: graph.mentions.len(),
        relations: graph.relations.len(),
        evidence: evidence_count,
        pending: graph.pending.len(),
    })
}

fn validate_transaction(transaction: &rusqlite::Transaction<'_>) -> anyhow::Result<()> {
    let foreign_key_errors: usize =
        transaction.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    anyhow::ensure!(
        foreign_key_errors == 0,
        "semantic graph foreign key validation failed"
    );
    let missing_evidence: usize = transaction.query_row(
        "SELECT count(*) \
         FROM relations relation \
         LEFT JOIN relation_evidence evidence ON evidence.relation_id = relation.id \
         WHERE evidence.id IS NULL AND relation.origin != 'user_assertion'",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        missing_evidence == 0,
        "semantic relation is missing required evidence"
    );
    Ok(())
}

fn build_candidate_with_hook(
    data_root: &Path,
    canonical: &CanonicalGraph,
    digest: &str,
    mut hook: impl FnMut(BuildStage) -> anyhow::Result<()>,
    accept: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<CandidateOutcome> {
    fs::create_dir_all(data_root)?;
    let _graph_guard = super::GRAPH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let next_path = data_root.join(NEXT_FILE);
    match fs::remove_file(&next_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut next_file = NextFile::new(next_path.clone());
    let graph = normalized_graph(canonical);
    let stats = {
        let mut connection = rusqlite::Connection::open(&next_path)?;
        connection.busy_timeout(std::time::Duration::from_secs(3))?;
        connection.pragma_update(None, "journal_mode", "DELETE")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.execute_batch(SCHEMA)?;
        hook(BuildStage::AfterSchema)?;
        let transaction = connection.transaction()?;
        let stats = insert_graph(&transaction, &graph, digest)?;
        hook(BuildStage::BeforeCommit)?;
        validate_transaction(&transaction)?;
        transaction.commit()?;
        let quick_check: String =
            connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        anyhow::ensure!(
            quick_check == "ok",
            "semantic graph integrity validation failed"
        );
        hook(BuildStage::AfterValidation)?;
        stats
    };

    if !accept()? {
        return Ok(CandidateOutcome::Retry);
    }

    let live_path = data_root.join(super::GRAPH_FILE);
    let backup_path = data_root.join(PREVIOUS_FILE);
    hook(BuildStage::BeforeBackup)?;
    if live_path.exists() {
        fs::copy(&live_path, &backup_path)?;
    }
    hook(BuildStage::BeforeReplace)?;
    atomic_replace(&next_path, &live_path, &backup_path)?;
    next_file.replaced = true;
    Ok(CandidateOutcome::Replaced(stats))
}

fn rebuild_atomic_with_hook(
    data_root: &Path,
    canonical: &CanonicalGraph,
    hook: impl FnMut(BuildStage) -> anyhow::Result<()>,
) -> anyhow::Result<BuildStats> {
    match build_candidate_with_hook(data_root, canonical, "", hook, || Ok(true))? {
        CandidateOutcome::Replaced(stats) => Ok(stats),
        CandidateOutcome::Retry => unreachable!("unconditional atomic rebuild cannot retry"),
    }
}

pub fn rebuild_atomic(data_root: &Path, canonical: &CanonicalGraph) -> anyhow::Result<BuildStats> {
    rebuild_atomic_with_hook(data_root, canonical, |_| Ok(()))
}

fn rebuild_from_snapshot_source(
    data_root: &Path,
    mut snapshot: impl FnMut() -> anyhow::Result<(String, CanonicalGraph)>,
    mut current_digest: impl FnMut() -> anyhow::Result<String>,
    dirty: &AtomicBool,
) -> anyhow::Result<BuildStats> {
    for _ in 0..32 {
        let (captured_digest, graph) = snapshot()?;
        let outcome = build_candidate_with_hook(
            data_root,
            &graph,
            &captured_digest,
            |_| Ok(()),
            || Ok(current_digest()? == captured_digest),
        )?;
        match outcome {
            CandidateOutcome::Replaced(stats) => return Ok(stats),
            CandidateOutcome::Retry => dirty.store(true, Ordering::Release),
        }
    }
    anyhow::bail!("knowledge ledger changed too frequently to produce a stable index")
}

fn rebuild_from_sources_with_dirty(
    data_root: &Path,
    dirty: &AtomicBool,
) -> anyhow::Result<BuildStats> {
    rebuild_from_snapshot_source(
        data_root,
        || {
            let ledger = canonical::reconcile_registry(data_root)?;
            let digest = ledger_digest(&ledger)?;
            let graph = canonical::build_canonical_graph(
                data_root,
                &ledger,
                chrono::Local::now().fixed_offset(),
            )?;
            Ok((digest, graph))
        },
        || ledger_digest(&overrides::load(data_root)?),
        dirty,
    )
}

pub fn rebuild_from_sources(data_root: &Path) -> anyhow::Result<BuildStats> {
    rebuild_from_sources_with_dirty(data_root, &AtomicBool::new(false))
}

pub fn open_readonly(data_root: &Path) -> anyhow::Result<rusqlite::Connection> {
    let live_path = data_root.join(super::GRAPH_FILE);
    let connection = rusqlite::Connection::open_with_flags(
        &live_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(std::time::Duration::from_secs(3))?;
    connection.pragma_update(None, "query_only", "ON")?;
    let schema_version: u32 =
        connection.query_row("SELECT schema_version FROM graph_meta", [], |row| {
            row.get(0)
        })?;
    anyhow::ensure!(
        schema_version == GRAPH_SCHEMA_VERSION,
        "unsupported semantic graph schema version"
    );
    Ok(connection)
}

type RebuildFn = dyn Fn(&Path) -> anyhow::Result<BuildStats> + Send + Sync + 'static;

enum Rebuilder {
    Sources,
    Custom(Arc<RebuildFn>),
}

struct SchedulerInner {
    dirty: AtomicBool,
    running: AtomicBool,
    rebuilder: Rebuilder,
}

#[derive(Clone)]
pub struct RebuildScheduler {
    inner: Arc<SchedulerInner>,
}

impl Default for RebuildScheduler {
    fn default() -> Self {
        Self {
            inner: Arc::new(SchedulerInner {
                dirty: AtomicBool::new(false),
                running: AtomicBool::new(false),
                rebuilder: Rebuilder::Sources,
            }),
        }
    }
}

impl RebuildScheduler {
    fn with_rebuilder(
        rebuild: impl Fn(&Path) -> anyhow::Result<BuildStats> + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner: Arc::new(SchedulerInner {
                dirty: AtomicBool::new(false),
                running: AtomicBool::new(false),
                rebuilder: Rebuilder::Custom(Arc::new(rebuild)),
            }),
        }
    }

    pub fn request(&self, data_root: PathBuf, emit: impl Fn(IndexStatus) + Send + Sync + 'static) {
        self.inner.dirty.store(true, Ordering::Release);
        if self
            .inner
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let inner = self.inner.clone();
        let emit = Arc::new(emit);
        std::thread::spawn(move || run_scheduler(inner, data_root, emit));
    }
}

fn run_scheduler(
    inner: Arc<SchedulerInner>,
    data_root: PathBuf,
    emit: Arc<dyn Fn(IndexStatus) + Send + Sync>,
) {
    loop {
        inner.dirty.swap(false, Ordering::AcqRel);
        emit(IndexStatus {
            state: "building".into(),
            error: None,
            stats: None,
        });
        let result = match &inner.rebuilder {
            Rebuilder::Sources => rebuild_from_sources_with_dirty(&data_root, &inner.dirty),
            Rebuilder::Custom(rebuild) => rebuild(&data_root),
        };
        match result {
            Ok(stats) => emit(IndexStatus {
                state: "ready".into(),
                error: None,
                stats: Some(stats),
            }),
            Err(error) => {
                eprintln!("graph: semantic index rebuild failed: {error:#}");
                emit(IndexStatus {
                    state: "error".into(),
                    error: Some(STATUS_ERROR.into()),
                    stats: None,
                });
            }
        }

        if inner.dirty.load(Ordering::Acquire) {
            continue;
        }
        inner.running.store(false, Ordering::Release);
        if inner.dirty.load(Ordering::Acquire)
            && inner
                .running
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            continue;
        }
        break;
    }
}

#[cfg(unix)]
fn atomic_replace(next: &Path, live: &Path, _backup: &Path) -> anyhow::Result<()> {
    fs::rename(next, live)?;
    Ok(())
}

#[cfg(windows)]
fn atomic_replace(next: &Path, live: &Path, backup: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, ReplaceFileW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        REPLACEFILE_WRITE_THROUGH,
    };

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    let live_exists = live.exists();
    let next = wide(next);
    let live = wide(live);
    if !live_exists {
        let replaced = unsafe {
            MoveFileExW(
                next.as_ptr(),
                live.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        anyhow::ensure!(replaced != 0, std::io::Error::last_os_error());
        return Ok(());
    }
    let backup = wide(backup);
    let replaced = unsafe {
        ReplaceFileW(
            live.as_ptr(),
            next.as_ptr(),
            backup.as_ptr(),
            REPLACEFILE_WRITE_THROUGH,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    anyhow::ensure!(replaced != 0, std::io::Error::last_os_error());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::canonical::{
        CanonicalEntity, CanonicalEvidence, CanonicalGraph, CanonicalMention, CanonicalRelation,
        PendingItem, RelationOrigin, RelationStatus,
    };
    use crate::store::RelationPredicate;
    use rusqlite::OptionalExtension;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    fn entity(id: &str, name: &str) -> CanonicalEntity {
        CanonicalEntity {
            id: id.into(),
            kind: "person".into(),
            name: name.into(),
            aliases: vec![format!("{name} alias")],
            confirmed: true,
        }
    }

    fn evidence(id: &str, note_id: &str, subject: &str, object: &str) -> CanonicalEvidence {
        CanonicalEvidence {
            id: id.into(),
            note_id: note_id.into(),
            paragraph_index: 0,
            start: 0,
            end: 5,
            quote: "Alice uses Bob".into(),
            source_seqs: vec![1, 2],
            source_hash: "source-hash".into(),
            subject_mentions: vec![subject.into()],
            object_mentions: vec![object.into()],
        }
    }

    fn relation(
        id: &str,
        status: RelationStatus,
        evidence: CanonicalEvidence,
    ) -> CanonicalRelation {
        CanonicalRelation {
            id: id.into(),
            subject_id: "kg_a".into(),
            predicate: RelationPredicate {
                kind: "uses".into(),
                label: None,
            },
            object_id: "kg_b".into(),
            confidence: 0.91,
            valid_from: Some("2026-01-01T00:00:00+00:00".into()),
            valid_to: (status == RelationStatus::Historical)
                .then(|| "2026-02-01T00:00:00+00:00".into()),
            status,
            origin: RelationOrigin::Model,
            provider: Some("fixture-provider".into()),
            model: Some("fixture-model".into()),
            note_ids: vec![evidence.note_id.clone()],
            evidence: vec![evidence],
        }
    }

    fn fixture() -> CanonicalGraph {
        let mut entities = BTreeMap::new();
        entities.insert("kg_b".into(), entity("kg_b", "Bob"));
        entities.insert("kg_a".into(), entity("kg_a", "Alice"));
        CanonicalGraph {
            entities,
            mentions: vec![
                CanonicalMention {
                    id: "mn_b".into(),
                    note_id: "note-1".into(),
                    entity_id: "kg_b".into(),
                    paragraph_index: 0,
                    start: 11,
                    end: 14,
                    quote: "Bob".into(),
                },
                CanonicalMention {
                    id: "mn_a".into(),
                    note_id: "note-1".into(),
                    entity_id: "kg_a".into(),
                    paragraph_index: 0,
                    start: 0,
                    end: 5,
                    quote: "Alice".into(),
                },
            ],
            relations: vec![
                relation(
                    "rel_z_historical",
                    RelationStatus::Historical,
                    evidence("ev_z", "note-2", "mn_a", "mn_b"),
                ),
                relation(
                    "rel_a_current",
                    RelationStatus::Current,
                    evidence("ev_a", "note-1", "mn_a", "mn_b"),
                ),
            ],
            pending: vec![PendingItem::RelationReview {
                note_id: "note-1".into(),
                relation_id: "rel_pending".into(),
            }],
        }
    }

    fn table_names(conn: &rusqlite::Connection) -> Vec<String> {
        conn.prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    }

    fn relation_rows(conn: &rusqlite::Connection) -> Vec<(String, String, String)> {
        conn.prepare("SELECT id, status, origin FROM relations ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn build_id(conn: &rusqlite::Connection) -> String {
        conn.query_row("SELECT build_id FROM graph_meta", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn rebuild_creates_only_v2_truth_tables_and_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let graph = fixture();

        let first = rebuild_atomic(root.path(), &graph).unwrap();
        let first_conn = open_readonly(root.path()).unwrap();
        let first_rows = relation_rows(&first_conn);
        let first_build = build_id(&first_conn);
        drop(first_conn);

        let second = rebuild_atomic(root.path(), &graph).unwrap();
        let conn = open_readonly(root.path()).unwrap();

        assert_eq!(first, second);
        assert_eq!(
            second,
            BuildStats {
                entities: 2,
                mentions: 2,
                relations: 2,
                evidence: 2,
                pending: 1,
            }
        );
        assert_eq!(
            table_names(&conn),
            vec![
                "entities",
                "entity_mentions",
                "graph_meta",
                "note_entities",
                "pending_review",
                "relation_evidence",
                "relations",
            ]
        );
        assert_eq!(relation_rows(&conn), first_rows);
        assert_eq!(build_id(&conn), first_build);
        assert_eq!(
            conn.query_row("SELECT schema_version FROM graph_meta", [], |row| {
                row.get::<_, u32>(0)
            })
            .unwrap(),
            GRAPH_SCHEMA_VERSION
        );
        assert_eq!(
            conn.query_row(
                "SELECT mention_count FROM note_entities WHERE note_id = 'note-1' AND entity_id = 'kg_a'",
                [],
                |row| row.get::<_, usize>(0),
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn every_injected_failure_keeps_live_bytes_and_queries_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let original = fixture();
        rebuild_atomic(root.path(), &original).unwrap();
        let live = root.path().join(super::super::GRAPH_FILE);
        let original_bytes = std::fs::read(&live).unwrap();
        let original_rows = relation_rows(&open_readonly(root.path()).unwrap());
        let original_build = build_id(&open_readonly(root.path()).unwrap());

        let mut changed = fixture();
        changed.entities.get_mut("kg_a").unwrap().name = "Changed Alice".into();
        for fail_at in [
            BuildStage::AfterSchema,
            BuildStage::BeforeCommit,
            BuildStage::AfterValidation,
            BuildStage::BeforeBackup,
            BuildStage::BeforeReplace,
        ] {
            let error = rebuild_atomic_with_hook(root.path(), &changed, |stage| {
                anyhow::ensure!(stage != fail_at, "injected failure at {stage:?}");
                Ok(())
            })
            .unwrap_err();
            assert!(error.to_string().contains("injected failure"));
            assert_eq!(std::fs::read(&live).unwrap(), original_bytes, "{fail_at:?}");
            let conn = open_readonly(root.path()).unwrap();
            assert_eq!(relation_rows(&conn), original_rows, "{fail_at:?}");
            assert_eq!(build_id(&conn), original_build, "{fail_at:?}");
            assert!(!root.path().join("graph.sqlite.next").exists());
        }
    }

    #[test]
    fn held_reader_finishes_on_old_snapshot_after_atomic_replacement() {
        let root = tempfile::tempdir().unwrap();
        let original = fixture();
        rebuild_atomic(root.path(), &original).unwrap();
        let old = open_readonly(root.path()).unwrap();
        let old_build = build_id(&old);
        assert_eq!(relation_rows(&old).len(), 2);

        let mut changed = fixture();
        changed.entities.get_mut("kg_a").unwrap().name = "Alice Updated".into();
        rebuild_atomic(root.path(), &changed).unwrap();

        assert_eq!(relation_rows(&old).len(), 2);
        assert_eq!(build_id(&old), old_build);
        let new = open_readonly(root.path()).unwrap();
        assert_ne!(build_id(&new), old_build);
        assert_eq!(
            new.query_row("SELECT name FROM entities WHERE id = 'kg_a'", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "Alice Updated"
        );
    }

    #[test]
    fn ledger_digest_change_discards_candidate_and_retries_fresh_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let snapshots = AtomicUsize::new(0);
        let digest_checks = AtomicUsize::new(0);
        let dirty = AtomicBool::new(false);
        let stats = rebuild_from_snapshot_source(
            root.path(),
            || {
                let call = snapshots.fetch_add(1, Ordering::SeqCst);
                let mut graph = fixture();
                if call > 0 {
                    graph.entities.get_mut("kg_a").unwrap().name = "Fresh Alice".into();
                }
                Ok((if call == 0 { "old" } else { "fresh" }.to_string(), graph))
            },
            || {
                let call = digest_checks.fetch_add(1, Ordering::SeqCst);
                Ok(if call == 0 { "changed" } else { "fresh" }.to_string())
            },
            &dirty,
        )
        .unwrap();

        assert_eq!(stats.entities, 2);
        assert_eq!(snapshots.load(Ordering::SeqCst), 2);
        assert!(dirty.load(Ordering::SeqCst));
        let conn = open_readonly(root.path()).unwrap();
        assert_eq!(
            conn.query_row("SELECT name FROM entities WHERE id = 'kg_a'", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "Fresh Alice"
        );
    }

    fn update_max(max: &AtomicUsize, value: usize) {
        let mut observed = max.load(Ordering::SeqCst);
        while value > observed {
            match max.compare_exchange(observed, value, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return,
                Err(actual) => observed = actual,
            }
        }
    }

    fn wait_for_calls(calls: &AtomicUsize, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while calls.load(Ordering::SeqCst) < expected {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {expected} builds"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn scheduler_coalesces_dirty_requests_without_overlap_or_lost_wakeup() {
        let calls = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let first_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (started_tx, started_rx) = mpsc::channel();
        let calls_for_build = calls.clone();
        let active_for_build = active.clone();
        let max_for_build = max_active.clone();
        let gate_for_build = first_gate.clone();
        let scheduler = RebuildScheduler::with_rebuilder(move |_root: &Path| {
            let call = calls_for_build.fetch_add(1, Ordering::SeqCst) + 1;
            let now_active = active_for_build.fetch_add(1, Ordering::SeqCst) + 1;
            update_max(&max_for_build, now_active);
            started_tx.send(call).unwrap();
            if call == 1 {
                let (lock, ready) = &*gate_for_build;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = ready.wait(released).unwrap();
                }
            }
            active_for_build.fetch_sub(1, Ordering::SeqCst);
            Ok(BuildStats::default())
        });
        let root = tempfile::tempdir().unwrap();
        let (status_tx, status_rx) = mpsc::channel();
        scheduler.request(root.path().to_path_buf(), {
            let status_tx = status_tx.clone();
            move |status| {
                status_tx.send(status).unwrap();
            }
        });
        assert_eq!(started_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 1);

        std::thread::scope(|scope| {
            for _ in 0..16 {
                let scheduler = scheduler.clone();
                let root = root.path().to_path_buf();
                scope.spawn(move || scheduler.request(root, |_| {}));
            }
        });
        {
            let (lock, ready) = &*first_gate;
            *lock.lock().unwrap() = true;
            ready.notify_all();
        }
        assert_eq!(started_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 2);
        wait_for_calls(&calls, 2);
        let mut ready_events = 0;
        while ready_events < 2 {
            let status = status_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if status.state == "ready" {
                ready_events += 1;
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(max_active.load(Ordering::SeqCst), 1);

        let (late_tx, late_rx) = mpsc::channel();
        scheduler.request(root.path().to_path_buf(), move |status| {
            if status.state == "ready" {
                late_tx.send(()).unwrap();
            }
        });
        late_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        wait_for_calls(&calls, 3);
        assert_eq!(max_active.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn scheduler_error_status_is_stable_and_does_not_leak_details() {
        let scheduler = RebuildScheduler::with_rebuilder(|_| {
            anyhow::bail!("secret token and /private/customer/path")
        });
        let root = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        scheduler.request(root.path().to_path_buf(), move |status| {
            if status.state == "error" {
                tx.send(status).unwrap();
            }
        });
        let status = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            status.error.as_deref(),
            Some("semantic graph rebuild failed")
        );
        assert!(status.stats.is_none());
        assert!(status.error.unwrap().find("secret").is_none());
    }

    #[test]
    fn readonly_open_rejects_missing_or_non_v2_database_without_creating_it() {
        let root = tempfile::tempdir().unwrap();
        assert!(open_readonly(root.path()).is_err());
        assert!(!root.path().join(super::super::GRAPH_FILE).exists());

        let live = root.path().join(super::super::GRAPH_FILE);
        let conn = rusqlite::Connection::open(&live).unwrap();
        conn.execute("CREATE TABLE legacy(value TEXT)", []).unwrap();
        drop(conn);
        assert!(open_readonly(root.path()).is_err());
        assert!(rusqlite::Connection::open_with_flags(
            live,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        )
        .unwrap()
        .query_row(
            "SELECT name FROM sqlite_master WHERE name = 'graph_meta'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .unwrap()
        .is_none());
    }

    #[test]
    fn rebuild_from_sources_builds_a_valid_empty_index_without_a_legacy_truth_db() {
        let root = tempfile::tempdir().unwrap();
        let stats = rebuild_from_sources(root.path()).unwrap();
        assert_eq!(stats, BuildStats::default());
        let conn = open_readonly(root.path()).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM entities", [], |row| row
                .get::<_, usize>(0))
                .unwrap(),
            0
        );
        assert!(root.path().join(overrides::KNOWLEDGE_FILE).exists());
    }
}
