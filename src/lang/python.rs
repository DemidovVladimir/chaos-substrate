//! rustpython-parser-based Python extraction.
//!
//! Parses a Python source file into an AST and emits the same node/edge/chunk
//! shapes that the old regex extractors produced. Gracefully degrades on parse
//! failure: a warning is printed to stderr and the file node (already registered
//! by `begin_file`) is left as-is.

use crate::{
    extractor::{
        chunk_for_node, edge, import_stable_id, is_bare_module_specifier, is_python_test_file,
        is_test_symbol, slice_lines,
    },
    lang::FileExtraction,
    models::{EdgeKind, KnowledgeNode, NodeKind},
    weights,
};
use rustpython_parser::{ast, Parse};
use serde_json::json;
use uuid::Uuid;

/// Entry point called from `extractor.rs` after `begin_file` has run.
pub(crate) fn extract(ctx: &mut FileExtraction<'_>) -> anyhow::Result<()> {
    let source = ctx.file.content.as_str();
    let stmts = match ast::Suite::parse(source, &ctx.file.path) {
        Ok(s) => s,
        Err(err) => {
            eprintln!(
                "[chaos-substrate] {}: python parse failed: {err}",
                ctx.file.path
            );
            return Ok(());
        }
    };

    walk(&stmts, ctx);
    Ok(())
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
    let line = ctx.lines.line(start_offset) as i32;
    let end_line = ctx.lines.line(end_offset) as i32;

    let kind = if is_python_test_file(&ctx.file.path) || is_test_symbol(name) {
        NodeKind::Test
    } else {
        base_kind
    };

    let stable_id = format!("{}:{}:{}", ctx.file.path, kind.as_str(), name);
    let code = slice_lines(&ctx.file.content, line as usize, end_line as usize);

    let node = KnowledgeNode {
        id: Uuid::new_v4(),
        repo_id: ctx.repo_id,
        file_id: Some(ctx.file.id),
        kind: kind.clone(),
        stable_id,
        name: name.to_string(),
        line_start: Some(line),
        line_end: Some(end_line),
        metadata: json!({
            "language": "python",
            "file": ctx.file.path,
            "python_kind": python_kind
        }),
    };

    ctx.symbol_names.entry(name.to_string()).or_insert(node.id);

    ctx.result.edges.push(edge(
        ctx.repo_id,
        ctx.file_node_id,
        node.id,
        EdgeKind::Contains,
        weights::CONTAINS_CODE,
        json!({"language": "python", "kind": python_kind}),
    ));

    ctx.result.chunks.push(chunk_for_node(
        ctx.repo_id,
        Some(ctx.file.id),
        Some(node.id),
        kind.as_str(),
        &format!(
            "Language: python\nFile: {}\nSymbol: {}\nKind: {}\nLines: {}-{}\n\n{}",
            ctx.file.path, name, python_kind, line, end_line, code
        ),
        Some(line),
        Some(end_line),
        json!({
            "symbol": name,
            "kind": kind.as_str(),
            "python_kind": python_kind,
            "file": ctx.file.path
        }),
    ));

    ctx.result.nodes.push(node);
}

/// Emit an import dependency node and its `Imports` edge.
fn emit_import(module: &str, offset: usize, ctx: &mut FileExtraction<'_>) {
    let line = ctx.lines.line(offset) as i32;
    let is_bare = is_bare_module_specifier(module);

    let node = KnowledgeNode {
        id: Uuid::new_v4(),
        repo_id: ctx.repo_id,
        file_id: if is_bare { None } else { Some(ctx.file.id) },
        kind: NodeKind::Dependency,
        stable_id: import_stable_id(ctx.file, module, is_bare),
        name: module.to_string(),
        line_start: if is_bare { None } else { Some(line) },
        line_end: if is_bare { None } else { Some(line) },
        metadata: json!({
            "module": module,
            "language": "python",
            "scope": if is_bare { "bare" } else { "relative" }
        }),
    };

    ctx.result.edges.push(edge(
        ctx.repo_id,
        ctx.file_node_id,
        node.id,
        EdgeKind::Imports,
        weights::IMPORTS_MODULE,
        json!({"file": ctx.file.path, "module": module, "line": line}),
    ));

    ctx.result.nodes.push(node);
}
