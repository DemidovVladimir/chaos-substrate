//! rustpython-parser-based Python extraction.
//!
//! Parses a Python source file into an AST and emits the same node/edge/chunk
//! shapes that the old regex extractors produced. A parse failure propagates
//! as `Err`: the extractor's language wrapper warns and degrades to a
//! whole-file fallback chunk (the file node is already registered by
//! `begin_file`).

use crate::{
    extractor::{is_python_test_file, is_test_symbol},
    lang::FileExtraction,
    models::NodeKind,
    user_surface::{Surface, SurfaceEntry},
    weights,
};
use rustpython_ast::{Expr, ExprCall, ExprSubscript, Visitor};
use rustpython_parser::{ast, Parse};
use serde_json::json;

/// Entry point called from `extractor.rs` after `begin_file` has run.
pub(crate) fn extract(ctx: &mut FileExtraction<'_>) -> anyhow::Result<()> {
    let source = ctx.file.content.as_str();
    let stmts =
        ast::Suite::parse(source, &ctx.file.path).map_err(|err| anyhow::anyhow!("{err}"))?;

    let mut surface: Vec<SurfaceEntry> = Vec::new();
    walk(&stmts, ctx, &mut surface);
    collect_calls(&stmts, ctx, &mut surface);
    ctx.emit_user_surface(surface);
    Ok(())
}

/// Collects function/method call sites from the parsed suite — and, on the
/// way, the user-surface reads/definitions only visible at expression level:
/// `os.environ["X"]` / `os.environ.get("X")` / `os.getenv("X")` and argparse
/// `add_parser("name", help=…)` subcommands. The `Visitor` recurses into
/// nested expressions, and comments/strings are not part of the AST so they
/// cannot produce false-positive call edges.
#[derive(Default)]
struct CallCollector {
    calls: Vec<(String, u32)>,
    /// (VAR, access, offset)
    env_reads: Vec<(String, &'static str, u32)>,
    /// (name, help, start, end)
    cli_commands: Vec<(String, String, u32, u32)>,
    /// (document text, call span) — GraphQL documents embedded as
    /// `gql("…")` / `gql("""…""")` string-literal calls, handed to
    /// [`crate::lang::graphql::extract_embedded_document`] after the walk.
    graphql_docs: Vec<(String, u32, u32)>,
}

impl Visitor for CallCollector {
    fn visit_expr_call(&mut self, node: ExprCall) {
        let name = match node.func.as_ref() {
            Expr::Name(n) => Some(n.id.to_string()),
            Expr::Attribute(a) => Some(a.attr.to_string()),
            _ => None,
        };
        if let Some(name) = name {
            self.calls
                .push((name, node.range.start().to_usize() as u32));
        }
        self.collect_surface_call(&node);
        self.collect_graphql_call(&node);
        self.generic_visit_expr_call(node);
    }

    fn visit_expr_subscript(&mut self, node: ExprSubscript) {
        // os.environ["X"]
        if is_os_environ(&node.value) {
            if let Some(var) = const_str(&node.slice) {
                self.env_reads
                    .push((var, "os.environ[…]", node.range.start().to_usize() as u32));
            }
        }
        self.generic_visit_expr_subscript(node);
    }
}

impl CallCollector {
    fn collect_surface_call(&mut self, node: &ExprCall) {
        let Expr::Attribute(func) = node.func.as_ref() else {
            return;
        };
        let attr = func.attr.as_str();
        let start = node.range.start().to_usize() as u32;

        // os.getenv("X") / os.environ.get("X")
        let access = if attr == "getenv" && is_name(&func.value, "os") {
            Some("os.getenv")
        } else if attr == "get" && is_os_environ(&func.value) {
            Some("os.environ.get")
        } else {
            None
        };
        if let Some(access) = access {
            if let Some(var) = node.args.first().and_then(const_str) {
                self.env_reads.push((var, access, start));
            }
            return;
        }

        // argparse: subparsers.add_parser("name", help="…")
        if attr == "add_parser" {
            if let Some(name) = node.args.first().and_then(const_str) {
                let help = kwarg_str(node, "help").unwrap_or_default();
                self.cli_commands
                    .push((name, help, start, node.range.end().to_usize() as u32));
            }
        }
    }

    /// `gql("query { … }")` / `gql("""…""")` (graphql-core / gql-client
    /// convention) — the string literal IS a GraphQL document. The whole call
    /// span is the host anchor.
    fn collect_graphql_call(&mut self, node: &ExprCall) {
        if !matches!(node.func.as_ref(), Expr::Name(n) if n.id.as_str() == "gql") {
            return;
        }
        let Some(text) = node.args.first().and_then(const_str) else {
            return;
        };
        self.graphql_docs.push((
            text,
            node.range.start().to_usize() as u32,
            node.range.end().to_usize() as u32,
        ));
    }
}

fn collect_calls(
    stmts: &[ast::Stmt],
    ctx: &mut FileExtraction<'_>,
    surface: &mut Vec<SurfaceEntry>,
) {
    let mut cc = CallCollector::default();
    // rustpython's Visitor::visit_stmt consumes Stmt by value
    for stmt in stmts.iter().cloned() {
        cc.visit_stmt(stmt);
    }
    for (callee, off) in cc.calls {
        ctx.calls.push(crate::lang::CallSite {
            file: ctx.file.path.clone(),
            callee,
            line: ctx.lines.line(off as usize) as i32,
        });
    }
    for (var, access, off) in cc.env_reads {
        let line = ctx.lines.line(off as usize);
        surface.push(SurfaceEntry {
            surface: Surface::Env,
            name: var,
            framework: "os.environ",
            detail: json!({"access": access}),
            line_start: line,
            line_end: line,
        });
    }
    for (name, help, start, end) in cc.cli_commands {
        surface.push(SurfaceEntry {
            surface: Surface::Cli,
            name,
            framework: "argparse",
            detail: json!({"role": "subcommand", "help": help}),
            line_start: ctx.lines.line(start as usize),
            line_end: ctx.lines.line(end as usize),
        });
    }
    // Embedded GraphQL documents: nodes anchor to the host call span, chunk
    // bodies carry the GraphQL text.
    for (text, start, end) in cc.graphql_docs {
        crate::lang::graphql::extract_embedded_document(
            ctx,
            &text,
            (start as usize, end as usize),
            "gql-call",
        );
    }
}

const ROUTE_DECORATOR_METHODS: &[&str] = &["get", "post", "put", "patch", "delete", "route"];

/// Read a function's decorators for user-surface definitions:
/// `@app.get("/x")` / `@router.post("/x")` (FastAPI-shaped),
/// `@app.route("/x", methods=["GET"])` (Flask-shaped), and
/// `@cli.command()` / `@click.command` (click).
fn collect_decorator_surface(
    func_name: &str,
    decorators: &[Expr],
    start: usize,
    end: usize,
    ctx: &FileExtraction<'_>,
    surface: &mut Vec<SurfaceEntry>,
) {
    let line_start = ctx.lines.line(start);
    let line_end = ctx.lines.line(end);
    for dec in decorators {
        // `@cli.command` without parens
        if let Expr::Attribute(a) = dec {
            if a.attr.as_str() == "command" {
                surface.push(click_command(func_name, None, line_start, line_end));
            }
            continue;
        }
        let Expr::Call(call) = dec else {
            continue;
        };
        let Expr::Attribute(func) = call.func.as_ref() else {
            continue;
        };
        let attr = func.attr.as_str();

        if attr == "command" {
            let explicit = call.args.first().and_then(const_str);
            surface.push(click_command(func_name, explicit, line_start, line_end));
            continue;
        }

        if !ROUTE_DECORATOR_METHODS.contains(&attr) {
            continue;
        }
        let Some(path) = call.args.first().and_then(const_str) else {
            continue;
        };
        if !path.starts_with('/') {
            continue;
        }
        let (method, framework) = if attr == "route" {
            (flask_methods(call), "flask-like")
        } else {
            (attr.to_ascii_uppercase(), "fastapi-like")
        };
        surface.push(SurfaceEntry {
            surface: Surface::Route,
            name: format!("{method} {path}"),
            framework,
            detail: json!({
                "method": method,
                "route_path": path,
                "handler": func_name,
            }),
            line_start,
            line_end,
        });
    }
}

/// `methods=["GET", "POST"]` of a Flask `@app.route`, joined as `GET|POST`;
/// Flask's default when absent is GET.
fn flask_methods(call: &ExprCall) -> String {
    let Some(kw) = call
        .keywords
        .iter()
        .find(|kw| kw.arg.as_ref().map(|a| a.as_str()) == Some("methods"))
    else {
        return "GET".to_string();
    };
    let Expr::List(list) = &kw.value else {
        return "GET".to_string();
    };
    let methods: Vec<String> = list
        .elts
        .iter()
        .filter_map(const_str)
        .map(|m| m.to_ascii_uppercase())
        .collect();
    if methods.is_empty() {
        "GET".to_string()
    } else {
        methods.join("|")
    }
}

/// A click command entry: click's default name is the function name with
/// underscores replaced by hyphens.
fn click_command(
    func_name: &str,
    explicit: Option<String>,
    line_start: usize,
    line_end: usize,
) -> SurfaceEntry {
    let name = explicit.unwrap_or_else(|| func_name.replace('_', "-"));
    SurfaceEntry {
        surface: Surface::Cli,
        name,
        framework: "click",
        detail: json!({"role": "command", "handler": func_name}),
        line_start,
        line_end,
    }
}

// ---------------------------------------------------------------------------
// Expression helpers
// ---------------------------------------------------------------------------

/// Is this expression the bare name `name`?
fn is_name(expr: &Expr, name: &str) -> bool {
    matches!(expr, Expr::Name(n) if n.id.as_str() == name)
}

/// Is this expression literally `os.environ`?
fn is_os_environ(expr: &Expr) -> bool {
    matches!(expr, Expr::Attribute(a) if a.attr.as_str() == "environ" && is_name(&a.value, "os"))
}

/// String constant value, if the expression is one.
fn const_str(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Constant(c) => c.value.as_str().map(|s| s.to_string()),
        _ => None,
    }
}

/// String value of the `name=…` keyword argument, if present.
fn kwarg_str(call: &ExprCall, name: &str) -> Option<String> {
    call.keywords
        .iter()
        .find(|kw| kw.arg.as_ref().map(|a| a.as_str()) == Some(name))
        .and_then(|kw| const_str(&kw.value))
}

// ---------------------------------------------------------------------------
// AST walk
// ---------------------------------------------------------------------------

/// Walk a list of statements, emitting symbols and imports. Recurses into
/// function and class bodies to capture methods and nested definitions, and
/// reads function decorators for route registrations (`@app.get("/x")`,
/// `@app.route("/x", methods=[…])`) and click commands (`@cli.command()`).
fn walk(stmts: &[ast::Stmt], ctx: &mut FileExtraction<'_>, surface: &mut Vec<SurfaceEntry>) {
    for stmt in stmts {
        match stmt {
            ast::Stmt::FunctionDef(f) => {
                let start = f.range.start().to_usize();
                let end = f.range.end().to_usize();
                emit_symbol(
                    f.name.as_str(),
                    "function",
                    NodeKind::Function,
                    start,
                    end,
                    ctx,
                );
                collect_decorator_surface(
                    f.name.as_str(),
                    &f.decorator_list,
                    start,
                    end,
                    ctx,
                    surface,
                );
                walk(&f.body, ctx, surface);
            }
            ast::Stmt::AsyncFunctionDef(f) => {
                let start = f.range.start().to_usize();
                let end = f.range.end().to_usize();
                emit_symbol(
                    f.name.as_str(),
                    "function",
                    NodeKind::Function,
                    start,
                    end,
                    ctx,
                );
                collect_decorator_surface(
                    f.name.as_str(),
                    &f.decorator_list,
                    start,
                    end,
                    ctx,
                    surface,
                );
                walk(&f.body, ctx, surface);
            }
            ast::Stmt::ClassDef(c) => {
                let start = c.range.start().to_usize();
                let end = c.range.end().to_usize();
                emit_symbol(c.name.as_str(), "class", NodeKind::Struct, start, end, ctx);
                walk(&c.body, ctx, surface);
            }
            ast::Stmt::Import(i) => {
                let offset = i.range.start().to_usize();
                for alias in &i.names {
                    emit_import(alias.name.as_str(), offset, ctx);
                }
            }
            ast::Stmt::ImportFrom(i) => {
                let offset = i.range.start().to_usize();
                let level = i.level.as_ref().map(|l| l.to_usize()).unwrap_or(0);
                let base = i.module.as_ref().map(|m| m.as_str()).unwrap_or("");
                let module = if level > 0 {
                    format!("{}{}", ".".repeat(level), base)
                } else {
                    base.to_string()
                };
                if !module.is_empty() {
                    emit_import(&module, offset, ctx);
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Emitters
// ---------------------------------------------------------------------------

/// Emit a function/class symbol node, its `Contains` edge and its chunk.
fn emit_symbol(
    name: &str,
    python_kind: &str,
    base_kind: NodeKind,
    start_offset: usize,
    end_offset: usize,
    ctx: &mut FileExtraction<'_>,
) {
    let kind = if is_python_test_file(&ctx.file.path) || is_test_symbol(name) {
        NodeKind::Test
    } else {
        base_kind
    };

    let stable_id = format!("{}:{}:{}", ctx.file.path, kind.as_str(), name);

    ctx.emit_code_symbol(
        name,
        kind.clone(),
        stable_id,
        "python",
        weights::CONTAINS_CODE,
        serde_json::json!({"language": "python", "kind": python_kind}),
        serde_json::json!({"language": "python", "file": ctx.file.path, "python_kind": python_kind}),
        python_kind,
        start_offset,
        end_offset,
        serde_json::json!({
            "symbol": name,
            "kind": kind.as_str(),
            "python_kind": python_kind,
            "file": ctx.file.path
        }),
        None,
    );
}

/// Emit an import dependency node and its `Imports` edge.
fn emit_import(module: &str, offset: usize, ctx: &mut FileExtraction<'_>) {
    ctx.emit_dependency(module, "python", weights::IMPORTS_MODULE, offset);
}
