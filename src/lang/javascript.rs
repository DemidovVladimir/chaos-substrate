//! oxc-based JS/TS extraction.
//!
//! Parses a JavaScript or TypeScript source file using the `oxc` parser and emits
//! the same node/edge/chunk shapes the old regex extractors produced, with the
//! added benefit that class methods are now captured as `Function` symbols.
//! Gracefully degrades on parse failure: a warning is printed to stderr and the
//! file node (already registered by `begin_file`) is left as-is.

use crate::{
    extractor::{
        cdk_service, chunk_for_node, edge, is_js_ts_test_file, is_test_symbol, looks_like_cdk_file,
        slice_lines,
    },
    lang::FileExtraction,
    models::{EdgeKind, KnowledgeNode, NodeKind},
    weights,
};
use anyhow::Result;
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    BindingPattern, Class, Expression, Function, ImportDeclaration, MethodDefinition,
    NewExpression, TSEnumDeclaration, TSInterfaceDeclaration, TSTypeAliasDeclaration,
    VariableDeclarator,
};
use oxc_ast_visit::{walk, Visit};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};
use oxc_syntax::scope::ScopeFlags;
use serde_json::json;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Internal collector types
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum CollectedNodeKind {
    Function,
    Struct,
    Trait,
    Enum,
    TypeAlias,
}

impl CollectedNodeKind {
    fn to_node_kind(&self) -> NodeKind {
        match self {
            CollectedNodeKind::Function => NodeKind::Function,
            CollectedNodeKind::Struct => NodeKind::Struct,
            CollectedNodeKind::Trait => NodeKind::Trait,
            CollectedNodeKind::Enum => NodeKind::Enum,
            CollectedNodeKind::TypeAlias => NodeKind::TypeAlias,
        }
    }
}

struct JsSymbol {
    name: String,
    kind: CollectedNodeKind,
    start: u32,
    end: u32,
}

struct JsImport {
    module: String,
    start: u32,
}

#[derive(Default)]
struct Collector {
    is_cdk: bool,
    symbols: Vec<JsSymbol>,
    imports: Vec<JsImport>,
    stacks: Vec<(String, u32, u32)>,
    constructs: Vec<(String, String, u32, u32)>,
    calls: Vec<(String, u32)>,
}

impl<'a> Visit<'a> for Collector {
    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        if let Some(id) = &it.id {
            self.symbols.push(JsSymbol {
                name: id.name.to_string(),
                kind: CollectedNodeKind::Function,
                start: it.span.start,
                end: it.span.end,
            });
        }
        walk::walk_function(self, it, flags);
    }

    fn visit_variable_declarator(&mut self, it: &VariableDeclarator<'a>) {
        let is_fn = matches!(
            it.init.as_ref(),
            Some(Expression::ArrowFunctionExpression(_)) | Some(Expression::FunctionExpression(_))
        );
        if is_fn {
            if let BindingPattern::BindingIdentifier(id) = &it.id {
                self.symbols.push(JsSymbol {
                    name: id.name.to_string(),
                    kind: CollectedNodeKind::Function,
                    start: it.span.start,
                    end: it.span.end,
                });
            }
        }
        walk::walk_variable_declarator(self, it);
    }

    fn visit_class(&mut self, it: &Class<'a>) {
        if let Some(id) = &it.id {
            let class_name = id.name.to_string();
            self.symbols.push(JsSymbol {
                name: class_name.clone(),
                kind: CollectedNodeKind::Struct,
                start: it.span.start,
                end: it.span.end,
            });
            // CDK stack detection: class X extends <prefix.>Stack
            if self.is_cdk {
                if let Some(super_expr) = &it.super_class {
                    if let Some(ct) = callee_text(super_expr) {
                        let last_segment = ct.split('.').next_back().unwrap_or(&ct);
                        if last_segment == "Stack" {
                            self.stacks.push((class_name, it.span.start, it.span.end));
                        }
                    }
                }
            }
        }
        walk::walk_class(self, it);
    }

    fn visit_new_expression(&mut self, it: &NewExpression<'a>) {
        if !self.is_cdk {
            walk::walk_new_expression(self, it);
            return;
        }
        if let Some(construct_type) = callee_text(&it.callee) {
            let last_seg = construct_type
                .split('.')
                .next_back()
                .unwrap_or(&construct_type);
            if last_seg.starts_with(|c: char| c.is_ascii_uppercase()) && it.arguments.len() >= 2 {
                let arg0_is_this =
                    matches!(it.arguments[0], oxc_ast::ast::Argument::ThisExpression(_));
                let logical_id = match &it.arguments[1] {
                    oxc_ast::ast::Argument::StringLiteral(s) => Some(s.value.to_string()),
                    _ => None,
                };
                if arg0_is_this {
                    if let Some(lid) = logical_id {
                        self.constructs
                            .push((construct_type, lid, it.span.start, it.span.end));
                    }
                }
            }
        }
        walk::walk_new_expression(self, it);
    }

    fn visit_method_definition(&mut self, it: &MethodDefinition<'a>) {
        if let Some(name) = it.key.static_name() {
            self.symbols.push(JsSymbol {
                name: name.to_string(),
                kind: CollectedNodeKind::Function,
                start: it.span().start,
                end: it.span().end,
            });
        }
        walk::walk_method_definition(self, it);
    }

    fn visit_ts_interface_declaration(&mut self, it: &TSInterfaceDeclaration<'a>) {
        self.symbols.push(JsSymbol {
            name: it.id.name.to_string(),
            kind: CollectedNodeKind::Trait,
            start: it.span.start,
            end: it.span.end,
        });
        walk::walk_ts_interface_declaration(self, it);
    }

    fn visit_ts_enum_declaration(&mut self, it: &TSEnumDeclaration<'a>) {
        self.symbols.push(JsSymbol {
            name: it.id.name.to_string(),
            kind: CollectedNodeKind::Enum,
            start: it.span.start,
            end: it.span.end,
        });
        walk::walk_ts_enum_declaration(self, it);
    }

    fn visit_ts_type_alias_declaration(&mut self, it: &TSTypeAliasDeclaration<'a>) {
        self.symbols.push(JsSymbol {
            name: it.id.name.to_string(),
            kind: CollectedNodeKind::TypeAlias,
            start: it.span.start,
            end: it.span.end,
        });
        walk::walk_ts_type_alias_declaration(self, it);
    }

    fn visit_import_declaration(&mut self, it: &ImportDeclaration<'a>) {
        self.imports.push(JsImport {
            module: it.source.value.to_string(),
            start: it.span.start,
        });
        walk::walk_import_declaration(self, it);
    }

    fn visit_call_expression(&mut self, it: &oxc_ast::ast::CallExpression<'a>) {
        if let Some(id) = it.callee.get_identifier_reference() {
            self.calls.push((id.name.to_string(), it.span.start));
        } else if let oxc_ast::ast::Expression::StaticMemberExpression(m) = &it.callee {
            self.calls
                .push((m.property.name.to_string(), it.span.start));
        }
        walk::walk_call_expression(self, it);
    }
}

// ---------------------------------------------------------------------------
// Helper: extract dotted name from an expression (used for callee & superclass)
// ---------------------------------------------------------------------------

fn callee_text(expr: &Expression) -> Option<String> {
    match expr {
        Expression::Identifier(id) => Some(id.name.to_string()),
        Expression::StaticMemberExpression(m) => {
            let obj = callee_text(&m.object)?;
            Some(format!("{}.{}", obj, m.property.name))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Entry point called from `extractor.rs` after `begin_file` has run.
pub(crate) fn extract(ctx: &mut FileExtraction<'_>) -> Result<()> {
    let content = ctx.file.content.clone();
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(ctx.file.path.as_str()).unwrap_or_default();
    let ret = Parser::new(&allocator, &content, source_type).parse();

    // Graceful degrade: if parse produced no usable AST
    if !ret.errors.is_empty() && ret.program.body.is_empty() {
        crate::lang::warn_parse_failure(&ctx.file.path, &format!("{} errors", ret.errors.len()));
        return Ok(());
    }

    let mut collector = Collector {
        is_cdk: looks_like_cdk_file(&content),
        ..Default::default()
    };
    collector.visit_program(&ret.program);

    for sym in collector.symbols {
        emit_symbol(&sym, ctx);
    }

    for imp in collector.imports {
        emit_import(&imp, ctx);
    }

    // CDK detection: collection was gated on `looks_like_cdk_file` (see
    // `Collector::is_cdk`), so these vecs are empty for non-CDK files.
    for (name, start, end) in collector.stacks {
        emit_cdk_stack(&name, start, end, ctx);
    }
    for (construct_type, logical_id, start, end) in collector.constructs {
        emit_cdk_construct(&construct_type, &logical_id, start, end, ctx);
    }

    for (callee, off) in collector.calls {
        ctx.calls.push(crate::lang::CallSite {
            file: ctx.file.path.clone(),
            callee,
            line: ctx.lines.line(off as usize) as i32,
        });
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Emitters
// ---------------------------------------------------------------------------

fn emit_symbol(sym: &JsSymbol, ctx: &mut FileExtraction<'_>) {
    let name = sym.name.trim();
    if name.is_empty() {
        return;
    }

    let base_kind = sym.kind.to_node_kind();
    let kind = if is_js_ts_test_file(&ctx.file.path) || is_test_symbol(name) {
        NodeKind::Test
    } else {
        base_kind
    };

    let language = ctx.file.language.as_str();
    let stable_id = format!("{}:{}:{}", ctx.file.path, kind.as_str(), name);

    ctx.emit_code_symbol(
        name,
        kind.clone(),
        stable_id,
        language,
        weights::CONTAINS_CODE,
        json!({}),
        json!({ "language": language, "file": ctx.file.path }),
        kind.as_str(),
        sym.start as usize,
        sym.end as usize,
        json!({ "symbol": name, "kind": kind.as_str(), "file": ctx.file.path }),
    );
}

fn emit_import(imp: &JsImport, ctx: &mut FileExtraction<'_>) {
    let module = imp.module.trim();
    if module.is_empty() {
        return;
    }
    ctx.emit_dependency(
        module,
        ctx.file.language.as_str(),
        weights::IMPORTS_MODULE,
        imp.start as usize,
    );
}

fn emit_cdk_stack(name: &str, start: u32, end: u32, ctx: &mut FileExtraction<'_>) {
    let l = ctx.lines.line(start as usize);
    let e = ctx.lines.line(end as usize);
    let code = slice_lines(&ctx.file.content, l, e);
    let path = &ctx.file.path;

    let node = KnowledgeNode {
        id: Uuid::new_v4(),
        repo_id: ctx.repo_id,
        file_id: Some(ctx.file.id),
        kind: NodeKind::DeploymentResource,
        stable_id: format!("{}:aws-cdk:stack:{name}", path),
        name: name.to_string(),
        line_start: Some(l as i32),
        line_end: Some(e as i32),
        metadata: json!({"technology": "aws_cdk", "resource_kind": "stack", "file": path}),
    };

    ctx.result.edges.push(edge(
        ctx.repo_id,
        ctx.file_node_id,
        node.id,
        EdgeKind::Defines,
        weights::DEFINES_SYMBOL,
        json!({"technology": "aws_cdk"}),
    ));

    ctx.result.chunks.push(chunk_for_node(
        ctx.repo_id,
        Some(ctx.file.id),
        Some(node.id),
        "aws_cdk_stack",
        &format!(
            "Technology: AWS CDK\nFile: {}\nStack: {}\nLines: {}-{}\n\n{}",
            path, name, l, e, code
        ),
        Some(l as i32),
        Some(e as i32),
        json!({"technology": "aws_cdk", "kind": "stack", "symbol": name, "file": path}),
    ));

    ctx.result.nodes.push(node);
}

fn emit_cdk_construct(
    construct_type: &str,
    logical_id: &str,
    start: u32,
    end: u32,
    ctx: &mut FileExtraction<'_>,
) {
    let l = ctx.lines.line(start as usize);
    let e = ctx.lines.line(end as usize);
    let code = slice_lines(&ctx.file.content, l, e);
    let path = &ctx.file.path;
    let service = cdk_service(construct_type);

    let node = KnowledgeNode {
        id: Uuid::new_v4(),
        repo_id: ctx.repo_id,
        file_id: Some(ctx.file.id),
        kind: NodeKind::DeploymentResource,
        stable_id: format!(
            "{}:aws-cdk:resource:{}:{}",
            path, construct_type, logical_id
        ),
        name: format!("{construct_type} {logical_id}"),
        line_start: Some(l as i32),
        line_end: Some(e as i32),
        metadata: json!({
            "technology": "aws_cdk",
            "resource_kind": "construct",
            "construct_type": construct_type,
            "logical_id": logical_id,
            "service": service,
            "file": path
        }),
    };

    ctx.result.edges.push(edge(
        ctx.repo_id,
        ctx.file_node_id,
        node.id,
        EdgeKind::Configures,
        weights::CONFIGURES,
        json!({"technology": "aws_cdk", "service": service}),
    ));

    ctx.result.chunks.push(chunk_for_node(
        ctx.repo_id,
        Some(ctx.file.id),
        Some(node.id),
        "aws_cdk_resource",
        &format!(
            "Technology: AWS CDK\nFile: {}\nResource: {}\nLogical ID: {}\nService: {}\nLines: {}-{}\n\n{}",
            path, construct_type, logical_id, service, l, e, code
        ),
        Some(l as i32),
        Some(e as i32),
        json!({
            "technology": "aws_cdk",
            "kind": "resource",
            "construct_type": construct_type,
            "logical_id": logical_id,
            "service": service,
            "file": path
        }),
    ));

    ctx.result.nodes.push(node);
}
