use super::query::{self, GraphFilter};
use crate::ipc;
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelationOrigin {
    Manual,
    Confirmed,
    Model,
    Cooccurrence,
    UserAssertion,
}

impl RelationOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Confirmed => "confirmed",
            Self::Model => "model",
            Self::Cooccurrence => "cooccurrence",
            Self::UserAssertion => "user_assertion",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PathEdge {
    pub id: String,
    pub subject_id: String,
    pub object_id: String,
    pub predicate_type: String,
    pub predicate_label: Option<String>,
    pub origin: RelationOrigin,
    pub confidence: f64,
    pub evidence_count: i64,
    pub note_count: i64,
}

fn edge_cost_micros(edge: &PathEdge) -> u64 {
    match edge.origin {
        RelationOrigin::Manual | RelationOrigin::Confirmed | RelationOrigin::UserAssertion => {
            1_000_000
        }
        RelationOrigin::Model => (1.2 + (1.0 - edge.confidence))
            .mul_add(1_000_000.0, 0.0)
            .round() as u64,
        RelationOrigin::Cooccurrence => 3_000_000,
    }
}

pub(crate) fn edge_cost(edge: &PathEdge) -> f64 {
    edge_cost_micros(edge) as f64 / 1_000_000.0
}

fn confidence_micros(confidence: f64) -> u64 {
    (confidence.clamp(0.0, 1.0) * 1_000_000.0).round() as u64
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PathRank {
    cost_micros: u64,
    hops: usize,
    min_confidence: Reverse<u64>,
    stable_path_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Traversal {
    edge_index: usize,
    from_id: String,
    to_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueueState {
    rank: PathRank,
    current: String,
    entity_ids: Vec<String>,
    traversals: Vec<Traversal>,
}

impl Ord for QueueState {
    fn cmp(&self, other: &Self) -> Ordering {
        (
            &self.rank,
            &self.current,
            &self.entity_ids,
            &self.traversals,
        )
            .cmp(&(
                &other.rank,
                &other.current,
                &other.entity_ids,
                &other.traversals,
            ))
    }
}

impl PartialOrd for QueueState {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub(crate) fn shortest_path_from_edges(
    start: &str,
    end: &str,
    edges: &[PathEdge],
) -> Option<ipc::KnowledgePath> {
    if start == end {
        return Some(ipc::KnowledgePath {
            entity_ids: vec![start.into()],
            steps: Vec::new(),
            total_cost: 0.0,
        });
    }
    let start_rank = PathRank {
        cost_micros: 0,
        hops: 0,
        min_confidence: Reverse(u64::MAX),
        stable_path_ids: Vec::new(),
    };
    let mut queue = BinaryHeap::from([Reverse(QueueState {
        rank: start_rank.clone(),
        current: start.into(),
        entity_ids: vec![start.into()],
        traversals: Vec::new(),
    })]);
    let mut adjacency = BTreeMap::<String, Vec<usize>>::new();
    for (edge_index, edge) in edges.iter().enumerate() {
        adjacency
            .entry(edge.subject_id.clone())
            .or_default()
            .push(edge_index);
        adjacency
            .entry(edge.object_id.clone())
            .or_default()
            .push(edge_index);
    }
    for edge_indexes in adjacency.values_mut() {
        edge_indexes.sort_by(|left, right| edges[*left].id.cmp(&edges[*right].id));
    }
    let mut labels = BTreeMap::<(String, BTreeSet<String>), Vec<PathRank>>::new();
    labels.insert(
        (start.into(), BTreeSet::from([start.into()])),
        vec![start_rank],
    );

    while let Some(Reverse(state)) = queue.pop() {
        let visited = state.entity_ids.iter().cloned().collect::<BTreeSet<_>>();
        let label_key = (state.current.clone(), visited.clone());
        if !labels
            .get(&label_key)
            .is_some_and(|known| known.contains(&state.rank))
        {
            continue;
        }
        if state.current == end {
            let steps = state
                .traversals
                .iter()
                .map(|traversal| {
                    let edge = &edges[traversal.edge_index];
                    ipc::KnowledgePathStep {
                        id: edge.id.clone(),
                        from_id: traversal.from_id.clone(),
                        to_id: traversal.to_id.clone(),
                        subject_id: edge.subject_id.clone(),
                        object_id: edge.object_id.clone(),
                        predicate_type: edge.predicate_type.clone(),
                        predicate_label: edge.predicate_label.clone(),
                        direction: if traversal.from_id == edge.subject_id {
                            "forward".into()
                        } else {
                            "reverse".into()
                        },
                        origin: edge.origin.as_str().into(),
                        confidence: edge.confidence,
                        evidence_count: edge.evidence_count,
                        note_count: edge.note_count,
                    }
                })
                .collect();
            return Some(ipc::KnowledgePath {
                entity_ids: state.entity_ids,
                steps,
                total_cost: state.rank.cost_micros as f64 / 1_000_000.0,
            });
        }

        for edge_index in adjacency.get(&state.current).into_iter().flatten().copied() {
            let edge = &edges[edge_index];
            let next = if edge.subject_id == state.current {
                edge.object_id.as_str()
            } else if edge.object_id == state.current {
                edge.subject_id.as_str()
            } else {
                continue;
            };
            if state.entity_ids.iter().any(|entity_id| entity_id == next) {
                continue;
            }
            let min_confidence = if state.rank.hops == 0 {
                confidence_micros(edge.confidence)
            } else {
                state
                    .rank
                    .min_confidence
                    .0
                    .min(confidence_micros(edge.confidence))
            };
            let mut stable_path_ids = state.rank.stable_path_ids.clone();
            stable_path_ids.push(edge.id.clone());
            let next_rank = PathRank {
                cost_micros: state
                    .rank
                    .cost_micros
                    .saturating_add(edge_cost_micros(edge)),
                hops: state.rank.hops + 1,
                min_confidence: Reverse(min_confidence),
                stable_path_ids,
            };
            let mut entity_ids = state.entity_ids.clone();
            entity_ids.push(next.into());
            let mut next_visited = visited.clone();
            next_visited.insert(next.into());
            let next_key = (next.to_string(), next_visited);
            let known = labels.entry(next_key).or_default();
            if known
                .iter()
                .any(|candidate| path_rank_safely_dominates(candidate, &next_rank))
            {
                continue;
            }
            known.retain(|candidate| !path_rank_safely_dominates(&next_rank, candidate));
            known.push(next_rank.clone());
            let mut traversals = state.traversals.clone();
            traversals.push(Traversal {
                edge_index,
                from_id: state.current.clone(),
                to_id: next.into(),
            });
            queue.push(Reverse(QueueState {
                rank: next_rank,
                current: next.into(),
                entity_ids,
                traversals,
            }));
        }
    }
    None
}

fn path_rank_safely_dominates(left: &PathRank, right: &PathRank) -> bool {
    if left.cost_micros != right.cost_micros {
        return left.cost_micros < right.cost_micros;
    }
    if left.hops != right.hops {
        return left.hops < right.hops;
    }
    left.min_confidence.0 >= right.min_confidence.0 && left.stable_path_ids <= right.stable_path_ids
}

fn semantic_origin(origin: &str) -> anyhow::Result<RelationOrigin> {
    match origin {
        "manual" => Ok(RelationOrigin::Manual),
        "confirmed" => Ok(RelationOrigin::Confirmed),
        "model" => Ok(RelationOrigin::Model),
        "user_assertion" => Ok(RelationOrigin::UserAssertion),
        _ => anyhow::bail!("semantic relation has an unknown origin"),
    }
}

pub fn shortest_path(
    data_root: &Path,
    start: &str,
    end: &str,
    filter: &GraphFilter,
) -> anyhow::Result<Option<ipc::KnowledgePath>> {
    let context = query::open_read_context(data_root)?;
    let Some(start) = query::resolve_entity_from_context(&context, start) else {
        return Ok(None);
    };
    let Some(end) = query::resolve_entity_from_context(&context, end) else {
        return Ok(None);
    };
    let graph = query::semantic_graph_from_context(&context, filter)?;
    let live = graph
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if !live.contains(start.as_str()) || !live.contains(end.as_str()) {
        return Ok(None);
    }
    let mut edges = graph
        .semantic_edges
        .into_iter()
        .map(|edge| {
            Ok(PathEdge {
                id: edge.id,
                subject_id: edge.subject_id,
                object_id: edge.object_id,
                predicate_type: edge.predicate_type,
                predicate_label: edge.predicate_label,
                origin: semantic_origin(&edge.origin)?,
                confidence: edge.confidence,
                evidence_count: edge.evidence_count,
                note_count: edge.note_count,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if filter.include_cooccurrence {
        edges.extend(graph.cooccurrence_edges.into_iter().map(|edge| PathEdge {
            id: crate::store::stable_id("co_", &[edge.a.clone(), edge.b.clone()]),
            subject_id: edge.a,
            object_id: edge.b,
            predicate_type: "cooccurrence".into(),
            predicate_label: Some(format!("共同出现（{} 篇）", edge.weight)),
            origin: RelationOrigin::Cooccurrence,
            confidence: 0.0,
            evidence_count: 0,
            note_count: edge.weight,
        }));
    }
    edges.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(shortest_path_from_edges(&start, &end, &edges))
}

#[cfg(test)]
mod tests {
    use super::{edge_cost, shortest_path_from_edges, PathEdge, RelationOrigin};

    fn edge(
        id: &str,
        subject_id: &str,
        object_id: &str,
        origin: RelationOrigin,
        confidence: f64,
    ) -> PathEdge {
        PathEdge {
            id: id.into(),
            subject_id: subject_id.into(),
            object_id: object_id.into(),
            predicate_type: "related_to".into(),
            predicate_label: None,
            origin,
            confidence,
            evidence_count: 1,
            note_count: 1,
        }
    }

    fn manual_edge() -> PathEdge {
        edge("manual", "a", "b", RelationOrigin::Manual, 1.0)
    }

    fn model_edge(confidence: f64) -> PathEdge {
        edge("model", "a", "b", RelationOrigin::Model, confidence)
    }

    fn cooccurrence_edge() -> PathEdge {
        edge("co", "a", "b", RelationOrigin::Cooccurrence, 0.0)
    }

    #[test]
    fn edge_costs_match_the_public_policy_exactly() {
        assert_eq!(edge_cost(&manual_edge()), 1.0);
        assert_eq!(edge_cost(&model_edge(0.9)), 1.3);
        assert_eq!(edge_cost(&cooccurrence_edge()), 3.0);
    }

    #[test]
    fn deterministic_path_ranks_cost_hops_confidence_then_lexical_ids() {
        let confidence_edges = vec![
            edge("r_ab", "a", "b", RelationOrigin::Manual, 0.8),
            edge("r_bd", "b", "d", RelationOrigin::Confirmed, 0.8),
            edge("r_ac", "a", "c", RelationOrigin::Manual, 0.9),
            edge("r_cd", "c", "d", RelationOrigin::Confirmed, 0.9),
        ];
        let path = shortest_path_from_edges("a", "d", &confidence_edges).unwrap();
        assert_eq!(
            path.steps
                .iter()
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>(),
            ["r_ac", "r_cd"]
        );

        let lexical_edges = vec![
            edge("r_za", "a", "b", RelationOrigin::Manual, 0.9),
            edge("r_zb", "b", "d", RelationOrigin::Confirmed, 0.9),
            edge("r_aa", "a", "c", RelationOrigin::Manual, 0.9),
            edge("r_ab", "c", "d", RelationOrigin::Confirmed, 0.9),
        ];
        let path = shortest_path_from_edges("a", "d", &lexical_edges).unwrap();
        assert_eq!(
            path.steps
                .iter()
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>(),
            ["r_aa", "r_ab"]
        );

        let hop_edges = vec![
            edge("co_direct", "a", "d", RelationOrigin::Cooccurrence, 0.0),
            edge("m1", "a", "b", RelationOrigin::Manual, 1.0),
            edge("m2", "b", "c", RelationOrigin::Manual, 1.0),
            edge("m3", "c", "d", RelationOrigin::Manual, 1.0),
        ];
        let path = shortest_path_from_edges("a", "d", &hop_edges).unwrap();
        assert_eq!(
            path.steps
                .iter()
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>(),
            ["co_direct"]
        );
    }

    #[test]
    fn shared_low_confidence_suffix_reopens_lexically_better_prefix() {
        let edges = vec![
            edge("z_prefix", "start", "via_z", RelationOrigin::Manual, 0.9),
            edge("z_join", "via_z", "join", RelationOrigin::Manual, 0.9),
            edge("a_prefix", "start", "via_a", RelationOrigin::Manual, 0.8),
            edge("a_join", "via_a", "join", RelationOrigin::Manual, 0.8),
            edge("shared_low", "join", "end", RelationOrigin::Manual, 0.7),
        ];

        let path = shortest_path_from_edges("start", "end", &edges).unwrap();
        assert_eq!(
            path.steps
                .iter()
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>(),
            ["a_prefix", "a_join", "shared_low"]
        );
    }

    #[test]
    fn reverse_traversal_preserves_the_stored_relation_direction() {
        let edges = vec![edge("r_ab", "a", "b", RelationOrigin::Confirmed, 1.0)];
        let path = shortest_path_from_edges("b", "a", &edges).unwrap();
        assert_eq!(path.steps[0].subject_id, "a");
        assert_eq!(path.steps[0].object_id, "b");
        assert_eq!(path.steps[0].from_id, "b");
        assert_eq!(path.steps[0].to_id, "a");
        assert_eq!(path.steps[0].direction, "reverse");
    }
}
