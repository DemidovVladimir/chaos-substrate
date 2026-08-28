//! Small in-memory adjacency over a slice of [`Edge`]s: neighbour lookup in
//! both directions and deterministic one-hop expansion from a seed set. Used by
//! the feature use-case to pull the immediate context of a community.

use crate::domain::model::{Edge, EdgeKind};
use std::collections::{BTreeSet, HashMap, HashSet};
use uuid::Uuid;

/// Undirected adjacency view over borrowed edges.
pub struct Adjacency<'a> {
    edges: &'a [Edge],
    /// node → indices into `edges` where the node is source or target.
    incident: HashMap<Uuid, Vec<usize>>,
}

impl<'a> Adjacency<'a> {
    /// Build the index. Edge order per node is deterministic (sorted by
    /// `(kind, source, target, id)`) regardless of input order.
    pub fn new(edges: &'a [Edge]) -> Self {
        let mut incident: HashMap<Uuid, Vec<usize>> = HashMap::new();
        for (i, e) in edges.iter().enumerate() {
            incident.entry(e.source_node_id).or_default().push(i);
            if e.target_node_id != e.source_node_id {
                incident.entry(e.target_node_id).or_default().push(i);
            }
        }
        for list in incident.values_mut() {
            list.sort_by(|&a, &b| edge_key(&edges[a]).cmp(&edge_key(&edges[b])));
        }
        Self { edges, incident }
    }

    /// Neighbours of `node` over edges of any of `kinds` (empty = all kinds),
    /// in either direction, deduplicated and sorted.
    pub fn neighbors(&self, node: Uuid, kinds: &[EdgeKind]) -> Vec<Uuid> {
        let mut out = BTreeSet::new();
        for &i in self.incident.get(&node).map(Vec::as_slice).unwrap_or(&[]) {
            let e = &self.edges[i];
            if !kinds.is_empty() && !kinds.contains(&e.kind) {
                continue;
            }
            out.insert(other_end(e, node));
        }
        out.into_iter().collect()
    }

    /// Edges leaving the seed set (either direction) over `kinds` (empty = all),
    /// paired with the node at the other end. Seeds are visited in sorted
    /// order, each seed's edges in `(kind, source, target, id)` order; an edge
    /// is reported once; edges between two seeds are excluded. At most `cap`
    /// entries (0 = unlimited).
    pub fn one_hop(&self, seeds: &[Uuid], kinds: &[EdgeKind], cap: usize) -> Vec<(Edge, Uuid)> {
        let seed_set: HashSet<Uuid> = seeds.iter().copied().collect();
        let mut ordered: Vec<Uuid> = seed_set.iter().copied().collect();
        ordered.sort();
        let mut seen_edges: HashSet<usize> = HashSet::new();
        let mut out = Vec::new();
        for seed in ordered {
            for &i in self.incident.get(&seed).map(Vec::as_slice).unwrap_or(&[]) {
                if cap > 0 && out.len() >= cap {
                    return out;
                }
                let e = &self.edges[i];
                if !kinds.is_empty() && !kinds.contains(&e.kind) {
                    continue;
                }
                let other = other_end(e, seed);
                if seed_set.contains(&other) || !seen_edges.insert(i) {
                    continue;
                }
                out.push((e.clone(), other));
            }
        }
        out
    }
}

fn edge_key(e: &Edge) -> (&'static str, Uuid, Uuid, Uuid) {
    (e.kind.as_str(), e.source_node_id, e.target_node_id, e.id)
}

fn other_end(e: &Edge, node: Uuid) -> Uuid {
    if e.source_node_id == node {
        e.target_node_id
    } else {
        e.source_node_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn edge(s: u128, t: u128, kind: EdgeKind) -> Edge {
        Edge {
            id: Uuid::from_u128(s * 1000 + t),
            repo_id: Uuid::nil(),
            source_node_id: id(s),
            target_node_id: id(t),
            kind,
            confidence: 1.0,
        }
    }

    fn sample() -> Vec<Edge> {
        vec![
            edge(3, 1, EdgeKind::Calls),
            edge(1, 2, EdgeKind::Contains),
            edge(2, 4, EdgeKind::Imports),
            edge(5, 2, EdgeKind::Calls),
        ]
    }

    #[test]
    fn neighbors_both_directions_and_kind_filter() {
        let edges = sample();
        let adj = Adjacency::new(&edges);
        assert_eq!(adj.neighbors(id(1), &[]), vec![id(2), id(3)]);
        assert_eq!(adj.neighbors(id(2), &[EdgeKind::Calls]), vec![id(5)]);
        assert!(adj.neighbors(id(9), &[]).is_empty());
    }

    #[test]
    fn one_hop_is_deterministic_and_skips_internal_edges() {
        let edges = sample();
        let adj = Adjacency::new(&edges);
        let a = adj.one_hop(&[id(2), id(1)], &[], 0);
        let a_ids: Vec<Uuid> = a.iter().map(|(_, o)| *o).collect();
        // 1→2 is internal; from seed 1: 3 (calls); from seed 2: 4 (imports), 5 (calls).
        assert_eq!(a_ids, vec![id(3), id(5), id(4)]);
        let mut reversed = edges.clone();
        reversed.reverse();
        let adj2 = Adjacency::new(&reversed);
        let b: Vec<Uuid> = adj2
            .one_hop(&[id(1), id(2)], &[], 0)
            .iter()
            .map(|(_, o)| *o)
            .collect();
        assert_eq!(a_ids, b);
        assert_eq!(adj.one_hop(&[id(1), id(2)], &[], 2).len(), 2);
        assert_eq!(adj.one_hop(&[id(2)], &[EdgeKind::Imports], 0).len(), 1);
    }
}
