//! User-surface `ExtraPass`es: environment variables a file reads and HTTP
//! routes it registers, found by tree-sitter node-shape walks (never regex).
//!
//! | Language | Env vars | Routes |
//! |---|---|---|
//! | Rust | `std::env::var("X")`, `env::var("X")`, `env!("X")`, `option_env!("X")` | axum `.route("/p", get(h).post(h2))` (method from the handler wrapper, else `ANY`); actix/rocket `#[get("/p")]` |
//! | TS/JS | `process.env.X`, `process.env["X"]` | express/fastify `app.get\|post\|…("/p", …)` (any receiver, path must start with `/`); NestJS `@Get("/p")` |
//! | Python | `os.environ["X"]`, `os.environ.get("X")`, `os.getenv("X")` | Flask `@app.route("/p", methods=[…])`, FastAPI `@app.get("/p")` |
//!
//! Route names are `"GET /users/:id"` (method upper-case, `ANY` when unknown).
//! Surfaces are deduplicated per (kind, name, owner); `owner` is the enclosing
//! symbol's stable_id (for an attribute/decorator, the symbol it decorates),
//! `None` at file level.

use super::walker::{enclosing_symbol, text};
use crate::ports::extractor::{FileExtraction, FileInput, Surface, SurfaceKind};
use tree_sitter::{Node, Tree};

const METHODS: &[&str] = &["get", "post", "put", "delete", "patch", "head", "options"];

fn is_method(s: &str) -> bool {
    METHODS.contains(&s.to_ascii_lowercase().as_str())
}

fn walk(tree: &Tree, mut f: impl FnMut(Node)) {
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        f(n);
        let mut cursor = n.walk();
        let kids: Vec<Node> = n.named_children(&mut cursor).collect();
        stack.extend(kids.into_iter().rev());
    }
}

fn named_child<'t>(n: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut cursor = n.walk();
    let found = n.named_children(&mut cursor).find(|c| c.kind() == kind);
    found
}

fn first_named(n: Node) -> Option<Node> {
    n.named_child(0)
}

/// Literal value of a plain string node (Rust `string_literal`, TS/JS `string`
/// or substitution-free `template_string`, Python `string`); `None` otherwise.
fn string_value(n: Node, src: &str) -> Option<String> {
    match n.kind() {
        "string_literal" => Some(
            named_child(n, "string_content")
                .map(|c| text(c, src).to_string())
                .unwrap_or_else(|| strip_quotes(text(n, src))),
        ),
        "template_string" => {
            if named_child(n, "template_substitution").is_some() {
                None
            } else {
                Some(strip_quotes(text(n, src)))
            }
        }
        "string" => {
            let (s, e) = (named_child(n, "string_start"), named_child(n, "string_end"));
            match (s, e) {
                (Some(s), Some(e)) => {
                    if text(s, src).to_ascii_lowercase().contains('f') {
                        None
                    } else {
                        Some(src[s.end_byte()..e.start_byte()].to_string())
                    }
                }
                _ => Some(strip_quotes(text(n, src))),
            }
        }
        _ => None,
    }
}

fn strip_quotes(raw: &str) -> String {
    let t = raw.trim();
    let t = t.trim_start_matches(|c: char| c.is_ascii_alphabetic() || c == '#');
    let t = t.trim_end_matches('#');
    let inner = t
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| t.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .or_else(|| t.strip_prefix('`').and_then(|s| s.strip_suffix('`')))
        .unwrap_or(t);
    inner.to_string()
}

/// First string argument of an `arguments` / `argument_list` / `token_tree` node.
fn first_string_arg<'t>(args: Node<'t>, src: &str) -> Option<(String, Node<'t>)> {
    let mut cursor = args.walk();
    let found = args
        .named_children(&mut cursor)
        .find_map(|a| string_value(a, src).map(|v| (v, a)));
    found
}

fn route_name(method: &str, path: &str) -> String {
    let path = if path.is_empty() { "/" } else { path };
    format!("{} {}", method.to_ascii_uppercase(), path)
}

struct Collector<'a> {
    file: &'a FileInput,
    out: Vec<Surface>,
}

impl Collector<'_> {
    fn owner(&self, symbols: &[crate::ports::extractor::Symbol], at: usize) -> Option<String> {
        let id = enclosing_symbol(symbols, &self.file.path, at);
        (!id.starts_with("file:")).then_some(id)
    }

    /// Byte offset used to attribute a decorator/attribute: the declaration it precedes.
    fn decorated_at(n: Node) -> usize {
        let mut next = n.next_named_sibling();
        while let Some(s) = next {
            if !matches!(
                s.kind(),
                "attribute_item" | "decorator" | "comment" | "line_comment" | "block_comment"
            ) {
                return s.start_byte();
            }
            next = s.next_named_sibling();
        }
        n.start_byte()
    }

    fn push(&mut self, kind: SurfaceKind, name: String, node: Node, owner: Option<String>) {
        if name.trim().is_empty() {
            return;
        }
        let s = Surface {
            kind,
            name,
            line: node.start_position().row as u32 + 1,
            owner,
        };
        if !self
            .out
            .iter()
            .any(|e| e.kind == s.kind && e.name == s.name && e.owner == s.owner)
        {
            self.out.push(s);
        }
    }

    fn finish(mut self, out: &mut FileExtraction) {
        self.out.sort_by_key(|s| s.line);
        for s in self.out {
            if !out
                .surfaces
                .iter()
                .any(|e| e.kind == s.kind && e.name == s.name && e.owner == s.owner)
            {
                out.surfaces.push(s);
            }
        }
    }
}

/// HTTP methods named by an axum handler expression: `get(h)`, `get(h).post(h2)`.
fn axum_methods(mut n: Node, src: &str, acc: &mut Vec<String>) {
    loop {
        if n.kind() != "call_expression" {
            return;
        }
        let Some(f) = n.child_by_field_name("function") else {
            return;
        };
        match f.kind() {
            "identifier" if is_method(text(f, src)) => {
                acc.push(text(f, src).to_string());
                return;
            }
            "field_expression" => {
                let (Some(field), Some(value)) = (
                    f.child_by_field_name("field"),
                    f.child_by_field_name("value"),
                ) else {
                    return;
                };
                if is_method(text(field, src)) {
                    acc.push(text(field, src).to_string());
                }
                n = value;
            }
            _ => return,
        }
    }
}

/// Rust surfaces (see module docs).
pub fn rust(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let mut c = Collector { file, out: vec![] };
    let symbols = out.symbols.clone();
    walk(tree, |n| match n.kind() {
        "call_expression" => {
            let (Some(f), Some(args)) = (
                n.child_by_field_name("function"),
                n.child_by_field_name("arguments"),
            ) else {
                return;
            };
            let ftext = text(f, src);
            if f.kind() == "scoped_identifier"
                && (ftext.ends_with("env::var") || ftext.ends_with("env::var_os"))
            {
                if let Some((v, node)) = first_string_arg(args, src) {
                    let owner = c.owner(&symbols, n.start_byte());
                    c.push(SurfaceKind::EnvVar, v, node, owner);
                }
            } else if f.kind() == "field_expression"
                && f.child_by_field_name("field")
                    .is_some_and(|x| text(x, src) == "route")
            {
                let Some((path, pnode)) = first_string_arg(args, src) else {
                    return;
                };
                let mut methods = Vec::new();
                let mut cursor = args.walk();
                for a in args.named_children(&mut cursor).skip(1) {
                    axum_methods(a, src, &mut methods);
                }
                if methods.is_empty() {
                    methods.push("ANY".into());
                }
                methods.reverse();
                let owner = c.owner(&symbols, n.start_byte());
                for m in methods {
                    c.push(
                        SurfaceKind::HttpRoute,
                        route_name(&m, &path),
                        pnode,
                        owner.clone(),
                    );
                }
            }
        }
        "macro_invocation" => {
            let Some(m) = n.child_by_field_name("macro") else {
                return;
            };
            if !matches!(text(m, src), "env" | "option_env") {
                return;
            }
            if let Some((v, node)) =
                named_child(n, "token_tree").and_then(|t| first_string_arg(t, src))
            {
                let owner = c.owner(&symbols, n.start_byte());
                c.push(SurfaceKind::EnvVar, v, node, owner);
            }
        }
        "attribute_item" => {
            let Some(attr) = named_child(n, "attribute") else {
                return;
            };
            let Some(ident) = first_named(attr).filter(|i| i.kind() == "identifier") else {
                return;
            };
            if !is_method(text(ident, src)) {
                return;
            }
            let Some((path, pnode)) = attr
                .child_by_field_name("arguments")
                .or_else(|| named_child(attr, "token_tree"))
                .and_then(|t| first_string_arg(t, src))
            else {
                return;
            };
            let owner = c.owner(&symbols, Collector::decorated_at(n));
            c.push(
                SurfaceKind::HttpRoute,
                route_name(text(ident, src), &path),
                pnode,
                owner,
            );
        }
        _ => {}
    });
    c.finish(out);
}

/// TypeScript / JavaScript surfaces (see module docs).
pub fn typescript(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let mut c = Collector { file, out: vec![] };
    let symbols = out.symbols.clone();
    let is_process_env = |n: Node| {
        n.kind() == "member_expression"
            && n.child_by_field_name("object")
                .is_some_and(|o| text(o, src) == "process")
            && n.child_by_field_name("property")
                .is_some_and(|p| text(p, src) == "env")
    };
    walk(tree, |n| match n.kind() {
        "member_expression" => {
            let (Some(obj), Some(prop)) = (
                n.child_by_field_name("object"),
                n.child_by_field_name("property"),
            ) else {
                return;
            };
            if is_process_env(obj) {
                let owner = c.owner(&symbols, n.start_byte());
                c.push(
                    SurfaceKind::EnvVar,
                    text(prop, src).to_string(),
                    prop,
                    owner,
                );
            }
        }
        "subscript_expression" => {
            let (Some(obj), Some(idx)) = (
                n.child_by_field_name("object"),
                n.child_by_field_name("index"),
            ) else {
                return;
            };
            if is_process_env(obj) {
                if let Some(v) = string_value(idx, src) {
                    let owner = c.owner(&symbols, n.start_byte());
                    c.push(SurfaceKind::EnvVar, v, idx, owner);
                }
            }
        }
        "call_expression" => {
            let (Some(f), Some(args)) = (
                n.child_by_field_name("function"),
                n.child_by_field_name("arguments"),
            ) else {
                return;
            };
            if f.kind() != "member_expression" || args.kind() != "arguments" {
                return;
            }
            let Some(prop) = f.child_by_field_name("property") else {
                return;
            };
            let method = text(prop, src);
            if !is_method(method) && method != "all" {
                return;
            }
            let Some((path, pnode)) = first_string_arg(args, src) else {
                return;
            };
            if !path.starts_with('/') {
                return;
            }
            let m = if method == "all" { "ANY" } else { method };
            let owner = c.owner(&symbols, n.start_byte());
            c.push(SurfaceKind::HttpRoute, route_name(m, &path), pnode, owner);
        }
        "decorator" => {
            let Some(call) = first_named(n).filter(|x| x.kind() == "call_expression") else {
                return;
            };
            let (Some(f), Some(args)) = (
                call.child_by_field_name("function"),
                call.child_by_field_name("arguments"),
            ) else {
                return;
            };
            let name = text(f, src);
            if f.kind() != "identifier"
                || !is_method(name)
                || name.chars().next().is_none_or(|ch| ch.is_ascii_lowercase())
            {
                return;
            }
            let (path, pnode) = first_string_arg(args, src).unwrap_or((String::new(), args));
            let owner = c.owner(&symbols, Collector::decorated_at(n));
            c.push(
                SurfaceKind::HttpRoute,
                route_name(name, &path),
                pnode,
                owner,
            );
        }
        _ => {}
    });
    c.finish(out);
}

/// Python surfaces (see module docs).
pub fn python(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let mut c = Collector { file, out: vec![] };
    let symbols = out.symbols.clone();
    walk(tree, |n| match n.kind() {
        "subscript" => {
            let (Some(v), Some(sub)) = (
                n.child_by_field_name("value"),
                n.child_by_field_name("subscript"),
            ) else {
                return;
            };
            if text(v, src) != "os.environ" {
                return;
            }
            if let Some(name) = string_value(sub, src) {
                let owner = c.owner(&symbols, n.start_byte());
                c.push(SurfaceKind::EnvVar, name, sub, owner);
            }
        }
        "call" => {
            let (Some(f), Some(args)) = (
                n.child_by_field_name("function"),
                n.child_by_field_name("arguments"),
            ) else {
                return;
            };
            if matches!(
                text(f, src),
                "os.environ.get" | "os.getenv" | "environ.get" | "getenv"
            ) {
                if let Some((name, node)) = first_string_arg(args, src) {
                    let owner = c.owner(&symbols, n.start_byte());
                    c.push(SurfaceKind::EnvVar, name, node, owner);
                }
            }
        }
        "decorator" => {
            let Some(call) = first_named(n).filter(|x| x.kind() == "call") else {
                return;
            };
            let (Some(f), Some(args)) = (
                call.child_by_field_name("function"),
                call.child_by_field_name("arguments"),
            ) else {
                return;
            };
            if f.kind() != "attribute" {
                return;
            }
            let Some(attr) = f.child_by_field_name("attribute") else {
                return;
            };
            let attr = text(attr, src);
            let Some((path, pnode)) = first_string_arg(args, src) else {
                return;
            };
            let methods: Vec<String> = if attr == "route" {
                let listed: Vec<String> = named_child(args, "keyword_argument")
                    .filter(|k| {
                        k.child_by_field_name("name")
                            .is_some_and(|x| text(x, src) == "methods")
                    })
                    .and_then(|k| k.child_by_field_name("value"))
                    .map(|list| {
                        let mut cursor = list.walk();
                        list.named_children(&mut cursor)
                            .filter_map(|s| string_value(s, src))
                            .collect()
                    })
                    .unwrap_or_default();
                if listed.is_empty() {
                    vec!["ANY".into()]
                } else {
                    listed
                }
            } else if is_method(attr) {
                vec![attr.to_string()]
            } else {
                return;
            };
            let owner = c.owner(&symbols, Collector::decorated_at(n));
            for m in methods {
                c.push(
                    SurfaceKind::HttpRoute,
                    route_name(&m, &path),
                    pnode,
                    owner.clone(),
                );
            }
        }
        _ => {}
    });
    c.finish(out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::Language;

    fn run(path: &str, lang: Language, src: &str) -> Vec<(SurfaceKind, String, Option<String>)> {
        let file = FileInput {
            path: path.into(),
            language: lang,
            content: src.into(),
        };
        let spec = super::super::lang::spec_for(lang, path).unwrap();
        super::super::walker::extract(spec, &file)
            .surfaces
            .into_iter()
            .map(|s| (s.kind, s.name, s.owner))
            .collect()
    }

    fn env(name: &str, owner: Option<&str>) -> (SurfaceKind, String, Option<String>) {
        (SurfaceKind::EnvVar, name.into(), owner.map(String::from))
    }

    fn route(name: &str, owner: Option<&str>) -> (SurfaceKind, String, Option<String>) {
        (SurfaceKind::HttpRoute, name.into(), owner.map(String::from))
    }

    #[test]
    fn rust_env_and_routes() {
        let src = r#"
use axum::{routing::get, Router};
static V: &str = env!("BUILD_VERSION");

fn config() -> String {
    let a = std::env::var("DATABASE_URL").unwrap();
    let b = env::var("DATABASE_URL").unwrap_or_default();
    let c = option_env!("FEATURE_FLAG");
    a + &b
}

fn app() -> Router {
    Router::new()
        .route("/users", get(list).post(create))
        .route("/users/:id", get(one))
        .route("/health", handler)
}

#[get("/actix/{id}")]
async fn actix_one() {}
"#;
        let got = run("src/main.rs", Language::Rust, src);
        assert_eq!(
            got,
            [
                env("BUILD_VERSION", None),
                env("DATABASE_URL", Some("rust:src/main.rs:function:config")),
                env("FEATURE_FLAG", Some("rust:src/main.rs:function:config")),
                route("GET /users", Some("rust:src/main.rs:function:app")),
                route("POST /users", Some("rust:src/main.rs:function:app")),
                route("GET /users/:id", Some("rust:src/main.rs:function:app")),
                route("ANY /health", Some("rust:src/main.rs:function:app")),
                route(
                    "GET /actix/{id}",
                    Some("rust:src/main.rs:function:actix_one")
                ),
            ]
        );
    }

    #[test]
    fn typescript_env_and_routes() {
        let src = r#"
import express from 'express';
const port = process.env.PORT ?? process.env["PORT"];

export function start() {
  const url = process.env.DATABASE_URL;
  const app = express();
  app.get('/users/:id', (req, res) => res.json(req.params));
  app.post("/users", create);
  app.all('/any', h);
  axios.get("https://example.com/x");
  return app;
}

@Controller('cats')
export class CatsController {
  @Get('/cats/:id')
  findOne() { return process.env.CAT; }

  @Post()
  create() {}
}
"#;
        let got = run("src/server.ts", Language::TypeScript, src);
        assert_eq!(
            got,
            [
                env("PORT", None),
                env(
                    "DATABASE_URL",
                    Some("typescript:src/server.ts:function:start")
                ),
                route(
                    "GET /users/:id",
                    Some("typescript:src/server.ts:function:start")
                ),
                route(
                    "POST /users",
                    Some("typescript:src/server.ts:function:start")
                ),
                route("ANY /any", Some("typescript:src/server.ts:function:start")),
                route(
                    "GET /cats/:id",
                    Some("typescript:src/server.ts:method:CatsController::findOne")
                ),
                env(
                    "CAT",
                    Some("typescript:src/server.ts:method:CatsController::findOne")
                ),
                route(
                    "POST /",
                    Some("typescript:src/server.ts:method:CatsController::create")
                ),
            ]
        );
    }

    #[test]
    fn javascript_express_routes() {
        let src = "const app = require('express')();\napp.delete('/items/:id', del);\nconst key = process.env.API_KEY;\n";
        let got = run("app.js", Language::JavaScript, src);
        assert_eq!(
            got,
            [route("DELETE /items/:id", None), env("API_KEY", None)]
        );
    }

    #[test]
    fn python_env_and_routes() {
        let src = r#"
import os
from flask import Flask

DEBUG = os.environ["DEBUG"]

def settings():
    a = os.environ.get("DATABASE_URL")
    b = os.getenv("SECRET_KEY", "x")
    return a, b

@app.route("/users", methods=["GET", "POST"])
def users():
    return []

@app.route("/ping")
def ping():
    return "pong"

class Api:
    @router.get("/items/{item_id}")
    def item(self, item_id):
        return os.environ["ITEMS"]
"#;
        let got = run("app/main.py", Language::Python, src);
        assert_eq!(
            got,
            [
                env("DEBUG", None),
                env("DATABASE_URL", Some("python:app/main.py:function:settings")),
                env("SECRET_KEY", Some("python:app/main.py:function:settings")),
                route("GET /users", Some("python:app/main.py:function:users")),
                route("POST /users", Some("python:app/main.py:function:users")),
                route("ANY /ping", Some("python:app/main.py:function:ping")),
                route(
                    "GET /items/{item_id}",
                    Some("python:app/main.py:method:Api::item")
                ),
                env("ITEMS", Some("python:app/main.py:method:Api::item")),
            ]
        );
    }
}
