use crate::models::{KnowledgeEdge, SearchHit};
use crate::simple_graph_optimizer::WeightedMultiGraph;
use serde::Serialize;
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
pub struct ContextPath {
    pub start_node: Uuid,
    pub end_node: Uuid,
    pub total_cost: f64,
    pub nodes: Vec<Uuid>,
}

/// Traversing an edge against its direction costs this much extra.
const REVERSE_TRAVERSAL_PENALTY: f64 = 0.15;
/// Floor for `confidence` in the cost division, so a malformed zero-confidence
/// edge can't produce an infinite traversal cost.
const MIN_CONFIDENCE: f64 = 0.05;

/// Weighted graph routing over persisted knowledge nodes: the cheapest paths
/// between retrieval hits, where an edge's traversal cost is
/// `cost / confidence` — the exact inverse of the `coupling_weight` the L1
/// community detection uses, so "shortest path" means "route through the
/// strongest, most certain relations" in both layers.
///
/// When more paths exist than `max_paths`, CROSS-FILE paths are kept first:
/// two hits sitting in the same file are trivially adjacent and tell the
/// reader nothing, while a path bridging files is the actual context.
pub fn best_context_paths(
    hits: &[SearchHit],
    edges: &[KnowledgeEdge],
    max_paths: usize,
) -> Vec<ContextPath> {
    let mut seen = HashSet::new();
    let starts = hits
        .iter()
        .filter_map(|hit| hit.node_id)
        .filter(|node_id| seen.insert(*node_id))
        .collect::<Vec<_>>();
    let file_of: HashMap<Uuid, &str> = hits
        .iter()
        .filter_map(|hit| Some((hit.node_id?, hit.file_path.as_deref()?)))
        .collect();

    // Canonical edge order: the SQL load has no ORDER BY, and equal-cost
    // routes tie-break by insertion order, so unsorted edges would make the
    // reported path (not its cost) vary between runs.
    let mut ordered: Vec<&KnowledgeEdge> = edges.iter().collect();
    ordered.sort_by_key(|edge| edge.id);

    let mut graph = WeightedMultiGraph::new();
    for edge in ordered {
        let traversal = edge.cost / edge.confidence.clamp(MIN_CONFIDENCE, 1.0);
        graph.add_edge(edge.source_node_id, edge.target_node_id, traversal);
        graph.add_edge(
            edge.target_node_id,
            edge.source_node_id,
            traversal + REVERSE_TRAVERSAL_PENALTY,
        );
    }

    let mut paths = Vec::new();
    for (idx, start) in starts.iter().enumerate() {
        for target in starts.iter().skip(idx + 1) {
            if let Some(path) = graph.shortest_path(*start, *target) {
                paths.push(ContextPath {
                    start_node: *start,
                    end_node: *target,
                    total_cost: path.total_cost,
                    nodes: path.nodes,
                });
            }
        }
    }
    paths.sort_by(|a, b| {
        let a_same = same_file(&file_of, a);
        let b_same = same_file(&file_of, b);
        a_same
            .cmp(&b_same)
            .then(
                a.total_cost
                    .partial_cmp(&b.total_cost)
                    .unwrap_or(Ordering::Equal),
            )
            .then_with(|| (a.start_node, a.end_node).cmp(&(b.start_node, b.end_node)))
    });
    paths.truncate(max_paths);
    paths
}

/// True when both endpoints are known to live in the same file (unknown
/// files count as cross-file, so they are not unfairly demoted).
fn same_file(file_of: &HashMap<Uuid, &str>, path: &ContextPath) -> bool {
    match (file_of.get(&path.start_node), file_of.get(&path.end_node)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::EdgeKind;
    use serde_json::json;

    fn hit(node_id: Uuid, file: &str) -> SearchHit {
        SearchHit {
            chunk_id: Uuid::new_v4(),
            node_id: Some(node_id),
            file_path: Some(file.to_string()),
            line_start: Some(1),
            line_end: Some(1),
            score: 1.0,
            content: String::new(),
            metadata: json!({}),
        }
    }

    fn edge(source: Uuid, target: Uuid, cost: f64, confidence: f64) -> KnowledgeEdge {
        KnowledgeEdge {
            id: Uuid::new_v4(),
            repo_id: Uuid::nil(),
            source_node_id: source,
            target_node_id: target,
            kind: EdgeKind::Calls,
            cost,
            confidence,
            metadata: json!({}),
        }
    }

    /// Path cost is `cost / confidence` — the inverse of the community
    /// layer's coupling weight, so both layers share one strength model.
    #[test]
    fn path_cost_is_cost_over_confidence() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let edges = vec![edge(a, b, 0.35, 0.7)];
        let hits = vec![hit(a, "x.rs"), hit(b, "y.rs")];
        let paths = best_context_paths(&hits, &edges, 8);
        assert_eq!(paths.len(), 1);
        assert!((paths[0].total_cost - 0.5).abs() < 1e-9, "0.35 / 0.7 = 0.5");
    }

    /// A low-confidence shortcut must not beat a parser-certain route of
    /// equal raw cost.
    #[test]
    fn certain_route_beats_equal_cost_heuristic_shortcut() {
        let (a, mid, b) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let edges = vec![
            // Heuristic direct edge: raw cost 0.4 but confidence 0.5 → 0.8.
            edge(a, b, 0.4, 0.5),
            // Certain two-hop route: 0.2 + 0.2 = 0.4 effective.
            edge(a, mid, 0.2, 1.0),
            edge(mid, b, 0.2, 1.0),
        ];
        let hits = vec![hit(a, "x.rs"), hit(b, "y.rs")];
        let paths = best_context_paths(&hits, &edges, 8);
        assert_eq!(paths[0].nodes, vec![a, mid, b]);
        assert!((paths[0].total_cost - 0.4).abs() < 1e-9);
    }

    /// When truncating, cross-file paths are kept ahead of cheaper but
    /// trivial same-file adjacencies.
    #[test]
    fn cross_file_paths_outrank_cheaper_same_file_paths() {
        let (a1, a2, b1) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let edges = vec![
            edge(a1, a2, 0.1, 1.0), // same file, cheapest
            edge(a2, b1, 0.5, 1.0), // crosses into the other file
        ];
        let hits = vec![hit(a1, "a.rs"), hit(a2, "a.rs"), hit(b1, "b.rs")];
        let paths = best_context_paths(&hits, &edges, 2);
        assert_eq!(paths.len(), 2);
        // Both kept slots go to the cross-file connections.
        assert_eq!(paths[0].end_node, b1);
        assert_eq!(paths[1].end_node, b1);
    }

    /// Equal-cost alternatives must resolve to the same route regardless of
    /// the order edges arrive from the database.
    #[test]
    fn equal_cost_route_choice_is_deterministic() {
        let (s, via_a, via_b, t) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let diamond = vec![
            edge(s, via_a, 0.2, 1.0),
            edge(via_a, t, 0.2, 1.0),
            edge(s, via_b, 0.2, 1.0),
            edge(via_b, t, 0.2, 1.0),
        ];
        let hits = vec![hit(s, "x.rs"), hit(t, "y.rs")];
        let forward = best_context_paths(&hits, &diamond, 8);
        let mut reversed_input = diamond.clone();
        reversed_input.reverse();
        let backward = best_context_paths(&hits, &reversed_input, 8);
        assert_eq!(forward[0].nodes, backward[0].nodes);
        assert_eq!(forward[0].total_cost, backward[0].total_cost);
    }
}
