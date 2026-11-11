//! Entry points: where someone from outside comes into what the indexed
//! repositories hold. Derived from the persisted index (node kinds and the
//! metadata the extractors record), never from source.
//!
//! | Category | Listed from |
//! |---|---|
//! | `graphql` | SDL `Field` nodes under Query/Mutation/Subscription, in the schema OWNER repo (the one defining the most root fields; tie: earlier repo), with every operation in any repo whose `metadata.root_fields` names the field |
//! | `contracts` | Solidity `Method`s with `visibility` public/external in contracts and interfaces (libraries, `*.s.sol`, `*.t.sol`, `script/`, `test/` excluded), with their events |
//! | `mcp_tools` | `McpTool` surface nodes outside tests |
//! | `http_routes` | `HttpRoute` surface nodes outside tests |
//! | `hooks` | functions with `metadata.hook` (a React `use*` function wrapping a data hook) outside tests, with the GraphQL root fields their calls reach within [`HOOK_HOPS`] hops |
//! | `documents` | in a repository whose files are mostly Markdown (hidden folders such as `.github/` left out): each document's title (its one H1, else its front-matter `title:`, else the file name) and chapters (the shallowest heading level under the title) |
//! | `timelines` | `Section`s with `metadata.timestamp` (transcript segments and timestamped headings), per file |
//!
//! `features` holds the features with something a client, an agent or anyone
//! on-chain can call (GraphQL, contracts, MCP tools, routes); the project hub
//! always draws those.

use crate::application::correlations::is_root_field;
use crate::domain::model::{EdgeKind, Language, Node, NodeKind};
use crate::ports::index_store::Graph;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use uuid::Uuid;

/// Call hops followed from a client hook to the GraphQL operations it runs.
pub const HOOK_HOPS: usize = 3;

/// One repository's persisted index, as entry points read it.
pub struct RepoIndex<'a> {
    pub alias: &'a str,
    pub graph: &'a Graph,
    /// node → feature (community).
    pub community_of: HashMap<Uuid, Uuid>,
}

impl<'a> RepoIndex<'a> {
    /// `members` are `(community, node)` rows.
    pub fn new(alias: &'a str, graph: &'a Graph, members: &[(Uuid, Uuid)]) -> Self {
        Self {
            alias,
            graph,
            community_of: members.iter().map(|(c, n)| (*n, *c)).collect(),
        }
    }
}

/// Entry points by category (see the module docs); every row names its repo.
#[derive(Debug, Default)]
pub struct EntryPoints {
    pub graphql: Vec<Value>,
    pub contracts: Vec<Value>,
    pub mcp_tools: Vec<Value>,
    pub http_routes: Vec<Value>,
    pub hooks: Vec<Value>,
    pub documents: Vec<Value>,
    pub timelines: Vec<Value>,
    /// Features holding a GraphQL field, contract function, MCP tool or route.
    pub features: HashSet<Uuid>,
}

impl EntryPoints {
    pub fn contract_functions(&self) -> usize {
        self.contracts
            .iter()
            .map(|c| c["functions"].as_array().map_or(0, Vec::len))
            .sum()
    }

    pub fn timeline_entries(&self) -> usize {
        self.timelines
            .iter()
            .map(|t| t["entries"].as_array().map_or(0, Vec::len))
            .sum()
    }

    /// Count per category, non-empty categories only.
    pub fn counts(&self) -> BTreeMap<String, usize> {
        [
            ("graphql_fields", self.graphql.len()),
            ("contract_functions", self.contract_functions()),
            ("mcp_tools", self.mcp_tools.len()),
            ("http_routes", self.http_routes.len()),
            ("client_hooks", self.hooks.len()),
            ("documents", self.documents.len()),
            ("timeline_entries", self.timeline_entries()),
        ]
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(k, n)| (k.to_string(), n))
        .collect()
    }

    /// The manifest block pages render.
    pub fn to_json(&self) -> Value {
        json!({
            "graphql": self.graphql, "contracts": self.contracts, "mcp_tools": self.mcp_tools,
            "http_routes": self.http_routes, "hooks": self.hooks, "documents": self.documents,
            "timelines": self.timelines,
        })
    }

    /// One provenance line: what was counted and from which index data.
    pub fn provenance(&self) -> String {
        format!(
            "{} GraphQL root fields (schema owner = repo defining the most root fields; callers = operations' metadata.root_fields); {} public/external functions in {} contracts and interfaces (Solidity metadata.visibility; *.s.sol, *.t.sol, script/, test/ and libraries excluded); {} MCP tools and {} HTTP routes (surface nodes outside tests); {} client hooks (metadata.hook, GraphQL fields within {HOOK_HOPS} call hops); {} documents (repos mostly Markdown); {} timeline entries (Section metadata.timestamp)",
            self.graphql.len(),
            self.contract_functions(),
            self.contracts.len(),
            self.mcp_tools.len(),
            self.http_routes.len(),
            self.hooks.len(),
            self.documents.len(),
            self.timeline_entries(),
        )
    }
}

fn meta_str<'a>(n: &'a Node, key: &str) -> &'a str {
    n.metadata[key].as_str().unwrap_or("")
}

/// Foundry layout: deploy scripts (`*.s.sol`, `script/`) and tests (`*.t.sol`,
/// `test/`) are never deployed, so they are not entry points.
pub fn is_script_or_test(path: &str) -> bool {
    path.ends_with(".s.sol")
        || path.ends_with(".t.sol")
        || path
            .split('/')
            .any(|seg| seg == "script" || seg == "test" || seg == "tests")
}

/// Test, fixture and mock files: what they register serves no real caller.
pub fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let file = lower.rsplit('/').next().unwrap_or("");
    lower.split('/').any(|s| {
        matches!(
            s,
            "test" | "tests" | "__tests__" | "__mocks__" | "e2e" | "spec" | "fixtures" | "mocks"
        )
    }) || file.contains(".test.")
        || file.contains(".spec.")
        || file.starts_with("test_")
        || file.ends_with("_test.py")
}

/// Lookups over one repo's graph.
struct Ctx<'a> {
    nodes: &'a [Node],
    by_id: HashMap<Uuid, &'a Node>,
    community_of: &'a HashMap<Uuid, Uuid>,
    /// file → the feature most of its nodes belong to.
    file_feature: HashMap<Uuid, Uuid>,
    /// surface node → the symbol that defines it.
    owner_of: HashMap<Uuid, Uuid>,
    calls: HashMap<Uuid, Vec<Uuid>>,
    defines: HashMap<Uuid, Vec<Uuid>>,
}

impl<'a> Ctx<'a> {
    fn new(r: &'a RepoIndex) -> Self {
        let nodes = r.graph.nodes.as_slice();
        let by_id: HashMap<Uuid, &Node> = nodes.iter().map(|n| (n.id, n)).collect();
        let mut per_file: HashMap<Uuid, BTreeMap<Uuid, usize>> = HashMap::new();
        for n in nodes {
            if let (Some(f), Some(c)) = (n.file_id, r.community_of.get(&n.id)) {
                *per_file.entry(f).or_default().entry(*c).or_default() += 1;
            }
        }
        let file_feature = per_file
            .into_iter()
            .filter_map(|(f, cs)| {
                cs.into_iter()
                    .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
                    .map(|(c, _)| (f, c))
            })
            .collect();
        let mut owner_of = HashMap::new();
        let mut calls: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        let mut defines: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        for e in &r.graph.edges {
            match e.kind {
                EdgeKind::Calls => calls
                    .entry(e.source_node_id)
                    .or_default()
                    .push(e.target_node_id),
                EdgeKind::Defines => {
                    defines
                        .entry(e.source_node_id)
                        .or_default()
                        .push(e.target_node_id);
                    if by_id
                        .get(&e.source_node_id)
                        .is_some_and(|s| s.kind != NodeKind::File)
                    {
                        owner_of.insert(e.target_node_id, e.source_node_id);
                    }
                }
                _ => {}
            }
        }
        Self {
            nodes,
            by_id,
            community_of: &r.community_of,
            file_feature,
            owner_of,
            calls,
            defines,
        }
    }

    /// The node's feature; a surface takes its owner's, else its file's.
    fn feature(&self, n: &Node) -> Option<Uuid> {
        self.community_of
            .get(&n.id)
            .or_else(|| {
                self.owner_of
                    .get(&n.id)
                    .and_then(|o| self.community_of.get(o))
            })
            .or_else(|| n.file_id.and_then(|f| self.file_feature.get(&f)))
            .copied()
    }

    fn owner_name(&self, n: &Node) -> Option<&str> {
        self.owner_of
            .get(&n.id)
            .and_then(|o| self.by_id.get(o))
            .map(|o| o.name.as_str())
    }

    /// GraphQL root fields the operations reachable from `start` select:
    /// operations it defines, then those of what it calls, up to [`HOOK_HOPS`].
    fn reaches(&self, start: Uuid) -> Vec<String> {
        let mut seen = HashSet::from([start]);
        let mut frontier = vec![start];
        let mut fields = BTreeSet::new();
        for depth in 0..=HOOK_HOPS {
            let mut next = Vec::new();
            for id in &frontier {
                for t in self.defines.get(id).into_iter().flatten() {
                    if let Some(op) = self.by_id.get(t).filter(|n| n.kind == NodeKind::Operation) {
                        for f in op.metadata["root_fields"].as_array().into_iter().flatten() {
                            if let Some(f) = f.as_str() {
                                fields.insert(f.to_string());
                            }
                        }
                    }
                }
                if depth < HOOK_HOPS {
                    for t in self.calls.get(id).into_iter().flatten() {
                        if seen.insert(*t) {
                            next.push(*t);
                        }
                    }
                }
            }
            frontier = next;
        }
        fields.into_iter().collect()
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn level(n: &Node) -> u64 {
    n.metadata["level"].as_u64().unwrap_or(1)
}

/// Entry points across `repos` (see the module docs).
pub fn entry_points(repos: &[RepoIndex]) -> EntryPoints {
    let mut ep = EntryPoints::default();
    let ctx: Vec<Ctx> = repos.iter().map(Ctx::new).collect();
    graphql(repos, &ctx, &mut ep);
    for (r, c) in repos.iter().zip(&ctx) {
        contracts(r.alias, c, &mut ep);
        surfaces(r.alias, c, &mut ep);
        hooks(r.alias, c, &mut ep);
        documents(r, c, &mut ep);
        timelines(r.alias, c, &mut ep);
    }
    ep
}

fn graphql(repos: &[RepoIndex], ctx: &[Ctx], ep: &mut EntryPoints) {
    let mut defs: BTreeMap<&str, Vec<(usize, &Node)>> = BTreeMap::new();
    let mut root_count = vec![0usize; repos.len()];
    let mut callers: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    for (ri, c) in ctx.iter().enumerate() {
        for n in c.nodes {
            match n.kind {
                NodeKind::Field
                    if meta_str(n, "language") == "graphql"
                        && is_root_field(meta_str(n, "qualified_name")) =>
                {
                    defs.entry(meta_str(n, "qualified_name"))
                        .or_default()
                        .push((ri, n));
                    root_count[ri] += 1;
                }
                NodeKind::Operation => {
                    for f in n.metadata["root_fields"].as_array().into_iter().flatten() {
                        if let Some(f) = f.as_str() {
                            callers.entry(f).or_default().push(json!({
                                "repo": repos[ri].alias, "op": n.name,
                                "path": meta_str(n, "path"), "line": n.line_start,
                            }));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    for (field, ds) in &defs {
        let Some(owner) = ds
            .iter()
            .map(|(ri, _)| *ri)
            .max_by(|a, b| root_count[*a].cmp(&root_count[*b]).then_with(|| b.cmp(a)))
        else {
            continue;
        };
        let own: Vec<&Node> = ds
            .iter()
            .filter(|(ri, _)| *ri == owner)
            .map(|(_, n)| *n)
            .collect();
        let mut files: Vec<&str> = own.iter().map(|n| meta_str(n, "path")).collect();
        files.sort();
        files.dedup();
        let feature = own.iter().find_map(|n| ctx[owner].feature(n));
        if let Some(f) = feature {
            ep.features.insert(f);
        }
        // The one-line header (all arguments, no descriptions); the longest
        // wins across files. Older indexes only have the first-line signature.
        let signature = own
            .iter()
            .map(|n| match meta_str(n, "header") {
                "" => n.signature.as_deref().unwrap_or(""),
                h => h,
            })
            .max_by_key(|s| s.len())
            .unwrap_or("");
        let (root, name) = field.split_once('.').unwrap_or(("", field));
        ep.graphql.push(json!({
            "field": field, "root": root, "name": name, "repo": repos[owner].alias,
            "signature": signature, "files": files, "feature": feature,
            "callers": callers.get(field).cloned().unwrap_or_default(),
        }));
    }
    let root_rank = |r: &str| match r {
        "Mutation" => 0,
        "Query" => 1,
        _ => 2,
    };
    ep.graphql.sort_by(|a, b| {
        let key = |v: &Value| {
            (
                root_rank(v["root"].as_str().unwrap_or("")),
                v["name"].as_str().unwrap_or("").to_lowercase(),
            )
        };
        key(a).cmp(&key(b))
    });
}

fn contracts(alias: &str, c: &Ctx, ep: &mut EntryPoints) {
    let sol = |n: &&Node| {
        meta_str(n, "language") == "solidity" && !is_script_or_test(meta_str(n, "path"))
    };
    // (path, container name) → (kind, line)
    let mut containers: BTreeMap<(&str, &str), (&str, Option<i32>)> = BTreeMap::new();
    for n in c.nodes.iter().filter(sol) {
        let kind = match n.kind {
            NodeKind::Interface => "interface",
            NodeKind::Type if n.metadata["library"] == json!(true) => "library",
            NodeKind::Type if !meta_str(n, "qualified_name").contains("::") => "contract",
            _ => continue,
        };
        containers.insert((meta_str(n, "path"), n.name.as_str()), (kind, n.line_start));
    }
    let mut functions: BTreeMap<(&str, &str), Vec<Value>> = BTreeMap::new();
    let mut events: BTreeMap<(&str, &str), Vec<&str>> = BTreeMap::new();
    for n in c.nodes.iter().filter(sol) {
        let q = meta_str(n, "qualified_name");
        let Some((container, _)) = q.split_once("::") else {
            continue;
        };
        let key = (meta_str(n, "path"), container);
        match containers.get(&key) {
            Some((kind, _)) if *kind != "library" => {}
            _ => continue,
        }
        if n.kind == NodeKind::Field && n.metadata["event"] == json!(true) {
            events.entry(key).or_default().push(n.name.as_str());
            continue;
        }
        let vis = meta_str(n, "visibility");
        if n.kind != NodeKind::Method || !(vis == "public" || vis == "external") {
            continue;
        }
        let feature = c.feature(n);
        if let Some(f) = feature {
            ep.features.insert(f);
        }
        functions.entry(key).or_default().push(json!({
            "name": n.name,
            "signature": match meta_str(n, "header") { "" => n.signature.as_deref().unwrap_or(""), h => h },
            "visibility": vis,
            "mutability": meta_str(n, "mutability"),
            "modifiers": n.metadata.get("modifiers").cloned().unwrap_or(json!([])),
            "line": n.line_start, "feature": feature,
        }));
    }
    let mut rows: Vec<Value> = functions
        .into_iter()
        .map(|((path, name), mut fns)| {
            fns.sort_by_key(|f| f["line"].as_i64().unwrap_or(0));
            let (kind, line) = containers[&(path, name)];
            json!({
                "repo": alias, "name": name, "kind": kind, "path": path, "line": line,
                "functions": fns, "events": events.get(&(path, name)).cloned().unwrap_or_default(),
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        let key = |v: &Value| {
            (
                v["kind"].as_str().unwrap_or("") != "contract",
                v["name"].as_str().unwrap_or("").to_lowercase(),
                v["path"].as_str().unwrap_or("").to_string(),
            )
        };
        key(a).cmp(&key(b))
    });
    ep.contracts.extend(rows);
}

/// MCP tools and HTTP routes.
fn surfaces(alias: &str, c: &Ctx, ep: &mut EntryPoints) {
    let mut tools = Vec::new();
    let mut routes = Vec::new();
    for n in c.nodes {
        if !matches!(n.kind, NodeKind::McpTool | NodeKind::HttpRoute)
            || is_test_path(meta_str(n, "path"))
        {
            continue;
        }
        let feature = c.feature(n);
        if let Some(f) = feature {
            ep.features.insert(f);
        }
        if n.kind == NodeKind::McpTool {
            tools.push(json!({
                "repo": alias, "name": n.name, "detail": n.metadata.get("detail"),
                "path": meta_str(n, "path"), "line": n.line_start,
                "owner": c.owner_name(n), "feature": feature,
            }));
        } else {
            let (method, route) = n.name.split_once(' ').unwrap_or(("ANY", n.name.as_str()));
            routes.push(json!({
                "repo": alias, "method": method, "route": route,
                "path": meta_str(n, "path"), "line": n.line_start,
                "owner": c.owner_name(n), "feature": feature,
            }));
        }
    }
    let by = |keys: [&'static str; 3]| {
        move |a: &Value, b: &Value| {
            let k = |v: &Value| {
                keys.map(|key| match &v[key] {
                    Value::Number(n) => format!("{:012}", n.as_i64().unwrap_or(0)),
                    other => other.as_str().unwrap_or("").to_lowercase(),
                })
            };
            k(a).cmp(&k(b))
        }
    };
    tools.sort_by(by(["path", "line", "name"]));
    routes.sort_by(by(["route", "method", "path"]));
    ep.mcp_tools.extend(tools);
    ep.http_routes.extend(routes);
}

fn hooks(alias: &str, c: &Ctx, ep: &mut EntryPoints) {
    let mut rows: Vec<Value> = c
        .nodes
        .iter()
        .filter(|n| {
            matches!(n.kind, NodeKind::Function | NodeKind::Method)
                && n.metadata["hook"].is_object()
                && !is_test_path(meta_str(n, "path"))
        })
        .map(|n| {
            json!({
                "repo": alias, "name": n.name,
                "kinds": n.metadata["hook"]["kinds"], "wraps": n.metadata["hook"]["wraps"],
                "path": meta_str(n, "path"), "line": n.line_start,
                "feature": c.feature(n), "reaches": c.reaches(n.id),
            })
        })
        .collect();
    rows.sort_by_key(|v| {
        (
            v["name"].as_str().unwrap_or("").to_lowercase(),
            v["path"].as_str().unwrap_or("").to_string(),
        )
    });
    ep.hooks.extend(rows);
}

/// The plan of a text repository: per Markdown file, its title and chapters.
fn documents(r: &RepoIndex, c: &Ctx, ep: &mut EntryPoints) {
    let md = r
        .graph
        .files
        .iter()
        .filter(|f| f.language == Language::Markdown)
        .count();
    if md == 0 || md * 2 < r.graph.files.len() {
        return;
    }
    let hidden = |p: &str| p.split('/').any(|s| s.starts_with('.'));
    let mut by_path: BTreeMap<&str, Vec<&Node>> = BTreeMap::new();
    let mut front: HashMap<&str, (&str, &Node)> = HashMap::new();
    for n in c.nodes {
        if n.kind == NodeKind::File && !meta_str(n, "title").is_empty() && !hidden(&n.name) {
            front.insert(n.name.as_str(), (meta_str(n, "title"), n));
            // A titled page with no headings is still a document.
            by_path.entry(n.name.as_str()).or_default();
        }
        if n.kind == NodeKind::Section
            && meta_str(n, "language") == "markdown"
            && n.metadata["transcript"] != json!(true)
            && !hidden(meta_str(n, "path"))
        {
            by_path.entry(meta_str(n, "path")).or_default().push(n);
        }
    }
    for (path, mut secs) in by_path {
        secs.sort_by_key(|n| n.line_start);
        let h1: Vec<&&Node> = secs.iter().filter(|n| level(n) == 1).collect();
        let (title, title_node, title_level) = match h1.as_slice() {
            [only] => (only.name.as_str(), Some(**only), 1),
            _ => (
                front.get(path).map_or_else(|| file_name(path), |(t, _)| *t),
                None,
                0,
            ),
        };
        let chapter_level = secs
            .iter()
            .map(|n| level(n))
            .filter(|l| *l > title_level)
            .min();
        let chapters: Vec<Value> = secs
            .iter()
            .filter(|n| Some(level(n)) == chapter_level)
            .map(|n| json!({"title": n.name, "line": n.line_start, "feature": c.feature(n)}))
            .collect();
        let feature = title_node
            .or(secs.first().copied())
            .or(front.get(path).map(|(_, f)| *f))
            .and_then(|n| c.feature(n));
        ep.documents.push(json!({
            "repo": r.alias, "path": path, "title": title,
            "line": title_node.and_then(|n| n.line_start).unwrap_or(1),
            "feature": feature, "chapters": chapters,
        }));
    }
}

fn timelines(alias: &str, c: &Ctx, ep: &mut EntryPoints) {
    let mut by_path: BTreeMap<&str, Vec<&Node>> = BTreeMap::new();
    let mut titles: HashMap<&str, &str> = HashMap::new();
    for n in c.nodes.iter().filter(|n| n.kind == NodeKind::Section) {
        let path = meta_str(n, "path");
        if n.metadata["timestamp"].is_string() {
            by_path.entry(path).or_default().push(n);
        } else if level(n) == 1 {
            titles.entry(path).or_insert(n.name.as_str());
        }
    }
    for (path, mut marks) in by_path {
        marks.sort_by_key(|n| (n.metadata["seconds"].as_u64().unwrap_or(0), n.line_start));
        let entries: Vec<Value> = marks
            .iter()
            .map(|n| {
                json!({
                    "t": n.metadata["timestamp"], "seconds": n.metadata["seconds"],
                    "label": n.metadata["label"], "line": n.line_start, "feature": c.feature(n),
                })
            })
            .collect();
        ep.timelines.push(json!({
            "repo": alias, "path": path,
            "title": titles.get(path).copied().unwrap_or_else(|| file_name(path)),
            "entries": entries,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Edge, SourceFile};

    fn n(kind: NodeKind, name: &str, sig: &str, meta: Value) -> Node {
        Node {
            id: Uuid::new_v4(),
            repo_id: Uuid::nil(),
            file_id: None,
            kind,
            stable_id: format!("{}:{name}", meta["path"].as_str().unwrap_or("")),
            name: name.into(),
            signature: Some(sig.into()),
            doc: None,
            line_start: Some(1),
            line_end: Some(2),
            metadata: meta,
        }
    }

    fn graph(nodes: Vec<Node>) -> Graph {
        Graph {
            files: vec![],
            nodes,
            edges: vec![],
        }
    }

    fn edge(a: &Node, b: &Node, kind: EdgeKind) -> Edge {
        Edge {
            id: Uuid::new_v4(),
            repo_id: Uuid::nil(),
            source_node_id: a.id,
            target_node_id: b.id,
            kind,
            confidence: 1.0,
        }
    }

    fn file(path: &str, language: Language) -> SourceFile {
        SourceFile {
            id: Uuid::new_v4(),
            repo_id: Uuid::nil(),
            path: path.into(),
            language,
            content_hash: String::new(),
            line_count: 1,
        }
    }

    #[test]
    fn graphql_and_contracts_list_owned_fields_and_public_functions() {
        let gql = |q: &str, sig: &str| {
            n(
                NodeKind::Field,
                q.rsplit('.').next().unwrap(),
                sig,
                json!({"qualified_name": q, "language": "graphql", "path": "schema.graphql"}),
            )
        };
        let op = |name: &str, fields: &[&str], path: &str| {
            n(
                NodeKind::Operation,
                name,
                name,
                json!({"root_fields": fields, "language": "typescript", "path": path}),
            )
        };
        let sol = |kind: NodeKind, name: &str, q: &str, path: &str, extra: Value| {
            let mut meta = json!({"qualified_name": q, "language": "solidity", "path": path});
            for (k, v) in extra.as_object().unwrap() {
                meta[k] = v.clone();
            }
            n(kind, name, &format!("function {name}()"), meta)
        };
        let api = graph(vec![
            gql(
                "Mutation.createLab",
                "createLab(input: CreateLabInput!): Lab! @aws_lambda",
            ),
            gql("Query.lab", "lab(id: ID!): Lab"),
            op("InternalLab", &["Query.lab"], "src/internal.ts"),
        ]);
        // A client with a schema copy: its copy is not a second API.
        let web = graph(vec![
            gql("Mutation.createLab", "createLab"),
            op("CreateLab", &["Mutation.createLab"], "src/lab.ts"),
        ]);
        let f = "src/Factory.sol";
        let chain = graph(vec![
            sol(NodeKind::Type, "Factory", "Factory", f, json!({})),
            sol(
                NodeKind::Method,
                "mint",
                "Factory::mint",
                f,
                json!({"visibility": "external", "modifiers": ["onlyOwner"]}),
            ),
            sol(
                NodeKind::Method,
                "owner",
                "Factory::owner",
                f,
                json!({"visibility": "public", "mutability": "view"}),
            ),
            sol(
                NodeKind::Method,
                "_check",
                "Factory::_check",
                f,
                json!({"visibility": "internal"}),
            ),
            sol(
                NodeKind::Field,
                "Minted",
                "Factory::Minted",
                f,
                json!({"event": true}),
            ),
            sol(
                NodeKind::Interface,
                "IFactory",
                "IFactory",
                "src/IFactory.sol",
                json!({}),
            ),
            sol(
                NodeKind::Method,
                "mint",
                "IFactory::mint",
                "src/IFactory.sol",
                json!({"visibility": "external"}),
            ),
            sol(
                NodeKind::Type,
                "Lib",
                "Lib",
                "src/Lib.sol",
                json!({"library": true}),
            ),
            sol(
                NodeKind::Method,
                "add",
                "Lib::add",
                "src/Lib.sol",
                json!({"visibility": "public"}),
            ),
            sol(
                NodeKind::Type,
                "Deploy",
                "Deploy",
                "script/Deploy.s.sol",
                json!({}),
            ),
            sol(
                NodeKind::Method,
                "run",
                "Deploy::run",
                "script/Deploy.s.sol",
                json!({"visibility": "public"}),
            ),
        ]);
        let ep = entry_points(&[
            RepoIndex::new("web", &web, &[]),
            RepoIndex::new("api", &api, &[]),
            RepoIndex::new("chain", &chain, &[]),
        ]);

        let fields: Vec<(&str, &str)> = ep
            .graphql
            .iter()
            .map(|g| (g["field"].as_str().unwrap(), g["repo"].as_str().unwrap()))
            .collect();
        assert_eq!(
            fields,
            [("Mutation.createLab", "api"), ("Query.lab", "api")]
        );
        assert_eq!(
            ep.graphql[0]["signature"],
            "createLab(input: CreateLabInput!): Lab! @aws_lambda"
        );
        assert_eq!(ep.graphql[0]["callers"][0]["repo"], "web");
        assert_eq!(ep.graphql[0]["callers"][0]["op"], "CreateLab");
        assert_eq!(ep.graphql[1]["callers"][0]["repo"], "api");

        let contracts: Vec<(&str, &str, usize)> = ep
            .contracts
            .iter()
            .map(|c| {
                (
                    c["name"].as_str().unwrap(),
                    c["kind"].as_str().unwrap(),
                    c["functions"].as_array().unwrap().len(),
                )
            })
            .collect();
        assert_eq!(
            contracts,
            [("Factory", "contract", 2), ("IFactory", "interface", 1)]
        );
        assert_eq!(ep.contracts[0]["events"], json!(["Minted"]));
        assert_eq!(
            ep.contracts[0]["functions"][0]["modifiers"],
            json!(["onlyOwner"])
        );
        assert_eq!(ep.contract_functions(), 3);
        assert!(is_script_or_test("script/prod/Upgrade.s.sol"));
        assert!(is_script_or_test("test/Lab.t.sol"));
        assert!(!is_script_or_test("src/core/Lab.sol"));
    }

    #[test]
    fn tools_routes_and_hooks_carry_owner_feature_and_reach() {
        let feat = Uuid::new_v4();
        let ts = |kind: NodeKind, name: &str, path: &str, extra: Value| {
            let mut meta = json!({"language": "typescript", "path": path});
            for (k, v) in extra.as_object().unwrap() {
                meta[k] = v.clone();
            }
            n(kind, name, name, meta)
        };
        let register = ts(
            NodeKind::Function,
            "registerWhoami",
            "mcp/tools.ts",
            json!({}),
        );
        let tool = ts(
            NodeKind::McpTool,
            "agent_connect_whoami",
            "mcp/tools.ts",
            json!({"detail": "Who am I"}),
        );
        let route = ts(
            NodeKind::HttpRoute,
            "POST /api/labs/[id]",
            "app/api/labs/[id]/route.ts",
            json!({}),
        );
        let e2e = ts(
            NodeKind::HttpRoute,
            "GET /health",
            "e2e/server.ts",
            json!({}),
        );
        let hook = ts(
            NodeKind::Function,
            "useCreateLab",
            "hooks/use-create-lab.ts",
            json!({"hook": {"kinds": ["mutation"], "wraps": ["useMutation"]}}),
        );
        let sdk = ts(NodeKind::Function, "createLab", "sdk/labs.ts", json!({}));
        let op = ts(
            NodeKind::Operation,
            "CreateLab",
            "sdk/labs.ts",
            json!({"root_fields": ["Mutation.createLab"]}),
        );
        let mut g = graph(vec![
            register.clone(),
            tool.clone(),
            route.clone(),
            e2e,
            hook.clone(),
            sdk.clone(),
            op.clone(),
        ]);
        g.edges = vec![
            edge(&register, &tool, EdgeKind::Defines),
            edge(&hook, &sdk, EdgeKind::Calls),
            edge(&sdk, &op, EdgeKind::Defines),
        ];
        let ep = entry_points(&[RepoIndex::new("api", &g, &[(feat, register.id)])]);
        assert_eq!(ep.mcp_tools.len(), 1);
        assert_eq!(ep.mcp_tools[0]["owner"], "registerWhoami");
        assert_eq!(ep.mcp_tools[0]["detail"], "Who am I");
        assert_eq!(ep.mcp_tools[0]["feature"], json!(feat));
        assert!(ep.features.contains(&feat));
        let routes: Vec<(&str, &str)> = ep
            .http_routes
            .iter()
            .map(|r| (r["method"].as_str().unwrap(), r["route"].as_str().unwrap()))
            .collect();
        assert_eq!(routes, [("POST", "/api/labs/[id]")]);
        assert_eq!(ep.hooks.len(), 1);
        assert_eq!(ep.hooks[0]["kinds"], json!(["mutation"]));
        assert_eq!(ep.hooks[0]["reaches"], json!(["Mutation.createLab"]));
        assert_eq!(
            ep.counts().keys().cloned().collect::<Vec<_>>(),
            ["client_hooks", "http_routes", "mcp_tools"]
        );
    }

    #[test]
    fn documents_plan_text_repos_and_timelines_list_timestamps() {
        let md = |name: &str, path: &str, line: i32, extra: Value| {
            let mut meta = json!({"language": "markdown", "path": path});
            for (k, v) in extra.as_object().unwrap() {
                meta[k] = v.clone();
            }
            let mut x = n(NodeKind::Section, name, name, meta);
            x.line_start = Some(line);
            x
        };
        let mut book = graph(vec![
            md("The Book", "ch1.md", 1, json!({"level": 1})),
            md("Origins", "ch1.md", 5, json!({"level": 2})),
            md("Detail", "ch1.md", 9, json!({"level": 3})),
            md("Methods", "ch1.md", 20, json!({"level": 2})),
            md("Part A", "ch2.md", 1, json!({"level": 1})),
            md("Part B", "ch2.md", 30, json!({"level": 1})),
            md("Talk", "talk.md", 1, json!({"level": 1})),
            md(
                "00:00",
                "talk.md",
                3,
                json!({"level": 2, "timestamp": "00:00", "seconds": 0, "label": "hello", "transcript": true}),
            ),
            md(
                "02:00",
                "talk.md",
                9,
                json!({"level": 2, "timestamp": "02:00", "seconds": 120, "label": "next", "transcript": true}),
            ),
        ]);
        book.nodes.push(n(
            NodeKind::File,
            "guides/setup.mdx",
            "",
            json!({"language": "markdown", "title": "Set up"}),
        ));
        book.nodes
            .push(md("Prompt", ".github/prompts/x.md", 1, json!({"level": 1})));
        book.files = vec![
            file("ch1.md", Language::Markdown),
            file("guides/setup.mdx", Language::Markdown),
            file("ch2.md", Language::Markdown),
            file("talk.md", Language::Markdown),
            file("build.py", Language::Python),
        ];
        let ep = entry_points(&[RepoIndex::new("book", &book, &[])]);
        let plan: Vec<(&str, Vec<&str>)> = ep
            .documents
            .iter()
            .map(|d| {
                (
                    d["title"].as_str().unwrap(),
                    d["chapters"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|c| c["title"].as_str().unwrap())
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            plan,
            vec![
                ("The Book", vec!["Origins", "Methods"]),
                ("ch2.md", vec!["Part A", "Part B"]),
                ("Set up", vec![]),
                ("Talk", vec![]),
            ]
        );
        assert_eq!(ep.timelines.len(), 1);
        assert_eq!(ep.timelines[0]["title"], "Talk");
        let ts: Vec<&str> = ep.timelines[0]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["t"].as_str().unwrap())
            .collect();
        assert_eq!(ts, ["00:00", "02:00"]);
        assert_eq!(ep.timeline_entries(), 2);

        // A code repository's READMEs are not a plan.
        book.files.push(file("a.py", Language::Python));
        book.files.push(file("b.py", Language::Python));
        book.files.push(file("c.py", Language::Python));
        book.files.push(file("d.py", Language::Python));
        assert!(entry_points(&[RepoIndex::new("code", &book, &[])])
            .documents
            .is_empty());
        assert!(is_test_path("apps/web/e2e/server.ts"));
        assert!(is_test_path("src/lab.test.ts"));
        assert!(!is_test_path("src/app/api/labs/route.ts"));
    }
}
