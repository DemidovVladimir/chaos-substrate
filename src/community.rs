//! L1 community / "god-node" detection over the L0 knowledge multigraph.
//!
//! This is the substrate for the hierarchical-memory layer (see
//! `docs/HIERARCHICAL_MEMORY_ROADMAP.md`). It groups L0 nodes into communities
//! (features / subsystems) with a *quotient graph* of typed, aggregated edges
//! between them. The same module powers the read-only P0 spike
//! (`chaos communities`), the persisted P1 layer, and the P4 change-plan tool.
//!
//! # Algorithm
//!
//! Deterministic, multi-level **Louvain** modularity optimization, ported from
//! the well-known reference bookkeeping (Blondel et al. / python-louvain) but
//! with **all randomness removed**: nodes are always visited in canonical
//! `stable_id` order and ties break toward the smallest community
//! representative. Same `(nodes, edges, config)` ⇒ byte-identical partition.
//!
//! # Determinism contract
//!
//! - No RNG anywhere (the roadmap forbids unseeded RNG; we go further and use a
//!   fixed canonical order instead of a seeded shuffle).
//! - Community ids are derived with UUIDv5 from `repo_id` + the smallest member
//!   `stable_id`, so they are stable across re-indexes even though L0 node
//!   UUIDs are regenerated on every `analyze`. Determinism is therefore
//!   verified at the `stable_id` level (member *sets*), not raw node UUIDs.
//!
//! # Repository node
//!
//! The synthetic `repository` node is excluded: it `contains` every file (a
//! star of thousands of edges) and would otherwise collapse the whole repo into
//! a single community.

use crate::models::{KnowledgeEdge, KnowledgeNode};
use crate::storage::Storage;
use anyhow::Result;
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

/// Detection algorithm/version recorded in `communities.detection_params`, so a
/// future tuning change is visible and re-detect can be forced.
pub const DETECTION_VERSION: i64 = 1;

/// Fixed namespace for deterministic community UUIDv5 derivation. Generated
/// once and pinned; must never change or community ids would churn.
pub(crate) const COMMUNITY_NAMESPACE: Uuid = Uuid::from_bytes([
    0x9e, 0x4f, 0x1a, 0x77, 0x2c, 0x38, 0x4b, 0x6d, 0xa1, 0x0c, 0x5e, 0x82, 0x3f, 0x91, 0xd4, 0x6a,
]);

/// Below this absolute modularity gain a further Louvain level is not worth it.
const MIN_MODULARITY_GAIN: f64 = 1e-7;
/// Hard cap on Louvain levels (convergence is normally 2-4).
const MAX_LEVELS: usize = 32;
/// Hard cap on local-moving passes within a single level.
const MAX_PASSES_PER_LEVEL: usize = 64;
/// Default number of representative members surfaced per community.
const TOP_MEMBERS: usize = 8;

/// Tuning for [`detect_communities`].
#[derive(Debug, Clone)]
pub struct CommunityConfig {
    /// Modularity resolution γ. 1.0 = standard; higher ⇒ finer/smaller
    /// communities, lower ⇒ coarser/larger.
    pub resolution: f64,
}

impl Default for CommunityConfig {
    fn default() -> Self {
        Self { resolution: 1.0 }
    }
}

/// A representative member surfaced for a community (by weighted degree).
#[derive(Debug, Clone, Serialize)]
pub struct TopMember {
    pub stable_id: String,
    pub name: String,
    pub kind: String,
    pub degree: usize,
}

/// A detected community (god-node). `member_node_ids` / `member_stable_ids`
/// are aligned and sorted by `stable_id` for determinism.
#[derive(Debug, Clone, Serialize)]
pub struct DetectedCommunity {
    /// Deterministic UUIDv5 (namespace, `repo_id:min_member_stable_id`).
    pub id: Uuid,
    pub label: String,
    pub size: usize,
    pub member_node_ids: Vec<Uuid>,
    pub member_stable_ids: Vec<String>,
    pub internal_edges: usize,
    pub dominant_language: Option<String>,
    pub language_distribution: BTreeMap<String, usize>,
    pub top_members: Vec<TopMember>,
}

/// One typed, aggregated edge of the quotient graph (between two communities).
#[derive(Debug, Clone, Serialize)]
pub struct DetectedQuotientEdge {
    pub source_community_id: Uuid,
    pub target_community_id: Uuid,
    /// Dominant (most frequent) L0 edge kind crossing this boundary.
    pub kind: String,
    /// Summed coupling weight (`confidence/cost`) across the boundary.
    pub weight: f64,
    pub edge_count: usize,
    pub kind_counts: BTreeMap<String, usize>,
}

/// Full result of one detection run. Vectors are deterministically ordered.
#[derive(Debug, Clone, Serialize)]
pub struct CommunityDetection {
    pub communities: Vec<DetectedCommunity>,
    pub quotient_edges: Vec<DetectedQuotientEdge>,
    pub modularity: f64,
    pub levels: usize,
    pub resolution: f64,
    /// Nodes considered (excludes the repository node and any node with no id
    /// match). Useful for the spike's sanity reporting.
    pub node_count: usize,
    pub edge_count: usize,
}

/// Coupling weight for an L0 edge: stronger structural edges (low `cost`, high
/// `confidence`) couple more. `cost` is clamped away from zero. This is the
/// fix for the naive `1 - cost/max_cost` scheme, which would zero out
/// `imports`/`calls` (the cross-file edges that actually define features).
fn coupling_weight(edge: &KnowledgeEdge) -> f64 {
    let cost = edge.cost.max(1e-6);
    let confidence = edge.confidence.clamp(0.0, 1.0);
    (confidence / cost).max(0.0)
}

fn node_language(node: &KnowledgeNode) -> Option<String> {
    node.metadata
        .get("language")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Load the persisted L0 graph for a repo, detect communities, and replace the
/// persisted L1 layer. Returns the detection so callers can report counts.
/// Runs against the database (source of truth) so it is correct for both full
/// `analyze` (fresh node ids) and incremental `add` (remapped node ids).
pub async fn detect_and_persist(
    storage: &Storage,
    repo_id: Uuid,
    config: &CommunityConfig,
) -> Result<CommunityDetection> {
    let nodes = storage.load_all_nodes(repo_id).await?;
    let edges = storage.load_all_edges(repo_id).await?;
    let detection = detect_communities(repo_id, &nodes, &edges, config);
    let params = json!({
        "algorithm": "louvain",
        "resolution": detection.resolution,
        "version": DETECTION_VERSION,
        "levels": detection.levels,
        "modularity": detection.modularity,
    });
    storage
        .replace_communities(repo_id, &detection, &params)
        .await?;
    Ok(detection)
}

/// A package-manifest *declared dependency* aggregate node (from `package.json` /
/// `Cargo.toml`), as opposed to a real code symbol or a source-level import. These
/// carry no feature signal on their own, so they're excluded from community
/// formation (but kept in the graph for search). Source imports use
/// `import:bare:…` / `…:import:…` stable_ids and are NOT matched here.
fn is_manifest_dependency(node: &KnowledgeNode) -> bool {
    node.kind == crate::models::NodeKind::Dependency
        && (node.stable_id.contains(":npm:dependency:")
            || node.stable_id.contains(":cargo:dependency:"))
}

/// Detect communities over the L0 graph. Pure and deterministic.
pub fn detect_communities(
    repo_id: Uuid,
    nodes: &[KnowledgeNode],
    edges: &[KnowledgeEdge],
    config: &CommunityConfig,
) -> CommunityDetection {
    // 1. Index nodes that can form a feature, in canonical stable_id order. The
    //    repository root is excluded, and so are package-manifest *declared
    //    dependency* aggregate nodes (`…:npm:dependency:…` / `…:cargo:dependency:…`)
    //    — those are just the package.json / Cargo.toml dependency list and, left
    //    in, make every manifest its own meaningless "feature" (the package.json
    //    roots in the Unknown bucket). They stay in the graph as searchable nodes
    //    and chunks; they're only kept out of community formation.
    let mut indexed: Vec<&KnowledgeNode> = nodes
        .iter()
        .filter(|n| n.kind != crate::models::NodeKind::Repository)
        .filter(|n| !is_manifest_dependency(n))
        .collect();
    indexed.sort_by(|a, b| a.stable_id.cmp(&b.stable_id).then(a.id.cmp(&b.id)));

    let n = indexed.len();
    let mut index_of: HashMap<Uuid, usize> = HashMap::with_capacity(n);
    for (idx, node) in indexed.iter().enumerate() {
        index_of.insert(node.id, idx);
    }

    if n == 0 {
        return CommunityDetection {
            communities: Vec::new(),
            quotient_edges: Vec::new(),
            modularity: 0.0,
            levels: 0,
            resolution: config.resolution,
            node_count: 0,
            edge_count: 0,
        };
    }

    // 2. Build the undirected weighted base graph (sum parallel/both-direction
    //    edges into one symmetric weight per unordered pair).
    let mut pair_weight: HashMap<(usize, usize), f64> = HashMap::new();
    let mut considered_edges = 0usize;
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
        considered_edges += 1;
        let key = if s < t { (s, t) } else { (t, s) };
        *pair_weight.entry(key).or_insert(0.0) += coupling_weight(edge);
    }

    let base = Graph::from_pairs(n, &pair_weight);

    // 3. Multi-level Louvain. `node_to_comm` maps original index -> community
    //    label of the current top level; we compose down each aggregation.
    let mut node_to_comm: Vec<usize> = (0..n).collect();
    let mut graph = base.clone();
    let mut modularity = graph.modularity(&(0..graph.n).collect::<Vec<_>>(), config.resolution);
    let mut levels = 0usize;

    for _ in 0..MAX_LEVELS {
        let level_comm = graph.one_level(config.resolution);
        let new_modularity = graph.modularity(&level_comm, config.resolution);
        levels += 1;

        // Compose: every original node currently mapped to super-node `c`
        // inherits that super-node's new community label.
        for slot in node_to_comm.iter_mut() {
            *slot = level_comm[*slot];
        }

        let community_count = distinct_count(&level_comm);
        let improved = new_modularity - modularity > MIN_MODULARITY_GAIN;
        modularity = new_modularity;

        // Stop when no merging happened or the gain is negligible.
        if community_count == graph.n || !improved {
            break;
        }
        graph = graph.induce(&level_comm);
    }

    // 4. Compact community labels to 0..k by first appearance (canonical order).
    let compact = compact_labels(&node_to_comm);
    let k = distinct_count(&compact);

    // 5. Gather members per community.
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); k];
    for (idx, &c) in compact.iter().enumerate() {
        members[c].push(idx);
    }

    // 6. Weighted degree within the base graph (for top-member ranking).
    let base_degree = &base.degree;

    let communities = build_communities(
        repo_id,
        &indexed,
        &members,
        &pair_weight,
        &compact,
        base_degree,
    );

    // build_communities returns communities aligned to compact labels 0..k, so
    // position == label. Map label -> community UUID for the quotient graph.
    let mut comm_id_by_label: Vec<Uuid> = vec![Uuid::nil(); k];
    for (label, community) in communities.iter().enumerate() {
        comm_id_by_label[label] = community.id;
    }

    let quotient_edges =
        build_quotient_edges(&pair_weight, &compact, &comm_id_by_label, edges, &index_of);

    CommunityDetection {
        communities,
        quotient_edges,
        modularity,
        levels,
        resolution: config.resolution,
        node_count: n,
        edge_count: considered_edges,
    }
}

/// Per-community assembly: deterministic id, label, members, language mix,
/// top members. Returned in compact-label order (label 0 first).
fn build_communities(
    repo_id: Uuid,
    indexed: &[&KnowledgeNode],
    members: &[Vec<usize>],
    pair_weight: &HashMap<(usize, usize), f64>,
    compact: &[usize],
    base_degree: &[f64],
) -> Vec<DetectedCommunity> {
    // internal edge counts per community.
    let mut internal_edges = vec![0usize; members.len()];
    for &(a, b) in pair_weight.keys() {
        if compact[a] == compact[b] {
            internal_edges[compact[a]] += 1;
        }
    }

    let mut out = Vec::with_capacity(members.len());
    for (label, member_idxs) in members.iter().enumerate() {
        // Sort members by stable_id for determinism.
        let mut sorted: Vec<usize> = member_idxs.clone();
        sorted.sort_by(|&a, &b| indexed[a].stable_id.cmp(&indexed[b].stable_id));

        let min_stable_id = sorted
            .first()
            .map(|&i| indexed[i].stable_id.clone())
            .unwrap_or_default();
        let id = Uuid::new_v5(
            &COMMUNITY_NAMESPACE,
            format!("{repo_id}:{min_stable_id}").as_bytes(),
        );

        let member_node_ids: Vec<Uuid> = sorted.iter().map(|&i| indexed[i].id).collect();
        let member_stable_ids: Vec<String> = sorted
            .iter()
            .map(|&i| indexed[i].stable_id.clone())
            .collect();

        // Language distribution (BTreeMap for ordered output).
        let mut language_distribution: BTreeMap<String, usize> = BTreeMap::new();
        for &i in &sorted {
            if let Some(lang) = node_language(indexed[i]) {
                *language_distribution.entry(lang).or_insert(0) += 1;
            }
        }
        let dominant_language = language_distribution
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
            .map(|(lang, _)| lang.clone());

        // Top members by weighted degree, tie-break by stable_id.
        let mut ranked: Vec<usize> = sorted.clone();
        ranked.sort_by(|&a, &b| {
            base_degree[b]
                .partial_cmp(&base_degree[a])
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(indexed[a].stable_id.cmp(&indexed[b].stable_id))
        });
        let top_members: Vec<TopMember> = ranked
            .iter()
            .take(TOP_MEMBERS)
            .map(|&i| TopMember {
                stable_id: indexed[i].stable_id.clone(),
                name: indexed[i].name.clone(),
                kind: indexed[i].kind.as_str().to_string(),
                degree: base_degree[i].round() as usize,
            })
            .collect();

        let label_text = community_label(&top_members, &member_stable_ids);

        out.push(DetectedCommunity {
            id,
            label: label_text,
            size: sorted.len(),
            member_node_ids,
            member_stable_ids,
            internal_edges: internal_edges[label],
            dominant_language,
            language_distribution,
            top_members,
        });
    }
    out
}

/// A human-ish label: dominant top-level directory of members, else the top
/// member's name. Deterministic.
fn community_label(top_members: &[TopMember], member_stable_ids: &[String]) -> String {
    // Most common leading path segment among member stable_ids.
    let mut prefix_counts: BTreeMap<String, usize> = BTreeMap::new();
    for sid in member_stable_ids {
        let path = sid.split(':').next().unwrap_or(sid);
        let seg = path.split('/').find(|s| !s.is_empty()).unwrap_or("");
        if !seg.is_empty() && seg != "file" {
            *prefix_counts.entry(seg.to_string()).or_insert(0) += 1;
        }
    }
    let prefix = prefix_counts
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
        .map(|(seg, _)| seg.clone());
    match (prefix, top_members.first()) {
        (Some(p), Some(top)) => format!("{p} · {}", top.name),
        (Some(p), None) => p,
        (None, Some(top)) => top.name.clone(),
        (None, None) => "community".to_string(),
    }
}

/// Aggregate L0 edges crossing community boundaries into typed quotient edges.
/// Direction is normalized (smaller community-id first) so each undirected
/// boundary appears once. Deterministically ordered.
fn build_quotient_edges(
    _pair_weight: &HashMap<(usize, usize), f64>,
    compact: &[usize],
    comm_id_by_label: &[Uuid],
    edges: &[KnowledgeEdge],
    index_of: &HashMap<Uuid, usize>,
) -> Vec<DetectedQuotientEdge> {
    struct Agg {
        weight: f64,
        edge_count: usize,
        kind_counts: BTreeMap<String, usize>,
    }
    let mut agg: BTreeMap<(Uuid, Uuid), Agg> = BTreeMap::new();

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
        let (ca, cb) = (compact[s], compact[t]);
        if ca == cb {
            continue;
        }
        let (mut ua, mut ub) = (comm_id_by_label[ca], comm_id_by_label[cb]);
        if ub < ua {
            std::mem::swap(&mut ua, &mut ub);
        }
        let entry = agg.entry((ua, ub)).or_insert_with(|| Agg {
            weight: 0.0,
            edge_count: 0,
            kind_counts: BTreeMap::new(),
        });
        entry.weight += coupling_weight(edge);
        entry.edge_count += 1;
        *entry
            .kind_counts
            .entry(edge.kind.as_str().to_string())
            .or_insert(0) += 1;
    }

    agg.into_iter()
        .map(|((source, target), a)| {
            let kind = a
                .kind_counts
                .iter()
                .max_by(|x, y| x.1.cmp(y.1).then(y.0.cmp(x.0)))
                .map(|(k, _)| k.clone())
                .unwrap_or_else(|| "mentions".to_string());
            DetectedQuotientEdge {
                source_community_id: source,
                target_community_id: target,
                kind,
                weight: a.weight,
                edge_count: a.edge_count,
                kind_counts: a.kind_counts,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Louvain working graph
// ---------------------------------------------------------------------------

/// An undirected weighted graph for one Louvain level. `loops[i]` is the
/// (un-doubled) self-loop weight accumulated from collapsed internal edges;
/// `degree[i]` counts each self-loop twice (standard convention). `m` is the
/// total edge weight and is invariant across aggregation levels.
#[derive(Clone)]
struct Graph {
    n: usize,
    adj: Vec<Vec<(usize, f64)>>,
    loops: Vec<f64>,
    degree: Vec<f64>,
    m: f64,
}

impl Graph {
    fn from_pairs(n: usize, pair_weight: &HashMap<(usize, usize), f64>) -> Self {
        let mut adj = vec![Vec::new(); n];
        let mut degree = vec![0.0f64; n];
        let mut m = 0.0;
        // Deterministic adjacency: sort pairs before building.
        let mut pairs: Vec<(&(usize, usize), &f64)> = pair_weight.iter().collect();
        pairs.sort_by(|a, b| a.0.cmp(b.0));
        for (&(a, b), &w) in pairs {
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

    /// Modularity of the partition `comm` (one label per node).
    fn modularity(&self, comm: &[usize], resolution: f64) -> f64 {
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
                    // Each undirected internal edge counted from both ends ⇒
                    // contributes w twice across the loop; matches `internals`.
                    *internal.entry(c).or_insert(0.0) += w / 2.0;
                }
            }
        }
        let mut q = 0.0;
        for (c, &t) in &tot {
            let intern = internal.get(c).copied().unwrap_or(0.0);
            q += intern / self.m - resolution * (t / two_m) * (t / two_m);
        }
        q
    }

    /// One level of local moving to convergence. Returns a compacted community
    /// label (0..k) per node, in canonical (node-index) order.
    fn one_level(&self, resolution: f64) -> Vec<usize> {
        let two_m = 2.0 * self.m.max(1e-12);
        let mut comm: Vec<usize> = (0..self.n).collect();
        // Σ_tot per community = sum of member degrees.
        let mut tot: Vec<f64> = self.degree.clone();

        for _ in 0..MAX_PASSES_PER_LEVEL {
            let mut moved = false;
            // Canonical visit order: ascending node index (already stable_id
            // sorted at level 0; representative-sorted at higher levels).
            for i in 0..self.n {
                let ci = comm[i];
                // Weight from i to each neighboring community (excl. self-loop).
                let mut neigh: BTreeMap<usize, f64> = BTreeMap::new();
                for &(j, w) in &self.adj[i] {
                    if j == i {
                        continue;
                    }
                    *neigh.entry(comm[j]).or_insert(0.0) += w;
                }
                // Remove i from its community.
                tot[ci] -= self.degree[i];
                let ki = self.degree[i];

                // Baseline: returning to ci.
                let mut best_comm = ci;
                let mut best_gain =
                    neigh.get(&ci).copied().unwrap_or(0.0) - resolution * tot[ci] * ki / two_m;

                // Candidates in deterministic (ascending community id) order.
                for (&c, &dnc) in &neigh {
                    let gain = dnc - resolution * tot[c] * ki / two_m;
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

    /// Build the aggregated graph where each community of `comm` becomes one
    /// node. Internal edges fold into `loops`; cross edges stay in `adj`.
    /// `comm` must be compacted (labels 0..k).
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
                    // Internal edge; counted once per direction in adj, so each
                    // contributes w/2 to the un-doubled self-loop.
                    loops[ci] += w / 2.0;
                } else if i < j {
                    let key = if ci < cj { (ci, cj) } else { (cj, ci) };
                    *pair.entry(key).or_insert(0.0) += w;
                }
            }
        }
        let mut adj = vec![Vec::new(); k];
        let mut degree = vec![0.0f64; k];
        for (c, &lw) in loops.iter().enumerate() {
            degree[c] += 2.0 * lw;
        }
        let mut m = 0.0;
        for (&(a, b), &w) in &pair {
            adj[a].push((b, w));
            adj[b].push((a, w));
            degree[a] += w;
            degree[b] += w;
            m += w;
        }
        m += loops.iter().sum::<f64>();
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
    let mut next = 0usize;
    let mut out = Vec::with_capacity(labels.len());
    for &l in labels {
        let id = *map.entry(l).or_insert_with(|| {
            let v = next;
            next += 1;
            v
        });
        out.push(id);
    }
    out
}

fn distinct_count(labels: &[usize]) -> usize {
    let mut max = 0usize;
    let mut seen_any = false;
    for &l in labels {
        seen_any = true;
        if l + 1 > max {
            max = l + 1;
        }
    }
    if seen_any {
        max
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{EdgeKind, NodeKind};
    use serde_json::json;

    fn node(stable: &str, kind: NodeKind, lang: &str) -> KnowledgeNode {
        KnowledgeNode {
            id: Uuid::new_v4(),
            repo_id: Uuid::nil(),
            file_id: None,
            kind,
            stable_id: stable.into(),
            name: stable.rsplit(':').next().unwrap_or(stable).into(),
            line_start: Some(1),
            line_end: Some(2),
            metadata: json!({ "language": lang }),
        }
    }

    fn edge(src: Uuid, dst: Uuid, kind: EdgeKind, cost: f64, conf: f64) -> KnowledgeEdge {
        KnowledgeEdge {
            id: Uuid::new_v4(),
            repo_id: Uuid::nil(),
            source_node_id: src,
            target_node_id: dst,
            kind,
            cost,
            confidence: conf,
            metadata: json!({}),
        }
    }

    /// Package-manifest dependency aggregate nodes are kept out of community
    /// formation (so a `package.json` never becomes a "feature"), while source
    /// imports and real symbols still cluster.
    #[test]
    fn manifest_dependencies_excluded_from_communities() {
        // A manifest dependency aggregate node vs. a source-import node.
        let manifest_dep = node(
            "packages/ui/package.json:npm:dependency:react",
            NodeKind::Dependency,
            "json",
        );
        let source_import = node("import:bare:@acme/ui", NodeKind::Dependency, "typescript");
        assert!(is_manifest_dependency(&manifest_dep));
        assert!(!is_manifest_dependency(&source_import));

        // Two real symbols + a manifest dep node, all wired together. Only the two
        // symbols should form the community; the manifest dep is excluded.
        let a = node("file/a.ts:fn:a", NodeKind::Function, "typescript");
        let b = node("file/a.ts:fn:b", NodeKind::Function, "typescript");
        let nodes = vec![a.clone(), b.clone(), manifest_dep.clone()];
        let edges = vec![
            edge(a.id, b.id, EdgeKind::Calls, 1.0, 1.0),
            edge(a.id, manifest_dep.id, EdgeKind::DependsOn, 1.0, 1.0),
        ];
        let det = detect_communities(Uuid::nil(), &nodes, &edges, &CommunityConfig::default());
        let members: Vec<&str> = det
            .communities
            .iter()
            .flat_map(|c| c.top_members.iter())
            .map(|m| m.stable_id.as_str())
            .collect();
        assert!(members.contains(&"file/a.ts:fn:a"));
        assert!(members.contains(&"file/a.ts:fn:b"));
        assert!(
            !members.iter().any(|m| m.contains(":npm:dependency:")),
            "manifest dependency must not be a community member"
        );
    }

    /// Three disjoint cliques ⇒ exactly three communities.
    #[test]
    fn separates_disjoint_cliques() {
        let mut nodes = Vec::new();
        for c in 0..3 {
            for m in 0..4 {
                nodes.push(node(
                    &format!("grp{c}/file{m}:function:f{c}_{m}"),
                    NodeKind::Function,
                    "rust",
                ));
            }
        }
        let mut edges = Vec::new();
        for c in 0..3usize {
            for a in 0..4 {
                for b in (a + 1)..4 {
                    let na = &nodes[c * 4 + a];
                    let nb = &nodes[c * 4 + b];
                    edges.push(edge(na.id, nb.id, EdgeKind::Calls, 0.1, 1.0));
                }
            }
        }
        let det = detect_communities(Uuid::nil(), &nodes, &edges, &CommunityConfig::default());
        assert_eq!(
            det.communities.len(),
            3,
            "three cliques => three communities"
        );
        for c in &det.communities {
            assert_eq!(c.size, 4);
        }
    }

    /// Same input twice ⇒ byte-identical serialized output (no RNG).
    #[test]
    fn detection_is_deterministic() {
        let mut nodes = Vec::new();
        for c in 0..4 {
            for m in 0..5 {
                nodes.push(node(
                    &format!("mod{c}/f{m}:function:fn_{c}_{m}"),
                    NodeKind::Function,
                    "rust",
                ));
            }
        }
        let mut edges = Vec::new();
        // Dense intra-module, sparse inter-module.
        for c in 0..4usize {
            for a in 0..5 {
                for b in (a + 1)..5 {
                    edges.push(edge(
                        nodes[c * 5 + a].id,
                        nodes[c * 5 + b].id,
                        EdgeKind::Imports,
                        0.3,
                        1.0,
                    ));
                }
            }
        }
        for c in 0..3usize {
            edges.push(edge(
                nodes[c * 5].id,
                nodes[(c + 1) * 5].id,
                EdgeKind::Calls,
                0.35,
                0.7,
            ));
        }
        let cfg = CommunityConfig::default();
        let a = detect_communities(Uuid::nil(), &nodes, &edges, &cfg);
        let b = detect_communities(Uuid::nil(), &nodes, &edges, &cfg);
        let ja = serde_json::to_string(&a).unwrap();
        let jb = serde_json::to_string(&b).unwrap();
        assert_eq!(ja, jb, "detection must be byte-identical across runs");
    }

    /// Quotient edges aggregate typed cross-community edges.
    #[test]
    fn quotient_edges_aggregate_by_pair() {
        // Two clusters joined by 3 cross edges (2 calls, 1 depends_on).
        let mut nodes = Vec::new();
        for m in 0..3 {
            nodes.push(node(
                &format!("a/f{m}:function:a{m}"),
                NodeKind::Function,
                "rust",
            ));
        }
        for m in 0..3 {
            nodes.push(node(
                &format!("b/f{m}:function:b{m}"),
                NodeKind::Function,
                "rust",
            ));
        }
        let mut edges = Vec::new();
        for a in 0..3 {
            for b in (a + 1)..3 {
                edges.push(edge(nodes[a].id, nodes[b].id, EdgeKind::Calls, 0.1, 1.0));
                edges.push(edge(
                    nodes[3 + a].id,
                    nodes[3 + b].id,
                    EdgeKind::Calls,
                    0.1,
                    1.0,
                ));
            }
        }
        // cross edges
        edges.push(edge(nodes[0].id, nodes[3].id, EdgeKind::Calls, 0.35, 0.7));
        edges.push(edge(nodes[1].id, nodes[4].id, EdgeKind::Calls, 0.35, 0.7));
        edges.push(edge(
            nodes[2].id,
            nodes[5].id,
            EdgeKind::DependsOn,
            0.2,
            1.0,
        ));

        let det = detect_communities(Uuid::nil(), &nodes, &edges, &CommunityConfig::default());
        assert_eq!(det.communities.len(), 2);
        assert_eq!(det.quotient_edges.len(), 1);
        let qe = &det.quotient_edges[0];
        assert_eq!(qe.edge_count, 3);
        assert_eq!(qe.kind, "calls"); // 2 calls vs 1 depends_on
    }

    /// A single *file* whose symbols couple to different clusters lands in
    /// multiple communities — the file-level overlap that drives L2 blast
    /// radius even though the node partition itself is disjoint.
    #[test]
    fn file_overlaps_multiple_communities() {
        let mut nodes = Vec::new();
        for m in 0..4 {
            nodes.push(node(
                &format!("xmod/f{m}:function:x{m}"),
                NodeKind::Function,
                "rust",
            ));
        }
        for m in 0..4 {
            nodes.push(node(
                &format!("ymod/f{m}:function:y{m}"),
                NodeKind::Function,
                "rust",
            ));
        }
        // Two symbols of the SAME file, each glued to a different cluster.
        let func_a = node("shared/F.rs:function:funcA", NodeKind::Function, "rust");
        let func_b = node("shared/F.rs:function:funcB", NodeKind::Function, "rust");
        nodes.push(func_a.clone());
        nodes.push(func_b.clone());

        let mut edges = Vec::new();
        for a in 0..4 {
            for b in (a + 1)..4 {
                edges.push(edge(nodes[a].id, nodes[b].id, EdgeKind::Calls, 0.1, 1.0));
                edges.push(edge(
                    nodes[4 + a].id,
                    nodes[4 + b].id,
                    EdgeKind::Calls,
                    0.1,
                    1.0,
                ));
            }
        }
        // funcA -> all of X ; funcB -> all of Y.
        for a in 0..4 {
            edges.push(edge(func_a.id, nodes[a].id, EdgeKind::Calls, 0.1, 1.0));
            edges.push(edge(func_b.id, nodes[4 + a].id, EdgeKind::Calls, 0.1, 1.0));
        }

        let det = detect_communities(Uuid::nil(), &nodes, &edges, &CommunityConfig::default());
        // Which communities contain a node from file "shared/F.rs"?
        let mut files_to_comms: std::collections::BTreeMap<
            String,
            std::collections::BTreeSet<Uuid>,
        > = std::collections::BTreeMap::new();
        for c in &det.communities {
            for sid in &c.member_stable_ids {
                let file = sid.split(':').next().unwrap_or("").to_string();
                files_to_comms.entry(file).or_default().insert(c.id);
            }
        }
        let shared = files_to_comms.get("shared/F.rs").expect("file present");
        assert!(
            shared.len() >= 2,
            "file shared/F.rs should overlap >=2 communities, got {}",
            shared.len()
        );
    }

    /// The repository node and its star edges are excluded.
    #[test]
    fn excludes_repository_node() {
        let mut nodes = vec![node("repo", NodeKind::Repository, "rust")];
        nodes.push(node("a/f0:function:a0", NodeKind::Function, "rust"));
        nodes.push(node("a/f1:function:a1", NodeKind::Function, "rust"));
        let repo_id = nodes[0].id;
        let mut edges = vec![
            edge(repo_id, nodes[1].id, EdgeKind::Contains, 0.1, 1.0),
            edge(repo_id, nodes[2].id, EdgeKind::Contains, 0.1, 1.0),
            edge(nodes[1].id, nodes[2].id, EdgeKind::Calls, 0.1, 1.0),
        ];
        let _ = &mut edges;
        let det = detect_communities(Uuid::nil(), &nodes, &edges, &CommunityConfig::default());
        assert_eq!(det.node_count, 2, "repository node excluded from detection");
    }
}
