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
    RelationEvidence, RelationFact, RelationPredicate,
};
use std::collections::BTreeMap;
use std::io::{Read, Write};
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
                id: "ent_1".into(),
                kind: "person".into(),
                name: "Alice".into(),
                aliases: Vec::new(),
            },
            Entity {
                id: "ent_2".into(),
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
                    entity: "ent_1".into(),
                    start: 0,
                    end: 5,
                },
                Mention {
                    id: String::new(),
                    entity: "ent_2".into(),
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

fn relation_http_server(payload: serde_json::Value) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let count = stream.read(&mut chunk).unwrap();
            if count == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..count]);
            let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if request.len() >= header_end + 4 + content_length {
                break;
            }
        }
        let content = payload.to_string();
        let envelope = serde_json::json!({
            "choices": [{ "message": { "content": content } }]
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            envelope.len(),
            envelope
        );
        stream.write_all(response.as_bytes()).unwrap();
    });
    format!("http://{address}")
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
    let note_id = "note-reaing";
    let mut initial_ledger = fixture_ledger();
    initial_ledger
        .legacy_ids
        .insert(format!("{note_id}/ent_1"), "kg_alice".into());
    initial_ledger
        .legacy_ids
        .insert(format!("{note_id}/ent_2"), "kg_beacon".into());
    std::fs::write(
        root.path().join(overrides::KNOWLEDGE_FILE),
        serde_json::to_vec(&initial_ledger).unwrap(),
    )
    .unwrap();
    let mut original_source = relation_doc(note_id, "failed");
    let original_graph =
        crate::refine::relations::materialize(note_id, &original_source, vec![raw_relation()])
            .unwrap();
    let original_source_hash = store::source_hash(&original_source.paragraphs);
    crate::refine::relations::apply_validated_graph(
        &mut original_source,
        GraphExtraction {
            contract_version: store::aing_graph::GRAPH_CONTRACT_VERSION,
            provider: "openai".into(),
            model: "fixture-http-v1".into(),
            run_id: "run-before-reaing".into(),
            generated_at: "2026-07-21T12:00:00+08:00".into(),
            source_hash: original_source_hash,
            mode: "http".into(),
        },
        original_graph,
    );
    write_note(root.path(), note_id, &original_source);
    index::rebuild_from_sources(root.path()).unwrap();
    let indexed_before = query::semantic_graph(root.path(), &filter()).unwrap();
    let model_relation_id = indexed_before
        .semantic_edges
        .iter()
        .find(|relation| relation.predicate_type == "uses")
        .unwrap()
        .id
        .clone();
    assert_eq!(
        query::relation_detail(root.path(), &model_relation_id)
            .unwrap()
            .unwrap()
            .provider
            .as_deref(),
        Some("openai")
    );
    let alice_mention_id = query::entity_mentions(root.path(), "kg_alice")
        .unwrap()
        .first()
        .unwrap()
        .id
        .clone();
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
            mention_ids: vec![alice_mention_id.clone()],
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
    // Split/merge decisions can change the projected canonical relation ID.
    // End the relation by the ID that a rebuilt index actually exposes.
    index::rebuild_from_sources(root.path()).unwrap();
    let projected_relation_id = query::semantic_graph(root.path(), &filter())
        .unwrap()
        .semantic_edges
        .into_iter()
        .find(|relation| relation.predicate_type == "uses")
        .unwrap()
        .id;
    let ended = query::apply_operation(
        root.path(),
        &KnowledgeOperationInput::EndRelation {
            relation_id: projected_relation_id.clone(),
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

    // A real same-note Aing replacement changes provider/model/run provenance
    // while keeping the source identity. Human governance must be projected
    // into both complete source rebuilds rather than merely surviving in JSON.
    let mut doc = relation_doc(note_id, "done");
    let validated =
        crate::refine::relations::materialize(note_id, &doc, vec![raw_relation()]).unwrap();
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
    write_note(root.path(), note_id, &doc);
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
    let split_id = split.entity_id.unwrap();
    assert_eq!(snapshot.mention_bindings[&alice_mention_id], split_id);
    assert!(snapshot
        .relation_decisions
        .restored_operations
        .contains(&suppress.operation_id));
    assert!(snapshot
        .relation_decisions
        .ended
        .contains_key(&projected_relation_id));
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

    let renamed = query::semantic_entity_detail(root.path(), "kg_alice", &filter())
        .unwrap()
        .unwrap();
    assert_eq!(renamed.name, "Alice Chen");
    assert!(!renamed.aliases.contains(&"A. Chen".into()));

    let redirected = query::semantic_entity_detail(root.path(), &created_id, &filter())
        .unwrap()
        .unwrap();
    assert_eq!(redirected.id, "kg_beacon");
    let split_mentions = query::entity_mentions(root.path(), &split_id).unwrap();
    assert_eq!(
        split_mentions
            .iter()
            .map(|mention| mention.id.as_str())
            .collect::<Vec<_>>(),
        vec![alice_mention_id.as_str()]
    );

    let rebuilt = query::semantic_graph(root.path(), &filter()).unwrap();
    let ended_relation = rebuilt
        .semantic_edges
        .iter()
        .find(|relation| relation.predicate_type == "uses")
        .unwrap();
    assert_eq!(ended_relation.status, "historical");
    assert_eq!(
        ended_relation.valid_to.as_deref(),
        Some("2026-07-21T12:30:00+08:00")
    );
    let restored_detail = query::relation_detail(root.path(), &ended_relation.id)
        .unwrap()
        .unwrap();
    assert_eq!(restored_detail.provider.as_deref(), Some("agent"));
    assert_eq!(
        restored_detail.model.as_deref(),
        Some("fixture-agent-model")
    );
    assert_eq!(restored_detail.evidence.len(), 1);
    assert_eq!(restored_detail.evidence[0].note_id, note_id);

    let manual_relation = rebuilt
        .semantic_edges
        .iter()
        .find(|relation| relation.predicate_type == "responsible_for")
        .unwrap();
    assert_eq!(manual_relation.origin, "user_assertion");
    assert_eq!(
        query::relation_detail(root.path(), &manual_relation.id)
            .unwrap()
            .unwrap()
            .provider,
        None
    );
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

#[cfg(unix)]
#[test]
fn http_and_agent_executors_parse_real_boundaries_with_evidence_and_provenance_parity() {
    use crate::refine::agent::{AgentKind, AgentRelationExecutor};
    use crate::refine::llm::{HttpRelationExecutor, LlmConfig};
    use std::os::unix::fs::PermissionsExt;

    let note_id = "parity-note";
    let doc = relation_doc("parity-note", "failed");
    let raw_payload = serde_json::json!({
        "relations": [{
            "subject": "Alice",
            "predicate": { "type": "uses" },
            "object": "Beacon",
            "confidence": 0.95,
            "valid_from": null,
            "valid_to": null,
            "evidence": [{
                "paragraph_index": 0,
                "start": 0,
                "end": 17,
                "quote": "Alice uses Beacon"
            }]
        }]
    });
    let http_model = "http-exact-model";
    let http_executor = HttpRelationExecutor::new(LlmConfig {
        base_url: relation_http_server(raw_payload),
        model: http_model.into(),
        api_key: "fixture-key".into(),
    })
    .unwrap();
    let http = RelationExecutor::extract(&http_executor, note_id, &doc).unwrap();

    // Prepare the exact disk state the production MCP writer creates. The fake
    // CLI only copies this payload into the executor's random isolated root;
    // AgentRelationExecutor still owns process spawning, disk reload, provenance
    // checks and final graph validation.
    let agent_model = "agent-exact-model";
    let source_hash = store::source_hash(&doc.paragraphs);
    let submitted_entities = vec![
        Entity {
            id: "submitted-alice".into(),
            kind: "person".into(),
            name: "Alice".into(),
            aliases: Vec::new(),
        },
        Entity {
            id: "submitted-beacon".into(),
            kind: "project".into(),
            name: "Beacon".into(),
            aliases: Vec::new(),
        },
    ];
    let submitted_relations = vec![RelationFact {
        id: "untrusted-relation-id".into(),
        subject: "submitted-alice".into(),
        predicate: predicate(),
        object: "submitted-beacon".into(),
        subject_mentions: Vec::new(),
        object_mentions: Vec::new(),
        confidence: 0.95,
        valid_from: None,
        valid_to: None,
        evidence: vec![RelationEvidence {
            id: "untrusted-evidence-id".into(),
            paragraph_index: 0,
            start: 0,
            end: 17,
            quote: "Alice uses Beacon".into(),
            source_seqs: vec![1, 2],
            source_hash,
        }],
    }];
    let mut agent_written = doc.clone();
    crate::mcp::tools::normalize_agent_graph(
        note_id,
        &mut agent_written,
        &crate::mcp::server::ApplyAingGraphParams {
            note_id: note_id.into(),
            entities: submitted_entities,
            relations: submitted_relations,
            contract_version: store::aing_graph::GRAPH_CONTRACT_VERSION,
            model: agent_model.into(),
        },
    )
    .unwrap();

    let fake = tempfile::tempdir().unwrap();
    let output = fake.path().join("agent-output.json");
    std::fs::write(&output, serde_json::to_vec(&agent_written).unwrap()).unwrap();
    let bin = fake.path().join("fake-agent");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\ncp '{}' \"$VN_APP_DATA/notes/{note_id}/aing.json\"\n",
            output.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
    let agent_executor =
        AgentRelationExecutor::new(AgentKind::Claude, bin.to_str().unwrap(), agent_model).unwrap();
    let agent = RelationExecutor::extract(&agent_executor, note_id, &doc).unwrap();

    let relation_signature = |graph: &store::aing_graph::ValidatedGraph| {
        let relation = &graph.relations[0];
        (
            relation.predicate.clone(),
            relation.confidence,
            relation.evidence[0].paragraph_index,
            relation.evidence[0].start,
            relation.evidence[0].end,
            relation.evidence[0].quote.clone(),
            relation.evidence[0].source_seqs.clone(),
            relation.evidence[0].source_hash.clone(),
        )
    };
    assert_eq!(relation_signature(&http), relation_signature(&agent));
    assert_eq!(http.relations[0].evidence[0].source_seqs, vec![1, 2]);
    assert_eq!(
        agent_written.graph_extraction.as_ref().unwrap().provider,
        "agent"
    );
    assert_eq!(
        agent_written.graph_extraction.as_ref().unwrap().model,
        agent_model
    );
    assert_eq!(RelationExecutor::provider(&http_executor), "openai");
    assert_eq!(RelationExecutor::model(&http_executor), http_model);
    assert_eq!(RelationExecutor::provider(&agent_executor), "agent");
    assert_eq!(RelationExecutor::model(&agent_executor), agent_model);
}

#[test]
fn multi_note_multi_paragraph_backfill_preserves_transcript_order() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join(overrides::KNOWLEDGE_FILE),
        serde_json::to_vec(&overrides::KnowledgeLedger::empty()).unwrap(),
    )
    .unwrap();
    let note_ids = vec![
        "backfill-order-a".to_string(),
        "backfill-order-b".to_string(),
    ];
    for (note_index, note_id) in note_ids.iter().enumerate() {
        let mut doc = relation_doc(note_id, "failed");
        doc.paragraphs.extend([
            RefinedParagraph {
                speaker: "R2".into(),
                name: Some(format!("Speaker {note_index}")),
                person_id: None,
                start_ms: 1_001,
                end_ms: 2_000,
                text: format!("Context paragraph for {note_id}"),
                source_seqs: vec![10 + note_index as u64, 20 + note_index as u64],
                mentions: Vec::new(),
            },
            RefinedParagraph {
                speaker: "R1".into(),
                name: None,
                person_id: None,
                start_ms: 2_001,
                end_ms: 3_000,
                text: format!("Closing paragraph for {note_id}"),
                source_seqs: vec![30 + note_index as u64],
                mentions: Vec::new(),
            },
        ]);
        write_note(root.path(), note_id, &doc);
    }
    let before = note_ids
        .iter()
        .map(|note_id| store::load_refined(&root.path().join("notes").join(note_id)).unwrap())
        .collect::<Vec<_>>();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut progress = Vec::new();
    let terminal = backfill::run_batch(
        "run-multi-note-order",
        &root.path().join("notes"),
        &note_ids,
        &backfill::approved_source_hashes(&root.path().join("notes"), &note_ids).unwrap(),
        &FixtureExecutor { calls },
        &std::sync::atomic::AtomicBool::new(false),
        |event| progress.push(event),
        || {
            index::rebuild_from_sources(root.path())?;
            Ok(41)
        },
    );
    let after = note_ids
        .iter()
        .map(|note_id| store::load_refined(&root.path().join("notes").join(note_id)).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(terminal.state, "completed");
    assert_eq!(terminal.rebuild_generation, Some(41));
    assert_eq!(
        progress
            .iter()
            .filter_map(|event| event.current_note_id.as_deref())
            .collect::<Vec<_>>(),
        vec![
            "backfill-order-a",
            "backfill-order-a",
            "backfill-order-b",
            "backfill-order-b"
        ]
    );
    for (before, after) in before.iter().zip(&after) {
        assert_eq!(
            serde_json::to_value(&before.paragraphs).unwrap(),
            serde_json::to_value(&after.paragraphs).unwrap()
        );
        assert_eq!(after.paragraphs.len(), 3);
        assert_eq!(after.relations.len(), 1);
        assert_eq!(after.graph_extraction.as_ref().unwrap().provider, "agent");
    }
    let rebuilt = query::semantic_graph(root.path(), &filter()).unwrap();
    let relations = rebuilt
        .semantic_edges
        .iter()
        .filter(|edge| edge.predicate_type == "uses")
        .collect::<Vec<_>>();
    assert_eq!(relations.len(), 2);
    let mut indexed_note_ids = relations
        .iter()
        .flat_map(|edge| {
            query::relation_detail(root.path(), &edge.id)
                .unwrap()
                .unwrap()
                .note_ids
        })
        .collect::<Vec<_>>();
    indexed_note_ids.sort();
    assert_eq!(indexed_note_ids, note_ids);
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
