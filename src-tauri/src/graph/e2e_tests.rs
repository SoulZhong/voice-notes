use super::canonical::{
    CanonicalEntity, CanonicalEvidence, CanonicalGraph, CanonicalMention, CanonicalRelation,
    RelationOrigin, RelationStatus,
};
use super::{index, overrides, query, resolve};
use crate::graph::query::GraphFilter;
use crate::ipc::{KnowledgeOperationInput, SplitEntityRequest};
use crate::refine::backfill::{self, RelationExecutor};
use crate::refine::llm::{RawEvidence, RawRelation};
use crate::store::{
    self, Entity, GraphExtraction, Mention, RefineStages, RefinedDoc, RefinedParagraph,
    RelationPredicate,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

fn predicate() -> RelationPredicate {
    RelationPredicate {
        kind: "uses".into(),
        label: None,
    }
}

fn relation_doc(note_id: &str, relations_state: &str) -> RefinedDoc {
    let text = "Alice uses Beacon";
    let mut doc = RefinedDoc {
        schema_version: store::refined::REFINED_SCHEMA_VERSION,
        generated_at: "2026-07-21T12:00:00+08:00".into(),
        llm_model: Some("fixture-model".into()),
        stages: RefineStages {
            filter: "done".into(),
            recluster: "done".into(),
            llm: "done".into(),
            entities: "done".into(),
            relations: relations_state.into(),
        },
        discarded_seqs: Vec::new(),
        entities: vec![
            Entity {
                id: "ent_alice".into(),
                kind: "person".into(),
                name: "Alice".into(),
                aliases: Vec::new(),
            },
            Entity {
                id: "ent_beacon".into(),
                kind: "project".into(),
                name: "Beacon".into(),
                aliases: Vec::new(),
            },
        ],
        graph_extraction: None,
        relations: Vec::new(),
        graph_support_mentions: Vec::new(),
        paragraphs: vec![RefinedParagraph {
            speaker: "R1".into(),
            name: None,
            person_id: None,
            start_ms: 0,
            end_ms: 1_000,
            text: text.into(),
            source_seqs: vec![1, 2],
            mentions: vec![
                Mention {
                    id: String::new(),
                    entity: "ent_alice".into(),
                    start: 0,
                    end: 5,
                },
                Mention {
                    id: String::new(),
                    entity: "ent_beacon".into(),
                    start: 11,
                    end: 17,
                },
            ],
        }],
    };
    store::ensure_graph_ids(note_id, &mut doc);
    doc
}

fn raw_relation() -> RawRelation {
    RawRelation {
        subject: "Alice".into(),
        predicate: predicate(),
        object: "Beacon".into(),
        confidence: 0.95,
        valid_from: None,
        valid_to: None,
        evidence: vec![RawEvidence {
            paragraph_index: 0,
            start: 0,
            end: 17,
            quote: "Alice uses Beacon".into(),
        }],
    }
}

fn write_note(root: &std::path::Path, note_id: &str, doc: &RefinedDoc) {
    let note_dir = root.join("notes").join(note_id);
    std::fs::create_dir_all(&note_dir).unwrap();
    store::write_refined_atomic(&note_dir, doc).unwrap();
}

fn fixture_graph() -> CanonicalGraph {
    let entities = BTreeMap::from([
        (
            "kg_alice".into(),
            CanonicalEntity {
                id: "kg_alice".into(),
                kind: "person".into(),
                name: "Alice".into(),
                aliases: Vec::new(),
                confirmed: true,
            },
        ),
        (
            "kg_beacon".into(),
            CanonicalEntity {
                id: "kg_beacon".into(),
                kind: "project".into(),
                name: "Beacon".into(),
                aliases: Vec::new(),
                confirmed: true,
            },
        ),
    ]);
    let mentions = vec![
        CanonicalMention {
            id: "mn_alice".into(),
            note_id: "note-1".into(),
            entity_id: "kg_alice".into(),
            paragraph_index: 0,
            start: 0,
            end: 5,
            quote: "Alice".into(),
        },
        CanonicalMention {
            id: "mn_beacon".into(),
            note_id: "note-1".into(),
            entity_id: "kg_beacon".into(),
            paragraph_index: 0,
            start: 11,
            end: 17,
            quote: "Beacon".into(),
        },
    ];
    let evidence = CanonicalEvidence {
        id: "ev_initial".into(),
        note_id: "note-1".into(),
        paragraph_index: 0,
        start: 0,
        end: 17,
        quote: "Alice uses Beacon".into(),
        source_seqs: vec![1, 2],
        source_hash: "fixture-source".into(),
        subject_mentions: vec!["mn_alice".into()],
        object_mentions: vec!["mn_beacon".into()],
    };
    CanonicalGraph {
        entities,
        mentions,
        relations: vec![CanonicalRelation {
            id: "cr_initial".into(),
            subject_id: "kg_alice".into(),
            predicate: predicate(),
            object_id: "kg_beacon".into(),
            confidence: 0.95,
            valid_from: None,
            valid_to: None,
            status: RelationStatus::Current,
            origin: RelationOrigin::Model,
            provider: Some("openai".into()),
            model: Some("fixture-model".into()),
            note_ids: vec!["note-1".into()],
            evidence: vec![evidence],
        }],
        pending: Vec::new(),
    }
}

fn fixture_ledger() -> overrides::KnowledgeLedger {
    overrides::KnowledgeLedger {
        schema_version: 1,
        registry: BTreeMap::from([
            (
                "kg_alice".into(),
                overrides::RegistryEntity {
                    kind: "person".into(),
                    name: "Alice".into(),
                    aliases: Vec::new(),
                    status: "confirmed".into(),
                },
            ),
            (
                "kg_beacon".into(),
                overrides::RegistryEntity {
                    kind: "project".into(),
                    name: "Beacon".into(),
                    aliases: Vec::new(),
                    status: "confirmed".into(),
                },
            ),
        ]),
        legacy_ids: BTreeMap::from([("e:alice".into(), "kg_alice".into())]),
        operations: Vec::new(),
    }
}

fn install_fixture(root: &std::path::Path) {
    std::fs::write(
        root.join(overrides::KNOWLEDGE_FILE),
        serde_json::to_vec(&fixture_ledger()).unwrap(),
    )
    .unwrap();
    index::rebuild_atomic(root, &fixture_graph()).unwrap();
}

fn filter() -> GraphFilter {
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
fn schema_v1_and_legacy_ids_bootstrap_without_rewriting_transcript() {
    let root = tempfile::tempdir().unwrap();
    {
        let connection = super::open(root.path()).unwrap();
        connection
            .execute(
                "INSERT INTO entities(id, kind, name, aliases, is_person) VALUES(?1, ?2, ?3, '[]', 0)",
                rusqlite::params!["e:beacon", "project", "Beacon"],
            )
            .unwrap();
    }
    let note_dir = root.path().join("notes/n1");
    std::fs::create_dir_all(&note_dir).unwrap();
    let legacy = serde_json::json!({
        "schema_version": 1,
        "generated_at": "2026-07-21T12:00:00+08:00",
        "stages": {"filter":"done","recluster":"done","llm":"done"},
        "discarded_seqs": [],
        "entities": [{"id":"ent_beacon","kind":"project","name":"Beacon"}],
        "paragraphs": [{
            "speaker":"R1","start_ms":0,"end_ms":1000,"text":"Beacon ships",
            "source_seqs":[1],"mentions":[{"entity":"ent_beacon","start":0,"end":6}]
        }]
    });
    std::fs::write(
        note_dir.join(store::refined::AING_DOC_FILE),
        serde_json::to_vec_pretty(&legacy).unwrap(),
    )
    .unwrap();
    let before = std::fs::read(note_dir.join(store::refined::AING_DOC_FILE)).unwrap();

    index::rebuild_from_sources(root.path()).unwrap();
    let ledger = overrides::load(root.path()).unwrap();
    let stable = ledger.legacy_ids["e:beacon"].clone();
    assert!(stable.starts_with("kg_"));
    assert_eq!(ledger.legacy_ids["n1/ent_beacon"], stable);
    assert_eq!(
        std::fs::read(note_dir.join(store::refined::AING_DOC_FILE)).unwrap(),
        before
    );
    let indexed: String = index::open_readonly(root.path())
        .unwrap()
        .query_row("SELECT id FROM entities WHERE name = 'Beacon'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(indexed, stable);
}

#[test]
fn governance_operations_survive_reaing_and_two_rebuilds() {
    let root = tempfile::tempdir().unwrap();
    install_fixture(root.path());
    let rename = query::apply_operation(
        root.path(),
        &KnowledgeOperationInput::RenameEntity {
            entity_id: "kg_alice".into(),
            name: "Alice Chen".into(),
        },
    )
    .unwrap();
    let alias = query::apply_operation(
        root.path(),
        &KnowledgeOperationInput::AddAlias {
            entity_id: "kg_alice".into(),
            alias: "A. Chen".into(),
        },
    )
    .unwrap();
    let created = query::apply_operation(
        root.path(),
        &KnowledgeOperationInput::CreateEntity {
            kind: "project".into(),
            name: "Temporary Beacon".into(),
            aliases: Vec::new(),
        },
    )
    .unwrap();
    let created_id = created.entity_id.clone().unwrap();
    let merged = query::merge_operation(root.path(), &created_id, "kg_beacon").unwrap();
    let split = query::split_operation(
        root.path(),
        &SplitEntityRequest {
            entity_id: "kg_alice".into(),
            name: "Alice Evidence Split".into(),
            kind: None,
            aliases: Vec::new(),
            mention_ids: vec!["mn_alice".into()],
        },
    )
    .unwrap();
    let suppress = query::apply_operation(
        root.path(),
        &KnowledgeOperationInput::SuppressRelation {
            subject_id: "kg_alice".into(),
            predicate: predicate(),
            object_id: "kg_beacon".into(),
        },
    )
    .unwrap();
    let restored = query::apply_operation(
        root.path(),
        &KnowledgeOperationInput::RestoreRelation {
            operation_id: suppress.operation_id.clone(),
        },
    )
    .unwrap();
    let ended = query::apply_operation(
        root.path(),
        &KnowledgeOperationInput::EndRelation {
            relation_id: "cr_initial".into(),
            valid_to: "2026-07-21T12:30:00+08:00".into(),
        },
    )
    .unwrap();
    let manual = query::apply_operation(
        root.path(),
        &KnowledgeOperationInput::CreateRelation {
            subject_id: "kg_alice".into(),
            predicate: RelationPredicate {
                kind: "responsible_for".into(),
                label: None,
            },
            object_id: "kg_beacon".into(),
            valid_from: None,
            valid_to: None,
            note: Some("explicit fixture assertion".into()),
            evidence_ids: Vec::new(),
            user_assertion: true,
        },
    )
    .unwrap();
    let undo = query::undo_operation(root.path(), &alias.operation_id).unwrap();

    // Simulated re-Aing replaces only model provenance/facts; the ledger remains
    // byte-for-byte human truth across both complete source rebuilds.
    let mut doc = relation_doc("note-reaing", "done");
    let validated =
        crate::refine::relations::materialize("note-reaing", &doc, vec![raw_relation()]).unwrap();
    let reaing_source_hash = store::source_hash(&doc.paragraphs);
    crate::refine::relations::apply_validated_graph(
        &mut doc,
        GraphExtraction {
            contract_version: store::aing_graph::GRAPH_CONTRACT_VERSION,
            provider: "agent".into(),
            model: "fixture-agent-model".into(),
            run_id: "run_reaing".into(),
            generated_at: "2026-07-21T13:00:00+08:00".into(),
            source_hash: reaing_source_hash,
            mode: "reaing".into(),
        },
        validated,
    );
    write_note(root.path(), "note-reaing", &doc);
    let decisions_before = overrides::load(root.path()).unwrap().operations;
    index::rebuild_from_sources(root.path()).unwrap();
    index::rebuild_from_sources(root.path()).unwrap();

    let ledger = overrides::load(root.path()).unwrap();
    assert!(ledger.operations.starts_with(&decisions_before));
    let snapshot = resolve::replay(&ledger).unwrap();
    assert_eq!(snapshot.registry["kg_alice"].name, "Alice Chen");
    assert!(!snapshot.registry["kg_alice"]
        .aliases
        .contains(&"A. Chen".into()));
    assert_eq!(snapshot.redirects[&created_id], "kg_beacon");
    assert_eq!(
        snapshot.mention_bindings["mn_alice"],
        split.entity_id.unwrap()
    );
    assert!(snapshot
        .relation_decisions
        .restored_operations
        .contains(&suppress.operation_id));
    assert!(snapshot.relation_decisions.ended.contains_key("cr_initial"));
    assert!(!snapshot.relation_decisions.created.is_empty());
    for operation_id in [
        rename.operation_id,
        merged.operation_id,
        restored.operation_id,
        ended.operation_id,
        manual.operation_id,
        undo.operation_id,
    ] {
        assert!(ledger
            .operations
            .iter()
            .any(|operation| operation.id == operation_id));
    }
}

struct FixtureExecutor {
    calls: Arc<AtomicUsize>,
}

impl RelationExecutor for FixtureExecutor {
    fn provider(&self) -> &str {
        "agent"
    }

    fn model(&self) -> &str {
        "fixture-agent-model"
    }

    fn extract(
        &self,
        note_id: &str,
        doc: &RefinedDoc,
    ) -> anyhow::Result<store::aing_graph::ValidatedGraph> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        crate::refine::relations::materialize(note_id, doc, vec![raw_relation()])
            .map_err(|issues| anyhow::anyhow!(serde_json::to_string(&issues).unwrap()))
    }
}

#[test]
fn corrupt_ledger_keeps_prior_index_and_rejects_writes() {
    let root = tempfile::tempdir().unwrap();
    install_fixture(root.path());
    write_note(
        root.path(),
        "backfill-note",
        &relation_doc("backfill-note", "failed"),
    );
    std::fs::write(root.path().join(overrides::KNOWLEDGE_FILE), b"{corrupt").unwrap();

    let readable = query::semantic_graph(root.path(), &filter()).unwrap();
    assert_eq!(readable.semantic_edges.len(), 1);
    assert!(readable.degraded);
    assert!(query::apply_operation(
        root.path(),
        &KnowledgeOperationInput::RenameEntity {
            entity_id: "kg_alice".into(),
            name: "Rejected".into(),
        },
    )
    .is_err());
    let calls = Arc::new(AtomicUsize::new(0));
    let executor = FixtureExecutor {
        calls: calls.clone(),
    };
    assert!(backfill::run_one(
        &root.path().join("notes/backfill-note"),
        "backfill-note",
        &executor,
    )
    .is_err());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "corrupt ledger must reject before model execution"
    );
}

#[test]
fn failed_next_build_keeps_prior_index_readable() {
    let root = tempfile::tempdir().unwrap();
    install_fixture(root.path());
    let before = query::semantic_graph(root.path(), &filter()).unwrap();
    let mut changed = fixture_graph();
    changed.entities.get_mut("kg_alice").unwrap().name = "Changed".into();
    let error = index::rebuild_atomic_fail_before_publish(root.path(), &changed).unwrap_err();
    assert!(error
        .to_string()
        .contains("injected .next publication failure"));
    let after = query::semantic_graph(root.path(), &filter()).unwrap();
    assert_eq!(
        after
            .nodes
            .iter()
            .map(|node| (&node.id, &node.name))
            .collect::<Vec<_>>(),
        before
            .nodes
            .iter()
            .map(|node| (&node.id, &node.name))
            .collect::<Vec<_>>()
    );
    assert_eq!(after.semantic_edges.len(), before.semantic_edges.len());
    assert!(!root.path().join("graph.sqlite.next").exists());
}

#[test]
fn http_and_agent_normalization_have_evidence_and_provenance_parity() {
    let doc = relation_doc("parity-note", "failed");
    let http =
        crate::refine::relations::materialize("parity-note", &doc, vec![raw_relation()]).unwrap();
    let agent =
        crate::refine::relations::materialize("parity-note", &doc, vec![raw_relation()]).unwrap();
    assert_eq!(http, agent);
    assert_eq!(http.relations[0].evidence[0].quote, "Alice uses Beacon");
    assert_eq!(http.relations[0].evidence[0].source_seqs, vec![1, 2]);

    let mut http_doc = doc.clone();
    let mut agent_doc = doc.clone();
    for (target, provider, model, graph) in [
        (&mut http_doc, "openai", "http-exact-model", http),
        (&mut agent_doc, "agent", "agent-exact-model", agent),
    ] {
        crate::refine::relations::apply_validated_graph(
            target,
            GraphExtraction {
                contract_version: store::aing_graph::GRAPH_CONTRACT_VERSION,
                provider: provider.into(),
                model: model.into(),
                run_id: format!("run-{provider}"),
                generated_at: "2026-07-21T12:00:00+08:00".into(),
                source_hash: store::source_hash(&target.paragraphs),
                mode: "fixture".into(),
            },
            graph,
        );
    }
    assert_eq!(http_doc.relations, agent_doc.relations);
    assert_eq!(
        http_doc.graph_extraction.as_ref().unwrap().provider,
        "openai"
    );
    assert_eq!(
        agent_doc.graph_extraction.as_ref().unwrap().provider,
        "agent"
    );
    assert_eq!(
        http_doc.graph_extraction.as_ref().unwrap().model,
        "http-exact-model"
    );
    assert_eq!(
        agent_doc.graph_extraction.as_ref().unwrap().model,
        "agent-exact-model"
    );
}

#[test]
fn backfill_changes_only_relation_artifacts() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join(overrides::KNOWLEDGE_FILE),
        serde_json::to_vec(&overrides::KnowledgeLedger::empty()).unwrap(),
    )
    .unwrap();
    let note_id = "backfill-preserves-text";
    let doc = relation_doc(note_id, "failed");
    write_note(root.path(), note_id, &doc);
    let before = store::load_refined(&root.path().join("notes").join(note_id)).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let outcome = backfill::run_one(
        &root.path().join("notes").join(note_id),
        note_id,
        &FixtureExecutor { calls },
    )
    .unwrap();
    let after = store::load_refined(&root.path().join("notes").join(note_id)).unwrap();
    assert!(outcome.committed);
    assert_eq!(
        before
            .paragraphs
            .iter()
            .map(|paragraph| (&paragraph.text, &paragraph.source_seqs))
            .collect::<Vec<_>>(),
        after
            .paragraphs
            .iter()
            .map(|paragraph| (&paragraph.text, &paragraph.source_seqs))
            .collect::<Vec<_>>()
    );
    assert_eq!(before.paragraphs.len(), after.paragraphs.len());
    assert_eq!(after.relations.len(), 1);
    assert_eq!(after.graph_extraction.as_ref().unwrap().provider, "agent");
}

#[test]
fn queued_mutations_keep_generation_correlated_rebuild_events() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let build_gate = gate.clone();
    let builds = Arc::new(AtomicUsize::new(0));
    let build_count = builds.clone();
    let scheduler = index::RebuildScheduler::with_rebuilder(move |_| {
        if build_count.fetch_add(1, Ordering::SeqCst) == 0 {
            let (lock, condition) = &*build_gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = condition.wait(released).unwrap();
            }
        }
        Ok(index::BuildStats::default())
    });
    let root = tempfile::tempdir().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let first = scheduler
        .request(root.path().to_path_buf(), {
            let tx = tx.clone();
            move |status| tx.send(status).unwrap()
        })
        .unwrap();
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap().generation,
        first
    );
    let queued = scheduler
        .request(root.path().to_path_buf(), {
            let tx = tx.clone();
            move |status| tx.send(status).unwrap()
        })
        .unwrap();
    assert!(queued > first);
    {
        let (lock, condition) = &*gate;
        *lock.lock().unwrap() = true;
        condition.notify_all();
    }
    let statuses = (0..3)
        .map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        statuses
            .iter()
            .map(|status| (status.state.as_str(), status.generation))
            .collect::<Vec<_>>(),
        vec![("ready", first), ("building", queued), ("ready", queued)]
    );
    assert_eq!(builds.load(Ordering::SeqCst), 2);
}
