//! GraphQL extraction: standalone SDL / executable documents (`.graphql`,
//! `.gql`, `.graphqls`) and documents EMBEDDED in host languages.
//!
//! The `tree-sitter-graphql` grammar exposes no named fields, so the generic
//! walker's `DefRule` table cannot address a type's `name` child. Instead the
//! GraphQL `LangSpec` has an empty rule table and does all of its work in one
//! `ExtraPass` (`definitions`) that walks the tree by child TYPE.
//!
//! Symbols produced (stable_id = `graphql:{path}:{kind}:{qualified}`):
//! * `object_type_definition` → `Type` (container), `interface_type_definition`
//!   → `Interface` (container), `enum_type_definition` → `Enum`,
//!   `input_object_type_definition` → `Type`, `union_type_definition` /
//!   `scalar_type_definition` → `TypeAlias`;
//! * `field_definition` → `Field`, parent = the type, qualified `Type.field`;
//! * `operation_definition` → `Operation` (anonymous → `anon#n`,
//!   `metadata.operation_type` ∈ query|mutation|subscription) plus one
//!   `Relation{Queries, "<Query|Mutation|Subscription>.<field>"}` per ROOT
//!   selection field — the analyzer resolves the target by the SDL field's
//!   qualified name;
//! * `fragment_definition` → `Fragment`.
//!
//! Embedded documents (`gql`/`graphql` tagged templates and `gql(...)` calls in
//! TS/JS; `gql("...")` calls and operation-looking triple-quoted strings in
//! Python) are blanked of `${…}` substitutions with offset-preserving spaces,
//! parsed with the same grammar, and yield the same `Operation`/`Fragment`
//! symbols with the HOST path in the id (`graphql:{host_path}:operation:{name}`)
//! and host line numbers. Each also gets `Relation{source: enclosing host
//! symbol id (or "file:{path}"), Defines, target_name: op qualified name}`.
//! **Resolution of `Defines`:** the target is always a symbol of the SAME
//! file as the source, so the analyzer resolves `target_name` against the
//! qualified names of that file's `Operation`/`Fragment` symbols only — never
//! repo-wide.

use super::lang::LangSpec;
use super::walker::{enclosing_symbol, text};
use crate::domain::model::{EdgeKind, NodeKind};
use crate::ports::extractor::{FileExtraction, FileInput, Relation, Symbol};
use std::collections::HashMap;
use tree_sitter::{Node, Parser, Tree};

const MAX_DOC: usize = 1000;
const MAX_SIGNATURE: usize = 200;

static SPEC: LangSpec = LangSpec {
    language: || tree_sitter_graphql::LANGUAGE.into(),
    defs: &[],
    import_query: "",
    call_query: "",
    doc_comment_types: &[],
    body_types: &[],
    extras: &[definitions],
};

/// The GraphQL language specification (SDL and executable documents).
pub fn spec() -> Option<&'static LangSpec> {
    Some(&SPEC)
}

/// Where a parsed document sits inside its host file.
#[derive(Clone, Copy)]
struct Placement {
    /// Byte offset of the document's first byte in the host content.
    byte_offset: usize,
    /// 0-based row of the document's first byte in the host content.
    row_offset: u32,
}

const TOP_LEVEL: Placement = Placement {
    byte_offset: 0,
    row_offset: 0,
};

/// Hands out `#n` suffixes for duplicate stable ids, in source order.
#[derive(Default)]
struct Ids(HashMap<String, usize>);

impl Ids {
    fn next(&mut self, base: String) -> String {
        let n = self.0.entry(base.clone()).or_insert(0);
        *n += 1;
        if *n == 1 {
            base
        } else {
            format!("{base}#{n}")
        }
    }
}

fn child_of_type<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut cursor = node.walk();
    let found = node.named_children(&mut cursor).find(|c| c.kind() == kind);
    found
}

fn children_of_type<'t>(node: Node<'t>, kind: &str) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|c| c.kind() == kind)
        .collect()
}

fn description(node: Node, src: &str) -> Option<String> {
    let d = child_of_type(node, "description")?;
    let raw = text(d, src).trim();
    let inner = raw
        .strip_prefix("\"\"\"")
        .and_then(|s| s.strip_suffix("\"\"\""))
        .or_else(|| raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .unwrap_or(raw);
    let lines: Vec<&str> = inner.lines().map(str::trim).collect();
    let doc = lines.join("\n").trim().to_string();
    (!doc.is_empty()).then(|| doc.chars().take(MAX_DOC).collect())
}

/// Declaration text after the description, up to the first `{`, first line only.
fn signature(node: Node, src: &str) -> String {
    let start = child_of_type(node, "description")
        .map(|d| d.end_byte())
        .unwrap_or(node.start_byte());
    let head = &src[start..node.end_byte()];
    let head = head.split('{').next().unwrap_or("");
    let first = head.trim().lines().next().unwrap_or("").trim();
    first.chars().take(MAX_SIGNATURE).collect()
}

/// A document being extracted: its text, the host path and where it sits.
#[derive(Clone, Copy)]
struct Doc<'a> {
    src: &'a str,
    path: &'a str,
    place: Placement,
}

/// Accumulates symbols/relations across the documents of one file.
#[derive(Default)]
struct Sink {
    ids: Ids,
    anon: usize,
    symbols: Vec<Symbol>,
    relations: Vec<Relation>,
}

fn make_symbol(
    doc: Doc,
    sink: &mut Sink,
    node: Node,
    kind: NodeKind,
    (name, qualified): (String, String),
    parent: Option<String>,
    segments_from: Option<Node>,
) -> Symbol {
    let Doc { src, path, place } = doc;
    let stable_id = sink
        .ids
        .next(format!("graphql:{path}:{}:{qualified}", kind.as_str()));
    let segments = segments_from
        .map(|b| {
            let mut cursor = b.walk();
            b.named_children(&mut cursor)
                .map(|c| {
                    (
                        c.start_byte() + place.byte_offset,
                        c.end_byte() + place.byte_offset,
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    Symbol {
        stable_id,
        kind,
        name,
        qualified_name: qualified,
        parent,
        signature: Some(signature(node, src)),
        doc: description(node, src),
        line_start: node.start_position().row as u32 + 1 + place.row_offset,
        line_end: node.end_position().row as u32 + 1 + place.row_offset,
        byte_start: node.start_byte() + place.byte_offset,
        byte_end: node.end_byte() + place.byte_offset,
        segments,
        metadata: serde_json::json!({}),
    }
}

fn name_of(node: Node, src: &str) -> Option<String> {
    child_of_type(node, "name").map(|n| text(n, src).trim().to_string())
}

/// One pass over a parsed document: type-system definitions (when `sdl`),
/// operations and fragments. Appends to `symbols`/`relations`.
fn collect(tree: &Tree, doc: Doc, sdl: bool, sink: &mut Sink) {
    let src = doc.src;
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        let kind = n.kind();
        let type_kind = match kind {
            "object_type_definition" | "input_object_type_definition" => Some(NodeKind::Type),
            "interface_type_definition" => Some(NodeKind::Interface),
            "enum_type_definition" => Some(NodeKind::Enum),
            "union_type_definition" | "scalar_type_definition" => Some(NodeKind::TypeAlias),
            _ => None,
        };
        if let Some(k) = type_kind {
            if sdl {
                if let Some(name) = name_of(n, src) {
                    let fields = child_of_type(n, "fields_definition")
                        .or_else(|| child_of_type(n, "input_fields_definition"))
                        .or_else(|| child_of_type(n, "enum_values_definition"));
                    let sym =
                        make_symbol(doc, sink, n, k, (name.clone(), name.clone()), None, fields);
                    let parent_id = sym.stable_id.clone();
                    sink.symbols.push(sym);
                    if let Some(fields) = fields.filter(|f| f.kind() == "fields_definition") {
                        for f in children_of_type(fields, "field_definition") {
                            let Some(fname) = name_of(f, src) else {
                                continue;
                            };
                            let qualified = format!("{name}.{fname}");
                            let sym = make_symbol(
                                doc,
                                sink,
                                f,
                                NodeKind::Field,
                                (fname, qualified),
                                Some(parent_id.clone()),
                                None,
                            );
                            sink.symbols.push(sym);
                        }
                    }
                }
            }
            continue;
        }
        match kind {
            "operation_definition" => {
                let op_type = child_of_type(n, "operation_type")
                    .map(|t| text(t, src).trim().to_string())
                    .unwrap_or_else(|| "query".to_string());
                let name = name_of(n, src).unwrap_or_else(|| {
                    sink.anon += 1;
                    format!("anon#{}", sink.anon)
                });
                let selection = child_of_type(n, "selection_set");
                let mut sym = make_symbol(
                    doc,
                    sink,
                    n,
                    NodeKind::Operation,
                    (name.clone(), name.clone()),
                    None,
                    selection,
                );
                sym.metadata = serde_json::json!({ "operation_type": op_type });
                let root = match op_type.as_str() {
                    "mutation" => "Mutation",
                    "subscription" => "Subscription",
                    _ => "Query",
                };
                if let Some(sel) = selection {
                    for s in children_of_type(sel, "selection") {
                        let Some(field) = child_of_type(s, "field") else {
                            continue;
                        };
                        if let Some(fname) = name_of(field, src) {
                            sink.relations.push(Relation {
                                source: sym.stable_id.clone(),
                                kind: EdgeKind::Queries,
                                target_name: format!("{root}.{fname}"),
                            });
                        }
                    }
                }
                sink.symbols.push(sym);
                continue;
            }
            "fragment_definition" => {
                let name = child_of_type(n, "fragment_name")
                    .and_then(|f| name_of(f, src))
                    .unwrap_or_else(|| "fragment".to_string());
                let sym = make_symbol(
                    doc,
                    sink,
                    n,
                    NodeKind::Fragment,
                    (name.clone(), name),
                    None,
                    child_of_type(n, "selection_set"),
                );
                sink.symbols.push(sym);
                continue;
            }
            _ => {}
        }
        let mut cursor = n.walk();
        let kids: Vec<Node> = n.named_children(&mut cursor).collect();
        stack.extend(kids.into_iter().rev());
    }
}

/// The GraphQL `ExtraPass`: all definitions of a standalone document, and the
/// file's free ranges recomputed around them.
fn definitions(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let mut sink = Sink::default();
    collect(
        tree,
        Doc {
            src: &file.content,
            path: &file.path,
            place: TOP_LEVEL,
        },
        true,
        &mut sink,
    );
    let Sink {
        symbols, relations, ..
    } = sink;
    let owned: Vec<(usize, usize)> = symbols
        .iter()
        .filter(|s| s.parent.is_none())
        .map(|s| (s.byte_start, s.byte_end))
        .collect();
    // The grammar wraps every definition in `source_file > document > definition`.
    let root = tree.root_node();
    let doc = child_of_type(root, "document").unwrap_or(root);
    let mut cursor = doc.walk();
    out.free_ranges = doc
        .named_children(&mut cursor)
        .filter(|n| {
            !owned
                .iter()
                .any(|(s, e)| n.start_byte() >= *s && n.end_byte() <= *e)
        })
        .map(|n| (n.start_byte(), n.end_byte()))
        .collect();
    out.symbols = symbols;
    out.relations.extend(relations);
}

fn parse_graphql(doc: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_graphql::LANGUAGE.into())
        .ok()?;
    parser.parse(doc, None)
}

/// Whether a string looks like an executable GraphQL document.
fn looks_like_document(doc: &str) -> bool {
    let t = doc.trim_start();
    ["query", "mutation", "subscription", "fragment", "{"]
        .iter()
        .any(|k| t.starts_with(k))
}

/// One embedded document found in a host file: its text with `${…}` blanked,
/// placed at `byte_offset`/`row_offset` of the host content.
struct Embedded {
    doc: String,
    place: Placement,
}

/// Parse `embedded` documents and add their operations/fragments (host path,
/// host lines) plus `Defines` relations from the enclosing host symbol.
fn add_embedded(embedded: Vec<Embedded>, file: &FileInput, out: &mut FileExtraction) {
    let mut sink = Sink::default();
    for e in embedded {
        let Some(tree) = parse_graphql(&e.doc) else {
            continue;
        };
        if tree.root_node().has_error() {
            out.warnings.push(format!(
                "embedded GraphQL at {}:{} has syntax errors (partial extraction)",
                file.path,
                e.place.row_offset + 1
            ));
        }
        let owner = enclosing_symbol(&out.symbols, &file.path, e.place.byte_offset);
        let first = sink.symbols.len();
        collect(
            &tree,
            Doc {
                src: &e.doc,
                path: &file.path,
                place: e.place,
            },
            false,
            &mut sink,
        );
        for s in &sink.symbols[first..] {
            sink.relations.push(Relation {
                source: owner.clone(),
                kind: EdgeKind::Defines,
                target_name: s.qualified_name.clone(),
            });
        }
    }
    out.symbols.extend(sink.symbols);
    out.relations.extend(sink.relations);
    out.symbols.sort_by_key(|s| s.byte_start);
}

/// Interior of a JS/TS `template_string` with every `template_substitution`
/// replaced by spaces of the same byte length.
fn blank_template(tpl: Node, src: &str) -> Embedded {
    let inner_start = tpl.start_byte() + 1;
    let inner_end = tpl.end_byte().saturating_sub(1).max(inner_start);
    let mut doc = src[inner_start..inner_end].to_string();
    let mut cursor = tpl.walk();
    for sub in tpl
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "template_substitution")
    {
        let (s, e) = (sub.start_byte() - inner_start, sub.end_byte() - inner_start);
        let blank: String = src[sub.byte_range()]
            .chars()
            .map(|c| if c == '\n' { '\n' } else { ' ' })
            .collect();
        doc.replace_range(s..e, &blank);
    }
    Embedded {
        doc,
        place: Placement {
            byte_offset: inner_start,
            row_offset: tpl.start_position().row as u32,
        },
    }
}

fn is_gql_name(s: &str) -> bool {
    matches!(s, "gql" | "graphql")
}

/// TS/JS `ExtraPass`: `gql\`…\``, `graphql\`…\``, `gql(\`…\`)`.
pub fn embedded_ts(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        if n.kind() == "call_expression" {
            let callee = n.child_by_field_name("function");
            let args = n.child_by_field_name("arguments");
            if let (Some(f), Some(a)) = (callee, args) {
                if f.kind() == "identifier" && is_gql_name(text(f, src)) {
                    let tpl = if a.kind() == "template_string" {
                        Some(a)
                    } else {
                        child_of_type(a, "template_string")
                    };
                    if let Some(tpl) = tpl {
                        found.push(blank_template(tpl, src));
                        continue;
                    }
                }
            }
        }
        let mut cursor = n.walk();
        let kids: Vec<Node> = n.named_children(&mut cursor).collect();
        stack.extend(kids.into_iter().rev());
    }
    add_embedded(found, file, out);
}

/// Python `ExtraPass`: `gql("…")` calls and any non-f string whose trimmed
/// content starts with `query|mutation|subscription|fragment|{`.
pub fn embedded_python(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        if n.kind() == "string" {
            let start = child_of_type(n, "string_start");
            let end = child_of_type(n, "string_end");
            if let (Some(s), Some(e)) = (start, end) {
                let prefix = text(s, src).to_ascii_lowercase();
                let in_gql_call = n
                    .parent()
                    .filter(|p| p.kind() == "argument_list")
                    .and_then(|p| p.parent())
                    .and_then(|c| c.child_by_field_name("function"))
                    .is_some_and(|f| f.kind() == "identifier" && is_gql_name(text(f, src)));
                let inner = &src[s.end_byte()..e.start_byte()];
                if !prefix.contains('f') && (in_gql_call || looks_like_document(inner)) {
                    found.push(Embedded {
                        doc: inner.to_string(),
                        place: Placement {
                            byte_offset: s.end_byte(),
                            row_offset: s.end_position().row as u32,
                        },
                    });
                }
            }
            continue;
        }
        let mut cursor = n.walk();
        let kids: Vec<Node> = n.named_children(&mut cursor).collect();
        stack.extend(kids.into_iter().rev());
    }
    add_embedded(found, file, out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::Language;

    fn gql(path: &str, src: &str) -> FileExtraction {
        let file = FileInput {
            path: path.into(),
            language: Language::GraphQL,
            content: src.into(),
        };
        super::super::walker::extract(spec().unwrap(), &file)
    }

    const SCHEMA: &str = r#"
"""
A registered user.
"""
type User implements Node {
  id: ID!
  "Display name"
  name: String
}

interface Node { id: ID! }

enum Role { ADMIN USER }

input NewUser { name: String! }

union SearchResult = User | Post

scalar DateTime

type Query {
  user(id: ID!): User
  users: [User!]!
}

type Mutation {
  createUser(input: NewUser!): User
}
"#;

    #[test]
    fn sdl_types_fields_and_docs() {
        let x = gql("schema.graphql", SCHEMA);
        let ids: Vec<&str> = x.symbols.iter().map(|s| s.stable_id.as_str()).collect();
        assert!(ids.contains(&"graphql:schema.graphql:type:User"));
        assert!(ids.contains(&"graphql:schema.graphql:field:User.id"));
        assert!(ids.contains(&"graphql:schema.graphql:interface:Node"));
        assert!(ids.contains(&"graphql:schema.graphql:enum:Role"));
        assert!(ids.contains(&"graphql:schema.graphql:type:NewUser"));
        assert!(ids.contains(&"graphql:schema.graphql:type_alias:SearchResult"));
        assert!(ids.contains(&"graphql:schema.graphql:type_alias:DateTime"));
        assert!(ids.contains(&"graphql:schema.graphql:field:Query.user"));
        assert!(ids.contains(&"graphql:schema.graphql:field:Mutation.createUser"));
        let user = x.symbols.iter().find(|s| s.name == "User").unwrap();
        assert_eq!(user.doc.as_deref(), Some("A registered user."));
        assert_eq!(user.signature.as_deref(), Some("type User implements Node"));
        assert_eq!(user.segments.len(), 2);
        // The description belongs to the type node, so the range starts at it.
        assert_eq!((user.line_start, user.line_end), (2, 9));
        let name = x
            .symbols
            .iter()
            .find(|s| s.qualified_name == "User.name")
            .unwrap();
        assert_eq!(name.doc.as_deref(), Some("Display name"));
        assert_eq!(
            name.parent.as_deref(),
            Some("graphql:schema.graphql:type:User")
        );
        assert!(x.free_ranges.is_empty(), "{:?}", x.free_ranges);
        assert!(x.warnings.is_empty(), "{:?}", x.warnings);
    }

    #[test]
    fn operations_and_queries_relations() {
        let src = "query GetUser($id: ID!) {\n  user(id: $id) { ...UserFields }\n  users { id }\n}\n\nmutation { createUser(input: {name: \"a\"}) { id } }\n\nfragment UserFields on User { id name }\n\n{ users { id } }\n";
        let x = gql("ops/user.graphql", src);
        let ops: Vec<(&str, &str)> = x
            .symbols
            .iter()
            .map(|s| (s.stable_id.as_str(), s.kind.as_str()))
            .collect();
        assert_eq!(
            ops,
            [
                ("graphql:ops/user.graphql:operation:GetUser", "operation"),
                ("graphql:ops/user.graphql:operation:anon#1", "operation"),
                ("graphql:ops/user.graphql:fragment:UserFields", "fragment"),
                ("graphql:ops/user.graphql:operation:anon#2", "operation"),
            ]
        );
        assert_eq!(x.symbols[1].metadata["operation_type"], "mutation");
        assert_eq!(x.symbols[3].metadata["operation_type"], "query");
        let rels: Vec<(&str, &str)> = x
            .relations
            .iter()
            .map(|r| (r.source.as_str(), r.target_name.as_str()))
            .collect();
        assert_eq!(
            rels,
            [
                ("graphql:ops/user.graphql:operation:GetUser", "Query.user"),
                ("graphql:ops/user.graphql:operation:GetUser", "Query.users"),
                (
                    "graphql:ops/user.graphql:operation:anon#1",
                    "Mutation.createUser"
                ),
                ("graphql:ops/user.graphql:operation:anon#2", "Query.users"),
            ]
        );
        assert!(x.relations.iter().all(|r| r.kind == EdgeKind::Queries));
        // The schema's field ids end with exactly those names.
        let schema = gql("schema.graphql", SCHEMA);
        for r in &x.relations {
            assert!(
                schema
                    .symbols
                    .iter()
                    .any(|s| s.qualified_name == r.target_name),
                "{} not in schema",
                r.target_name
            );
        }
    }

    #[test]
    fn embedded_in_tsx_component() {
        let src = "import { gql, useQuery } from '@apollo/client';\n\nconst FIELDS = gql`\n  fragment F on User { id }\n`;\n\nexport function Profile({ id }: { id: string }) {\n  const q = gql(`\n    query Profile($id: ID!) {\n      user(id: $id) { ...F }\n      ${FIELDS}\n    }\n  `);\n  const { data } = useQuery(q, { variables: { id } });\n  return <div>{data?.user?.id}</div>;\n}\n";
        let file = FileInput {
            path: "src/Profile.tsx".into(),
            language: Language::TypeScript,
            content: src.into(),
        };
        let x = super::super::walker::extract(
            super::super::typescript::spec(Language::TypeScript, &file.path).unwrap(),
            &file,
        );
        let frag = x
            .symbols
            .iter()
            .find(|s| s.kind == NodeKind::Fragment)
            .expect("fragment");
        assert_eq!(frag.stable_id, "graphql:src/Profile.tsx:fragment:F");
        assert_eq!(frag.line_start, 4);
        let op = x
            .symbols
            .iter()
            .find(|s| s.kind == NodeKind::Operation)
            .expect("operation");
        assert_eq!(op.stable_id, "graphql:src/Profile.tsx:operation:Profile");
        assert_eq!((op.line_start, op.line_end), (9, 12));
        assert_eq!(&src[op.byte_start..op.byte_start + 13], "query Profile");
        assert!(x.relations.contains(&Relation {
            source: "typescript:src/Profile.tsx:function:Profile".into(),
            kind: EdgeKind::Defines,
            target_name: "Profile".into(),
        }));
        assert!(x.relations.contains(&Relation {
            source: "file:src/Profile.tsx".into(),
            kind: EdgeKind::Defines,
            target_name: "F".into(),
        }));
        assert!(x.relations.contains(&Relation {
            source: "graphql:src/Profile.tsx:operation:Profile".into(),
            kind: EdgeKind::Queries,
            target_name: "Query.user".into(),
        }));
        assert!(x.warnings.is_empty(), "{:?}", x.warnings);
    }

    #[test]
    fn embedded_in_python_module() {
        let src = "from gql import gql\n\nLIST = gql(\"\"\"\n  query ListUsers { users { id } }\n\"\"\")\n\nclass Client:\n    def create(self):\n        doc = '''\n        mutation Create { createUser(input: {name: \"x\"}) { id } }\n        '''\n        return doc\n\nNOT_GQL = \"hello\"\nF = f\"query {LIST}\"\n";
        let file = FileInput {
            path: "app/api.py".into(),
            language: Language::Python,
            content: src.into(),
        };
        let x = super::super::walker::extract(super::super::python::spec().unwrap(), &file);
        let ops: Vec<&str> = x
            .symbols
            .iter()
            .filter(|s| s.kind == NodeKind::Operation)
            .map(|s| s.stable_id.as_str())
            .collect();
        assert_eq!(
            ops,
            [
                "graphql:app/api.py:operation:ListUsers",
                "graphql:app/api.py:operation:Create"
            ]
        );
        let create = x.symbols.iter().find(|s| s.name == "Create").unwrap();
        assert_eq!(create.line_start, 10);
        assert!(x.relations.contains(&Relation {
            source: "python:app/api.py:method:Client::create".into(),
            kind: EdgeKind::Defines,
            target_name: "Create".into(),
        }));
        assert!(x.relations.contains(&Relation {
            source: "file:app/api.py".into(),
            kind: EdgeKind::Defines,
            target_name: "ListUsers".into(),
        }));
        assert!(x
            .relations
            .iter()
            .any(|r| r.target_name == "Mutation.createUser"));
    }
}
