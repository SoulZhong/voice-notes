#[allow(unused_imports)] // Public request contract is consumed through ipc.rs today.
pub use crate::ipc::{BackfillFailure, BackfillPreview, BackfillProgress, BackfillRequest};
use crate::settings::Settings;
use crate::store::aing_graph::{ValidatedGraph, GRAPH_CONTRACT_VERSION};
use crate::store::{GraphExtraction, RefinedDoc};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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

pub(crate) struct BackfillGate {
    running: Arc<AtomicBool>,
}

impl BackfillGate {
    pub(crate) fn acquire(running: Arc<AtomicBool>) -> anyhow::Result<Self> {
        running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| anyhow::anyhow!("关系补建已在运行"))?;
        Ok(Self { running })
    }
}

impl Drop for BackfillGate {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
    }
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

/// 纯只读选择：不迁移旧稿、不写 stage、不触发索引。
pub fn preview(
    data_root: &Path,
    settings: &Settings,
    requested: Option<&[String]>,
) -> anyhow::Result<BackfillPreview> {
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
                        note_ids: selected,
                        provider: provider.into(),
                        model: model.into(),
                        contract_version: GRAPH_CONTRACT_VERSION,
                    });
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
    executor: &dyn RelationExecutor,
    cancel: &AtomicBool,
) -> Result<BackfillOutcome, RunOneError> {
    run_one_controlled_with_writer(
        note_dir,
        note_id,
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
    run_one_controlled(note_dir, note_id, executor, cancel).map_err(RunOneError::into_anyhow)
}

#[allow(dead_code)] // Public Task 9 contract; batch execution uses the cancellable core directly.
pub fn run_one(
    note_dir: &Path,
    note_id: &str,
    executor: &dyn RelationExecutor,
) -> anyhow::Result<BackfillOutcome> {
    run_one_controlled(note_dir, note_id, executor, &AtomicBool::new(false))
        .map_err(RunOneError::into_anyhow)
}

pub(crate) fn run_batch(
    notes_root: &Path,
    note_ids: &[String],
    executor: &dyn RelationExecutor,
    cancel: &AtomicBool,
    mut emit: impl FnMut(BackfillProgress),
    mut request_rebuild: impl FnMut() -> anyhow::Result<()>,
) -> BackfillProgress {
    let total = note_ids.len();
    let mut completed = 0usize;
    let mut committed = 0usize;
    let mut failed = Vec::new();
    let mut cancelled = false;
    let mut panicked = false;
    for note_id in note_ids {
        emit(BackfillProgress {
            state: "running".into(),
            completed,
            total,
            current_note_id: Some(note_id.clone()),
            failed: failed.clone(),
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_one_controlled(&notes_root.join(note_id), note_id, executor, cancel)
        }));
        match result {
            Ok(Ok(outcome)) => {
                completed += 1;
                committed += usize::from(outcome.committed);
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
            state: "running".into(),
            completed,
            total,
            current_note_id: Some(note_id.clone()),
            failed: failed.clone(),
        });
        if cancelled || panicked {
            break;
        }
    }
    if committed > 0 {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut request_rebuild)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failed.push(BackfillFailure {
                note_id: String::new(),
                error: format!("索引排队失败，已保留 dirty 标记供重试:{error:#}"),
            }),
            Err(_) => {
                panicked = true;
                failed.push(BackfillFailure {
                    note_id: String::new(),
                    error: "索引排队异常退出，dirty 标记状态未知".into(),
                });
            }
        }
    }
    let terminal = BackfillProgress {
        state: if cancelled {
            "cancelled"
        } else if panicked {
            "failed"
        } else {
            "completed"
        }
        .into(),
        completed,
        total,
        current_note_id: None,
        failed,
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
            &root.path().join("notes"),
            &note_ids,
            &executor,
            &cancel,
            |event| progress.push(event),
            || {
                rebuilds.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );
        assert_eq!(final_progress.state, "cancelled");
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
            &root.path().join("notes"),
            &note_ids,
            &resume_executor,
            &cancel,
            |_| {},
            || Ok(()),
        );
        assert_eq!(resumed.state, "completed");
        assert_eq!(resumed.completed, 2);
        assert_eq!(resume_calls.load(Ordering::SeqCst), 1);
        assert!(is_current(
            &store::load_refined(&root.path().join("notes/two")).unwrap()
        ));
    }

    #[test]
    fn global_gate_rejects_overlap_and_resets_on_drop() {
        let running = Arc::new(AtomicBool::new(false));
        let first = BackfillGate::acquire(Arc::clone(&running)).unwrap();
        assert!(BackfillGate::acquire(Arc::clone(&running)).is_err());
        drop(first);
        assert!(BackfillGate::acquire(running).is_ok());

        let running = Arc::new(AtomicBool::new(false));
        let unwind_running = Arc::clone(&running);
        let _ = std::panic::catch_unwind(move || {
            let _guard = BackfillGate::acquire(unwind_running).unwrap();
            panic!("simulated worker panic");
        });
        assert!(!running.load(Ordering::SeqCst));
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
            &root.path().join("notes"),
            &[note_id.into()],
            &executor,
            &AtomicBool::new(false),
            |progress| events.borrow_mut().push(progress),
            || {
                scheduler_calls.fetch_add(1, Ordering::SeqCst);
                scheduler
                    .request(root.path().to_path_buf(), |_| {})
                    .map(|_| ())
            },
        );
        assert_eq!(scheduler_calls.load(Ordering::SeqCst), 1);
        assert!(root.path().join(".graph-index-dirty").is_file());
        assert_eq!(terminal.state, "completed");
        assert_eq!((terminal.completed, terminal.total), (1, 1));
        assert_eq!(terminal.failed.len(), 1);
        assert!(terminal.failed[0].error.contains("dirty"));
        assert_eq!(events.borrow().last().unwrap().state, "completed");

        let failed_id = "provider-failed";
        write_note(root.path(), failed_id, &fixture_doc(failed_id, 0, "failed"));
        let executor = MaterializingExecutor {
            calls: Arc::new(AtomicUsize::new(0)),
            cancel_on_call: None,
            fail: true,
            invalid: false,
        };
        let terminal = run_batch(
            &root.path().join("notes"),
            &[failed_id.into()],
            &executor,
            &AtomicBool::new(false),
            |_| {},
            || anyhow::bail!("must not rebuild without a commit"),
        );
        assert_eq!(terminal.state, "completed");
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
            &root.path().join("notes"),
            &[first.into(), second.into()],
            &PanicOnSecond {
                calls: AtomicUsize::new(0),
            },
            &AtomicBool::new(false),
            |progress| events.borrow_mut().push(progress),
            || {
                rebuilds.fetch_add(1, Ordering::SeqCst);
                scheduler
                    .request(root.path().to_path_buf(), |_| {})
                    .map(|_| ())
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
            state: "running".into(),
            completed: 1,
            total: 3,
            current_note_id: Some("two".into()),
            failed: vec![],
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
