//! Use-case: feature correlations.
//!
//! INTRA (per repo): pairs of L1 communities that share files (a file whose
//! nodes span both) or symbol names (same-name Function/Type nodes in both).
//!
//! CROSS (every ordered pair of distinct repos, consumer → provider):
//! - `package_dep`: a consumer File node whose `metadata.packages` names a
//!   package the provider publishes (package names are supplied by the caller
//!   — the entry layer reads manifests; use-cases never touch the filesystem).
//! - `http_route`: a provider `HttpRoute` node (`GET /users/:id`) whose path
//!   matches a call path recorded on a consumer File node under
//!   `metadata.http_calls` (analyze must populate it from fetch/axios/reqwest
//!   string literals; absent metadata simply yields no links).
//!
//! Everything is resolved from the persisted index through the ports; the
//! report page is hash-gated on the sorted link keys.

use crate::domain::hash;
use crate::domain::model::{Node, NodeKind, Repo};
use crate::domain::provenance::{source, Breadcrumb};
use crate::ports::community_store::CommunityStore;
use crate::ports::index_store::IndexStore;
use crate::ports::pages::{PageKind, PageStore};
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use uuid::Uuid;

pub const MAX_INTRA_PER_REPO: usize = 50;
pub const MAX_CROSS: usize = 100;
pub const MAX_EVIDENCE_FILES: usize = 5;
pub const PACKAGE_DEP_CONFIDENCE: f32 = 0.9;
pub const HTTP_ROUTE_CONFIDENCE: f32 = 0.75;

/// A member repository of the correlation run.
#[derive(Debug, Clone)]
pub struct RepoRef {
    pub alias: String,
    pub repo: Repo,
}

pub struct Deps<'a> {
    pub store: &'a dyn IndexStore,
    pub communities: &'a dyn CommunityStore,
    pub pages: &'a dyn PageStore,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FeatureRef {
    pub id: Uuid,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct IntraPair {
    pub repo: String,
    pub a: String,
    pub b: String,
    pub shared_files: Vec<String>,
    pub shared_symbols: Vec<String>,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Link {
    pub consumer_repo: String,
    pub consumer_feature: FeatureRef,
    pub provider_repo: String,
    pub provider_feature: FeatureRef,
    pub kind: String,
    pub evidence: Vec<String>,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PageReport {
    pub path: String,
    pub written: bool,
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CorrelationReport {
    pub intra: Vec<IntraPair>,
    pub cross: Vec<Link>,
    pub page: PageReport,
    pub provenance: Vec<Breadcrumb>,
}

/// One loaded repo: its communities plus derived per-community indexes.
struct RepoView {
    alias: String,
    communities: Vec<CommunityView>,
    /// node id → community id.
    node_community: HashMap<Uuid, Uuid>,
    /// file id → path.
    file_paths: HashMap<Uuid, String>,
    nodes: Vec<Node>,
}

struct CommunityView {
    id: Uuid,
    label: String,
    member_count: usize,
    files: BTreeSet<String>,
    symbols: BTreeSet<String>,
    /// file id → number of member nodes in that file.
    file_node_counts: HashMap<Uuid, usize>,
}

impl RepoView {
    fn feature(&self, id: Uuid) -> FeatureRef {
        let label = self
            .communities
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.label.clone())
            .unwrap_or_else(|| id.to_string());
        FeatureRef { id, label }
    }

    /// The community of a file: the one holding the most nodes of that file.
    fn community_of_file(&self, file_id: Uuid) -> Option<Uuid> {
        self.communities
            .iter()
            .filter_map(|c| c.file_node_counts.get(&file_id).map(|n| (*n, c.id)))
            .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)))
            .map(|(_, id)| id)
    }

    /// Community owning the most nodes of the repo's entry files; fallback:
    /// the largest community.
    fn provider_root_community(&self) -> Option<Uuid> {
        let entry_ids: Vec<Uuid> = self
            .file_paths
            .iter()
            .filter(|(_, p)| is_entry_file(p))
            .map(|(id, _)| *id)
            .collect();
        let mut best: Option<(usize, Uuid)> = None;
        for c in &self.communities {
            let n: usize = entry_ids
                .iter()
                .filter_map(|f| c.file_node_counts.get(f))
                .sum();
            if n > 0 && best.is_none_or(|(bn, bid)| n > bn || (n == bn && c.id < bid)) {
                best = Some((n, c.id));
            }
        }
        best.map(|(_, id)| id).or_else(|| {
            self.communities
                .iter()
                .max_by(|a, b| {
                    a.member_count
                        .cmp(&b.member_count)
                        .then_with(|| b.id.cmp(&a.id))
                })
                .map(|c| c.id)
        })
    }
}

/// Shallow, conventionally-named entry points of a package.
pub fn is_entry_file(path: &str) -> bool {
    let depth = path.matches('/').count();
    let base = path.rsplit('/').next().unwrap_or(path);
    let named = matches!(
        base,
        "lib.rs"
            | "main.rs"
            | "index.ts"
            | "index.tsx"
            | "index.js"
            | "index.mjs"
            | "index.cjs"
            | "__init__.py"
            | "main.py"
    );
    named && depth <= 2
}

fn string_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|xs| {
            xs.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

async fn load_repo(deps: &Deps<'_>, r: &RepoRef, crumbs: &mut Vec<Breadcrumb>) -> Result<RepoView> {
    let graph = deps.store.load_graph(r.repo.id).await?;
    let communities = deps.communities.load_communities(r.repo.id).await?;
    let members = deps.communities.load_members(r.repo.id).await?;
    crumbs.push(
        Breadcrumb::new(
            source::POSTGRES,
            "load",
            format!(
                "{}: {} nodes, {} communities, {} memberships",
                r.alias,
                graph.nodes.len(),
                communities.len(),
                members.len()
            ),
        )
        .with_locator(r.repo.root_path.clone()),
    );
    let file_paths: HashMap<Uuid, String> =
        graph.files.iter().map(|f| (f.id, f.path.clone())).collect();
    let by_id: HashMap<Uuid, &Node> = graph.nodes.iter().map(|n| (n.id, n)).collect();
    let mut node_community = HashMap::new();
    let mut views: BTreeMap<Uuid, CommunityView> = communities
        .iter()
        .map(|c| {
            (
                c.id,
                CommunityView {
                    id: c.id,
                    label: c.label.clone(),
                    member_count: 0,
                    files: c.files.iter().cloned().collect(),
                    symbols: BTreeSet::new(),
                    file_node_counts: HashMap::new(),
                },
            )
        })
        .collect();
    for (cid, nid) in &members {
        let Some(view) = views.get_mut(cid) else {
            continue;
        };
        node_community.insert(*nid, *cid);
        view.member_count += 1;
        let Some(node) = by_id.get(nid) else { continue };
        if let Some(fid) = node.file_id {
            *view.file_node_counts.entry(fid).or_default() += 1;
            if let Some(p) = file_paths.get(&fid) {
                view.files.insert(p.clone());
            }
        }
        if matches!(node.kind, NodeKind::Function | NodeKind::Type) {
            view.symbols.insert(node.name.clone());
        }
    }
    Ok(RepoView {
        alias: r.alias.clone(),
        communities: views.into_values().collect(),
        node_community,
        file_paths,
        nodes: graph.nodes,
    })
}

fn jaccard(shared: usize, a: usize, b: usize) -> f32 {
    let union = a + b - shared;
    if union == 0 {
        0.0
    } else {
        shared as f32 / union as f32
    }
}

/// Pairwise overlap score: 0.7 × file Jaccard + 0.3 × symbol Jaccard.
pub fn overlap_score(
    shared_files: usize,
    files_a: usize,
    files_b: usize,
    shared_symbols: usize,
    syms_a: usize,
    syms_b: usize,
) -> f32 {
    0.7 * jaccard(shared_files, files_a, files_b) + 0.3 * jaccard(shared_symbols, syms_a, syms_b)
}

fn intra_pairs(view: &RepoView) -> Vec<IntraPair> {
    let mut out = Vec::new();
    let cs = &view.communities;
    for i in 0..cs.len() {
        for j in (i + 1)..cs.len() {
            let (a, b) = (&cs[i], &cs[j]);
            let shared_files: Vec<String> = a.files.intersection(&b.files).cloned().collect();
            let shared_symbols: Vec<String> = a.symbols.intersection(&b.symbols).cloned().collect();
            if shared_files.is_empty() && shared_symbols.len() < 2 {
                continue;
            }
            let score = overlap_score(
                shared_files.len(),
                a.files.len(),
                b.files.len(),
                shared_symbols.len(),
                a.symbols.len(),
                b.symbols.len(),
            );
            out.push(IntraPair {
                repo: view.alias.clone(),
                a: a.label.clone(),
                b: b.label.clone(),
                shared_files,
                shared_symbols,
                score,
            });
        }
    }
    out.sort_by(|x, y| {
        y.score
            .partial_cmp(&x.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| x.a.cmp(&y.a))
            .then_with(|| x.b.cmp(&y.b))
    });
    out.truncate(MAX_INTRA_PER_REPO);
    out
}

/// `package_dep` links from `consumer` to `provider` for the packages `provider` publishes.
fn package_dep_links(consumer: &RepoView, provider: &RepoView, published: &[String]) -> Vec<Link> {
    let Some(provider_root) = provider.provider_root_community() else {
        return Vec::new();
    };
    // (consumer community, package) → consumer files
    let mut grouped: BTreeMap<(Uuid, String), BTreeSet<String>> = BTreeMap::new();
    for node in consumer.nodes.iter().filter(|n| n.kind == NodeKind::File) {
        let Some(fid) = node.file_id else { continue };
        let packages = string_list(&node.metadata["packages"]);
        if packages.is_empty() {
            continue;
        }
        let Some(cid) = consumer
            .node_community
            .get(&node.id)
            .copied()
            .or_else(|| consumer.community_of_file(fid))
        else {
            continue;
        };
        let path = consumer
            .file_paths
            .get(&fid)
            .cloned()
            .unwrap_or_else(|| node.name.clone());
        for pkg in packages {
            if published.iter().any(|p| p == &pkg) {
                grouped.entry((cid, pkg)).or_default().insert(path.clone());
            }
        }
    }
    grouped
        .into_iter()
        .map(|((cid, pkg), files)| {
            let mut evidence = vec![format!("package {pkg}")];
            evidence.extend(files.iter().take(MAX_EVIDENCE_FILES).cloned());
            Link {
                consumer_repo: consumer.alias.clone(),
                consumer_feature: consumer.feature(cid),
                provider_repo: provider.alias.clone(),
                provider_feature: provider.feature(provider_root),
                kind: "package_dep".into(),
                evidence,
                confidence: PACKAGE_DEP_CONFIDENCE,
            }
        })
        .collect()
}

const STATIC_EXTS: &[&str] = &[
    "js", "mjs", "cjs", "css", "map", "png", "jpg", "jpeg", "gif", "svg", "ico", "webp", "woff",
    "woff2", "ttf", "html", "json", "txt", "xml", "pdf",
];

/// Normalise a route or call path: strip query/fragment, lower-case, params
/// (`:id`, `{id}`, `<id>`, `[id]`, `${…}`, `*`) → `*`. `None` when it is not a
/// route-like path (relative, static asset, all-wildcard, spaces, `..`).
pub fn normalize_route(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    if let Some(i) = s.find(['?', '#']) {
        s = &s[..i];
    }
    if !s.starts_with('/') || s.starts_with("//") || s.contains(' ') || s.contains("..") {
        return None;
    }
    let mut segs: Vec<String> = Vec::new();
    for seg in s.split('/').filter(|x| !x.is_empty()) {
        let wild = seg.starts_with(':')
            || seg.starts_with('{')
            || seg.starts_with('<')
            || seg.starts_with('[')
            || seg.contains("${")
            || seg.contains('*');
        if wild {
            segs.push("*".to_string());
        } else {
            let lower = seg.to_ascii_lowercase();
            if let Some((_, ext)) = lower.rsplit_once('.') {
                if STATIC_EXTS.contains(&ext) {
                    return None;
                }
            }
            segs.push(lower);
        }
    }
    if segs.is_empty() || segs.iter().all(|x| x == "*") {
        return None;
    }
    Some(format!("/{}", segs.join("/")))
}

fn seg_eq(a: &str, b: &str) -> bool {
    a == "*" || b == "*" || a == b
}

/// Do two normalised paths denote the same route? Same segment count with
/// every segment equal-or-wildcard, or the shorter (≥ 2 segments) is a suffix
/// of the longer; either way at least one concrete segment must coincide, and
/// a single-segment route must be ≥ 4 chars.
pub fn routes_match(a: &str, b: &str) -> bool {
    let sa: Vec<&str> = a.split('/').filter(|x| !x.is_empty()).collect();
    let sb: Vec<&str> = b.split('/').filter(|x| !x.is_empty()).collect();
    if sa.is_empty() || sb.is_empty() {
        return false;
    }
    let concrete = |xs: &[&str], ys: &[&str]| {
        xs.iter()
            .zip(ys.iter())
            .any(|(x, y)| x != &"*" && y != &"*" && x == y)
    };
    if sa.len() == sb.len() {
        let all = sa.iter().zip(sb.iter()).all(|(x, y)| seg_eq(x, y));
        if !all || !concrete(&sa, &sb) {
            return false;
        }
        return sa.len() > 1 || sa[0].len() >= 4;
    }
    let (short, long) = if sa.len() < sb.len() {
        (sa, sb)
    } else {
        (sb, sa)
    };
    if short.len() < 2 {
        return false;
    }
    let tail = &long[long.len() - short.len()..];
    short.iter().zip(tail.iter()).all(|(x, y)| seg_eq(x, y)) && concrete(&short, tail)
}

/// Split `GET /users/:id` into (method, normalised path).
fn route_node_path(name: &str) -> Option<(String, String)> {
    let (method, path) = name.split_once(' ').unwrap_or(("ANY", name));
    Some((method.to_ascii_uppercase(), normalize_route(path)?))
}

/// `http_route` links from `consumer` call paths to `provider` route nodes.
fn http_route_links(consumer: &RepoView, provider: &RepoView) -> Vec<Link> {
    let mut routes: Vec<(String, String, Uuid)> = Vec::new(); // (method, path, provider community)
    for node in provider
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::HttpRoute)
    {
        let Some((method, path)) = route_node_path(&node.name) else {
            continue;
        };
        let cid = provider
            .node_community
            .get(&node.id)
            .copied()
            .or_else(|| node.file_id.and_then(|f| provider.community_of_file(f)));
        let Some(cid) = cid else { continue };
        routes.push((method, path, cid));
    }
    if routes.is_empty() {
        return Vec::new();
    }
    routes.sort();
    routes.dedup();
    // (consumer community, provider community) → evidence set
    let mut grouped: BTreeMap<(Uuid, Uuid), BTreeSet<String>> = BTreeMap::new();
    for node in consumer.nodes.iter().filter(|n| n.kind == NodeKind::File) {
        let Some(fid) = node.file_id else { continue };
        let calls = string_list(&node.metadata["http_calls"]);
        if calls.is_empty() {
            continue;
        }
        let Some(ccid) = consumer
            .node_community
            .get(&node.id)
            .copied()
            .or_else(|| consumer.community_of_file(fid))
        else {
            continue;
        };
        let path = consumer
            .file_paths
            .get(&fid)
            .cloned()
            .unwrap_or_else(|| node.name.clone());
        for raw in calls {
            let Some(call) = normalize_route(&raw) else {
                continue;
            };
            for (method, route, pcid) in &routes {
                if routes_match(&call, route) {
                    grouped
                        .entry((ccid, *pcid))
                        .or_default()
                        .insert(format!("{method} {route} <- {call} ({path})"));
                }
            }
        }
    }
    grouped
        .into_iter()
        .map(|((ccid, pcid), evidence)| Link {
            consumer_repo: consumer.alias.clone(),
            consumer_feature: consumer.feature(ccid),
            provider_repo: provider.alias.clone(),
            provider_feature: provider.feature(pcid),
            kind: "http_route".into(),
            evidence: evidence.into_iter().take(MAX_EVIDENCE_FILES).collect(),
            confidence: HTTP_ROUTE_CONFIDENCE,
        })
        .collect()
}

fn link_key(l: &Link) -> String {
    format!(
        "{}:{}>{}:{}:{}:{}",
        l.consumer_repo,
        l.consumer_feature.id,
        l.provider_repo,
        l.provider_feature.id,
        l.kind,
        l.evidence.join(",")
    )
}

/// Run the correlation report over `repos`.
///
/// `manifests` = `(alias, published package name)` pairs supplied by the
/// caller (read from package.json / Cargo.toml by the entry layer).
pub async fn correlations(
    deps: &Deps<'_>,
    repos: &[RepoRef],
    manifests: &[(String, String)],
    out_dir: &Path,
    min_confidence: f32,
) -> Result<CorrelationReport> {
    let mut crumbs = Vec::new();
    let mut views = Vec::with_capacity(repos.len());
    for r in repos {
        views.push(load_repo(deps, r, &mut crumbs).await?);
    }

    let mut intra = Vec::new();
    for v in &views {
        let pairs = intra_pairs(v);
        tracing::info!(repo = %v.alias, pairs = pairs.len(), "intra-repo correlations");
        intra.extend(pairs);
    }
    crumbs.push(Breadcrumb::new(source::GRAPH, "overlap", format!("{} intra-repo pairs (shared files ≥ 1 or shared symbols ≥ 2; 0.7×file + 0.3×symbol Jaccard)", intra.len())));

    let mut cross: Vec<Link> = Vec::new();
    for consumer in &views {
        for provider in &views {
            if consumer.alias == provider.alias {
                continue;
            }
            let published: Vec<String> = manifests
                .iter()
                .filter(|(a, _)| a == &provider.alias)
                .map(|(_, p)| p.clone())
                .collect();
            if !published.is_empty() {
                cross.extend(package_dep_links(consumer, provider, &published));
            }
            cross.extend(http_route_links(consumer, provider));
        }
    }
    let before = cross.len();
    cross.retain(|l| l.confidence >= min_confidence);
    cross.sort_by(|x, y| {
        y.confidence
            .partial_cmp(&x.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| link_key(x).cmp(&link_key(y)))
    });
    cross.truncate(MAX_CROSS);
    tracing::info!(
        links = cross.len(),
        dropped = before - cross.len(),
        "cross-repo links"
    );
    crumbs.push(Breadcrumb::new(
        source::GRAPH,
        "link",
        format!("{} cross-repo links kept of {before} (package_dep via File metadata.packages × published names; http_route via metadata.http_calls × HttpRoute nodes; min_confidence {min_confidence})", cross.len()),
    ));

    let mut keys: Vec<String> = cross.iter().map(link_key).collect();
    keys.extend(
        intra
            .iter()
            .map(|p| format!("{}:{}~{}:{:.4}", p.repo, p.a, p.b, p.score)),
    );
    keys.sort();
    let content_hash = hash::combine(&keys);
    let path = out_dir.join("correlations.html");
    let aliases: Vec<&str> = repos.iter().map(|r| r.alias.as_str()).collect();
    let manifest = json!({
        "title": "Feature correlations",
        "project": aliases.join(", "),
        "summary": format!("{} intra-repo feature pairs and {} cross-repo links across {} repositories.", intra.len(), cross.len(), repos.len()),
        "intra": intra.iter().map(|p| json!({
            "repo": p.repo, "a": p.a, "b": p.b,
            "shared": p.shared_files.iter().chain(p.shared_symbols.iter()).collect::<Vec<_>>(),
            "shared_files": p.shared_files, "shared_symbols": p.shared_symbols, "score": p.score,
        })).collect::<Vec<_>>(),
        "cross": cross.iter().map(|l| json!({
            "consumer": format!("{} / {}", l.consumer_repo, l.consumer_feature.label),
            "provider": format!("{} / {}", l.provider_repo, l.provider_feature.label),
            "consumer_repo": l.consumer_repo, "consumer_feature": l.consumer_feature,
            "provider_repo": l.provider_repo, "provider_feature": l.provider_feature,
            "kind": l.kind, "evidence": l.evidence.join("; "), "evidence_items": l.evidence, "confidence": l.confidence,
        })).collect::<Vec<_>>(),
        "content_hash": content_hash,
        "provenance": crumbs,
    });
    let written = deps
        .pages
        .write_page(&path, PageKind::Correlations, &manifest)?;
    crumbs.push(
        Breadcrumb::new(
            source::PAGE,
            "write",
            if written {
                "correlations page written"
            } else {
                "correlations page unchanged (hash-gated)"
            },
        )
        .with_locator(path.display().to_string()),
    );
    Ok(CorrelationReport {
        intra,
        cross,
        page: PageReport {
            path: path.display().to_string(),
            written,
            content_hash,
        },
        provenance: crumbs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        Chunk, Community, Edge, IndexBundle, Language, ModelRef, SourceFile,
    };
    use crate::ports::index_store::Graph;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct FakeStore {
        graphs: HashMap<Uuid, Graph>,
        communities: HashMap<Uuid, Vec<Community>>,
        members: HashMap<Uuid, Vec<(Uuid, Uuid)>>,
    }

    #[async_trait]
    impl IndexStore for FakeStore {
        async fn upsert_repo(&self, _: &Repo) -> Result<()> {
            unimplemented!()
        }
        async fn find_repo(&self, _: &str) -> Result<Option<Repo>> {
            unimplemented!()
        }
        async fn list_repos(&self) -> Result<Vec<Repo>> {
            unimplemented!()
        }
        async fn set_repo_commit(&self, _: Uuid, _: Option<&str>) -> Result<()> {
            unimplemented!()
        }
        async fn replace_repo(&self, _: Uuid, _: &IndexBundle) -> Result<()> {
            unimplemented!()
        }
        async fn merge_files(&self, _: Uuid, _: &[String], _: &IndexBundle) -> Result<()> {
            unimplemented!()
        }
        async fn load_graph(&self, repo_id: Uuid) -> Result<Graph> {
            Ok(self.graphs.get(&repo_id).cloned().unwrap_or_default())
        }
        async fn chunks_missing_embeddings(&self, _: Uuid, _: &ModelRef) -> Result<Vec<Chunk>> {
            unimplemented!()
        }
        async fn save_embeddings(
            &self,
            _: &ModelRef,
            _: &[(Uuid, String, Vec<f32>)],
        ) -> Result<()> {
            unimplemented!()
        }
        async fn count_chunks(&self, _: Uuid) -> Result<i64> {
            unimplemented!()
        }
    }

    #[async_trait]
    impl CommunityStore for FakeStore {
        async fn replace_communities(
            &self,
            _: Uuid,
            _: &[Community],
            _: &[(Uuid, Uuid)],
        ) -> Result<()> {
            unimplemented!()
        }
        async fn load_communities(&self, repo_id: Uuid) -> Result<Vec<Community>> {
            Ok(self.communities.get(&repo_id).cloned().unwrap_or_default())
        }
        async fn load_members(&self, repo_id: Uuid) -> Result<Vec<(Uuid, Uuid)>> {
            Ok(self.members.get(&repo_id).cloned().unwrap_or_default())
        }
        async fn communities_needing_summary(
            &self,
            _: Uuid,
            _: &ModelRef,
        ) -> Result<Vec<Community>> {
            unimplemented!()
        }
        async fn save_summary_embedding(&self, _: Uuid, _: &ModelRef, _: &[f32]) -> Result<()> {
            unimplemented!()
        }
    }

    #[derive(Default)]
    struct FakePages {
        written: Mutex<HashMap<String, Value>>,
    }

    impl PageStore for FakePages {
        fn write_page(&self, path: &Path, _: PageKind, manifest: &Value) -> Result<bool> {
            let key = path.display().to_string();
            let mut w = self.written.lock().unwrap();
            if w.get(&key)
                .map(|m| m["content_hash"] == manifest["content_hash"])
                .unwrap_or(false)
            {
                return Ok(false);
            }
            w.insert(key, manifest.clone());
            Ok(true)
        }
        fn read_manifest(&self, path: &Path) -> Option<Value> {
            self.written
                .lock()
                .unwrap()
                .get(&path.display().to_string())
                .cloned()
        }
    }

    fn id(s: &str) -> Uuid {
        hash::scoped_id(Uuid::nil(), s)
    }

    fn node(
        repo: Uuid,
        file: Option<Uuid>,
        kind: NodeKind,
        stable: &str,
        name: &str,
        metadata: Value,
    ) -> Node {
        Node {
            id: hash::scoped_id(repo, stable),
            repo_id: repo,
            file_id: file,
            kind,
            stable_id: stable.into(),
            name: name.into(),
            signature: None,
            doc: None,
            line_start: None,
            line_end: None,
            metadata,
        }
    }

    fn file(repo: Uuid, path: &str) -> SourceFile {
        SourceFile {
            id: hash::scoped_id(repo, &format!("file:{path}")),
            repo_id: repo,
            path: path.into(),
            language: Language::TypeScript,
            content_hash: String::new(),
            line_count: 1,
        }
    }

    fn community(repo: Uuid, label: &str, files: &[&str]) -> Community {
        Community {
            id: hash::scoped_id(repo, &format!("community:{label}")),
            repo_id: repo,
            label: label.into(),
            member_count: 0,
            summary: None,
            summary_hash: None,
            top_members: vec![],
            files: files.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn repo_ref(alias: &str) -> RepoRef {
        RepoRef {
            alias: alias.into(),
            repo: Repo {
                id: id(alias),
                name: alias.into(),
                root_path: format!("/{alias}"),
                commit_sha: None,
            },
        }
    }

    /// Two repos: `client` (files with packages + http_calls) and `api`
    /// (publishes "api-sdk", registers routes). `api` also has two
    /// overlapping communities for the intra test.
    fn fixture() -> (FakeStore, Vec<RepoRef>) {
        let client = id("client");
        let api = id("api");
        let cf = file(client, "src/app.ts");
        let cf2 = file(client, "src/other.ts");
        let client_nodes = vec![
            node(
                client,
                Some(cf.id),
                NodeKind::File,
                "file:src/app.ts",
                "src/app.ts",
                json!({"packages": ["api-sdk", "react"], "http_calls": ["/users/${id}", "/static/logo.png"]}),
            ),
            node(
                client,
                Some(cf.id),
                NodeKind::Function,
                "typescript:src/app.ts:function:main",
                "main",
                json!({}),
            ),
            node(
                client,
                Some(cf2.id),
                NodeKind::File,
                "file:src/other.ts",
                "src/other.ts",
                json!({"packages": ["react"]}),
            ),
        ];
        let c_ui = community(client, "ui", &[]);
        let c_misc = community(client, "misc", &[]);
        let client_members = vec![
            (c_ui.id, client_nodes[0].id),
            (c_ui.id, client_nodes[1].id),
            (c_misc.id, client_nodes[2].id),
        ];

        let af = file(api, "src/index.ts");
        let af2 = file(api, "src/users.ts");
        let af3 = file(api, "src/shared.ts");
        let api_nodes = vec![
            node(
                api,
                Some(af.id),
                NodeKind::File,
                "file:src/index.ts",
                "src/index.ts",
                json!({}),
            ),
            node(
                api,
                Some(af.id),
                NodeKind::Function,
                "typescript:src/index.ts:function:boot",
                "boot",
                json!({}),
            ),
            node(
                api,
                Some(af2.id),
                NodeKind::HttpRoute,
                "http_route:GET /users/:id",
                "GET /users/:id",
                json!({}),
            ),
            node(
                api,
                Some(af2.id),
                NodeKind::Function,
                "typescript:src/users.ts:function:getUser",
                "getUser",
                json!({}),
            ),
            node(
                api,
                Some(af3.id),
                NodeKind::Function,
                "typescript:src/shared.ts:function:helper",
                "helper",
                json!({}),
            ),
            node(
                api,
                Some(af3.id),
                NodeKind::Type,
                "typescript:src/shared.ts:type:Ctx",
                "Ctx",
                json!({}),
            ),
            node(
                api,
                Some(af3.id),
                NodeKind::Function,
                "typescript:src/shared.ts:function:helper2",
                "helper",
                json!({}),
            ),
        ];
        let a_core = community(api, "core", &["src/index.ts"]);
        let a_users = community(api, "users", &["src/users.ts"]);
        let api_members = vec![
            (a_core.id, api_nodes[0].id),
            (a_core.id, api_nodes[1].id),
            (a_core.id, api_nodes[4].id),
            (a_users.id, api_nodes[2].id),
            (a_users.id, api_nodes[3].id),
            (a_users.id, api_nodes[5].id),
            (a_users.id, api_nodes[6].id),
        ];
        let store = FakeStore {
            graphs: HashMap::from([
                (
                    client,
                    Graph {
                        files: vec![cf, cf2],
                        nodes: client_nodes,
                        edges: Vec::<Edge>::new(),
                    },
                ),
                (
                    api,
                    Graph {
                        files: vec![af, af2, af3],
                        nodes: api_nodes,
                        edges: vec![],
                    },
                ),
            ]),
            communities: HashMap::from([
                (client, vec![c_ui, c_misc]),
                (api, vec![a_core, a_users]),
            ]),
            members: HashMap::from([(client, client_members), (api, api_members)]),
        };
        (store, vec![repo_ref("client"), repo_ref("api")])
    }

    #[tokio::test]
    async fn intra_overlap_scoring() {
        let (store, repos) = fixture();
        let pages = FakePages::default();
        let deps = Deps {
            store: &store,
            communities: &store,
            pages: &pages,
        };
        let rep = correlations(&deps, &repos, &[], Path::new("/out"), 0.5)
            .await
            .unwrap();
        let api_pairs: Vec<&IntraPair> = rep.intra.iter().filter(|p| p.repo == "api").collect();
        assert_eq!(api_pairs.len(), 1);
        let p = api_pairs[0];
        assert_eq!(p.shared_files, vec!["src/shared.ts"]);
        assert_eq!(p.shared_symbols, vec!["helper"]);
        // files: core {index, shared} users {users, shared} → 1/3; symbols: core {boot, helper} users {getUser, Ctx, helper} → 1/4
        let expected = 0.7 * (1.0 / 3.0) + 0.3 * 0.25;
        assert!((p.score - expected).abs() < 1e-6, "{}", p.score);
        // client communities share nothing
        assert!(rep.intra.iter().all(|p| p.repo != "client"));
    }

    #[tokio::test]
    async fn package_dep_and_http_route_links() {
        let (store, repos) = fixture();
        let pages = FakePages::default();
        let deps = Deps {
            store: &store,
            communities: &store,
            pages: &pages,
        };
        let manifests = vec![("api".to_string(), "api-sdk".to_string())];
        let rep = correlations(&deps, &repos, &manifests, Path::new("/out"), 0.5)
            .await
            .unwrap();
        let kinds: Vec<&str> = rep.cross.iter().map(|l| l.kind.as_str()).collect();
        assert_eq!(kinds, vec!["package_dep", "http_route"]);
        let pkg = &rep.cross[0];
        assert_eq!(
            (
                pkg.consumer_repo.as_str(),
                pkg.consumer_feature.label.as_str()
            ),
            ("client", "ui")
        );
        assert_eq!(
            (
                pkg.provider_repo.as_str(),
                pkg.provider_feature.label.as_str()
            ),
            ("api", "core")
        );
        assert_eq!(pkg.evidence, vec!["package api-sdk", "src/app.ts"]);
        assert_eq!(pkg.confidence, PACKAGE_DEP_CONFIDENCE);
        let route = &rep.cross[1];
        assert_eq!(route.provider_feature.label, "users");
        assert_eq!(
            route.evidence,
            vec!["GET /users/* <- /users/* (src/app.ts)"]
        );
        // "react" is not published by api, "misc" gets no link
        assert!(rep.cross.iter().all(|l| l.consumer_feature.label != "misc"));
        // min_confidence filter drops the 0.75 route link
        let rep2 = correlations(&deps, &repos, &manifests, Path::new("/out2"), 0.8)
            .await
            .unwrap();
        assert_eq!(rep2.cross.len(), 1);
    }

    #[tokio::test]
    async fn page_is_hash_gated() {
        let (store, repos) = fixture();
        let pages = FakePages::default();
        let deps = Deps {
            store: &store,
            communities: &store,
            pages: &pages,
        };
        let first = correlations(&deps, &repos, &[], Path::new("/out"), 0.5)
            .await
            .unwrap();
        assert!(first.page.written);
        assert_eq!(first.page.path, "/out/correlations.html");
        let second = correlations(&deps, &repos, &[], Path::new("/out"), 0.5)
            .await
            .unwrap();
        assert!(!second.page.written);
        assert_eq!(first.page.content_hash, second.page.content_hash);
        let manifest = pages
            .read_manifest(Path::new("/out/correlations.html"))
            .unwrap();
        assert_eq!(manifest["content_hash"], first.page.content_hash);
        assert!(manifest["intra"].is_array() && manifest["cross"].is_array());
        // different inputs → different hash
        let third = correlations(
            &deps,
            &repos,
            &[("api".into(), "api-sdk".into())],
            Path::new("/out"),
            0.5,
        )
        .await
        .unwrap();
        assert!(third.page.written);
        assert_ne!(third.page.content_hash, first.page.content_hash);
    }

    #[test]
    fn route_normalisation() {
        assert_eq!(normalize_route("/users/:id"), Some("/users/*".into()));
        assert_eq!(
            normalize_route("/Users/{id}/posts?x=1"),
            Some("/users/*/posts".into())
        );
        assert_eq!(normalize_route("/a/${id}"), Some("/a/*".into()));
        assert_eq!(normalize_route("/static/logo.png"), None);
        assert_eq!(normalize_route("relative/path"), None);
        assert_eq!(normalize_route("/:a/:b"), None);
        assert_eq!(normalize_route("//cdn"), None);
        assert_eq!(normalize_route("/a b"), None);
    }

    #[test]
    fn route_matching_table() {
        let table = [
            ("/users/*", "/users/*", true),
            ("/users/42", "/users/*", true),
            ("/users/*", "/posts/*", false),
            ("/*/*", "/users/*", false), // no concrete segment in common
            ("/api/v1/users/*", "/users/*", true), // suffix match
            ("/api/v1/users/*", "/*", false), // suffix too short
            ("/health", "/health", true),
            ("/abc", "/abc", false),               // single segment too short
            ("/api/users", "/api/users/*", false), // different length, suffix mismatch
            ("", "/users", false),
        ];
        for (a, b, want) in table {
            assert_eq!(routes_match(a, b), want, "{a} vs {b}");
            assert_eq!(routes_match(b, a), want, "{b} vs {a}");
        }
    }

    #[test]
    fn entry_files() {
        assert!(is_entry_file("src/lib.rs"));
        assert!(is_entry_file("index.ts"));
        assert!(is_entry_file("src/pkg/__init__.py"));
        assert!(!is_entry_file("src/a/b/c/index.ts"));
        assert!(!is_entry_file("src/users.ts"));
    }
}
