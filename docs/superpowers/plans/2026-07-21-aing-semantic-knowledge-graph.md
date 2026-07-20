# Aing Semantic Knowledge Graph Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver evidence-backed semantic relations, durable entity/relation governance, and an exploratory graph with explainable paths for both HTTP and local-Agent Aing.

**Architecture:** Keep per-note model facts in `aing.json`, replay durable human decisions from `knowledge-overrides.json`, and atomically rebuild `graph.sqlite` as a disposable query index. HTTP and Agent extraction converge on one Rust validation contract; Tauri commands expose semantic graph, review, governance, backfill, and path APIs to small Svelte components layered around the existing graph page.

**Tech Stack:** Rust 1.89, serde/serde_json, sha2, rusqlite, Tauri 2, Svelte 5 runes, TypeScript 5.6, d3-force 3, Vitest 4.

## Global Constraints

- Start execution only after preserving or committing the current user-owned changes in `src/lib/ForceGraph.svelte` and `.impeccable.md`. Never discard or overwrite them; Task 11 must reconcile its changes with the version present at execution time.
- Treat `aing.json` as model-fact truth, `knowledge-overrides.json` as human-decision truth, and `graph.sqlite` as a replaceable index. No task may blur these layers.
- All model relations require exact paragraph evidence plus subject/object mention IDs. Invalid graph payloads are rejected as a whole and never partially written.
- Graph failures degrade to the last readable index or the co-occurrence view. They must not fail recording, ASR, transcript refinement, playback, export, or people management.
- `knowledge-overrides.json` corruption is a visible read-only failure, never an empty-ledger fallback.
- No historical relation backfill starts automatically. The user must explicitly confirm the note count, provider, model, and privacy notice.
- Keep all new stable ordering deterministic: sort by stable ID after domain-specific ranking and use `BTreeMap`/`BTreeSet` at serialization boundaries.
- Run focused tests after every red/green step. Before each task commit, run `cargo fmt --check` for Rust changes, `npm test -- --run <file>` for focused frontend changes, and `npm run check` for Svelte/TypeScript changes.
- Commit only files listed by the task. Preserve unrelated dirty files.

---

## Task 1: Version the per-note graph contract and stable IDs

**Files:**

- Create: `src-tauri/src/store/aing_graph.rs`
- Modify: `src-tauri/src/store/mod.rs`
- Modify: `src-tauri/src/store/refined.rs`
- Modify: `src/lib/notes.ts`
- Test: `src-tauri/src/store/aing_graph.rs` (`#[cfg(test)]`)
- Test: `src-tauri/src/store/refined.rs` (`#[cfg(test)]`)
- Test: `src/lib/notes.test.ts`

**Interfaces:**

- Produce `store::GraphExtraction`, `store::RelationPredicate`, `store::RelationEvidence`, and `store::RelationFact` with serde defaults matching schema v2.
- Produce stable helpers:

```rust
pub fn mention_id(note_id: &str, paragraph: &RefinedParagraph, entity: &str, start: usize, end: usize) -> String;
pub fn evidence_id(note_id: &str, source_seqs: &[u64], start: usize, end: usize, quote: &str) -> String;
pub fn relation_fact_id(note_id: &str, relation: &RelationFact) -> String;
pub fn source_hash(paragraphs: &[RefinedParagraph]) -> String;
pub fn ensure_graph_ids(note_id: &str, doc: &mut RefinedDoc);
```

- Change `Mention` to `{ id, entity, start, end }`; add `RefineStages.relations`, `RefinedDoc.graph_extraction`, and `RefinedDoc.relations`.
- Preserve schema-v1 reads through `#[serde(default)]`; new writes use `REFINED_SCHEMA_VERSION = 2`.

- [ ] **Step 1: Add failing compatibility and determinism tests**

Add tests that deserialize a literal schema-v1 document with missing graph fields, call `ensure_graph_ids("note-1", &mut doc)`, and assert:

```rust
assert_eq!(doc.stages.relations, "off");
assert!(doc.graph_extraction.is_none());
assert!(doc.relations.is_empty());
assert!(doc.paragraphs[0].mentions[0].id.starts_with("mn_"));
assert_eq!(doc.paragraphs[0].mentions[0].id, first_id);
assert_eq!(doc.paragraphs[0].mentions[0].id.len(), 27);
```

Add a second test whose only change is the evidence quote and assert that `evidence_id` changes, while equal normalized quotes produce the same ID. Add a TypeScript fixture omitting `id`, `relations`, `relations` stage, and `graph_extraction` to prove the public types remain optional on reads.

- [ ] **Step 2: Run the focused tests and observe the expected failure**

Run:

```bash
cd src-tauri && cargo test store::aing_graph
cd src-tauri && cargo test store::refined::tests::schema_v1_defaults_graph_fields
npm test -- --run src/lib/notes.test.ts
```

Expected: Rust fails because `aing_graph` and the new fields do not exist; TypeScript fails because the new fixture cannot name the graph interfaces.

- [ ] **Step 3: Add the schema types and canonical hash encoding**

Implement `aing_graph.rs` with these complete public data shapes:

```rust
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelationPredicate {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RelationEvidence {
    #[serde(default)]
    pub id: String,
    pub paragraph_index: usize,
    pub start: usize,
    pub end: usize,
    pub quote: String,
    #[serde(default)]
    pub source_seqs: Vec<u64>,
    #[serde(default)]
    pub source_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RelationFact {
    #[serde(default)]
    pub id: String,
    pub subject: String,
    pub predicate: RelationPredicate,
    pub object: String,
    #[serde(default)]
    pub subject_mentions: Vec<String>,
    #[serde(default)]
    pub object_mentions: Vec<String>,
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<String>,
    #[serde(default)]
    pub evidence: Vec<RelationEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GraphExtraction {
    pub contract_version: u32,
    pub provider: String,
    pub model: String,
    pub run_id: String,
    pub generated_at: String,
    pub source_hash: String,
    pub mode: String,
}

pub fn stable_id(prefix: &str, fields: &[String]) -> String {
    let mut h = Sha256::new();
    for field in fields {
        h.update((field.len() as u64).to_be_bytes());
        h.update(field.as_bytes());
    }
    format!("{prefix}{}", &format!("{:x}", h.finalize())[..24])
}
```

Use length-prefixed UTF-8 fields, sorted mention/evidence ID lists, and `quote.trim().split_whitespace().collect::<Vec<_>>().join(" ")` for quote normalization. `source_hash` hashes paragraph `source_seqs` and final text in paragraph order.

- [ ] **Step 4: Wire schema v2 and lazy legacy-ID repair**

In `refined.rs`, set `REFINED_SCHEMA_VERSION` to `2`, add `stage_off` defaults, and change `load_refined` to call:

```rust
let mut doc: RefinedDoc = serde_json::from_slice(&bytes).ok()?;
let note_id = note_dir.file_name()?.to_str()?;
crate::store::aing_graph::ensure_graph_ids(note_id, &mut doc);
Some(doc)
```

Do not rewrite merely because legacy IDs were synthesized. Update every Rust fixture constructing `Mention`, `RefineStages`, or `RefinedDoc`, then mirror the read shapes in `src/lib/notes.ts` with optional legacy fields and required fields for v2 writes.

- [ ] **Step 5: Run tests and commit**

Run:

```bash
cd src-tauri && cargo test store::
npm test -- --run src/lib/notes.test.ts
cd src-tauri && cargo fmt --check
git add src-tauri/src/store/aing_graph.rs src-tauri/src/store/mod.rs src-tauri/src/store/refined.rs src/lib/notes.ts src/lib/notes.test.ts
git commit -m "feat(graph): version evidence-backed Aing facts"
```

Expected: all focused tests pass; no schema-v1 fixture requires a migration write.

---

## Task 2: Validate, normalize, merge, and publish relation facts

**Files:**

- Modify: `src-tauri/src/store/aing_graph.rs`
- Test: `src-tauri/src/store/aing_graph.rs`
- Create fixture: `src-tauri/tests/fixtures/aing_graph_valid.json`
- Create fixture: `src-tauri/tests/fixtures/aing_graph_invalid.json`

**Interfaces:**

```rust
pub const GRAPH_CONTRACT_VERSION: u32 = 1;
pub const CORE_PREDICATES: &[&str];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationIssue { pub relation_index: usize, pub field: String, pub message: String }

#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedGraph { pub relations: Vec<RelationFact> }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishTier { Published, Pending, RawOnly }

pub fn validate_graph(note_id: &str, doc: &RefinedDoc, relations: Vec<RelationFact>) -> Result<ValidatedGraph, Vec<ValidationIssue>>;
pub fn publish_tier(relation: &RelationFact, identity_conflict: bool, time_conflict: bool) -> PublishTier;
```

- [ ] **Step 1: Add the validator matrix as failing tests**

Cover all eight core predicates, trimmed non-empty custom labels, confidence bounds, entity existence, non-empty subject/object mentions, mention ownership, self-loop rejection, evidence existence, paragraph bounds, Unicode scalar offsets, exact quote equality, non-empty `source_seqs` subset, duplicate evidence removal, and equal triple evidence merge. Assert errors use stable field paths such as `relations[2].evidence[0].quote`.

Add tier assertions:

```rust
assert_eq!(publish_tier(&fact(0.80, "uses"), false, false), PublishTier::Published);
assert_eq!(publish_tier(&fact(0.50, "uses"), false, false), PublishTier::Pending);
assert_eq!(publish_tier(&fact(0.79, "uses"), false, false), PublishTier::Pending);
assert_eq!(publish_tier(&fact(0.99, "custom"), false, false), PublishTier::Pending);
assert_eq!(publish_tier(&fact(0.49, "uses"), false, false), PublishTier::RawOnly);
assert_eq!(publish_tier(&fact(0.99, "uses"), true, false), PublishTier::Pending);
```

- [ ] **Step 2: Run the test and verify it is red**

Run `cd src-tauri && cargo test store::aing_graph::tests::validator`. Expected: missing validation functions.

- [ ] **Step 3: Implement the all-or-nothing validator**

Use `HashMap<&str, (&str, usize)>` for mention ownership, `chars().collect::<Vec<_>>()` for evidence slicing, and a `BTreeMap<(String, String, String, Option<String>, Option<String>), RelationFact>` for deterministic relation merging. Trim custom labels before keying. Recompute every mention, evidence, and relation ID after normalization; never trust incoming IDs.

On any issue, return every issue and no normalized payload. On success, sort evidence by ID, mention IDs lexically, and relations by recomputed relation ID.

- [ ] **Step 4: Prove stable fixture behavior**

Deserialize `aing_graph_valid.json`, validate it twice with reversed relation input order, serialize both results, and assert byte equality. Deserialize `aing_graph_invalid.json` and assert at least one issue each for predicate, mentions, evidence, and confidence.

- [ ] **Step 5: Run and commit**

Run:

```bash
cd src-tauri && cargo test store::aing_graph
cd src-tauri && cargo fmt --check
git add src-tauri/src/store/aing_graph.rs src-tauri/tests/fixtures/aing_graph_valid.json src-tauri/tests/fixtures/aing_graph_invalid.json
git commit -m "feat(graph): validate and tier semantic relations"
```

Expected: the validator suite passes with deterministic JSON output.

---

## Task 3: Add the durable knowledge override ledger and stable entity registry

**Files:**

- Create: `src-tauri/src/graph/overrides.rs`
- Create: `src-tauri/src/graph/resolve.rs`
- Modify: `src-tauri/src/graph/mod.rs`
- Modify: `src-tauri/src/store/mod.rs`
- Test: `src-tauri/src/graph/overrides.rs`
- Test: `src-tauri/src/graph/resolve.rs`

**Interfaces:**

```rust
pub const KNOWLEDGE_FILE: &str = "knowledge-overrides.json";
pub const KNOWLEDGE_LOCK_FILE: &str = ".knowledge-overrides.lock";

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct KnowledgeLedger {
    pub schema_version: u32,
    pub registry: BTreeMap<String, RegistryEntity>,
    pub legacy_ids: BTreeMap<String, String>,
    pub operations: Vec<KnowledgeOperation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryEntity { pub kind: String, pub name: String, pub aliases: Vec<String>, pub status: String }

pub fn load(data_root: &Path) -> Result<KnowledgeLedger, KnowledgeLoadError>;
pub fn update<T>(data_root: &Path, change: impl FnOnce(&mut KnowledgeLedger) -> anyhow::Result<T>) -> anyhow::Result<T>;
pub fn allocate_entity_id(kind: &str, name: &str, note_id: &str, local_id: &str) -> String;
pub fn allocate_split_entity_id(operation_id: &str) -> String;
```

`KnowledgeOperation.kind` is a tagged serde enum with exact variants: `RenameEntity`, `AddAlias`, `RemoveAlias`, `MergeEntity`, `BindMention`, `ConfirmRelation`, `EditRelation`, `SuppressRelation`, `EndRelation`, `RestoreRelation`, `CreateEntity`, `CreateRelation`, and `Undo`. Every variant carries `id`, `at`, `before`, and its typed payload; operation rows are append-only.

Use this exact envelope and tagged action shape so JSON operation names remain stable snake_case:

```rust
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
    RenameEntity { entity_id: String, name: String },
    AddAlias { entity_id: String, alias: String },
    RemoveAlias { entity_id: String, alias: String },
    MergeEntity { source_id: String, target_id: String },
    BindMention { mention_id: String, entity_id: String },
    ConfirmRelation { relation_id: String },
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
    EndRelation { relation_id: String, valid_to: String },
    RestoreRelation { operation_id: String },
    CreateEntity { entity: RegistryEntity },
    CreateRelation { relation: UserRelation },
    Undo { operation_id: String },
}
```

`UserRelation` contains stable subject/object IDs, `RelationPredicate`, optional validity/note, evidence IDs, and `user_assertion: bool`. Reject it unless evidence IDs are non-empty or `user_assertion` is true.

```rust
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
```

- [ ] **Step 1: Add failing ledger safety tests**

Test empty initialization, deterministic `kg_` IDs, same-name same-kind allocation remaining distinct when seeds differ, rename preserving ID, append-only operations, legacy mapping, corruption returning `KnowledgeLoadError::Corrupt`, and a held file lock rejecting a second writer within the bounded retry window. Also inject an atomic-write failure before rename and assert the previous JSON bytes remain unchanged.

- [ ] **Step 2: Run the focused tests and confirm red**

Run `cd src-tauri && cargo test graph::overrides`. Expected: module and types are missing.

- [ ] **Step 3: Implement lock, load, backup, and atomic replace**

Follow `store/notelock.rs` using `std::fs::File::try_lock`, five 20 ms retries, and a guard-owned file handle. `update` must:

1. Acquire `.knowledge-overrides.lock`.
2. Load and validate the existing ledger; corruption returns an error without creating a replacement.
3. Apply the closure in memory.
4. Serialize to `knowledge-overrides.json.tmp` with pretty JSON.
5. Sync the temp file, copy the previous readable file to `knowledge-overrides.json.bak`, then rename temp over the live file.
6. Sync the data-root directory on Unix.

The initial file is exactly:

```json
{"schema_version":1,"registry":{},"legacy_ids":{},"operations":[]}
```

- [ ] **Step 4: Implement the pure resolver snapshot**

In `resolve.rs`, expose:

```rust
pub struct ResolverSnapshot {
    pub registry: BTreeMap<String, RegistryEntity>,
    pub redirects: BTreeMap<String, String>,
    pub mention_bindings: BTreeMap<String, String>,
    pub relation_decisions: RelationDecisions,
}

pub fn replay(ledger: &KnowledgeLedger) -> anyhow::Result<ResolverSnapshot>;
pub fn resolve_entity(snapshot: &ResolverSnapshot, people: &Voiceprints, note_id: &str, local: &Entity, mention_ids: &[String]) -> Resolution;
```

Apply resolution in this order: mention binding, person ID/redirect, merge redirect, exact confirmed name-or-alias plus kind, `legacy_ids`, allocate new `kg_*`. Reject redirect cycles and ambiguous exact matches as pending conflicts; do not use fuzzy matching for automatic resolution.

- [ ] **Step 5: Add the legacy bootstrap migration**

On the first valid ledger load only, read existing `entities`, `entity_redirects`, and `entity_name_overrides` from `graph.sqlite`; create registry entries and `legacy_ids` mappings without deleting SQLite rows. Record a single `CreateEntity` operation per imported entity and a `MergeEntity` for each redirect, all sorted by old ID so bootstrap bytes are deterministic.

- [ ] **Step 6: Run and commit**

Run:

```bash
cd src-tauri && cargo test graph::
cd src-tauri && cargo fmt --check
git add src-tauri/src/graph/overrides.rs src-tauri/src/graph/resolve.rs src-tauri/src/graph/mod.rs src-tauri/src/store/mod.rs
git commit -m "feat(graph): persist stable knowledge decisions"
```

Expected: corruption stays visible, concurrent writers cannot lose updates, and stable IDs survive rename/replay.

---

## Task 4: Resolve canonical entities, split evidence, and replay relation decisions

**Files:**

- Modify: `src-tauri/src/graph/resolve.rs`
- Create: `src-tauri/src/graph/canonical.rs`
- Modify: `src-tauri/src/graph/mod.rs`
- Test: `src-tauri/src/graph/canonical.rs`

**Interfaces:**

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalEntity {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub confirmed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalEvidence {
    pub id: String,
    pub note_id: String,
    pub paragraph_index: usize,
    pub start: usize,
    pub end: usize,
    pub quote: String,
    pub source_seqs: Vec<u64>,
    pub source_hash: String,
    pub subject_mentions: Vec<String>,
    pub object_mentions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalRelation {
    pub id: String,
    pub subject_id: String,
    pub predicate: RelationPredicate,
    pub object_id: String,
    pub confidence: f64,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub status: RelationStatus,
    pub origin: RelationOrigin,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub note_ids: Vec<String>,
    pub evidence: Vec<CanonicalEvidence>,
}

pub struct CanonicalGraph {
    pub entities: BTreeMap<String, CanonicalEntity>,
    pub mentions: Vec<CanonicalMention>,
    pub relations: Vec<CanonicalRelation>,
    pub pending: Vec<PendingItem>,
}

pub fn reconcile_registry(data_root: &Path) -> anyhow::Result<KnowledgeLedger>;
pub fn build_canonical_graph(data_root: &Path, ledger: &KnowledgeLedger, now: DateTime<FixedOffset>) -> anyhow::Result<CanonicalGraph>;
```

- [ ] **Step 1: Add the parser/replay matrix as failing tests**

Build documents in two opposite directory orders and assert the canonical JSON is equal. Cover alias matching, same name/different kind, ambiguous same name/same kind, person binding, legacy IDs, multi-hop merges, cycle rejection, split by selected mention IDs, undoing the split, and one fact whose subject evidence is split across two stable entities.

For relation replay, cover confirm, edit direction/type/time, suppress by stable triple, restore, end, user-created relation with evidence, user-created `user_assertion`, and a time conflict entering pending. Assert a suppressed triple stays suppressed when the model supplies new evidence IDs.

- [ ] **Step 2: Run and observe failure**

Run `cd src-tauri && cargo test graph::canonical`. Expected: canonical builder is missing.

- [ ] **Step 3: Implement deterministic note scanning and identity assignment**

`reconcile_registry` scans `data_root/notes/*/aing.json` under one `overrides::update` call, validates note IDs, sorts paths by note ID, and allocates missing stable entities without invoking a model or touching SQLite. Release the ledger lock before `build_canonical_graph`. The pure builder scans the same sorted paths, records unreadable model documents as `PendingItem::InvalidDocument`, and resolves every mention separately; an unbound local entity may map to one stable entity, while bound mentions can map to different stable entities.

Use this grouping key before decisions:

```rust
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RelationKey {
    subject_id: String,
    predicate_type: String,
    predicate_label: Option<String>,
    object_id: String,
}
```

Before resolving a fact, verify its evidence against the current paragraph index/range. If that position is stale, relocate by matching `source_seqs + source_hash + quote`; if the match is unique, update the in-memory location, otherwise emit `PendingItem::StaleEvidence` without deleting the model or human fact.

When a model fact spans multiple resolved subject or object identities, split its evidence by mention ownership, produce the valid Cartesian evidence groups only, recompute canonical relation IDs, and send evidence that cannot retain both sides to `PendingItem::SplitConflict`.

- [ ] **Step 4: Implement decision precedence and temporal state**

Replay operations in file order while honoring `Undo` compensation. Apply priorities in this order: user-created/edited, confirmed model, undecided model, suppressed/ended state. `valid_to <= now` is historical. Do not infer that a new relation ends an old one; emit a time-conflict pending item when current mutually exclusive `responsible_for` or `assigned_to` relations overlap.

The final published/pending/raw decision must call Task 2's `publish_tier`; `RawOnly` facts stay only in `aing.json` and never appear in `CanonicalGraph.relations` or `pending`.

- [ ] **Step 5: Run and commit**

Run:

```bash
cd src-tauri && cargo test graph::
cd src-tauri && cargo fmt --check
git add src-tauri/src/graph/resolve.rs src-tauri/src/graph/canonical.rs src-tauri/src/graph/mod.rs
git commit -m "feat(graph): resolve canonical semantic knowledge"
```

Expected: operation replay is deterministic, evidence-level splits move relations correctly, and model reruns cannot override human decisions.

---

## Task 5: Rebuild the semantic SQLite index by atomic replacement

**Files:**

- Create: `src-tauri/src/graph/index.rs`
- Modify: `src-tauri/src/graph/mod.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/Cargo.lock`
- Test: `src-tauri/src/graph/index.rs`

**Interfaces:**

```rust
pub const GRAPH_SCHEMA_VERSION: u32 = 2;

pub fn rebuild_atomic(data_root: &Path, canonical: &CanonicalGraph) -> anyhow::Result<BuildStats>;
pub fn rebuild_from_sources(data_root: &Path) -> anyhow::Result<BuildStats>;
pub fn open_readonly(data_root: &Path) -> anyhow::Result<rusqlite::Connection>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildStats {
    pub entities: usize,
    pub mentions: usize,
    pub relations: usize,
    pub evidence: usize,
    pub pending: usize,
}
```

The v2 schema contains `entities`, `note_entities`, `entity_mentions`, `relations`, `relation_evidence`, `pending_review`, and `graph_meta`. Keep compatibility co-occurrence rows derivable from `note_entities`; do not keep `entity_redirects` or `entity_name_overrides` as truth after bootstrap.

- [ ] **Step 1: Write failing schema, idempotency, and rollback tests**

Create a canonical graph with two entities, one current relation, one historical relation, two evidence rows, and one pending item. Rebuild twice and assert identical counts and query ordering. Inject a failure after temp schema creation but before commit and assert the bytes and query results of the live `graph.sqlite` remain readable and unchanged.

Add a test that holds a read connection to the old database while replacement completes; the old connection must still finish its query and a newly opened connection must see the new `graph_meta.build_id`.

- [ ] **Step 2: Run the focused test and confirm red**

Run `cd src-tauri && cargo test graph::index`. Expected: module and schema are absent.

- [ ] **Step 3: Implement the temp database build**

Build at `graph.sqlite.next`, remove only a stale `.next` file under `GRAPH_LOCK`, set `journal_mode=DELETE` for the temporary database, create the complete schema, insert rows inside one transaction, run `PRAGMA foreign_key_check`, verify every relation has at least one evidence row unless `origin='user_assertion'`, and commit.

After validation, compare the ledger digest captured before the build with the live ledger; if it changed, discard `.next`, mark the scheduler dirty, and retry from a fresh ledger snapshot. Then close the connection, copy the closed current database to `graph.sqlite.previous`, and call a platform helper:

```rust
fn atomic_replace(next: &Path, live: &Path, backup: &Path) -> anyhow::Result<()>;
```

On Unix, `atomic_replace` uses `std::fs::rename(next, live)`, whose destination replacement is atomic. On Windows, add a target-specific direct `windows-sys` dependency and use `ReplaceFileW(live, next, backup, REPLACEFILE_WRITE_THROUGH, null_mut(), null_mut())`. Never rename the live file away first, and never delete or truncate it before `.next` passes validation.

- [ ] **Step 4: Replace incremental startup/upsert calls**

Change startup and post-Aing indexing to call a single coalescing rebuild scheduler. The scheduler owns an `AtomicBool` dirty flag plus one worker; concurrent requests set dirty, the worker rebuilds, then reruns once if dirty became true during the build. Keep the previous graph readable on failure and emit a `graph_index_status` event with `{ state, error, stats }`.

- [ ] **Step 5: Run and commit**

Run:

```bash
cd src-tauri && cargo test graph::
cd src-tauri && cargo fmt --check
git add src-tauri/src/graph/index.rs src-tauri/src/graph/mod.rs src-tauri/src/lib.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat(graph): atomically rebuild semantic index"
```

Expected: a failed rebuild leaves the prior index queryable and a successful rebuild swaps all graph tables together.

---

## Task 6: Expose semantic queries, governance commands, and deterministic paths

**Files:**

- Create: `src-tauri/src/graph/query.rs`
- Create: `src-tauri/src/graph/path.rs`
- Modify: `src-tauri/src/graph/mod.rs`
- Modify: `src-tauri/src/ipc.rs`
- Modify: `src-tauri/src/lib.rs`
- Test: `src-tauri/src/graph/query.rs`
- Test: `src-tauri/src/graph/path.rs`
- Test: `src-tauri/src/lib.rs`

**Interfaces:**

```rust
pub struct GraphFilter {
    pub entity_kinds: Vec<String>,
    pub predicate_types: Vec<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub include_history: bool,
    pub include_cooccurrence: bool,
}

pub fn semantic_graph(data_root: &Path, filter: &GraphFilter) -> anyhow::Result<ipc::SemanticGraphData>;
pub fn semantic_entity_detail(data_root: &Path, entity_id: &str, filter: &GraphFilter) -> anyhow::Result<Option<ipc::SemanticEntityDetail>>;
pub fn relation_detail(data_root: &Path, relation_id: &str) -> anyhow::Result<Option<ipc::RelationDetail>>;
pub fn pending_review(data_root: &Path, filter: &GraphFilter) -> anyhow::Result<Vec<ipc::PendingReviewItem>>;
pub fn entity_mentions(data_root: &Path, entity_id: &str) -> anyhow::Result<Vec<ipc::MentionEvidence>>;
pub fn shortest_path(data_root: &Path, start: &str, end: &str, filter: &GraphFilter) -> anyhow::Result<Option<ipc::KnowledgePath>>;
```

Add Tauri commands with the same names plus these mutations:

```rust
fn apply_knowledge_operation(app: AppHandle, operation: ipc::KnowledgeOperationInput) -> Result<ipc::KnowledgeMutationResult, String>;
fn split_entity(app: AppHandle, request: ipc::SplitEntityRequest) -> Result<ipc::KnowledgeMutationResult, String>;
fn merge_entities(app: AppHandle, source_id: String, target_id: String) -> Result<ipc::KnowledgeMutationResult, String>;
fn undo_knowledge_operation(app: AppHandle, operation_id: String) -> Result<ipc::KnowledgeMutationResult, String>;
```

- [ ] **Step 1: Add failing query and path tests**

Test current/history and date overlap filters, predicate/entity filters, relation evidence order, pending groups, mention grouping, legacy ID redirect, and graph read degradation when the ledger is corrupt.

For Dijkstra, use a fixture with two equal-cost routes and assert ordering by total cost, hop count, higher minimum confidence, then lexical stable-ID sequence. Assert costs exactly:

```rust
assert_eq!(edge_cost(manual_edge()), 1.0);
assert_eq!(edge_cost(model_edge(0.9)), 1.3);
assert_eq!(edge_cost(cooccurrence_edge()), 3.0);
```

Assert direction is preserved in each returned step even though exploration may traverse an edge in either direction.

- [ ] **Step 2: Run and confirm red**

Run `cd src-tauri && cargo test graph::query` followed by `cd src-tauri && cargo test graph::path`. Expected: new APIs do not exist.

- [ ] **Step 3: Implement read models and SQL filters**

Define IPC structs with `#[derive(Debug, Clone, Serialize)]`; semantic edges include relation ID, subject/object IDs, predicate type/label, status, confidence, origin, evidence count, note count, and valid interval. Co-occurrence edges remain a distinct array and never masquerade as semantic edges.

Use bound SQL parameters for all filters. Resolve legacy IDs before lookup. Read commands may return the last index plus `degraded: true` and a message; mutation commands must fail when the ledger is corrupt.

- [ ] **Step 4: Implement deterministic Dijkstra**

Represent the queue key as ordered integer micro-cost to avoid floating ordering instability:

```rust
fn edge_cost_micros(edge: &PathEdge) -> u64 {
    match edge.origin {
        RelationOrigin::Manual | RelationOrigin::Confirmed => 1_000_000,
        RelationOrigin::Model => (1.2 + (1.0 - edge.confidence))
            .mul_add(1_000_000.0, 0.0)
            .round() as u64,
        RelationOrigin::Cooccurrence => 3_000_000,
    }
}
```

The queue rank is `(cost_micros, hops, Reverse(min_confidence_micros), stable_path_ids)`. Exclude suppressed, pending, raw-only, and filtered historical edges; include co-occurrence only when explicitly requested.

- [ ] **Step 5: Implement mutation-to-rebuild handoff**

Validate operation inputs, call `overrides::update`, append exactly one typed operation (or the atomic paired end+confirm payload for responsibility changes), release the ledger lock, then request a rebuild. Return operation ID and `rebuild_state: "queued"`; do not hold the ledger lock while rebuilding.

- [ ] **Step 6: Register commands and run tests**

Add every command to `tauri::generate_handler!`, then run:

```bash
cd src-tauri && cargo test graph::
cd src-tauri && cargo fmt --check
git add src-tauri/src/graph/query.rs src-tauri/src/graph/path.rs src-tauri/src/graph/mod.rs src-tauri/src/ipc.rs src-tauri/src/lib.rs
git commit -m "feat(graph): expose semantic queries and governance"
```

Expected: query filters, mutations, legacy redirects, and path tie-breaking pass with stable output.

---

## Task 7: Extract and persist relations through the HTTP Aing path

**Files:**

- Modify: `src-tauri/src/refine/llm.rs`
- Create: `src-tauri/src/refine/relations.rs`
- Modify: `src-tauri/src/refine/mod.rs`
- Modify: `src-tauri/src/lib.rs`
- Test: `src-tauri/src/refine/llm.rs`
- Test: `src-tauri/src/refine/relations.rs`

**Interfaces:**

```rust
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RawRelation {
    pub subject: String,
    pub predicate: RelationPredicate,
    pub object: String,
    pub confidence: f64,
    #[serde(default)]
    pub valid_from: Option<String>,
    #[serde(default)]
    pub valid_to: Option<String>,
    pub evidence: Vec<RawEvidence>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RawEvidence {
    pub paragraph_index: usize,
    pub start: usize,
    pub end: usize,
    pub quote: String,
}

pub fn materialize(note_id: &str, doc: &RefinedDoc, raw: Vec<RawRelation>) -> Result<ValidatedGraph, Vec<ValidationIssue>>;
pub fn apply_validated_graph(doc: &mut RefinedDoc, extraction: GraphExtraction, graph: ValidatedGraph);
```

Change `llm::polish` to return `(LlmOutcome, Vec<RawEntity>, Vec<RawRelation>)`. Paragraph indices in the response are absolute indices in `RefinedDoc.paragraphs`, not chunk-local indices.

- [ ] **Step 1: Add failing HTTP parsing/materialization tests**

Extend the existing local mock server response to include one `responsible_for` relation with Chinese Unicode offsets. Assert text polish, entity extraction, relation parsing, stable subject/object mention IDs, exact evidence quote, provider/model metadata, and `stages.relations == "done"`.

Add failure cases for an invalid quote and a response containing a relation to an absent entity. Assert text changes remain, `stages.llm`/`stages.entities` reflect their own outcome, `stages.relations == "failed"`, and the prior successful `relations` plus `graph_extraction` remain unchanged.

- [ ] **Step 2: Run the focused tests and observe failure**

Run:

```bash
cd src-tauri && cargo test refine::llm::tests::parses_relations
cd src-tauri && cargo test refine::relations
```

Expected: `RawRelation`, materialization, and relation stages do not exist.

- [ ] **Step 3: Extend the structured prompt and response parser**

Append to `SYSTEM_PROMPT` the eight predicate codes, custom fallback, confidence, optional validity, absolute paragraph index, Unicode scalar offsets, and exact quote requirement. The response contract is:

```json
{"glossary":{},"texts":["修订文本"],"entities":[{"name":"张三","kind":"person","aliases":[]}],"relations":[{"subject":"张三","predicate":{"type":"responsible_for","label":null},"object":"灯塔计划","confidence":0.92,"valid_from":null,"valid_to":null,"evidence":[{"paragraph_index":0,"start":0,"end":8,"quote":"张三负责灯塔计划"}]}]}
```

Pass `(absolute_index, text)` pairs into each chunk prompt. Parse a malformed or absent `relations` field as a graph-stage error, not a text-stage error; an explicit empty array is a successful relation extraction.

- [ ] **Step 4: Materialize names into local entities and mention IDs**

Resolve `subject`/`object` by case-folded exact canonical name or alias. For each raw evidence row, select subject/object mentions in that paragraph whose span overlaps `[start, end)` and whose `entity` matches the resolved local ID. Fill evidence `source_seqs` from the paragraph, compute IDs through Task 1 helpers, and pass the entire payload through Task 2's validator.

If validation fails, log the field issues, keep any previous successful graph facts, and do not write a partial relation list.

- [ ] **Step 5: Preserve old graph facts until a replacement validates**

Before `run_local` overwrites the per-note document, load the existing document and carry its `relations` and `graph_extraction` into the new in-memory document as a fallback snapshot. Successful materialization replaces both atomically and sets `stages.relations = "done"`; graph failure restores the snapshot and sets the relation stage to `"failed"`. Text and entity stage state remain independent.

After a successful document write, request the coalescing index rebuild from Task 5. Do not rebuild while the note file is half-written.

- [ ] **Step 6: Run and commit**

Run:

```bash
cd src-tauri && cargo test refine::
cd src-tauri && cargo fmt --check
git add src-tauri/src/refine/llm.rs src-tauri/src/refine/relations.rs src-tauri/src/refine/mod.rs src-tauri/src/lib.rs
git commit -m "feat(aing): extract semantic relations over HTTP"
```

Expected: valid HTTP facts are evidence-backed and invalid graph payloads never roll back successful text refinement or erase the prior graph snapshot.

---

## Task 8: Give local Agents contract-equivalent graph read/write tools

**Files:**

- Modify: `src-tauri/src/mcp/server.rs`
- Modify: `src-tauri/src/mcp/tools.rs`
- Modify: `src-tauri/src/refine/agent.rs`
- Modify: `src-tauri/src/mcp/skill_template.md`
- Test: `src-tauri/src/mcp/server.rs`
- Test: `src-tauri/src/mcp/tools.rs`
- Test: `src-tauri/src/refine/agent.rs`

**Interfaces:**

```rust
#[derive(Deserialize, schemars::JsonSchema)]
pub struct GetAingContextParams { pub note_id: String }

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ApplyAingGraphParams {
    pub note_id: String,
    pub entities: Vec<Entity>,
    pub relations: Vec<RelationFact>,
    pub contract_version: u32,
    pub model: String,
}

pub fn get_aing_context(roots: &DataRoots, note_id: &str) -> anyhow::Result<serde_json::Value>;
pub fn apply_aing_graph(roots: &DataRoots, params: ApplyAingGraphParams) -> anyhow::Result<serde_json::Value>;
```

- [ ] **Step 1: Add failing tool contract and command-whitelist tests**

Assert `VnMcp::tool_router()` and `catalog()` both contain `get_aing_context` and `apply_aing_graph`. Assert context includes final paragraphs, `source_seqs`, entities, mention IDs, core predicates, contract version, and current document source hash. Assert graph write rejects a wrong contract, stale evidence hashes/quotes, invalid evidence, traversal note IDs, and a payload that attempts an unknown human-decision field.

For every Agent kind, inspect `refine_command` and assert the effective allowed tools are exactly `get_note`, `apply_refined_texts`, `get_aing_context`, and `apply_aing_graph`; built-in file/shell tools remain disabled where the CLI supports a whitelist.

- [ ] **Step 2: Run and observe failure**

Run `cd src-tauri && cargo test mcp::`, then `cd src-tauri && cargo test refine::agent`. Expected: two tools and the four-tool prompt are missing.

- [ ] **Step 3: Implement read and constrained write tools**

`get_aing_context` loads the current `aing.json`, calls `ensure_graph_ids`, and returns `source_hash` calculated from final text. `apply_aing_graph` reloads immediately before validation, requires the contract version to match, normalizes submitted entities, recomputes their mentions against the current final text, rejects stale evidence hashes/quotes through Task 2, and writes only `entities`, computed paragraph `mentions`, `relations`, `graph_extraction`, and entity/relation stages. It then atomically replaces that note's `aing.json` under the note lock; `graph_extraction.source_hash` is computed server-side from the document being committed.

The tool cannot import or call `graph::overrides`; its parameter schema has no registry or operation fields.

- [ ] **Step 4: Update the Agent workflow and success predicate**

Change `refine_prompt` to require:

1. `get_note` and `apply_refined_texts` for final text.
2. `get_aing_context` after the text write.
3. Extraction using only the advertised predicates and exact evidence.
4. One `apply_aing_graph` call, including an empty relation array when no relationship is supported.

`run_refine` succeeds only when the paragraph count is unchanged, `stages.llm == "done"`, `stages.entities == "done"`, `stages.relations == "done"`, and the stored extraction source hash matches final paragraphs. If graph write fails, retain `llm == "done"`, preserve the old graph snapshot, mark entity/relation stages failed, and return a graph-specific error for logs.

- [ ] **Step 5: Update capability documentation and parity fixture**

Update `mcp::catalog`, `skill_template.md`, and the catalog count assertions. Feed the same `aing_graph_valid.json` relation payload to the HTTP materializer and MCP writer; assert their persisted normalized `entities`, `mentions`, and `relations` serialize identically after ignoring provider/model/run metadata.

- [ ] **Step 6: Run and commit**

Run:

```bash
cd src-tauri && cargo test mcp::
cd src-tauri && cargo test refine::agent
cd src-tauri && cargo test http_and_agent_graph_contracts_match
cd src-tauri && cargo fmt --check
git add src-tauri/src/mcp/server.rs src-tauri/src/mcp/tools.rs src-tauri/src/refine/agent.rs src-tauri/src/mcp/skill_template.md
git commit -m "feat(aing): add Agent semantic graph parity"
```

Expected: HTTP and Agent normalize the same fixture identically, while Agent tools still cannot mutate human decisions.

---

## Task 9: Add explicit, resumable relation-only backfill

**Files:**

- Create: `src-tauri/src/refine/backfill.rs`
- Modify: `src-tauri/src/refine/mod.rs`
- Modify: `src-tauri/src/refine/llm.rs`
- Modify: `src-tauri/src/refine/agent.rs`
- Modify: `src-tauri/src/ipc.rs`
- Modify: `src-tauri/src/lib.rs`
- Test: `src-tauri/src/refine/backfill.rs`

**Interfaces:**

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct BackfillRequest { pub note_ids: Option<Vec<String>>, pub provider: String }

#[derive(Debug, Clone, Serialize)]
pub struct BackfillPreview {
    pub note_ids: Vec<String>,
    pub provider: String,
    pub model: String,
    pub contract_version: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackfillProgress {
    pub state: String,
    pub completed: usize,
    pub total: usize,
    pub current_note_id: Option<String>,
    pub failed: Vec<BackfillFailure>,
}

pub fn preview(data_root: &Path, settings: &Settings, requested: Option<&[String]>) -> anyhow::Result<BackfillPreview>;
pub fn run_one(note_dir: &Path, note_id: &str, executor: &dyn RelationExecutor) -> anyhow::Result<BackfillOutcome>;
```

Add Tauri commands `preview_relation_backfill`, `start_relation_backfill`, and `cancel_relation_backfill`, plus event `relation_backfill_progress`.

- [ ] **Step 1: Add failing selection, cancellation, and preservation tests**

Create three note fixtures: current contract/current hash, old contract, and failed relation stage. Assert preview selects only the latter two by default. Run with cancellation raised during the second note and assert the first note committed, the second kept its previous graph snapshot, and resuming skips the first.

Assert paragraph text bytes before and after every relation-only run are identical. Assert a failed new extraction never clears an older successful relation list.

- [ ] **Step 2: Run and confirm red**

Run `cd src-tauri && cargo test refine::backfill`. Expected: module and commands do not exist.

- [ ] **Step 3: Extract provider-neutral relation execution**

Define:

```rust
pub trait RelationExecutor: Send + Sync {
    fn provider(&self) -> &str;
    fn model(&self) -> &str;
    fn extract(&self, note_id: &str, doc: &RefinedDoc) -> anyhow::Result<ValidatedGraph>;
}
```

Implement `HttpRelationExecutor` with a relation-only prompt and `AgentRelationExecutor` with the two graph MCP tools. Neither executor receives a mutable paragraph text slice. `run_one` snapshots the old graph fields, validates the new result, writes atomically on success, and restores the snapshot on error or cancellation.

- [ ] **Step 4: Add a single global backfill gate**

Extend `AppState` with `relation_backfill_running: Arc<AtomicBool>` and `relation_backfill_cancel: Arc<AtomicBool>`. Reject a second run, use an RAII reset guard, process notes serially, check cancellation before model invocation and before commit, emit progress after each terminal note, and request one coalesced index rebuild at the end.

Never start backfill from setup, migration, setting save, or contract-version detection.

- [ ] **Step 5: Register commands and run tests**

Run:

```bash
cd src-tauri && cargo test refine::backfill
cd src-tauri && cargo fmt --check
git add src-tauri/src/refine/backfill.rs src-tauri/src/refine/mod.rs src-tauri/src/refine/llm.rs src-tauri/src/refine/agent.rs src-tauri/src/ipc.rs src-tauri/src/lib.rs
git commit -m "feat(graph): add resumable relation backfill"
```

Expected: backfill is user-triggered, serial, cancellable, resumable, and cannot change transcript text.

---

## Task 10: Add typed frontend APIs and pure graph-view state

**Files:**

- Create: `src/lib/knowledge.ts`
- Create: `src/lib/knowledgeView.ts`
- Create: `src/lib/knowledgeView.test.ts`
- Modify: `src/lib/graph.ts`
- Modify: `src/lib/graphFilter.svelte.ts`
- Modify: `src/lib/graph.test.ts`

**Interfaces:**

```ts
export type RelationStatus = "current" | "historical";
export type RelationOrigin = "model" | "confirmed" | "manual" | "user_assertion";

export interface SemanticEdge {
  id: string;
  subject_id: string;
  object_id: string;
  predicate_type: string;
  predicate_label: string | null;
  status: RelationStatus;
  confidence: number;
  origin: RelationOrigin;
  evidence_count: number;
  note_count: number;
  valid_from: string | null;
  valid_to: string | null;
}

export interface SemanticGraphData {
  nodes: EntitySummary[];
  semantic_edges: SemanticEdge[];
  cooccurrence_edges: EdgeRow[];
  degraded: boolean;
  message: string | null;
}

export interface KnowledgeFilter {
  entity_kinds: string[];
  predicate_types: string[];
  from: string | null;
  to: string | null;
  include_history: boolean;
  include_cooccurrence: boolean;
}

export const semanticGraph = (filter: KnowledgeFilter) => invoke<SemanticGraphData>("semantic_graph", { filter });
export const semanticEntityDetail = (entityId: string, filter: KnowledgeFilter) => invoke<SemanticEntityDetail | null>("semantic_entity_detail", { entityId, filter });
export const relationDetail = (relationId: string) => invoke<RelationDetail | null>("relation_detail", { relationId });
export const pendingReview = (filter: KnowledgeFilter) => invoke<PendingReviewItem[]>("pending_review", { filter });
export const entityMentions = (entityId: string) => invoke<MentionEvidence[]>("entity_mentions", { entityId });
export const knowledgePath = (start: string, end: string, filter: KnowledgeFilter) => invoke<KnowledgePath | null>("shortest_path", { start, end, filter });
```

- [ ] **Step 1: Write failing pure view-state tests**

Test relation labels for core/custom predicates, current/history/date filtering, entity-kind filtering, BFS one-hop expansion, collapse to default backbone, path node/edge emphasis, co-occurrence opt-in, stable sort, and empty semantic graph fallback.

Use exact assertions:

```ts
expect(relationLabel({ predicate_type: "responsible_for", predicate_label: null })).toBe("负责");
expect(relationLabel({ predicate_type: "custom", predicate_label: "推动" })).toBe("推动");
expect(nextExpandedIds(seed, edges, 1)).toEqual(new Set(["kg_a", "kg_b", "kg_c"]));
expect(viewEdges(data, { ...DEFAULT_KNOWLEDGE_FILTER, include_cooccurrence: false }).every((e) => e.layer === "semantic")).toBe(true);
```

- [ ] **Step 2: Run and confirm red**

Run `npm test -- --run src/lib/knowledgeView.test.ts src/lib/graph.test.ts`. Expected: knowledge types and helpers are missing.

- [ ] **Step 3: Implement the TypeScript contract and invoke wrappers**

Mirror every Task 6 IPC field without renaming semantic meaning. Define discriminated `KnowledgeOperationInput` variants for every supported mutation, `SplitEntityRequest`, `KnowledgeMutationResult`, `BackfillPreview`, and `BackfillProgress`. Keep existing `graphData`/`noteGraphData` APIs intact for co-occurrence fallback and article view.

- [ ] **Step 4: Implement pure view derivation**

`knowledgeView.ts` exports:

```ts
export const DEFAULT_KNOWLEDGE_FILTER: KnowledgeFilter;
export function relationLabel(edge: Pick<SemanticEdge, "predicate_type" | "predicate_label">): string;
export function filterSemanticGraph(data: SemanticGraphData, filter: KnowledgeFilter): SemanticGraphData;
export function nextExpandedIds(seed: Set<string>, edges: SemanticEdge[], hops: number, capPerNode?: number): Set<string>;
export function defaultBackbone(data: SemanticGraphData, maxNodes?: number, perNode?: number): Set<string>;
export function viewEdges(data: SemanticGraphData, filter: KnowledgeFilter): RenderEdge[];
export function pathEmphasis(path: KnowledgePath | null): { nodeIds: Set<string>; edgeIds: Set<string> };
```

Use a default neighbor cap of eight, stable confidence/note-count/ID sorting, and never truncate relation labels.

- [ ] **Step 5: Extend shared filter state and commit**

Add predicate, date, history, co-occurrence, pending-panel, path-start, and path-end state to `GraphFilterState`. Run:

```bash
npm test -- --run src/lib/knowledgeView.test.ts src/lib/graph.test.ts
npm run check
git add src/lib/knowledge.ts src/lib/knowledgeView.ts src/lib/knowledgeView.test.ts src/lib/graph.ts src/lib/graphFilter.svelte.ts src/lib/graph.test.ts
git commit -m "feat(graph): add semantic graph client state"
```

Expected: pure filters and exploration state pass without needing Tauri or a browser.

---

## Task 11: Build entity governance, pending review, and evidence drawers

**Files:**

- Create: `src/lib/knowledgeGovernance.ts`
- Create: `src/lib/knowledgeGovernance.test.ts`
- Create: `src/lib/EntityGovernance.svelte`
- Create: `src/lib/EntitySplitDialog.svelte`
- Create: `src/lib/PendingReviewPanel.svelte`
- Create: `src/lib/RelationDrawer.svelte`
- Modify: `src/lib/Sidebar.svelte`
- Modify: `src/routes/graph/+page.svelte`

**Interfaces:**

```ts
export interface GovernanceController {
  busy: boolean;
  error: string;
  lastOperationId: string | null;
  submit(operation: KnowledgeOperationInput): Promise<KnowledgeMutationResult>;
  split(request: SplitEntityRequest): Promise<KnowledgeMutationResult>;
  undo(operationId: string): Promise<KnowledgeMutationResult>;
}

export function createGovernanceController(api: GovernanceApi, refresh: () => Promise<void>): GovernanceController;
export function splitPreview(selected: MentionEvidence[], total: MentionEvidence[]): SplitPreview;
export function groupPending(items: PendingReviewItem[]): PendingGroup[];
```

Component contracts:

```ts
// EntityGovernance.svelte props
{ detail: SemanticEntityDetail; onChanged: () => Promise<void>; onOpenRelation: (id: string) => void }

// EntitySplitDialog.svelte props
{ entity: SemanticEntityDetail; mentions: MentionEvidence[]; onClose: () => void; onCommitted: () => Promise<void> }

// PendingReviewPanel.svelte props
{ items: PendingReviewItem[]; onClose: () => void; onChanged: () => Promise<void>; onOpenRelation: (id: string) => void }

// RelationDrawer.svelte props
{ relationId: string; onClose: () => void; onChanged: () => Promise<void> }
```

- [ ] **Step 1: Add failing controller and preview tests**

Test busy deduplication, failed mutation retaining the form, successful mutation refreshing once, undo using the returned operation ID, pending grouping order, split counts by notes/mentions/relations, and an empty split selection disabling submit.

- [ ] **Step 2: Run and confirm red**

Run `npm test -- --run src/lib/knowledgeGovernance.test.ts`. Expected: controller/helpers are missing.

- [ ] **Step 3: Implement controller and typed action builders**

Keep network mutation code outside Svelte components. Export builders for rename, add/remove alias, merge, bind person, confirm/edit/suppress/end/restore relation, create entity/relation, split, and undo. `submit` must block concurrent duplicates, preserve errors, await the queued rebuild status refresh, and then invoke the supplied refresh callback once.

- [ ] **Step 4: Build entity governance in the existing detail view**

Replace the current rename-only header logic with `EntityGovernance`. Keep the graph canvas mounted when `?e=` changes and open governance in a side sheet, so selecting a node can focus/expand the network without replacing it with a separate full-page box. Present three visible sections: overview, current/history relations, and evidence grouped by note. Include canonical name/type/status/stats; aliases as removable chips plus add input; merge, split, create relation, and person-link actions. Keep right-click shortcuts, but ensure every shortcut also appears in the detail panel.

The split dialog loads all mention evidence, displays full quote/text without ellipsis, groups by note, supports select-all per note, and shows the exact moving-note, mention, and affected-relation counts before submit. After commit, show an Undo action tied to the operation ID.

- [ ] **Step 5: Build pending and relation review surfaces**

Add a persistent `待整理 N` entry to the graph portion of `Sidebar.svelte`. The panel groups duplicate/person candidates, low confidence, custom predicates, and time/identity conflicts. Each row supports confirm, edit, suppress, and later; suppress is a durable operation, while later only closes the row for the current session.

The relation drawer shows full direction and label, status, confidence, provider/model, validity, current/history versions, and all full evidence quotes with note/time links. It exposes confirm, edit direction/type/time/note, end, suppress, restore, and undo. A relation without evidence must visibly show `用户直接声明`.

- [ ] **Step 6: Verify accessibility and commit**

Ensure dialogs trap focus, Escape closes, destructive suppression has a confirmation, all rows are keyboard reachable, and error/status text uses `aria-live`. Run:

```bash
npm test -- --run src/lib/knowledgeGovernance.test.ts
npm run check
git add src/lib/knowledgeGovernance.ts src/lib/knowledgeGovernance.test.ts src/lib/EntityGovernance.svelte src/lib/EntitySplitDialog.svelte src/lib/PendingReviewPanel.svelte src/lib/RelationDrawer.svelte src/lib/Sidebar.svelte src/routes/graph/+page.svelte
git commit -m "feat(graph): add evidence-level knowledge governance"
```

Expected: every governance operation has a visible primary entry, persistent mutation feedback, and an undo path.

---

## Task 12: Render a semantic exploration network and explainable paths

**Files:**

- Modify: `src/lib/ForceGraph.svelte`
- Create: `src/lib/KnowledgeGraphToolbar.svelte`
- Create: `src/lib/KnowledgePathPanel.svelte`
- Modify: `src/routes/graph/+page.svelte`
- Modify: `src/lib/Sidebar.svelte`
- Test: `src/lib/knowledgeView.test.ts`

**Interfaces:**

Extend the force graph props without breaking existing note/co-occurrence callers:

```ts
{
  nodes: EntitySummary[];
  edges: RenderEdge[] | EdgeRow[];
  onPick: (id: string, isPerson: boolean) => void;
  onEdgePick?: (id: string, layer: "semantic" | "cooccurrence") => void;
  focusedNodeIds?: Set<string>;
  focusedEdgeIds?: Set<string>;
  reducedMotion?: boolean;
}
```

`RenderEdge` contains `{ id, a, b, weight, layer, label, directed, confidence, status }`. Existing `EdgeRow` is converted to a co-occurrence `RenderEdge` inside the component.

- [ ] **Step 1: Add failing view-policy tests**

Add tests for semantic edge priority over co-occurrence, full labels, path-only emphasis, default backbone caps, repeated one-hop expansion, collapse, history inclusion, and a semantic-empty/co-occurrence-present fallback. No expected label may contain `…` or end in three periods.

- [ ] **Step 2: Run and confirm red**

Run `npm test -- --run src/lib/knowledgeView.test.ts`. Expected: render-edge and label-visibility policies are missing.

- [ ] **Step 3: Reconcile and extend `ForceGraph.svelte`**

First inspect the execution-time diff of `ForceGraph.svelte`; preserve the current full centered node-label implementation. Add semantic SVG edges as solid lines with arrow markers and co-occurrence edges as faint dashed lines. A full relation name is rendered on its edge using `textPath`; never substring or ellipsize it.

Show semantic labels when the graph has at most 30 visible semantic edges, when zoom is at least 1.35, when the edge is hovered/focused, or when it belongs to the active path. Hide the entire label at lower semantic zoom instead of shortening its content. Keep node names centered on their nodes, with line wrapping but no detached boxes.

Clicking a semantic edge calls `onEdgePick(relationId, "semantic")`; clicking a co-occurrence edge calls `onEdgePick(edgeId, "cooccurrence")`. Focused path edges retain color while unrelated edges fade to 15% opacity. Respect `prefers-reduced-motion` by placing the graph at its settled snapshot without animated transitions.

- [ ] **Step 4: Add toolbar, expansion, filters, and fallback**

`KnowledgeGraphToolbar` controls entity kinds, relation types, date range, current/history, co-occurrence visibility, collapse, and show-all. The route loads `semanticGraph(filter)`, renders the default semantic backbone, expands one semantic hop on node click, and can collapse to the backbone.

When semantic edges are empty but co-occurrence exists, render the existing co-occurrence graph and show `尚未补建语义关系` with a backfill action. When the semantic query is degraded, show the returned message and retain the usable graph.

- [ ] **Step 5: Add two-point path interaction**

The entity menu offers `设为路径起点`; choosing another node sets the endpoint and calls `knowledgePath`. `KnowledgePathPanel` lists each step with direction, full relation label, origin/confidence, and evidence action. Provide an explicit `包含共现弱连接` toggle that reruns the query; co-occurrence path steps remain dashed and labeled `共同出现（N 篇）`.

Search selects and focuses matching nodes without deleting unrelated graph data. The filtered list view below/alongside the canvas exposes the same node, edge, evidence, and path actions to keyboard users.

- [ ] **Step 6: Test, check, and commit**

Run:

```bash
npm test -- --run src/lib/knowledgeView.test.ts src/lib/graph.test.ts
npm run check
git add src/lib/ForceGraph.svelte src/lib/KnowledgeGraphToolbar.svelte src/lib/KnowledgePathPanel.svelte src/routes/graph/+page.svelte src/lib/Sidebar.svelte src/lib/knowledgeView.test.ts
git commit -m "feat(graph): render exploratory semantic paths"
```

Expected: semantic relations are the visual foreground, full node/edge labels remain discoverable, and the path view is evidence-linked and deterministic.

---

## Task 13: Add backfill UI, migration coverage, performance fixtures, and end-to-end proof

**Files:**

- Create: `src/lib/relationBackfill.ts`
- Create: `src/lib/relationBackfill.test.ts`
- Create: `src/lib/RelationBackfillDialog.svelte`
- Modify: `src/routes/ai/+page.svelte`
- Modify: `src/routes/graph/+page.svelte`
- Create: `src-tauri/src/graph/e2e_tests.rs`
- Create: `src-tauri/src/graph/large_fixture.rs`
- Modify: `src-tauri/src/graph/mod.rs`
- Modify: `DESIGN.md`

**Interfaces:**

```ts
export const previewRelationBackfill = (noteIds?: string[]) => invoke<BackfillPreview>("preview_relation_backfill", { noteIds: noteIds ?? null });
export const startRelationBackfill = (request: BackfillRequest) => invoke<void>("start_relation_backfill", { request });
export const cancelRelationBackfill = () => invoke<void>("cancel_relation_backfill");
export function subscribeRelationBackfill(handler: (progress: BackfillProgress) => void): Promise<UnlistenFn>;
```

- [ ] **Step 1: Add failing backfill state tests**

Test preview loading/error, explicit confirmation requirement, progress event reduction, cancellation state, resumable completion, and unsubscribe cleanup. Assert the dialog cannot call start before the user has seen note count, provider, model, contract version, and the privacy notice.

- [ ] **Step 2: Run and confirm red**

Run `npm test -- --run src/lib/relationBackfill.test.ts`. Expected: client and reducer do not exist.

- [ ] **Step 3: Implement the backfill dialog in both entry points**

Expose the same dialog from the AI page and graph empty/degraded banner. Preview first; show selected note count, executor, exact model, contract version, `将把修订稿发送给当前配置的执行体`, and no invented price. After confirmation, show completed/total, current note title/ID, failures, cancel, close-after-terminal, and resume using the default preview selection.

- [ ] **Step 4: Add migration and fault-injection integration tests**

In `graph/e2e_tests.rs`, included from `graph/mod.rs` under `#[cfg(test)]`, cover:

1. Schema-v1 `aing.json` plus old `e:` SQLite IDs bootstrap into stable `kg_*` IDs.
2. Rename, alias, merge, split, suppress, restore, end, create, and undo survive re-Aing simulation and two index rebuilds.
3. A corrupt ledger leaves the prior index readable and mutation/backfill commands rejected.
4. A failed `.next` build leaves the prior index intact.
5. HTTP and Agent fixture normalization is equal.
6. Backfill never changes paragraph text.

Use a temporary data root and direct Rust entry points; do not require a running GUI or real model.

- [ ] **Step 5: Add and enforce the large deterministic fixture**

Generate exactly 1,000 entities, 5,000 semantic relations, 1,500 co-occurrence edges, and evidence rows from a fixed seed in `graph/large_fixture.rs`, also included under `#[cfg(test)]`. Add ignored timing tests with generous release-mode budgets:

```rust
assert!(rebuild_elapsed < std::time::Duration::from_secs(5));
assert!(query_elapsed < std::time::Duration::from_millis(500));
assert!(path_elapsed < std::time::Duration::from_millis(500));
```

Run them explicitly in release mode during final verification. Also serialize the fixture to a browser-test import command or test-only Tauri command so the real graph page can be inspected with the same data.

- [ ] **Step 6: Update architecture documentation**

Update `DESIGN.md` with the three truth layers, schema v2, override ledger, resolver precedence, atomic index swap, HTTP/Agent contract parity, backfill consent, semantic/co-occurrence visual hierarchy, governance actions, and deterministic path cost/tie rules. Remove statements that describe `e:<name>` or co-occurrence as the canonical semantic model.

- [ ] **Step 7: Run the complete automated verification**

Run:

```bash
npm test
npm run check
npm run build
cd src-tauri && cargo test
cd src-tauri && cargo test --release semantic_graph_large -- --ignored
cd src-tauri && cargo fmt --check
```

Expected: all frontend and Rust tests pass; the large fixture remains inside the budgets.

- [ ] **Step 8: Run browser and desktop smoke acceptance**

Start the app with the project's normal local command and verify:

1. HTTP Aing and Agent Aing each create a relation whose full label and exact quote are visible.
2. Backfill changes relations but not transcript text.
3. Rename, alias, merge, split, suppress, history, restore, and undo survive re-Aing and restart.
4. Node labels remain centered on nodes; edge labels are full when revealed; no `…` is used for graph content.
5. One-hop expansion, collapse, filters, search, relation drawer, pending review, and both path modes work by pointer and keyboard.
6. Light/dark themes and reduced-motion behavior remain readable.
7. The 1,000/5,000 fixture can open, settle, expand, show all, and find a path without freezing.
8. Missing model, failed relation extraction, corrupt new index, and semantic-empty data all leave notes and the co-occurrence fallback usable.

- [ ] **Step 9: Commit the completion proof**

Run:

```bash
git add src/lib/relationBackfill.ts src/lib/relationBackfill.test.ts src/lib/RelationBackfillDialog.svelte src/routes/ai/+page.svelte src/routes/graph/+page.svelte src-tauri/src/graph/e2e_tests.rs src-tauri/src/graph/large_fixture.rs src-tauri/src/graph/mod.rs DESIGN.md
git commit -m "test(graph): prove semantic graph phase one"
```

Expected: the branch contains the full closed loop and its executable acceptance evidence.

---

## Final Verification Checklist

- [ ] `aing.json` v1 reads without rewrite; v2 writes stable mention/evidence/relation IDs.
- [ ] HTTP and Agent pass the same validator and persist equivalent model facts.
- [ ] Every published AI relation has exact evidence plus provider/model provenance.
- [ ] `knowledge-overrides.json` is append-only, cross-process locked, atomically replaced, and visibly fails closed on corruption.
- [ ] Rename preserves stable IDs; old `e:` links resolve; same-name entities can coexist after evidence-level split.
- [ ] Re-Aing, backfill, restart, and SQLite rebuild preserve every human decision.
- [ ] High-confidence, pending, and raw-only facts remain strictly separated.
- [ ] Semantic edges dominate visually; co-occurrence is optional and visibly weak.
- [ ] Full graph labels are never replaced with ellipses or detached label cards.
- [ ] Path costs and ties are deterministic and every semantic step opens evidence.
- [ ] Backfill is explicit, cancellable, resumable, and transcript-preserving.
- [ ] Graph failures never interrupt the core note lifecycle.
- [ ] Full test suite, release performance fixture, browser smoke, and desktop smoke pass.
