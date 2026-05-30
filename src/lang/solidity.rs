//! solang-parser-based Solidity extraction.
//!
//! Parses a Solidity source file into a `SourceUnit` AST and emits the same
//! node/edge/chunk shapes that the old regex extractors produced. Gracefully
//! degrades on parse failure: a warning is printed to stderr and the file node
//! (already registered by `begin_file`) is left as-is.

use crate::{
    extractor::{chunk_for_node, edge, slice_lines},
    lang::FileExtraction,
    models::{EdgeKind, KnowledgeNode, NodeKind},
    weights,
};
use serde_json::json;
use solang_parser::pt::{
    CatchClause, ContractPart, ContractTy, Expression, FunctionTy, Import, ImportPath,
    SourceUnitPart, Statement,
};
use uuid::Uuid;

/// Entry point called from `extractor.rs` after `begin_file` has run.
pub(crate) fn extract(ctx: &mut FileExtraction<'_>) -> anyhow::Result<()> {
    let source = ctx.file.content.as_str();
    let (unit, _comments) = match solang_parser::parse(source, 0) {
        Ok(result) => result,
        Err(diags) => {
            crate::lang::warn_parse_failure(
                &ctx.file.path,
                &format!("{} diagnostics", diags.len()),
            );
            return Ok(());
        }
    };

    for part in &unit.0 {
        match part {
            SourceUnitPart::ContractDefinition(c) => {
                emit_contract(c, ctx);
            }
            SourceUnitPart::ImportDirective(import) => {
                emit_import_directive(import, ctx);
            }
            SourceUnitPart::FunctionDefinition(f) => {
                // File-level / free function — emit as a member with no
                // enclosing contract, matching old regex behaviour.
                emit_free_function(f, ctx);
            }
            _ => {}
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

fn emit_contract(c: &solang_parser::pt::ContractDefinition, ctx: &mut FileExtraction<'_>) {
    let Some(ident) = &c.name else { return };
    let name = &ident.name;

    let kind_text = match &c.ty {
        ContractTy::Interface(_) => "interface",
        ContractTy::Library(_) => "library",
        ContractTy::Abstract(_) | ContractTy::Contract(_) => "contract",
    };

    let node_kind = match kind_text {
        "interface" => NodeKind::Trait,
        "library" => NodeKind::Module,
        _ => NodeKind::Struct,
    };

    let l = ctx.lines.line(c.loc.start());
    let e = ctx.lines.line(c.loc.end());
    let code = slice_lines(&ctx.file.content, l, e);
    let path = &ctx.file.path.clone();

    let stable_id = format!("{}:solidity:{}:{}", path, kind_text, name);

    let node = KnowledgeNode {
        id: Uuid::new_v4(),
        repo_id: ctx.repo_id,
        file_id: Some(ctx.file.id),
        kind: node_kind.clone(),
        stable_id,
        name: name.clone(),
        line_start: Some(l as i32),
        line_end: Some(e as i32),
        metadata: json!({
            "language": "solidity",
            "file": path,
            "solidity_kind": kind_text
        }),
    };

    ctx.symbol_names.entry(name.clone()).or_insert(node.id);

    ctx.result.edges.push(edge(
        ctx.repo_id,
        ctx.file_node_id,
        node.id,
        EdgeKind::Defines,
        weights::DEFINES_SYMBOL,
        json!({"language": "solidity", "kind": kind_text}),
    ));

    ctx.result.chunks.push(chunk_for_node(
        ctx.repo_id,
        Some(ctx.file.id),
        Some(node.id),
        node_kind.as_str(),
        &format!(
            "Language: solidity\nFile: {}\nSymbol: {}\nSolidity kind: {}\nLines: {}-{}\n\n{}",
            path, name, kind_text, l, e, code
        ),
        Some(l as i32),
        Some(e as i32),
        json!({
            "symbol": name,
            "kind": node_kind.as_str(),
            "solidity_kind": kind_text,
            "file": path
        }),
    ));

    let contract_node_id = node.id;
    ctx.result.nodes.push(node);

    // Inheritance
    for base in &c.base {
        emit_inheritance(&base.name.to_string(), contract_node_id, ctx);
    }

    // Members
    for part in &c.parts {
        match part {
            ContractPart::FunctionDefinition(f) => {
                emit_member_function(f, ctx);
            }
            ContractPart::EventDefinition(e_def) => {
                emit_event(e_def, ctx);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Members
// ---------------------------------------------------------------------------

fn emit_member_function(f: &solang_parser::pt::FunctionDefinition, ctx: &mut FileExtraction<'_>) {
    let (solidity_kind, node_kind, name) = match &f.ty {
        FunctionTy::Function => {
            let Some(ident) = &f.name else { return };
            ("function", NodeKind::Function, ident.name.clone())
        }
        FunctionTy::Constructor => ("constructor", NodeKind::Function, "constructor".to_string()),
        FunctionTy::Fallback => ("fallback", NodeKind::Function, "fallback".to_string()),
        FunctionTy::Receive => ("fallback", NodeKind::Function, "receive".to_string()),
        FunctionTy::Modifier => {
            let Some(ident) = &f.name else { return };
            ("modifier", NodeKind::Concept, ident.name.clone())
        }
    };

    emit_member(
        name,
        solidity_kind,
        node_kind,
        f.loc.start(),
        f.loc.end(),
        ctx,
    );

    collect_function_calls(f, ctx);
}

fn emit_free_function(f: &solang_parser::pt::FunctionDefinition, ctx: &mut FileExtraction<'_>) {
    // Only emit named free functions (FunctionTy::Function with a name).
    if f.ty != FunctionTy::Function {
        return;
    }
    let Some(ident) = &f.name else { return };
    emit_member(
        ident.name.clone(),
        "function",
        NodeKind::Function,
        f.loc.start(),
        f.loc.end(),
        ctx,
    );

    collect_function_calls(f, ctx);
}

// ---------------------------------------------------------------------------
// Call sites
// ---------------------------------------------------------------------------

/// Walk a function body for call sites and push them to `ctx.calls` for later
/// resolution. Operating on the parsed AST means comments and strings cannot
/// produce false-positive call edges.
fn collect_function_calls(f: &solang_parser::pt::FunctionDefinition, ctx: &mut FileExtraction<'_>) {
    let Some(body) = &f.body else { return };
    let mut out: Vec<(String, usize)> = Vec::new();
    walk_call_stmt(body, &mut out);
    for (callee, off) in out {
        ctx.calls.push(crate::lang::CallSite {
            file: ctx.file.path.clone(),
            callee,
            line: ctx.lines.line(off) as i32,
        });
    }
}

fn callee_name(e: &Expression) -> Option<String> {
    match e {
        Expression::Variable(id) => Some(id.name.clone()),
        Expression::MemberAccess(_, _, id) => Some(id.name.clone()),
        _ => None,
    }
}

fn walk_call_expr(e: &Expression, out: &mut Vec<(String, usize)>) {
    use Expression::*;
    match e {
        FunctionCall(loc, func, args) => {
            if let Some(n) = callee_name(func) {
                out.push((n, loc.start()));
            }
            walk_call_expr(func, out);
            for a in args {
                walk_call_expr(a, out);
            }
        }
        FunctionCallBlock(_, func, _) => walk_call_expr(func, out),
        NamedFunctionCall(loc, func, args) => {
            if let Some(n) = callee_name(func) {
                out.push((n, loc.start()));
            }
            for a in args {
                walk_call_expr(&a.expr, out);
            }
        }
        MemberAccess(_, base, _) => walk_call_expr(base, out),
        Assign(_, l, r)
        | Add(_, l, r)
        | Subtract(_, l, r)
        | Multiply(_, l, r)
        | Divide(_, l, r)
        | Equal(_, l, r)
        | More(_, l, r)
        | Less(_, l, r)
        | And(_, l, r)
        | Or(_, l, r) => {
            walk_call_expr(l, out);
            walk_call_expr(r, out);
        }
        ArraySubscript(_, b, idx) => {
            walk_call_expr(b, out);
            if let Some(i) = idx {
                walk_call_expr(i, out);
            }
        }
        Parenthesis(_, inner) | Not(_, inner) | Negate(_, inner) => walk_call_expr(inner, out),
        _ => {}
    }
}

fn walk_call_stmt(s: &Statement, out: &mut Vec<(String, usize)>) {
    use Statement::*;
    match s {
        Block { statements, .. } => statements.iter().for_each(|s| walk_call_stmt(s, out)),
        Expression(_, e) => walk_call_expr(e, out),
        VariableDefinition(_, _, Some(e)) => walk_call_expr(e, out),
        If(_, c, t, els) => {
            walk_call_expr(c, out);
            walk_call_stmt(t, out);
            if let Some(e) = els {
                walk_call_stmt(e, out);
            }
        }
        While(_, c, b) => {
            walk_call_expr(c, out);
            walk_call_stmt(b, out);
        }
        Return(_, Some(e)) => walk_call_expr(e, out),
        Emit(_, e) => walk_call_expr(e, out),
        For(_, init, cond, post, body) => {
            if let Some(s) = init {
                walk_call_stmt(s, out);
            }
            if let Some(e) = cond {
                walk_call_expr(e, out);
            }
            if let Some(e) = post {
                walk_call_expr(e, out);
            }
            if let Some(s) = body {
                walk_call_stmt(s, out);
            }
        }
        DoWhile(_, body, cond) => {
            walk_call_stmt(body, out);
            walk_call_expr(cond, out);
        }
        Revert(_, _, args) => {
            for e in args {
                walk_call_expr(e, out);
            }
        }
        Try(_, e, returns, catches) => {
            walk_call_expr(e, out);
            if let Some((_, body)) = returns {
                walk_call_stmt(body, out);
            }
            for c in catches {
                match c {
                    CatchClause::Simple(_, _, body) => walk_call_stmt(body, out),
                    CatchClause::Named(_, _, _, body) => walk_call_stmt(body, out),
                }
            }
        }
        _ => {}
    }
}

fn emit_event(e_def: &solang_parser::pt::EventDefinition, ctx: &mut FileExtraction<'_>) {
    let Some(ident) = &e_def.name else { return };
    emit_member(
        ident.name.clone(),
        "event",
        NodeKind::Concept,
        e_def.loc.start(),
        e_def.loc.end(),
        ctx,
    );
}

fn emit_member(
    name: String,
    solidity_kind: &str,
    node_kind: NodeKind,
    start: usize,
    end: usize,
    ctx: &mut FileExtraction<'_>,
) {
    let l = ctx.lines.line(start);
    let e = ctx.lines.line(end);
    let code = slice_lines(&ctx.file.content, l, e);
    let path = &ctx.file.path.clone();

    let stable_id = format!("{}:solidity:{}:{}", path, solidity_kind, name);

    let node = KnowledgeNode {
        id: Uuid::new_v4(),
        repo_id: ctx.repo_id,
        file_id: Some(ctx.file.id),
        kind: node_kind.clone(),
        stable_id,
        name: name.clone(),
        line_start: Some(l as i32),
        line_end: Some(e as i32),
        metadata: json!({
            "language": "solidity",
            "file": path,
            "solidity_kind": solidity_kind
        }),
    };

    ctx.symbol_names.entry(name.clone()).or_insert(node.id);

    ctx.result.edges.push(edge(
        ctx.repo_id,
        ctx.file_node_id,
        node.id,
        EdgeKind::Contains,
        weights::CONTAINS_MEMBER,
        json!({"language": "solidity", "kind": solidity_kind}),
    ));

    ctx.result.chunks.push(chunk_for_node(
        ctx.repo_id,
        Some(ctx.file.id),
        Some(node.id),
        node_kind.as_str(),
        &format!(
            "Language: solidity\nFile: {}\nSymbol: {}\nSolidity kind: {}\nLines: {}-{}\n\n{}",
            path, name, solidity_kind, l, e, code
        ),
        Some(l as i32),
        Some(e as i32),
        json!({
            "symbol": name,
            "kind": node_kind.as_str(),
            "solidity_kind": solidity_kind,
            "file": path
        }),
    ));

    ctx.result.nodes.push(node);
}

// ---------------------------------------------------------------------------
// Inheritance
// ---------------------------------------------------------------------------

fn emit_inheritance(base: &str, contract_node_id: Uuid, ctx: &mut FileExtraction<'_>) {
    if base.is_empty() {
        return;
    }

    let node = KnowledgeNode {
        id: Uuid::new_v4(),
        repo_id: ctx.repo_id,
        file_id: None,
        kind: NodeKind::Concept,
        stable_id: format!("solidity:inheritance:{base}"),
        name: base.to_string(),
        line_start: None,
        line_end: None,
        metadata: json!({
            "language": "solidity",
            "relationship": "inheritance"
        }),
    };

    ctx.result.edges.push(edge(
        ctx.repo_id,
        contract_node_id,
        node.id,
        EdgeKind::Implements,
        weights::IMPLEMENTS,
        json!({"language": "solidity", "base": base}),
    ));

    ctx.result.nodes.push(node);
}

// ---------------------------------------------------------------------------
// Imports
// ---------------------------------------------------------------------------

fn emit_import_directive(import: &Import, ctx: &mut FileExtraction<'_>) {
    let (module, loc) = match import {
        Import::Plain(path, loc) => (import_path_str(path), *loc),
        Import::Rename(path, _, loc) => (import_path_str(path), *loc),
        Import::GlobalSymbol(path, _, loc) => (import_path_str(path), *loc),
    };

    if module.is_empty() {
        return;
    }

    ctx.emit_dependency(&module, "solidity", weights::IMPORTS_SOLIDITY, loc.start());
}

fn import_path_str(path: &ImportPath) -> String {
    match path {
        ImportPath::Filename(lit) => lit.string.clone(),
        ImportPath::Path(ident_path) => ident_path.to_string(),
    }
}
