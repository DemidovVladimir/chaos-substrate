//! `compose` across a project: after every member repo's feature site is
//! written (and `correlations` has run), ONE hub page under the project's
//! `docs/features_memory/index.html` shows
//!
//! - what was indexed, per repo: commit, files, symbols, edges, chunks,
//!   features, languages, and a link to the repo's feature site;
//! - the feature graph: every feature community is a node (colour = the
//!   dominant language of its files); edges are the call/import/... weight
//!   between two features of one repo (graph edges crossing community
//!   boundaries) and the cross-repo links `correlations` found;
//! - every feature of every repo, the cross-repo links with evidence, and the
//!   project feature (question) pages already in the directory;
//! - the entry points (`entry_points`): the GraphQL API with its callers,
//!   public contract functions, MCP tools, HTTP routes, client hooks, and for
//!   text the document plan and transcript timelines. Features holding an API
//!   field, contract function, MCP tool or route are always drawn.
//!
//! Everything comes from the persisted index through the ports; feature page
//! links come from the site index manifests `compose` just wrote. The page is
//! hash-gated on the data it shows.

use crate::application::compose::{page_hash, snippet, ComposeReport};
use crate::application::correlations::CorrelationReport;
use crate::application::entry_points::{entry_points, RepoIndex};
use crate::domain::model::{Community, EdgeKind, Language, Repo};
use crate::domain::provenance::{source, Breadcrumb};
use crate::ports::community_store::CommunityStore;
use crate::ports::index_store::Graph;
use crate::ports::index_store::IndexStore;
use crate::ports::pages::{PageKind, PageStore};
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

/// Largest features per repo drawn in the graph (cross-link endpoints are
/// always drawn on top of these).
pub const GRAPH_PER_REPO: usize = 14;

pub struct Deps<'a> {
    pub store: &'a dyn IndexStore,
    pub communities: &'a dyn CommunityStore,
    pub pages: &'a dyn PageStore,
}

/// One project member whose feature site was just composed.
pub struct Member {
    pub alias: String,
    pub repo: Repo,
    pub root: PathBuf,
    pub site: ComposeReport,
}

/// A project feature page (one question answered across repos) found in the
/// hub's directory.
#[derive(Debug, Clone)]
pub struct QuestionPage {
    pub title: String,
    pub path: PathBuf,
    pub repos: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoSite {
    pub alias: String,
    pub site: String,
    pub pages_written: usize,
    pub pages_cached: usize,
    pub features: usize,
}

/// Compact return of a project `compose`.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectComposeReport {
    pub page: String,
    pub page_written: bool,
    pub content_hash: String,
    pub repos: Vec<RepoSite>,
    pub features: usize,
    pub graph_nodes: usize,
    pub cross_links: usize,
    /// Entry points listed, per category (non-empty only): graphql_fields,
    /// contract_functions, mcp_tools, http_routes, client_hooks, documents,
    /// timeline_entries.
    pub entry_points: BTreeMap<String, usize>,
    pub correlations_page: String,
    pub provenance: Vec<Breadcrumb>,
}

/// One member's persisted index, loaded once.
pub struct Loaded {
    pub graph: Graph,
    pub communities: Vec<Community>,
    pub members: Vec<(Uuid, Uuid)>,
    pub chunks: i64,
}

/// `to` relative to the directory `from_dir` (both absolute), `/`-separated.
pub fn rel_href(from_dir: &Path, to: &Path) -> String {
    let a: Vec<Component> = from_dir.components().collect();
    let b: Vec<Component> = to.components().collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<String> = vec!["..".into(); a.len() - common];
    parts.extend(
        b[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    }
}

/// Edge kinds that say one feature uses another (containment is structure).
fn is_dependency(kind: EdgeKind) -> bool {
    !matches!(kind, EdgeKind::Contains)
}

pub async fn compose_project(
    deps: Deps<'_>,
    project: &str,
    members: &[Member],
    correlations: &CorrelationReport,
    questions: &[QuestionPage],
    out_dir: &Path,
) -> Result<ProjectComposeReport> {
    let mut provenance = Vec::new();
    let mut repos_json = Vec::new();
    let mut features_json = Vec::new();
    let mut edges_json = Vec::new();
    let mut sites = Vec::new();
    let cross_ends: HashSet<Uuid> = correlations
        .cross
        .iter()
        .flat_map(|l| [l.consumer_feature.id, l.provider_feature.id])
        .collect();
    let mut totals = BTreeMap::<&str, i64>::new();

    let mut loaded = Vec::with_capacity(members.len());
    for m in members {
        let graph = deps
            .store
            .load_graph(m.repo.id)
            .await
            .with_context(|| format!("load graph of {}", m.alias))?;
        let mut communities = deps.communities.load_communities(m.repo.id).await?;
        communities.sort_by(|a, b| {
            b.member_count
                .cmp(&a.member_count)
                .then_with(|| a.id.cmp(&b.id))
        });
        loaded.push(Loaded {
            graph,
            communities,
            members: deps.communities.load_members(m.repo.id).await?,
            chunks: deps.store.count_chunks(m.repo.id).await?,
        });
    }
    let index: Vec<RepoIndex> = members
        .iter()
        .zip(&loaded)
        .map(|(m, l)| RepoIndex::new(&m.alias, &l.graph, &l.members))
        .collect();
    let entries = entry_points(&index);
    provenance.push(Breadcrumb::new(
        source::GRAPH,
        "entry_points",
        entries.provenance(),
    ));

    for (m, l) in members.iter().zip(&loaded) {
        let (graph, communities, members_rows, chunks) =
            (&l.graph, &l.communities, &l.members, l.chunks);

        // Languages by file count; per-feature dominant language by its files.
        let lang_of: HashMap<&str, Language> = graph
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.language))
            .collect();
        let mut langs: BTreeMap<&str, usize> = BTreeMap::new();
        for f in &graph.files {
            *langs.entry(f.language.as_str()).or_default() += 1;
        }
        let mut langs: Vec<(&str, usize)> = langs.into_iter().collect();
        langs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let symbols = graph.nodes.iter().filter(|n| n.kind.is_unit()).count();

        // Feature page hrefs from the site index manifest compose wrote.
        let site_dir = PathBuf::from(&m.site.out);
        let site_index = site_dir.join("index.html");
        let page_of: HashMap<String, String> = deps
            .pages
            .read_manifest(&site_index)
            .and_then(|v| v.get("communities").cloned())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|c| {
                let id = c.get("id")?.as_str()?.to_string();
                let page = c.get("page")?.as_str()?;
                Some((id, rel_href(out_dir, &site_dir.join(page))))
            })
            .collect();

        // Drawn in the graph: the largest features, every cross-link endpoint
        // and every feature holding an entry point.
        let drawn: HashSet<Uuid> = communities
            .iter()
            .enumerate()
            .filter(|(i, c)| {
                *i < GRAPH_PER_REPO
                    || cross_ends.contains(&c.id)
                    || entries.features.contains(&c.id)
            })
            .map(|(_, c)| c.id)
            .collect();
        for c in communities {
            let mut by_lang: BTreeMap<&str, usize> = BTreeMap::new();
            for p in &c.files {
                if let Some(l) = lang_of.get(p.as_str()) {
                    *by_lang.entry(l.as_str()).or_default() += 1;
                }
            }
            let lang = by_lang
                .iter()
                .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
                .map(|(l, _)| *l)
                .unwrap_or("");
            features_json.push(json!({
                "id": c.id,
                "repo": m.alias,
                "label": c.label,
                "size": c.member_count,
                "files": c.files.len(),
                "lang": lang,
                "summary": snippet(c.summary.as_deref()),
                "top": c.top_members.iter().take(6).collect::<Vec<_>>(),
                "page": page_of.get(&c.id.to_string()),
                "drawn": drawn.contains(&c.id),
            }));
        }

        // Feature → feature weight: graph edges whose endpoints sit in two
        // different drawn features of this repo.
        let community_of: HashMap<Uuid, Uuid> =
            members_rows.iter().map(|(c, n)| (*n, *c)).collect();
        let mut pairs: BTreeMap<(Uuid, Uuid), BTreeMap<&'static str, usize>> = BTreeMap::new();
        for e in graph.edges.iter().filter(|e| is_dependency(e.kind)) {
            let (Some(&a), Some(&b)) = (
                community_of.get(&e.source_node_id),
                community_of.get(&e.target_node_id),
            ) else {
                continue;
            };
            if a == b || !drawn.contains(&a) || !drawn.contains(&b) {
                continue;
            }
            *pairs
                .entry((a, b))
                .or_default()
                .entry(e.kind.as_str())
                .or_default() += 1;
        }
        for ((a, b), kinds) in pairs {
            let weight: usize = kinds.values().sum();
            edges_json
                .push(json!({"a": a, "b": b, "repo": m.alias, "weight": weight, "kinds": kinds}));
        }

        let nodes = graph.nodes.len();
        let edges = graph.edges.len();
        for (k, v) in [
            ("files", graph.files.len() as i64),
            ("symbols", symbols as i64),
            ("edges", edges as i64),
            ("chunks", chunks),
            ("features", communities.len() as i64),
        ] {
            *totals.entry(k).or_default() += v;
        }
        repos_json.push(json!({
            "alias": m.alias,
            "path": m.root.display().to_string(),
            "commit": m.repo.commit_sha,
            "files": graph.files.len(),
            "symbols": symbols,
            "nodes": nodes,
            "edges": edges,
            "chunks": chunks,
            "features": communities.len(),
            "languages": langs.iter().map(|(l, n)| json!([l, n])).collect::<Vec<_>>(),
            "site": rel_href(out_dir, &site_index),
        }));
        sites.push(RepoSite {
            alias: m.alias.clone(),
            site: site_index.display().to_string(),
            pages_written: m.site.pages_written,
            pages_cached: m.site.pages_cached,
            features: communities.len(),
        });
        provenance.push(
            Breadcrumb::new(
                source::POSTGRES,
                "load",
                format!(
                    "{}: {} files, {nodes} nodes, {edges} edges, {chunks} chunks, {} features",
                    m.alias,
                    graph.files.len(),
                    communities.len()
                ),
            )
            .with_locator(m.root.display().to_string()),
        );
    }

    let cross_json: Vec<Value> = correlations
        .cross
        .iter()
        .map(|l| {
            json!({
                "from": l.consumer_feature.id, "from_repo": l.consumer_repo, "from_label": l.consumer_feature.label,
                "to": l.provider_feature.id, "to_repo": l.provider_repo, "to_label": l.provider_feature.label,
                "kind": l.kind, "evidence": l.evidence, "confidence": l.confidence,
            })
        })
        .collect();
    let mut questions_json: Vec<Value> = questions
        .iter()
        .map(|q| json!({"title": q.title, "href": rel_href(out_dir, &q.path), "repos": q.repos}))
        .collect();
    questions_json.sort_by_key(|q| q["title"].as_str().unwrap_or("").to_lowercase());
    provenance.push(Breadcrumb::new(
        source::GRAPH,
        "quotient",
        format!(
            "{} feature-to-feature edges (graph edges crossing features, containment excluded) among the {} largest features per repo plus every cross-link endpoint and entry-point feature; {} cross-repo links and {} intra-repo pairs from correlations",
            edges_json.len(),
            GRAPH_PER_REPO,
            cross_json.len(),
            correlations.intra.len()
        ),
    ));

    let graph_nodes = features_json
        .iter()
        .filter(|f| f["drawn"] == json!(true))
        .count();
    let correlations_page = PathBuf::from(&correlations.page.path);
    let mut manifest = json!({
        "title": format!("{project} project map"),
        "project": project,
        "summary": format!(
            "{} repositories indexed: {} files, {} symbols, {} graph edges, {} embedded chunks, grouped into {} features. {} cross-repo links.",
            members.len(), totals["files"], totals["symbols"], totals["edges"], totals["chunks"], totals["features"], cross_json.len()
        ),
        "totals": totals,
        "repos": repos_json,
        "features": features_json,
        "edges": edges_json,
        "cross": cross_json,
        "intra_pairs": correlations.intra.len(),
        "entry_points": entries.to_json(),
        "questions": questions_json,
        "correlations": rel_href(out_dir, &correlations_page),
        "provenance": provenance,
    });
    let content_hash = page_hash(&manifest);
    manifest["content_hash"] = json!(content_hash);
    let path = out_dir.join("index.html");
    let page_written = deps
        .pages
        .write_page(&path, PageKind::ProjectIndex, &manifest)
        .with_context(|| format!("write {}", path.display()))?;
    provenance.push(
        Breadcrumb::new(
            source::PAGE,
            "write",
            if page_written {
                "project hub written"
            } else {
                "project hub unchanged (hash-gated)"
            },
        )
        .with_locator(path.display().to_string()),
    );
    tracing::info!(page = %path.display(), written = page_written, "compose: project hub");
    Ok(ProjectComposeReport {
        page: path.display().to_string(),
        page_written,
        content_hash,
        repos: sites,
        features: features_json.len(),
        graph_nodes,
        cross_links: cross_json.len(),
        entry_points: entries.counts(),
        correlations_page: correlations.page.path.clone(),
        provenance,
    })
}

/// Whether `manifest` is a project feature page (one question across repos):
/// returns its title and member aliases.
pub fn question_page(manifest: &Value) -> Option<(String, Vec<String>)> {
    // `question` + `links` mark a project feature page; the hub itself also
    // carries `repos`, so `repos` alone is not enough.
    manifest.get("links")?.as_array()?;
    let repos = manifest.get("repos")?.as_array()?;
    let title = manifest.get("question")?.as_str()?.to_string();
    let aliases = repos
        .iter()
        .filter_map(|r| r.get("alias").and_then(Value::as_str).map(String::from))
        .collect();
    Some((title, aliases))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_href_walks_up_then_down() {
        let hub = Path::new("/p/docs/features_memory");
        assert_eq!(
            rel_href(
                hub,
                Path::new("/p/api/docs/features_memory/site/index.html")
            ),
            "../../api/docs/features_memory/site/index.html"
        );
        assert_eq!(
            rel_href(hub, Path::new("/p/docs/features_memory/correlations.html")),
            "correlations.html"
        );
        assert_eq!(rel_href(hub, hub), ".");
    }

    #[test]
    fn question_page_reads_project_feature_manifests_only() {
        let q = json!({"question": "how is a lab created", "links": [], "repos": [{"alias": "api"}, {"alias": "web"}]});
        assert_eq!(
            question_page(&q),
            Some((
                "how is a lab created".into(),
                vec!["api".into(), "web".into()]
            ))
        );
        assert_eq!(
            question_page(&json!({"title": "site", "features": []})),
            None
        );
    }

    #[test]
    fn containment_is_not_a_dependency() {
        assert!(!is_dependency(EdgeKind::Contains));
        assert!(is_dependency(EdgeKind::Calls));
        assert!(is_dependency(EdgeKind::Queries));
    }
}
