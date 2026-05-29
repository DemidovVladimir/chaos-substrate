//! solang-parser-based Solidity extraction.
//!
//! Parses a Solidity source file into a `SourceUnit` AST and emits the same
//! node/edge/chunk shapes that the old regex extractors produced. Gracefully
//! degrades on parse failure: a warning is printed to stderr and the file node
//! (already registered by `begin_file`) is left as-is.

use crate::{
    extractor::{chunk_for_node, edge, import_stable_id, is_bare_module_specifier, slice_lines},
    lang::FileExtraction,
    models::{EdgeKind, KnowledgeNode, NodeKind},
    weights,
};
use serde_json::json;
use solang_parser::pt::{ContractPart, ContractTy, FunctionTy, Import, ImportPath, SourceUnitPart};
use uuid::Uuid;

/// Entry point called from `extractor.rs` after `begin_file` has run.
pub(crate) fn extract(ctx: &mut FileExtraction<'_>) -> anyhow::Result<()> {
    let source = ctx.file.content.as_str();
    let (unit, _comments) = match solang_parser::parse(source, 0) {
        Ok(result) => result,
        Err(diags) => {
            eprintln!(
                "[chaos-substrate] {}: solidity parse failed ({} diagnostics)",
                ctx.file.path,
                diags.len()
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

    let line = ctx.lines.line(loc.start()) as i32;
    let is_bare = is_bare_module_specifier(&module);
    let path = ctx.file.path.clone();

    let node = KnowledgeNode {
        id: Uuid::new_v4(),
        repo_id: ctx.repo_id,
        file_id: if is_bare { None } else { Some(ctx.file.id) },
        kind: NodeKind::Dependency,
        stable_id: import_stable_id(ctx.file, &module, is_bare),
        name: module.clone(),
        line_start: if is_bare { None } else { Some(line) },
        line_end: if is_bare { None } else { Some(line) },
        metadata: json!({
            "module": module,
            "language": "solidity",
            "scope": if is_bare { "bare" } else { "relative" }
        }),
    };

    ctx.result.edges.push(edge(
        ctx.repo_id,
        ctx.file_node_id,
        node.id,
        EdgeKind::Imports,
        weights::IMPORTS_SOLIDITY,
        json!({"file": path, "module": module, "line": line}),
    ));

    ctx.result.nodes.push(node);
}

fn import_path_str(path: &ImportPath) -> String {
    match path {
        ImportPath::Filename(lit) => lit.string.clone(),
        ImportPath::Path(ident_path) => ident_path.to_string(),
    }
}
