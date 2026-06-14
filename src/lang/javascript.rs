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
    user_surface::{Surface, SurfaceEntry},
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

/// Byte spans needed to build a class chunk that excludes method bodies:
/// the methods are separately captured as `Function` symbols, so the class
/// chunk keeps only its declaration header, field/property declarations and
/// a method-name roster (no duplicated bodies, no blind splitting).
struct ClassParts {
    /// Byte offset where the class body (`{`) starts.
    body_start: u32,
    /// Byte spans of property/field declarations (incl. index signatures).
    property_spans: Vec<(u32, u32)>,
    /// Method names in source order, for the roster line.
    method_names: Vec<String>,
}

struct JsSymbol {
    name: String,
    kind: CollectedNodeKind,
    start: u32,
    end: u32,
    class_parts: Option<ClassParts>,
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
    /// (METHOD, "/path", receiver, start, end) — framework-shaped route
    /// registrations (`app.get('/x', …)`), mirroring the linker's provider
    /// markers so a bare axios instance doesn't masquerade as a server.
    routes: Vec<(String, String, String, u32, u32)>,
    /// (VAR, start) — `process.env.X` / `process.env["X"]` reads.
    env_reads: Vec<(String, u32)>,
    /// (name, start, end) — commander/yargs-style `.command('name …')`.
    cli_commands: Vec<(String, u32, u32)>,
}

/// Receivers whose `.get(`/`.post(`/… calls count as server-side route
/// registrations (same shape as the linker's provider markers).
const ROUTE_RECEIVERS: &[&str] = &["app", "router", "fastify", "server"];
const ROUTE_METHODS: &[&str] = &["get", "post", "put", "patch", "delete", "all"];

impl<'a> Visit<'a> for Collector {
    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        if let Some(id) = &it.id {
            self.symbols.push(JsSymbol {
                name: id.name.to_string(),
                kind: CollectedNodeKind::Function,
                start: it.span.start,
                end: it.span.end,
                class_parts: None,
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
                    class_parts: None,
                });
            }
        }
        walk::walk_variable_declarator(self, it);
    }

    fn visit_class(&mut self, it: &Class<'a>) {
        if let Some(id) = &it.id {
            let class_name = id.name.to_string();
            let mut property_spans = Vec::new();
            let mut method_names = Vec::new();
            for element in &it.body.body {
                use oxc_ast::ast::ClassElement;
                match element {
                    ClassElement::PropertyDefinition(p) => {
                        property_spans.push((p.span.start, p.span.end));
                    }
                    ClassElement::AccessorProperty(p) => {
                        property_spans.push((p.span.start, p.span.end));
                    }
                    ClassElement::TSIndexSignature(p) => {
                        property_spans.push((p.span.start, p.span.end));
                    }
                    ClassElement::MethodDefinition(m) => {
                        if let Some(name) = m.key.static_name() {
                            method_names.push(name.to_string());
                        }
                    }
                    ClassElement::StaticBlock(_) => {}
                }
            }
            self.symbols.push(JsSymbol {
                name: class_name.clone(),
                kind: CollectedNodeKind::Struct,
                start: it.span.start,
                end: it.span.end,
                class_parts: Some(ClassParts {
                    body_start: it.body.span.start,
                    property_spans,
                    method_names,
                }),
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
                class_parts: None,
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
            class_parts: None,
        });
        walk::walk_ts_interface_declaration(self, it);
    }

    fn visit_ts_enum_declaration(&mut self, it: &TSEnumDeclaration<'a>) {
        self.symbols.push(JsSymbol {
            name: it.id.name.to_string(),
            kind: CollectedNodeKind::Enum,
            start: it.span.start,
            end: it.span.end,
            class_parts: None,
        });
        walk::walk_ts_enum_declaration(self, it);
    }

    fn visit_ts_type_alias_declaration(&mut self, it: &TSTypeAliasDeclaration<'a>) {
        self.symbols.push(JsSymbol {
            name: it.id.name.to_string(),
            kind: CollectedNodeKind::TypeAlias,
            start: it.span.start,
            end: it.span.end,
            class_parts: None,
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
            self.collect_user_surface_call(m, it);
        }
        walk::walk_call_expression(self, it);
    }

    fn visit_static_member_expression(&mut self, it: &oxc_ast::ast::StaticMemberExpression<'a>) {
        // process.env.FOO
        if is_process_env(&it.object) {
            self.env_reads
                .push((it.property.name.to_string(), it.span.start));
        }
        walk::walk_static_member_expression(self, it);
    }

    fn visit_computed_member_expression(
        &mut self,
        it: &oxc_ast::ast::ComputedMemberExpression<'a>,
    ) {
        // process.env["FOO"]
        if is_process_env(&it.object) {
            if let Expression::StringLiteral(s) = &it.expression {
                self.env_reads.push((s.value.to_string(), it.span.start));
            }
        }
        walk::walk_computed_member_expression(self, it);
    }
}

impl Collector {
    /// Route registrations (`app.get('/x', …)`) and CLI command definitions
    /// (`program.command('serve …')`) from a member-call expression.
    fn collect_user_surface_call(
        &mut self,
        m: &oxc_ast::ast::StaticMemberExpression<'_>,
        it: &oxc_ast::ast::CallExpression<'_>,
    ) {
        let property = m.property.name.as_str();
        let first_string = it.arguments.first().and_then(|arg| match arg {
            oxc_ast::ast::Argument::StringLiteral(s) => Some(s.value.to_string()),
            _ => None,
        });

        if ROUTE_METHODS.contains(&property) {
            if let Expression::Identifier(receiver) = &m.object {
                if ROUTE_RECEIVERS.contains(&receiver.name.as_str()) {
                    if let Some(path) = first_string.as_deref() {
                        if path.starts_with('/') {
                            self.routes.push((
                                property.to_ascii_uppercase(),
                                path.to_string(),
                                receiver.name.to_string(),
                                it.span.start,
                                it.span.end,
                            ));
                        }
                    }
                }
            }
            return;
        }

        if property == "command" {
            if let Some(spec) = first_string {
                // commander specs look like "serve [options] <port>"; the
                // command name is the first token. A leading '/' means it's
                // a path, not a command.
                let name = spec.split_whitespace().next().unwrap_or("").to_string();
                if !name.is_empty() && !name.starts_with('/') {
                    self.cli_commands.push((name, it.span.start, it.span.end));
                }
            }
        }
    }
}

/// Is this expression literally `process.env`?
fn is_process_env(expr: &Expression<'_>) -> bool {
    if let Expression::StaticMemberExpression(inner) = expr {
        if inner.property.name == "env" {
            if let Expression::Identifier(obj) = &inner.object {
                return obj.name == "process";
            }
        }
    }
    false
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

    let chunks_before = ctx.result.chunks.len();

    // Graceful degrade: if parse produced no usable AST
    if !ret.errors.is_empty() && ret.program.body.is_empty() {
        crate::lang::warn_parse_failure(&ctx.file.path, &format!("{} errors", ret.errors.len()));
        ctx.emit_whole_file_fallback(chunks_before, "parse_failed");
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

    // Config objects, .d.ts declarations, top-level test() specs, and data
    // literals collect no symbols; without a fallback chunk the file is
    // invisible to retrieval. Checked before user-surface emission so a lone
    // env-read chunk doesn't mask an unindexed file body.
    ctx.emit_whole_file_fallback(chunks_before, "no_symbols_extracted");

    let mut surface: Vec<SurfaceEntry> = Vec::new();
    for (method, path, receiver, start, end) in collector.routes {
        surface.push(SurfaceEntry {
            surface: Surface::Route,
            name: format!("{method} {path}"),
            framework: if receiver == "fastify" {
                "fastify"
            } else {
                "express-like"
            },
            detail: json!({"method": method, "route_path": path, "receiver": receiver}),
            line_start: ctx.lines.line(start as usize),
            line_end: ctx.lines.line(end as usize),
        });
    }
    for (name, start, end) in collector.cli_commands {
        surface.push(SurfaceEntry {
            surface: Surface::Cli,
            name,
            framework: "commander-like",
            detail: json!({"role": "command"}),
            line_start: ctx.lines.line(start as usize),
            line_end: ctx.lines.line(end as usize),
        });
    }
    for (var, start) in collector.env_reads {
        let line = ctx.lines.line(start as usize);
        surface.push(SurfaceEntry {
            surface: Surface::Env,
            name: var,
            framework: "process.env",
            detail: json!({"access": "process.env"}),
            line_start: line,
            line_end: line,
        });
    }
    ctx.emit_user_surface(surface);

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

    // Class chunks exclude method bodies (methods are separate symbols with
    // their own chunks): declaration header + fields + a method roster. The
    // roster lists bare names with no parentheses so the `name(` call
    // heuristic cannot fabricate call edges from it.
    let code_override = sym.class_parts.as_ref().map(|parts| {
        let header_start = ctx.lines.line(sym.start as usize);
        let header_end = ctx.lines.line(parts.body_start as usize);
        let mut body = slice_lines(&ctx.file.content, header_start, header_end);
        for (start, end) in &parts.property_spans {
            body.push('\n');
            body.push_str(&slice_lines(
                &ctx.file.content,
                ctx.lines.line(*start as usize),
                ctx.lines.line(*end as usize),
            ));
        }
        if !parts.method_names.is_empty() {
            body.push_str(&format!(
                "\nMethods (extracted separately): {}",
                parts.method_names.join(", ")
            ));
        }
        body
    });

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
        code_override.as_deref(),
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
