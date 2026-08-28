//! L1 community ("feature") detection over the L0 graph.
//!
//! Deterministic multi-level **Louvain** modularity optimisation with all
//! randomness removed: nodes are visited in canonical `stable_id` order and
//! ties break toward the smallest community representative, so the same
//! `(nodes, edges)` always yields the same partition. A post-pass
//! ([`consolidate_tiny_communities`]) folds fragments smaller than
//! [`MIN_COMMUNITY_SIZE`] into their folder-preferred best-coupled neighbour
//! (isolates into the folder-dominant community), so the layer holds features,
//! not per-file singletons.
//!
//! Community ids are `scoped_id(repo_id, "community:" + smallest member
//! stable_id)`, stable across re-indexes. The synthetic `Repository` node is
//! excluded: it contains every file and would collapse the repo into one
//! community.

use crate::domain::hash;
use crate::domain::merkle;
use crate::domain::model::{Community, Edge, EdgeKind, Node, NodeKind};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

/// Below this absolute modularity gain a further Louvain level is not worth it.
const MIN_MODULARITY_GAIN: f64 = 1e-7;
/// Hard cap on Louvain levels (convergence is normally 2-4).
const MAX_LEVELS: usize = 32;
/// Hard cap on local-moving passes within a single level.
const MAX_PASSES_PER_LEVEL: usize = 64;
/// Representative members surfaced per community.
const TOP_MEMBERS: usize = 8;
/// Communities below this size are merge candidates: fewer than 4 members is
/// "less than one substantive file" — a fragment, not a feature.
pub const MIN_COMMUNITY_SIZE: usize = 4;
/// Fixpoint cap for the consolidation pass (converges in 1-2 in practice).
const MAX_MERGE_PASSES: usize = 8;
/// Modularity resolution γ (1.0 = standard).
const RESOLUTION: f64 = 1.0;

/// A detected community. `members` are sorted by member `stable_id`.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectedCommunity {
    /// `scoped_id(repo_id, "community:" + smallest member stable_id)`.
    pub id: Uuid,
    /// Dominant top-level path segment + top member name.
    pub label: String,
    pub members: Vec<Uuid>,
    /// Member names by weighted degree (top [`TOP_MEMBERS`]).
    pub top_members: Vec<String>,
    /// Member file paths, sorted and deduplicated.
    pub files: Vec<String>,
}

/// Result of one detection run; `communities` are in deterministic order.
#[derive(Debug, Clone, PartialEq)]
pub struct Detection {
    pub communities: Vec<DetectedCommunity>,
    pub modularity: f64,
    pub levels: usize,
    /// Tiny communities folded into a neighbour by the consolidation pass.
    pub merged_fragments: usize,
}

/// Coupling weight of an L0 edge: its confidence, halved for `Contains`
/// (structural containment couples less than an import or call).
fn coupling_weight(edge: &Edge) -> f64 {
    let c = f64::from(edge.confidence.clamp(0.0, 1.0));
    match edge.kind {
        EdgeKind::Contains => 0.5 * c,
        _ => c,
    }
}

/// Repo-relative path a node's `stable_id` encodes (`file:{path}` or
/// `{lang}:{path}:{kind}:{name}`); empty for the repository node.
pub fn node_path(stable_id: &str) -> &str {
    if let Some(rest) = stable_id.strip_prefix("file:") {
        return rest;
    }
    let mut parts = stable_id.splitn(3, ':');
    let _lang = parts.next();
    parts.next().unwrap_or("")
}

fn parent_dir(path: &str) -> &str {
    path.rfind('/').map(|p| &path[..p]).unwrap_or("")
}

/// Detect communities over the L0 graph. Pure and deterministic.
pub fn detect(repo_id: Uuid, nodes: &[Node], edges: &[Edge]) -> Detection {
    let mut indexed: Vec<&Node> = nodes
        .iter()
        .filter(|n| n.kind != NodeKind::Repository)
        .collect();
    indexed.sort_by(|a, b| a.stable_id.cmp(&b.stable_id).then(a.id.cmp(&b.id)));
    let n = indexed.len();
    let index_of: HashMap<Uuid, usize> =
        indexed.iter().enumerate().map(|(i, n)| (n.id, i)).collect();
    if n == 0 {
        return Detection {
            communities: Vec::new(),
            modularity: 0.0,
            levels: 0,
            merged_fragments: 0,
        };
    }

    // Symmetric weight per unordered pair (parallel/both-direction edges summed).
    let mut pair_weight: BTreeMap<(usize, usize), f64> = BTreeMap::new();
    for edge in edges {
        let (Some(&s), Some(&t)) = (
            index_of.get(&edge.source_node_id),
            index_of.get(&edge.target_node_id),
        ) else {
            continue;
        };
        if s == t {
            continue;
        }
        let key = if s < t { (s, t) } else { (t, s) };
        *pair_weight.entry(key).or_insert(0.0) += coupling_weight(edge);
    }

    let base = Graph::from_pairs(n, &pair_weight);
    let mut node_to_comm: Vec<usize> = (0..n).collect();
    let mut graph = base.clone();
    let mut modularity = graph.modularity(&(0..graph.n).collect::<Vec<_>>());
    let mut levels = 0usize;
    for _ in 0..MAX_LEVELS {
        let level_comm = graph.one_level();
        let new_modularity = graph.modularity(&level_comm);
        levels += 1;
        for slot in node_to_comm.iter_mut() {
            *slot = level_comm[*slot];
        }
        let improved = new_modularity - modularity > MIN_MODULARITY_GAIN;
        modularity = new_modularity;
        if distinct_count(&level_comm) == graph.n || !improved {
            break;
        }
        graph = graph.induce(&level_comm);
    }

    let labels = compact_labels(&node_to_comm);
    let (labels, merged_fragments) = consolidate_tiny_communities(&indexed, &pair_weight, labels);
    let labels = compact_labels(&labels);
    let k = distinct_count(&labels);
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); k];
    for (idx, &c) in labels.iter().enumerate() {
        members[c].push(idx);
    }

    let mut communities: Vec<DetectedCommunity> = members
        .iter()
        .map(|m| build_community(repo_id, &indexed, m, &base.degree))
        .collect();
    communities.sort_by(|a, b| a.id.cmp(&b.id));
    Detection {
        communities,
        modularity,
        levels,
        merged_fragments,
    }
}

/// Convert a detection into persistable [`Community`] rows plus
/// `(community_id, node_id)` member pairs; `subtree_hash` is the Merkle
/// roll-up of the member files under `file_hashes`.
pub fn to_communities(
    det: &Detection,
    repo_id: Uuid,
    file_hashes: &HashMap<String, String>,
) -> (Vec<Community>, Vec<(Uuid, Uuid)>) {
    let mut rows = Vec::with_capacity(det.communities.len());
    let mut members = Vec::new();
    for c in &det.communities {
        rows.push(Community {
            id: c.id,
            repo_id,
            label: c.label.clone(),
            member_count: c.members.len() as i32,
            subtree_hash: merkle::community_hash(&c.files, file_hashes),
            summary: None,
            summary_hash: None,
            top_members: c.top_members.clone(),
            files: c.files.clone(),
        });
        members.extend(c.members.iter().map(|&m| (c.id, m)));
    }
    (rows, members)
}

/// Fold sub-threshold communities into real ones, deterministically:
/// connected fragments merge into a neighbour chosen folder-first (members in
/// the fragment's dominant directory), then by summed coupling weight, then by
/// smallest min-member `stable_id`; isolates fold into the community with the
/// most members under the isolate's deepest folder ancestor (target must be at
/// or above the threshold). Candidates are visited in ascending min-member
/// `stable_id` order and merges apply immediately.
fn consolidate_tiny_communities(
    indexed: &[&Node],
    pair_weight: &BTreeMap<(usize, usize), f64>,
    mut labels: Vec<usize>,
) -> (Vec<usize>, usize) {
    let n = labels.len();
    let mut adj: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    for (&(a, b), &w) in pair_weight {
        adj[a].push((b, w));
        adj[b].push((a, w));
    }
    for list in &mut adj {
        list.sort_by(|x, y| x.0.cmp(&y.0));
    }
    let path = |i: usize| node_path(&indexed[i].stable_id);
    let sid = |i: usize| indexed[i].stable_id.as_str();
    let min_sid = |labels: &[usize], l: usize| -> &str {
        (0..n)
            .filter(|&i| labels[i] == l)
            .map(sid)
            .min()
            .unwrap_or("")
    };

    let mut merged_total = 0usize;
    for _ in 0..MAX_MERGE_PASSES {
        let mut min_sid_by_label: BTreeMap<usize, &str> = BTreeMap::new();
        let mut size_by_label: BTreeMap<usize, usize> = BTreeMap::new();
        for (i, &l) in labels.iter().enumerate() {
            *size_by_label.entry(l).or_insert(0) += 1;
            let e = min_sid_by_label.entry(l).or_insert(sid(i));
            if sid(i) < *e {
                *e = sid(i);
            }
        }
        let mut candidates: Vec<usize> = size_by_label
            .iter()
            .filter(|(_, &size)| size < MIN_COMMUNITY_SIZE)
            .map(|(&l, _)| l)
            .collect();
        candidates.sort_by_key(|l| min_sid_by_label[l]);

        let mut merged_this_pass = 0usize;
        for label in candidates {
            let members: Vec<usize> = (0..n).filter(|&i| labels[i] == label).collect();
            if members.is_empty() || members.len() >= MIN_COMMUNITY_SIZE {
                continue;
            }
            let mut dir_votes: BTreeMap<&str, usize> = BTreeMap::new();
            for &i in &members {
                let dir = parent_dir(path(i));
                if !dir.is_empty() {
                    *dir_votes.entry(dir).or_insert(0) += 1;
                }
            }
            let dominant_dir: Option<&str> = dir_votes
                .iter()
                .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
                .map(|(d, _)| *d);

            let mut weight_by_label: BTreeMap<usize, f64> = BTreeMap::new();
            for &i in &members {
                for &(j, w) in &adj[i] {
                    if labels[j] != label {
                        *weight_by_label.entry(labels[j]).or_insert(0.0) += w;
                    }
                }
            }

            let target: Option<usize> = if !weight_by_label.is_empty() {
                let shares_folder = |l: usize| {
                    dominant_dir.is_some_and(|dir| {
                        (0..n).any(|i| labels[i] == l && parent_dir(path(i)) == dir)
                    })
                };
                weight_by_label
                    .iter()
                    .map(|(&l, &w)| (shares_folder(l), w, l))
                    .max_by(|a, b| {
                        a.0.cmp(&b.0)
                            .then(a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal))
                            .then_with(|| min_sid(&labels, b.2).cmp(min_sid(&labels, a.2)))
                    })
                    .map(|(_, _, l)| l)
            } else {
                let mut found = None;
                let mut ancestor = dominant_dir.unwrap_or("");
                while !ancestor.is_empty() && found.is_none() {
                    let prefix = format!("{ancestor}/");
                    let mut votes: BTreeMap<usize, usize> = BTreeMap::new();
                    for (i, &l) in labels.iter().enumerate() {
                        if l != label && path(i).starts_with(&prefix) {
                            *votes.entry(l).or_insert(0) += 1;
                        }
                    }
                    found = votes
                        .iter()
                        .filter(|(&l, _)| {
                            (0..n).filter(|&i| labels[i] == l).count() >= MIN_COMMUNITY_SIZE
                        })
                        .map(|(&l, &count)| (count, l))
                        .max_by(|a, b| {
                            a.0.cmp(&b.0)
                                .then_with(|| min_sid(&labels, b.1).cmp(min_sid(&labels, a.1)))
                        })
                        .map(|(_, l)| l);
                    ancestor = parent_dir(ancestor);
                }
                found
            };

            if let Some(target) = target {
                for &i in &members {
                    labels[i] = target;
                }
                merged_this_pass += 1;
            }
        }
        merged_total += merged_this_pass;
        if merged_this_pass == 0 {
            break;
        }
    }
    (labels, merged_total)
}

fn build_community(
    repo_id: Uuid,
    indexed: &[&Node],
    member_idxs: &[usize],
    degree: &[f64],
) -> DetectedCommunity {
    let mut sorted: Vec<usize> = member_idxs.to_vec();
    sorted.sort_by(|&a, &b| indexed[a].stable_id.cmp(&indexed[b].stable_id));
    let min_stable_id = sorted
        .first()
        .map(|&i| indexed[i].stable_id.as_str())
        .unwrap_or("");
    let id = hash::scoped_id(repo_id, &format!("community:{min_stable_id}"));

    let mut ranked = sorted.clone();
    // Symbols before files/modules (a File node's Contains fan-out would
    // otherwise always win), then by weighted degree.
    let is_structural = |i: usize| {
        matches!(
            indexed[i].kind,
            crate::domain::model::NodeKind::File
                | crate::domain::model::NodeKind::Repository
                | crate::domain::model::NodeKind::Module
        )
    };
    ranked.sort_by(|&a, &b| {
        is_structural(a)
            .cmp(&is_structural(b))
            .then(degree[b].partial_cmp(&degree[a]).unwrap_or(Ordering::Equal))
            .then(indexed[a].stable_id.cmp(&indexed[b].stable_id))
    });
    let top_members: Vec<String> = ranked
        .iter()
        .take(TOP_MEMBERS)
        .map(|&i| indexed[i].name.clone())
        .collect();

    let mut files: Vec<String> = sorted
        .iter()
        .map(|&i| node_path(&indexed[i].stable_id))
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    files.sort();
    files.dedup();

    let label = community_label(&files, top_members.first().map(String::as_str));
    DetectedCommunity {
        id,
        label,
        members: sorted.iter().map(|&i| indexed[i].id).collect(),
        top_members,
        files,
    }
}

/// Dominant top-level directory of the member files (ties to the smallest
/// name) joined with the top member's name. Deterministic.
fn community_label(files: &[String], top: Option<&str>) -> String {
    // The directory most of the member files live in (ties: shortest, then
    // alphabetical) — communities usually span layers, so a common prefix
    // would collapse to the repository root.
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for f in files {
        let dir = match f.rfind('/') {
            Some(i) => &f[..i],
            None => ".",
        };
        *counts.entry(dir.to_string()).or_insert(0) += 1;
    }
    let prefix = counts
        .iter()
        .max_by(|a, b| {
            a.1.cmp(b.1)
                .then(b.0.len().cmp(&a.0.len()))
                .then(b.0.cmp(a.0))
        })
        .map(|(s, _)| s.clone());
    match (prefix, top) {
        (Some(p), Some(t)) => format!("{p} · {t}"),
        (Some(p), None) => p,
        (None, Some(t)) => t.to_string(),
        (None, None) => "community".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Louvain working graph
// ---------------------------------------------------------------------------

/// Undirected weighted graph for one Louvain level. `loops[i]` is the
/// un-doubled self-loop weight from collapsed internal edges; `degree[i]`
/// counts each self-loop twice; `m` is total edge weight (level-invariant).
#[derive(Clone)]
struct Graph {
    n: usize,
    adj: Vec<Vec<(usize, f64)>>,
    loops: Vec<f64>,
    degree: Vec<f64>,
    m: f64,
}

impl Graph {
    fn from_pairs(n: usize, pair_weight: &BTreeMap<(usize, usize), f64>) -> Self {
        let mut adj = vec![Vec::new(); n];
        let mut degree = vec![0.0f64; n];
        let mut m = 0.0;
        for (&(a, b), &w) in pair_weight {
            adj[a].push((b, w));
            adj[b].push((a, w));
            degree[a] += w;
            degree[b] += w;
            m += w;
        }
        Graph {
            n,
            adj,
            loops: vec![0.0; n],
            degree,
            m,
        }
    }

    fn modularity(&self, comm: &[usize]) -> f64 {
        if self.m <= 0.0 {
            return 0.0;
        }
        let two_m = 2.0 * self.m;
        let mut internal: BTreeMap<usize, f64> = BTreeMap::new();
        let mut tot: BTreeMap<usize, f64> = BTreeMap::new();
        for i in 0..self.n {
            let c = comm[i];
            *tot.entry(c).or_insert(0.0) += self.degree[i];
            *internal.entry(c).or_insert(0.0) += self.loops[i];
            for &(j, w) in &self.adj[i] {
                if comm[j] == c {
                    *internal.entry(c).or_insert(0.0) += w / 2.0;
                }
            }
        }
        tot.iter()
            .map(|(c, &t)| {
                let intern = internal.get(c).copied().unwrap_or(0.0);
                intern / self.m - RESOLUTION * (t / two_m) * (t / two_m)
            })
            .sum()
    }

    /// One level of local moving to convergence; compacted labels per node.
    fn one_level(&self) -> Vec<usize> {
        let two_m = 2.0 * self.m.max(1e-12);
        let mut comm: Vec<usize> = (0..self.n).collect();
        let mut tot: Vec<f64> = self.degree.clone();
        for _ in 0..MAX_PASSES_PER_LEVEL {
            let mut moved = false;
            for i in 0..self.n {
                let ci = comm[i];
                let mut neigh: BTreeMap<usize, f64> = BTreeMap::new();
                for &(j, w) in &self.adj[i] {
                    if j != i {
                        *neigh.entry(comm[j]).or_insert(0.0) += w;
                    }
                }
                let ki = self.degree[i];
                tot[ci] -= ki;
                let mut best_comm = ci;
                let mut best_gain =
                    neigh.get(&ci).copied().unwrap_or(0.0) - RESOLUTION * tot[ci] * ki / two_m;
                for (&c, &dnc) in &neigh {
                    let gain = dnc - RESOLUTION * tot[c] * ki / two_m;
                    if gain > best_gain + 1e-12 || (gain > best_gain - 1e-12 && c < best_comm) {
                        best_gain = gain;
                        best_comm = c;
                    }
                }
                tot[best_comm] += ki;
                if best_comm != ci {
                    comm[i] = best_comm;
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
        compact_labels(&comm)
    }

    /// Aggregate: each community of `comm` (compacted) becomes one node.
    fn induce(&self, comm: &[usize]) -> Graph {
        let k = distinct_count(comm);
        let mut loops = vec![0.0f64; k];
        let mut pair: BTreeMap<(usize, usize), f64> = BTreeMap::new();
        for i in 0..self.n {
            let ci = comm[i];
            loops[ci] += self.loops[i];
            for &(j, w) in &self.adj[i] {
                let cj = comm[j];
                if ci == cj {
                    loops[ci] += w / 2.0;
                } else if i < j {
                    let key = if ci < cj { (ci, cj) } else { (cj, ci) };
                    *pair.entry(key).or_insert(0.0) += w;
                }
            }
        }
        let mut adj = vec![Vec::new(); k];
        let mut degree: Vec<f64> = loops.iter().map(|l| 2.0 * l).collect();
        let mut m = loops.iter().sum::<f64>();
        for (&(a, b), &w) in &pair {
            adj[a].push((b, w));
            adj[b].push((a, w));
            degree[a] += w;
            degree[b] += w;
            m += w;
        }
        Graph {
            n: k,
            adj,
            loops,
            degree,
            m,
        }
    }
}

/// Renumber arbitrary labels to 0..k by first appearance.
fn compact_labels(labels: &[usize]) -> Vec<usize> {
    let mut map: HashMap<usize, usize> = HashMap::new();
    labels
        .iter()
        .map(|&l| {
            let next = map.len();
            *map.entry(l).or_insert(next)
        })
        .collect()
}

fn distinct_count(labels: &[usize]) -> usize {
    labels.iter().map(|&l| l + 1).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(stable: &str, kind: NodeKind) -> Node {
        Node {
            id: hash::scoped_id(Uuid::nil(), stable),
            repo_id: Uuid::nil(),
            file_id: None,
            kind,
            stable_id: stable.into(),
            name: stable.rsplit(':').next().unwrap_or(stable).into(),
            signature: None,
            doc: None,
            line_start: Some(1),
            line_end: Some(2),
            metadata: json!({}),
        }
    }

    fn edge(s: &Node, t: &Node, kind: EdgeKind) -> Edge {
        Edge {
            id: hash::edge_id(s.id, t.id, kind.as_str()),
            repo_id: Uuid::nil(),
            source_node_id: s.id,
            target_node_id: t.id,
            kind,
            confidence: 1.0,
        }
    }

    /// Three 4-node cliques in three folders.
    fn cliques() -> (Vec<Node>, Vec<Edge>) {
        let mut nodes = Vec::new();
        for c in 0..3 {
            for m in 0..4 {
                nodes.push(node(
                    &format!("rust:grp{c}/file{m}.rs:function:f{c}_{m}"),
                    NodeKind::Function,
                ));
            }
        }
        let mut edges = Vec::new();
        for c in 0..3usize {
            for a in 0..4 {
                for b in (a + 1)..4 {
                    edges.push(edge(&nodes[c * 4 + a], &nodes[c * 4 + b], EdgeKind::Calls));
                }
            }
        }
        (nodes, edges)
    }

    #[test]
    fn node_path_parses_stable_ids() {
        assert_eq!(node_path("file:src/a.rs"), "src/a.rs");
        assert_eq!(node_path("rust:src/a.rs:function:Foo::bar"), "src/a.rs");
        assert_eq!(node_path("repo"), "");
    }

    #[test]
    fn separates_cliques_and_is_deterministic() {
        let (nodes, edges) = cliques();
        let det = detect(Uuid::nil(), &nodes, &edges);
        assert_eq!(det.communities.len(), 3);
        assert!(det.modularity > 0.5);
        assert_eq!(det.merged_fragments, 0);
        for c in &det.communities {
            assert_eq!(c.members.len(), 4);
            assert_eq!(c.files.len(), 4);
            assert!(c.label.starts_with("grp"), "{}", c.label);
            assert_eq!(
                c.id,
                hash::scoped_id(
                    Uuid::nil(),
                    &format!(
                        "community:{}",
                        nodes
                            .iter()
                            .find(|n| n.id == c.members[0])
                            .unwrap()
                            .stable_id
                    )
                )
            );
        }
        let mut rev_nodes = nodes.clone();
        rev_nodes.reverse();
        let mut rev_edges = edges.clone();
        rev_edges.reverse();
        assert_eq!(det, detect(Uuid::nil(), &rev_nodes, &rev_edges));
    }

    #[test]
    fn repository_node_is_excluded() {
        let (mut nodes, mut edges) = cliques();
        let repo = node("repo", NodeKind::Repository);
        for n in nodes.clone() {
            edges.push(edge(&repo, &n, EdgeKind::Contains));
        }
        nodes.push(repo.clone());
        let det = detect(Uuid::nil(), &nodes, &edges);
        assert_eq!(det.communities.len(), 3);
        assert!(det
            .communities
            .iter()
            .all(|c| !c.members.contains(&repo.id)));
    }

    #[test]
    fn consolidation_folds_two_node_fragment() {
        let (mut nodes, mut edges) = cliques();
        // A weakly attached 2-node fragment in grp1's folder.
        let x = node("rust:grp1/extra.rs:function:x", NodeKind::Function);
        let y = node("rust:grp1/extra.rs:function:y", NodeKind::Function);
        edges.push(edge(&x, &y, EdgeKind::Calls));
        let mut weak = edge(&x, &nodes[4], EdgeKind::Imports);
        weak.confidence = 0.05;
        edges.push(weak);
        nodes.push(x.clone());
        nodes.push(y.clone());
        let det = detect(Uuid::nil(), &nodes, &edges);
        assert_eq!(det.merged_fragments, 1);
        assert_eq!(det.communities.len(), 3);
        let host = det
            .communities
            .iter()
            .find(|c| c.members.contains(&x.id))
            .unwrap();
        assert!(host.members.contains(&y.id));
        assert!(host.members.contains(&nodes[4].id));
        assert_eq!(host.members.len(), 6);
    }

    #[test]
    fn isolate_folds_into_folder_dominant_community() {
        let (mut nodes, edges) = cliques();
        let lone = node("file:grp2/README.md", NodeKind::File);
        nodes.push(lone.clone());
        let det = detect(Uuid::nil(), &nodes, &edges);
        assert_eq!(det.communities.len(), 3);
        let host = det
            .communities
            .iter()
            .find(|c| c.members.contains(&lone.id))
            .unwrap();
        assert!(host.files.iter().all(|f| f.starts_with("grp2/")));
    }

    #[test]
    fn to_communities_fills_hashes_and_members() {
        let (nodes, edges) = cliques();
        let det = detect(Uuid::nil(), &nodes, &edges);
        let leaves: Vec<(String, String)> = nodes
            .iter()
            .map(|n| {
                (
                    node_path(&n.stable_id).to_string(),
                    hash::sha256_hex(&n.stable_id),
                )
            })
            .collect();
        let fh = merkle::file_hashes(&leaves);
        let (rows, members) = to_communities(&det, Uuid::nil(), &fh);
        assert_eq!(rows.len(), 3);
        assert_eq!(members.len(), 12);
        for r in &rows {
            assert_eq!(r.member_count, 4);
            assert_eq!(r.subtree_hash, merkle::community_hash(&r.files, &fh));
            assert_eq!(r.top_members.len(), 4);
        }
    }

    #[test]
    fn empty_input() {
        let det = detect(Uuid::nil(), &[], &[]);
        assert!(det.communities.is_empty());
        assert_eq!(det.levels, 0);
    }
}
