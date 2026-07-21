use super::canonical::{self, CanonicalGraph, PendingItem};
use super::overrides;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub const GRAPH_SCHEMA_VERSION: u32 = 2;

const NEXT_FILE: &str = "graph.sqlite.next";
const PREVIOUS_FILE: &str = "graph.sqlite.previous";
const INDEX_LOCK_FILE: &str = ".graph-index.lock";
const CURRENT_MANIFEST_FILE: &str = ".graph-index-current.sqlite";
const CURRENT_MANIFEST_NEXT_FILE: &str = ".graph-index-current.sqlite.next";
const VERSION_PREFIX: &str = "graph.sqlite.version.";
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
  id             INTEGER PRIMARY KEY CHECK (id = 1),
  schema_version INTEGER NOT NULL CHECK (schema_version = 2),
  build_id       TEXT NOT NULL,
  ledger_digest  TEXT NOT NULL
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
    AfterNextSync,
    BeforeBackup,
    AfterBackupSync,
    BeforeReplace,
    BeforePointerCommit,
    AfterPublish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StorageLayout {
    Direct,
    Versioned,
}

#[cfg(windows)]
fn platform_layout() -> StorageLayout {
    StorageLayout::Versioned
}

#[cfg(not(windows))]
fn platform_layout() -> StorageLayout {
    StorageLayout::Direct
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

struct GraphIndexFileLock {
    _file: File,
}

impl GraphIndexFileLock {
    fn open(data_root: &Path) -> std::io::Result<File> {
        OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(data_root.join(INDEX_LOCK_FILE))
    }

    fn acquire(data_root: &Path) -> std::io::Result<Self> {
        let file = Self::open(data_root)?;
        file.lock()?;
        Ok(Self { _file: file })
    }

    fn try_acquire(data_root: &Path) -> std::io::Result<Option<Self>> {
        let file = Self::open(data_root)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error),
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
        "INSERT INTO graph_meta(id, schema_version, build_id, ledger_digest) VALUES(1, ?1, ?2, ?3)",
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
    layout: StorageLayout,
    hook: &mut impl FnMut(BuildStage) -> anyhow::Result<()>,
    publish_if_current: impl FnOnce(&mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<bool>,
) -> anyhow::Result<CandidateOutcome> {
    fs::create_dir_all(data_root)?;
    let _graph_guard = super::GRAPH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _index_file_lock = GraphIndexFileLock::acquire(data_root)?;
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
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&next_path)?
        .sync_all()?;
    hook(BuildStage::AfterNextSync)?;

    let accepted = {
        let mut publish = || {
            match layout {
                StorageLayout::Direct => {
                    let live_path = data_root.join(super::GRAPH_FILE);
                    let backup_path = data_root.join(PREVIOUS_FILE);
                    hook(BuildStage::BeforeBackup)?;
                    if live_path.exists() {
                        fs::copy(&live_path, &backup_path)?;
                        OpenOptions::new()
                            .read(true)
                            .write(true)
                            .open(&backup_path)?
                            .sync_all()?;
                        hook(BuildStage::AfterBackupSync)?;
                    }
                    hook(BuildStage::BeforeReplace)?;
                    atomic_replace(&next_path, &live_path, &backup_path)?;
                    next_file.replaced = true;
                }
                StorageLayout::Versioned => {
                    publish_versioned(data_root, &next_path, hook)?;
                }
            }
            Ok(())
        };
        publish_if_current(&mut publish)?
    };
    if !accepted {
        return Ok(CandidateOutcome::Retry);
    }
    hook(BuildStage::AfterPublish)?;
    Ok(CandidateOutcome::Replaced(stats))
}

fn rebuild_atomic_with_hook(
    data_root: &Path,
    canonical: &CanonicalGraph,
    hook: impl FnMut(BuildStage) -> anyhow::Result<()>,
) -> anyhow::Result<BuildStats> {
    rebuild_atomic_with_layout(data_root, canonical, platform_layout(), hook)
}

fn rebuild_atomic_with_layout(
    data_root: &Path,
    canonical: &CanonicalGraph,
    layout: StorageLayout,
    mut hook: impl FnMut(BuildStage) -> anyhow::Result<()>,
) -> anyhow::Result<BuildStats> {
    match build_candidate_with_hook(data_root, canonical, "", layout, &mut hook, |publish| {
        publish()?;
        Ok(true)
    })? {
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
    hook: impl FnMut(BuildStage) -> anyhow::Result<()>,
) -> anyhow::Result<BuildStats> {
    rebuild_from_snapshot_source_with_publish(
        data_root,
        &mut snapshot,
        |captured, publish| {
            let matches = current_digest()? == captured;
            if matches {
                if let Some(publish) = publish {
                    publish()?;
                }
            }
            Ok(matches)
        },
        hook,
    )
}

fn rebuild_from_snapshot_source_with_publish(
    data_root: &Path,
    mut snapshot: impl FnMut() -> anyhow::Result<(String, CanonicalGraph)>,
    mut accept: impl FnMut(&str, Option<&mut dyn FnMut() -> anyhow::Result<()>>) -> anyhow::Result<bool>,
    mut hook: impl FnMut(BuildStage) -> anyhow::Result<()>,
) -> anyhow::Result<BuildStats> {
    for _ in 0..32 {
        let (captured_digest, graph) = snapshot()?;
        let outcome = build_candidate_with_hook(
            data_root,
            &graph,
            &captured_digest,
            platform_layout(),
            &mut hook,
            |publish| accept(&captured_digest, Some(publish)),
        )?;
        match outcome {
            CandidateOutcome::Replaced(stats) if accept(&captured_digest, None)? => {
                return Ok(stats)
            }
            CandidateOutcome::Replaced(_) | CandidateOutcome::Retry => {}
        }
    }
    anyhow::bail!("knowledge ledger changed too frequently to produce a stable index")
}

fn rebuild_from_sources_with_hook(
    data_root: &Path,
    hook: impl FnMut(BuildStage) -> anyhow::Result<()>,
) -> anyhow::Result<BuildStats> {
    rebuild_from_snapshot_source_with_publish(
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
        |captured, publish| {
            overrides::with_locked_ledger(data_root, |ledger| {
                let matches = ledger_digest(ledger)? == captured;
                if matches {
                    if let Some(publish) = publish {
                        publish()?;
                    }
                }
                Ok(matches)
            })
        },
        hook,
    )
}

pub fn rebuild_from_sources(data_root: &Path) -> anyhow::Result<BuildStats> {
    rebuild_from_sources_with_hook(data_root, |_| Ok(()))
}

fn next_version_file(data_root: &Path) -> anyhow::Result<(String, PathBuf)> {
    for sequence in 1_u64.. {
        let file_name = format!("{VERSION_PREFIX}{sequence:020}");
        let path = data_root.join(&file_name);
        if !path.exists() {
            return Ok((file_name, path));
        }
    }
    unreachable!("u64 version sequence cannot be exhausted")
}

fn publish_versioned(
    data_root: &Path,
    next_path: &Path,
    hook: &mut impl FnMut(BuildStage) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    hook(BuildStage::BeforeBackup)?;
    let (file_name, version_path) = next_version_file(data_root)?;
    fs::copy(next_path, &version_path)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&version_path)?
        .sync_all()?;
    hook(BuildStage::AfterBackupSync)?;
    hook(BuildStage::BeforeReplace)?;
    update_current_manifest(data_root, &file_name, hook)?;
    Ok(())
}

fn update_current_manifest(
    data_root: &Path,
    file_name: &str,
    hook: &mut impl FnMut(BuildStage) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let manifest_path = data_root.join(CURRENT_MANIFEST_FILE);
    if !manifest_path.exists() {
        let manifest_next_path = data_root.join(CURRENT_MANIFEST_NEXT_FILE);
        match fs::remove_file(&manifest_next_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut manifest_next = NextFile::new(manifest_next_path.clone());
        let mut connection = open_manifest_for_update(&manifest_next_path)?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO current_graph(id, file_name) VALUES(1, ?1)",
            [file_name],
        )?;
        hook(BuildStage::BeforePointerCommit)?;
        transaction.commit()?;
        drop(connection);
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&manifest_next_path)?
            .sync_all()?;
        fs::rename(&manifest_next_path, &manifest_path)?;
        manifest_next.replaced = true;
        sync_directory(data_root)?;
        return Ok(());
    }

    let mut connection = open_manifest_for_update(&manifest_path)?;
    let transaction = connection.transaction()?;
    let rows: usize =
        transaction.query_row("SELECT count(*) FROM current_graph", [], |row| row.get(0))?;
    anyhow::ensure!(
        rows <= 1,
        "semantic graph current manifest is not a singleton"
    );
    if rows == 0 {
        transaction.execute(
            "INSERT INTO current_graph(id, file_name) VALUES(1, ?1)",
            [file_name],
        )?;
    } else {
        let changed = transaction.execute(
            "UPDATE current_graph SET file_name = ?1 WHERE id = 1",
            [file_name],
        )?;
        anyhow::ensure!(
            changed == 1,
            "semantic graph current manifest key is invalid"
        );
    }
    hook(BuildStage::BeforePointerCommit)?;
    transaction.commit()?;
    drop(connection);
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&manifest_path)?
        .sync_all()?;
    sync_directory(data_root)?;
    Ok(())
}

fn open_manifest_for_update(path: &Path) -> anyhow::Result<rusqlite::Connection> {
    let connection = rusqlite::Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(3))?;
    connection.pragma_update(None, "journal_mode", "DELETE")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS current_graph (\
           id INTEGER PRIMARY KEY CHECK (id = 1),\
           file_name TEXT NOT NULL\
         );",
    )?;
    Ok(connection)
}

pub fn open_readonly(data_root: &Path) -> anyhow::Result<rusqlite::Connection> {
    open_readonly_with_layout(data_root, platform_layout())
}

fn open_readonly_with_layout(
    data_root: &Path,
    layout: StorageLayout,
) -> anyhow::Result<rusqlite::Connection> {
    let live_path = match layout {
        StorageLayout::Direct => data_root.join(super::GRAPH_FILE),
        StorageLayout::Versioned => current_version_path(data_root)?,
    };
    let connection = rusqlite::Connection::open_with_flags(
        &live_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(std::time::Duration::from_secs(3))?;
    connection.pragma_update(None, "query_only", "ON")?;
    validate_read_schema(&connection)?;
    Ok(connection)
}

fn current_version_path(data_root: &Path) -> anyhow::Result<PathBuf> {
    let manifest_path = data_root.join(CURRENT_MANIFEST_FILE);
    if !manifest_path.exists() {
        return Ok(data_root.join(super::GRAPH_FILE));
    }
    let connection = rusqlite::Connection::open_with_flags(
        &manifest_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(std::time::Duration::from_secs(3))?;
    let tables = connection
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(
        tables == ["current_graph"],
        "semantic graph current manifest schema is invalid"
    );
    let columns = connection
        .prepare("PRAGMA table_info(current_graph)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(
        columns == ["id", "file_name"],
        "semantic graph current manifest columns are invalid"
    );
    let (rows, id, file_name): (usize, Option<i64>, Option<String>) = connection.query_row(
        "SELECT count(*), min(id), min(file_name) FROM current_graph",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    anyhow::ensure!(
        rows == 1 && id == Some(1),
        "semantic graph current manifest is not a singleton"
    );
    let file_name =
        file_name.ok_or_else(|| anyhow::anyhow!("semantic graph current manifest has no file"))?;
    anyhow::ensure!(
        file_name.starts_with(VERSION_PREFIX)
            && file_name[VERSION_PREFIX.len()..]
                .chars()
                .all(|character| character.is_ascii_digit())
            && Path::new(&file_name)
                .file_name()
                .and_then(|name| name.to_str())
                == Some(&file_name),
        "semantic graph current manifest file is invalid"
    );
    let quick_check: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    anyhow::ensure!(
        quick_check == "ok",
        "semantic graph current manifest is corrupt"
    );
    Ok(data_root.join(file_name))
}

fn validate_read_schema(connection: &rusqlite::Connection) -> anyhow::Result<()> {
    let tables = connection
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(
        tables
            == [
                "entities",
                "entity_mentions",
                "graph_meta",
                "note_entities",
                "pending_review",
                "relation_evidence",
                "relations",
            ],
        "semantic graph schema tables are incomplete"
    );

    for (table, expected) in [
        (
            "entities",
            &[
                "id",
                "kind",
                "name",
                "aliases",
                "confirmed",
                "is_person",
                "updated_at",
            ][..],
        ),
        (
            "note_entities",
            &["note_id", "entity_id", "mention_count"][..],
        ),
        (
            "entity_mentions",
            &[
                "id",
                "note_id",
                "entity_id",
                "paragraph_index",
                "start_offset",
                "end_offset",
                "quote",
            ][..],
        ),
        (
            "relations",
            &[
                "id",
                "subject_id",
                "predicate_type",
                "predicate_label",
                "object_id",
                "confidence",
                "valid_from",
                "valid_to",
                "status",
                "origin",
                "provider",
                "model",
                "note_ids",
            ][..],
        ),
        (
            "relation_evidence",
            &[
                "relation_id",
                "id",
                "note_id",
                "paragraph_index",
                "start_offset",
                "end_offset",
                "quote",
                "source_seqs",
                "source_hash",
                "subject_mentions",
                "object_mentions",
            ][..],
        ),
        (
            "pending_review",
            &["id", "kind", "note_id", "relation_id", "payload"][..],
        ),
        (
            "graph_meta",
            &["id", "schema_version", "build_id", "ledger_digest"][..],
        ),
    ] {
        let columns = connection
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;
        anyhow::ensure!(
            columns == expected,
            "semantic graph {table} columns are invalid"
        );
    }

    for (table, expected_primary_key) in [
        ("entities", &["id"][..]),
        ("note_entities", &["note_id", "entity_id"][..]),
        ("entity_mentions", &["id"][..]),
        ("relations", &["id"][..]),
        ("relation_evidence", &["relation_id", "id"][..]),
        ("pending_review", &["id"][..]),
        ("graph_meta", &["id"][..]),
    ] {
        let primary_key = connection
            .prepare(&format!(
                "SELECT name FROM pragma_table_info('{table}') WHERE pk > 0 ORDER BY pk"
            ))?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        anyhow::ensure!(
            primary_key == expected_primary_key,
            "semantic graph {table} primary key is invalid"
        );
    }

    for (table, expected_foreign_keys) in [
        ("note_entities", &["entity_id:entities:id:NO ACTION"][..]),
        ("entity_mentions", &["entity_id:entities:id:NO ACTION"][..]),
        (
            "relations",
            &[
                "object_id:entities:id:NO ACTION",
                "subject_id:entities:id:NO ACTION",
            ][..],
        ),
        (
            "relation_evidence",
            &["relation_id:relations:id:CASCADE"][..],
        ),
    ] {
        let mut foreign_keys = connection
            .prepare(&format!("PRAGMA foreign_key_list({table})"))?
            .query_map([], |row| {
                Ok(format!(
                    "{}:{}:{}:{}",
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(6)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        foreign_keys.sort();
        anyhow::ensure!(
            foreign_keys == expected_foreign_keys,
            "semantic graph {table} foreign keys are invalid"
        );
    }

    let normalized_schema = |table: &str| -> anyhow::Result<String> {
        let sql: String = connection.query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )?;
        Ok(sql
            .chars()
            .filter(|character| !character.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect())
    };
    for (table, required_checks) in [
        (
            "graph_meta",
            &["check(id=1)", "check(schema_version=2)"][..],
        ),
        ("note_entities", &["check(mention_count>=0)"][..]),
        (
            "entity_mentions",
            &[
                "check(paragraph_index>=0)",
                "check(start_offset>=0)",
                "check(end_offset>=start_offset)",
            ][..],
        ),
        (
            "relation_evidence",
            &[
                "check(paragraph_index>=0)",
                "check(start_offset>=0)",
                "check(end_offset>=start_offset)",
            ][..],
        ),
        (
            "relations",
            &[
                "check(confidence>=0.0andconfidence<=1.0)",
                "check(statusin('current','historical'))",
                "check(originin('model','confirmed','manual','user_assertion'))",
            ][..],
        ),
    ] {
        let schema = normalized_schema(table)?;
        anyhow::ensure!(
            required_checks.iter().all(|check| schema.contains(check)),
            "semantic graph {table} constraints are invalid"
        );
    }

    let (meta_rows, meta_id, schema_version): (usize, Option<i64>, Option<u32>) = connection
        .query_row(
            "SELECT count(*), min(id), min(schema_version) FROM graph_meta",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
    anyhow::ensure!(
        meta_rows == 1 && meta_id == Some(1),
        "graph_meta must be a singleton"
    );
    anyhow::ensure!(
        schema_version == Some(GRAPH_SCHEMA_VERSION),
        "unsupported semantic graph schema version"
    );
    let meta_primary_key: i64 = connection.query_row(
        "SELECT pk FROM pragma_table_info('graph_meta') WHERE name = 'id'",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(meta_primary_key == 1, "graph_meta singleton key is missing");

    let foreign_key_errors: usize =
        connection.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    anyhow::ensure!(
        foreign_key_errors == 0,
        "semantic graph foreign keys are invalid"
    );
    let missing_evidence: usize = connection.query_row(
        "SELECT count(*) FROM relations relation \
         LEFT JOIN relation_evidence evidence ON evidence.relation_id = relation.id \
         WHERE evidence.id IS NULL AND relation.origin != 'user_assertion'",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        missing_evidence == 0,
        "semantic graph evidence invariant failed"
    );
    let quick_check: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    anyhow::ensure!(quick_check == "ok", "semantic graph integrity check failed");
    Ok(())
}

type RebuildFn = dyn Fn(&Path) -> anyhow::Result<BuildStats> + Send + Sync + 'static;
type EmitFn = dyn Fn(IndexStatus) + Send + Sync + 'static;

enum Rebuilder {
    Sources,
    Custom(Arc<RebuildFn>),
}

struct SchedulerInner {
    dirty: AtomicBool,
    running: AtomicBool,
    rebuilder: Rebuilder,
    latest: Mutex<Option<RebuildRequest>>,
}

#[derive(Clone)]
struct RebuildRequest {
    data_root: PathBuf,
    emit: Arc<EmitFn>,
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
                latest: Mutex::new(None),
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
                latest: Mutex::new(None),
            }),
        }
    }

    pub fn request(&self, data_root: PathBuf, emit: impl Fn(IndexStatus) + Send + Sync + 'static) {
        *self
            .inner
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(RebuildRequest {
            data_root,
            emit: Arc::new(emit),
        });
        self.inner.dirty.store(true, Ordering::Release);
        start_scheduler(self.inner.clone());
    }
}

struct RunningGuard {
    inner: Arc<SchedulerInner>,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.inner.running.store(false, Ordering::Release);
        if self.inner.dirty.load(Ordering::Acquire) {
            start_scheduler(self.inner.clone());
        }
    }
}

fn start_scheduler(inner: Arc<SchedulerInner>) {
    if inner
        .running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let worker_inner = inner.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("graph-index-rebuild".into())
        .spawn(move || run_scheduler(worker_inner))
    {
        inner.running.store(false, Ordering::Release);
        inner.dirty.store(false, Ordering::Release);
        eprintln!("graph: unable to start semantic index rebuild: {error}");
        let request = inner
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(request) = request {
            emit_status(
                &request.emit,
                IndexStatus {
                    state: "error".into(),
                    error: Some(STATUS_ERROR.into()),
                    stats: None,
                },
            );
        }
    }
}

fn emit_status(emit: &Arc<EmitFn>, status: IndexStatus) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| emit(status)));
}

fn run_scheduler(inner: Arc<SchedulerInner>) {
    let _running_guard = RunningGuard {
        inner: inner.clone(),
    };
    while inner.dirty.swap(false, Ordering::AcqRel) {
        let Some(request) = inner
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        else {
            break;
        };
        emit_status(
            &request.emit,
            IndexStatus {
                state: "building".into(),
                error: None,
                stats: None,
            },
        );
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &inner.rebuilder {
                Rebuilder::Sources => rebuild_from_sources(&request.data_root),
                Rebuilder::Custom(rebuild) => rebuild(&request.data_root),
            }))
            .unwrap_or_else(|_| Err(anyhow::anyhow!("semantic graph rebuilder panicked")));
        match result {
            Ok(stats) => emit_status(
                &request.emit,
                IndexStatus {
                    state: "ready".into(),
                    error: None,
                    stats: Some(stats),
                },
            ),
            Err(error) => {
                eprintln!("graph: semantic index rebuild failed: {error:#}");
                emit_status(
                    &request.emit,
                    IndexStatus {
                        state: "error".into(),
                        error: Some(STATUS_ERROR.into()),
                        stats: None,
                    },
                );
            }
        }
    }
}

#[cfg(unix)]
fn atomic_replace(next: &Path, live: &Path, _backup: &Path) -> anyhow::Result<()> {
    fs::rename(next, live)?;
    sync_directory(
        live.parent()
            .ok_or_else(|| anyhow::anyhow!("graph database has no parent directory"))?,
    )?;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn atomic_replace(_next: &Path, _live: &Path, _backup: &Path) -> anyhow::Result<()> {
    anyhow::bail!("direct semantic graph replacement is unavailable on this platform")
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
    use std::sync::atomic::{AtomicUsize, Ordering};
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
    fn sync_failures_before_publish_keep_live_bytes_and_queries_unchanged() {
        let root = tempfile::tempdir().unwrap();
        rebuild_atomic(root.path(), &fixture()).unwrap();
        let live = root.path().join(super::super::GRAPH_FILE);
        let original_bytes = std::fs::read(&live).unwrap();
        let original_build = build_id(&open_readonly(root.path()).unwrap());
        let mut changed = fixture();
        changed.entities.get_mut("kg_a").unwrap().name = "Durable Alice".into();

        for fail_at in [BuildStage::AfterNextSync, BuildStage::AfterBackupSync] {
            rebuild_atomic_with_hook(root.path(), &changed, |stage| {
                anyhow::ensure!(stage != fail_at, "sync failure at {stage:?}");
                Ok(())
            })
            .unwrap_err();
            assert_eq!(std::fs::read(&live).unwrap(), original_bytes);
            assert_eq!(
                build_id(&open_readonly(root.path()).unwrap()),
                original_build
            );
        }
    }

    #[test]
    fn graph_index_file_lock_excludes_an_independent_file_handle() {
        let root = tempfile::tempdir().unwrap();
        let first = GraphIndexFileLock::try_acquire(root.path())
            .unwrap()
            .expect("first lock");
        assert!(GraphIndexFileLock::try_acquire(root.path())
            .unwrap()
            .is_none());
        drop(first);
        assert!(GraphIndexFileLock::try_acquire(root.path())
            .unwrap()
            .is_some());
    }

    #[test]
    fn locked_ledger_callback_serializes_with_an_override_update() {
        let root = tempfile::tempdir().unwrap();
        overrides::load(root.path()).unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let root_for_reader = root.path().to_path_buf();
        let reader = std::thread::spawn(move || {
            overrides::with_locked_ledger(&root_for_reader, |_ledger| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let (done_tx, done_rx) = mpsc::channel();
        let root_for_writer = root.path().to_path_buf();
        let writer = std::thread::spawn(move || {
            overrides::update(&root_for_writer, |_ledger| Ok(())).unwrap();
            done_tx.send(()).unwrap();
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(40)).is_err());
        release_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        reader.join().unwrap();
        writer.join().unwrap();
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
    fn versioned_layout_keeps_held_reader_and_retains_old_versions() {
        let root = tempfile::tempdir().unwrap();
        rebuild_atomic_with_layout(
            root.path(),
            &fixture(),
            StorageLayout::Versioned,
            |_| Ok(()),
        )
        .unwrap();
        let old = open_readonly_with_layout(root.path(), StorageLayout::Versioned).unwrap();
        let old_build = build_id(&old);

        let mut changed = fixture();
        changed.entities.get_mut("kg_a").unwrap().name = "Versioned Alice".into();
        rebuild_atomic_with_layout(root.path(), &changed, StorageLayout::Versioned, |_| Ok(()))
            .unwrap();

        assert_eq!(build_id(&old), old_build);
        assert_eq!(relation_rows(&old).len(), 2);
        let new = open_readonly_with_layout(root.path(), StorageLayout::Versioned).unwrap();
        assert_ne!(build_id(&new), old_build);
        assert_eq!(
            new.query_row("SELECT name FROM entities WHERE id = 'kg_a'", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "Versioned Alice"
        );
        let versions = std::fs::read_dir(root.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(VERSION_PREFIX)
            })
            .count();
        assert_eq!(versions, 2);
    }

    #[test]
    fn versioned_pointer_failure_keeps_old_pointer_and_readers_intact() {
        let root = tempfile::tempdir().unwrap();
        rebuild_atomic_with_layout(
            root.path(),
            &fixture(),
            StorageLayout::Versioned,
            |_| Ok(()),
        )
        .unwrap();
        let old = open_readonly_with_layout(root.path(), StorageLayout::Versioned).unwrap();
        let old_build = build_id(&old);
        let mut changed = fixture();
        changed.entities.get_mut("kg_a").unwrap().name = "Never Published".into();

        rebuild_atomic_with_layout(root.path(), &changed, StorageLayout::Versioned, |stage| {
            anyhow::ensure!(
                stage != BuildStage::BeforePointerCommit,
                "pointer commit failure"
            );
            Ok(())
        })
        .unwrap_err();

        assert_eq!(build_id(&old), old_build);
        assert_eq!(relation_rows(&old).len(), 2);
        assert_eq!(
            build_id(&open_readonly_with_layout(root.path(), StorageLayout::Versioned).unwrap()),
            old_build
        );
    }

    #[test]
    fn first_versioned_pointer_failure_keeps_direct_compatibility_pointer() {
        let root = tempfile::tempdir().unwrap();
        rebuild_atomic_with_layout(root.path(), &fixture(), StorageLayout::Direct, |_| Ok(()))
            .unwrap();
        let old_build =
            build_id(&open_readonly_with_layout(root.path(), StorageLayout::Direct).unwrap());
        let mut changed = fixture();
        changed.entities.get_mut("kg_a").unwrap().name = "Not Current".into();

        rebuild_atomic_with_layout(root.path(), &changed, StorageLayout::Versioned, |stage| {
            anyhow::ensure!(
                stage != BuildStage::BeforePointerCommit,
                "pointer commit failure"
            );
            Ok(())
        })
        .unwrap_err();

        assert!(!root.path().join(CURRENT_MANIFEST_FILE).exists());
        assert_eq!(
            build_id(&open_readonly_with_layout(root.path(), StorageLayout::Versioned).unwrap()),
            old_build
        );
    }

    #[test]
    fn first_versioned_rebuild_migrates_from_v1_compatibility_without_deleting_it() {
        let root = tempfile::tempdir().unwrap();
        let legacy_path = root.path().join(super::super::GRAPH_FILE);
        let legacy = rusqlite::Connection::open(&legacy_path).unwrap();
        legacy
            .execute("CREATE TABLE legacy_truth(value TEXT)", [])
            .unwrap();
        drop(legacy);
        assert!(open_readonly_with_layout(root.path(), StorageLayout::Versioned).is_err());

        rebuild_atomic_with_layout(
            root.path(),
            &fixture(),
            StorageLayout::Versioned,
            |_| Ok(()),
        )
        .unwrap();

        assert_eq!(
            relation_rows(
                &open_readonly_with_layout(root.path(), StorageLayout::Versioned).unwrap()
            )
            .len(),
            2
        );
        assert!(legacy_path.exists());
        assert!(root.path().join(CURRENT_MANIFEST_FILE).exists());
    }

    #[test]
    fn ledger_digest_change_discards_candidate_and_retries_fresh_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let snapshots = AtomicUsize::new(0);
        let digest_checks = AtomicUsize::new(0);
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
            |_| Ok(()),
        )
        .unwrap();

        assert_eq!(stats.entities, 2);
        assert_eq!(snapshots.load(Ordering::SeqCst), 2);
        let conn = open_readonly(root.path()).unwrap();
        assert_eq!(
            conn.query_row("SELECT name FROM entities WHERE id = 'kg_a'", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "Fresh Alice"
        );
    }

    #[test]
    fn override_update_at_before_backup_waits_and_final_digest_matches_ledger() {
        let root = tempfile::tempdir().unwrap();
        overrides::load(root.path()).unwrap();
        let mut updater = None;
        let mut updater_done = None;
        let mut spawned = false;
        rebuild_from_sources_with_hook(root.path(), |stage| {
            if stage == BuildStage::BeforeBackup && !spawned {
                spawned = true;
                let root = root.path().to_path_buf();
                let (started_tx, started_rx) = mpsc::channel();
                let (done_tx, done_rx) = mpsc::channel();
                updater = Some(std::thread::spawn(move || {
                    started_tx.send(()).unwrap();
                    overrides::update(&root, |ledger| {
                        let entity_id = "kg_concurrent".to_string();
                        let entity = overrides::RegistryEntity {
                            kind: "project".into(),
                            name: "Concurrent".into(),
                            aliases: vec![],
                            status: "confirmed".into(),
                        };
                        ledger.registry.insert(entity_id.clone(), entity.clone());
                        ledger.operations.push(overrides::KnowledgeOperation {
                            id: "op_concurrent".into(),
                            at: "2026-07-21T00:00:00+00:00".into(),
                            before: serde_json::to_value(overrides::RegistryState {
                                entity_id: entity_id.clone(),
                                entity: None,
                            })?,
                            after: serde_json::to_value(overrides::RegistryState {
                                entity_id,
                                entity: Some(entity.clone()),
                            })?,
                            action: overrides::KnowledgeAction::CreateEntity { entity },
                        });
                        Ok(())
                    })
                    .unwrap();
                    done_tx.send(()).unwrap();
                }));
                started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                assert!(done_rx.recv_timeout(Duration::from_millis(40)).is_err());
                updater_done = Some(done_rx);
            }
            if stage == BuildStage::AfterPublish {
                if let Some(handle) = updater.take() {
                    handle.join().unwrap();
                    updater_done
                        .take()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                }
            }
            Ok(())
        })
        .unwrap();

        assert!(spawned);
        let indexed_digest: String = open_readonly(root.path())
            .unwrap()
            .query_row("SELECT ledger_digest FROM graph_meta", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            indexed_digest,
            ledger_digest(&overrides::load(root.path()).unwrap()).unwrap()
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
                let status_tx = status_tx.clone();
                scope.spawn(move || {
                    scheduler.request(root, move |status| {
                        status_tx.send(status).unwrap();
                    })
                });
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
    fn scheduler_recovers_after_rebuilder_panic() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_build = calls.clone();
        let scheduler = RebuildScheduler::with_rebuilder(move |_| {
            if calls_for_build.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("first rebuild panic");
            }
            Ok(BuildStats::default())
        });
        let root = tempfile::tempdir().unwrap();
        let (first_tx, first_rx) = mpsc::channel();
        scheduler.request(root.path().to_path_buf(), move |status| {
            if status.state == "error" {
                first_tx.send(()).unwrap();
            }
        });
        first_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let (second_tx, second_rx) = mpsc::channel();
        scheduler.request(root.path().to_path_buf(), move |status| {
            if status.state == "ready" {
                second_tx.send(()).unwrap();
            }
        });
        second_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn scheduler_recovers_after_emitter_panic() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_build = calls.clone();
        let scheduler = RebuildScheduler::with_rebuilder(move |_| {
            calls_for_build.fetch_add(1, Ordering::SeqCst);
            Ok(BuildStats::default())
        });
        let root = tempfile::tempdir().unwrap();
        scheduler.request(root.path().to_path_buf(), |_| panic!("emit panic"));
        wait_for_calls(&calls, 1);

        let (tx, rx) = mpsc::channel();
        scheduler.request(root.path().to_path_buf(), move |status| {
            if status.state == "ready" {
                tx.send(()).unwrap();
            }
        });
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn scheduler_dirty_rerun_uses_latest_root_and_emitter() {
        let (build_tx, build_rx) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let gate_for_build = gate.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_build = calls.clone();
        let scheduler = RebuildScheduler::with_rebuilder(move |root| {
            let call = calls_for_build.fetch_add(1, Ordering::SeqCst);
            build_tx.send(root.to_path_buf()).unwrap();
            if call == 0 {
                let (lock, ready) = &*gate_for_build;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = ready.wait(released).unwrap();
                }
            }
            Ok(BuildStats::default())
        });
        let root_a = tempfile::tempdir().unwrap();
        let root_b = tempfile::tempdir().unwrap();
        scheduler.request(root_a.path().to_path_buf(), |_| {});
        assert_eq!(
            build_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            root_a.path()
        );
        let (b_tx, b_rx) = mpsc::channel();
        scheduler.request(root_b.path().to_path_buf(), move |status| {
            if status.state == "ready" {
                b_tx.send(()).unwrap();
            }
        });
        {
            let (lock, ready) = &*gate;
            *lock.lock().unwrap() = true;
            ready.notify_all();
        }
        assert_eq!(
            build_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            root_b.path()
        );
        b_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn scheduler_reports_one_error_after_bounded_digest_mismatch_retries() {
        let snapshots = Arc::new(AtomicUsize::new(0));
        let snapshots_for_build = snapshots.clone();
        let scheduler = RebuildScheduler::with_rebuilder(move |root| {
            rebuild_from_snapshot_source(
                root,
                || {
                    snapshots_for_build.fetch_add(1, Ordering::SeqCst);
                    Ok(("captured".into(), fixture()))
                },
                || Ok("always-different".into()),
                |_| Ok(()),
            )
        });
        let root = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        scheduler.request(root.path().to_path_buf(), move |status| {
            if status.state == "error" {
                tx.send(()).unwrap();
            }
        });
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(snapshots.load(Ordering::SeqCst), 32);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(snapshots.load(Ordering::SeqCst), 32);
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
    fn readonly_open_rejects_a_forged_v2_with_missing_truth_tables() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join(super::super::GRAPH_FILE);
        let conn = rusqlite::Connection::open(&live).unwrap();
        conn.execute(
            "CREATE TABLE graph_meta(schema_version INTEGER, build_id TEXT, ledger_digest TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO graph_meta VALUES(?1, 'forged', 'forged')",
            [GRAPH_SCHEMA_VERSION],
        )
        .unwrap();
        drop(conn);

        assert!(open_readonly(root.path()).is_err());
    }

    #[test]
    fn readonly_open_rejects_more_than_one_graph_meta_row() {
        let root = tempfile::tempdir().unwrap();
        rebuild_atomic(root.path(), &fixture()).unwrap();
        let live = root.path().join(super::super::GRAPH_FILE);
        let conn = rusqlite::Connection::open(&live).unwrap();
        conn.execute("ALTER TABLE graph_meta RENAME TO old_graph_meta", [])
            .unwrap();
        conn.execute(
            "CREATE TABLE graph_meta(\
                id INTEGER, schema_version INTEGER, build_id TEXT, ledger_digest TEXT\
             )",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO graph_meta SELECT * FROM old_graph_meta", [])
            .unwrap();
        conn.execute(
            "INSERT INTO graph_meta(id, schema_version, build_id, ledger_digest) \
             VALUES(2, ?1, 'extra', '')",
            [GRAPH_SCHEMA_VERSION],
        )
        .unwrap();
        conn.execute("DROP TABLE old_graph_meta", []).unwrap();
        drop(conn);

        assert!(open_readonly(root.path()).is_err());
    }

    #[test]
    fn readonly_open_rejects_graph_meta_without_singleton_checks() {
        let root = tempfile::tempdir().unwrap();
        rebuild_atomic(root.path(), &fixture()).unwrap();
        let live = root.path().join(super::super::GRAPH_FILE);
        let conn = rusqlite::Connection::open(&live).unwrap();
        conn.execute("ALTER TABLE graph_meta RENAME TO old_graph_meta", [])
            .unwrap();
        conn.execute(
            "CREATE TABLE graph_meta(\
                id INTEGER PRIMARY KEY, schema_version INTEGER NOT NULL, \
                build_id TEXT NOT NULL, ledger_digest TEXT NOT NULL\
             )",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO graph_meta SELECT * FROM old_graph_meta", [])
            .unwrap();
        conn.execute("DROP TABLE old_graph_meta", []).unwrap();
        drop(conn);

        assert!(open_readonly(root.path()).is_err());
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
