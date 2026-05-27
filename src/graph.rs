use crate::models::{KnowledgeEdge, SearchHit};
use crate::simple_graph_optimizer::WeightedMultiGraph;
use serde::Serialize;
use std::{cmp::Ordering, collections::HashSet};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
pub struct ContextPath {
    pub start_node: Uuid,
    pub end_node: Uuid,
    pub total_cost: f64,
    pub nodes: Vec<Uuid>,
}

/// Weighted graph routing over persisted knowledge nodes.
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
    let mut graph = WeightedMultiGraph::new();
    for edge in edges {
        graph.add_edge(edge.source_node_id, edge.target_node_id, edge.cost);
        graph.add_edge(edge.target_node_id, edge.source_node_id, edge.cost + 0.15);
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
        a.total_cost
            .partial_cmp(&b.total_cost)
            .unwrap_or(Ordering::Equal)
    });
    paths.truncate(max_paths);
    paths
}
