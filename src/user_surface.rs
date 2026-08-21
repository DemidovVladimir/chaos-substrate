//! User-surface extraction — the entrypoints a person actually touches.
//!
//! Collects, per file, the CLI commands a user can type (clap, commander,
//! argparse, click), the HTTP routes the product serves (express/fastify,
//! Flask/FastAPI, actix/rocket attributes, axum `.route`), the environment
//! variables the code reads (`std::env`, `process.env`, `os.environ`), and the
//! GraphQL root fields an SDL schema exposes (`Query.user`). Each becomes a
//! first-class node (`cli_command` / `http_route` / `env_var` /
//! `graphql_field`) with its own chunk, so "how do you operate this product"
//! is answerable from the persisted index instead of from docs.
//!
//! Nodes are deliberately per-file (`{path}:env:{VAR}`, never a repo-wide
//! `env:{VAR}`): a shared hub node would glue every file reading
//! `DATABASE_URL` into one Louvain community — the same god-node failure mode
//! the import extraction already solved by dropping third-party hubs.
//!
//! This module owns the language-agnostic entry shape, the emitter, and the
//! `syn`-based collector for Rust. The JS/TS and Python collectors live in
//! their language modules (`crate::lang::javascript` / `crate::lang::python`)
//! because they need those parsers' AST types, and feed entries back through
//! [`crate::lang::FileExtraction::emit_user_surface`].

use crate::{
    extractor::{chunk_for_node, edge, slice_lines},
    models::{EdgeKind, ExtractionResult, KnowledgeNode, NodeKind, SourceFile},
    weights,
};
use serde_json::{json, Value};
use std::collections::HashSet;
use syn::spanned::Spanned;
use uuid::Uuid;

/// Which user-facing surface an entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Surface {
    Cli,
    Route,
    Env,
    /// A root field of a GraphQL schema (`Query.user`) — the API surface a
    /// client can call, collected from SDL by `crate::lang::graphql`.
    GraphqlField,
}

impl Surface {
    fn node_kind(self) -> NodeKind {
        match self {
            Surface::Cli => NodeKind::CliCommand,
            Surface::Route => NodeKind::HttpRoute,
            Surface::Env => NodeKind::EnvVar,
            Surface::GraphqlField => NodeKind::GraphqlField,
        }
    }

    /// Short tag used in stable ids and metadata.
    fn tag(self) -> &'static str {
        match self {
            Surface::Cli => "cli",
            Surface::Route => "route",
            Surface::Env => "env",
            Surface::GraphqlField => "graphql_field",
        }
    }

    /// Human label used in chunk headers.
    fn label(self) -> &'static str {
        match self {
            Surface::Cli => "CLI command",
            Surface::Route => "HTTP route",
            Surface::Env => "environment variable",
            Surface::GraphqlField => "GraphQL field",
        }
    }
}

/// One collected entrypoint, language-agnostic. Lines are 1-based.
pub(crate) struct SurfaceEntry {
    pub surface: Surface,
    /// CLI command name, `"METHOD /path"` for routes, or the env var name.
    pub name: String,
    /// What defined it: clap, commander, express, flask, std::env, …
    pub framework: &'static str,
    /// Extra metadata merged into the node (`method`, `route_path`, `help`, …).
    pub detail: Value,
    pub line_start: usize,
    pub line_end: usize,
}

/// Emit nodes, edges and chunks for the entries collected from one file.
/// Dedups by stable id within the file (first definition/read wins), so an env
/// var read five times in a file stays one node.
pub(crate) fn emit_surface_entries(
    repo_id: Uuid,
    file: &SourceFile,
    file_node_id: Uuid,
    entries: Vec<SurfaceEntry>,
    result: &mut ExtractionResult,
) {
    let mut seen: HashSet<String> = HashSet::new();
    for entry in entries {
        let name = entry.name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let surface = entry.surface;
        let stable_id = format!("{}:{}:{}", file.path, surface.tag(), name);
        if !seen.insert(stable_id.clone()) {
            continue;
        }

        let mut metadata = json!({
            "surface": surface.tag(),
            "language": file.language.as_str(),
            "file": file.path,
            "framework": entry.framework,
        });
        if let (Some(meta), Some(detail)) = (metadata.as_object_mut(), entry.detail.as_object()) {
            for (k, v) in detail {
                meta.insert(k.clone(), v.clone());
            }
        }

        let (edge_kind, weight) = match surface {
            Surface::Env => (EdgeKind::Configures, weights::READS_ENV),
            _ => (EdgeKind::Defines, weights::DEFINES_ENTRYPOINT),
        };

        let kind = surface.node_kind();
        let line_start = entry.line_start.max(1);
        let line_end = entry.line_end.max(line_start);
        let code = slice_lines(&file.content, line_start, line_end);

        let node = KnowledgeNode {
            id: Uuid::new_v4(),
            repo_id,
            file_id: Some(file.id),
            kind: kind.clone(),
            stable_id,
            name: name.clone(),
            line_start: Some(line_start as i32),
            line_end: Some(line_end as i32),
            metadata,
        };

        result.edges.push(edge(
            repo_id,
            file_node_id,
            node.id,
            edge_kind,
            weight,
            json!({"surface": surface.tag(), "file": file.path}),
        ));

        result.chunks.push(chunk_for_node(
            repo_id,
            Some(file.id),
            Some(node.id),
            kind.as_str(),
            &format!(
                "User surface: {label}\nLanguage: {language}\nFile: {path}\n{title}: {name}\nFramework: {framework}\nLines: {line_start}-{line_end}\n\n{code}",
                label = surface.label(),
                language = file.language.as_str(),
                path = file.path,
                title = match surface {
                    Surface::Cli => "Command",
                    Surface::Route => "Route",
                    Surface::Env => "Variable",
                    Surface::GraphqlField => "Field",
                },
                framework = entry.framework,
            ),
            Some(line_start as i32),
            Some(line_end as i32),
            json!({
                "surface": surface.tag(),
                "kind": kind.as_str(),
                "name": name,
                "framework": entry.framework,
                "file": file.path
            }),
        ));

        result.nodes.push(node);
    }
}

// ---------------------------------------------------------------------------
// Rust collector (syn)
// ---------------------------------------------------------------------------

const HTTP_METHODS: &[&str] = &["get", "post", "put", "patch", "delete", "head"];

/// Collect Rust user-surface entries from an already-parsed file:
/// clap derive (`#[derive(Parser)]` programs, `#[derive(Subcommand)]`
/// variants), clap builder (`Command::new("…")`), `std::env::var` /
/// `env!`/`option_env!` reads, axum `.route("/…", get(handler))`, and
/// actix/rocket-style `#[get("/…")]` attribute routes.
pub(crate) fn collect_rust_surface(syntax: &syn::File) -> Vec<SurfaceEntry> {
    let mut collector = RustSurfaceCollector::default();
    syn::visit::visit_file(&mut collector, syntax);
    collector.entries
}

#[derive(Default)]
struct RustSurfaceCollector {
    entries: Vec<SurfaceEntry>,
}

impl<'ast> syn::visit::Visit<'ast> for RustSurfaceCollector {
    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        if derives(&item.attrs, "Parser") {
            let name = command_name_attr(&item.attrs)
                .unwrap_or_else(|| kebab_case(&item.ident.to_string()));
            let (l, e) = span_lines(item);
            self.entries.push(SurfaceEntry {
                surface: Surface::Cli,
                name,
                framework: "clap",
                detail: json!({"role": "program", "help": doc_first_line(&item.attrs)}),
                line_start: l,
                line_end: e,
            });
        }
        syn::visit::visit_item_struct(self, item);
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        if derives(&item.attrs, "Subcommand") {
            for variant in &item.variants {
                let name = command_name_attr(&variant.attrs)
                    .unwrap_or_else(|| kebab_case(&variant.ident.to_string()));
                let (l, e) = span_lines(variant);
                self.entries.push(SurfaceEntry {
                    surface: Surface::Cli,
                    name,
                    framework: "clap",
                    detail: json!({
                        "role": "subcommand",
                        "help": doc_first_line(&variant.attrs),
                        "parent": kebab_case(&item.ident.to_string()),
                    }),
                    line_start: l,
                    line_end: e,
                });
            }
        }
        syn::visit::visit_item_enum(self, item);
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        // actix/rocket-style attribute routes: #[get("/users/{id}")]
        for attr in &item.attrs {
            let Some(method) = attr.path().segments.last().map(|s| s.ident.to_string()) else {
                continue;
            };
            if !HTTP_METHODS.contains(&method.as_str()) {
                continue;
            }
            let Some(path) = first_str_arg(attr) else {
                continue;
            };
            if !path.starts_with('/') {
                continue;
            }
            let (l, e) = span_lines(item);
            let method = method.to_ascii_uppercase();
            self.entries.push(SurfaceEntry {
                surface: Surface::Route,
                name: format!("{method} {path}"),
                framework: "route-attribute",
                detail: json!({
                    "method": method,
                    "route_path": path,
                    "handler": item.sig.ident.to_string(),
                }),
                line_start: l,
                line_end: e,
            });
        }
        syn::visit::visit_item_fn(self, item);
    }

    fn visit_expr_call(&mut self, expr: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = expr.func.as_ref() {
            let segments: Vec<String> = p
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            let joined = segments.join("::");
            let is_env_read = joined == "var"
                || joined == "var_os"
                || joined.ends_with("env::var")
                || joined.ends_with("env::var_os");
            // Bare `var("X")` is ambiguous; require the env path to be visible.
            if is_env_read && joined.contains("env") {
                if let Some(var) = first_call_str_arg(expr) {
                    let (l, e) = span_lines(expr);
                    self.entries.push(SurfaceEntry {
                        surface: Surface::Env,
                        name: var,
                        framework: "std::env",
                        detail: json!({"access": joined}),
                        line_start: l,
                        line_end: e,
                    });
                }
            }
            if joined == "Command::new" || joined.ends_with("::Command::new") {
                if let Some(name) = first_call_str_arg(expr) {
                    let (l, e) = span_lines(expr);
                    self.entries.push(SurfaceEntry {
                        surface: Surface::Cli,
                        name,
                        framework: "clap-builder",
                        detail: json!({"role": "command"}),
                        line_start: l,
                        line_end: e,
                    });
                }
            }
        }
        syn::visit::visit_expr_call(self, expr);
    }

    fn visit_expr_method_call(&mut self, expr: &'ast syn::ExprMethodCall) {
        // axum-style: .route("/users", get(list_users))
        if expr.method == "route" {
            if let Some(syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            })) = expr.args.first()
            {
                let path = s.value();
                if path.starts_with('/') {
                    let method = expr
                        .args
                        .iter()
                        .nth(1)
                        .and_then(route_method_name)
                        .unwrap_or_else(|| "ANY".to_string());
                    let (l, e) = span_lines(expr);
                    self.entries.push(SurfaceEntry {
                        surface: Surface::Route,
                        name: format!("{method} {path}"),
                        framework: "axum",
                        detail: json!({"method": method, "route_path": path}),
                        line_start: l,
                        line_end: e,
                    });
                }
            }
        }
        syn::visit::visit_expr_method_call(self, expr);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if let Some(last) = mac.path.segments.last().map(|s| s.ident.to_string()) {
            if last == "env" || last == "option_env" {
                if let Some(var) = first_macro_str_arg(mac) {
                    let (l, e) = span_lines(mac);
                    self.entries.push(SurfaceEntry {
                        surface: Surface::Env,
                        name: var,
                        framework: "std::env",
                        detail: json!({"access": format!("{last}!")}),
                        line_start: l,
                        line_end: e,
                    });
                }
            }
        }
        syn::visit::visit_macro(self, mac);
    }
}

// ---------------------------------------------------------------------------
// syn helpers
// ---------------------------------------------------------------------------

/// 1-based source line range of a syn AST node, via proc-macro2's
/// `span-locations` feature (enabled in Cargo.toml). Outer attributes — doc
/// comments included — are part of an item's span. Shared with the extractor
/// for span-exact symbol attribution.
pub(crate) fn span_lines<T: Spanned>(node: &T) -> (usize, usize) {
    let span = node.span();
    (span.start().line.max(1), span.end().line.max(1))
}

/// Does any `#[derive(…)]` on these attrs name `trait_name` (last path segment)?
fn derives(attrs: &[syn::Attribute], trait_name: &str) -> bool {
    attrs
        .iter()
        .filter(|attr| attr.path().is_ident("derive"))
        .any(|attr| {
            let mut found = false;
            let _ = attr.parse_nested_meta(|meta| {
                if let Some(seg) = meta.path.segments.last() {
                    if seg.ident == trait_name {
                        found = true;
                    }
                }
                Ok(())
            });
            found
        })
}

/// `#[command(name = "x")]` override, if present.
fn command_name_attr(attrs: &[syn::Attribute]) -> Option<String> {
    let mut name = None;
    for attr in attrs {
        if !attr.path().is_ident("command") && !attr.path().is_ident("clap") {
            continue;
        }
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                if let Ok(value) = meta.value() {
                    if let Ok(lit) = value.parse::<syn::LitStr>() {
                        name = Some(lit.value());
                    }
                }
            } else if meta.input.peek(syn::Token![=]) {
                // Consume `key = value` pairs we don't care about so parsing
                // continues past them.
                let _ = meta.value().map(|v| v.parse::<syn::Expr>());
            }
            Ok(())
        });
    }
    name
}

/// First line of the `///` doc comment, or empty string.
fn doc_first_line(attrs: &[syn::Attribute]) -> String {
    attrs
        .iter()
        .filter(|attr| attr.path().is_ident("doc"))
        .find_map(|attr| {
            if let syn::Meta::NameValue(nv) = &attr.meta {
                if let syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) = &nv.value
                {
                    let line = s.value().trim().to_string();
                    if !line.is_empty() {
                        return Some(line);
                    }
                }
            }
            None
        })
        .unwrap_or_default()
}

/// First string literal among an attribute's arguments: the `"/path"` in
/// `#[get("/path")]` or `#[get("/path", data = "<form>")]`.
fn first_str_arg(attr: &syn::Attribute) -> Option<String> {
    let args = attr
        .parse_args_with(syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated)
        .ok()?;
    args.iter().find_map(|expr| match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) => Some(s.value()),
        _ => None,
    })
}

/// First string-literal argument of a call: the `"X"` in `env::var("X")`.
fn first_call_str_arg(expr: &syn::ExprCall) -> Option<String> {
    match expr.args.first() {
        Some(syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        })) => Some(s.value()),
        _ => None,
    }
}

/// First string literal among a macro's tokens: the `"X"` in `env!("X")`.
fn first_macro_str_arg(mac: &syn::Macro) -> Option<String> {
    let args = mac
        .parse_body_with(syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated)
        .ok()?;
    args.iter().find_map(|expr| match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) => Some(s.value()),
        _ => None,
    })
}

/// `GET`/`POST`/… from the second `.route()` argument when it is a simple
/// `get(handler)`-shaped call.
fn route_method_name(expr: &syn::Expr) -> Option<String> {
    if let syn::Expr::Call(call) = expr {
        if let syn::Expr::Path(p) = call.func.as_ref() {
            let last = p.path.segments.last()?.ident.to_string();
            if HTTP_METHODS.contains(&last.as_str()) || last == "any" {
                return Some(last.to_ascii_uppercase());
            }
        }
    }
    None
}

/// `CamelCase` → `camel-case`, matching clap's default `rename_all`.
fn kebab_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kebab_case_matches_clap_default() {
        assert_eq!(kebab_case("FeatureContext"), "feature-context");
        assert_eq!(kebab_case("Add"), "add");
        assert_eq!(kebab_case("MCP"), "m-c-p");
    }

    #[test]
    fn collects_clap_derive_env_and_axum_routes() {
        let src = r#"
            use clap::{Parser, Subcommand};

            /// Chaos CLI.
            #[derive(Parser)]
            struct Cli {
                #[command(subcommand)]
                command: Commands,
            }

            #[derive(Subcommand)]
            enum Commands {
                /// Index a repository.
                Analyze { path: String },
                #[command(name = "feature-context")]
                FeatureContext,
            }

            fn main() {
                let url = std::env::var("DATABASE_URL").unwrap();
                let model = env::var("CHAOS_EMBED_MODEL").ok();
                let built = env!("CARGO_PKG_VERSION");
                let app = axum::Router::new().route("/health", get(health));
            }
        "#;
        let syntax = syn::parse_file(src).unwrap();
        let entries = collect_rust_surface(&syntax);

        let names: Vec<(Surface, &str)> = entries
            .iter()
            .map(|e| (e.surface, e.name.as_str()))
            .collect();
        assert!(names.contains(&(Surface::Cli, "cli")));
        assert!(names.contains(&(Surface::Cli, "analyze")));
        assert!(names.contains(&(Surface::Cli, "feature-context")));
        assert!(names.contains(&(Surface::Env, "DATABASE_URL")));
        assert!(names.contains(&(Surface::Env, "CHAOS_EMBED_MODEL")));
        assert!(names.contains(&(Surface::Env, "CARGO_PKG_VERSION")));
        assert!(names.contains(&(Surface::Route, "GET /health")));

        let analyze = entries
            .iter()
            .find(|e| e.name == "analyze")
            .expect("analyze subcommand");
        assert_eq!(analyze.detail["help"], "Index a repository.");
        assert_eq!(analyze.detail["role"], "subcommand");
        assert!(analyze.line_start > 1);
    }

    #[test]
    fn attribute_routes_are_collected() {
        let src = r#"
            #[get("/users/{id}")]
            async fn user(id: web::Path<u64>) -> impl Responder {
                unimplemented!()
            }
        "#;
        let syntax = syn::parse_file(src).unwrap();
        let entries = collect_rust_surface(&syntax);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "GET /users/{id}");
        assert_eq!(entries[0].detail["handler"], "user");
    }
}
