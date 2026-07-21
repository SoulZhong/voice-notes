use super::canonical::{self, CanonicalGraph, PendingItem};
use super::overrides;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const GRAPH_SCHEMA_VERSION: u32 = 2;

const NEXT_FILE: &str = "graph.sqlite.next";
const PREVIOUS_FILE: &str = "graph.sqlite.previous";
const INDEX_LOCK_FILE: &str = ".graph-index.lock";
const CURRENT_POINTER_FILE: &str = ".graph-index-current";
const CURRENT_POINTER_NEXT_FILE: &str = ".graph-index-current.next";
const DIRTY_MARKER_FILE: &str = ".graph-index-dirty";
const VERSION_PREFIX: &str = "graph.sqlite.version.";
const POINTER_FORMAT_VERSION: u32 = 1;
const MAX_POINTER_BYTES: u64 = 1024;
const SAFE_PREVIOUS_VERSIONS: usize = 2;
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
    AfterPointerStageSync,
    BeforePointerCommit,
    AfterPublish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StorageLayout {
    Direct,
    Versioned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CurrentPointer {
    format_version: u32,
    file_name: String,
    build_id: String,
    checksum: String,
}

impl CurrentPointer {
    fn new(file_name: String, build_id: String) -> anyhow::Result<Self> {
        anyhow::ensure!(
            parse_version_file_name(&file_name).is_some(),
            "semantic graph pointer version file is invalid"
        );
        anyhow::ensure!(
            valid_build_id(&build_id),
            "semantic graph pointer build id is invalid"
        );
        let checksum = pointer_checksum(POINTER_FORMAT_VERSION, &file_name, &build_id);
        Ok(Self {
            format_version: POINTER_FORMAT_VERSION,
            file_name,
            build_id,
            checksum,
        })
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.format_version == POINTER_FORMAT_VERSION,
            "unsupported semantic graph pointer version"
        );
        anyhow::ensure!(
            parse_version_file_name(&self.file_name).is_some(),
            "semantic graph pointer version file is invalid"
        );
        anyhow::ensure!(
            valid_build_id(&self.build_id),
            "semantic graph pointer build id is invalid"
        );
        anyhow::ensure!(
            self.checksum == pointer_checksum(self.format_version, &self.file_name, &self.build_id),
            "semantic graph pointer checksum is invalid"
        );
        Ok(())
    }

    fn encode(&self) -> anyhow::Result<Vec<u8>> {
        self.validate()?;
        let mut bytes = serde_json::to_vec(self)?;
        bytes.push(b'\n');
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_POINTER_BYTES,
            "semantic graph pointer is too large"
        );
        Ok(bytes)
    }
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

fn pointer_checksum(format_version: u32, file_name: &str, build_id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"semantic-graph-pointer\0");
    hash.update(format_version.to_le_bytes());
    hash.update(file_name.as_bytes());
    hash.update(b"\0");
    hash.update(build_id.as_bytes());
    hex::encode(hash.finalize())
}

fn valid_build_id(value: &str) -> bool {
    value.len() == "build_".len() + 24
        && value.starts_with("build_")
        && value["build_".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn parse_version_file_name(value: &str) -> Option<u64> {
    let suffix = value.strip_prefix(VERSION_PREFIX)?;
    if suffix.len() != 20 || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    suffix.parse().ok()
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
    build_id: &str,
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
        rusqlite::params![GRAPH_SCHEMA_VERSION, build_id, ledger_digest],
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

fn with_graph_index_lock<T>(
    data_root: &Path,
    action: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    fs::create_dir_all(data_root)?;
    // Lock order is process graph lock -> cross-process index lock -> knowledge lock.
    // Knowledge writers never acquire either graph lock while holding KnowledgeLock.
    let _graph_guard = super::GRAPH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _index_file_lock = GraphIndexFileLock::acquire(data_root)?;
    action()
}

fn build_candidate_with_hook(
    data_root: &Path,
    canonical: &CanonicalGraph,
    digest: &str,
    layout: StorageLayout,
    hook: &mut impl FnMut(BuildStage) -> anyhow::Result<()>,
    publish_if_current: impl FnOnce(&mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<bool>,
) -> anyhow::Result<CandidateOutcome> {
    with_graph_index_lock(data_root, || {
        build_candidate_with_hook_locked(
            data_root,
            canonical,
            digest,
            layout,
            hook,
            publish_if_current,
        )
    })
}

fn build_candidate_with_hook_locked(
    data_root: &Path,
    canonical: &CanonicalGraph,
    digest: &str,
    layout: StorageLayout,
    hook: &mut impl FnMut(BuildStage) -> anyhow::Result<()>,
    publish_if_current: impl FnOnce(&mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<bool>,
) -> anyhow::Result<CandidateOutcome> {
    let next_path = data_root.join(NEXT_FILE);
    match fs::remove_file(&next_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut next_file = NextFile::new(next_path.clone());
    let graph = normalized_graph(canonical);
    let build_id = graph_build_id(&graph)?;
    let stats = {
        let mut connection = rusqlite::Connection::open(&next_path)?;
        connection.busy_timeout(std::time::Duration::from_secs(3))?;
        connection.pragma_update(None, "journal_mode", "DELETE")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.execute_batch(SCHEMA)?;
        hook(BuildStage::AfterSchema)?;
        let transaction = connection.transaction()?;
        let stats = insert_graph(&transaction, &graph, digest, &build_id)?;
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
                    publish_versioned(data_root, &next_path, &build_id, hook)?;
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
    // Low-level contract: `canonical` is a caller-owned snapshot. Callers that derive
    // snapshots from mutable sources must use `rebuild_from_sources`, which samples
    // only after acquiring the cross-process index lock.
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
    with_graph_index_lock(data_root, || {
        for _ in 0..32 {
            let (captured_digest, graph) = snapshot()?;
            let outcome = build_candidate_with_hook_locked(
                data_root,
                &graph,
                &captured_digest,
                platform_layout(),
                &mut hook,
                |publish| accept(&captured_digest, Some(publish)),
            )?;
            match outcome {
                CandidateOutcome::Replaced(stats) if accept(&captured_digest, None)? => {
                    return Ok(stats);
                }
                CandidateOutcome::Replaced(_) | CandidateOutcome::Retry => {}
            }
        }
        anyhow::bail!("knowledge ledger changed too frequently to produce a stable index")
    })
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

fn symlink_metadata_optional(path: &Path) -> std::io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn ensure_regular_file_without_links(path: &Path, metadata: &fs::Metadata) -> anyhow::Result<()> {
    anyhow::ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "semantic graph path is not a regular file: {}",
        path.display()
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        anyhow::ensure!(
            metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "semantic graph path is a reparse point: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn open_read_no_follow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(windows)]
fn open_read_no_follow(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_read_no_follow(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).open(path)
}

fn open_validated_regular_file(path: &Path) -> anyhow::Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    ensure_regular_file_without_links(path, &metadata)?;
    let file = open_read_no_follow(path)?;
    ensure_regular_file_without_links(path, &file.metadata()?)?;
    Ok(file)
}

fn read_current_pointer(data_root: &Path) -> anyhow::Result<Option<CurrentPointer>> {
    use std::io::Read;

    let path = data_root.join(CURRENT_POINTER_FILE);
    let Some(metadata) = symlink_metadata_optional(&path)? else {
        return Ok(None);
    };
    ensure_regular_file_without_links(&path, &metadata)?;
    let file = open_read_no_follow(&path)?;
    ensure_regular_file_without_links(&path, &file.metadata()?)?;
    let mut bytes = Vec::new();
    file.take(MAX_POINTER_BYTES + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_POINTER_BYTES,
        "semantic graph pointer is too large"
    );
    let pointer: CurrentPointer = serde_json::from_slice(&bytes)?;
    pointer.validate()?;
    Ok(Some(pointer))
}

fn next_version_file(data_root: &Path) -> anyhow::Result<(String, PathBuf)> {
    let mut maximum = 0_u64;
    for entry in fs::read_dir(data_root)? {
        let entry = entry?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(sequence) = parse_version_file_name(&file_name) else {
            continue;
        };
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure_regular_file_without_links(&entry.path(), &metadata)?;
        maximum = maximum.max(sequence);
    }
    let sequence = maximum
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("semantic graph version sequence is exhausted"))?;
    let file_name = format!("{VERSION_PREFIX}{sequence:020}");
    Ok((file_name.clone(), data_root.join(file_name)))
}

fn cleanup_version_files(data_root: &Path, current_file: Option<&str>) -> anyhow::Result<()> {
    cleanup_version_files_with(data_root, current_file, |path| fs::remove_file(path))
}

fn cleanup_version_files_with(
    data_root: &Path,
    current_file: Option<&str>,
    mut remove: impl FnMut(&Path) -> std::io::Result<()>,
) -> anyhow::Result<()> {
    let current_sequence = match current_file {
        Some(file_name) => Some(
            parse_version_file_name(file_name)
                .ok_or_else(|| anyhow::anyhow!("semantic graph current version file is invalid"))?,
        ),
        None => None,
    };
    let mut versions = Vec::new();
    for entry in fs::read_dir(data_root)? {
        let entry = entry?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(sequence) = parse_version_file_name(&file_name) else {
            continue;
        };
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if ensure_regular_file_without_links(&path, &metadata).is_err() {
            continue;
        }
        versions.push((sequence, file_name, path));
    }
    versions.sort_by(|left, right| right.0.cmp(&left.0));

    let mut retained_previous = 0_usize;
    for (sequence, file_name, path) in versions {
        if current_file == Some(file_name.as_str()) {
            continue;
        }
        let retain_as_previous = current_sequence.is_some_and(|current| sequence < current)
            && retained_previous < SAFE_PREVIOUS_VERSIONS;
        if retain_as_previous {
            retained_previous += 1;
            continue;
        }
        // Best effort by design: Windows sharing violations mean a held reader keeps
        // its immutable file. A later rebuild retries the same safe deletion.
        let _ = remove(&path);
    }
    Ok(())
}

fn publish_versioned(
    data_root: &Path,
    next_path: &Path,
    build_id: &str,
    hook: &mut impl FnMut(BuildStage) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    use std::io::Write;

    hook(BuildStage::BeforeBackup)?;
    let previous_pointer = read_current_pointer(data_root)?;
    cleanup_version_files(
        data_root,
        previous_pointer
            .as_ref()
            .map(|pointer| pointer.file_name.as_str()),
    )?;
    let (file_name, version_path) = next_version_file(data_root)?;
    let mut version_guard = NextFile::new(version_path.clone());
    let mut source = open_validated_regular_file(next_path)?;
    let mut version = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&version_path)?;
    std::io::copy(&mut source, &mut version)?;
    version.flush()?;
    version.sync_all()?;
    drop(version);
    hook(BuildStage::AfterBackupSync)?;
    hook(BuildStage::BeforeReplace)?;
    let pointer = CurrentPointer::new(file_name, build_id.to_owned())?;
    update_current_pointer(data_root, &pointer, hook)?;
    version_guard.replaced = true;
    cleanup_version_files(data_root, Some(&pointer.file_name))?;
    Ok(())
}

fn update_current_pointer(
    data_root: &Path,
    pointer: &CurrentPointer,
    hook: &mut impl FnMut(BuildStage) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    update_current_pointer_with(data_root, pointer, hook, atomic_install_pointer)
}

fn update_current_pointer_with(
    data_root: &Path,
    pointer: &CurrentPointer,
    hook: &mut impl FnMut(BuildStage) -> anyhow::Result<()>,
    install: impl FnOnce(&Path, &Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    use std::io::Write;

    let pointer_path = data_root.join(CURRENT_POINTER_FILE);
    if let Some(metadata) = symlink_metadata_optional(&pointer_path)? {
        ensure_regular_file_without_links(&pointer_path, &metadata)?;
    }
    let staged_path = data_root.join(CURRENT_POINTER_NEXT_FILE);
    if let Some(metadata) = symlink_metadata_optional(&staged_path)? {
        ensure_regular_file_without_links(&staged_path, &metadata)?;
        fs::remove_file(&staged_path)?;
    }
    let mut staged_guard = NextFile::new(staged_path.clone());
    let mut staged = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&staged_path)?;
    staged.write_all(&pointer.encode()?)?;
    staged.flush()?;
    staged.sync_all()?;
    drop(staged);
    hook(BuildStage::AfterPointerStageSync)?;
    hook(BuildStage::BeforePointerCommit)?;
    install(&staged_path, &pointer_path)?;
    staged_guard.replaced = true;
    sync_directory(data_root)?;
    Ok(())
}

#[cfg(unix)]
fn atomic_install_pointer(staged: &Path, current: &Path) -> anyhow::Result<()> {
    fs::rename(staged, current)?;
    Ok(())
}

#[cfg(windows)]
fn atomic_install_pointer(staged: &Path, current: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let staged = staged
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let current = current
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let installed = unsafe {
        MoveFileExW(
            staged.as_ptr(),
            current.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    anyhow::ensure!(installed != 0, std::io::Error::last_os_error());
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn atomic_install_pointer(staged: &Path, current: &Path) -> anyhow::Result<()> {
    fs::rename(staged, current)?;
    Ok(())
}

pub fn open_readonly(data_root: &Path) -> anyhow::Result<rusqlite::Connection> {
    open_readonly_with_layout(data_root, platform_layout())
}

fn open_readonly_with_layout(
    data_root: &Path,
    layout: StorageLayout,
) -> anyhow::Result<rusqlite::Connection> {
    let (live_path, expected_build_id) = match layout {
        StorageLayout::Direct => (data_root.join(super::GRAPH_FILE), None),
        StorageLayout::Versioned => resolve_current_version(data_root)?,
    };
    if expected_build_id.is_some() {
        open_validated_regular_file(&live_path)?;
    }
    let connection = rusqlite::Connection::open_with_flags(
        &live_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(std::time::Duration::from_secs(3))?;
    connection.pragma_update(None, "query_only", "ON")?;
    validate_read_schema(&connection)?;
    if let Some(expected_build_id) = expected_build_id {
        let actual_build_id: String =
            connection.query_row("SELECT build_id FROM graph_meta", [], |row| row.get(0))?;
        anyhow::ensure!(
            actual_build_id == expected_build_id,
            "semantic graph pointer build id does not match its version"
        );
    }
    Ok(connection)
}

fn current_version_path(data_root: &Path) -> anyhow::Result<PathBuf> {
    resolve_current_version(data_root).map(|(path, _)| path)
}

fn resolve_current_version(data_root: &Path) -> anyhow::Result<(PathBuf, Option<String>)> {
    let Some(pointer) = read_current_pointer(data_root)? else {
        return Ok((data_root.join(super::GRAPH_FILE), None));
    };
    let version_path = data_root.join(&pointer.file_name);
    open_validated_regular_file(&version_path)?;
    Ok((version_path, Some(pointer.build_id)))
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
type SpawnJob = Box<dyn FnOnce() + Send + 'static>;
type SpawnFn = dyn Fn(SpawnJob) -> std::io::Result<()> + Send + Sync + 'static;

enum Rebuilder {
    Sources,
    Custom(Arc<RebuildFn>),
}

enum SchedulerSpawner {
    Thread,
    Custom(Arc<SpawnFn>),
}

impl SchedulerSpawner {
    fn spawn(&self, job: SpawnJob) -> std::io::Result<()> {
        match self {
            Self::Thread => std::thread::Builder::new()
                .name("graph-index-rebuild".into())
                .spawn(job)
                .map(|_| ()),
            Self::Custom(spawn) => spawn(job),
        }
    }
}

#[derive(Default)]
struct SchedulerState {
    generation: u64,
    dirty: bool,
    running_generation: Option<u64>,
    latest: Option<RebuildRequest>,
}

struct SchedulerInner {
    state: Mutex<SchedulerState>,
    rebuilder: Rebuilder,
    spawner: SchedulerSpawner,
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
                state: Mutex::new(SchedulerState::default()),
                rebuilder: Rebuilder::Sources,
                spawner: SchedulerSpawner::Thread,
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
                state: Mutex::new(SchedulerState::default()),
                rebuilder: Rebuilder::Custom(Arc::new(rebuild)),
                spawner: SchedulerSpawner::Thread,
            }),
        }
    }

    #[cfg(test)]
    fn with_rebuilder_and_spawner(
        rebuild: impl Fn(&Path) -> anyhow::Result<BuildStats> + Send + Sync + 'static,
        spawn: impl Fn(SpawnJob) -> std::io::Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner: Arc::new(SchedulerInner {
                state: Mutex::new(SchedulerState::default()),
                rebuilder: Rebuilder::Custom(Arc::new(rebuild)),
                spawner: SchedulerSpawner::Custom(Arc::new(spawn)),
            }),
        }
    }

    pub fn request(
        &self,
        data_root: PathBuf,
        emit: impl Fn(IndexStatus) + Send + Sync + 'static,
    ) -> anyhow::Result<()> {
        let generation_to_spawn = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            persist_dirty_marker(&data_root)?;
            state.generation = state
                .generation
                .checked_add(1)
                .expect("semantic graph scheduler generation exhausted");
            state.latest = Some(RebuildRequest {
                data_root,
                emit: Arc::new(emit),
            });
            state.dirty = true;
            if state.running_generation.is_none() {
                let generation = state.generation;
                state.running_generation = Some(generation);
                Some(generation)
            } else {
                None
            }
        };
        if let Some(generation) = generation_to_spawn {
            spawn_claimed_scheduler(self.inner.clone(), generation)?;
        }
        Ok(())
    }
}

fn persist_dirty_marker(data_root: &Path) -> anyhow::Result<()> {
    use std::io::Write;

    let path = data_root.join(DIRTY_MARKER_FILE);
    let mut marker = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)?;
    marker.write_all(b"semantic-graph-dirty\n")?;
    marker.sync_all()?;
    sync_directory(data_root)?;
    Ok(())
}

fn clear_dirty_marker(data_root: &Path) -> anyhow::Result<()> {
    match fs::remove_file(data_root.join(DIRTY_MARKER_FILE)) {
        Ok(()) => sync_directory(data_root)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

struct RunningGuard {
    inner: Arc<SchedulerInner>,
    generation: u64,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        let generation_to_spawn = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.running_generation != Some(self.generation) {
                None
            } else {
                state.running_generation = None;
                if state.dirty {
                    let generation = state.generation;
                    state.running_generation = Some(generation);
                    Some(generation)
                } else {
                    None
                }
            }
        };
        if let Some(generation) = generation_to_spawn {
            let _ = spawn_claimed_scheduler(self.inner.clone(), generation);
        }
    }
}

fn spawn_claimed_scheduler(inner: Arc<SchedulerInner>, generation: u64) -> anyhow::Result<()> {
    let worker_inner = inner.clone();
    let job: SpawnJob = Box::new(move || run_scheduler(worker_inner, generation));
    if let Err(error) = inner.spawner.spawn(job) {
        eprintln!("graph: unable to start semantic index rebuild: {error}");
        let (newer_generation, request_for_error) = {
            let mut state = inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.running_generation != Some(generation) {
                (None, None)
            } else {
                state.running_generation = None;
                if state.dirty && state.generation > generation {
                    let newer_generation = state.generation;
                    state.running_generation = Some(newer_generation);
                    (Some(newer_generation), None)
                } else {
                    (None, state.latest.clone())
                }
            }
        };
        if let Some(newer_generation) = newer_generation {
            return spawn_claimed_scheduler(inner, newer_generation);
        } else if let Some(request) = request_for_error {
            emit_status(
                &request.emit,
                IndexStatus {
                    state: "error".into(),
                    error: Some(STATUS_ERROR.into()),
                    stats: None,
                },
            );
        }
        anyhow::bail!("semantic graph rebuild is pending retry");
    }
    Ok(())
}

fn emit_status(emit: &Arc<EmitFn>, status: IndexStatus) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| emit(status)));
}

fn run_scheduler(inner: Arc<SchedulerInner>, generation: u64) {
    let _running_guard = RunningGuard {
        inner: inner.clone(),
        generation,
    };
    loop {
        let request = {
            let mut state = inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !state.dirty {
                None
            } else {
                state.dirty = false;
                state.latest.clone()
            }
        };
        let Some(request) = request else {
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
            Ok(stats) => {
                let clear_result = {
                    let state = inner
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let should_clear = !state.dirty
                        || state.latest.as_ref().is_some_and(|latest| {
                            latest.data_root.as_path() != request.data_root.as_path()
                        });
                    if should_clear {
                        clear_dirty_marker(&request.data_root)
                    } else {
                        Ok(())
                    }
                };
                if let Err(error) = clear_result {
                    eprintln!("graph: unable to clear semantic index retry marker: {error:#}");
                }
                emit_status(
                    &request.emit,
                    IndexStatus {
                        state: "ready".into(),
                        error: None,
                        stats: Some(stats),
                    },
                );
            }
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
    fn repeated_versioned_rebuilds_keep_a_bounded_safe_version_set() {
        let root = tempfile::tempdir().unwrap();
        for generation in 0..8 {
            let mut graph = fixture();
            graph.entities.get_mut("kg_a").unwrap().name = format!("Alice {generation}");
            rebuild_atomic_with_layout(root.path(), &graph, StorageLayout::Versioned, |_| Ok(()))
                .unwrap();
        }

        let versions = std::fs::read_dir(root.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| parse_version_file_name(&entry.file_name().to_string_lossy()).is_some())
            .count();
        assert!(versions <= 3, "retained {versions} immutable versions");
        assert_eq!(
            open_readonly_with_layout(root.path(), StorageLayout::Versioned)
                .unwrap()
                .query_row("SELECT name FROM entities WHERE id = 'kg_a'", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "Alice 7"
        );
    }

    #[test]
    fn held_version_delete_failure_is_retained_then_retried_after_reader_closes() {
        let root = tempfile::tempdir().unwrap();
        for sequence in 1..=5 {
            std::fs::write(
                root.path().join(format!("{VERSION_PREFIX}{sequence:020}")),
                format!("version {sequence}"),
            )
            .unwrap();
        }
        let held_path = root
            .path()
            .join("graph.sqlite.version.00000000000000000001");
        let held_reader = std::fs::File::open(&held_path).unwrap();
        let current = "graph.sqlite.version.00000000000000000005";

        cleanup_version_files_with(root.path(), Some(current), |path| {
            if path == held_path {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "simulated Windows sharing violation",
                ));
            }
            std::fs::remove_file(path)
        })
        .unwrap();
        assert!(held_path.exists());
        assert_eq!(
            std::fs::read_dir(root.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    parse_version_file_name(&entry.file_name().to_string_lossy()).is_some()
                })
                .count(),
            4
        );

        drop(held_reader);
        cleanup_version_files(root.path(), Some(current)).unwrap();
        assert!(!held_path.exists());
        assert_eq!(
            std::fs::read_dir(root.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    parse_version_file_name(&entry.file_name().to_string_lossy()).is_some()
                })
                .count(),
            3
        );
    }

    #[test]
    fn current_pointer_is_a_small_immutable_document_not_a_sqlite_database() {
        let root = tempfile::tempdir().unwrap();
        rebuild_atomic_with_layout(
            root.path(),
            &fixture(),
            StorageLayout::Versioned,
            |_| Ok(()),
        )
        .unwrap();

        let bytes = std::fs::read(root.path().join(CURRENT_POINTER_FILE)).unwrap();
        assert!(bytes.len() < 1024);
        assert_eq!(bytes.first(), Some(&b'{'));
        let sqlite_probe = rusqlite::Connection::open_with_flags(
            root.path().join(CURRENT_POINTER_FILE),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        assert!(sqlite_probe
            .query_row("PRAGMA schema_version", [], |row| row.get::<_, u32>(0))
            .is_err());
    }

    #[test]
    fn version_file_names_require_exactly_twenty_ascii_digits() {
        assert_eq!(
            parse_version_file_name("graph.sqlite.version.00000000000000000001"),
            Some(1)
        );
        for invalid in [
            "graph.sqlite.version.",
            "graph.sqlite.version.0000000000000000001",
            "graph.sqlite.version.000000000000000000001",
            "graph.sqlite.version.0000000000000000000x",
            "graph.sqlite.version.０００００００００００００００００００１",
            "graph.sqlite.version.00000000000000000001/escape",
            "other.00000000000000000001",
        ] {
            assert_eq!(parse_version_file_name(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn staged_pointer_failure_keeps_old_pointer_and_cleans_the_stage() {
        let root = tempfile::tempdir().unwrap();
        rebuild_atomic_with_layout(
            root.path(),
            &fixture(),
            StorageLayout::Versioned,
            |_| Ok(()),
        )
        .unwrap();
        let pointer_path = root.path().join(CURRENT_POINTER_FILE);
        let original_pointer = std::fs::read(&pointer_path).unwrap();
        let original_build =
            build_id(&open_readonly_with_layout(root.path(), StorageLayout::Versioned).unwrap());
        let mut changed = fixture();
        changed.entities.get_mut("kg_a").unwrap().name = "Never Current".into();

        rebuild_atomic_with_layout(root.path(), &changed, StorageLayout::Versioned, |stage| {
            anyhow::ensure!(
                stage != BuildStage::AfterPointerStageSync,
                "staged pointer failure"
            );
            Ok(())
        })
        .unwrap_err();

        assert_eq!(std::fs::read(pointer_path).unwrap(), original_pointer);
        assert!(!root.path().join(CURRENT_POINTER_NEXT_FILE).exists());
        assert_eq!(
            build_id(&open_readonly_with_layout(root.path(), StorageLayout::Versioned).unwrap()),
            original_build
        );
    }

    #[test]
    fn stale_staged_pointer_artifact_is_ignored_and_replaced_on_the_next_build() {
        let root = tempfile::tempdir().unwrap();
        rebuild_atomic_with_layout(
            root.path(),
            &fixture(),
            StorageLayout::Versioned,
            |_| Ok(()),
        )
        .unwrap();
        let old_build =
            build_id(&open_readonly_with_layout(root.path(), StorageLayout::Versioned).unwrap());
        std::fs::write(
            root.path().join(CURRENT_POINTER_NEXT_FILE),
            b"process died after staging this garbage",
        )
        .unwrap();
        assert_eq!(
            build_id(&open_readonly_with_layout(root.path(), StorageLayout::Versioned).unwrap()),
            old_build
        );

        let mut changed = fixture();
        changed.entities.get_mut("kg_a").unwrap().name = "Recovered Alice".into();
        rebuild_atomic_with_layout(root.path(), &changed, StorageLayout::Versioned, |_| Ok(()))
            .unwrap();

        assert!(!root.path().join(CURRENT_POINTER_NEXT_FILE).exists());
        assert_ne!(
            build_id(&open_readonly_with_layout(root.path(), StorageLayout::Versioned).unwrap()),
            old_build
        );
    }

    #[test]
    fn pointer_installer_adapter_preserves_old_or_installs_new_as_one_document() {
        let root = tempfile::tempdir().unwrap();
        let old = CurrentPointer::new(
            "graph.sqlite.version.00000000000000000001".into(),
            "build_000000000000000000000001".into(),
        )
        .unwrap();
        update_current_pointer_with(root.path(), &old, &mut |_| Ok(()), |staged, current| {
            assert!(!current.exists());
            std::fs::rename(staged, current)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            read_current_pointer(root.path()).unwrap(),
            Some(old.clone())
        );

        let new = CurrentPointer::new(
            "graph.sqlite.version.00000000000000000002".into(),
            "build_000000000000000000000002".into(),
        )
        .unwrap();
        update_current_pointer_with(root.path(), &new, &mut |_| Ok(()), |_staged, _current| {
            anyhow::bail!("install failed before replacement")
        })
        .unwrap_err();
        assert_eq!(read_current_pointer(root.path()).unwrap(), Some(old));
        assert!(!root.path().join(CURRENT_POINTER_NEXT_FILE).exists());

        update_current_pointer_with(root.path(), &new, &mut |_| Ok(()), |staged, current| {
            std::fs::rename(staged, current)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(read_current_pointer(root.path()).unwrap(), Some(new));
    }

    #[test]
    fn tampered_pointer_checksum_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        rebuild_atomic_with_layout(
            root.path(),
            &fixture(),
            StorageLayout::Versioned,
            |_| Ok(()),
        )
        .unwrap();
        let pointer_path = root.path().join(CURRENT_POINTER_FILE);
        let mut pointer: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&pointer_path).unwrap()).unwrap();
        pointer["build_id"] = serde_json::Value::String("build_ffffffffffffffffffffffff".into());
        std::fs::write(pointer_path, serde_json::to_vec(&pointer).unwrap()).unwrap();

        assert!(open_readonly_with_layout(root.path(), StorageLayout::Versioned).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn pointer_and_version_symlinks_are_rejected_without_following_them() {
        use std::os::unix::fs::symlink;

        let pointer_root = tempfile::tempdir().unwrap();
        rebuild_atomic_with_layout(
            pointer_root.path(),
            &fixture(),
            StorageLayout::Versioned,
            |_| Ok(()),
        )
        .unwrap();
        let pointer_path = pointer_root.path().join(CURRENT_POINTER_FILE);
        let pointer_target = pointer_root.path().join("pointer.real");
        std::fs::rename(&pointer_path, &pointer_target).unwrap();
        symlink(&pointer_target, &pointer_path).unwrap();
        assert!(read_current_pointer(pointer_root.path()).is_err());

        let version_root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        rebuild_atomic_with_layout(
            version_root.path(),
            &fixture(),
            StorageLayout::Versioned,
            |_| Ok(()),
        )
        .unwrap();
        let version_path = current_version_path(version_root.path()).unwrap();
        let outside_version = outside.path().join("outside.sqlite");
        std::fs::rename(&version_path, &outside_version).unwrap();
        symlink(&outside_version, &version_path).unwrap();
        assert!(open_readonly_with_layout(version_root.path(), StorageLayout::Versioned).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_legal_version_symlink_is_rejected_not_treated_as_absent() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let missing_target = outside.path().join("must-not-be-created.sqlite");
        symlink(
            &missing_target,
            root.path()
                .join("graph.sqlite.version.00000000000000000001"),
        )
        .unwrap();

        assert!(rebuild_atomic_with_layout(
            root.path(),
            &fixture(),
            StorageLayout::Versioned,
            |_| Ok(())
        )
        .is_err());
        assert!(!missing_target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_pointer_symlink_is_rejected_not_treated_as_absent() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let missing_target = outside.path().join("must-not-be-created.pointer");
        symlink(&missing_target, root.path().join(CURRENT_POINTER_FILE)).unwrap();

        assert!(read_current_pointer(root.path()).is_err());
        assert!(rebuild_atomic_with_layout(
            root.path(),
            &fixture(),
            StorageLayout::Versioned,
            |_| Ok(())
        )
        .is_err());
        assert!(!missing_target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn version_cleanup_never_deletes_unknown_names_or_links() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_target = outside.path().join("outside.sqlite");
        std::fs::write(&outside_target, b"outside").unwrap();
        let legal_link = root
            .path()
            .join("graph.sqlite.version.00000000000000000001");
        let unknown_file = root.path().join("graph.sqlite.version.not-a-version");
        let unknown_link = root.path().join("unrelated-link");
        symlink(&outside_target, &legal_link).unwrap();
        std::fs::write(&unknown_file, b"unknown").unwrap();
        symlink(&outside_target, &unknown_link).unwrap();

        cleanup_version_files(root.path(), None).unwrap();

        assert!(std::fs::symlink_metadata(&legal_link).is_ok());
        assert!(unknown_file.exists());
        assert!(std::fs::symlink_metadata(&unknown_link).is_ok());
        assert_eq!(std::fs::read(&outside_target).unwrap(), b"outside");
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

        assert!(!root.path().join(CURRENT_POINTER_FILE).exists());
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
        assert!(root.path().join(CURRENT_POINTER_FILE).exists());
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
    fn waiting_source_resamples_only_after_acquiring_the_index_lock() {
        let root = tempfile::tempdir().unwrap();
        let state = Arc::new(AtomicUsize::new(0));
        let (a_locked_tx, a_locked_rx) = mpsc::channel();
        let (release_a_tx, release_a_rx) = mpsc::channel();
        let root_a = root.path().to_path_buf();
        let state_a = state.clone();
        let a = std::thread::spawn(move || {
            let mut paused = false;
            rebuild_from_snapshot_source(
                &root_a,
                || {
                    let sampled = state_a.load(Ordering::SeqCst);
                    let mut graph = fixture();
                    graph.entities.get_mut("kg_a").unwrap().name = if sampled == 0 {
                        "Old Alice"
                    } else {
                        "New Alice"
                    }
                    .into();
                    Ok(("same-ledger-digest".into(), graph))
                },
                || Ok("same-ledger-digest".into()),
                |stage| {
                    if stage == BuildStage::AfterSchema && !paused {
                        paused = true;
                        a_locked_tx.send(()).unwrap();
                        release_a_rx.recv().unwrap();
                    }
                    Ok(())
                },
            )
            .unwrap();
        });
        a_locked_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let (sampled_tx, sampled_rx) = mpsc::channel();
        let root_b = root.path().to_path_buf();
        let state_b = state.clone();
        let b = std::thread::spawn(move || {
            rebuild_from_snapshot_source(
                &root_b,
                || {
                    let sampled = state_b.load(Ordering::SeqCst);
                    sampled_tx.send(sampled).unwrap();
                    let mut graph = fixture();
                    graph.entities.get_mut("kg_a").unwrap().name = if sampled == 0 {
                        "Old Alice"
                    } else {
                        "New Alice"
                    }
                    .into();
                    Ok(("same-ledger-digest".into(), graph))
                },
                || Ok("same-ledger-digest".into()),
                |_| Ok(()),
            )
            .unwrap();
        });

        let early_sample = sampled_rx.recv_timeout(Duration::from_millis(50));
        state.store(1, Ordering::SeqCst);
        release_a_tx.send(()).unwrap();
        a.join().unwrap();
        b.join().unwrap();
        let sampled_early = early_sample.is_ok();
        let sampled = early_sample.unwrap_or_else(|_| sampled_rx.recv().unwrap());

        assert!(!sampled_early, "waiting source sampled before the lock");
        assert_eq!(sampled, 1);
        assert_eq!(
            open_readonly(root.path())
                .unwrap()
                .query_row("SELECT name FROM entities WHERE id = 'kg_a'", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "New Alice"
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
        scheduler
            .request(root.path().to_path_buf(), {
                let status_tx = status_tx.clone();
                move |status| {
                    status_tx.send(status).unwrap();
                }
            })
            .unwrap();
        assert_eq!(started_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 1);

        std::thread::scope(|scope| {
            for _ in 0..16 {
                let scheduler = scheduler.clone();
                let root = root.path().to_path_buf();
                let status_tx = status_tx.clone();
                scope.spawn(move || {
                    scheduler
                        .request(root, move |status| {
                            status_tx.send(status).unwrap();
                        })
                        .unwrap();
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
        scheduler
            .request(root.path().to_path_buf(), move |status| {
                if status.state == "ready" {
                    late_tx.send(()).unwrap();
                }
            })
            .unwrap();
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
        scheduler
            .request(root.path().to_path_buf(), move |status| {
                if status.state == "error" {
                    tx.send(status).unwrap();
                }
            })
            .unwrap();
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
        scheduler
            .request(root.path().to_path_buf(), move |status| {
                if status.state == "error" {
                    first_tx.send(()).unwrap();
                }
            })
            .unwrap();
        first_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let (second_tx, second_rx) = mpsc::channel();
        scheduler
            .request(root.path().to_path_buf(), move |status| {
                if status.state == "ready" {
                    second_tx.send(()).unwrap();
                }
            })
            .unwrap();
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
        scheduler
            .request(root.path().to_path_buf(), |_| panic!("emit panic"))
            .unwrap();
        wait_for_calls(&calls, 1);

        let (tx, rx) = mpsc::channel();
        scheduler
            .request(root.path().to_path_buf(), move |status| {
                if status.state == "ready" {
                    tx.send(()).unwrap();
                }
            })
            .unwrap();
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
        scheduler
            .request(root_a.path().to_path_buf(), |_| {})
            .unwrap();
        assert_eq!(
            build_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            root_a.path()
        );
        let (b_tx, b_rx) = mpsc::channel();
        scheduler
            .request(root_b.path().to_path_buf(), move |status| {
                if status.state == "ready" {
                    b_tx.send(()).unwrap();
                }
            })
            .unwrap();
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
        scheduler
            .request(root.path().to_path_buf(), move |status| {
                if status.state == "error" {
                    tx.send(()).unwrap();
                }
            })
            .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(snapshots.load(Ordering::SeqCst), 32);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(snapshots.load(Ordering::SeqCst), 32);
    }

    #[test]
    fn scheduler_spawn_failure_with_a_newer_request_runs_the_new_generation() {
        let (spawn_started_tx, spawn_started_rx) = mpsc::channel();
        let (release_spawn_tx, release_spawn_rx) = mpsc::channel();
        let release_spawn_rx = Arc::new(Mutex::new(release_spawn_rx));
        let spawn_calls = Arc::new(AtomicUsize::new(0));
        let spawn_calls_for_spawner = spawn_calls.clone();
        let release_spawn_rx_for_spawner = release_spawn_rx.clone();
        let (build_tx, build_rx) = mpsc::channel();
        let scheduler = RebuildScheduler::with_rebuilder_and_spawner(
            move |root| {
                build_tx.send(root.to_path_buf()).unwrap();
                Ok(BuildStats::default())
            },
            move |job: SpawnJob| {
                let call = spawn_calls_for_spawner.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    spawn_started_tx.send(()).unwrap();
                    release_spawn_rx_for_spawner.lock().unwrap().recv().unwrap();
                    return Err(std::io::Error::other("injected spawn failure"));
                }
                std::thread::Builder::new()
                    .name("test-graph-index-rebuild".into())
                    .spawn(job)
                    .map(|_| ())
            },
        );
        let root_a = tempfile::tempdir().unwrap();
        let root_b = tempfile::tempdir().unwrap();
        let scheduler_a = scheduler.clone();
        let root_a_path = root_a.path().to_path_buf();
        let request_a = std::thread::spawn(move || scheduler_a.request(root_a_path, |_| {}));
        spawn_started_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap();

        let (b_tx, b_rx) = mpsc::channel();
        scheduler
            .request(root_b.path().to_path_buf(), move |status| {
                if status.state == "ready" {
                    b_tx.send(()).unwrap();
                }
            })
            .unwrap();
        release_spawn_tx.send(()).unwrap();
        request_a.join().unwrap().unwrap();

        b_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            build_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            root_b.path()
        );
        assert_eq!(spawn_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn scheduler_spawn_failure_without_a_new_request_reports_once_without_retry_loop() {
        let spawn_calls = Arc::new(AtomicUsize::new(0));
        let spawn_calls_for_spawner = spawn_calls.clone();
        let scheduler = RebuildScheduler::with_rebuilder_and_spawner(
            |_| Ok(BuildStats::default()),
            move |_job: SpawnJob| {
                spawn_calls_for_spawner.fetch_add(1, Ordering::SeqCst);
                Err(std::io::Error::other("injected spawn failure"))
            },
        );
        let root = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        scheduler
            .request(root.path().to_path_buf(), move |status| {
                if status.state == "error" {
                    tx.send(()).unwrap();
                }
            })
            .unwrap_err();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(spawn_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn scheduler_spawn_failure_returns_error_and_keeps_durable_retry_marker() {
        let spawn_calls = Arc::new(AtomicUsize::new(0));
        let spawn_calls_for_spawner = spawn_calls.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let scheduler = RebuildScheduler::with_rebuilder_and_spawner(
            |_| Ok(BuildStats::default()),
            move |job: SpawnJob| {
                if spawn_calls_for_spawner.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(std::io::Error::other("injected spawn failure"));
                }
                std::thread::Builder::new()
                    .name("test-graph-index-retry".into())
                    .spawn(job)
                    .map(|_| ())
            },
        );
        let root = tempfile::tempdir().unwrap();

        assert!(scheduler
            .request(root.path().to_path_buf(), |_| {})
            .is_err());
        assert!(root.path().join(DIRTY_MARKER_FILE).is_file());
        scheduler
            .request(root.path().to_path_buf(), move |status| {
                if status.state == "ready" {
                    ready_tx.send(()).unwrap();
                }
            })
            .unwrap();
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!root.path().join(DIRTY_MARKER_FILE).exists());
        assert_eq!(spawn_calls.load(Ordering::SeqCst), 2);
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
