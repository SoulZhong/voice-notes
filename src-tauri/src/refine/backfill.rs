#[allow(unused_imports)] // Public request contract is consumed through ipc.rs today.
pub use crate::ipc::{BackfillFailure, BackfillPreview, BackfillProgress, BackfillRequest};
use crate::settings::Settings;
use crate::store::aing_graph::{ValidatedGraph, GRAPH_CONTRACT_VERSION};
use crate::store::{GraphExtraction, RefinedDoc};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub trait RelationExecutor: Send + Sync {
    fn provider(&self) -> &str;
    fn model(&self) -> &str;
    fn extract(&self, note_id: &str, doc: &RefinedDoc) -> anyhow::Result<ValidatedGraph>;
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BackfillOutcome {
    pub note_id: String,
    pub state: String,
    pub provider: String,
    pub model: String,
    pub contract_version: u32,
    pub source_hash: String,
    pub committed: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ApprovedBackfill {
    pub preview: BackfillPreview,
    pub source_hashes: BTreeMap<String, String>,
}

pub(crate) struct BackfillGate {
    running: Arc<AtomicBool>,
    active_run_id: Arc<Mutex<Option<String>>>,
    run_id: String,
}

impl BackfillGate {
    pub(crate) fn acquire(
        running: Arc<AtomicBool>,
        active_run_id: Arc<Mutex<Option<String>>>,
        run_id: &str,
    ) -> anyhow::Result<Self> {
        validate_run_id(run_id)?;
        running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| anyhow::anyhow!("关系补建已在运行"))?;
        let mut active = active_run_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.is_some() {
            running.store(false, Ordering::SeqCst);
            anyhow::bail!("关系补建运行标识尚未释放");
        }
        *active = Some(run_id.into());
        drop(active);
        Ok(Self {
            running,
            active_run_id,
            run_id: run_id.into(),
        })
    }
}

impl Drop for BackfillGate {
    fn drop(&mut self) {
        let mut active = self
            .active_run_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.as_deref() == Some(self.run_id.as_str()) {
            *active = None;
        }
        self.running.store(false, Ordering::SeqCst);
    }
}

fn validate_run_id(run_id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !run_id.is_empty()
            && run_id.len() <= 128
            && run_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "非法关系补建 run_id"
    );
    Ok(())
}

pub(crate) fn request_cancel(
    active_run_id: &Mutex<Option<String>>,
    cancel: &AtomicBool,
    run_id: &str,
) -> anyhow::Result<()> {
    validate_run_id(run_id)?;
    let active = active_run_id
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    anyhow::ensure!(
        active.as_deref() == Some(run_id),
        "关系补建 run_id 已过期或不在运行"
    );
    cancel.store(true, Ordering::SeqCst);
    Ok(())
}

fn configured_provider(settings: &Settings) -> anyhow::Result<(&str, &str)> {
    match settings.refine_provider.as_str() {
        "openai" => {
            anyhow::ensure!(
                !settings.refine_model.trim().is_empty(),
                "关系补建模型未配置"
            );
            Ok(("openai", settings.refine_model.trim()))
        }
        "agent" => {
            let kind = crate::refine::agent::AgentKind::from_key(&settings.refine_agent)
                .ok_or_else(|| anyhow::anyhow!("未知 Agent provider: {}", settings.refine_agent))?;
            anyhow::ensure!(
                matches!(
                    kind,
                    crate::refine::agent::AgentKind::Claude
                        | crate::refine::agent::AgentKind::Gemini
                ),
                "{} Agent 无法结构性限制为两个关系 MCP 工具，本次补建已安全拒绝",
                settings.refine_agent
            );
            anyhow::ensure!(
                !settings.refine_agent_model.trim().is_empty(),
                "关系补建 Agent 模型未配置"
            );
            Ok(("agent", settings.refine_agent_model.trim()))
        }
        provider => anyhow::bail!("未知关系补建 provider: {provider}"),
    }
}

pub(crate) fn is_current(doc: &RefinedDoc) -> bool {
    let source_hash = crate::store::source_hash(&doc.paragraphs);
    doc.stages.relations == "done"
        && doc
            .graph_extraction
            .as_ref()
            .map(|extraction| {
                extraction.contract_version == GRAPH_CONTRACT_VERSION
                    && extraction.source_hash == source_hash
            })
            .unwrap_or(false)
}

fn selected_by_default(doc: &RefinedDoc) -> bool {
    doc.stages.relations == "failed"
        || doc
            .graph_extraction
            .as_ref()
            .map(|extraction| extraction.contract_version != GRAPH_CONTRACT_VERSION)
            .unwrap_or(false)
}

fn validate_backfill_note_id(note_id: &str) -> anyhow::Result<()> {
    crate::store::validate_note_id(note_id)?;
    let mut components = Path::new(note_id).components();
    anyhow::ensure!(
        matches!(components.next(), Some(std::path::Component::Normal(_)))
            && components.next().is_none(),
        "非法笔记 id: {note_id:?}"
    );
    Ok(())
}

fn load_preview_doc(notes_root: &Path, note_id: &str) -> anyhow::Result<Option<RefinedDoc>> {
    let anchored = crate::store::refined::AnchoredRefinedDir::open(notes_root, note_id)?;
    anchored.load_current()
}

fn hash_preview_field(hasher: &mut Sha256, value: &str) {
    hasher.update(value.len().to_be_bytes());
    hasher.update(value.as_bytes());
}

fn consent_token(
    notes_root: &Path,
    note_ids: &[String],
    provider: &str,
    model: &str,
) -> anyhow::Result<String> {
    let source_hashes = approved_source_hashes(notes_root, note_ids)?;
    Ok(consent_token_from_hashes(
        note_ids,
        provider,
        model,
        &source_hashes,
    ))
}

fn consent_token_from_hashes(
    note_ids: &[String],
    provider: &str,
    model: &str,
    source_hashes: &BTreeMap<String, String>,
) -> String {
    let mut hasher = Sha256::new();
    hash_preview_field(&mut hasher, provider);
    hash_preview_field(&mut hasher, model);
    hasher.update(GRAPH_CONTRACT_VERSION.to_be_bytes());
    for note_id in note_ids {
        hash_preview_field(&mut hasher, note_id);
        hash_preview_field(
            &mut hasher,
            source_hashes
                .get(note_id)
                .expect("approved sources cover every selected note"),
        );
    }
    format!("backfill-preview-{:x}", hasher.finalize())
}

pub(crate) fn approved_source_hashes(
    notes_root: &Path,
    note_ids: &[String],
) -> anyhow::Result<BTreeMap<String, String>> {
    note_ids
        .iter()
        .map(|note_id| {
            let doc = load_preview_doc(notes_root, note_id)?
                .ok_or_else(|| anyhow::anyhow!("笔记 {note_id} 没有可补建的 aing.json"))?;
            Ok((
                note_id.clone(),
                crate::store::source_hash(&doc.paragraphs),
            ))
        })
        .collect()
}

pub(crate) fn validate_request(
    fresh: &BackfillPreview,
    request: &BackfillRequest,
) -> anyhow::Result<()> {
    validate_run_id(&request.run_id)?;
    anyhow::ensure!(!request.note_ids.is_empty(), "关系补建选择不能为空");
    anyhow::ensure!(
        request.consent_token == fresh.consent_token
            && request.note_ids == fresh.note_ids
            && request.provider == fresh.provider
            && request.model == fresh.model
            && request.contract_version == fresh.contract_version,
        "关系补建预览已变化，请重新预览并确认"
    );
    Ok(())
}

pub(crate) fn preflight(
    data_root: &Path,
    settings: &Settings,
    request: &BackfillRequest,
) -> anyhow::Result<ApprovedBackfill> {
    let preview = preview(data_root, settings, Some(&request.note_ids))?;
    validate_request(&preview, request)?;
    let source_hashes = approved_source_hashes(&data_root.join("notes"), &preview.note_ids)?;
    anyhow::ensure!(
        preview.consent_token
            == consent_token_from_hashes(
                &preview.note_ids,
                &preview.provider,
                &preview.model,
                &source_hashes,
            ),
        "关系补建预览已变化，请重新预览并确认"
    );
    Ok(ApprovedBackfill {
        preview,
        source_hashes,
    })
}

/// 纯只读选择：不迁移旧稿、不写 stage、不触发索引。
pub fn preview(
    data_root: &Path,
    settings: &Settings,
    requested: Option<&[String]>,
) -> anyhow::Result<BackfillPreview> {
    // Human decisions are truth. Validate existing bytes before provider setup
    // or any note-directory scan; missing remains a valid read-only state.
    crate::graph::overrides::validate_existing(data_root)?;
    let (provider, model) = configured_provider(settings)?;
    let notes_root = data_root.join("notes");
    let note_ids = match requested {
        Some(requested) => {
            let mut selected = BTreeSet::new();
            for note_id in requested {
                validate_backfill_note_id(note_id)?;
                anyhow::ensure!(
                    load_preview_doc(&notes_root, note_id)?.is_some(),
                    "笔记 {note_id} 没有可补建的 aing.json"
                );
                selected.insert(note_id.clone());
            }
            selected.into_iter().collect()
        }
        None => {
            let mut selected = Vec::new();
            let entries = match std::fs::read_dir(&notes_root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(BackfillPreview {
                        consent_token: consent_token(&notes_root, &selected, provider, model)?,
                        note_ids: selected,
                        provider: provider.into(),
                        model: model.into(),
                        contract_version: GRAPH_CONTRACT_VERSION,
                    })
                }
                Err(error) => return Err(error.into()),
            };
            for entry in entries {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let note_id = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("笔记目录名不是 UTF-8"))?;
                validate_backfill_note_id(&note_id)?;
                let Some(doc) = load_preview_doc(&notes_root, &note_id)? else {
                    continue;
                };
                if selected_by_default(&doc) {
                    selected.push(note_id);
                }
            }
            selected.sort();
            selected
        }
    };
    Ok(BackfillPreview {
        consent_token: consent_token(&notes_root, &note_ids, provider, model)?,
        note_ids,
        provider: provider.into(),
        model: model.into(),
        contract_version: GRAPH_CONTRACT_VERSION,
    })
}

enum RunOneError {
    Cancelled,
    Failed(anyhow::Error),
}

impl RunOneError {
    #[allow(dead_code)] // Public run_one is retained as the Task 9 contract.
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Cancelled => anyhow::anyhow!("关系补建已取消"),
            Self::Failed(error) => error,
        }
    }
}

fn ensure_revision_unchanged(snapshot: &RefinedDoc, latest: &RefinedDoc) -> anyhow::Result<()> {
    anyhow::ensure!(
        crate::store::source_hash(&snapshot.paragraphs)
            == crate::store::source_hash(&latest.paragraphs),
        "关系补建期间正文已变化，请重试"
    );
    let snapshot_seqs = snapshot
        .paragraphs
        .iter()
        .map(|paragraph| paragraph.source_seqs.as_slice())
        .collect::<Vec<_>>();
    let latest_seqs = latest
        .paragraphs
        .iter()
        .map(|paragraph| paragraph.source_seqs.as_slice())
        .collect::<Vec<_>>();
    anyhow::ensure!(
        snapshot_seqs == latest_seqs,
        "关系补建期间 source seq 已变化，请重试"
    );
    anyhow::ensure!(
        serde_json::to_vec(snapshot)? == serde_json::to_vec(latest)?,
        "关系补建期间 aing.json 已变化，请重试"
    );
    Ok(())
}

fn run_one_controlled_with_writer(
    note_dir: &Path,
    note_id: &str,
    approved_source_hash: Option<&str>,
    executor: &dyn RelationExecutor,
    cancel: &AtomicBool,
    write: impl FnOnce(
        &crate::store::refined::AnchoredRefinedDir,
        &RefinedDoc,
        &crate::store::notelock::NoteLock,
    ) -> anyhow::Result<()>,
) -> Result<BackfillOutcome, RunOneError> {
    if cancel.load(Ordering::SeqCst) {
        return Err(RunOneError::Cancelled);
    }
    validate_backfill_note_id(note_id).map_err(RunOneError::Failed)?;
    if note_dir.file_name().and_then(|name| name.to_str()) != Some(note_id) {
        return Err(RunOneError::Failed(anyhow::anyhow!("笔记 id 与目录不匹配")));
    }
    let notes_root = note_dir
        .parent()
        .ok_or_else(|| RunOneError::Failed(anyhow::anyhow!("笔记目录缺少 notes 根")))?;
    let anchored = crate::store::refined::AnchoredRefinedDir::open(notes_root, note_id)
        .map_err(RunOneError::Failed)?;
    // The override ledger is human-decision truth. A corrupt ledger makes every
    // graph write read-only, including model backfill, while the last index stays
    // available to readers.
    crate::graph::overrides::load(
        notes_root
            .parent()
            .ok_or_else(|| RunOneError::Failed(anyhow::anyhow!("笔记目录缺少数据根")))?,
    )
    .map_err(|error| RunOneError::Failed(error.into()))?;
    let snapshot = anchored
        .load_current()
        .map_err(RunOneError::Failed)?
        .ok_or_else(|| RunOneError::Failed(anyhow::anyhow!("aing.json 不存在或已损坏")))?;
    if let Some(approved_source_hash) = approved_source_hash {
        if crate::store::source_hash(&snapshot.paragraphs) != approved_source_hash {
            return Err(RunOneError::Failed(anyhow::anyhow!(
                "关系补建预览后正文已变化，请重新预览并确认"
            )));
        }
    }
    if is_current(&snapshot) {
        return Ok(BackfillOutcome {
            note_id: note_id.into(),
            state: "skipped".into(),
            provider: executor.provider().into(),
            model: executor.model().into(),
            contract_version: GRAPH_CONTRACT_VERSION,
            source_hash: crate::store::source_hash(&snapshot.paragraphs),
            committed: false,
        });
    }
    if cancel.load(Ordering::SeqCst) {
        return Err(RunOneError::Cancelled);
    }

    // 外部 provider 只收到不可变快照；此处没有 NoteLock 或任何文件锁存活。
    let extracted = executor
        .extract(note_id, &snapshot)
        .map_err(RunOneError::Failed)?;
    if cancel.load(Ordering::SeqCst) {
        return Err(RunOneError::Cancelled);
    }

    let note_lock = anchored
        .acquire_lock()
        .map_err(|error| RunOneError::Failed(error.into()))?
        .ok_or_else(|| {
            RunOneError::Failed(anyhow::anyhow!("笔记正在被另一进程修改，请稍后重试"))
        })?;
    let latest = anchored
        .load_locked(&note_lock)
        .map_err(RunOneError::Failed)?
        .ok_or_else(|| RunOneError::Failed(anyhow::anyhow!("aing.json 不存在或已损坏")))?;
    ensure_revision_unchanged(&snapshot, &latest).map_err(RunOneError::Failed)?;
    if cancel.load(Ordering::SeqCst) {
        return Err(RunOneError::Cancelled);
    }

    // executor 的返回值不是信任边界；提交前必须针对锁内最新文档再过 Task 2 validator。
    let graph = crate::store::aing_graph::validate_graph(note_id, &latest, extracted.relations)
        .map_err(|issues| {
            RunOneError::Failed(anyhow::anyhow!(
                "图谱校验失败:{}",
                serde_json::to_string(&issues).unwrap_or_else(|_| "invalid graph".into())
            ))
        })?;
    let mut candidate = latest.clone();
    let source_hash = crate::store::source_hash(&candidate.paragraphs);
    let generated_at = chrono::Local::now().to_rfc3339();
    let extraction = GraphExtraction {
        contract_version: GRAPH_CONTRACT_VERSION,
        provider: executor.provider().into(),
        model: executor.model().into(),
        run_id: crate::store::stable_id(
            "run_",
            &[
                note_id.into(),
                executor.provider().into(),
                executor.model().into(),
                generated_at.clone(),
                source_hash.clone(),
            ],
        ),
        generated_at,
        source_hash,
        mode: "relation-backfill".into(),
    };
    crate::refine::relations::apply_validated_graph(&mut candidate, extraction, graph);
    if !candidate
        .paragraphs
        .iter()
        .map(|paragraph| paragraph.text.as_bytes())
        .eq(latest
            .paragraphs
            .iter()
            .map(|paragraph| paragraph.text.as_bytes()))
    {
        return Err(RunOneError::Failed(anyhow::anyhow!("关系补建不得修改正文")));
    }
    if cancel.load(Ordering::SeqCst) {
        return Err(RunOneError::Cancelled);
    }
    write(&anchored, &candidate, &note_lock).map_err(RunOneError::Failed)?;
    Ok(BackfillOutcome {
        note_id: note_id.into(),
        state: "committed".into(),
        provider: executor.provider().into(),
        model: executor.model().into(),
        contract_version: GRAPH_CONTRACT_VERSION,
        source_hash: crate::store::source_hash(&candidate.paragraphs),
        committed: true,
    })
}

fn run_one_controlled(
    note_dir: &Path,
    note_id: &str,
    approved_source_hash: Option<&str>,
    executor: &dyn RelationExecutor,
    cancel: &AtomicBool,
) -> Result<BackfillOutcome, RunOneError> {
    run_one_controlled_with_writer(
        note_dir,
        note_id,
        approved_source_hash,
        executor,
        cancel,
        |anchored, candidate, note_lock| anchored.write_locked(candidate, note_lock),
    )
}

#[cfg(test)]
pub(crate) fn run_one_with_cancel(
    note_dir: &Path,
    note_id: &str,
    executor: &dyn RelationExecutor,
    cancel: &AtomicBool,
) -> anyhow::Result<BackfillOutcome> {
    run_one_controlled(note_dir, note_id, None, executor, cancel).map_err(RunOneError::into_anyhow)
}

#[allow(dead_code)] // Public Task 9 contract; batch execution uses the cancellable core directly.
pub fn run_one(
    note_dir: &Path,
    note_id: &str,
    executor: &dyn RelationExecutor,
) -> anyhow::Result<BackfillOutcome> {
    run_one_controlled(note_dir, note_id, None, executor, &AtomicBool::new(false))
        .map_err(RunOneError::into_anyhow)
}

pub(crate) fn run_batch(
    run_id: &str,
    notes_root: &Path,
    note_ids: &[String],
    approved_source_hashes: &BTreeMap<String, String>,
    executor: &dyn RelationExecutor,
    cancel: &AtomicBool,
    mut emit: impl FnMut(BackfillProgress),
    mut request_rebuild: impl FnMut() -> anyhow::Result<u64>,
) -> BackfillProgress {
    let total = note_ids.len();
    let mut completed = 0usize;
    let mut committed = 0usize;
    let mut succeeded = 0usize;
    let mut failed = Vec::new();
    let mut cancelled = false;
    let mut panicked = false;
    for note_id in note_ids {
        emit(BackfillProgress {
            run_id: run_id.into(),
            state: "running".into(),
            completed,
            total,
            current_note_id: Some(note_id.clone()),
            failed: failed.clone(),
            rebuild_generation: None,
            index_error: None,
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let approved_source_hash = approved_source_hashes.get(note_id).ok_or_else(|| {
                RunOneError::Failed(anyhow::anyhow!("关系补建缺少已批准的正文快照"))
            })?;
            run_one_controlled(
                &notes_root.join(note_id),
                note_id,
                Some(approved_source_hash),
                executor,
                cancel,
            )
        }));
        match result {
            Ok(Ok(outcome)) => {
                completed += 1;
                committed += usize::from(outcome.committed);
                succeeded += 1;
            }
            Ok(Err(RunOneError::Cancelled)) => cancelled = true,
            Ok(Err(RunOneError::Failed(error))) => {
                completed += 1;
                failed.push(BackfillFailure {
                    note_id: note_id.clone(),
                    error: format!("{error:#}"),
                });
            }
            Err(_) => {
                panicked = true;
                failed.push(BackfillFailure {
                    note_id: note_id.clone(),
                    error: "关系补建执行器异常退出".into(),
                });
            }
        }
        emit(BackfillProgress {
            run_id: run_id.into(),
            state: "running".into(),
            completed,
            total,
            current_note_id: Some(note_id.clone()),
            failed: failed.clone(),
            rebuild_generation: None,
            index_error: None,
        });
        if cancelled || panicked {
            break;
        }
    }
    let mut rebuild_generation = None;
    let mut index_error = None;
    if committed > 0 || (!cancelled && !panicked && failed.is_empty()) {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut request_rebuild)) {
            Ok(Ok(generation)) => rebuild_generation = Some(generation),
            Ok(Err(error)) => {
                index_error = Some(format!("索引排队失败，已保留 dirty 标记供重试:{error:#}"))
            }
            Err(_) => {
                index_error = Some("索引排队异常退出，dirty 标记状态未知".into());
            }
        }
    }
    let terminal = BackfillProgress {
        run_id: run_id.into(),
        state: if cancelled {
            "cancelled"
        } else if panicked {
            "failed"
        } else if failed.is_empty() {
            "completed"
        } else if succeeded > 0 {
            "partial"
        } else {
            "failed"
        }
        .into(),
        completed,
        total,
        current_note_id: None,
        failed,
        rebuild_generation,
        index_error,
    };
    emit(terminal.clone());
    terminal
}

pub(crate) fn panic_progress(
    mut last_progress: BackfillProgress,
    cancelled: bool,
) -> BackfillProgress {
    let active_note_id = last_progress.current_note_id.take().unwrap_or_default();
    last_progress.state = if cancelled { "cancelled" } else { "failed" }.into();
    last_progress.failed.push(BackfillFailure {
        note_id: active_note_id,
        error: "关系补建线程异常退出".into(),
    });
    last_progress.index_error = None;
    last_progress
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refine::llm::{RawEvidence, RawRelation};
    use crate::store::{
        self, Entity, GraphExtraction, Mention, RefineStages, RefinedDoc, RefinedParagraph,
        RelationPredicate,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    fn fixture_doc(note_id: &str, contract_version: u32, relation_stage: &str) -> RefinedDoc {
        let mut doc = RefinedDoc {
            schema_version: crate::store::refined::REFINED_SCHEMA_VERSION,
            generated_at: "2026-07-21T09:00:00+08:00".into(),
            llm_model: Some("text-model".into()),
            stages: RefineStages {
                filter: "done".into(),
                recluster: "done".into(),
                llm: "done".into(),
                entities: "done".into(),
                relations: relation_stage.into(),
            },
            discarded_seqs: vec![],
            entities: vec![
                Entity {
                    id: "ent_1".into(),
                    kind: "person".into(),
                    name: "张三".into(),
                    aliases: vec![],
                },
                Entity {
                    id: "ent_2".into(),
                    kind: "project".into(),
                    name: "灯塔计划".into(),
                    aliases: vec![],
                },
            ],
            graph_extraction: None,
            relations: vec![],
            graph_support_mentions: vec![],
            paragraphs: vec![RefinedParagraph {
                speaker: "S1".into(),
                name: Some("张三".into()),
                person_id: None,
                start_ms: 0,
                end_ms: 1000,
                text: "张三负责灯塔计划".into(),
                source_seqs: vec![1, 2],
                mentions: vec![
                    Mention {
                        id: String::new(),
                        entity: "ent_1".into(),
                        start: 0,
                        end: 2,
                    },
                    Mention {
                        id: String::new(),
                        entity: "ent_2".into(),
                        start: 4,
                        end: 8,
                    },
                ],
            }],
        };
        store::ensure_graph_ids(note_id, &mut doc);
        let graph =
            crate::refine::relations::materialize(note_id, &doc, vec![raw_relation(0.71)]).unwrap();
        doc.relations = graph.relations;
        doc.graph_extraction = Some(GraphExtraction {
            contract_version,
            provider: "old-provider".into(),
            model: "old-model".into(),
            run_id: "old-run".into(),
            generated_at: doc.generated_at.clone(),
            source_hash: store::source_hash(&doc.paragraphs),
            mode: "old".into(),
        });
        doc
    }

    fn raw_relation(confidence: f64) -> RawRelation {
        RawRelation {
            subject: "张三".into(),
            predicate: RelationPredicate {
                kind: "responsible_for".into(),
                label: None,
            },
            object: "灯塔计划".into(),
            confidence,
            valid_from: None,
            valid_to: None,
            evidence: vec![RawEvidence {
                paragraph_index: 0,
                start: 0,
                end: 8,
                quote: "张三负责灯塔计划".into(),
            }],
        }
    }

    fn write_note(root: &std::path::Path, note_id: &str, doc: &RefinedDoc) {
        let dir = root.join("notes").join(note_id);
        std::fs::create_dir_all(&dir).unwrap();
        store::write_refined_atomic(&dir, doc).unwrap();
    }

    fn text_bytes(doc: &RefinedDoc) -> Vec<Vec<u8>> {
        doc.paragraphs
            .iter()
            .map(|paragraph| paragraph.text.as_bytes().to_vec())
            .collect()
    }

    struct MaterializingExecutor {
        calls: Arc<AtomicUsize>,
        cancel_on_call: Option<(usize, Arc<AtomicBool>)>,
        fail: bool,
        invalid: bool,
    }

    impl MaterializingExecutor {
        fn success(calls: Arc<AtomicUsize>) -> Self {
            Self {
                calls,
                cancel_on_call: None,
                fail: false,
                invalid: false,
            }
        }
    }

    impl RelationExecutor for MaterializingExecutor {
        fn provider(&self) -> &str {
            "test-provider"
        }

        fn model(&self) -> &str {
            "test-model"
        }

        fn extract(
            &self,
            note_id: &str,
            doc: &RefinedDoc,
        ) -> anyhow::Result<crate::store::aing_graph::ValidatedGraph> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some((at, cancel)) = &self.cancel_on_call {
                if call == *at {
                    cancel.store(true, Ordering::SeqCst);
                }
            }
            if self.fail {
                anyhow::bail!("provider failed");
            }
            let mut graph =
                crate::refine::relations::materialize(note_id, doc, vec![raw_relation(0.93)])
                    .map_err(|issues| anyhow::anyhow!("{issues:?}"))?;
            if self.invalid {
                graph.relations[0].subject = "missing".into();
            }
            Ok(graph)
        }
    }

    #[test]
    fn preview_defaults_to_non_current_notes_and_explicit_selection_is_strict_and_sorted() {
        let root = tempfile::tempdir().unwrap();
        let current = fixture_doc(
            "current",
            crate::store::aing_graph::GRAPH_CONTRACT_VERSION,
            "done",
        );
        let old = fixture_doc("old", 0, "done");
        let failed = fixture_doc(
            "failed",
            crate::store::aing_graph::GRAPH_CONTRACT_VERSION,
            "failed",
        );
        let mut off = fixture_doc(
            "off",
            crate::store::aing_graph::GRAPH_CONTRACT_VERSION,
            "off",
        );
        off.graph_extraction = None;
        let mut stale = fixture_doc(
            "stale",
            crate::store::aing_graph::GRAPH_CONTRACT_VERSION,
            "done",
        );
        stale.graph_extraction.as_mut().unwrap().source_hash = "stale".into();
        write_note(root.path(), "current", &current);
        write_note(root.path(), "old", &old);
        write_note(root.path(), "failed", &failed);
        write_note(root.path(), "off", &off);
        write_note(root.path(), "stale", &stale);
        std::fs::create_dir_all(root.path().join("notes/unrefined")).unwrap();

        let mut settings = crate::settings::Settings::default();
        settings.refine_provider = "openai".into();
        settings.refine_model = "relation-model".into();
        let found = preview(root.path(), &settings, None).unwrap();
        assert_eq!(found.note_ids, vec!["failed", "old"]);
        assert_eq!(found.provider, "openai");
        assert_eq!(found.model, "relation-model");
        assert_eq!(
            found.contract_version,
            crate::store::aing_graph::GRAPH_CONTRACT_VERSION
        );

        let requested = vec!["old".into(), "current".into(), "old".into()];
        assert_eq!(
            preview(root.path(), &settings, Some(&requested))
                .unwrap()
                .note_ids,
            vec!["current", "old"]
        );
        assert!(preview(root.path(), &settings, Some(&["../old".into()])).is_err());
        assert!(preview(root.path(), &settings, Some(&[".".into()])).is_err());
        assert!(preview(root.path(), &settings, Some(&["missing".into()])).is_err());
        assert!(preview(root.path(), &settings, Some(&["unrefined".into()])).is_err());
    }

    #[test]
    fn agent_preview_only_advertises_executors_that_start_can_construct() {
        let root = tempfile::tempdir().unwrap();
        let mut settings = crate::settings::Settings::default();
        settings.refine_provider = "agent".into();
        settings.refine_agent_model = "agent-model".into();
        let fake_bin = root.path().join("fake-agent");
        std::fs::write(&fake_bin, b"").unwrap();
        settings.refine_agent_bin = fake_bin.to_string_lossy().into_owned();

        for unsupported in ["codex", "cursor"] {
            settings.refine_agent = unsupported.into();
            assert!(preview(root.path(), &settings, None).is_err());
        }

        settings.refine_agent = "claude".into();
        let shown = preview(root.path(), &settings, None).unwrap();
        let executor = crate::refine::agent::AgentRelationExecutor::new(
            crate::refine::agent::AgentKind::Claude,
            &settings.refine_agent_bin,
            &settings.refine_agent_model,
        )
        .unwrap();
        assert_eq!(shown.provider, executor.provider());
        assert_eq!(shown.model, executor.model());
    }

    #[test]
    fn preview_consent_token_binds_selection_source_provider_model_and_contract() {
        let root = tempfile::tempdir().unwrap();
        let note_id = "consent-bound";
        write_note(root.path(), note_id, &fixture_doc(note_id, 0, "failed"));
        let mut settings = crate::settings::Settings::default();
        settings.refine_provider = "openai".into();
        settings.refine_model = "relation-model-a".into();

        let first = preview(root.path(), &settings, Some(&[note_id.into()])).unwrap();
        assert!(first.consent_token.starts_with("backfill-preview-"));
        let exact = BackfillRequest {
            run_id: "run-consent-a".into(),
            consent_token: first.consent_token.clone(),
            note_ids: first.note_ids.clone(),
            provider: first.provider.clone(),
            model: first.model.clone(),
            contract_version: first.contract_version,
        };
        validate_request(&first, &exact).unwrap();

        let note_dir = root.path().join("notes").join(note_id);
        let mut changed = store::load_refined(&note_dir).unwrap();
        changed.paragraphs[0].text.push_str("，正文已变");
        changed.paragraphs[0].source_seqs.push(99);
        store::write_refined_atomic(&note_dir, &changed).unwrap();
        let source_changed = preview(root.path(), &settings, Some(&[note_id.into()])).unwrap();
        assert_ne!(source_changed.consent_token, first.consent_token);
        assert!(validate_request(&source_changed, &exact).is_err());

        settings.refine_model = "relation-model-b".into();
        let model_changed = preview(root.path(), &settings, Some(&[note_id.into()])).unwrap();
        assert_ne!(model_changed.consent_token, source_changed.consent_token);
        assert!(validate_request(&model_changed, &exact).is_err());
    }

    #[test]
    fn batch_rechecks_each_approved_source_before_calling_the_provider() {
        let root = tempfile::tempdir().unwrap();
        let note_ids = vec!["first".to_string(), "later".to_string()];
        for note_id in &note_ids {
            write_note(root.path(), note_id, &fixture_doc(note_id, 0, "failed"));
        }
        let mut settings = crate::settings::Settings::default();
        settings.refine_provider = "openai".into();
        settings.refine_model = "relation-model".into();
        let shown = preview(root.path(), &settings, Some(&note_ids)).unwrap();
        let request = BackfillRequest {
            run_id: "run-source-drift".into(),
            consent_token: shown.consent_token,
            note_ids: shown.note_ids,
            provider: shown.provider,
            model: shown.model,
            contract_version: shown.contract_version,
        };
        let approved = preflight(root.path(), &settings, &request).unwrap();

        struct BlockingExecutor {
            calls: Arc<std::sync::Mutex<Vec<String>>>,
            first_entered: Arc<std::sync::Barrier>,
            release_first: Arc<std::sync::Barrier>,
        }
        impl RelationExecutor for BlockingExecutor {
            fn provider(&self) -> &str {
                "openai"
            }
            fn model(&self) -> &str {
                "relation-model"
            }
            fn extract(
                &self,
                note_id: &str,
                doc: &RefinedDoc,
            ) -> anyhow::Result<crate::store::aing_graph::ValidatedGraph> {
                self.calls.lock().unwrap().push(note_id.into());
                if note_id == "first" {
                    self.first_entered.wait();
                    self.release_first.wait();
                }
                crate::refine::relations::materialize(note_id, doc, vec![raw_relation(0.93)])
                    .map_err(|issues| anyhow::anyhow!("{issues:?}"))
            }
        }

        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let first_entered = Arc::new(std::sync::Barrier::new(2));
        let release_first = Arc::new(std::sync::Barrier::new(2));
        let executor = Arc::new(BlockingExecutor {
            calls: Arc::clone(&calls),
            first_entered: Arc::clone(&first_entered),
            release_first: Arc::clone(&release_first),
        });
        let notes_root = root.path().join("notes");
        let cancel = Arc::new(AtomicBool::new(false));
        let worker = std::thread::spawn({
            let note_ids = note_ids.clone();
            let cancel = Arc::clone(&cancel);
            move || {
                run_batch(
                    "run-source-drift",
                    &notes_root,
                    &note_ids,
                    &approved.source_hashes,
                    executor.as_ref(),
                    &cancel,
                    |_| {},
                    || Ok(73),
                )
            }
        });

        first_entered.wait();
        let later_dir = root.path().join("notes/later");
        let mut changed = store::load_refined(&later_dir).unwrap();
        changed.paragraphs[0].text.push_str("，用户刚刚修改");
        changed.paragraphs[0].source_seqs.push(99);
        store::write_refined_atomic(&later_dir, &changed).unwrap();
        let changed_bytes = std::fs::read(later_dir.join(store::AING_DOC_FILE)).unwrap();
        release_first.wait();

        let terminal = worker.join().unwrap();
        assert_eq!(calls.lock().unwrap().as_slice(), ["first"]);
        assert_eq!(terminal.state, "partial");
        assert_eq!(terminal.rebuild_generation, Some(73));
        assert!(terminal.failed.iter().any(|failure| {
            failure.note_id == "later" && failure.error.contains("预览后正文已变化")
        }));
        assert_eq!(
            std::fs::read(later_dir.join(store::AING_DOC_FILE)).unwrap(),
            changed_bytes
        );
        assert_eq!(
            store::load_refined(&later_dir)
                .unwrap()
                .graph_extraction
                .unwrap()
                .run_id,
            "old-run"
        );
    }

    #[test]
    fn preview_rejects_corrupt_human_ledger_before_note_selection() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(crate::graph::overrides::KNOWLEDGE_FILE),
            b"{broken",
        )
        .unwrap();
        let mut settings = crate::settings::Settings::default();
        settings.refine_provider = "openai".into();
        settings.refine_model = "relation-model".into();

        let error = preview(root.path(), &settings, Some(&["../must-not-scan".into()]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("knowledge-overrides.json"), "{error}");
        assert!(
            !error.contains("非法笔记"),
            "ledger validation must run first: {error}"
        );
    }

    #[test]
    fn cancel_is_scoped_to_the_exact_active_run() {
        let active = Arc::new(std::sync::Mutex::new(None));
        let running = Arc::new(AtomicBool::new(false));
        let cancel = AtomicBool::new(false);
        let gate = BackfillGate::acquire(Arc::clone(&running), Arc::clone(&active), "run-current")
            .unwrap();

        assert!(request_cancel(&active, &cancel, "run-old").is_err());
        assert!(!cancel.load(Ordering::SeqCst));
        request_cancel(&active, &cancel, "run-current").unwrap();
        assert!(cancel.load(Ordering::SeqCst));
        drop(gate);
        assert!(active.lock().unwrap().is_none());
    }

    #[test]
    fn cancellation_before_provider_and_between_extraction_and_commit_preserves_every_byte() {
        let root = tempfile::tempdir().unwrap();
        let note_id = "cancelled";
        let before = fixture_doc(note_id, 0, "failed");
        write_note(root.path(), note_id, &before);
        let note_dir = root.path().join("notes").join(note_id);
        let before_bytes = std::fs::read(note_dir.join(store::AING_DOC_FILE)).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let cancel = Arc::new(AtomicBool::new(true));
        let executor = MaterializingExecutor::success(Arc::clone(&calls));

        assert!(run_one_with_cancel(&note_dir, note_id, &executor, &cancel).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            std::fs::read(note_dir.join(store::AING_DOC_FILE)).unwrap(),
            before_bytes
        );

        cancel.store(false, Ordering::SeqCst);
        let executor = MaterializingExecutor {
            calls: Arc::clone(&calls),
            cancel_on_call: Some((1, Arc::clone(&cancel))),
            fail: false,
            invalid: false,
        };
        assert!(run_one_with_cancel(&note_dir, note_id, &executor, &cancel).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read(note_dir.join(store::AING_DOC_FILE)).unwrap(),
            before_bytes
        );
    }

    #[test]
    fn success_changes_only_graph_fields_and_failures_preserve_the_complete_old_graph() {
        let root = tempfile::tempdir().unwrap();
        let success_id = "success";
        let failed_id = "extract-failed";
        let invalid_id = "validation-failed";
        for note_id in [success_id, failed_id, invalid_id] {
            write_note(root.path(), note_id, &fixture_doc(note_id, 0, "failed"));
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let executor = MaterializingExecutor::success(Arc::clone(&calls));
        let success_dir = root.path().join("notes").join(success_id);
        let before = store::load_refined(&success_dir).unwrap();
        let outcome = run_one(&success_dir, success_id, &executor).unwrap();
        assert_eq!(outcome.state, "committed");
        let after = store::load_refined(&success_dir).unwrap();
        assert_eq!(text_bytes(&after), text_bytes(&before));
        assert_eq!(after.entities.len(), before.entities.len());
        assert_eq!(after.relations[0].confidence, 0.93);
        let extraction = after.graph_extraction.unwrap();
        assert_eq!(extraction.provider, executor.provider());
        assert_eq!(extraction.model, executor.model());
        let skipped = run_one(&success_dir, success_id, &executor).unwrap();
        assert_eq!(skipped.state, "skipped");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        for (note_id, fail, invalid) in [(failed_id, true, false), (invalid_id, false, true)] {
            let dir = root.path().join("notes").join(note_id);
            let bytes = std::fs::read(dir.join(store::AING_DOC_FILE)).unwrap();
            let executor = MaterializingExecutor {
                calls: Arc::new(AtomicUsize::new(0)),
                cancel_on_call: None,
                fail,
                invalid,
            };
            assert!(run_one(&dir, note_id, &executor).is_err());
            assert_eq!(
                std::fs::read(dir.join(store::AING_DOC_FILE)).unwrap(),
                bytes,
                "failed extraction/validation must preserve all old graph fields"
            );
        }
    }

    #[test]
    fn cancellation_during_second_note_keeps_first_commit_and_resume_skips_it() {
        let root = tempfile::tempdir().unwrap();
        let note_ids = vec!["one".to_string(), "two".to_string()];
        for note_id in &note_ids {
            write_note(root.path(), note_id, &fixture_doc(note_id, 0, "failed"));
        }
        let second_before =
            std::fs::read(root.path().join("notes/two").join(store::AING_DOC_FILE)).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let executor = MaterializingExecutor {
            calls: Arc::clone(&calls),
            cancel_on_call: Some((2, Arc::clone(&cancel))),
            fail: false,
            invalid: false,
        };
        let mut progress = Vec::new();
        let rebuilds = AtomicUsize::new(0);
        let final_progress = run_batch(
            "run-cancel",
            &root.path().join("notes"),
            &note_ids,
            &approved_source_hashes(&root.path().join("notes"), &note_ids).unwrap(),
            &executor,
            &cancel,
            |event| progress.push(event),
            || {
                rebuilds.fetch_add(1, Ordering::SeqCst);
                Ok(41)
            },
        );
        assert_eq!(final_progress.state, "cancelled");
        assert_eq!(final_progress.run_id, "run-cancel");
        assert_eq!(final_progress.rebuild_generation, Some(41));
        assert_eq!(final_progress.completed, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(rebuilds.load(Ordering::SeqCst), 1);
        assert!(is_current(
            &store::load_refined(&root.path().join("notes/one")).unwrap()
        ));
        assert_eq!(
            std::fs::read(root.path().join("notes/two").join(store::AING_DOC_FILE)).unwrap(),
            second_before
        );
        assert_eq!(progress.last().unwrap().state, "cancelled");

        cancel.store(false, Ordering::SeqCst);
        let resume_calls = Arc::new(AtomicUsize::new(0));
        let resume_executor = MaterializingExecutor::success(Arc::clone(&resume_calls));
        let resumed = run_batch(
            "run-resume",
            &root.path().join("notes"),
            &note_ids,
            &approved_source_hashes(&root.path().join("notes"), &note_ids).unwrap(),
            &resume_executor,
            &cancel,
            |_| {},
            || Ok(42),
        );
        assert_eq!(resumed.state, "completed");
        assert_eq!(resumed.rebuild_generation, Some(42));
        assert_eq!(resumed.completed, 2);
        assert_eq!(resume_calls.load(Ordering::SeqCst), 1);
        assert!(is_current(
            &store::load_refined(&root.path().join("notes/two")).unwrap()
        ));
    }

    #[test]
    fn global_gate_rejects_overlap_and_resets_on_drop() {
        let running = Arc::new(AtomicBool::new(false));
        let active = Arc::new(std::sync::Mutex::new(None));
        let first =
            BackfillGate::acquire(Arc::clone(&running), Arc::clone(&active), "run-first").unwrap();
        assert!(
            BackfillGate::acquire(Arc::clone(&running), Arc::clone(&active), "run-second").is_err()
        );
        drop(first);
        assert!(BackfillGate::acquire(running, active, "run-second").is_ok());

        let running = Arc::new(AtomicBool::new(false));
        let active = Arc::new(std::sync::Mutex::new(None));
        let unwind_running = Arc::clone(&running);
        let unwind_active = Arc::clone(&active);
        let _ = std::panic::catch_unwind(move || {
            let _guard = BackfillGate::acquire(unwind_running, unwind_active, "run-panic").unwrap();
            panic!("simulated worker panic");
        });
        assert!(!running.load(Ordering::SeqCst));
        assert!(active.lock().unwrap().is_none());
    }

    #[test]
    fn write_failure_and_concurrent_revision_change_never_overwrite_the_prior_truth() {
        let root = tempfile::tempdir().unwrap();
        let write_failed_id = "write-failed";
        let concurrent_id = "concurrent";
        for note_id in [write_failed_id, concurrent_id] {
            write_note(root.path(), note_id, &fixture_doc(note_id, 0, "failed"));
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let executor = MaterializingExecutor::success(calls);
        let failed_dir = root.path().join("notes").join(write_failed_id);
        let old_bytes = std::fs::read(failed_dir.join(store::AING_DOC_FILE)).unwrap();
        let result = run_one_controlled_with_writer(
            &failed_dir,
            write_failed_id,
            None,
            &executor,
            &AtomicBool::new(false),
            |_, _, _| anyhow::bail!("disk full"),
        );
        assert!(matches!(result, Err(RunOneError::Failed(_))));
        assert_eq!(
            std::fs::read(failed_dir.join(store::AING_DOC_FILE)).unwrap(),
            old_bytes
        );

        struct ConcurrentEditExecutor {
            note_dir: std::path::PathBuf,
        }
        impl RelationExecutor for ConcurrentEditExecutor {
            fn provider(&self) -> &str {
                "test-provider"
            }
            fn model(&self) -> &str {
                "test-model"
            }
            fn extract(
                &self,
                note_id: &str,
                doc: &RefinedDoc,
            ) -> anyhow::Result<crate::store::aing_graph::ValidatedGraph> {
                let graph =
                    crate::refine::relations::materialize(note_id, doc, vec![raw_relation(0.93)])
                        .map_err(|issues| anyhow::anyhow!("{issues:?}"))?;
                let mut concurrent = store::load_refined(&self.note_dir).unwrap();
                concurrent.paragraphs[0].text.push_str("（用户编辑）");
                concurrent.paragraphs[0].source_seqs.push(99);
                store::write_refined_atomic(&self.note_dir, &concurrent).unwrap();
                Ok(graph)
            }
        }
        let concurrent_dir = root.path().join("notes").join(concurrent_id);
        let executor = ConcurrentEditExecutor {
            note_dir: concurrent_dir.clone(),
        };
        assert!(run_one(&concurrent_dir, concurrent_id, &executor).is_err());
        let latest = store::load_refined(&concurrent_dir).unwrap();
        assert!(latest.paragraphs[0].text.ends_with("（用户编辑）"));
        assert_eq!(latest.paragraphs[0].source_seqs.last(), Some(&99));
        assert_eq!(latest.graph_extraction.unwrap().run_id, "old-run");
    }

    #[test]
    fn terminal_progress_reports_failures_and_scheduler_failure_keeps_dirty_marker() {
        let root = tempfile::tempdir().unwrap();
        let note_id = "rebuild-dirty";
        write_note(root.path(), note_id, &fixture_doc(note_id, 0, "failed"));
        let scheduler = crate::graph::index::RebuildScheduler::with_rebuilder_and_spawner(
            |_| anyhow::bail!("must not run"),
            |_| Err(std::io::Error::other("spawn denied")),
        );
        let scheduler_calls = AtomicUsize::new(0);
        let events = std::cell::RefCell::new(Vec::new());
        let executor = MaterializingExecutor::success(Arc::new(AtomicUsize::new(0)));
        let terminal = run_batch(
            "run-dirty",
            &root.path().join("notes"),
            &[note_id.into()],
            &approved_source_hashes(&root.path().join("notes"), &[note_id.into()]).unwrap(),
            &executor,
            &AtomicBool::new(false),
            |progress| events.borrow_mut().push(progress),
            || {
                scheduler_calls.fetch_add(1, Ordering::SeqCst);
                scheduler
                    .request(root.path().to_path_buf(), |_| {})
                    .map(|generation| generation)
            },
        );
        assert_eq!(scheduler_calls.load(Ordering::SeqCst), 1);
        assert!(root.path().join(".graph-index-dirty").is_file());
        assert_eq!(terminal.state, "completed");
        assert_eq!((terminal.completed, terminal.total), (1, 1));
        assert!(terminal.failed.is_empty());
        assert!(terminal.index_error.as_deref().unwrap().contains("dirty"));
        assert_eq!(events.borrow().last().unwrap().state, "completed");
        assert_eq!(terminal.rebuild_generation, None);

        let terminal = run_batch(
            "run-rebuild-panic",
            &root.path().join("notes"),
            &[note_id.into()],
            &approved_source_hashes(&root.path().join("notes"), &[note_id.into()]).unwrap(),
            &executor,
            &AtomicBool::new(false),
            |_| {},
            || panic!("injected scheduler panic"),
        );
        assert_eq!(terminal.state, "completed");
        assert!(terminal.failed.is_empty());
        assert!(terminal.index_error.as_deref().unwrap().contains("异常退出"));

        let failed_id = "provider-failed";
        write_note(root.path(), failed_id, &fixture_doc(failed_id, 0, "failed"));
        let executor = MaterializingExecutor {
            calls: Arc::new(AtomicUsize::new(0)),
            cancel_on_call: None,
            fail: true,
            invalid: false,
        };
        let terminal = run_batch(
            "run-provider-failed",
            &root.path().join("notes"),
            &[failed_id.into()],
            &approved_source_hashes(&root.path().join("notes"), &[failed_id.into()]).unwrap(),
            &executor,
            &AtomicBool::new(false),
            |_| {},
            || anyhow::bail!("must not rebuild without a commit"),
        );
        assert_eq!(terminal.state, "failed");
        assert_eq!(terminal.rebuild_generation, None);
        assert_eq!(terminal.failed[0].note_id, failed_id);
        assert!(terminal.failed[0].error.contains("provider failed"));
    }

    #[test]
    fn committed_work_rebuilds_once_and_reports_active_note_when_next_executor_panics() {
        struct PanicOnSecond {
            calls: AtomicUsize,
        }

        impl RelationExecutor for PanicOnSecond {
            fn provider(&self) -> &str {
                "test-provider"
            }

            fn model(&self) -> &str {
                "test-model"
            }

            fn extract(
                &self,
                note_id: &str,
                doc: &RefinedDoc,
            ) -> anyhow::Result<crate::store::aing_graph::ValidatedGraph> {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
                    panic!("second provider panicked");
                }
                crate::refine::relations::materialize(note_id, doc, vec![raw_relation(0.93)])
                    .map_err(|issues| anyhow::anyhow!("{issues:?}"))
            }
        }

        let root = tempfile::tempdir().unwrap();
        let first = "panic-first";
        let second = "panic-second";
        write_note(root.path(), first, &fixture_doc(first, 0, "failed"));
        write_note(root.path(), second, &fixture_doc(second, 0, "failed"));
        let scheduler = crate::graph::index::RebuildScheduler::with_rebuilder_and_spawner(
            |_| anyhow::bail!("must not run"),
            |_| Err(std::io::Error::other("spawn denied")),
        );
        let rebuilds = AtomicUsize::new(0);
        let events = std::cell::RefCell::new(Vec::new());
        let terminal = run_batch(
            "run-panic",
            &root.path().join("notes"),
            &[first.into(), second.into()],
            &approved_source_hashes(
                &root.path().join("notes"),
                &[first.into(), second.into()],
            )
            .unwrap(),
            &PanicOnSecond {
                calls: AtomicUsize::new(0),
            },
            &AtomicBool::new(false),
            |progress| events.borrow_mut().push(progress),
            || {
                rebuilds.fetch_add(1, Ordering::SeqCst);
                scheduler
                    .request(root.path().to_path_buf(), |_| {})
                    .map(|generation| generation)
            },
        );

        assert!(is_current(
            &store::load_refined(&root.path().join("notes").join(first)).unwrap()
        ));
        assert!(!is_current(
            &store::load_refined(&root.path().join("notes").join(second)).unwrap()
        ));
        assert_eq!(rebuilds.load(Ordering::SeqCst), 1);
        assert!(root.path().join(".graph-index-dirty").is_file());
        assert_eq!(terminal.state, "failed");
        assert_eq!((terminal.completed, terminal.total), (1, 2));
        assert!(terminal
            .failed
            .iter()
            .any(|failure| failure.note_id == second && failure.error.contains("异常退出")));
        let events = events.into_inner();
        assert_eq!(events[0].current_note_id.as_deref(), Some(first));
        assert_eq!(events[0].completed, 0);
        assert!(events.iter().any(|progress| {
            progress.current_note_id.as_deref() == Some(second) && progress.completed == 1
        }));
        let last = events.last().unwrap();
        assert_eq!(last.state, terminal.state);
        assert_eq!(last.completed, terminal.completed);
        assert_eq!(last.total, terminal.total);
        assert_eq!(last.failed.len(), terminal.failed.len());
    }

    #[test]
    fn panic_progress_is_failed_unless_cancellation_was_requested() {
        let last = BackfillProgress {
            run_id: "run-panic-progress".into(),
            state: "running".into(),
            completed: 1,
            total: 3,
            current_note_id: Some("two".into()),
            failed: vec![],
            rebuild_generation: None,
            index_error: None,
        };
        let failed = panic_progress(last.clone(), false);
        assert_eq!(failed.state, "failed");
        assert_eq!((failed.completed, failed.total), (1, 3));
        assert_eq!(failed.failed.len(), 1);
        assert_eq!(failed.failed[0].note_id, "two");
        assert!(failed.current_note_id.is_none());

        let cancelled = panic_progress(last, true);
        assert_eq!(cancelled.state, "cancelled");
        assert_eq!((cancelled.completed, cancelled.total), (1, 3));
    }

    #[cfg(unix)]
    #[test]
    fn explicit_preview_rejects_symlinked_note_targets() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let note_id = "outside";
        write_note(outside.path(), note_id, &fixture_doc(note_id, 0, "failed"));
        std::fs::create_dir_all(root.path().join("notes")).unwrap();
        symlink(
            outside.path().join("notes").join(note_id),
            root.path().join("notes").join(note_id),
        )
        .unwrap();
        let mut settings = crate::settings::Settings::default();
        settings.refine_provider = "openai".into();
        settings.refine_model = "relation-model".into();
        assert!(preview(root.path(), &settings, Some(&[note_id.into()])).is_err());
    }
}
