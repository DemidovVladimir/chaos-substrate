//! apollo-parser-based GraphQL extraction.
//!
//! Handles both halves of the language: SDL type systems (`type`, `interface`,
//! `enum`, `union`, `scalar`, `input`, `directive`, extensions) and executable
//! documents (operations + fragments), for standalone `.graphql`/`.gql`/
//! `.graphqls` files and — via [`extract_embedded_document`] — documents
//! embedded in host-language source (`gql` tagged templates / calls).
//!
//! apollo-parser is error-tolerant: parsing never fails, syntax errors leave a
//! partial CST that still yields the recoverable definitions. Edge resolution
//! deliberately does NOT go through the shared `symbol_names` map or the
//! generic call-edge pass: references are queued as [`PendingRef`]s during the
//! walk and resolved by [`resolve_graphql_edges`] after the file loop, against
//! a graphql-only name map (see that function's doc for why).

use crate::{
    extractor::edge,
    lang::{FileExtraction, LineIndex},
    models::{EdgeKind, ExtractionResult, Language, NodeKind, SourceFile},
    user_surface::{Surface, SurfaceEntry},
    weights,
};
use apollo_parser::cst::{self, CstNode};
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use uuid::Uuid;

/// GraphQL's built-in root type names — the defaults when no
/// `schema { query: … }` definition remaps them.
const DEFAULT_ROOTS: [(&str, &str); 3] = [
    ("Query", "query"),
    ("Mutation", "mutation"),
    ("Subscription", "subscription"),
];

/// Fan-in cap for `UsesType` edges: edges to a type are capped at this many
/// distinct referencing files (the 13th file's edge is dropped) — a type
/// referenced that widely is shared vocabulary, and further edges to it would
/// glue unrelated features into one god-node community, the same failure mode
/// the external-import drop solved for `react`-style hubs. `Extends` refs are
/// exempt: an extension's merge link to its single base is never hub coupling.
const MAX_TYPE_FAN_IN_FILES: usize = 12;

/// SDL definition kinds that participate in type-name resolution. Extensions
/// (`extend_*`) resolve TO these, never AS these; operations and fragments
/// live in their own namespaces.
const SDL_TYPE_KINDS: [&str; 7] = [
    "object",
    "input",
    "interface",
    "enum",
    "union",
    "scalar",
    "directive",
];

/// Why a pending reference exists — decides the resolved edge kind. Declared
/// in resolution-priority order: `Extends` before `UsesType` so an extension's
/// merge link keeps its `relation: "extends"` metadata when the same
/// source→target pair also carries a plain type use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RefKind {
    /// `extend type X` → base `X`: `UsesType` with `relation: "extends"` —
    /// the merge is expressed as an edge (linked, not folded).
    Extends,
    /// Field return/arg type, union member, operation selection type →
    /// `UsesType`.
    UsesType,
    /// object/interface `implements` interface → `Implements`.
    Implements,
    /// `...FragmentName` spread → `Calls` (heuristic-tier weight).
    FragmentSpread,
}

/// A reference from a GraphQL node to a named type/fragment, collected during
/// the walk and resolved after the file loop by [`resolve_graphql_edges`].
pub(crate) struct PendingRef {
    pub source_node_id: Uuid,
    pub target_name: String,
    pub kind: RefKind,
    /// Repo-relative path of the referencing file (fan-in cap accounting).
    pub file: String,
}

/// Where a document's definitions anchor in the indexed file.
enum Anchor<'a> {
    /// A standalone `.graphql` file: CST offsets are file offsets.
    File,
    /// A document embedded in host-language source: every definition anchors
    /// to the host template span (host-parser spans are host byte offsets),
    /// and chunk bodies carry the GraphQL text, never a host-file slice.
    /// Per-definition lines inside a multi-definition template approximate to
    /// the template span.
    Embedded {
        start_off: usize,
        end_off: usize,
        origin: &'a str,
    },
}

/// Entry point called from `extractor.rs` after `begin_file` has run, for
/// `.graphql`/`.gql`/`.graphqls` files. Parse errors keep the partial tree
/// (warn + partial extraction); only a file yielding NO definitions at all
/// propagates `Err`, so the wrapper's centralized whole-file fallback records
/// it as `parse_failed`.
pub(crate) fn extract(ctx: &mut FileExtraction<'_>) -> anyhow::Result<()> {
    let tree = apollo_parser::Parser::new(ctx.file.content.as_str()).parse();
    let error_count = tree.errors().len();
    let emitted = walk_document(ctx, &tree.document(), &Anchor::File);
    if error_count > 0 {
        if emitted == 0 {
            anyhow::bail!("{error_count} graphql parse errors, no definitions recovered");
        }
        let path = ctx.file.path.as_str();
        tracing::warn!(
            path,
            "graphql parse produced {error_count} errors; extracted {emitted} definitions from the partial tree"
        );
    }
    Ok(())
}

/// Extract the definitions of a GraphQL document embedded in a host-language
/// file (a ``gql`…` `` tagged template or a `gql("…")` call). `host_span` is
/// the document's byte range in the HOST file — every emitted node anchors to
/// those lines, while chunk bodies carry the GraphQL text itself. Parse
/// errors are tolerated (interpolation holes arrive as blank lines): the
/// partial tree still yields definitions.
pub(crate) fn extract_embedded_document(
    ctx: &mut FileExtraction<'_>,
    text: &str,
    host_span: (usize, usize),
    origin: &str,
) {
    let tree = apollo_parser::Parser::new(text).parse();
    let error_count = tree.errors().len();
    if error_count > 0 {
        let path = ctx.file.path.as_str();
        tracing::warn!(
            path,
            "graphql parse of embedded {origin} document produced {error_count} errors; keeping the partial tree"
        );
    }
    walk_document(
        ctx,
        &tree.document(),
        &Anchor::Embedded {
            start_off: host_span.0,
            end_off: host_span.1,
            origin,
        },
    );
}

/// Walk a parsed document: emit nodes/chunks for its definitions, queue
/// pending type/fragment references, and collect SDL root fields as
/// user-surface entries. Returns how many definition nodes were emitted.
fn walk_document(
    ctx: &mut FileExtraction<'_>,
    document: &cst::Document,
    anchor: &Anchor<'_>,
) -> usize {
    // Pass 1: `schema { query: MyQuery }` root mappings — the schema block may
    // follow the types it names. Merged additively over the defaults: a
    // mapping overrides only the operation types it names, so an
    // `extend schema { subscription: Sub }` cannot wipe the Query/Mutation
    // defaults (per the GraphQL spec a schema EXTENSION adds to the existing
    // roots rather than replacing them).
    let mut roots = default_roots();
    apply_schema_root_mappings(document, &mut roots);

    let mut emitted = 0usize;
    let mut surface: Vec<SurfaceEntry> = Vec::new();
    for def in document.definitions() {
        if emit_definition(ctx, &def, anchor, &roots, &mut surface) {
            emitted += 1;
        }
    }
    ctx.emit_user_surface(surface);
    emitted
}

/// The built-in Query/Mutation/Subscription root mapping.
fn default_roots() -> BTreeMap<String, &'static str> {
    DEFAULT_ROOTS
        .iter()
        .map(|(name, op)| (name.to_string(), *op))
        .collect()
}

/// Apply a document's `schema { … }` definitions and `extend schema { … }`
/// extensions to `roots`: each named operation type replaces the current root
/// name for THAT operation only; operations the mapping doesn't name keep
/// their existing roots.
fn apply_schema_root_mappings(
    document: &cst::Document,
    roots: &mut BTreeMap<String, &'static str>,
) {
    for def in document.definitions() {
        let root_defs = match &def {
            cst::Definition::SchemaDefinition(schema) => schema.root_operation_type_definitions(),
            cst::Definition::SchemaExtension(schema) => schema.root_operation_type_definitions(),
            _ => continue,
        };
        for root in root_defs {
            let Some(name) = root.named_type().and_then(|t| t.name()) else {
                continue;
            };
            let Some(op) = root.operation_type() else {
                continue;
            };
            let op = operation_type_str(&op);
            roots.retain(|_, mapped| *mapped != op);
            roots.insert(name.text().to_string(), op);
        }
    }
}

/// Run-level post-pass: surface the root fields of object types whose
/// `schema { query: MyQuery }` mapping lives in ANOTHER file of the run.
/// `walk_document` resolves roots per file, so a split-file custom root
/// (mapping in `schema.graphql`, `type MyQuery` in `query.graphql`) surfaces
/// nothing at emit time. This pass re-parses the run's standalone GraphQL
/// files, aggregates the schema-root mappings across ALL of them (defaults
/// merged additively, exactly like the per-file pass), and emits the missed
/// `graphql_field` entries; types already surfaced in-file are skipped by
/// stable id, so nothing is emitted twice. Like edge resolution, an
/// incremental `chaos add` aggregates only within the changed-file batch.
pub(crate) fn surface_split_file_root_fields(repo_id: Uuid, result: &mut ExtractionResult) {
    let docs: Vec<(SourceFile, cst::Document)> = result
        .files
        .iter()
        .filter(|f| f.language == Language::GraphQL)
        .map(|f| {
            let doc = apollo_parser::Parser::new(f.content.as_str())
                .parse()
                .document();
            (f.clone(), doc)
        })
        .collect();
    if docs.is_empty() {
        return;
    }

    let mut roots = default_roots();
    for (_, doc) in &docs {
        apply_schema_root_mappings(doc, &mut roots);
    }

    // Stable ids already emitted in-file, and each file's own File node (the
    // surface edges attach to it).
    let surfaced: HashSet<String> = result
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::GraphqlField)
        .map(|n| n.stable_id.clone())
        .collect();
    let file_nodes: HashMap<Uuid, Uuid> = result
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::File)
        .filter_map(|n| Some((n.file_id?, n.id)))
        .collect();

    for (file, doc) in docs {
        let Some(&file_node_id) = file_nodes.get(&file.id) else {
            continue;
        };
        let lines = LineIndex::new(&file.content);
        let mut entries: Vec<SurfaceEntry> = Vec::new();
        for def in doc.definitions() {
            let (name, fields) = match &def {
                cst::Definition::ObjectTypeDefinition(t) => (t.name(), t.fields_definition()),
                cst::Definition::ObjectTypeExtension(t) => (t.name(), t.fields_definition()),
                _ => continue,
            };
            let (Some(name), Some(fields)) = (name, fields) else {
                continue;
            };
            let name = name.text().to_string();
            let Some(op_type) = roots.get(name.as_str()) else {
                continue;
            };
            collect_root_fields(&lines, &fields, &name, op_type, &Anchor::File, &mut entries);
        }
        entries.retain(|e| !surfaced.contains(&format!("{}:graphql_field:{}", file.path, e.name)));
        if !entries.is_empty() {
            crate::user_surface::emit_surface_entries(
                repo_id,
                &file,
                file_node_id,
                entries,
                result,
            );
        }
    }
}

/// Emit one definition's node + chunk and queue its references. Returns
/// whether a node was emitted (schema definitions are unnamed mappings only).
fn emit_definition(
    ctx: &mut FileExtraction<'_>,
    def: &cst::Definition,
    anchor: &Anchor<'_>,
    roots: &BTreeMap<String, &'static str>,
    surface: &mut Vec<SurfaceEntry>,
) -> bool {
    use cst::Definition as D;

    let mut refs: Vec<(String, RefKind)> = Vec::new();
    // Fields of a root object type (or an extension of one) become
    // user-surface entries after the name is known.
    let mut surface_fields: Option<cst::FieldsDefinition> = None;

    let (graphql_kind, node_kind, name) = match def {
        D::OperationDefinition(op) => return emit_operation(ctx, op, anchor),
        D::FragmentDefinition(frag) => return emit_fragment(ctx, frag, anchor),
        D::SchemaDefinition(_) | D::SchemaExtension(_) => return false,
        D::ObjectTypeDefinition(t) => {
            let Some(name) = t.name() else { return false };
            collect_implements(t.implements_interfaces(), &mut refs);
            collect_field_refs(t.fields_definition(), &mut refs);
            surface_fields = t.fields_definition();
            ("object", NodeKind::Struct, name.text().to_string())
        }
        D::ObjectTypeExtension(t) => {
            let Some(name) = t.name() else { return false };
            refs.push((name.text().to_string(), RefKind::Extends));
            collect_implements(t.implements_interfaces(), &mut refs);
            collect_field_refs(t.fields_definition(), &mut refs);
            surface_fields = t.fields_definition();
            ("extend_object", NodeKind::Struct, name.text().to_string())
        }
        D::InterfaceTypeDefinition(t) => {
            let Some(name) = t.name() else { return false };
            collect_implements(t.implements_interfaces(), &mut refs);
            collect_field_refs(t.fields_definition(), &mut refs);
            ("interface", NodeKind::Trait, name.text().to_string())
        }
        D::InterfaceTypeExtension(t) => {
            let Some(name) = t.name() else { return false };
            refs.push((name.text().to_string(), RefKind::Extends));
            collect_implements(t.implements_interfaces(), &mut refs);
            collect_field_refs(t.fields_definition(), &mut refs);
            ("extend_interface", NodeKind::Trait, name.text().to_string())
        }
        D::EnumTypeDefinition(t) => {
            let Some(name) = t.name() else { return false };
            ("enum", NodeKind::Enum, name.text().to_string())
        }
        D::EnumTypeExtension(t) => {
            let Some(name) = t.name() else { return false };
            refs.push((name.text().to_string(), RefKind::Extends));
            ("extend_enum", NodeKind::Enum, name.text().to_string())
        }
        D::UnionTypeDefinition(t) => {
            let Some(name) = t.name() else { return false };
            collect_union_members(t.union_member_types(), &mut refs);
            ("union", NodeKind::TypeAlias, name.text().to_string())
        }
        D::UnionTypeExtension(t) => {
            let Some(name) = t.name() else { return false };
            refs.push((name.text().to_string(), RefKind::Extends));
            collect_union_members(t.union_member_types(), &mut refs);
            ("extend_union", NodeKind::TypeAlias, name.text().to_string())
        }
        D::ScalarTypeDefinition(t) => {
            let Some(name) = t.name() else { return false };
            ("scalar", NodeKind::TypeAlias, name.text().to_string())
        }
        D::ScalarTypeExtension(t) => {
            let Some(name) = t.name() else { return false };
            refs.push((name.text().to_string(), RefKind::Extends));
            (
                "extend_scalar",
                NodeKind::TypeAlias,
                name.text().to_string(),
            )
        }
        D::InputObjectTypeDefinition(t) => {
            let Some(name) = t.name() else { return false };
            collect_input_refs(t.input_fields_definition(), &mut refs);
            ("input", NodeKind::Struct, name.text().to_string())
        }
        D::InputObjectTypeExtension(t) => {
            let Some(name) = t.name() else { return false };
            refs.push((name.text().to_string(), RefKind::Extends));
            collect_input_refs(t.input_fields_definition(), &mut refs);
            ("extend_input", NodeKind::Struct, name.text().to_string())
        }
        D::DirectiveDefinition(t) => {
            let Some(name) = t.name() else { return false };
            collect_argument_refs(t.arguments_definition(), &mut refs);
            ("directive", NodeKind::TypeAlias, name.text().to_string())
        }
    };

    let (start_off, end_off, code) = anchored_span(def.syntax(), anchor);
    let mut node_meta = json!({
        "language": "graphql",
        "graphql_kind": graphql_kind,
        "file": ctx.file.path,
    });
    if let Anchor::Embedded { origin, .. } = anchor {
        node_meta["origin"] = json!(origin);
    }

    let node_id = ctx.emit_code_symbol(
        &name,
        node_kind.clone(),
        format!("{}:graphql:{}:{}", ctx.file.path, graphql_kind, name),
        "graphql",
        weights::CONTAINS_CODE,
        json!({"language": "graphql", "kind": graphql_kind}),
        node_meta,
        &format!("GraphQL {}", graphql_kind.replace('_', " ")),
        start_off,
        end_off,
        json!({
            "symbol": name,
            "kind": node_kind.as_str(),
            "graphql_kind": graphql_kind,
            "file": ctx.file.path,
        }),
        Some(&code),
    );

    for (target_name, kind) in refs {
        ctx.graphql_refs.push(PendingRef {
            source_node_id: node_id,
            target_name,
            kind,
            file: ctx.file.path.clone(),
        });
    }

    if let (Some(fields), Some(op_type)) = (surface_fields, roots.get(name.as_str())) {
        collect_root_fields(&ctx.lines, &fields, &name, op_type, anchor, surface);
    }
    true
}

/// Emit a `GraphqlOperation` node for a query/mutation/subscription and queue
/// the types its selection uses (variable definition types + inline-fragment
/// type conditions) and the fragments it spreads.
fn emit_operation(
    ctx: &mut FileExtraction<'_>,
    op: &cst::OperationDefinition,
    anchor: &Anchor<'_>,
) -> bool {
    let op_type = op
        .operation_type()
        .map(|t| operation_type_str(&t))
        .unwrap_or("query");
    let root_fields: Vec<String> = op
        .selection_set()
        .map(|set| {
            set.selections()
                .filter_map(|sel| match sel {
                    cst::Selection::Field(field) => Some(field.name()?.text().to_string()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    // Anonymous operations synthesize a stable name from their shape.
    let name = op.name().map(|n| n.text().to_string()).unwrap_or_else(|| {
        root_fields
            .first()
            .map(|first| format!("{op_type}:{first}"))
            .unwrap_or_else(|| op_type.to_string())
    });

    let (start_off, end_off, code) = anchored_span(op.syntax(), anchor);
    let mut node_meta = json!({
        "language": "graphql",
        "graphql_kind": "operation",
        "operation_type": op_type,
        "root_fields": root_fields,
        "file": ctx.file.path,
    });
    if let Anchor::Embedded { origin, .. } = anchor {
        node_meta["origin"] = json!(origin);
    }

    let node_id = ctx.emit_code_symbol(
        &name,
        NodeKind::GraphqlOperation,
        format!("{}:graphql:operation:{}", ctx.file.path, name),
        "graphql",
        weights::CONTAINS_CODE,
        json!({"language": "graphql", "kind": "operation"}),
        node_meta,
        &format!("GraphQL {op_type} operation"),
        start_off,
        end_off,
        json!({
            "symbol": name,
            "kind": NodeKind::GraphqlOperation.as_str(),
            "graphql_kind": "operation",
            "operation_type": op_type,
            "file": ctx.file.path,
        }),
        Some(&code),
    );

    if let Some(vars) = op.variable_definitions() {
        for var in vars.variable_definitions() {
            if let Some(target_name) = var.ty().as_ref().and_then(named_type_name) {
                ctx.graphql_refs.push(PendingRef {
                    source_node_id: node_id,
                    target_name,
                    kind: RefKind::UsesType,
                    file: ctx.file.path.clone(),
                });
            }
        }
    }
    queue_selection_refs(ctx, node_id, op.syntax());
    true
}

/// Emit a `GraphqlFragment` node and queue its type condition plus any nested
/// spreads / inline-fragment type conditions.
fn emit_fragment(
    ctx: &mut FileExtraction<'_>,
    frag: &cst::FragmentDefinition,
    anchor: &Anchor<'_>,
) -> bool {
    let Some(name) = frag.fragment_name().and_then(|n| n.name()) else {
        return false;
    };
    let name = name.text().to_string();
    let type_condition = frag
        .type_condition()
        .and_then(|c| c.named_type())
        .and_then(|t| t.name())
        .map(|n| n.text().to_string());

    let (start_off, end_off, code) = anchored_span(frag.syntax(), anchor);
    let mut node_meta = json!({
        "language": "graphql",
        "graphql_kind": "fragment",
        "type_condition": type_condition,
        "file": ctx.file.path,
    });
    if let Anchor::Embedded { origin, .. } = anchor {
        node_meta["origin"] = json!(origin);
    }

    let node_id = ctx.emit_code_symbol(
        &name,
        NodeKind::GraphqlFragment,
        format!("{}:graphql:fragment:{}", ctx.file.path, name),
        "graphql",
        weights::CONTAINS_CODE,
        json!({"language": "graphql", "kind": "fragment"}),
        node_meta,
        "GraphQL fragment",
        start_off,
        end_off,
        json!({
            "symbol": name,
            "kind": NodeKind::GraphqlFragment.as_str(),
            "graphql_kind": "fragment",
            "file": ctx.file.path,
        }),
        Some(&code),
    );

    if let Some(target_name) = type_condition {
        ctx.graphql_refs.push(PendingRef {
            source_node_id: node_id,
            target_name,
            kind: RefKind::UsesType,
            file: ctx.file.path.clone(),
        });
    }
    queue_selection_refs(ctx, node_id, frag.syntax());
    true
}

/// Queue the references inside an executable definition's subtree: fragment
/// spreads (→ `Calls`) and inline-fragment type conditions (→ `UsesType`).
/// Nested spreads attribute to the enclosing operation/fragment node —
/// top-level resolution is enough for feature-level edges.
fn queue_selection_refs(
    ctx: &mut FileExtraction<'_>,
    source: Uuid,
    syntax: &apollo_parser::SyntaxNode,
) {
    for node in syntax.descendants() {
        if let Some(spread) = cst::FragmentSpread::cast(node.clone()) {
            if let Some(name) = spread.fragment_name().and_then(|n| n.name()) {
                ctx.graphql_refs.push(PendingRef {
                    source_node_id: source,
                    target_name: name.text().to_string(),
                    kind: RefKind::FragmentSpread,
                    file: ctx.file.path.clone(),
                });
            }
        } else if let Some(inline) = cst::InlineFragment::cast(node) {
            if let Some(name) = inline
                .type_condition()
                .and_then(|c| c.named_type())
                .and_then(|t| t.name())
            {
                ctx.graphql_refs.push(PendingRef {
                    source_node_id: source,
                    target_name: name.text().to_string(),
                    kind: RefKind::UsesType,
                    file: ctx.file.path.clone(),
                });
            }
        }
    }
}

/// Collect a root type's fields as `graphql_field` user-surface entries
/// (`MyQuery.user`), honoring custom root mappings via `operation_type`.
fn collect_root_fields(
    lines: &LineIndex,
    fields: &cst::FieldsDefinition,
    parent_type: &str,
    operation_type: &str,
    anchor: &Anchor<'_>,
    surface: &mut Vec<SurfaceEntry>,
) {
    for field in fields.field_definitions() {
        let Some(field_name) = field.name() else {
            continue;
        };
        let field_name = field_name.text().to_string();
        let args: Vec<String> = field
            .arguments_definition()
            .map(|args| {
                args.input_value_definitions()
                    .filter_map(|arg| {
                        let arg_name = arg.name()?.text().to_string();
                        let ty = arg
                            .ty()
                            .map(|t| t.syntax().to_string().trim().to_string())
                            .unwrap_or_default();
                        Some(if ty.is_empty() {
                            arg_name
                        } else {
                            format!("{arg_name}: {ty}")
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let (start_off, end_off, _) = anchored_span(field.syntax(), anchor);
        surface.push(SurfaceEntry {
            surface: Surface::GraphqlField,
            name: format!("{parent_type}.{field_name}"),
            framework: "graphql-sdl",
            detail: json!({
                "parent_type": parent_type,
                "field": field_name,
                "operation_type": operation_type,
                "args": args,
            }),
            line_start: lines.line(start_off),
            line_end: lines.line(end_off),
        });
    }
}

/// Byte span + trimmed source text of a CST node under the given anchor.
/// Rowan ranges include trivia; trimming keeps line anchors on the definition
/// itself. Embedded documents anchor to the host template span instead —
/// their CST offsets index the embedded text, not the host file.
fn anchored_span(node: &apollo_parser::SyntaxNode, anchor: &Anchor<'_>) -> (usize, usize, String) {
    let raw = node.to_string();
    let leading = raw.len() - raw.trim_start().len();
    let text = raw.trim().to_string();
    match anchor {
        Anchor::File => {
            let start = usize::from(node.text_range().start()) + leading;
            let end = start + text.len();
            (start, end, text)
        }
        Anchor::Embedded {
            start_off, end_off, ..
        } => (*start_off, *end_off, text),
    }
}

fn operation_type_str(op: &cst::OperationType) -> &'static str {
    if op.mutation_token().is_some() {
        "mutation"
    } else if op.subscription_token().is_some() {
        "subscription"
    } else {
        "query"
    }
}

/// The underlying named type of a (possibly wrapped) type reference:
/// `[User!]!` → `User`.
fn named_type_name(ty: &cst::Type) -> Option<String> {
    match ty {
        cst::Type::NamedType(named) => Some(named.name()?.text().to_string()),
        cst::Type::ListType(list) => named_type_name(&list.ty()?),
        cst::Type::NonNullType(non_null) => match non_null.named_type() {
            Some(named) => Some(named.name()?.text().to_string()),
            None => named_type_name(&cst::Type::ListType(non_null.list_type()?)),
        },
    }
}

fn collect_implements(
    implements: Option<cst::ImplementsInterfaces>,
    refs: &mut Vec<(String, RefKind)>,
) {
    let Some(implements) = implements else { return };
    for named in implements.named_types() {
        if let Some(name) = named.name() {
            refs.push((name.text().to_string(), RefKind::Implements));
        }
    }
}

fn collect_field_refs(fields: Option<cst::FieldsDefinition>, refs: &mut Vec<(String, RefKind)>) {
    let Some(fields) = fields else { return };
    for field in fields.field_definitions() {
        if let Some(name) = field.ty().as_ref().and_then(named_type_name) {
            refs.push((name, RefKind::UsesType));
        }
        collect_argument_refs(field.arguments_definition(), refs);
    }
}

fn collect_argument_refs(
    args: Option<cst::ArgumentsDefinition>,
    refs: &mut Vec<(String, RefKind)>,
) {
    let Some(args) = args else { return };
    for arg in args.input_value_definitions() {
        if let Some(name) = arg.ty().as_ref().and_then(named_type_name) {
            refs.push((name, RefKind::UsesType));
        }
    }
}

fn collect_input_refs(
    fields: Option<cst::InputFieldsDefinition>,
    refs: &mut Vec<(String, RefKind)>,
) {
    let Some(fields) = fields else { return };
    for field in fields.input_value_definitions() {
        if let Some(name) = field.ty().as_ref().and_then(named_type_name) {
            refs.push((name, RefKind::UsesType));
        }
    }
}

fn collect_union_members(
    members: Option<cst::UnionMemberTypes>,
    refs: &mut Vec<(String, RefKind)>,
) {
    let Some(members) = members else { return };
    for named in members.named_types() {
        if let Some(name) = named.name() {
            refs.push((name.text().to_string(), RefKind::UsesType));
        }
    }
}

/// Resolve the queued GraphQL references into edges, against a graphql-only
/// name map built from THIS run's nodes (`metadata.language == "graphql"`).
/// Runs after the whole file loop, so forward and cross-file references
/// resolve regardless of walk order.
///
/// The shared `symbol_names` map is deliberately NOT used here: it is
/// walk-order-populated and shared across languages, so a forward reference
/// would silently drop and a TS class named `User` could capture a GraphQL
/// edge. The generic call-edge pass is bypassed for the same reason.
///
/// KNOWN LIMITATION: an incremental `chaos add` resolves only within the
/// changed-file batch — cross-file edges to unchanged files return on the
/// next full analyze.
///
/// Hub gating (the repo's recurring god-node failure mode): no `UsesType`
/// edges to scalar/directive targets (built-in scalars never resolve; custom
/// ones are skipped here), and `UsesType` edges to a type are capped at
/// [`MAX_TYPE_FAN_IN_FILES`] distinct referencing files. `Extends` refs
/// bypass both gates — an extension has exactly one base, so the merge link
/// is 1:1, never hub coupling.
pub(crate) fn resolve_graphql_edges(
    repo_id: Uuid,
    result: &mut ExtractionResult,
    refs: &[PendingRef],
) {
    if refs.is_empty() {
        return;
    }

    // Name → (node id, graphql_kind), first-wins over stable_id-sorted
    // candidates so resolution is deterministic regardless of walk order.
    let mut type_candidates: Vec<(&str, &str, Uuid, &str)> = Vec::new();
    let mut fragment_candidates: Vec<(&str, Uuid, &str)> = Vec::new();
    for node in &result.nodes {
        if node.metadata.get("language").and_then(|v| v.as_str()) != Some("graphql") {
            continue;
        }
        let Some(kind) = node.metadata.get("graphql_kind").and_then(|v| v.as_str()) else {
            continue;
        };
        if kind == "fragment" {
            fragment_candidates.push((node.name.as_str(), node.id, node.stable_id.as_str()));
        } else if SDL_TYPE_KINDS.contains(&kind) {
            type_candidates.push((node.name.as_str(), kind, node.id, node.stable_id.as_str()));
        }
    }
    type_candidates.sort_by_key(|(_, _, _, stable_id)| *stable_id);
    fragment_candidates.sort_by_key(|(_, _, stable_id)| *stable_id);
    let mut types: HashMap<&str, (Uuid, &str)> = HashMap::new();
    for (name, kind, id, _) in &type_candidates {
        types.entry(name).or_insert((*id, kind));
    }
    let mut fragments: HashMap<&str, Uuid> = HashMap::new();
    for (name, id, _) in &fragment_candidates {
        fragments.entry(name).or_insert(*id);
    }

    // Deterministic ref order — the walk order depends on the directory
    // walker, and the fan-in cap must drop the same edges every run.
    let mut ordered: Vec<&PendingRef> = refs.iter().collect();
    ordered.sort_by_key(|r| {
        (
            r.file.as_str(),
            r.target_name.as_str(),
            r.kind,
            r.source_node_id,
        )
    });

    let mut seen: HashSet<(Uuid, Uuid, &'static str)> = HashSet::new();
    let mut fan_in: HashMap<Uuid, HashSet<&str>> = HashMap::new();
    for pending in ordered {
        match pending.kind {
            RefKind::FragmentSpread => {
                let Some(&target) = fragments.get(pending.target_name.as_str()) else {
                    continue;
                };
                if target == pending.source_node_id
                    || !seen.insert((pending.source_node_id, target, "calls"))
                {
                    continue;
                }
                result.edges.push(edge(
                    repo_id,
                    pending.source_node_id,
                    target,
                    EdgeKind::Calls,
                    weights::CALLS_HEURISTIC,
                    json!({
                        "language": "graphql",
                        "relation": "fragment_spread",
                        "fragment": pending.target_name,
                    }),
                ));
            }
            RefKind::Implements => {
                let Some(&(target, _)) = types.get(pending.target_name.as_str()) else {
                    continue;
                };
                if target == pending.source_node_id
                    || !seen.insert((pending.source_node_id, target, "implements"))
                {
                    continue;
                }
                result.edges.push(edge(
                    repo_id,
                    pending.source_node_id,
                    target,
                    EdgeKind::Implements,
                    weights::IMPLEMENTS,
                    json!({
                        "language": "graphql",
                        "relation": "implements",
                        "interface": pending.target_name,
                    }),
                ));
            }
            RefKind::Extends | RefKind::UsesType => {
                let Some(&(target, target_kind)) = types.get(pending.target_name.as_str()) else {
                    continue;
                };
                let is_extends = pending.kind == RefKind::Extends;
                // Scalars and directives are shared vocabulary, not coupling —
                // but an extension has exactly ONE base, so its merge link is
                // never hub coupling: Extends bypasses this gate and the
                // fan-in cap below (`extend scalar DateTime` must link, and an
                // extension of a popular type must keep its base).
                if !is_extends && (target_kind == "scalar" || target_kind == "directive") {
                    continue;
                }
                if target == pending.source_node_id
                    || seen.contains(&(pending.source_node_id, target, "uses_type"))
                {
                    continue;
                }
                let files = fan_in.entry(target).or_default();
                if !files.contains(pending.file.as_str()) {
                    if files.len() >= MAX_TYPE_FAN_IN_FILES {
                        if !is_extends {
                            continue;
                        }
                        // Extends emits without consuming a fan-in slot.
                    } else {
                        files.insert(pending.file.as_str());
                    }
                }
                seen.insert((pending.source_node_id, target, "uses_type"));
                let relation = if pending.kind == RefKind::Extends {
                    "extends"
                } else {
                    "uses_type"
                };
                result.edges.push(edge(
                    repo_id,
                    pending.source_node_id,
                    target,
                    EdgeKind::UsesType,
                    weights::USES_TYPE,
                    json!({
                        "language": "graphql",
                        "relation": relation,
                        "type": pending.target_name,
                    }),
                ));
            }
        }
    }
}
