//! rustpython-parser-based Python extraction.
//!
//! Parses a Python source file into an AST and emits the same node/edge/chunk
//! shapes that the old regex extractors produced. Gracefully degrades on parse
//! failure: a warning is printed to stderr and the file node (already registered
//! by `begin_file`) is left as-is.

use crate::{
    extractor::{is_python_test_file, is_test_symbol},
    lang::FileExtraction,
    models::NodeKind,
    weights,
};
use rustpython_ast::{Expr, ExprCall, Visitor};
use rustpython_parser::{ast, Parse};

/// Entry point called from `extractor.rs` after `begin_file` has run.
pub(crate) fn extract(ctx: &mut FileExtraction<'_>) -> anyhow::Result<()> {
    let source = ctx.file.content.as_str();
    let stmts = match ast::Suite::parse(source, &ctx.file.path) {
        Ok(s) => s,
        Err(err) => {
            crate::lang::warn_parse_failure(&ctx.file.path, &format!("{err}"));
            return Ok(());
        }
    };

    walk(&stmts, ctx);
    collect_calls(&stmts, ctx);
    Ok(())
}

/// Collects function/method call sites from the parsed suite. The `Visitor`
/// recurses into nested expressions, and comments/strings are not part of the
/// AST so they cannot produce false-positive call edges.
#[derive(Default)]
struct CallCollector {
    calls: Vec<(String, u32)>,
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
        self.generic_visit_expr_call(node);
    }
}

fn collect_calls(stmts: &[ast::Stmt], ctx: &mut FileExtraction<'_>) {
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
}

// ---------------------------------------------------------------------------
// AST walk
// ---------------------------------------------------------------------------

/// Walk a list of statements, emitting symbols and imports. Recurses into
/// function and class bodies to capture methods and nested definitions.
fn walk(stmts: &[ast::Stmt], ctx: &mut FileExtraction<'_>) {
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
                walk(&f.body, ctx);
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
                walk(&f.body, ctx);
            }
            ast::Stmt::ClassDef(c) => {
                let start = c.range.start().to_usize();
                let end = c.range.end().to_usize();
                emit_symbol(c.name.as_str(), "class", NodeKind::Struct, start, end, ctx);
                walk(&c.body, ctx);
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
    );
}

/// Emit an import dependency node and its `Imports` edge.
fn emit_import(module: &str, offset: usize, ctx: &mut FileExtraction<'_>) {
    ctx.emit_dependency(module, "python", weights::IMPORTS_MODULE, offset);
}
