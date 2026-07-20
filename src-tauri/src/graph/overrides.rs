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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RegistryState {
    pub entity_id: String,
    pub entity: Option<RegistryEntity>,
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

#[derive(Debug)]
struct ValidatedLedger {
    ledger: KnowledgeLedger,
    baseline: BTreeMap<String, RegistryEntity>,
}

struct ValidatedBootstrap(ValidatedLedger);

impl ValidatedBootstrap {
    fn new(ledger: KnowledgeLedger) -> Result<Self, String> {
        let baseline = validate_ledger(&ledger)?;
        Ok(Self(ValidatedLedger { ledger, baseline }))
    }
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
    load_with_bootstrap_reader(data_root, bootstrap_legacy)
}

fn load_with_bootstrap_reader(
    data_root: &Path,
    read_bootstrap: impl FnOnce(&Path) -> anyhow::Result<KnowledgeLedger>,
) -> Result<KnowledgeLedger, KnowledgeLoadError> {
    let path = data_root.join(KNOWLEDGE_FILE);
    match read_ledger(&path) {
        Ok(Some(validated)) => Ok(validated.ledger),
        Ok(None) => {
            let bootstrap =
                read_bootstrap(data_root).map_err(|error| KnowledgeLoadError::Corrupt {
                    path: data_root.join(super::GRAPH_FILE),
                    message: error.to_string(),
                })?;
            let bootstrap = ValidatedBootstrap::new(bootstrap).map_err(|message| {
                KnowledgeLoadError::Corrupt {
                    path: data_root.join(super::GRAPH_FILE),
                    message,
                }
            })?;
            initialize_missing(data_root, bootstrap)
        }
        Err(error) => Err(error),
    }
}

pub fn update<T>(
    data_root: &Path,
    change: impl FnOnce(&mut KnowledgeLedger) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    fs::create_dir_all(data_root)?;
    let path = data_root.join(KNOWLEDGE_FILE);
    let bootstrap = match read_ledger(&path)? {
        Some(_) => None,
        None => Some(
            ValidatedBootstrap::new(bootstrap_legacy(data_root)?).map_err(anyhow::Error::msg)?,
        ),
    };
    let _lock = KnowledgeLock::acquire(data_root)?;
    let loaded = load_locked(data_root, bootstrap)?;
    let mut ledger = loaded.ledger;
    let original_baseline = loaded.baseline;
    let original_legacy_ids = ledger.legacy_ids.clone();
    let existing_operations = ledger.operations.clone();
    let result = change(&mut ledger)?;
    anyhow::ensure!(
        ledger.operations.starts_with(&existing_operations),
        "knowledge operations are append-only"
    );
    canonicalize_new_content(&mut ledger, existing_operations.len())?;
    ensure_legacy_ids_extend(&original_legacy_ids, &ledger.legacy_ids)?;
    let updated_baseline = validate_ledger(&ledger).map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        updated_baseline == original_baseline,
        "materialized registry mutation is not explained by appended operations"
    );
    write_ledger_atomic(data_root, &ledger)?;
    Ok(result)
}

fn initialize_missing(
    data_root: &Path,
    bootstrap: ValidatedBootstrap,
) -> Result<KnowledgeLedger, KnowledgeLoadError> {
    fs::create_dir_all(data_root).map_err(|source| KnowledgeLoadError::Io {
        path: data_root.into(),
        source,
    })?;
    let _lock = KnowledgeLock::acquire(data_root).map_err(|source| KnowledgeLoadError::Io {
        path: data_root.join(KNOWLEDGE_LOCK_FILE),
        source,
    })?;
    load_locked(data_root, Some(bootstrap)).map(|validated| validated.ledger)
}

fn load_locked(
    data_root: &Path,
    bootstrap: Option<ValidatedBootstrap>,
) -> Result<ValidatedLedger, KnowledgeLoadError> {
    let path = data_root.join(KNOWLEDGE_FILE);
    if let Some(ledger) = read_ledger(&path)? {
        return Ok(ledger);
    }
    let validated = bootstrap
        .ok_or_else(|| KnowledgeLoadError::Io {
            path: path.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "knowledge ledger disappeared while acquiring its lock",
            ),
        })?
        .0;
    let write_result = if validated.ledger == KnowledgeLedger::empty() {
        write_bytes_atomic_with(data_root, INITIAL_BYTES, || Ok(()))
    } else {
        write_ledger_atomic(data_root, &validated.ledger)
    };
    write_result.map_err(|error| KnowledgeLoadError::Io {
        path,
        source: std::io::Error::other(error),
    })?;
    Ok(validated)
}

fn read_ledger(path: &Path) -> Result<Option<ValidatedLedger>, KnowledgeLoadError> {
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
    let baseline = validate_ledger(&ledger).map_err(|message| KnowledgeLoadError::Corrupt {
        path: path.into(),
        message,
    })?;
    Ok(Some(ValidatedLedger { ledger, baseline }))
}

fn validate_ledger(ledger: &KnowledgeLedger) -> Result<BTreeMap<String, RegistryEntity>, String> {
    if ledger.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported schema_version {}",
            ledger.schema_version
        ));
    }
    for (entity_id, entity) in &ledger.registry {
        if !is_canonical_set(&entity.aliases) {
            return Err(format!(
                "registry entity {entity_id} aliases are not canonical"
            ));
        }
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
            if !is_canonical_set(&relation.evidence_ids) {
                return Err(format!(
                    "operation {} evidence_ids are not canonical",
                    operation.id
                ));
            }
        }
        if let KnowledgeAction::CreateEntity { entity } = &operation.action {
            if !is_canonical_set(&entity.aliases) {
                return Err(format!(
                    "operation {} aliases are not canonical",
                    operation.id
                ));
            }
        }
    }
    super::resolve::validate_decision_history(&ledger.operations)
        .map_err(|error| error.to_string())?;
    validate_legacy_targets(ledger).map_err(|error| error.to_string())?;
    registry_baseline(ledger).map_err(|error| error.to_string())
}

fn ensure_legacy_ids_extend(
    original: &BTreeMap<String, String>,
    updated: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    for (legacy_id, target_id) in original {
        anyhow::ensure!(
            updated.get(legacy_id) == Some(target_id),
            "legacy_ids entry {legacy_id} is append-only and cannot be deleted or retargeted"
        );
    }
    Ok(())
}

fn validate_legacy_targets(ledger: &KnowledgeLedger) -> anyhow::Result<()> {
    let active = active_operation_mask(&ledger.operations)?;
    let mut redirects = BTreeMap::new();
    for (index, operation) in ledger.operations.iter().enumerate() {
        if !active[index] {
            continue;
        }
        if let KnowledgeAction::MergeEntity {
            source_id,
            target_id,
        } = &operation.action
        {
            redirects.insert(source_id.clone(), target_id.clone());
        }
    }
    'mapping: for (legacy_id, target_id) in &ledger.legacy_ids {
        let mut current = target_id.as_str();
        let mut visited = std::collections::BTreeSet::new();
        while let Some(next) = redirects.get(current) {
            if !visited.insert(current.to_string()) {
                continue 'mapping;
            }
            current = next;
        }
        anyhow::ensure!(
            ledger.registry.contains_key(current),
            "legacy_ids entry {legacy_id} has invalid target {target_id}"
        );
    }
    Ok(())
}

fn canonicalize_new_content(
    ledger: &mut KnowledgeLedger,
    existing_operation_count: usize,
) -> anyhow::Result<()> {
    for entity in ledger.registry.values_mut() {
        canonicalize_set(&mut entity.aliases);
    }
    for operation in ledger.operations.iter_mut().skip(existing_operation_count) {
        match &mut operation.action {
            KnowledgeAction::RenameEntity { .. }
            | KnowledgeAction::AddAlias { .. }
            | KnowledgeAction::RemoveAlias { .. }
            | KnowledgeAction::CreateEntity { .. } => {
                canonicalize_registry_state(&mut operation.before, &operation.id, "before")?;
                canonicalize_registry_state(&mut operation.after, &operation.id, "after")?;
                if let KnowledgeAction::CreateEntity { entity } = &mut operation.action {
                    canonicalize_set(&mut entity.aliases);
                }
            }
            KnowledgeAction::CreateRelation { relation } => {
                canonicalize_set(&mut relation.evidence_ids)
            }
            _ => {}
        }
    }
    Ok(())
}

fn canonicalize_registry_state(
    value: &mut serde_json::Value,
    operation_id: &str,
    field: &str,
) -> anyhow::Result<()> {
    let mut state: RegistryState = serde_json::from_value(value.clone()).map_err(|error| {
        anyhow::anyhow!("operation {operation_id} {field} snapshot is invalid: {error}")
    })?;
    if let Some(entity) = &mut state.entity {
        canonicalize_set(&mut entity.aliases);
    }
    *value = serde_json::to_value(state)?;
    Ok(())
}

fn canonicalize_set(values: &mut Vec<String>) {
    values.sort();
    values.dedup();
}

fn is_canonical_set(values: &[String]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

pub(crate) fn active_operation_mask(
    operations: &[KnowledgeOperation],
) -> anyhow::Result<Vec<bool>> {
    let indexes: BTreeMap<&str, usize> = operations
        .iter()
        .enumerate()
        .map(|(index, operation)| (operation.id.as_str(), index))
        .collect();
    anyhow::ensure!(
        indexes.len() == operations.len(),
        "duplicate knowledge operation id"
    );
    let mut active = vec![true; operations.len()];
    for index in (0..operations.len()).rev() {
        if !active[index] {
            continue;
        }
        if let KnowledgeAction::Undo { operation_id } = &operations[index].action {
            let target = indexes.get(operation_id.as_str()).copied().ok_or_else(|| {
                anyhow::anyhow!("undo references unknown operation {operation_id}")
            })?;
            anyhow::ensure!(
                target < index,
                "undo operation {} must reference an earlier operation",
                operations[index].id
            );
            active[target] = !active[target];
        }
    }
    Ok(active)
}

pub(crate) fn registry_baseline(
    ledger: &KnowledgeLedger,
) -> anyhow::Result<BTreeMap<String, RegistryEntity>> {
    let transitions: Vec<_> = ledger
        .operations
        .iter()
        .map(validate_entity_transition)
        .collect::<anyhow::Result<_>>()?;
    let mut baseline = ledger.registry.clone();
    let mut initialized = std::collections::BTreeSet::new();
    for transition in transitions.iter().flatten() {
        let (before, _) = transition;
        if !initialized.insert(before.entity_id.clone()) {
            continue;
        }
        set_registry_state(&mut baseline, before);
    }

    let mut historical = baseline.clone();
    for (index, transition) in transitions.iter().enumerate() {
        if let Some((before, after)) = transition {
            let current = historical.get(&before.entity_id).cloned();
            anyhow::ensure!(
                current == before.entity,
                "operation {} before snapshot does not match historical registry",
                ledger.operations[index].id
            );
            apply_entity_action(
                &mut historical,
                &ledger.operations[index],
                before.entity_id.as_str(),
            )?;
            anyhow::ensure!(
                historical.get(&after.entity_id).cloned() == after.entity,
                "operation {} after snapshot does not match historical action",
                ledger.operations[index].id
            );
        } else if matches!(
            ledger.operations[index].action,
            KnowledgeAction::Undo { .. }
        ) {
            let operations = &ledger.operations[..=index];
            let active = active_operation_mask(operations)?;
            historical = project_registry(&baseline, operations, &transitions[..=index], &active)?;
        }
    }

    let active = active_operation_mask(&ledger.operations)?;
    let replayed = project_registry(&baseline, &ledger.operations, &transitions, &active)?;
    anyhow::ensure!(
        replayed == ledger.registry,
        "materialized registry does not match active operation replay"
    );
    Ok(baseline)
}

fn project_registry(
    baseline: &BTreeMap<String, RegistryEntity>,
    operations: &[KnowledgeOperation],
    transitions: &[Option<(RegistryState, RegistryState)>],
    active: &[bool],
) -> anyhow::Result<BTreeMap<String, RegistryEntity>> {
    let mut registry = baseline.clone();
    for (index, transition) in transitions.iter().enumerate() {
        if !active[index] {
            continue;
        }
        let Some((before, _)) = transition else {
            continue;
        };
        apply_entity_action(&mut registry, &operations[index], &before.entity_id)?;
    }
    Ok(registry)
}

fn apply_entity_action(
    registry: &mut BTreeMap<String, RegistryEntity>,
    operation: &KnowledgeOperation,
    entity_id: &str,
) -> anyhow::Result<()> {
    match &operation.action {
        KnowledgeAction::RenameEntity { name, .. } => {
            registry
                .get_mut(entity_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "operation {} renames missing entity {entity_id}",
                        operation.id
                    )
                })?
                .name = name.clone();
        }
        KnowledgeAction::AddAlias { alias, .. } => {
            let entity = registry.get_mut(entity_id).ok_or_else(|| {
                anyhow::anyhow!(
                    "operation {} adds alias to missing entity {entity_id}",
                    operation.id
                )
            })?;
            entity.aliases.push(alias.clone());
            canonicalize_set(&mut entity.aliases);
        }
        KnowledgeAction::RemoveAlias { alias, .. } => {
            let entity = registry.get_mut(entity_id).ok_or_else(|| {
                anyhow::anyhow!(
                    "operation {} removes alias from missing entity {entity_id}",
                    operation.id
                )
            })?;
            entity.aliases.retain(|value| value != alias);
        }
        KnowledgeAction::CreateEntity { entity } => {
            anyhow::ensure!(
                !registry.contains_key(entity_id),
                "operation {} creates existing entity {entity_id}",
                operation.id
            );
            registry.insert(entity_id.to_string(), entity.clone());
        }
        _ => unreachable!("only entity operations have registry transitions"),
    }
    Ok(())
}

fn set_registry_state(registry: &mut BTreeMap<String, RegistryEntity>, state: &RegistryState) {
    match &state.entity {
        Some(entity) => {
            registry.insert(state.entity_id.clone(), entity.clone());
        }
        None => {
            registry.remove(&state.entity_id);
        }
    }
}

fn validate_entity_transition(
    operation: &KnowledgeOperation,
) -> anyhow::Result<Option<(RegistryState, RegistryState)>> {
    let entity_action = matches!(
        operation.action,
        KnowledgeAction::RenameEntity { .. }
            | KnowledgeAction::AddAlias { .. }
            | KnowledgeAction::RemoveAlias { .. }
            | KnowledgeAction::CreateEntity { .. }
    );
    if !entity_action {
        return Ok(None);
    }
    let before: RegistryState =
        serde_json::from_value(operation.before.clone()).map_err(|error| {
            anyhow::anyhow!(
                "operation {} before snapshot is invalid: {error}",
                operation.id
            )
        })?;
    let after: RegistryState =
        serde_json::from_value(operation.after.clone()).map_err(|error| {
            anyhow::anyhow!(
                "operation {} after snapshot is invalid: {error}",
                operation.id
            )
        })?;
    anyhow::ensure!(
        before.entity_id == after.entity_id,
        "operation {} changes entity id in snapshots",
        operation.id
    );
    for (field, state) in [("before", &before), ("after", &after)] {
        if let Some(entity) = &state.entity {
            anyhow::ensure!(
                is_canonical_set(&entity.aliases),
                "operation {} {field} aliases are not canonical",
                operation.id
            );
        }
    }
    match &operation.action {
        KnowledgeAction::RenameEntity { entity_id, name } => {
            anyhow::ensure!(
                before.entity_id == *entity_id,
                "operation {} entity id mismatch",
                operation.id
            );
            let mut expected = before.entity.clone().ok_or_else(|| {
                anyhow::anyhow!("operation {} rename before entity is missing", operation.id)
            })?;
            expected.name = name.clone();
            anyhow::ensure!(
                after.entity == Some(expected),
                "operation {} rename after snapshot mismatch",
                operation.id
            );
        }
        KnowledgeAction::AddAlias { entity_id, alias } => {
            anyhow::ensure!(
                before.entity_id == *entity_id,
                "operation {} entity id mismatch",
                operation.id
            );
            let mut expected = before.entity.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "operation {} add-alias before entity is missing",
                    operation.id
                )
            })?;
            expected.aliases.push(alias.clone());
            canonicalize_set(&mut expected.aliases);
            anyhow::ensure!(
                after.entity == Some(expected),
                "operation {} add-alias after snapshot mismatch",
                operation.id
            );
        }
        KnowledgeAction::RemoveAlias { entity_id, alias } => {
            anyhow::ensure!(
                before.entity_id == *entity_id,
                "operation {} entity id mismatch",
                operation.id
            );
            let mut expected = before.entity.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "operation {} remove-alias before entity is missing",
                    operation.id
                )
            })?;
            expected.aliases.retain(|value| value != alias);
            anyhow::ensure!(
                after.entity == Some(expected),
                "operation {} remove-alias after snapshot mismatch",
                operation.id
            );
        }
        KnowledgeAction::CreateEntity { entity } => {
            anyhow::ensure!(
                before.entity.is_none(),
                "operation {} create before entity must be null",
                operation.id
            );
            anyhow::ensure!(
                after.entity.as_ref() == Some(entity),
                "operation {} create after snapshot mismatch",
                operation.id
            );
        }
        _ => unreachable!(),
    }
    Ok(Some((before, after)))
}

struct KnowledgeLock {
    _file: File,
}

impl KnowledgeLock {
    fn acquire(data_root: &Path) -> std::io::Result<Self> {
        Self::acquire_with_observer(data_root, || {})
    }

    fn acquire_with_observer(
        data_root: &Path,
        mut on_attempt: impl FnMut(),
    ) -> std::io::Result<Self> {
        const RETRIES: u32 = 5;
        const BACKOFF: std::time::Duration = std::time::Duration::from_millis(20);
        for attempt in 0..=RETRIES {
            on_attempt();
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .open(data_root.join(KNOWLEDGE_LOCK_FILE))?;
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::WouldBlock) if attempt < RETRIES => {
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
    bootstrap_legacy_with_hook(data_root, || {})
}

fn bootstrap_legacy_with_hook(
    data_root: &Path,
    after_overrides_snapshot: impl FnOnce(),
) -> anyhow::Result<KnowledgeLedger> {
    let database = data_root.join(super::GRAPH_FILE);
    if !database.exists() {
        return Ok(KnowledgeLedger::empty());
    }
    let connection = rusqlite::Connection::open_with_flags(
        database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let snapshot = connection.unchecked_transaction()?;
    if !legacy_table_exists(&snapshot, "entities")? {
        snapshot.commit()?;
        return Ok(KnowledgeLedger::empty());
    }

    let mut overrides = BTreeMap::new();
    if legacy_table_exists(&snapshot, "entity_name_overrides")? {
        let mut statement =
            snapshot.prepare("SELECT id, name FROM entity_name_overrides ORDER BY id")?;
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (id, name) = row?;
            overrides.insert(id, name);
        }
    }
    after_overrides_snapshot();

    let mut ledger = KnowledgeLedger::empty();
    let mut ordered_operations = Vec::new();
    {
        let mut statement =
            snapshot.prepare("SELECT id, kind, name, aliases FROM entities ORDER BY id")?;
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
            let mut aliases: Vec<String> =
                serde_json::from_str(&aliases_json).map_err(|error| {
                    anyhow::anyhow!("legacy entity {old_id} aliases JSON is invalid: {error}")
                })?;
            canonicalize_set(&mut aliases);
            let entity = RegistryEntity {
                kind: kind.clone(),
                name: name.clone(),
                aliases,
                status: "confirmed".into(),
            };
            let entity_id = if kind == "person" {
                old_id.clone()
            } else {
                allocate_entity_id(&kind, &name, "legacy", &old_id)
            };
            ledger.registry.insert(entity_id.clone(), entity.clone());
            ledger.legacy_ids.insert(old_id.clone(), entity_id.clone());
            let operation_id = stable_id("op_", &["legacy_create".into(), old_id.clone()]);
            ordered_operations.push((
                old_id,
                0_u8,
                KnowledgeOperation {
                    id: operation_id,
                    at: "legacy_bootstrap".into(),
                    before: serde_json::to_value(RegistryState {
                        entity_id: entity_id.clone(),
                        entity: None,
                    })?,
                    after: serde_json::to_value(RegistryState {
                        entity_id,
                        entity: Some(entity.clone()),
                    })?,
                    action: KnowledgeAction::CreateEntity { entity },
                },
            ));
        }
    }

    if legacy_table_exists(&snapshot, "entity_redirects")? {
        let mut statement =
            snapshot.prepare("SELECT old_id, new_id FROM entity_redirects ORDER BY old_id")?;
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
    snapshot.commit()?;
    Ok(ledger)
}

fn legacy_table_exists(connection: &rusqlite::Connection, name: &str) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |row| row.get(0),
    )
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

    fn entity_state(entity_id: &str, entity: Option<&RegistryEntity>) -> serde_json::Value {
        serde_json::to_value(RegistryState {
            entity_id: entity_id.into(),
            entity: entity.cloned(),
        })
        .unwrap()
    }

    fn entity_operation(
        id: &str,
        entity_id: &str,
        before: Option<&RegistryEntity>,
        after: Option<&RegistryEntity>,
        action: KnowledgeAction,
    ) -> KnowledgeOperation {
        KnowledgeOperation {
            id: id.into(),
            at: "2026-07-21T00:00:00Z".into(),
            before: entity_state(entity_id, before),
            after: entity_state(entity_id, after),
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
    fn legacy_snapshot_reader_runs_before_initialization_lock_is_acquired() {
        let root = tempfile::tempdir().unwrap();
        let reader_ran = std::cell::Cell::new(false);

        let ledger = load_with_bootstrap_reader(root.path(), |data_root| {
            let probe = KnowledgeLock::acquire(data_root)
                .expect("snapshot reader must run before ledger lock is held");
            reader_ran.set(true);
            drop(probe);
            Ok(KnowledgeLedger::empty())
        })
        .unwrap();

        assert!(reader_ran.get());
        assert_eq!(ledger, KnowledgeLedger::empty());
    }

    #[test]
    fn locked_recheck_does_not_replace_a_disappeared_existing_ledger() {
        let root = tempfile::tempdir().unwrap();

        let error = load_locked(root.path(), None).unwrap_err();

        assert!(matches!(error, KnowledgeLoadError::Io { .. }));
        assert!(!root.path().join(KNOWLEDGE_FILE).exists());
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
        let original = entity("project", "Apollo");
        let renamed = entity("project", "Artemis");
        update(root.path(), |ledger| {
            ledger.registry.insert(id.clone(), original.clone());
            ledger.operations.push(entity_operation(
                "op_create",
                &id,
                None,
                Some(&original),
                KnowledgeAction::CreateEntity {
                    entity: original.clone(),
                },
            ));
            Ok(())
        })
        .unwrap();
        update(root.path(), |ledger| {
            ledger.registry.get_mut(&id).unwrap().name = "Artemis".into();
            ledger.operations.push(entity_operation(
                "op_rename",
                &id,
                Some(&original),
                Some(&renamed),
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
                KnowledgeAction::ConfirmRelation {
                    relation_id: "rf_1".into(),
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
    fn update_rejects_materialized_registry_mutation_without_matching_operation() {
        let root = tempfile::tempdir().unwrap();
        load(root.path()).unwrap();
        let before = fs::read(root.path().join(KNOWLEDGE_FILE)).unwrap();

        let error = update(root.path(), |ledger| {
            ledger
                .registry
                .insert("kg_unlogged".into(), entity("project", "Unlogged"));
            Ok(())
        })
        .unwrap_err();

        assert!(error.to_string().contains("materialized registry"));
        assert_eq!(fs::read(root.path().join(KNOWLEDGE_FILE)).unwrap(), before);
    }

    #[test]
    fn update_requires_undo_to_restore_the_materialized_registry() {
        let root = tempfile::tempdir().unwrap();
        let id = "kg_1";
        let original = entity("project", "Apollo");
        let renamed = entity("project", "Artemis");
        update(root.path(), |ledger| {
            ledger.registry.insert(id.into(), original.clone());
            ledger.operations.push(entity_operation(
                "op_create",
                id,
                None,
                Some(&original),
                KnowledgeAction::CreateEntity {
                    entity: original.clone(),
                },
            ));
            ledger.registry.insert(id.into(), renamed.clone());
            ledger.operations.push(entity_operation(
                "op_rename",
                id,
                Some(&original),
                Some(&renamed),
                KnowledgeAction::RenameEntity {
                    entity_id: id.into(),
                    name: renamed.name.clone(),
                },
            ));
            Ok(())
        })
        .unwrap();
        let before = fs::read(root.path().join(KNOWLEDGE_FILE)).unwrap();

        let error = update(root.path(), |ledger| {
            ledger.operations.push(operation(
                "op_undo",
                KnowledgeAction::Undo {
                    operation_id: "op_rename".into(),
                },
            ));
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("materialized registry"));
        assert_eq!(fs::read(root.path().join(KNOWLEDGE_FILE)).unwrap(), before);

        update(root.path(), |ledger| {
            ledger.registry.insert(id.into(), original.clone());
            ledger.operations.push(operation(
                "op_undo",
                KnowledgeAction::Undo {
                    operation_id: "op_rename".into(),
                },
            ));
            Ok(())
        })
        .unwrap();
        assert_eq!(load(root.path()).unwrap().registry[id], original);
    }

    #[test]
    fn legacy_mapping_round_trips() {
        let root = tempfile::tempdir().unwrap();
        update(root.path(), |ledger| {
            let created = entity("project", "Apollo");
            ledger.registry.insert("kg_123".into(), created.clone());
            ledger.operations.push(entity_operation(
                "op_create",
                "kg_123",
                None,
                Some(&created),
                KnowledgeAction::CreateEntity {
                    entity: created.clone(),
                },
            ));
            ledger.legacy_ids.insert("e:apollo".into(), "kg_123".into());
            Ok(())
        })
        .unwrap();
        assert_eq!(load(root.path()).unwrap().legacy_ids["e:apollo"], "kg_123");
    }

    #[test]
    fn update_rejects_deleting_or_retargeting_existing_legacy_ids() {
        let root = tempfile::tempdir().unwrap();
        let first = entity("project", "Apollo");
        let second = entity("project", "Artemis");
        update(root.path(), |ledger| {
            for (id, value, operation_id) in [
                ("kg_1", &first, "op_create_1"),
                ("kg_2", &second, "op_create_2"),
            ] {
                ledger.registry.insert(id.into(), value.clone());
                ledger.operations.push(entity_operation(
                    operation_id,
                    id,
                    None,
                    Some(value),
                    KnowledgeAction::CreateEntity {
                        entity: value.clone(),
                    },
                ));
            }
            ledger.legacy_ids.insert("e:apollo".into(), "kg_1".into());
            Ok(())
        })
        .unwrap();
        let before = fs::read(root.path().join(KNOWLEDGE_FILE)).unwrap();

        let delete_error = update(root.path(), |ledger| {
            ledger.legacy_ids.remove("e:apollo");
            Ok(())
        })
        .unwrap_err();
        assert!(delete_error.to_string().contains("legacy_ids"));
        assert_eq!(fs::read(root.path().join(KNOWLEDGE_FILE)).unwrap(), before);

        let retarget_error = update(root.path(), |ledger| {
            ledger.legacy_ids.insert("e:apollo".into(), "kg_2".into());
            Ok(())
        })
        .unwrap_err();
        assert!(retarget_error.to_string().contains("legacy_ids"));
        assert_eq!(fs::read(root.path().join(KNOWLEDGE_FILE)).unwrap(), before);
    }

    #[test]
    fn update_rejects_a_new_legacy_id_with_no_valid_target() {
        let root = tempfile::tempdir().unwrap();
        load(root.path()).unwrap();
        let before = fs::read(root.path().join(KNOWLEDGE_FILE)).unwrap();

        let error = update(root.path(), |ledger| {
            ledger
                .legacy_ids
                .insert("e:missing".into(), "kg_missing".into());
            Ok(())
        })
        .unwrap_err();

        assert!(error.to_string().contains("legacy_ids"));
        assert!(error.to_string().contains("kg_missing"));
        assert_eq!(fs::read(root.path().join(KNOWLEDGE_FILE)).unwrap(), before);
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
    fn malformed_decision_snapshot_is_corrupt_on_load_and_bytes_stay_visible() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(KNOWLEDGE_FILE);
        let mut ledger = KnowledgeLedger::empty();
        ledger.operations.push(KnowledgeOperation {
            id: "op_merge".into(),
            at: "t".into(),
            before: serde_json::Value::Null,
            after: serde_json::Value::Null,
            action: KnowledgeAction::MergeEntity {
                source_id: "kg_a".into(),
                target_id: "kg_b".into(),
            },
        });
        let bytes = serde_json::to_vec_pretty(&ledger).unwrap();
        fs::write(&path, &bytes).unwrap();

        let error = load(root.path()).unwrap_err();

        assert!(matches!(error, KnowledgeLoadError::Corrupt { .. }));
        assert!(error.to_string().contains("op_merge"));
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn inconsistent_decision_history_is_rejected_during_load() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(KNOWLEDGE_FILE);
        let mut ledger = KnowledgeLedger::empty();
        ledger.operations.extend([
            KnowledgeOperation {
                id: "op_bind_a".into(),
                at: "t".into(),
                before: serde_json::Value::Null,
                after: serde_json::Value::String("kg_a".into()),
                action: KnowledgeAction::BindMention {
                    mention_id: "mn_1".into(),
                    entity_id: "kg_a".into(),
                },
            },
            KnowledgeOperation {
                id: "op_bind_b".into(),
                at: "t".into(),
                before: serde_json::Value::Null,
                after: serde_json::Value::String("kg_b".into()),
                action: KnowledgeAction::BindMention {
                    mention_id: "mn_1".into(),
                    entity_id: "kg_b".into(),
                },
            },
        ]);
        fs::write(&path, serde_json::to_vec(&ledger).unwrap()).unwrap();

        let error = load(root.path()).unwrap_err();

        assert!(matches!(error, KnowledgeLoadError::Corrupt { .. }));
        assert!(error.to_string().contains("op_bind_b"));
    }

    #[test]
    fn legal_non_lifo_history_is_accepted_by_load_and_replay() {
        let root = tempfile::tempdir().unwrap();
        let original = entity("project", "Apollo");
        let renamed = entity("project", "Artemis");
        let mut renamed_with_alias = renamed.clone();
        renamed_with_alias.aliases.push("Moonshot".into());
        let mut projected = original.clone();
        projected.aliases.push("Moonshot".into());
        let mut ledger = KnowledgeLedger::empty();
        ledger.registry.insert("kg_1".into(), projected.clone());
        ledger.operations.extend([
            entity_operation(
                "op_create",
                "kg_1",
                None,
                Some(&original),
                KnowledgeAction::CreateEntity {
                    entity: original.clone(),
                },
            ),
            entity_operation(
                "op_rename",
                "kg_1",
                Some(&original),
                Some(&renamed),
                KnowledgeAction::RenameEntity {
                    entity_id: "kg_1".into(),
                    name: "Artemis".into(),
                },
            ),
            entity_operation(
                "op_alias",
                "kg_1",
                Some(&renamed),
                Some(&renamed_with_alias),
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
        fs::write(
            root.path().join(KNOWLEDGE_FILE),
            serde_json::to_vec(&ledger).unwrap(),
        )
        .unwrap();

        let loaded = load(root.path()).unwrap();
        let snapshot = crate::graph::resolve::replay(&loaded).unwrap();

        assert_eq!(snapshot.registry["kg_1"], projected);
    }

    #[test]
    fn restore_relation_rejects_unknown_forward_and_wrong_type_targets_on_load() {
        let invalid_ledgers = [
            vec![operation(
                "op_restore_unknown",
                KnowledgeAction::RestoreRelation {
                    operation_id: "op_missing".into(),
                },
            )],
            vec![
                operation(
                    "op_restore_forward",
                    KnowledgeAction::RestoreRelation {
                        operation_id: "op_suppress".into(),
                    },
                ),
                operation(
                    "op_suppress",
                    KnowledgeAction::SuppressRelation {
                        subject_id: "kg_a".into(),
                        predicate: crate::store::RelationPredicate {
                            kind: "uses".into(),
                            label: None,
                        },
                        object_id: "kg_b".into(),
                    },
                ),
            ],
            vec![
                KnowledgeOperation {
                    id: "op_bind".into(),
                    at: "t".into(),
                    before: serde_json::Value::Null,
                    after: serde_json::Value::String("kg_a".into()),
                    action: KnowledgeAction::BindMention {
                        mention_id: "mn_1".into(),
                        entity_id: "kg_a".into(),
                    },
                },
                operation(
                    "op_restore_bind",
                    KnowledgeAction::RestoreRelation {
                        operation_id: "op_bind".into(),
                    },
                ),
            ],
        ];

        for operations in invalid_ledgers {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join(KNOWLEDGE_FILE);
            let mut ledger = KnowledgeLedger::empty();
            ledger.operations = operations;
            fs::write(&path, serde_json::to_vec(&ledger).unwrap()).unwrap();

            let error = load(root.path()).unwrap_err();

            assert!(matches!(error, KnowledgeLoadError::Corrupt { .. }));
            assert!(error.to_string().contains("restore"));
        }
    }

    #[test]
    fn held_file_lock_rejects_second_writer_after_bounded_retries() {
        let root = tempfile::tempdir().unwrap();
        let _held = KnowledgeLock::acquire(root.path()).unwrap();
        let started = std::time::Instant::now();

        let error = update(root.path(), |_| Ok(())).unwrap_err();

        assert!(error.to_string().contains("lock"));
        assert!(started.elapsed() >= std::time::Duration::from_millis(90));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn lock_uses_one_initial_attempt_plus_five_retries() {
        let root = tempfile::tempdir().unwrap();
        let _held = KnowledgeLock::acquire(root.path()).unwrap();
        let attempts = std::cell::Cell::new(0_u32);

        let error = KnowledgeLock::acquire_with_observer(root.path(), || {
            attempts.set(attempts.get() + 1);
        })
        .err()
        .expect("held lock must reject acquisition");

        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(attempts.get(), 6);
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
                            KnowledgeAction::ConfirmRelation {
                                relation_id: id.into(),
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
            let created = entity("project", "Apollo");
            ledger.registry.insert("kg_1".into(), created.clone());
            ledger.operations.push(entity_operation(
                "op_create",
                "kg_1",
                None,
                Some(&created),
                KnowledgeAction::CreateEntity {
                    entity: created.clone(),
                },
            ));
            ledger.legacy_ids.insert("old".into(), "kg_1".into());
            Ok(())
        })
        .unwrap();
        let previous = fs::read(root.path().join(KNOWLEDGE_FILE)).unwrap();

        update(root.path(), |ledger| {
            ledger.legacy_ids.insert("older".into(), "kg_1".into());
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
    fn bootstrap_uses_one_sqlite_snapshot_across_all_legacy_tables() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join(super::super::GRAPH_FILE);
        let connection = super::super::open(root.path()).unwrap();
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .unwrap();
        connection
            .execute(
                "INSERT INTO entities(id, kind, name, aliases, is_person) VALUES('e:old','project','Old','[]',0)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO entity_name_overrides(id, name) VALUES('e:old', 'Old Override')",
                [],
            )
            .unwrap();
        drop(connection);

        let ledger = bootstrap_legacy_with_hook(root.path(), || {
            let writer = rusqlite::Connection::open(&database).unwrap();
            let transaction = writer.unchecked_transaction().unwrap();
            transaction
                .execute(
                    "INSERT INTO entities(id, kind, name, aliases, is_person) VALUES('e:new','project','New Raw','[]',0)",
                    [],
                )
                .unwrap();
            transaction
                .execute(
                    "INSERT INTO entity_name_overrides(id, name) VALUES('e:new', 'New Override')",
                    [],
                )
                .unwrap();
            transaction
                .execute(
                    "INSERT INTO entity_redirects(old_id, new_id) VALUES('e:old', 'e:new')",
                    [],
                )
                .unwrap();
            transaction.commit().unwrap();
        })
        .unwrap();

        assert_eq!(ledger.registry.len(), 1);
        assert_eq!(
            ledger.registry[ledger.legacy_ids["e:old"].as_str()].name,
            "Old Override"
        );
        assert!(!ledger.legacy_ids.contains_key("e:new"));
        assert!(!ledger
            .operations
            .iter()
            .any(|operation| matches!(operation.action, KnowledgeAction::MergeEntity { .. })));
    }

    #[test]
    fn bootstrap_preserves_legacy_person_id_without_allocating_kg_fork() {
        let root = tempfile::tempdir().unwrap();
        let conn = super::super::open(root.path()).unwrap();
        conn.execute(
            "INSERT INTO entities(id, kind, name, aliases, is_person) VALUES('P1','person','Ada','[]',1)",
            [],
        )
        .unwrap();
        drop(conn);

        let ledger = load(root.path()).unwrap();

        assert_eq!(ledger.legacy_ids["P1"], "P1");
        assert_eq!(ledger.registry["P1"].name, "Ada");
        assert!(!ledger.registry.keys().any(|id| id.starts_with("kg_")));
        let snapshot = crate::graph::resolve::replay(&ledger).unwrap();
        let resolution = crate::graph::resolve::resolve_entity(
            &snapshot,
            &serde_json::from_value::<crate::store::Voiceprints>(serde_json::json!({
                "schema_version": 1,
                "next_person": 2,
                "people": {"P1": {"name": "Ada"}}
            }))
            .unwrap(),
            "n1",
            &crate::store::Entity {
                id: "P1".into(),
                kind: "person".into(),
                name: "Ada".into(),
                aliases: Vec::new(),
            },
            &[],
        );
        assert_eq!(resolution.entity_id.as_deref(), Some("P1"));
    }

    #[test]
    fn malformed_legacy_aliases_abort_bootstrap_and_leave_ledger_absent() {
        let root = tempfile::tempdir().unwrap();
        let conn = super::super::open(root.path()).unwrap();
        conn.execute(
            "INSERT INTO entities(id, kind, name, aliases, is_person) VALUES('e:bad','project','Bad','not-json',0)",
            [],
        )
        .unwrap();
        drop(conn);

        let error = load(root.path()).unwrap_err();

        assert!(matches!(error, KnowledgeLoadError::Corrupt { .. }));
        assert!(error.to_string().contains("e:bad"));
        assert!(error.to_string().contains("aliases"));
        assert!(!root.path().join(KNOWLEDGE_FILE).exists());
    }

    #[test]
    fn invalid_first_bootstrap_is_rejected_without_persisting_a_ledger() {
        let root = tempfile::tempdir().unwrap();
        let connection = super::super::open(root.path()).unwrap();
        connection
            .execute(
                "INSERT INTO entity_redirects(old_id, new_id) VALUES('e:old', 'e:missing')",
                [],
            )
            .unwrap();
        drop(connection);

        let error = load(root.path()).unwrap_err();

        assert!(matches!(error, KnowledgeLoadError::Corrupt { .. }));
        assert!(error.to_string().contains("invalid target"));
        assert!(!root.path().join(KNOWLEDGE_FILE).exists());
    }

    #[test]
    fn set_like_vectors_are_canonicalized_before_persistence() {
        fn write(root: &Path, aliases: &[&str], evidence: &[&str]) -> Vec<u8> {
            update(root, |ledger| {
                ledger.registry.insert(
                    "kg_1".into(),
                    RegistryEntity {
                        kind: "project".into(),
                        name: "Apollo".into(),
                        aliases: aliases.iter().map(|value| (*value).into()).collect(),
                        status: "confirmed".into(),
                    },
                );
                let created = ledger.registry["kg_1"].clone();
                ledger.operations.push(entity_operation(
                    "op_entity",
                    "kg_1",
                    None,
                    Some(&created),
                    KnowledgeAction::CreateEntity {
                        entity: created.clone(),
                    },
                ));
                ledger.operations.push(operation(
                    "op_relation",
                    KnowledgeAction::CreateRelation {
                        relation: UserRelation {
                            subject_id: "kg_1".into(),
                            predicate: crate::store::RelationPredicate {
                                kind: "uses".into(),
                                label: None,
                            },
                            object_id: "kg_2".into(),
                            valid_from: None,
                            valid_to: None,
                            note: None,
                            evidence_ids: evidence.iter().map(|value| (*value).into()).collect(),
                            user_assertion: false,
                        },
                    },
                ));
                Ok(())
            })
            .unwrap();
            fs::read(root.join(KNOWLEDGE_FILE)).unwrap()
        }

        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let first = write(
            a.path(),
            &["Zulu", "Alpha", "Zulu"],
            &["ev_2", "ev_1", "ev_2"],
        );
        let second = write(b.path(), &["Alpha", "Zulu"], &["ev_1", "ev_2"]);

        assert_eq!(first, second);
        let ledger = load(a.path()).unwrap();
        assert_eq!(ledger.registry["kg_1"].aliases, vec!["Alpha", "Zulu"]);
        let KnowledgeAction::CreateRelation { relation } = &ledger.operations[1].action else {
            panic!("expected create relation")
        };
        assert_eq!(relation.evidence_ids, vec!["ev_1", "ev_2"]);
    }

    #[test]
    fn load_rejects_noncanonical_registry_and_operation_sets() {
        let registry_root = tempfile::tempdir().unwrap();
        let mut registry_ledger = KnowledgeLedger::empty();
        registry_ledger.registry.insert(
            "kg_1".into(),
            RegistryEntity {
                kind: "project".into(),
                name: "Apollo".into(),
                aliases: vec!["Zulu".into(), "Alpha".into()],
                status: "confirmed".into(),
            },
        );
        fs::write(
            registry_root.path().join(KNOWLEDGE_FILE),
            serde_json::to_vec(&registry_ledger).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            load(registry_root.path()),
            Err(KnowledgeLoadError::Corrupt { .. })
        ));

        let evidence_root = tempfile::tempdir().unwrap();
        let mut evidence_ledger = KnowledgeLedger::empty();
        evidence_ledger.operations.push(operation(
            "op_relation",
            KnowledgeAction::CreateRelation {
                relation: UserRelation {
                    subject_id: "kg_1".into(),
                    predicate: crate::store::RelationPredicate {
                        kind: "uses".into(),
                        label: None,
                    },
                    object_id: "kg_2".into(),
                    valid_from: None,
                    valid_to: None,
                    note: None,
                    evidence_ids: vec!["ev_2".into(), "ev_1".into()],
                    user_assertion: false,
                },
            },
        ));
        fs::write(
            evidence_root.path().join(KNOWLEDGE_FILE),
            serde_json::to_vec(&evidence_ledger).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            load(evidence_root.path()),
            Err(KnowledgeLoadError::Corrupt { .. })
        ));
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
