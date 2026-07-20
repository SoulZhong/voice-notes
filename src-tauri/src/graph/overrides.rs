use crate::store::{stable_id, RelationPredicate};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

pub const KNOWLEDGE_FILE: &str = "knowledge-overrides.json";
pub const KNOWLEDGE_LOCK_FILE: &str = ".knowledge-overrides.lock";

const KNOWLEDGE_BACKUP_FILE: &str = "knowledge-overrides.json.bak";
const KNOWLEDGE_TMP_FILE: &str = "knowledge-overrides.json.tmp";
const SCHEMA_VERSION: u32 = 1;
const INITIAL_BYTES: &[u8] =
    br#"{"schema_version":1,"registry":{},"legacy_ids":{},"operations":[]}"#;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct KnowledgeLedger {
    pub schema_version: u32,
    pub registry: BTreeMap<String, RegistryEntity>,
    pub legacy_ids: BTreeMap<String, String>,
    pub operations: Vec<KnowledgeOperation>,
}

impl KnowledgeLedger {
    pub fn empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryEntity {
    pub kind: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeOperation {
    pub id: String,
    pub at: String,
    pub before: serde_json::Value,
    pub after: serde_json::Value,
    #[serde(flatten)]
    pub action: KnowledgeAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum KnowledgeAction {
    RenameEntity {
        entity_id: String,
        name: String,
    },
    AddAlias {
        entity_id: String,
        alias: String,
    },
    RemoveAlias {
        entity_id: String,
        alias: String,
    },
    MergeEntity {
        source_id: String,
        target_id: String,
    },
    BindMention {
        mention_id: String,
        entity_id: String,
    },
    ConfirmRelation {
        relation_id: String,
    },
    EditRelation {
        relation_id: String,
        subject_id: String,
        predicate: RelationPredicate,
        object_id: String,
        valid_from: Option<String>,
        valid_to: Option<String>,
        note: Option<String>,
    },
    SuppressRelation {
        subject_id: String,
        predicate: RelationPredicate,
        object_id: String,
    },
    EndRelation {
        relation_id: String,
        valid_to: String,
    },
    RestoreRelation {
        operation_id: String,
    },
    CreateEntity {
        entity: RegistryEntity,
    },
    CreateRelation {
        relation: UserRelation,
    },
    Undo {
        operation_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserRelation {
    pub subject_id: String,
    pub predicate: RelationPredicate,
    pub object_id: String,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub note: Option<String>,
    pub evidence_ids: Vec<String>,
    pub user_assertion: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum KnowledgeLoadError {
    #[error("cannot read knowledge ledger {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("corrupt knowledge ledger {path}: {message}")]
    Corrupt { path: PathBuf, message: String },
}

pub fn allocate_entity_id(kind: &str, name: &str, note_id: &str, local_id: &str) -> String {
    stable_id(
        "kg_",
        &[
            kind.trim().to_lowercase(),
            name.trim().to_lowercase(),
            note_id.to_string(),
            local_id.to_string(),
        ],
    )
}

pub fn allocate_split_entity_id(operation_id: &str) -> String {
    stable_id("kg_", &["split".into(), operation_id.into()])
}

pub fn load(data_root: &Path) -> Result<KnowledgeLedger, KnowledgeLoadError> {
    let path = data_root.join(KNOWLEDGE_FILE);
    match read_ledger(&path) {
        Ok(Some(ledger)) => Ok(ledger),
        Ok(None) => initialize_missing(data_root),
        Err(error) => Err(error),
    }
}

pub fn update<T>(
    data_root: &Path,
    change: impl FnOnce(&mut KnowledgeLedger) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    fs::create_dir_all(data_root)?;
    let _lock = KnowledgeLock::acquire(data_root)?;
    let mut ledger = load_locked(data_root)?;
    let existing_operations = ledger.operations.clone();
    let result = change(&mut ledger)?;
    anyhow::ensure!(
        ledger.operations.starts_with(&existing_operations),
        "knowledge operations are append-only"
    );
    validate_ledger(&ledger).map_err(anyhow::Error::msg)?;
    write_ledger_atomic(data_root, &ledger)?;
    Ok(result)
}

fn initialize_missing(data_root: &Path) -> Result<KnowledgeLedger, KnowledgeLoadError> {
    fs::create_dir_all(data_root).map_err(|source| KnowledgeLoadError::Io {
        path: data_root.into(),
        source,
    })?;
    let _lock = KnowledgeLock::acquire(data_root).map_err(|source| KnowledgeLoadError::Io {
        path: data_root.join(KNOWLEDGE_LOCK_FILE),
        source,
    })?;
    load_locked(data_root)
}

fn load_locked(data_root: &Path) -> Result<KnowledgeLedger, KnowledgeLoadError> {
    let path = data_root.join(KNOWLEDGE_FILE);
    if let Some(ledger) = read_ledger(&path)? {
        return Ok(ledger);
    }
    let ledger = bootstrap_legacy(data_root).map_err(|error| KnowledgeLoadError::Corrupt {
        path: data_root.join(super::GRAPH_FILE),
        message: error.to_string(),
    })?;
    let write_result = if ledger == KnowledgeLedger::empty() {
        write_bytes_atomic_with(data_root, INITIAL_BYTES, || Ok(()))
    } else {
        write_ledger_atomic(data_root, &ledger)
    };
    write_result.map_err(|error| KnowledgeLoadError::Io {
        path,
        source: std::io::Error::other(error),
    })?;
    Ok(ledger)
}

fn read_ledger(path: &Path) -> Result<Option<KnowledgeLedger>, KnowledgeLoadError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(KnowledgeLoadError::Io {
                path: path.into(),
                source,
            })
        }
    };
    let ledger: KnowledgeLedger =
        serde_json::from_slice(&bytes).map_err(|error| KnowledgeLoadError::Corrupt {
            path: path.into(),
            message: error.to_string(),
        })?;
    validate_ledger(&ledger).map_err(|message| KnowledgeLoadError::Corrupt {
        path: path.into(),
        message,
    })?;
    Ok(Some(ledger))
}

fn validate_ledger(ledger: &KnowledgeLedger) -> Result<(), String> {
    if ledger.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported schema_version {}",
            ledger.schema_version
        ));
    }
    let mut operation_ids = std::collections::BTreeSet::new();
    for operation in &ledger.operations {
        if operation.id.trim().is_empty() {
            return Err("operation id must not be empty".into());
        }
        if !operation_ids.insert(operation.id.as_str()) {
            return Err(format!("duplicate operation id {}", operation.id));
        }
        if let KnowledgeAction::CreateRelation { relation } = &operation.action {
            if relation.evidence_ids.is_empty() && !relation.user_assertion {
                return Err(format!(
                    "operation {} relation requires evidence or user_assertion",
                    operation.id
                ));
            }
        }
    }
    Ok(())
}

struct KnowledgeLock {
    _file: File,
}

impl KnowledgeLock {
    fn acquire(data_root: &Path) -> std::io::Result<Self> {
        const RETRIES: u32 = 5;
        const BACKOFF: std::time::Duration = std::time::Duration::from_millis(20);
        for attempt in 0..RETRIES {
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .open(data_root.join(KNOWLEDGE_LOCK_FILE))?;
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::WouldBlock) if attempt + 1 < RETRIES => {
                    std::thread::sleep(BACKOFF);
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "knowledge ledger lock is held",
                    ));
                }
                Err(TryLockError::Error(error)) => return Err(error),
            }
        }
        unreachable!("bounded lock retry loop always returns")
    }
}

fn write_ledger_atomic(data_root: &Path, ledger: &KnowledgeLedger) -> anyhow::Result<()> {
    write_ledger_atomic_with(data_root, ledger, || Ok(()))
}

fn write_ledger_atomic_with(
    data_root: &Path,
    ledger: &KnowledgeLedger,
    before_rename: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(ledger)?;
    write_bytes_atomic_with(data_root, &bytes, before_rename)?;
    Ok(())
}

fn write_bytes_atomic_with<E>(
    data_root: &Path,
    bytes: &[u8],
    before_rename: impl FnOnce() -> Result<(), E>,
) -> Result<(), E>
where
    E: From<std::io::Error>,
{
    let tmp = data_root.join(KNOWLEDGE_TMP_FILE);
    let live = data_root.join(KNOWLEDGE_FILE);
    let backup = data_root.join(KNOWLEDGE_BACKUP_FILE);
    let mut committed = false;
    let result = (|| {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        std::io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
        if fs::metadata(&live).is_ok() {
            fs::copy(&live, &backup)?;
            File::open(&backup)?.sync_all()?;
        }
        before_rename()?;
        fs::rename(&tmp, &live)?;
        committed = true;
        sync_directory(data_root)?;
        Ok(())
    })();
    if !committed {
        let _ = fs::remove_file(tmp);
    }
    result
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn bootstrap_legacy(data_root: &Path) -> anyhow::Result<KnowledgeLedger> {
    let database = data_root.join(super::GRAPH_FILE);
    if !database.exists() {
        return Ok(KnowledgeLedger::empty());
    }
    let connection = rusqlite::Connection::open_with_flags(
        database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let table_exists = |name: &str| -> rusqlite::Result<bool> {
        connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [name],
            |row| row.get(0),
        )
    };
    if !table_exists("entities")? {
        return Ok(KnowledgeLedger::empty());
    }

    let mut overrides = BTreeMap::new();
    if table_exists("entity_name_overrides")? {
        let mut statement =
            connection.prepare("SELECT id, name FROM entity_name_overrides ORDER BY id")?;
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (id, name) = row?;
            overrides.insert(id, name);
        }
    }

    let mut ledger = KnowledgeLedger::empty();
    let mut ordered_operations = Vec::new();
    let mut statement =
        connection.prepare("SELECT id, kind, name, aliases FROM entities ORDER BY id")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (old_id, kind, extracted_name, aliases_json) = row?;
        let name = overrides.get(&old_id).cloned().unwrap_or(extracted_name);
        let entity = RegistryEntity {
            kind: kind.clone(),
            name: name.clone(),
            aliases: serde_json::from_str(&aliases_json).unwrap_or_default(),
            status: "confirmed".into(),
        };
        let entity_id = allocate_entity_id(&kind, &name, "legacy", &old_id);
        ledger.registry.insert(entity_id.clone(), entity.clone());
        ledger.legacy_ids.insert(old_id.clone(), entity_id);
        let operation_id = stable_id("op_", &["legacy_create".into(), old_id.clone()]);
        ordered_operations.push((
            old_id,
            0_u8,
            KnowledgeOperation {
                id: operation_id,
                at: "legacy_bootstrap".into(),
                before: serde_json::Value::Null,
                after: serde_json::to_value(&entity)?,
                action: KnowledgeAction::CreateEntity { entity },
            },
        ));
    }

    if table_exists("entity_redirects")? {
        let mut statement =
            connection.prepare("SELECT old_id, new_id FROM entity_redirects ORDER BY old_id")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (old_id, new_id) = row?;
            let source_id = ledger
                .legacy_ids
                .get(&old_id)
                .cloned()
                .unwrap_or_else(|| allocate_entity_id("legacy", "", "legacy", &old_id));
            let target_id = ledger
                .legacy_ids
                .get(&new_id)
                .cloned()
                .unwrap_or_else(|| allocate_entity_id("legacy", "", "legacy", &new_id));
            ledger.legacy_ids.insert(old_id.clone(), source_id.clone());
            let operation_id = stable_id("op_", &["legacy_merge".into(), old_id.clone(), new_id]);
            ordered_operations.push((
                old_id,
                1_u8,
                KnowledgeOperation {
                    id: operation_id,
                    at: "legacy_bootstrap".into(),
                    before: serde_json::Value::Null,
                    after: serde_json::Value::String(target_id.clone()),
                    action: KnowledgeAction::MergeEntity {
                        source_id,
                        target_id,
                    },
                },
            ));
        }
    }
    ordered_operations
        .sort_by(|left, right| (left.0.as_str(), left.1).cmp(&(right.0.as_str(), right.1)));
    ledger.operations = ordered_operations
        .into_iter()
        .map(|(_, _, operation)| operation)
        .collect();
    Ok(ledger)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn entity(kind: &str, name: &str) -> RegistryEntity {
        RegistryEntity {
            kind: kind.into(),
            name: name.into(),
            aliases: Vec::new(),
            status: "confirmed".into(),
        }
    }

    fn operation(id: &str, action: KnowledgeAction) -> KnowledgeOperation {
        KnowledgeOperation {
            id: id.into(),
            at: "2026-07-21T00:00:00Z".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action,
        }
    }

    #[test]
    fn missing_ledger_initializes_to_exact_v1_bytes() {
        let root = tempfile::tempdir().unwrap();
        let ledger = load(root.path()).unwrap();

        assert_eq!(ledger, KnowledgeLedger::empty());
        assert_eq!(
            fs::read(root.path().join(KNOWLEDGE_FILE)).unwrap(),
            br#"{"schema_version":1,"registry":{},"legacy_ids":{},"operations":[]}"#
        );
    }

    #[test]
    fn entity_ids_are_deterministic_and_seed_sensitive() {
        let first = allocate_entity_id("project", "Apollo", "note-1", "ent_1");
        assert_eq!(
            first,
            allocate_entity_id("project", "Apollo", "note-1", "ent_1")
        );
        assert!(first.starts_with("kg_"));
        assert_ne!(
            first,
            allocate_entity_id("project", "Apollo", "note-2", "ent_1")
        );
        assert_ne!(
            first,
            allocate_entity_id("project", "Apollo", "note-1", "ent_2")
        );
        assert_eq!(
            allocate_split_entity_id("op_123"),
            allocate_split_entity_id("op_123")
        );
    }

    #[test]
    fn update_preserves_entity_id_across_rename_and_appends_operation() {
        let root = tempfile::tempdir().unwrap();
        let id = allocate_entity_id("project", "Apollo", "n1", "ent_1");
        update(root.path(), |ledger| {
            ledger
                .registry
                .insert(id.clone(), entity("project", "Apollo"));
            ledger.operations.push(operation(
                "op_create",
                KnowledgeAction::CreateEntity {
                    entity: entity("project", "Apollo"),
                },
            ));
            Ok(())
        })
        .unwrap();
        update(root.path(), |ledger| {
            ledger.registry.get_mut(&id).unwrap().name = "Artemis".into();
            ledger.operations.push(operation(
                "op_rename",
                KnowledgeAction::RenameEntity {
                    entity_id: id.clone(),
                    name: "Artemis".into(),
                },
            ));
            Ok(())
        })
        .unwrap();

        let ledger = load(root.path()).unwrap();
        assert_eq!(ledger.registry.get(&id).unwrap().name, "Artemis");
        assert_eq!(ledger.operations.len(), 2);
        assert_eq!(ledger.operations[0].id, "op_create");
        assert_eq!(ledger.operations[1].id, "op_rename");
    }

    #[test]
    fn update_rejects_mutating_or_removing_existing_operations() {
        let root = tempfile::tempdir().unwrap();
        update(root.path(), |ledger| {
            ledger.operations.push(operation(
                "op_1",
                KnowledgeAction::AddAlias {
                    entity_id: "kg_a".into(),
                    alias: "A".into(),
                },
            ));
            Ok(())
        })
        .unwrap();
        let before = fs::read(root.path().join(KNOWLEDGE_FILE)).unwrap();

        let error = update(root.path(), |ledger| {
            ledger.operations[0].id = "changed".into();
            Ok(())
        })
        .unwrap_err();

        assert!(error.to_string().contains("append-only"));
        assert_eq!(fs::read(root.path().join(KNOWLEDGE_FILE)).unwrap(), before);
    }

    #[test]
    fn legacy_mapping_round_trips() {
        let root = tempfile::tempdir().unwrap();
        update(root.path(), |ledger| {
            ledger.legacy_ids.insert("e:apollo".into(), "kg_123".into());
            Ok(())
        })
        .unwrap();
        assert_eq!(load(root.path()).unwrap().legacy_ids["e:apollo"], "kg_123");
    }

    #[test]
    fn action_kind_serializes_as_stable_snake_case() {
        let row = operation(
            "op_1",
            KnowledgeAction::BindMention {
                mention_id: "mn_1".into(),
                entity_id: "kg_1".into(),
            },
        );
        let value = serde_json::to_value(row).unwrap();
        assert_eq!(value["kind"], "bind_mention");
        assert_eq!(value["payload"]["mention_id"], "mn_1");
        assert_eq!(value["before"], serde_json::Value::Null);
    }

    #[test]
    fn create_relation_requires_evidence_or_explicit_user_assertion() {
        let root = tempfile::tempdir().unwrap();
        let invalid = UserRelation {
            subject_id: "kg_a".into(),
            predicate: crate::store::RelationPredicate {
                kind: "uses".into(),
                label: None,
            },
            object_id: "kg_b".into(),
            valid_from: None,
            valid_to: None,
            note: None,
            evidence_ids: Vec::new(),
            user_assertion: false,
        };

        let error = update(root.path(), |ledger| {
            ledger.operations.push(operation(
                "op_relation",
                KnowledgeAction::CreateRelation { relation: invalid },
            ));
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("evidence"));
    }

    #[test]
    fn corrupt_json_is_visible_and_never_replaced() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(KNOWLEDGE_FILE);
        fs::write(&path, b"{not json").unwrap();

        assert!(matches!(
            load(root.path()),
            Err(KnowledgeLoadError::Corrupt { .. })
        ));
        assert!(update(root.path(), |_| Ok(())).is_err());
        assert_eq!(fs::read(path).unwrap(), b"{not json");
    }

    #[test]
    fn held_file_lock_rejects_second_writer_after_bounded_retries() {
        let root = tempfile::tempdir().unwrap();
        let _held = KnowledgeLock::acquire(root.path()).unwrap();
        let started = std::time::Instant::now();

        let error = update(root.path(), |_| Ok(())).unwrap_err();

        assert!(error.to_string().contains("lock"));
        assert!(started.elapsed() >= std::time::Duration::from_millis(60));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn concurrent_updates_are_serialized_without_lost_operations() {
        let root = tempfile::tempdir().unwrap();
        load(root.path()).unwrap();
        let root = std::sync::Arc::new(root);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let handles: Vec<_> = ["op_a", "op_b"]
            .into_iter()
            .map(|id| {
                let root = root.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    update(root.path(), |ledger| {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        ledger.operations.push(operation(
                            id,
                            KnowledgeAction::AddAlias {
                                entity_id: "kg_1".into(),
                                alias: id.into(),
                            },
                        ));
                        Ok(())
                    })
                })
            })
            .collect();
        barrier.wait();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        let ledger = load(root.path()).unwrap();
        let ids: std::collections::BTreeSet<_> = ledger
            .operations
            .iter()
            .map(|operation| operation.id.as_str())
            .collect();
        assert_eq!(ids, std::collections::BTreeSet::from(["op_a", "op_b"]));
    }

    #[test]
    fn failure_before_atomic_rename_leaves_previous_live_bytes_unchanged() {
        let root = tempfile::tempdir().unwrap();
        load(root.path()).unwrap();
        let live = root.path().join(KNOWLEDGE_FILE);
        let before = fs::read(&live).unwrap();
        let mut ledger = KnowledgeLedger::empty();
        ledger.legacy_ids.insert("old".into(), "kg_new".into());

        let error = write_ledger_atomic_with(root.path(), &ledger, || {
            anyhow::bail!("injected failure before rename")
        })
        .unwrap_err();

        assert!(error.to_string().contains("injected failure"));
        assert_eq!(fs::read(live).unwrap(), before);
    }

    #[test]
    fn successful_replace_keeps_the_previous_readable_bytes_as_backup() {
        let root = tempfile::tempdir().unwrap();
        load(root.path()).unwrap();
        update(root.path(), |ledger| {
            ledger.legacy_ids.insert("old".into(), "kg_1".into());
            Ok(())
        })
        .unwrap();
        let previous = fs::read(root.path().join(KNOWLEDGE_FILE)).unwrap();

        update(root.path(), |ledger| {
            ledger.legacy_ids.insert("older".into(), "kg_2".into());
            Ok(())
        })
        .unwrap();

        assert_eq!(
            fs::read(root.path().join(KNOWLEDGE_BACKUP_FILE)).unwrap(),
            previous
        );
    }

    #[test]
    fn bootstrap_imports_legacy_rows_in_deterministic_order_once() {
        fn seed(root: &std::path::Path) {
            let conn = super::super::open(root).unwrap();
            conn.execute(
                "INSERT INTO entities(id, kind, name, aliases, is_person) VALUES(?1,?2,?3,?4,0)",
                rusqlite::params!["e:zeta", "project", "Zeta", r#"["Z"]"#],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO entities(id, kind, name, aliases, is_person) VALUES(?1,?2,?3,?4,0)",
                rusqlite::params!["e:alpha", "project", "Alpha", "[]"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO entity_name_overrides(id, name) VALUES('e:zeta', 'Zeta Prime')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO entity_redirects(old_id, new_id) VALUES('e:old-zeta', 'e:zeta')",
                [],
            )
            .unwrap();
        }

        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        seed(a.path());
        seed(b.path());
        let first = load(a.path()).unwrap();
        let second = load(b.path()).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.registry.len(), 2);
        assert_eq!(first.operations.len(), 3);
        assert_eq!(
            first.registry[first.legacy_ids["e:zeta"].as_str()].name,
            "Zeta Prime"
        );
        assert!(matches!(
            first.operations[0].action,
            KnowledgeAction::CreateEntity { .. }
        ));
        assert!(matches!(
            first.operations[1].action,
            KnowledgeAction::MergeEntity { .. }
        ));
        assert!(matches!(
            first.operations[2].action,
            KnowledgeAction::CreateEntity { .. }
        ));
        assert_eq!(
            fs::read(a.path().join(KNOWLEDGE_FILE)).unwrap(),
            fs::read(b.path().join(KNOWLEDGE_FILE)).unwrap()
        );

        let bytes = fs::read(a.path().join(KNOWLEDGE_FILE)).unwrap();
        assert_eq!(load(a.path()).unwrap(), first);
        assert_eq!(fs::read(a.path().join(KNOWLEDGE_FILE)).unwrap(), bytes);
    }

    #[test]
    fn valid_json_shape_is_not_silently_reinterpreted() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join(KNOWLEDGE_FILE),
            serde_json::to_vec(
                &json!({"schema_version": 99, "registry": {}, "legacy_ids": {}, "operations": []}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            load(root.path()),
            Err(KnowledgeLoadError::Corrupt { .. })
        ));
    }
}
