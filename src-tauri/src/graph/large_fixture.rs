use super::canonical::{
    CanonicalEntity, CanonicalEvidence, CanonicalGraph, CanonicalMention, CanonicalRelation,
    RelationOrigin, RelationStatus,
};
use super::index;
#[cfg(test)]
use super::{path, query};
#[cfg(test)]
use crate::graph::query::GraphFilter;
use crate::store::RelationPredicate;
use std::collections::BTreeMap;
use std::path::Path;

const ENTITY_COUNT: usize = 1_000;
const RELATION_COUNT: usize = 5_000;
const COOCCURRENCE_COUNT: usize = 1_500;

fn entity_id(index: usize) -> String {
    format!("kg_{index:04}")
}

pub(crate) fn deterministic_large_graph() -> CanonicalGraph {
    let entities = (0..ENTITY_COUNT)
        .map(|index| {
            let id = entity_id(index);
            (
                id.clone(),
                CanonicalEntity {
                    id,
                    kind: match index % 5 {
                        0 => "person",
                        1 => "project",
                        2 => "org",
                        3 => "term",
                        _ => "task",
                    }
                    .into(),
                    name: format!("Fixture Entity {index:04}"),
                    aliases: vec![format!("Fixture Alias {index:04}")],
                    confirmed: index % 3 == 0,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();

    // Each unique two-entity note contributes exactly one undirected
    // co-occurrence edge. 1,000 cycle neighbours + 500 second neighbours are
    // disjoint, yielding exactly 1,500 distinct edges.
    let mut pairs = (0..ENTITY_COUNT)
        .map(|left| (left, (left + 1) % ENTITY_COUNT))
        .chain((0..500).map(|left| (left, left + 2)))
        .collect::<Vec<_>>();
    // Preserve the direct 0000 -> 0017 path used by the deterministic path
    // assertion without adding a sixteenth-hundredth weak pair.
    pairs[0] = (0, 17);
    assert_eq!(pairs.len(), COOCCURRENCE_COUNT);
    let mut mentions = pairs
        .iter()
        .enumerate()
        .flat_map(|(note_index, (left, right))| {
            let note_id = format!("fixture-co-{note_index:04}");
            [
                CanonicalMention {
                    id: format!("mn_co_{note_index:04}_a"),
                    note_id: note_id.clone(),
                    entity_id: entity_id(*left),
                    paragraph_index: 0,
                    start: 0,
                    end: 8,
                    quote: format!("Entity {left:04}"),
                },
                CanonicalMention {
                    id: format!("mn_co_{note_index:04}_b"),
                    note_id,
                    entity_id: entity_id(*right),
                    paragraph_index: 0,
                    start: 12,
                    end: 20,
                    quote: format!("Entity {right:04}"),
                },
            ]
        })
        .collect::<Vec<_>>();

    // Semantic evidence must reference real canonical mentions as production
    // data does; otherwise the large fixture can hide broken evidence joins.
    mentions.extend((0..RELATION_COUNT).flat_map(|index| {
        let (subject, object) = pairs[index % pairs.len()];
        let note_id = format!("fixture-semantic-{index:04}");
        let subject_quote = format!("Fixture Entity {subject:04}");
        let object_quote = format!("Fixture Entity {object:04}");
        let object_start = subject_quote.chars().count() + " relates to ".chars().count();
        [
            CanonicalMention {
                id: format!("mn_semantic_{index:04}_subject"),
                note_id: note_id.clone(),
                entity_id: entity_id(subject),
                paragraph_index: 0,
                start: 0,
                end: subject_quote.chars().count(),
                quote: subject_quote,
            },
            CanonicalMention {
                id: format!("mn_semantic_{index:04}_object"),
                note_id,
                entity_id: entity_id(object),
                paragraph_index: 0,
                start: object_start,
                end: object_start + object_quote.chars().count(),
                quote: object_quote,
            },
        ]
    }));

    let predicates = [
        "participates_in",
        "responsible_for",
        "belongs_to",
        "uses",
        "depends_on",
        "produces",
        "assigned_to",
        "occurs_at",
    ];
    let relations = (0..RELATION_COUNT)
        .map(|index| {
            let (subject, object) = pairs[index % pairs.len()];
            let note_id = format!("fixture-semantic-{index:04}");
            let subject_mention = format!("mn_semantic_{index:04}_subject");
            let object_mention = format!("mn_semantic_{index:04}_object");
            let quote =
                format!("Fixture Entity {subject:04} relates to Fixture Entity {object:04}");
            CanonicalRelation {
                id: format!("cr_fixture_{index:04}"),
                subject_id: entity_id(subject),
                predicate: RelationPredicate {
                    kind: predicates[index % predicates.len()].into(),
                    label: None,
                },
                object_id: entity_id(object),
                confidence: 0.8 + ((index % 20) as f64 / 100.0),
                valid_from: None,
                valid_to: None,
                status: RelationStatus::Current,
                origin: RelationOrigin::Model,
                provider: Some("fixture".into()),
                model: Some("semantic-graph-large-v1".into()),
                note_ids: vec![note_id.clone()],
                evidence: vec![CanonicalEvidence {
                    id: format!("ev_fixture_{index:04}"),
                    note_id,
                    paragraph_index: 0,
                    start: 0,
                    end: quote.chars().count(),
                    quote,
                    source_seqs: vec![index as u64 + 1],
                    source_hash: format!("fixture-source-{index:04}"),
                    subject_mentions: vec![subject_mention],
                    object_mentions: vec![object_mention],
                }],
            }
        })
        .collect::<Vec<_>>();

    CanonicalGraph {
        entities,
        mentions,
        relations,
        pending: Vec::new(),
    }
}

/// Debug/test importer for manual desktop inspection. It refuses every target
/// except a brand-new, explicitly named child of the operating-system temp
/// directory, so it cannot seed or overwrite the user's configured library.
#[allow(dead_code)]
pub(crate) fn import_semantic_graph_large_fixture(
    data_root: &Path,
) -> anyhow::Result<index::BuildStats> {
    let temp = std::env::temp_dir().canonicalize()?;
    let parent = data_root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("fixture root has no parent"))?
        .canonicalize()?;
    anyhow::ensure!(
        parent.starts_with(&temp),
        "fixture root must be inside the OS temp directory"
    );
    let name = data_root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::anyhow!("fixture root name is not valid UTF-8"))?;
    anyhow::ensure!(
        name.starts_with("aing-semantic-fixture-"),
        "fixture root must use the aing-semantic-fixture- prefix"
    );
    anyhow::ensure!(!data_root.exists(), "fixture root must not already exist");
    std::fs::create_dir(data_root)?;
    index::rebuild_atomic(data_root, &deterministic_large_graph())
}

#[cfg(test)]
fn all_filter() -> GraphFilter {
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
fn semantic_graph_large_import_requires_a_new_explicit_temp_root() {
    let parent = tempfile::tempdir().unwrap();
    assert!(import_semantic_graph_large_fixture(parent.path()).is_err());
    let wrong_name = parent.path().join("not-a-fixture");
    assert!(import_semantic_graph_large_fixture(&wrong_name).is_err());
    let allowed = parent.path().join("aing-semantic-fixture-safe");
    let stats = import_semantic_graph_large_fixture(&allowed).unwrap();
    assert_eq!(
        (stats.entities, stats.relations, stats.evidence),
        (1_000, 5_000, 5_000)
    );
    let graph = query::semantic_graph(&allowed, &all_filter()).unwrap();
    assert_eq!(graph.nodes.len(), ENTITY_COUNT);
    assert_eq!(graph.semantic_edges.len(), RELATION_COUNT);
    assert_eq!(graph.cooccurrence_edges.len(), COOCCURRENCE_COUNT);

    let detail = query::relation_detail(&allowed, "cr_fixture_0000")
        .unwrap()
        .unwrap();
    assert_eq!(detail.evidence.len(), 1);
    assert_eq!(
        detail.evidence[0].subject_mentions,
        vec!["mn_semantic_0000_subject"]
    );
    assert_eq!(
        detail.evidence[0].object_mentions,
        vec!["mn_semantic_0000_object"]
    );
    let subject_mentions = query::entity_mentions(&allowed, "kg_0000").unwrap();
    let object_mentions = query::entity_mentions(&allowed, "kg_0017").unwrap();
    assert!(subject_mentions
        .iter()
        .any(|mention| mention.id == "mn_semantic_0000_subject"));
    assert!(object_mentions
        .iter()
        .any(|mention| mention.id == "mn_semantic_0000_object"));
    let connection = index::open_readonly(&allowed).unwrap();
    let resolvable_evidence_mentions: usize = connection
        .query_row(
            "SELECT COUNT(*) FROM (\
               SELECT mention.id FROM relation_evidence evidence, json_each(evidence.subject_mentions) refs \
                 JOIN entity_mentions mention ON mention.id = refs.value \
               UNION ALL \
               SELECT mention.id FROM relation_evidence evidence, json_each(evidence.object_mentions) refs \
                 JOIN entity_mentions mention ON mention.id = refs.value\
             )",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(resolvable_evidence_mentions, RELATION_COUNT * 2);

    let first = path::shortest_path(&allowed, "kg_0000", "kg_0017", &all_filter())
        .unwrap()
        .unwrap();
    let second = path::shortest_path(&allowed, "kg_0000", "kg_0017", &all_filter())
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&second).unwrap(),
        "equal-cost path ties must resolve identically"
    );
    assert_eq!(first.entity_ids, vec!["kg_0000", "kg_0017"]);
    assert_eq!(first.steps.len(), 1);
    assert_eq!(first.steps[0].id, "cr_fixture_0000");
    assert_eq!(first.steps[0].evidence_count, 1);
    assert_eq!(first.steps[0].note_count, 1);
    assert!((first.total_cost - 1.4).abs() < f64::EPSILON);
    assert!(import_semantic_graph_large_fixture(&allowed).is_err());
}

#[test]
#[ignore = "release-only semantic graph performance budget"]
fn semantic_graph_large_rebuild_query_and_path_stay_within_release_budgets() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("aing-semantic-fixture-release");

    let rebuild_started = std::time::Instant::now();
    let stats = import_semantic_graph_large_fixture(&root).unwrap();
    let rebuild_elapsed = rebuild_started.elapsed();
    assert_eq!(stats.entities, ENTITY_COUNT);
    assert_eq!(stats.relations, RELATION_COUNT);
    assert_eq!(stats.evidence, RELATION_COUNT);

    let query_started = std::time::Instant::now();
    let graph = query::semantic_graph(&root, &all_filter()).unwrap();
    let query_elapsed = query_started.elapsed();
    assert_eq!(graph.nodes.len(), ENTITY_COUNT);
    assert_eq!(graph.semantic_edges.len(), RELATION_COUNT);
    assert_eq!(graph.cooccurrence_edges.len(), COOCCURRENCE_COUNT);

    let path_started = std::time::Instant::now();
    let found = path::shortest_path(&root, "kg_0000", "kg_0017", &all_filter())
        .unwrap()
        .unwrap();
    let path_elapsed = path_started.elapsed();
    assert_eq!(
        found.entity_ids.first().map(String::as_str),
        Some("kg_0000")
    );
    assert_eq!(found.entity_ids.last().map(String::as_str), Some("kg_0017"));

    eprintln!(
        "semantic_graph_large timings: rebuild={rebuild_elapsed:?} query={query_elapsed:?} path={path_elapsed:?}"
    );
    assert!(rebuild_elapsed < std::time::Duration::from_secs(5));
    assert!(query_elapsed < std::time::Duration::from_millis(500));
    assert!(path_elapsed < std::time::Duration::from_millis(500));
}
