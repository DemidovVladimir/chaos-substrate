//! Entry points: where someone from outside comes into what the indexed
//! repositories hold. Derived from the persisted index (node kinds and the
//! metadata the extractors record), never from source.
//!
//! | Category | Listed from |
//! |---|---|
//! | `graphql` | SDL `Field` nodes under Query/Mutation/Subscription, in the schema OWNER repo (the one defining the most root fields; tie: earlier repo), with every operation outside tests in any repo whose `metadata.root_fields` names the field, the `Resolver`s that `resolves` it and the registrations that `binds` it (line from the File node's `metadata.graphql_bindings`, Lambda handlers from its `metadata.lambda_entries`) |
//! | `contracts` | Solidity `Method`s with `visibility` public/external and public state-variable getters in contracts and interfaces (libraries, `*.s.sol`, `*.t.sol`, `script/`, `test/` excluded), each with its `selector` / `abi_signature`, guards, NatSpec `notice` (through `@inheritdoc`) and the events / errors its `Emits` / `Reverts` edges reach; per contract its events and custom errors; each function lists its `callers`, each contract its `event_callers`, `abi_users` (uses naming only the ABI) and `inherited_callers` (members of a base the index does not hold), resolved by [`crate::application::contract_abi`] across every repo given |
//! | `mcp_tools` | `McpTool` surface nodes outside tests |
//! | `http_routes` | `HttpRoute` surface nodes outside tests |
//! | `hooks` | functions with `metadata.hook` (a React `use*` function wrapping a data hook) outside tests, with the GraphQL root fields their calls reach within [`HOOK_HOPS`] hops and the contract calls (`{abi}.{member}`) they make or reach |
//! | `documents` | in a repository whose files are mostly Markdown (hidden folders such as `.github/` left out): each document's title (its one H1, else its front-matter `title:`, else the file name) and chapters (the shallowest heading level under the title) |
//! | `timelines` | `Section`s with `metadata.timestamp` (transcript segments and timestamped headings), per file |
//!
//! `features` holds the features with something a client, an agent or anyone
//! on-chain can call (GraphQL, contracts, MCP tools, routes); the project hub
//! always draws those.

use crate::application::contract_abi::{contract_links, ContractLink, RepoGraph, Tier};
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

    /// GraphQL root fields with at least one resolver found.
    pub fn graphql_fields_resolved(&self) -> usize {
        self.graphql
            .iter()
            .filter(|g| g["resolvers"].as_array().is_some_and(|r| !r.is_empty()))
            .count()
    }

    /// Caller entries on contract rows (function callers, event callers,
    /// inherited-member callers and ABI users).
    pub fn contract_callers(&self) -> usize {
        let len = |v: &Value| v.as_array().map_or(0, Vec::len);
        self.contracts
            .iter()
            .map(|c| {
                let fns: usize = c["functions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|f| len(&f["callers"]))
                    .sum();
                let events: usize = c["event_callers"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(_, v)| len(v))
                    .sum();
                fns + events + len(&c["inherited_callers"]) + len(&c["abi_users"])
            })
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
            ("graphql_fields_resolved", self.graphql_fields_resolved()),
            ("contract_functions", self.contract_functions()),
            ("contract_callers", self.contract_callers()),
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
            "{} GraphQL root fields (schema owner = repo defining the most root fields; callers = operations' metadata.root_fields outside tests), {} with a resolver (resolves edges; registrations from binds edges); {} public/external functions and public getters in {} contracts and interfaces (Solidity metadata.visibility / metadata.getter, selectors from metadata.selector, emits / reverts from Emits / Reverts edges; *.s.sol, *.t.sol, script/, test/ and libraries excluded) with {} contract callers (File metadata.contract_calls and consumer interfaces through UsesAbi; named/overlap/unique/inherited tiers; tests excluded); {} MCP tools and {} HTTP routes (surface nodes outside tests); {} client hooks (metadata.hook, GraphQL fields within {HOOK_HOPS} call hops); {} documents (repos mostly Markdown); {} timeline entries (Section metadata.timestamp)",
            self.graphql.len(),
            self.graphql_fields_resolved(),
            self.contract_functions(),
            self.contracts.len(),
            self.contract_callers(),
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
    /// Solidity function → events it emits / errors it reverts with.
    emits: HashMap<Uuid, Vec<Uuid>>,
    reverts: HashMap<Uuid, Vec<Uuid>>,
    /// GraphQL field → (resolver, `resolves` confidence).
    resolves_into: HashMap<Uuid, Vec<(Uuid, f32)>>,
    /// GraphQL field → the registering symbol (or File node) that `binds` it.
    binds_into: HashMap<Uuid, Vec<Uuid>>,
    /// file row → its File node.
    file_node: HashMap<Uuid, &'a Node>,
    /// owner stable_id → `{abi}.{member}` of the contract calls it makes.
    contract_calls: HashMap<&'a str, Vec<String>>,
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
        let mut emits: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        let mut reverts: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        let mut resolves_into: HashMap<Uuid, Vec<(Uuid, f32)>> = HashMap::new();
        let mut binds_into: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        for e in &r.graph.edges {
            match e.kind {
                EdgeKind::Emits => emits
                    .entry(e.source_node_id)
                    .or_default()
                    .push(e.target_node_id),
                EdgeKind::Reverts => reverts
                    .entry(e.source_node_id)
                    .or_default()
                    .push(e.target_node_id),
                EdgeKind::Resolves => resolves_into
                    .entry(e.target_node_id)
                    .or_default()
                    .push((e.source_node_id, e.confidence)),
                EdgeKind::Binds => binds_into
                    .entry(e.target_node_id)
                    .or_default()
                    .push(e.source_node_id),
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
        let mut contract_calls: HashMap<&str, Vec<String>> = HashMap::new();
        for f in nodes
            .iter()
            .filter(|n| n.kind == NodeKind::File && !is_test_path(&n.name))
        {
            for c in f.metadata["contract_calls"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let Some(owner) = c["owner"].as_str() else {
                    continue;
                };
                let label = format!(
                    "{}.{}",
                    c["abi"].as_str().unwrap_or("?"),
                    c["member"].as_str().unwrap_or("*")
                );
                let v = contract_calls.entry(owner).or_default();
                if !v.contains(&label) {
                    v.push(label);
                }
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
            emits,
            reverts,
            resolves_into,
            binds_into,
            file_node: nodes
                .iter()
                .filter(|n| n.kind == NodeKind::File)
                .filter_map(|n| n.file_id.map(|f| (f, n)))
                .collect(),
            contract_calls,
        }
    }

    /// Resolvers and registrations of one SDL field copy, as entry-point rows.
    fn resolution(&self, field: &str, n: &Node) -> (Vec<Value>, Vec<Value>) {
        let mut resolvers = Vec::new();
        for (rid, conf) in self.resolves_into.get(&n.id).into_iter().flatten() {
            if let Some(r) = self.by_id.get(rid) {
                resolvers.push(json!({
                    "symbol": meta_str(r, "qualified_name"), "path": meta_str(r, "path"),
                    "line": r.line_start, "line_end": r.line_end,
                    "confidence": (f64::from(*conf) * 100.0).round() / 100.0,
                    "feature": self.feature(r),
                }));
            }
        }
        let mut bindings = Vec::new();
        for sid in self.binds_into.get(&n.id).into_iter().flatten() {
            let Some(s) = self.by_id.get(sid) else {
                continue;
            };
            let is_file = s.kind == NodeKind::File;
            let file = if is_file {
                Some(*s)
            } else {
                s.file_id.and_then(|f| self.file_node.get(&f).copied())
            };
            let owner = (!is_file).then_some(s.stable_id.as_str());
            let line = file
                .and_then(|f| f.metadata["graphql_bindings"].as_array())
                .into_iter()
                .flatten()
                .find(|b| b["coordinate"].as_str() == Some(field) && b["owner"].as_str() == owner)
                .and_then(|b| b["line"].as_i64())
                .or(s.line_start.map(i64::from));
            bindings.push(json!({
                "symbol": if is_file { "(file)" } else { meta_str(s, "qualified_name") },
                "path": if is_file { s.name.as_str() } else { meta_str(s, "path") },
                "line": line,
                "lambda": file.map_or(json!([]), |f| match &f.metadata["lambda_entries"] {
                    Value::Array(a) => Value::Array(a.clone()),
                    _ => json!([]),
                }),
            }));
        }
        (resolvers, bindings)
    }

    /// Sorted, distinct names of the nodes `map` links `id` to.
    fn names(&self, map: &HashMap<Uuid, Vec<Uuid>>, id: Uuid) -> Vec<&str> {
        let mut v: Vec<&str> = map
            .get(&id)
            .into_iter()
            .flatten()
            .filter_map(|t| self.by_id.get(t))
            .map(|t| t.name.as_str())
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// `start` plus every node its calls reach within [`HOOK_HOPS`] hops.
    fn reach_set(&self, start: Uuid) -> Vec<Uuid> {
        let mut seen = vec![start];
        let mut frontier = vec![start];
        for _ in 0..HOOK_HOPS {
            let mut next = Vec::new();
            for id in &frontier {
                for t in self.calls.get(id).into_iter().flatten() {
                    if !seen.contains(t) {
                        seen.push(*t);
                        next.push(*t);
                    }
                }
            }
            frontier = next;
        }
        seen
    }

    /// Contract calls (`{abi}.{member}`) `start` makes or reaches, sorted.
    fn contract_calls_from(&self, start: Uuid) -> Vec<String> {
        let mut out: BTreeSet<String> = BTreeSet::new();
        for id in self.reach_set(start) {
            if let Some(n) = self.by_id.get(&id) {
                if let Some(cs) = self.contract_calls.get(n.stable_id.as_str()) {
                    out.extend(cs.iter().cloned());
                }
            }
        }
        out.into_iter().collect()
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
    let callers = ContractCallers::build(repos, &ctx);
    for (r, c) in repos.iter().zip(&ctx) {
        contracts(r.alias, c, &callers, &mut ep);
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
                NodeKind::Operation if !is_test_path(meta_str(n, "path")) => {
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
        let (mut resolvers, mut bindings) = (Vec::new(), Vec::new());
        for n in &own {
            let (r, b) = ctx[owner].resolution(field, n);
            resolvers.extend(r);
            bindings.extend(b);
        }
        let row_key = |v: &Value| {
            (
                v["path"].as_str().unwrap_or("").to_string(),
                v["line"].as_i64().unwrap_or(0),
                v["symbol"].as_str().unwrap_or("").to_string(),
            )
        };
        resolvers.sort_by_key(row_key);
        resolvers.dedup();
        bindings.sort_by_key(row_key);
        bindings.dedup();
        for r in &resolvers {
            if let Some(f) = r["feature"].as_str().and_then(|f| f.parse::<Uuid>().ok()) {
                ep.features.insert(f);
            }
        }
        let (root, name) = field.split_once('.').unwrap_or(("", field));
        ep.graphql.push(json!({
            "field": field, "root": root, "name": name, "repo": repos[owner].alias,
            "signature": signature, "files": files, "feature": feature,
            "callers": callers.get(field).cloned().unwrap_or_default(),
            "resolvers": resolvers, "bindings": bindings,
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

/// Contract callers across all repos ([`contract_links`], same-repo uses
/// kept), keyed for the contract rows.
#[derive(Default)]
struct ContractCallers {
    /// member node (function, getter or event) → callers.
    by_member: HashMap<Uuid, Vec<Value>>,
    /// container node → callers of members a base the index does not hold declares.
    inherited: HashMap<Uuid, Vec<Value>>,
    /// container node → uses that name only the ABI.
    abi_users: HashMap<Uuid, Vec<Value>>,
}

impl ContractCallers {
    fn build(repos: &[RepoIndex], ctx: &[Ctx]) -> Self {
        let graphs: Vec<RepoGraph> = repos
            .iter()
            .map(|r| RepoGraph {
                nodes: &r.graph.nodes,
                edges: &r.graph.edges,
            })
            .collect();
        let mut out = Self::default();
        for l in contract_links(&graphs, true) {
            let owner = l
                .consumer_node
                .and_then(|o| ctx[l.consumer].by_id.get(&o))
                .filter(|o| o.kind != NodeKind::File)
                .map(|o| o.name.clone());
            let row = caller_row(&l, repos[l.consumer].alias, owner);
            let slot = match (l.member_node, &l.member, l.tier) {
                (Some(m), _, _) => out.by_member.entry(m).or_default(),
                (None, Some(_), Tier::Inherited) => out.inherited.entry(l.container).or_default(),
                (None, None, _) => out.abi_users.entry(l.container).or_default(),
                _ => continue,
            };
            slot.push(row);
        }
        let key = |v: &Value| {
            (
                v["repo"].as_str().unwrap_or("").to_string(),
                v["path"].as_str().unwrap_or("").to_string(),
                v["line"].as_u64().unwrap_or(0),
            )
        };
        for list in out
            .by_member
            .values_mut()
            .chain(out.inherited.values_mut())
            .chain(out.abi_users.values_mut())
        {
            list.sort_by_key(key);
        }
        out
    }
}

fn caller_row(l: &ContractLink, repo: &str, owner: Option<String>) -> Value {
    let mut row = json!({
        "repo": repo, "path": l.path, "line": l.line, "owner": owner, "via": l.via,
        "abi": l.abi, "tier": l.tier.label(), "confidence": l.confidence,
    });
    if l.tier == Tier::Inherited {
        row["member"] = json!(l.member);
        row["bases"] = json!(l.bases);
    }
    row
}

fn contracts(alias: &str, c: &Ctx, callers: &ContractCallers, ep: &mut EntryPoints) {
    let sol = |n: &&Node| {
        meta_str(n, "language") == "solidity" && !is_script_or_test(meta_str(n, "path"))
    };
    // (path, container name) → (kind, line, node id)
    let mut containers: BTreeMap<(&str, &str), (&str, Option<i32>, Uuid)> = BTreeMap::new();
    for n in c.nodes.iter().filter(sol) {
        let kind = match n.kind {
            NodeKind::Interface => "interface",
            NodeKind::Type if n.metadata["library"] == json!(true) => "library",
            NodeKind::Type if !meta_str(n, "qualified_name").contains("::") => "contract",
            _ => continue,
        };
        containers.insert(
            (meta_str(n, "path"), n.name.as_str()),
            (kind, n.line_start, n.id),
        );
    }
    // `Contract::name` → NatSpec notices, for `@inheritdoc Contract`.
    let mut notices: HashMap<&str, Vec<&Value>> = HashMap::new();
    for n in c
        .nodes
        .iter()
        .filter(|n| meta_str(n, "language") == "solidity")
    {
        if let Some(ns) = n.metadata.get("natspec") {
            notices
                .entry(meta_str(n, "qualified_name"))
                .or_default()
                .push(ns);
        }
    }
    let notice_of = |n: &Node| -> Option<String> {
        let mut ns = n.metadata.get("natspec")?;
        for _ in 0..4 {
            if let Some(t) = ns["notice"].as_str() {
                return Some(t.to_string());
            }
            let base = ns["inheritdoc"].as_str()?;
            match notices
                .get(format!("{base}::{}", n.name).as_str())
                .map(Vec::as_slice)
            {
                Some([one]) => ns = one,
                _ => return None,
            }
        }
        None
    };
    let mut functions: BTreeMap<(&str, &str), Vec<Value>> = BTreeMap::new();
    let mut events: BTreeMap<(&str, &str), Vec<&str>> = BTreeMap::new();
    let mut errors: BTreeMap<(&str, &str), Vec<&str>> = BTreeMap::new();
    let mut event_callers: BTreeMap<(&str, &str), serde_json::Map<String, Value>> = BTreeMap::new();
    for n in c.nodes.iter().filter(sol) {
        let q = meta_str(n, "qualified_name");
        let Some((container, _)) = q.split_once("::") else {
            continue;
        };
        let key = (meta_str(n, "path"), container);
        match containers.get(&key) {
            Some((kind, _, _)) if *kind != "library" => {}
            _ => continue,
        }
        if n.kind == NodeKind::Field && n.metadata["event"] == json!(true) {
            events.entry(key).or_default().push(n.name.as_str());
            if let Some(cs) = callers.by_member.get(&n.id) {
                event_callers
                    .entry(key)
                    .or_default()
                    .insert(n.name.clone(), json!(cs));
            }
            continue;
        }
        if n.kind == NodeKind::Field && n.metadata["error"] == json!(true) {
            errors.entry(key).or_default().push(n.name.as_str());
            continue;
        }
        let vis = meta_str(n, "visibility");
        let getter = n.kind == NodeKind::Field && n.metadata["getter"] == json!(true);
        if !(getter || (n.kind == NodeKind::Method && (vis == "public" || vis == "external"))) {
            continue;
        }
        let feature = c.feature(n);
        if let Some(f) = feature {
            ep.features.insert(f);
        }
        let mut row = json!({
            "name": n.name,
            "signature": match meta_str(n, "header") { "" => n.signature.as_deref().unwrap_or(""), h => h },
            "visibility": vis,
            "mutability": meta_str(n, "mutability"),
            "modifiers": n.metadata.get("modifiers").cloned().unwrap_or(json!([])),
            "guards": n.metadata.get("guards").cloned().unwrap_or(json!([])),
            "emits": c.names(&c.emits, n.id),
            "reverts": c.names(&c.reverts, n.id),
            "line": n.line_start, "feature": feature,
        });
        if getter {
            let inputs: Vec<&str> = n.metadata["params"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p["type"].as_str())
                .collect();
            let ret = n.metadata["returns"][0]["type"].as_str().unwrap_or("");
            row["signature"] = json!(format!(
                "{}({}) view returns ({ret})",
                n.name,
                inputs.join(", ")
            ));
            row["getter"] = json!(true);
        }
        for k in ["selector", "abi_signature"] {
            if let Some(v) = n.metadata.get(k) {
                row[k] = v.clone();
            }
        }
        if let Some(notice) = notice_of(n) {
            row["notice"] = json!(notice);
        }
        if let Some(cs) = callers.by_member.get(&n.id) {
            row["callers"] = json!(cs);
        }
        functions.entry(key).or_default().push(row);
    }
    let mut rows: Vec<Value> = functions
        .into_iter()
        .map(|((path, name), mut fns)| {
            fns.sort_by_key(|f| f["line"].as_i64().unwrap_or(0));
            let (kind, line, id) = containers[&(path, name)];
            let mut row = json!({
                "repo": alias, "name": name, "kind": kind, "path": path, "line": line,
                "functions": fns, "events": events.get(&(path, name)).cloned().unwrap_or_default(),
                "errors": errors.get(&(path, name)).cloned().unwrap_or_default(),
            });
            if let Some(ec) = event_callers.remove(&(path, name)) {
                row["event_callers"] = Value::Object(ec);
            }
            if let Some(cs) = callers.inherited.get(&id) {
                row["inherited_callers"] = json!(cs);
            }
            if let Some(cs) = callers.abi_users.get(&id) {
                row["abi_users"] = json!(cs);
            }
            row
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
            let mut row = json!({
                "repo": alias, "name": n.name,
                "kinds": n.metadata["hook"]["kinds"], "wraps": n.metadata["hook"]["wraps"],
                "path": meta_str(n, "path"), "line": n.line_start,
                "feature": c.feature(n), "reaches": c.reaches(n.id),
            });
            let calls = c.contract_calls_from(n.id);
            if !calls.is_empty() {
                row["contract_calls"] = json!(calls);
            }
            row
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
    fn graphql_rows_carry_resolvers_and_bindings() {
        let gql = |q: &str, path: &str| {
            n(
                NodeKind::Field,
                q.rsplit('.').next().unwrap(),
                q,
                json!({"qualified_name": q, "language": "graphql", "path": path}),
            )
        };
        // Two SDL copies in the owner repo; the edges land on the first.
        let create = gql("Mutation.createLab", "src/appsync/labs.graphql");
        let create_copy = gql("Mutation.createLab", "src/appsync/merged.graphql");
        let lab = gql("Query.lab", "src/appsync/labs.graphql");
        let index = "lambda/appsync-resolver-labs-lambda/index.ts";
        let mut resolver = n(
            NodeKind::Resolver,
            "createLab",
            "case \"createLab\":",
            json!({"qualified_name": "originalHandler::createLab", "language": "typescript", "path": index}),
        );
        resolver.line_start = Some(13);
        resolver.line_end = Some(20);
        let config = "lib/appsync/labs-lambda-config.ts";
        let config_file = Uuid::new_v4();
        let mut method = n(
            NodeKind::Method,
            "getResolvers",
            "getResolvers()",
            json!({"qualified_name": "AppSyncLabsLambdaConfig::getResolvers", "language": "typescript", "path": config}),
        );
        method.file_id = Some(config_file);
        method.stable_id =
            format!("typescript:{config}:method:AppSyncLabsLambdaConfig::getResolvers");
        let mut file_node = n(
            NodeKind::File,
            config,
            "",
            json!({
                "language": "typescript",
                "graphql_bindings": [{"coordinate": "Mutation.createLab", "line": 666, "owner": method.stable_id}],
                "lambda_entries": [index],
            }),
        );
        file_node.file_id = Some(config_file);
        file_node.stable_id = format!("file:{config}");
        let op = |name: &str, path: &str| {
            n(
                NodeKind::Operation,
                name,
                name,
                json!({"root_fields": ["Mutation.createLab"], "language": "typescript", "path": path}),
            )
        };
        let mut api = graph(vec![
            create.clone(),
            create_copy,
            lab,
            resolver.clone(),
            method.clone(),
            file_node,
        ]);
        let mut resolves = edge(&resolver, &create, EdgeKind::Resolves);
        resolves.confidence = 0.9;
        api.edges = vec![resolves, edge(&method, &create, EdgeKind::Binds)];
        let lambda_feature = Uuid::new_v4();
        let web = graph(vec![
            op("CreateLab", "src/hooks/useCreateLab.ts"),
            op("CreateLabFixture", "src/__tests__/createLab.test.ts"),
        ]);
        let ep = entry_points(&[
            RepoIndex::new("api", &api, &[(lambda_feature, resolver.id)]),
            RepoIndex::new("web", &web, &[]),
        ]);
        let row = &ep.graphql[0];
        assert_eq!(row["field"], "Mutation.createLab");
        assert_eq!(
            row["resolvers"],
            json!([{"symbol": "originalHandler::createLab", "path": index, "line": 13,
                    "line_end": 20, "confidence": 0.9, "feature": lambda_feature}])
        );
        assert_eq!(
            row["bindings"],
            json!([{"symbol": "AppSyncLabsLambdaConfig::getResolvers", "path": config,
                    "line": 666, "lambda": [index]}])
        );
        // The test fixture's operation is not a caller.
        assert_eq!(row["callers"].as_array().unwrap().len(), 1);
        assert_eq!(row["callers"][0]["op"], "CreateLab");
        let other = &ep.graphql[1];
        assert_eq!(other["field"], "Query.lab");
        assert_eq!(other["resolvers"], json!([]));
        assert_eq!(other["bindings"], json!([]));
        assert_eq!(ep.counts()["graphql_fields_resolved"], 1);
        assert!(ep.features.contains(&lambda_feature));
        assert!(ep.provenance().contains("1 with a resolver"));
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
        let mut chain = graph(vec![
            sol(NodeKind::Type, "Factory", "Factory", f, json!({})),
            sol(
                NodeKind::Method,
                "mint",
                "Factory::mint",
                f,
                json!({"visibility": "external", "modifiers": ["onlyOwner"],
                    "guards": ["onlyOwner"], "abi_signature": "mint(address)",
                    "selector": crate::domain::hash::selector_hex("mint(address)")}),
            ),
            // onchainlabs LabNFT.sol:46 `uint256 public mintFeeWei;`
            sol(
                NodeKind::Field,
                "mintFeeWei",
                "Factory::mintFeeWei",
                f,
                json!({"state_var": true, "visibility": "public", "getter": true,
                    "mutability": "view", "type": "uint256", "params": [],
                    "returns": [{"name": "", "type": "uint256"}],
                    "abi_signature": "mintFeeWei()",
                    "selector": crate::domain::hash::selector_hex("mintFeeWei()")}),
            ),
            sol(
                NodeKind::Field,
                "nonces",
                "Factory::nonces",
                f,
                json!({"state_var": true, "visibility": "internal", "type": "uint256"}),
            ),
            sol(
                NodeKind::Field,
                "InvalidCaller",
                "Factory::InvalidCaller",
                f,
                json!({"error": true, "params": []}),
            ),
            sol(
                NodeKind::Method,
                "receive",
                "Factory::receive",
                f,
                json!({"visibility": "external", "mutability": "payable", "special": "receive"}),
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
            sol(
                NodeKind::Field,
                "IS_SCRIPT",
                "Deploy::IS_SCRIPT",
                "script/Deploy.s.sol",
                json!({"state_var": true, "visibility": "public", "getter": true}),
            ),
        ]);
        // onchainlabs OnChainLabBeacon::getTrustedAttesters: `@inheritdoc IOnChainLabBeacon`.
        chain.nodes.push(sol(
            NodeKind::Method,
            "getTrustedAttesters",
            "Factory::getTrustedAttesters",
            f,
            json!({"visibility": "external", "mutability": "view",
                "natspec": {"inheritdoc": "IFactory"}}),
        ));
        chain.nodes.push(sol(
            NodeKind::Method,
            "getTrustedAttesters",
            "IFactory::getTrustedAttesters",
            "src/IFactory.sol",
            json!({"visibility": "external", "mutability": "view",
                "natspec": {"notice": "The attesters every Lab trusts."}}),
        ));
        let by_name = |name: &str| chain.nodes.iter().find(|x| x.name == name).unwrap().clone();
        chain.edges = vec![
            edge(&by_name("mint"), &by_name("Minted"), EdgeKind::Emits),
            edge(
                &by_name("mint"),
                &by_name("InvalidCaller"),
                EdgeKind::Reverts,
            ),
        ];
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
            [("Factory", "contract", 5), ("IFactory", "interface", 2)]
        );
        assert_eq!(ep.contracts[0]["events"], json!(["Minted"]));
        assert_eq!(ep.contracts[0]["errors"], json!(["InvalidCaller"]));
        let fns = &ep.contracts[0]["functions"];
        let row = |name: &str| {
            fns.as_array()
                .unwrap()
                .iter()
                .find(|f| f["name"] == name)
                .unwrap()
                .clone()
        };
        let mint = row("mint");
        assert_eq!(mint["modifiers"], json!(["onlyOwner"]));
        assert_eq!(mint["guards"], json!(["onlyOwner"]));
        assert_eq!(mint["selector"], "0x6a627842");
        assert_eq!(mint["abi_signature"], "mint(address)");
        assert_eq!(mint["emits"], json!(["Minted"]));
        assert_eq!(mint["reverts"], json!(["InvalidCaller"]));
        let fee = row("mintFeeWei");
        assert_eq!(fee["getter"], true);
        assert_eq!(fee["signature"], "mintFeeWei() view returns (uint256)");
        assert_eq!(fee["mutability"], "view");
        assert_eq!(
            fee["selector"],
            crate::domain::hash::selector_hex("mintFeeWei()")
        );
        let receive = row("receive");
        assert_eq!(receive["visibility"], "external");
        assert!(receive.get("selector").is_none());
        assert_eq!(row("owner")["emits"], json!([]));
        assert_eq!(
            row("getTrustedAttesters")["notice"],
            "The attesters every Lab trusts."
        );
        assert!(row("owner").get("notice").is_none());
        assert_eq!(ep.contract_functions(), 7);
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

    /// chain: LabNFT (accountOf, event LabMinted, base ERC721); web: an
    /// ABI, a hook calling it, and a decode helper.
    fn contract_fixture() -> (Graph, Graph, Node) {
        let p = "src/NFT/LabNFT.sol";
        let sol = |kind: NodeKind, name: &str, q: &str, extra: Value| {
            let mut meta = json!({"qualified_name": q, "language": "solidity", "path": p});
            for (k, v) in extra.as_object().unwrap() {
                meta[k] = v.clone();
            }
            n(kind, name, &format!("function {name}()"), meta)
        };
        let chain = graph(vec![
            sol(
                NodeKind::Type,
                "LabNFT",
                "LabNFT",
                json!({"bases": ["ERC721"]}),
            ),
            sol(
                NodeKind::Method,
                "accountOf",
                "LabNFT::accountOf",
                json!({"visibility": "external", "mutability": "view", "abi_signature": "accountOf(uint256)"}),
            ),
            sol(
                NodeKind::Field,
                "LabMinted",
                "LabNFT::LabMinted",
                json!({"event": true}),
            ),
        ]);
        let abi = n(
            NodeKind::Abi,
            "labNftAbi",
            "export const labNftAbi",
            json!({"language": "typescript", "path": "src/abis.ts", "qualified_name": "labNftAbi",
                   "abi": {"functions": ["accountOf(uint256)", "ownerOf(uint256)"], "events": ["LabMinted(uint256)"]}}),
        );
        let mut hook = n(
            NodeKind::Function,
            "useLabAccount",
            "useLabAccount",
            json!({"language": "typescript", "path": "src/use-lab-account.ts", "hook": {"kinds": ["query"], "wraps": ["useReadContract"]}}),
        );
        hook.stable_id = "typescript:src/use-lab-account.ts:function:useLabAccount".into();
        let mut helper = n(
            NodeKind::Function,
            "readOwner",
            "readOwner",
            json!({"language": "typescript", "path": "src/owner.ts"}),
        );
        helper.stable_id = "typescript:src/owner.ts:function:readOwner".into();
        let call = |member: Option<&str>, kind: &str, owner: &str, line: u32| json!({"member": member, "kind": kind, "abi": "labNftAbi", "scope": "object", "via": "useReadContract", "owner": owner, "line": line});
        let f1 = n(
            NodeKind::File,
            "src/use-lab-account.ts",
            "",
            json!({"language": "typescript", "contract_calls": [call(Some("accountOf"), "function", &hook.stable_id, 12)]}),
        );
        let f2 = n(
            NodeKind::File,
            "src/owner.ts",
            "",
            json!({"language": "typescript", "contract_calls": [
                call(Some("ownerOf"), "function", &helper.stable_id, 30),
                call(Some("LabMinted"), "event", &helper.stable_id, 31),
                call(None, "abi", &helper.stable_id, 32),
            ]}),
        );
        let f3 = n(
            NodeKind::File,
            "src/owner.test.ts",
            "",
            json!({"language": "typescript", "contract_calls": [call(Some("accountOf"), "function", "x", 5)]}),
        );
        let mut web = graph(vec![
            abi.clone(),
            hook.clone(),
            helper.clone(),
            f1.clone(),
            f2.clone(),
            f3.clone(),
        ]);
        web.edges = vec![
            edge(&hook, &abi, EdgeKind::UsesAbi),
            edge(&helper, &abi, EdgeKind::UsesAbi),
            edge(&f3, &abi, EdgeKind::UsesAbi),
            edge(&hook, &helper, EdgeKind::Calls),
        ];
        (chain, web, hook)
    }

    #[test]
    fn contracts_list_callers_per_function() {
        let (chain, web, _) = contract_fixture();
        let ep = entry_points(&[
            RepoIndex::new("chain", &chain, &[]),
            RepoIndex::new("web", &web, &[]),
        ]);
        assert_eq!(ep.contracts.len(), 1);
        let row = &ep.contracts[0];
        let f = &row["functions"][0];
        assert_eq!(f["name"], "accountOf");
        let cs = f["callers"].as_array().unwrap();
        // The test file's accountOf call (src/owner.test.ts:5) is no caller.
        assert_eq!(cs.len(), 1, "{cs:?}");
        let c = &cs[0];
        assert_eq!(
            (c["repo"].as_str(), c["path"].as_str(), c["line"].as_u64()),
            (Some("web"), Some("src/use-lab-account.ts"), Some(12))
        );
        assert_eq!(
            (c["owner"].as_str(), c["via"].as_str(), c["abi"].as_str()),
            (
                Some("useLabAccount"),
                Some("useReadContract"),
                Some("labNftAbi")
            )
        );
        assert_eq!(c["tier"], "named");
    }

    #[test]
    fn contract_rows_list_abi_users_and_event_callers() {
        let (chain, web, _) = contract_fixture();
        let ep = entry_points(&[
            RepoIndex::new("chain", &chain, &[]),
            RepoIndex::new("web", &web, &[]),
        ]);
        let row = &ep.contracts[0];
        assert_eq!(row["events"], json!(["LabMinted"]));
        assert_eq!(row["event_callers"]["LabMinted"][0]["line"], 31);
        assert_eq!(row["inherited_callers"][0]["member"], "ownerOf");
        assert_eq!(row["inherited_callers"][0]["bases"], json!(["ERC721"]));
        assert_eq!(row["abi_users"][0]["line"], 32);
        assert_eq!(row["abi_users"][0]["owner"], "readOwner");
        assert_eq!(ep.contract_callers(), 4);
        assert_eq!(ep.counts()["contract_callers"], 4);
        // Per-repo pages only see their own repo: no callers on the chain alone.
        let alone = entry_points(&[RepoIndex::new("chain", &chain, &[])]);
        assert_eq!(alone.contract_callers(), 0);
    }

    #[test]
    fn hooks_list_contract_calls_within_hops() {
        let (chain, web, _) = contract_fixture();
        let ep = entry_points(&[
            RepoIndex::new("chain", &chain, &[]),
            RepoIndex::new("web", &web, &[]),
        ]);
        assert_eq!(ep.hooks.len(), 1);
        assert_eq!(
            ep.hooks[0]["contract_calls"],
            json!([
                "labNftAbi.*",
                "labNftAbi.LabMinted",
                "labNftAbi.accountOf",
                "labNftAbi.ownerOf"
            ])
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
