//! `compose` use-case: write the feature website of one repository — an index
//! page listing every L1 community plus one drill-down page per community
//! (members, files, relations to sibling communities, user surfaces, summary).
//!
//! Everything is resolved from the persisted index (communities + graph);
//! no source file is read. Every page carries a `content_hash` and is written
//! through the [`PageStore`] gate, so an unchanged repo re-composes to zero
//! writes.

/// (from community, to community, edge kind, direction) → (count, example symbol pairs).
type QuotientMap = BTreeMap<(Uuid, Uuid, &'static str, &'static str), (usize, Vec<String>)>;

use crate::domain::hash::sha256_hex;
use crate::domain::model::{Node, NodeKind, Repo};
use crate::domain::provenance::{source, Breadcrumb};
use crate::ports::community_store::CommunityStore;
use crate::ports::index_store::IndexStore;
use crate::ports::pages::{PageKind, PageStore};
use crate::ports::search_store::SearchStore;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Maximum member symbols listed on one feature page.
const MAX_MEMBERS: usize = 60;
/// Example symbol pairs kept per (community, direction, edge kind) relation.
const MAX_EXAMPLES: usize = 5;
/// Summary snippet length on the index page.
const SNIPPET_CHARS: usize = 240;

pub struct Deps<'a> {
    pub store: &'a dyn IndexStore,
    pub communities: &'a dyn CommunityStore,
    pub search: &'a dyn SearchStore,
    pub pages: &'a dyn PageStore,
}

/// Compact return of `compose` (goes straight into an agent context).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ComposeReport {
    pub out: String,
    pub pages_written: usize,
    pub pages_cached: usize,
    /// Communities listed on the index page.
    pub communities: usize,
    /// Communities listed without a drill-down page (`limit` cut them off).
    pub omitted: usize,
    pub provenance: Vec<Breadcrumb>,
}

/// One aggregated quotient edge between two communities.
#[derive(Debug, Clone, Serialize)]
struct Relation {
    other: Uuid,
    label: String,
    direction: &'static str,
    kind: &'static str,
    weight: usize,
    examples: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<String>,
}

/// Lower-case `[a-z0-9-]` slug, ≤ 80 chars, never empty.
pub fn slug(input: &str) -> String {
    let mut out = String::new();
    let mut dash = true;
    for c in input.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
        if out.len() >= 80 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "feature".to_string()
    } else {
        out
    }
}

fn unique_slug(base: &str, taken: &mut HashSet<String>) -> String {
    let mut candidate = base.to_string();
    let mut n = 1;
    while !taken.insert(candidate.clone()) {
        n += 1;
        candidate = format!("{base}-{n}");
    }
    candidate
}

fn snippet(summary: Option<&str>) -> String {
    let text = summary
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.chars().count() <= SNIPPET_CHARS {
        return text;
    }
    let cut: String = text.chars().take(SNIPPET_CHARS).collect();
    format!("{}…", cut.trim_end())
}

fn node_json(node: &Node) -> Value {
    json!({
        "name": node.name,
        "kind": node.kind.as_str(),
        "stable_id": node.stable_id,
        "path": crate::domain::community::node_path(&node.stable_id),
        "line_start": node.line_start,
        "line_end": node.line_end,
    })
}

pub async fn compose(
    deps: Deps<'_>,
    repo: &Repo,
    root: &Path,
    out: Option<PathBuf>,
    limit: Option<usize>,
) -> Result<ComposeReport> {
    let out_dir = out.unwrap_or_else(|| root.join("docs").join("features_memory").join("site"));
    let mut provenance = Vec::new();

    let mut communities = deps
        .communities
        .load_communities(repo.id)
        .await
        .context("load communities")?;
    let members = deps
        .communities
        .load_members(repo.id)
        .await
        .context("load community members")?;
    let graph = deps.store.load_graph(repo.id).await.context("load graph")?;
    provenance.push(
        Breadcrumb::new(
            source::POSTGRES,
            "load",
            format!(
                "{} communities, {} member rows, {} nodes, {} edges",
                communities.len(),
                members.len(),
                graph.nodes.len(),
                graph.edges.len()
            ),
        )
        .with_locator("communities, community_members, nodes, edges"),
    );

    // Largest communities first; stable tie-break by id.
    communities.sort_by(|a, b| {
        b.member_count
            .cmp(&a.member_count)
            .then_with(|| a.id.cmp(&b.id))
    });
    let total = communities.len();
    let listed = limit.map(|l| l.min(total)).unwrap_or(total);
    let omitted = total - listed;
    if omitted > 0 {
        tracing::info!(
            omitted,
            listed,
            "compose: limit leaves communities without a drill-down page"
        );
    }

    // Slugs: assigned in size order so the largest communities keep the shortest names.
    let mut taken = HashSet::new();
    let slugs: HashMap<Uuid, String> = communities
        .iter()
        .map(|c| (c.id, unique_slug(&slug(&c.label), &mut taken)))
        .collect();
    let page_of = |id: &Uuid, i: usize| -> Option<String> {
        (i < listed).then(|| format!("features/{}.html", slugs[id]))
    };
    let index_of: HashMap<Uuid, usize> = communities
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id, i))
        .collect();
    let label_of: HashMap<Uuid, &str> = communities
        .iter()
        .map(|c| (c.id, c.label.as_str()))
        .collect();

    // Membership + quotient edges.
    let nodes: HashMap<Uuid, &Node> = graph.nodes.iter().map(|n| (n.id, n)).collect();
    let community_of: HashMap<Uuid, Uuid> = members.iter().map(|(c, n)| (*n, *c)).collect();
    let mut members_by_community: HashMap<Uuid, Vec<&Node>> = HashMap::new();
    for (c, n) in &members {
        if let Some(node) = nodes.get(n) {
            members_by_community.entry(*c).or_default().push(node);
        }
    }
    // (community, other, direction, kind) -> (count, examples)
    let mut quotient: QuotientMap = BTreeMap::new();
    let mut cross_edges = 0usize;
    let mut sorted_edges: Vec<_> = graph.edges.iter().collect();
    sorted_edges.sort_by(|a, b| {
        (a.kind.as_str(), a.source_node_id, a.target_node_id).cmp(&(
            b.kind.as_str(),
            b.source_node_id,
            b.target_node_id,
        ))
    });
    for e in sorted_edges {
        let (Some(&a), Some(&b)) = (
            community_of.get(&e.source_node_id),
            community_of.get(&e.target_node_id),
        ) else {
            continue;
        };
        if a == b {
            continue;
        }
        cross_edges += 1;
        let example = match (nodes.get(&e.source_node_id), nodes.get(&e.target_node_id)) {
            (Some(s), Some(t)) => format!("{} → {}", s.name, t.name),
            _ => continue,
        };
        for (me, other, dir) in [(a, b, "out"), (b, a, "in")] {
            let entry = quotient
                .entry((me, other, dir, e.kind.as_str()))
                .or_insert_with(|| (0, Vec::new()));
            entry.0 += 1;
            if entry.1.len() < MAX_EXAMPLES {
                entry.1.push(example.clone());
            }
        }
    }
    provenance.push(Breadcrumb::new(source::GRAPH, "quotient", format!("{cross_edges} edges cross community boundaries; aggregated per (feature, direction, kind)")));

    let surface_nodes = deps
        .search
        .surfaces(repo.id, &[NodeKind::EnvVar, NodeKind::HttpRoute], None)
        .await
        .context("load surfaces")?;
    let surfaces_by_file: HashMap<Uuid, Vec<&Node>> = surface_nodes
        .iter()
        .filter_map(|n| n.file_id.map(|f| (f, n)))
        .fold(HashMap::new(), |mut m, (f, n)| {
            m.entry(f).or_default().push(n);
            m
        });
    let file_ids: HashMap<&str, Uuid> = graph
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.id))
        .collect();

    let mut written = 0usize;
    let mut cached = 0usize;
    let mut index_entries = Vec::with_capacity(total);
    let mut hash_lines = Vec::with_capacity(total);

    for (i, c) in communities.iter().enumerate() {
        let page = page_of(&c.id, i);
        let summary_hash = c.summary_hash.clone().unwrap_or_default();
        hash_lines.push(format!("{}:{}:{}", c.id, c.subtree_hash, summary_hash));
        index_entries.push(json!({
            "id": c.id,
            "label": c.label,
            "slug": slugs[&c.id],
            "member_count": c.member_count,
            "files_count": c.files.len(),
            "summary_snippet": snippet(c.summary.as_deref()),
            "page": page,
        }));
        if page.is_none() {
            continue;
        }

        // Members: top_members first (weighted degree order), then the rest by stable_id.
        let mut member_nodes: Vec<&Node> = members_by_community
            .get(&c.id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|n| !matches!(n.kind, NodeKind::File | NodeKind::Repository))
            .collect();
        member_nodes.sort_by(|a, b| a.stable_id.cmp(&b.stable_id));
        member_nodes.dedup_by(|a, b| a.id == b.id);
        let rank: HashMap<&str, usize> = c
            .top_members
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), i))
            .collect();
        member_nodes.sort_by_key(|n| {
            (
                rank.get(n.name.as_str()).copied().unwrap_or(usize::MAX),
                n.stable_id.clone(),
            )
        });
        let members_omitted = member_nodes.len().saturating_sub(MAX_MEMBERS);
        let member_json: Vec<Value> = member_nodes
            .iter()
            .take(MAX_MEMBERS)
            .map(|n| node_json(n))
            .collect();

        let mut relations: Vec<Relation> = quotient
            .iter()
            .filter(|((me, _, _, _), _)| *me == c.id)
            .map(|((_, other, dir, kind), (count, examples))| Relation {
                other: *other,
                label: label_of
                    .get(other)
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                direction: dir,
                kind,
                weight: *count,
                examples: examples.clone(),
                page: index_of
                    .get(other)
                    .and_then(|&j| page_of(other, j).map(|p| format!("../{p}"))),
            })
            .collect();
        relations.sort_by(|a, b| {
            b.weight
                .cmp(&a.weight)
                .then_with(|| (a.other, a.direction, a.kind).cmp(&(b.other, b.direction, b.kind)))
        });
        let relations_hash = sha256_hex(
            &relations
                .iter()
                .map(|r| {
                    format!(
                        "{}:{}:{}:{}:{}",
                        r.other,
                        r.direction,
                        r.kind,
                        r.weight,
                        r.page.as_deref().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );

        let mut surfaces: Vec<Value> = c
            .files
            .iter()
            .filter_map(|p| file_ids.get(p.as_str()).and_then(|id| surfaces_by_file.get(id)).map(|ns| (p, ns)))
            .flat_map(|(p, ns)| ns.iter().map(move |n| json!({"kind": n.kind.as_str(), "name": n.name, "path": p, "line": n.line_start})))
            .collect();
        surfaces.sort_by_key(|s| {
            (
                s["kind"].as_str().unwrap_or("").to_string(),
                s["name"].as_str().unwrap_or("").to_string(),
                s["path"].as_str().unwrap_or("").to_string(),
            )
        });

        let content_hash = sha256_hex(&format!(
            "{}\n{}\n{}",
            c.subtree_hash, summary_hash, relations_hash
        ));
        let manifest = json!({
            "title": c.label,
            "label": c.label,
            "repo": repo.name,
            "index": "../index.html",
            "community": {"id": c.id, "label": c.label, "slug": slugs[&c.id], "member_count": c.member_count, "subtree_hash": c.subtree_hash},
            "summary": c.summary,
            "members": member_json,
            "members_omitted": members_omitted,
            "files": c.files,
            "relations": relations,
            "surfaces": surfaces,
            "content_hash": content_hash,
            "provenance": provenance,
        });
        let path = out_dir
            .join("features")
            .join(format!("{}.html", slugs[&c.id]));
        if deps
            .pages
            .write_page(&path, PageKind::SitePage, &manifest)
            .with_context(|| format!("write {}", path.display()))?
        {
            written += 1;
        } else {
            cached += 1;
        }
    }

    hash_lines.sort();
    let index_hash = sha256_hex(&format!("{}\n{}", hash_lines.join("\n"), listed));
    let index_manifest = json!({
        "title": format!("{} — features", repo.name),
        "repo": repo.name,
        "generated_for_commit": repo.commit_sha,
        "summary": format!("{total} feature communities ({listed} with drill-down pages)"),
        "communities": index_entries,
        "features": communities.iter().enumerate().map(|(i, c)| json!({
            "label": c.label,
            "page": page_of(&c.id, i),
            "member_count": c.member_count,
            "file_count": c.files.len(),
            "summary": snippet(c.summary.as_deref()),
        })).collect::<Vec<_>>(),
        "omitted": omitted,
        "content_hash": index_hash,
        "provenance": provenance,
    });
    let index_path = out_dir.join("index.html");
    if deps
        .pages
        .write_page(&index_path, PageKind::SiteIndex, &index_manifest)
        .context("write index.html")?
    {
        written += 1;
    } else {
        cached += 1;
    }
    provenance.push(
        Breadcrumb::new(
            source::PAGE,
            "write",
            format!("{written} pages written, {cached} cached (content_hash gate)"),
        )
        .with_locator(out_dir.display().to_string()),
    );
    tracing::info!(written, cached, total, omitted, out = %out_dir.display(), "compose: site done");

    Ok(ComposeReport {
        out: out_dir.display().to_string(),
        pages_written: written,
        pages_cached: cached,
        communities: total,
        omitted,
        provenance,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        Chunk, Community, Edge, EdgeKind, IndexBundle, ModelRef, SourceFile,
    };
    use crate::ports::index_store::Graph;
    use crate::ports::search_store::Hit;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct Fake {
        graph: Graph,
        communities: Vec<Community>,
        members: Vec<(Uuid, Uuid)>,
        pages: Mutex<HashMap<PathBuf, Value>>,
    }

    #[async_trait]
    impl IndexStore for Fake {
        async fn upsert_repo(&self, _: &Repo) -> Result<()> {
            unimplemented!()
        }
        async fn find_repo(&self, _: &str) -> Result<Option<Repo>> {
            unimplemented!()
        }
        async fn list_repos(&self) -> Result<Vec<Repo>> {
            unimplemented!()
        }
        async fn set_repo_hashes(&self, _: Uuid, _: Option<&str>, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn replace_repo(&self, _: Uuid, _: &IndexBundle) -> Result<()> {
            unimplemented!()
        }
        async fn merge_files(&self, _: Uuid, _: &[String], _: &IndexBundle) -> Result<()> {
            unimplemented!()
        }
        async fn load_graph(&self, _: Uuid) -> Result<Graph> {
            Ok(self.graph.clone())
        }
        async fn load_chunk_hashes(&self, _: Uuid) -> Result<Vec<(String, String)>> {
            unimplemented!()
        }
        async fn set_file_subtree_hashes(&self, _: Uuid, _: &[(String, String)]) -> Result<()> {
            unimplemented!()
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
    impl CommunityStore for Fake {
        async fn replace_communities(
            &self,
            _: Uuid,
            _: &[Community],
            _: &[(Uuid, Uuid)],
        ) -> Result<()> {
            unimplemented!()
        }
        async fn load_communities(&self, _: Uuid) -> Result<Vec<Community>> {
            Ok(self.communities.clone())
        }
        async fn load_members(&self, _: Uuid) -> Result<Vec<(Uuid, Uuid)>> {
            Ok(self.members.clone())
        }
        async fn communities_needing_summary(
            &self,
            _: Uuid,
            _: &ModelRef,
        ) -> Result<Vec<Community>> {
            unimplemented!()
        }
        async fn save_summary(
            &self,
            _: Uuid,
            _: &str,
            _: &str,
            _: &ModelRef,
            _: &[f32],
        ) -> Result<()> {
            unimplemented!()
        }
    }

    #[async_trait]
    impl SearchStore for Fake {
        async fn semantic(
            &self,
            _: Uuid,
            _: &ModelRef,
            _: &[f32],
            _: Option<&[Uuid]>,
            _: i64,
        ) -> Result<Vec<Hit>> {
            unimplemented!()
        }
        async fn keyword(&self, _: Uuid, _: &str, _: Option<&[Uuid]>, _: i64) -> Result<Vec<Hit>> {
            unimplemented!()
        }
        async fn literal(&self, _: Uuid, _: &str, _: Option<&[Uuid]>, _: i64) -> Result<Vec<Hit>> {
            unimplemented!()
        }
        async fn communities_by_embedding(
            &self,
            _: Uuid,
            _: &ModelRef,
            _: &[f32],
            _: i64,
        ) -> Result<Vec<(Community, f32)>> {
            unimplemented!()
        }
        async fn community_file_ids(&self, _: &[Uuid]) -> Result<Vec<Uuid>> {
            unimplemented!()
        }
        async fn edges_for_nodes(&self, _: Uuid, _: &[Uuid]) -> Result<Vec<Edge>> {
            unimplemented!()
        }
        async fn nodes_by_ids(&self, _: &[Uuid]) -> Result<Vec<Node>> {
            unimplemented!()
        }
        async fn chunks_for_nodes(&self, _: &[Uuid]) -> Result<Vec<Chunk>> {
            unimplemented!()
        }
        async fn surfaces(
            &self,
            _: Uuid,
            kinds: &[NodeKind],
            _: Option<&[Uuid]>,
        ) -> Result<Vec<Node>> {
            Ok(self
                .graph
                .nodes
                .iter()
                .filter(|n| kinds.contains(&n.kind))
                .cloned()
                .collect())
        }
        async fn file_paths(&self, _: &[Uuid]) -> Result<Vec<(Uuid, String)>> {
            unimplemented!()
        }
    }

    impl PageStore for Fake {
        fn write_page(&self, path: &Path, _: PageKind, manifest: &Value) -> Result<bool> {
            let mut pages = self.pages.lock().unwrap();
            if pages
                .get(path)
                .map(|m| m["content_hash"] == manifest["content_hash"])
                .unwrap_or(false)
            {
                return Ok(false);
            }
            pages.insert(path.to_path_buf(), manifest.clone());
            Ok(true)
        }
        fn read_manifest(&self, path: &Path) -> Option<Value> {
            self.pages.lock().unwrap().get(path).cloned()
        }
    }

    fn id(s: &str) -> Uuid {
        crate::domain::hash::scoped_id(Uuid::nil(), s)
    }

    fn node(stable: &str, name: &str, kind: NodeKind, file: &str) -> Node {
        Node {
            id: id(stable),
            repo_id: Uuid::nil(),
            file_id: Some(id(&format!("file:{file}"))),
            kind,
            stable_id: stable.into(),
            name: name.into(),
            signature: None,
            doc: None,
            line_start: Some(1),
            line_end: Some(9),
            metadata: json!({}),
        }
    }

    fn community(label: &str, members: &[&Node], files: &[&str]) -> Community {
        Community {
            id: id(&format!("community:{label}:{}", files.join(","))),
            repo_id: Uuid::nil(),
            label: label.into(),
            member_count: members.len() as i32,
            subtree_hash: sha256_hex(label),
            summary: Some(format!("Feature: {label}")),
            summary_hash: Some(sha256_hex(&format!("s:{label}"))),
            top_members: members.iter().map(|n| n.name.clone()).collect(),
            files: files.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Three communities: "Auth" (3 members), "Auth" (duplicate label, 2), "Billing" (1).
    fn fixture() -> Fake {
        let a1 = node(
            "rust:src/auth.rs:function:login",
            "login",
            NodeKind::Function,
            "src/auth.rs",
        );
        let a2 = node(
            "rust:src/auth.rs:function:logout",
            "logout",
            NodeKind::Function,
            "src/auth.rs",
        );
        let a3 = node(
            "rust:src/auth.rs:env_var:TOKEN",
            "TOKEN",
            NodeKind::EnvVar,
            "src/auth.rs",
        );
        let b1 = node(
            "rust:src/session.rs:function:open",
            "open",
            NodeKind::Function,
            "src/session.rs",
        );
        let b2 = node(
            "rust:src/session.rs:function:close",
            "close",
            NodeKind::Function,
            "src/session.rs",
        );
        let c1 = node(
            "rust:src/bill.rs:function:charge",
            "charge",
            NodeKind::Function,
            "src/bill.rs",
        );
        let ca = community("Auth", &[&a1, &a2, &a3], &["src/auth.rs"]);
        let cb = community("Auth", &[&b1, &b2], &["src/session.rs"]);
        let cc = community("Billing", &[&c1], &["src/bill.rs"]);
        let members = vec![
            (ca.id, a1.id),
            (ca.id, a2.id),
            (ca.id, a3.id),
            (cb.id, b1.id),
            (cb.id, b2.id),
            (cc.id, c1.id),
        ];
        let edge = |s: &Node, t: &Node| Edge {
            id: crate::domain::hash::edge_id(s.id, t.id, "calls"),
            repo_id: Uuid::nil(),
            source_node_id: s.id,
            target_node_id: t.id,
            kind: EdgeKind::Calls,
            confidence: 1.0,
        };
        let files = ["src/auth.rs", "src/session.rs", "src/bill.rs"]
            .iter()
            .map(|p| SourceFile {
                id: id(&format!("file:{p}")),
                repo_id: Uuid::nil(),
                path: p.to_string(),
                language: crate::domain::model::Language::Rust,
                content_hash: "h".into(),
                line_count: 10,
                subtree_hash: None,
            })
            .collect();
        Fake {
            graph: Graph {
                files,
                nodes: vec![a1.clone(), a2.clone(), a3, b1.clone(), b2, c1.clone()],
                edges: vec![edge(&a1, &b1), edge(&a2, &b1), edge(&c1, &a1)],
            },
            communities: vec![cc, cb, ca],
            members,
            pages: Mutex::new(HashMap::new()),
        }
    }

    fn repo() -> Repo {
        Repo {
            id: Uuid::nil(),
            name: "demo".into(),
            root_path: "/r".into(),
            commit_sha: Some("abc".into()),
            root_hash: None,
        }
    }

    fn deps(f: &Fake) -> Deps<'_> {
        Deps {
            store: f,
            communities: f,
            search: f,
            pages: f,
        }
    }

    #[tokio::test]
    async fn writes_site_then_hash_gates_second_run() {
        let f = fixture();
        let out = PathBuf::from("/site");
        let r = compose(deps(&f), &repo(), Path::new("/r"), Some(out.clone()), None)
            .await
            .unwrap();
        assert_eq!(
            (r.pages_written, r.pages_cached, r.communities, r.omitted),
            (4, 0, 3, 0)
        );
        assert_eq!(r.out, "/site");
        let index = f.read_manifest(&out.join("index.html")).unwrap();
        assert_eq!(index["generated_for_commit"], "abc");
        assert_eq!(index["communities"].as_array().unwrap().len(), 3);
        // Largest first, and the "Auth" duplicate gets "-2".
        let slugs: Vec<&str> = index["communities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["slug"].as_str().unwrap())
            .collect();
        assert_eq!(slugs, vec!["auth", "auth-2", "billing"]);
        assert_eq!(index["communities"][0]["page"], "features/auth.html");

        let auth = f.read_manifest(&out.join("features/auth.html")).unwrap();
        assert_eq!(auth["members"].as_array().unwrap().len(), 3);
        assert_eq!(auth["surfaces"][0]["name"], "TOKEN");
        let rels = auth["relations"].as_array().unwrap();
        // out → auth-2 (2 calls), in ← billing (1 call)
        assert_eq!(rels.len(), 2);
        assert_eq!(rels[0]["direction"], "out");
        assert_eq!(rels[0]["weight"], 2);
        assert_eq!(rels[0]["page"], "../features/auth-2.html");
        assert_eq!(rels[0]["examples"].as_array().unwrap().len(), 2);
        assert_eq!(rels[1]["direction"], "in");
        assert_eq!(rels[1]["label"], "Billing");

        let r2 = compose(deps(&f), &repo(), Path::new("/r"), Some(out), None)
            .await
            .unwrap();
        assert_eq!((r2.pages_written, r2.pages_cached), (0, 4));
    }

    #[tokio::test]
    async fn limit_keeps_largest_and_lists_the_rest_without_link() {
        let f = fixture();
        let out = PathBuf::from("/site");
        let r = compose(
            deps(&f),
            &repo(),
            Path::new("/r"),
            Some(out.clone()),
            Some(1),
        )
        .await
        .unwrap();
        assert_eq!((r.pages_written, r.communities, r.omitted), (2, 3, 2));
        let index = f.read_manifest(&out.join("index.html")).unwrap();
        assert_eq!(index["communities"][0]["page"], "features/auth.html");
        assert!(index["communities"][1]["page"].is_null());
        assert!(index["communities"][2]["page"].is_null());
        assert!(f.read_manifest(&out.join("features/auth-2.html")).is_none());
        // Relations to unlisted siblings carry no link.
        let auth = f.read_manifest(&out.join("features/auth.html")).unwrap();
        assert!(auth["relations"][0]["page"].is_null());
    }

    #[tokio::test]
    async fn default_out_dir_and_slug_rules() {
        let f = fixture();
        let r = compose(deps(&f), &repo(), Path::new("/r"), None, Some(0))
            .await
            .unwrap();
        assert_eq!(r.out, "/r/docs/features_memory/site");
        assert_eq!((r.pages_written, r.omitted), (1, 3));
        assert_eq!(slug("  Hello, World!! "), "hello-world");
        assert_eq!(slug("///"), "feature");
        let mut taken = HashSet::new();
        assert_eq!(unique_slug("a", &mut taken), "a");
        assert_eq!(unique_slug("a", &mut taken), "a-2");
        assert_eq!(unique_slug("a", &mut taken), "a-3");
    }
}
