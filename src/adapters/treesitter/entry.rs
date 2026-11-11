//! Entry-point `ExtraPass`es: what an outside caller reaches in a file, on
//! top of the env vars and HTTP routes `surface` finds. tree-sitter node-shape
//! walks only (never regex).
//!
//! | Language | Found |
//! |---|---|
//! | TS/JS | MCP tools: `server.registerTool(NAME, {title, description}, h)`, `server.tool(NAME, "description", …)`, `server.addTool({name, description})`; `NAME` is a string or a top-level `const` of this file. Next.js route handlers: `export function GET` (any HTTP method) in `app/**/route.ts` → `GET /path` (route groups `(x)` and slots `@x` dropped, `_private` folders skipped); a default export in `pages/api/**` → `ANY /api/…`. React hooks: a `use*` function that calls a data hook ([`DATA_HOOKS`]) gets `metadata.hook = {kinds, wraps}` |
//! | Python | MCP tools: `@<server>.tool` / `@<server>.tool(name=…, description=…)` (name defaults to the function's, detail to its docstring's first line); low-level `Tool(name="…", description="…")` |
//! | Rust | MCP tools: `#[tool(…)]` on a function (rmcp; `name = "…"` or the function's name, `description = "…"`) |
//!
//! MCP tools are `SurfaceKind::McpTool` surfaces with a one-line `detail`;
//! routes join the `HttpRoute` surfaces.

use super::surface::{first_named, named_child, route_name, string_value, walk, Collector};
use super::walker::text;
use crate::domain::model::NodeKind;
use crate::ports::extractor::{FileExtraction, FileInput, SurfaceKind, Symbol};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use tree_sitter::{Node, Tree};

/// Longest `detail` kept (characters).
const MAX_DETAIL: usize = 240;

/// Data hooks a client hook wraps, and what each does: TanStack Query,
/// Apollo, urql, SWR, wagmi (contract reads and writes), tRPC (`x.useQuery`).
pub const DATA_HOOKS: &[(&str, &str)] = &[
    ("useQuery", "query"),
    ("useSuspenseQuery", "query"),
    ("useInfiniteQuery", "query"),
    ("useSuspenseInfiniteQuery", "query"),
    ("useQueries", "query"),
    ("useSuspenseQueries", "query"),
    ("useLazyQuery", "query"),
    ("useSWR", "query"),
    ("useSWRInfinite", "query"),
    ("useSWRImmutable", "query"),
    ("useReadContract", "query"),
    ("useReadContracts", "query"),
    ("useMutation", "mutation"),
    ("useSWRMutation", "mutation"),
    ("useWriteContract", "mutation"),
    ("useSendTransaction", "mutation"),
    ("useSubscription", "subscription"),
    ("useWatchContractEvent", "subscription"),
];

const HTTP_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

fn hook_kind(name: &str) -> Option<&'static str> {
    DATA_HOOKS.iter().find(|(n, _)| *n == name).map(|(_, k)| *k)
}

/// React's hook naming rule: `use` followed by an upper-case letter or digit.
pub fn is_hook_name(name: &str) -> bool {
    name.strip_prefix("use")
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// MCP tool names are `[A-Za-z0-9_.-]{1,128}`; anything else is not a tool.
fn is_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// The first paragraph of a docstring (lines up to the first blank line).
fn first_paragraph(doc: &str) -> Option<String> {
    let para: Vec<&str> = doc
        .trim_start()
        .lines()
        .map(str::trim)
        .take_while(|l| !l.is_empty())
        .collect();
    (!para.is_empty()).then(|| para.join(" "))
}

/// Whitespace collapsed to one line, at most [`MAX_DETAIL`] characters.
fn one_line(s: &str) -> Option<String> {
    let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() {
        return None;
    }
    Some(if t.chars().count() > MAX_DETAIL {
        let cut: String = t.chars().take(MAX_DETAIL - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        t
    })
}

fn named_children(n: Node) -> Vec<Node> {
    let mut cursor = n.walk();
    let kids: Vec<Node> = n
        .named_children(&mut cursor)
        .filter(|k| k.kind() != "comment")
        .collect();
    kids
}

// ---------------------------------------------------------------------------
// TypeScript / JavaScript
// ---------------------------------------------------------------------------

/// A TS/JS string expression: a literal, a substitution-free template, `a + b`
/// of those, `x as const`, or an identifier naming a known constant.
fn ts_string(n: Node, src: &str, consts: &HashMap<String, String>) -> Option<String> {
    match n.kind() {
        "string" | "template_string" => string_value(n, src),
        "binary_expression" => {
            let op = n.child_by_field_name("operator").map(|o| text(o, src));
            if op != Some("+") {
                return None;
            }
            let l = ts_string(n.child_by_field_name("left")?, src, consts)?;
            let r = ts_string(n.child_by_field_name("right")?, src, consts)?;
            Some(l + &r)
        }
        "parenthesized_expression" | "as_expression" | "satisfies_expression" => {
            ts_string(first_named(n)?, src, consts)
        }
        "identifier" => consts.get(text(n, src)).cloned(),
        _ => None,
    }
}

/// Top-level `const NAME = "…"` (exported or not) of this file.
fn ts_consts(root: Node, src: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for stmt in named_children(root) {
        let decl = if stmt.kind() == "export_statement" {
            stmt.child_by_field_name("declaration")
        } else {
            Some(stmt)
        };
        let Some(decl) = decl.filter(|d| d.kind() == "lexical_declaration") else {
            continue;
        };
        for d in named_children(decl) {
            let (Some(name), Some(value)) = (
                d.child_by_field_name("name"),
                d.child_by_field_name("value"),
            ) else {
                continue;
            };
            if name.kind() != "identifier" {
                continue;
            }
            let v = ts_string(value, src, &out);
            if let Some(v) = v {
                out.insert(text(name, src).to_string(), v);
            }
        }
    }
    out
}

/// Value of property `key` in an object literal.
fn ts_prop<'t>(obj: Node<'t>, key: &str, src: &str) -> Option<Node<'t>> {
    if obj.kind() != "object" {
        return None;
    }
    named_children(obj)
        .into_iter()
        .filter(|p| p.kind() == "pair")
        .find(|p| {
            p.child_by_field_name("key").is_some_and(|k| {
                text(k, src) == key || string_value(k, src).as_deref() == Some(key)
            })
        })
        .and_then(|p| p.child_by_field_name("value"))
}

/// The route a Next.js file serves: `(true, "/api/x/[id]")` for an app-router
/// `route.ts`, `(false, "/api/x")` for a pages-router API file.
pub fn next_route(path: &str) -> Option<(bool, String)> {
    let segs: Vec<&str> = path.split('/').collect();
    let (dirs, file) = segs.split_at(segs.len() - 1);
    let (stem, ext) = file[0].rsplit_once('.')?;
    if !matches!(ext, "ts" | "tsx" | "js" | "jsx" | "mts" | "mjs") || stem.contains('.') {
        return None;
    }
    if stem == "route" {
        let i = dirs.iter().position(|s| *s == "app")?;
        let mut parts = Vec::new();
        for s in &dirs[i + 1..] {
            if s.starts_with('_') {
                return None;
            }
            if (s.starts_with('(') && s.ends_with(')')) || s.starts_with('@') {
                continue;
            }
            parts.push(*s);
        }
        return Some((true, format!("/{}", parts.join("/"))));
    }
    let i = dirs.iter().position(|s| *s == "pages")?;
    if dirs.get(i + 1) != Some(&"api") {
        return None;
    }
    let mut parts: Vec<&str> = dirs[i + 1..].to_vec();
    if stem != "index" {
        parts.push(stem);
    }
    Some((false, format!("/{}", parts.join("/"))))
}

/// Names a module exports at top level, with the node naming each
/// (`default` for a default export).
fn ts_exports(root: Node, src: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    for stmt in named_children(root) {
        if stmt.kind() != "export_statement" {
            continue;
        }
        let mut cursor = stmt.walk();
        if stmt.children(&mut cursor).any(|c| c.kind() == "default") {
            out.push(("default".to_string(), stmt.start_byte()));
            continue;
        }
        if let Some(decl) = stmt.child_by_field_name("declaration") {
            match decl.kind() {
                "function_declaration" | "generator_function_declaration" => {
                    if let Some(n) = decl.child_by_field_name("name") {
                        out.push((text(n, src).to_string(), n.start_byte()));
                    }
                }
                "lexical_declaration" | "variable_declaration" => {
                    for d in named_children(decl) {
                        if let Some(n) = d.child_by_field_name("name") {
                            out.push((text(n, src).to_string(), n.start_byte()));
                        }
                    }
                }
                _ => {}
            }
        } else if let Some(clause) = named_child(stmt, "export_clause") {
            for spec in named_children(clause) {
                let n = spec
                    .child_by_field_name("alias")
                    .or_else(|| spec.child_by_field_name("name"));
                if let Some(n) = n {
                    out.push((text(n, src).to_string(), n.start_byte()));
                }
            }
        }
    }
    out
}

fn set_hook(sym: &mut Symbol, kinds: &BTreeSet<&str>, wraps: &BTreeSet<String>) {
    let hook = json!({"kinds": kinds, "wraps": wraps});
    match &mut sym.metadata {
        Value::Object(m) => {
            m.insert("hook".into(), hook);
        }
        other => *other = json!({ "hook": hook }),
    }
}

/// TypeScript / JavaScript entry points (see module docs).
pub fn typescript(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let root = tree.root_node();
    let consts = ts_consts(root, src);
    let mut c = Collector { file, out: vec![] };
    let symbols = out.symbols.clone();

    // Next.js route handlers.
    if let Some((app_router, route)) = next_route(&file.path) {
        for (name, at) in ts_exports(root, src) {
            let method = if app_router {
                match HTTP_METHODS.iter().find(|m| **m == name) {
                    Some(m) => *m,
                    None => continue,
                }
            } else if name == "default" {
                "ANY"
            } else {
                continue;
            };
            let Some(node) = root.descendant_for_byte_range(at, at) else {
                continue;
            };
            let owner = c.owner(&symbols, at);
            c.push_detail(
                SurfaceKind::HttpRoute,
                route_name(method, &route),
                node,
                owner,
                None,
            );
        }
    }

    // MCP tools + data hooks.
    let mut hooks: BTreeMap<usize, (BTreeSet<&'static str>, BTreeSet<String>)> = BTreeMap::new();
    walk(tree, |n| {
        if n.kind() != "call_expression" {
            return;
        }
        let Some(f) = n.child_by_field_name("function") else {
            return;
        };
        let callee = match f.kind() {
            "identifier" => text(f, src),
            "member_expression" => f
                .child_by_field_name("property")
                .map(|p| text(p, src))
                .unwrap_or(""),
            _ => return,
        };
        if let Some(kind) = hook_kind(callee) {
            let at = n.start_byte();
            let host = symbols
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    s.byte_start <= at
                        && at < s.byte_end
                        && matches!(s.kind, NodeKind::Function | NodeKind::Method)
                        && is_hook_name(&s.name)
                })
                .min_by_key(|(_, s)| s.byte_end - s.byte_start);
            if let Some((i, _)) = host {
                let e = hooks.entry(i).or_default();
                e.0.insert(kind);
                e.1.insert(callee.to_string());
            }
            return;
        }
        if f.kind() != "member_expression" {
            return;
        }
        let Some(args) = n.child_by_field_name("arguments") else {
            return;
        };
        let list = named_children(args);
        let (name, detail) = match callee {
            "registerTool" | "tool" if list.len() >= 2 => {
                let Some(name) = ts_string(list[0], src, &consts) else {
                    return;
                };
                let detail = if list[1].kind() == "object" {
                    ts_prop(list[1], "title", src)
                        .or_else(|| ts_prop(list[1], "description", src))
                        .and_then(|v| ts_string(v, src, &consts))
                } else {
                    ts_string(list[1], src, &consts)
                };
                (name, detail)
            }
            "addTool" => {
                let Some(obj) = list.first().filter(|o| o.kind() == "object") else {
                    return;
                };
                let Some(name) =
                    ts_prop(*obj, "name", src).and_then(|v| ts_string(v, src, &consts))
                else {
                    return;
                };
                let detail =
                    ts_prop(*obj, "description", src).and_then(|v| ts_string(v, src, &consts));
                (name, detail)
            }
            _ => return,
        };
        if !is_tool_name(&name) {
            return;
        }
        let owner = c.owner(&symbols, n.start_byte());
        c.push_detail(
            SurfaceKind::McpTool,
            name,
            n,
            owner,
            detail.as_deref().and_then(one_line),
        );
    });
    for (i, (kinds, wraps)) in hooks {
        set_hook(&mut out.symbols[i], &kinds, &wraps);
    }
    c.finish(out);
}

// ---------------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------------

/// String value of keyword argument `key=` in a Python `argument_list`.
fn py_kwarg(args: Option<Node>, key: &str, src: &str) -> Option<String> {
    let args = args?;
    named_children(args)
        .into_iter()
        .filter(|k| k.kind() == "keyword_argument")
        .find(|k| {
            k.child_by_field_name("name")
                .is_some_and(|x| text(x, src) == key)
        })
        .and_then(|k| k.child_by_field_name("value"))
        .and_then(|v| string_value(v, src))
}

/// Python entry points (see module docs).
pub fn python(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let mut c = Collector { file, out: vec![] };
    let symbols = out.symbols.clone();
    walk(tree, |n| match n.kind() {
        "decorator" => {
            let Some(expr) = first_named(n) else {
                return;
            };
            let (target, args) = match expr.kind() {
                "call" => (
                    expr.child_by_field_name("function"),
                    expr.child_by_field_name("arguments"),
                ),
                "attribute" => (Some(expr), None),
                _ => return,
            };
            let Some(target) = target.filter(|t| t.kind() == "attribute") else {
                return;
            };
            if target
                .child_by_field_name("attribute")
                .map(|a| text(a, src))
                != Some("tool")
            {
                return;
            }
            let fname = n
                .parent()
                .filter(|p| p.kind() == "decorated_definition")
                .and_then(|p| p.child_by_field_name("definition"))
                .and_then(|d| d.child_by_field_name("name"))
                .map(|x| text(x, src).to_string());
            let Some(name) = py_kwarg(args, "name", src).or(fname) else {
                return;
            };
            if !is_tool_name(&name) {
                return;
            }
            let owner = c.owner(&symbols, Collector::decorated_at(n));
            let doc = owner.as_deref().and_then(|o| {
                symbols
                    .iter()
                    .find(|s| s.stable_id == o)
                    .and_then(|s| s.doc.as_deref())
                    .and_then(first_paragraph)
            });
            let detail = py_kwarg(args, "title", src)
                .or_else(|| py_kwarg(args, "description", src))
                .or(doc);
            c.push_detail(
                SurfaceKind::McpTool,
                name,
                n,
                owner,
                detail.as_deref().and_then(one_line),
            );
        }
        "call" => {
            let (Some(f), Some(args)) = (
                n.child_by_field_name("function"),
                n.child_by_field_name("arguments"),
            ) else {
                return;
            };
            let ftext = text(f, src);
            if ftext != "Tool" && !ftext.ends_with(".Tool") {
                return;
            }
            let Some(name) = py_kwarg(Some(args), "name", src).filter(|x| is_tool_name(x)) else {
                return;
            };
            let detail = py_kwarg(Some(args), "description", src);
            let owner = c.owner(&symbols, n.start_byte());
            c.push_detail(
                SurfaceKind::McpTool,
                name,
                n,
                owner,
                detail.as_deref().and_then(one_line),
            );
        }
        _ => {}
    });
    c.finish(out);
}

// ---------------------------------------------------------------------------
// Rust
// ---------------------------------------------------------------------------

/// `key = "value"` pairs of an attribute's token tree.
fn rust_attr_pairs(tt: Node, src: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut cursor = tt.walk();
    let kids: Vec<Node> = tt.children(&mut cursor).collect();
    for w in kids.windows(3) {
        if w[0].kind() == "identifier" && text(w[1], src) == "=" {
            if let Some(v) = string_value(w[2], src) {
                out.insert(text(w[0], src).to_string(), v);
            }
        }
    }
    out
}

/// Rust entry points (see module docs).
pub fn rust(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let mut c = Collector { file, out: vec![] };
    let symbols = out.symbols.clone();
    walk(tree, |n| {
        if n.kind() != "attribute_item" {
            return;
        }
        let Some(attr) = named_child(n, "attribute") else {
            return;
        };
        let Some(ident) = first_named(attr) else {
            return;
        };
        if text(ident, src).rsplit("::").next() != Some("tool") {
            return;
        }
        let pairs = attr
            .child_by_field_name("arguments")
            .or_else(|| named_child(attr, "token_tree"))
            .map(|t| rust_attr_pairs(t, src))
            .unwrap_or_default();
        let owner = c.owner(&symbols, Collector::decorated_at(n));
        let fname = owner.as_deref().and_then(|o| {
            symbols
                .iter()
                .find(|s| s.stable_id == o)
                .map(|s| s.name.clone())
        });
        let Some(name) = pairs.get("name").cloned().or(fname) else {
            return;
        };
        if !is_tool_name(&name) {
            return;
        }
        let detail = pairs.get("description").and_then(|d| one_line(d));
        c.push_detail(SurfaceKind::McpTool, name, n, owner, detail);
    });
    c.finish(out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::Language;
    use crate::ports::extractor::Surface;

    fn extract(path: &str, lang: Language, src: &str) -> FileExtraction {
        let file = FileInput {
            path: path.into(),
            language: lang,
            content: src.into(),
        };
        let spec = super::super::lang::spec_for(lang, path).unwrap();
        super::super::walker::extract(spec, &file)
    }

    fn of(x: &FileExtraction, kind: SurfaceKind) -> Vec<&Surface> {
        x.surfaces.iter().filter(|s| s.kind == kind).collect()
    }

    #[test]
    fn typescript_mcp_tools_resolve_same_file_constants() {
        let src = r#"
export const WHOAMI_TOOL = "agent_connect_whoami";
const PREFIX = "data_room";
export const registerWhoami = (server: McpServer): void => {
  server.registerTool(
    WHOAMI_TOOL,
    { title: "Who am I", description: "Shows " + "the agent." },
    async () => ({ content: [] }),
  );
};
export function legacy(server) {
  server.tool("echo", "Echo the " + "input back", { text: z.string() }, async () => ({}));
  server.addTool({ name: "ping", description: "Ping", execute: async () => "pong" });
  server.tool("not a tool name", "x", () => {});
  toolbar.tool(PREFIX);
}
"#;
        let x = extract("src/server.ts", Language::TypeScript, src);
        let tools: Vec<(&str, Option<&str>, Option<&str>)> = of(&x, SurfaceKind::McpTool)
            .into_iter()
            .map(|s| (s.name.as_str(), s.detail.as_deref(), s.owner.as_deref()))
            .collect();
        assert_eq!(
            tools,
            vec![
                (
                    "agent_connect_whoami",
                    Some("Who am I"),
                    Some("typescript:src/server.ts:function:registerWhoami")
                ),
                (
                    "echo",
                    Some("Echo the input back"),
                    Some("typescript:src/server.ts:function:legacy")
                ),
                (
                    "ping",
                    Some("Ping"),
                    Some("typescript:src/server.ts:function:legacy")
                ),
            ]
        );
    }

    #[test]
    fn next_routes_from_file_paths() {
        assert_eq!(
            next_route("apps/labs/src/app/api/v1/addresses/[address]/pois/route.ts"),
            Some((true, "/api/v1/addresses/[address]/pois".into()))
        );
        assert_eq!(
            next_route("src/app/(auth)/@modal/api/login/route.js"),
            Some((true, "/api/login".into()))
        );
        assert_eq!(next_route("src/app/_lib/route.ts"), None);
        assert_eq!(next_route("src/app/route.ts"), Some((true, "/".into())));
        assert_eq!(
            next_route("pages/api/users/index.ts"),
            Some((false, "/api/users".into()))
        );
        assert_eq!(next_route("pages/about.tsx"), None);
        assert_eq!(next_route("src/app/api/x/route.test.ts"), None);
        assert_eq!(next_route("src/lib/route.ts"), None);
    }

    #[test]
    fn next_route_handlers_become_http_routes() {
        let src = r#"
import { NextResponse } from "next/server";
export async function GET(req: Request) { return NextResponse.json({}); }
export const POST = withAuth(async (req: Request) => NextResponse.json({}));
const handler = async () => new Response();
export { handler as DELETE };
export function helper() {}
"#;
        let x = extract(
            "apps/labs/src/app/api/labs/[id]/route.ts",
            Language::TypeScript,
            src,
        );
        let routes: Vec<(&str, u32)> = of(&x, SurfaceKind::HttpRoute)
            .into_iter()
            .map(|s| (s.name.as_str(), s.line))
            .collect();
        assert_eq!(
            routes,
            vec![
                ("GET /api/labs/[id]", 3),
                ("POST /api/labs/[id]", 4),
                ("DELETE /api/labs/[id]", 6)
            ]
        );
        let get = x
            .surfaces
            .iter()
            .find(|s| s.name.starts_with("GET"))
            .unwrap();
        assert_eq!(
            get.owner.as_deref(),
            Some("typescript:apps/labs/src/app/api/labs/[id]/route.ts:function:GET")
        );
        let pages = extract(
            "pages/api/health.ts",
            Language::TypeScript,
            "export default function handler(req, res) { res.end() }",
        );
        assert_eq!(
            of(&pages, SurfaceKind::HttpRoute)[0].name,
            "ANY /api/health"
        );
    }

    #[test]
    fn hooks_record_the_data_hooks_they_wrap() {
        let src = r#"
import { useQuery, useMutation } from "@tanstack/react-query";
export function usePois() {
  const address = useAddress();
  return useQuery({ queryKey: ["pois"], queryFn: async () => fetchPois(address) });
}
export const useUpdateLab = () => {
  const qc = useQueryClient();
  return useMutation({ mutationFn: updateLab });
};
export function useLabRoute() { return api.lab.get.useQuery({ id }); }
export function LabCard() { const q = useQuery({ queryKey: [] }); return null; }
export function usePlain() { return useState(0); }
"#;
        let x = extract("src/hooks.tsx", Language::TypeScript, src);
        let hooks: Vec<(&str, Value)> = x
            .symbols
            .iter()
            .filter(|s| !s.metadata["hook"].is_null())
            .map(|s| (s.name.as_str(), s.metadata["hook"].clone()))
            .collect();
        assert_eq!(
            hooks,
            vec![
                (
                    "usePois",
                    json!({"kinds": ["query"], "wraps": ["useQuery"]})
                ),
                (
                    "useUpdateLab",
                    json!({"kinds": ["mutation"], "wraps": ["useMutation"]})
                ),
                (
                    "useLabRoute",
                    json!({"kinds": ["query"], "wraps": ["useQuery"]})
                ),
            ]
        );
    }

    #[test]
    fn python_mcp_tool_decorators_and_low_level_tools() {
        let src = r#"
from mcp.server.fastmcp import FastMCP
mcp = FastMCP("molecule")

@mcp.tool()
def lab_info(oclId: str) -> dict:
    """Read one Lab and the paths
    already used in its data room.

    Longer text."""
    return {}

@mcp.tool(name="sha256_file", description="Hash a file")
async def sha(path: str) -> str:
    return ""

@server.tool
def bare():
    pass

def tools():
    return [types.Tool(name="lookup", description="Find a Lab", inputSchema={})]

@app.get("/x")
def not_a_tool():
    pass
"#;
        let x = extract("mcp/server.py", Language::Python, src);
        let tools: Vec<(&str, Option<&str>, u32)> = of(&x, SurfaceKind::McpTool)
            .into_iter()
            .map(|s| (s.name.as_str(), s.detail.as_deref(), s.line))
            .collect();
        assert_eq!(
            tools,
            vec![
                (
                    "lab_info",
                    Some("Read one Lab and the paths already used in its data room."),
                    5
                ),
                ("sha256_file", Some("Hash a file"), 13),
                ("bare", None, 17),
                ("lookup", Some("Find a Lab"), 22),
            ]
        );
    }

    #[test]
    fn rust_rmcp_tool_attributes() {
        let src = r#"
#[tool_router]
impl Server {
    #[tool(description = "Index a repository")]
    async fn index(&self) -> String { String::new() }

    #[tool(name = "chaos_feature", description = "Answer a question")]
    async fn feature(&self) -> String { String::new() }

    #[test]
    fn not_a_tool() {}
}
"#;
        let x = extract("src/mcp.rs", Language::Rust, src);
        let tools: Vec<(&str, Option<&str>)> = of(&x, SurfaceKind::McpTool)
            .into_iter()
            .map(|s| (s.name.as_str(), s.detail.as_deref()))
            .collect();
        assert_eq!(
            tools,
            vec![
                ("index", Some("Index a repository")),
                ("chaos_feature", Some("Answer a question")),
            ]
        );
    }

    #[test]
    fn hook_names_follow_the_react_rule() {
        assert!(is_hook_name("useLab"));
        assert!(is_hook_name("use3d"));
        assert!(!is_hook_name("user"));
        assert!(!is_hook_name("use"));
        assert!(!is_hook_name("Lab"));
    }
}
