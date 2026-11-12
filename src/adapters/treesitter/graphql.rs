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
//! TS/JS; `gql("...")` calls and operation-looking strings in Python, an
//! implicit concatenation `"a " "b"` being one string) are blanked of `${…}`
//! substitutions / f-string `{…}` interpolations with offset-preserving spaces,
//! parsed with the same grammar, and yield the same `Operation`/`Fragment`
//! symbols with the HOST path in the id (`graphql:{host_path}:operation:{name}`)
//! and host line numbers. Each also gets `Relation{source: enclosing host
//! symbol id (or "file:{path}"), Defines, target_name: the op's stable id}`
//! (the id, not the name: two same-named operations in one file, `Op` and
//! `Op#2`, each keep their own host).
//! **Resolution of `Defines`:** the target is always a symbol of the SAME
//! file as the source, so the analyzer resolves `target_name` against the
//! stable ids (then the unique qualified name) of that file's
//! `Operation`/`Fragment` symbols only — never repo-wide.

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

/// Text of `node` without its `description` children, whitespace collapsed.
fn without_descriptions(node: Node, src: &str) -> String {
    let mut out = String::new();
    let mut cursor = node.walk();
    let kids: Vec<Node> = node.named_children(&mut cursor).collect();
    let mut at = node.start_byte();
    for k in kids.iter().filter(|k| k.kind() == "description") {
        out.push_str(&src[at..k.start_byte()]);
        at = k.end_byte();
    }
    out.push_str(&src[at..node.end_byte()]);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One-line header of an SDL field: `name(arg: Type = default, …): Type
/// @directives`, descriptions dropped — what a client sees of the field.
fn field_header(field: Node, src: &str) -> String {
    let mut cursor = field.walk();
    let mut out = String::new();
    for c in field.named_children(&mut cursor) {
        match c.kind() {
            "name" => out.push_str(text(c, src).trim()),
            "arguments_definition" => {
                let mut ac = c.walk();
                let args: Vec<String> = c
                    .named_children(&mut ac)
                    .filter(|a| a.kind() == "input_value_definition")
                    .map(|a| without_descriptions(a, src))
                    .collect();
                out.push('(');
                out.push_str(&args.join(", "));
                out.push(')');
            }
            "type" => {
                out.push_str(": ");
                out.push_str(&text(c, src).split_whitespace().collect::<String>());
            }
            "directives" => {
                out.push(' ');
                out.push_str(
                    &text(c, src)
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                );
            }
            _ => {}
        }
    }
    out
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
                            let mut sym = make_symbol(
                                doc,
                                sink,
                                f,
                                NodeKind::Field,
                                (fname, qualified),
                                Some(parent_id.clone()),
                                None,
                            );
                            sym.metadata = serde_json::json!({ "header": field_header(f, src) });
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
                let root = match op_type.as_str() {
                    "mutation" => "Mutation",
                    "subscription" => "Subscription",
                    _ => "Query",
                };
                // Root fields are also kept on the node: a `Queries` edge only
                // resolves inside one repo, while cross-repo correlation needs
                // the field names an operation calls on another repo's schema.
                let mut root_fields: Vec<String> = Vec::new();
                if let Some(sel) = selection {
                    for s in children_of_type(sel, "selection") {
                        let Some(field) = child_of_type(s, "field") else {
                            continue;
                        };
                        if let Some(fname) = name_of(field, src) {
                            let qualified = format!("{root}.{fname}");
                            if !root_fields.contains(&qualified) {
                                root_fields.push(qualified.clone());
                            }
                            sink.relations.push(Relation {
                                source: sym.stable_id.clone(),
                                kind: EdgeKind::Queries,
                                target_name: qualified,
                            });
                        }
                    }
                }
                sym.metadata =
                    serde_json::json!({ "operation_type": op_type, "root_fields": root_fields });
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
                target_name: s.stable_id.clone(),
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

/// Whether a Python string node is the argument of a `gql(...)` / `graphql(...)` call.
fn in_python_gql_call(n: Node, src: &str) -> bool {
    n.parent()
        .filter(|p| p.kind() == "argument_list")
        .and_then(|p| p.parent())
        .and_then(|c| c.child_by_field_name("function"))
        .is_some_and(|f| f.kind() == "identifier" && is_gql_name(text(f, src)))
}

/// A byte of a GraphQL name, number, `...` spread or `$` / `@` prefix.
fn token_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-' | b'$' | b'@')
}

/// Whether blanks between `a` (the end of one piece) and `b` (the start of
/// the next) would split one token that Python's joined value keeps whole.
fn splits_token(a: u8, b: u8) -> bool {
    token_byte(a) && (b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.'))
}

/// A Python implicit concatenation (`"mutation X " "{ x }"`, pieces possibly
/// f-strings) as ONE document: the source from the first piece to the last
/// with every byte outside string contents blanked to a space (newlines kept),
/// f-string `{…}` interpolations blanked and `{{`/`}}` read as `{`/`}`, so
/// offsets and host lines are preserved. A `(…)` holding nothing but blanked
/// interpolations (`DecryptDataKey({', '.join(decls)})`) is blanked too, so
/// the rest parses cleanly. A token split across pieces (`"q" "uery Q"`,
/// which Python reads as `query Q`) is joined by swapping its leading
/// fragment with the blanked gap after it: only that fragment moves, onto the
/// line of the piece that continues it. `None` unless it sits in a
/// `gql(...)` call or looks like an executable document.
fn concatenated_python(n: Node, src: &str) -> Option<Embedded> {
    let pieces = children_of_type(n, "string");
    let (first, last) = (pieces.first()?, pieces.last()?);
    let (start, end) = (first.start_byte(), last.end_byte());
    let mut bytes = src.as_bytes()[start..end].to_vec();
    let mut keep = vec![false; bytes.len()];
    let mut interpolated = vec![false; bytes.len()];
    let mut contents: Vec<(usize, usize)> = Vec::with_capacity(pieces.len());
    for p in &pieces {
        let (Some(s), Some(e)) = (
            child_of_type(*p, "string_start"),
            child_of_type(*p, "string_end"),
        ) else {
            return None;
        };
        contents.push((s.end_byte() - start, e.start_byte() - start));
        keep[s.end_byte() - start..e.start_byte() - start].fill(true);
        if !text(s, src).to_ascii_lowercase().contains('f') {
            continue;
        }
        let mut stack = vec![*p];
        while let Some(x) = stack.pop() {
            match x.kind() {
                "interpolation" => {
                    keep[x.start_byte() - start..x.end_byte() - start].fill(false);
                    interpolated[x.start_byte() - start..x.end_byte() - start].fill(true);
                }
                // `{{` / `}}` is one literal brace: keep the first, blank the second.
                "escape_interpolation" if x.end_byte() - x.start_byte() == 2 => {
                    bytes[x.start_byte() + 1 - start] = b' ';
                }
                _ => {
                    let mut cursor = x.walk();
                    stack.extend(x.named_children(&mut cursor));
                }
            }
        }
    }
    for (b, k) in bytes.iter_mut().zip(&keep) {
        if !*k && *b != b'\n' {
            *b = b' ';
        }
    }
    // `( <blanked interpolation> )` → blank the parentheses as well.
    for open in 0..bytes.len() {
        if bytes[open] != b'(' || !keep[open] {
            continue;
        }
        let mut close = open + 1;
        while close < bytes.len() && bytes[close].is_ascii_whitespace() {
            close += 1;
        }
        if close < bytes.len()
            && bytes[close] == b')'
            && interpolated[open + 1..close].iter().any(|i| *i)
        {
            bytes[open] = b' ';
            bytes[close] = b' ';
        }
    }
    // Split tokens: every byte between two contents is blanked ASCII by now.
    let mut prev_end: Option<usize> = None;
    for &(cs, ce) in &contents {
        if cs == ce {
            continue;
        }
        if let Some(pe) = prev_end.filter(|pe| splits_token(bytes[pe - 1], bytes[cs])) {
            let mut fs = pe;
            while fs > 0 && token_byte(bytes[fs - 1]) {
                fs -= 1;
            }
            bytes[fs..cs].rotate_left(pe - fs);
        }
        prev_end = Some(ce);
    }
    let doc = String::from_utf8(bytes).ok()?;
    (in_python_gql_call(n, src) || looks_like_document(&doc)).then_some(Embedded {
        doc,
        place: Placement {
            byte_offset: start,
            row_offset: first.start_position().row as u32,
        },
    })
}

/// Python `ExtraPass`: `gql("…")` calls and any non-f string whose trimmed
/// content starts with `query|mutation|subscription|fragment|{`; an implicit
/// concatenation of strings is one document ([`concatenated_python`]).
pub fn embedded_python(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        if n.kind() == "concatenated_string" {
            found.extend(concatenated_python(n, src));
            continue;
        }
        if n.kind() == "string" {
            let start = child_of_type(n, "string_start");
            let end = child_of_type(n, "string_end");
            if let (Some(s), Some(e)) = (start, end) {
                let prefix = text(s, src).to_ascii_lowercase();
                let inner = &src[s.end_byte()..e.start_byte()];
                if !prefix.contains('f')
                    && (in_python_gql_call(n, src) || looks_like_document(inner))
                {
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
    fn field_header_is_one_line_without_descriptions() {
        let src = "type Mutation {\n  \"Create a lab.\"\n  createLab(\n    \"The lab's on-chain id.\"\n    oclId: ID!\n    \"Optional title.\"\n    title: String = \"x\"\n  ): Lab!\n    @aws_lambda @aws_api_key\n}\n";
        let x = gql("schema.graphql", src);
        let f = x
            .symbols
            .iter()
            .find(|s| s.qualified_name == "Mutation.createLab")
            .unwrap();
        assert_eq!(
            f.metadata["header"],
            "createLab(oclId: ID!, title: String = \"x\"): Lab! @aws_lambda @aws_api_key"
        );
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
        assert_eq!(
            x.symbols[0].metadata["root_fields"],
            serde_json::json!(["Query.user", "Query.users"])
        );
        assert_eq!(
            x.symbols[1].metadata["root_fields"],
            serde_json::json!(["Mutation.createUser"])
        );
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
            target_name: "graphql:src/Profile.tsx:operation:Profile".into(),
        }));
        assert!(x.relations.contains(&Relation {
            source: "file:src/Profile.tsx".into(),
            kind: EdgeKind::Defines,
            target_name: "graphql:src/Profile.tsx:fragment:F".into(),
        }));
        assert!(x.relations.contains(&Relation {
            source: "graphql:src/Profile.tsx:operation:Profile".into(),
            kind: EdgeKind::Queries,
            target_name: "Query.user".into(),
        }));
        assert!(x.warnings.is_empty(), "{:?}", x.warnings);
    }

    fn py(src: &str) -> FileExtraction {
        let file = FileInput {
            path: "mcp/server.py".into(),
            language: Language::Python,
            content: src.into(),
        };
        super::super::walker::extract(super::super::python::spec().unwrap(), &file)
    }

    fn operations(x: &FileExtraction) -> Vec<&Symbol> {
        x.symbols
            .iter()
            .filter(|s| s.kind == NodeKind::Operation)
            .collect()
    }

    /// molecule-plugin mcp/server.py: one mutation written as three
    /// implicitly concatenated literals.
    #[test]
    fn python_concatenated_strings_are_one_document() {
        let src = "def issue(serviceName, expiresIn, addr, message_signature):\n    tok = labs_graphql_call(\n        \"mutation GenerateServiceToken($serviceName: String!, $expiresIn: String!, $walletAddress: String, $messageSignature: String) \"\n        \"{ generateServiceToken(serviceName: $serviceName, expiresIn: $expiresIn, walletAddress: $walletAddress, messageSignature: $messageSignature) \"\n        \"{ token tokenId serviceName expiresAt isSuccess message } }\",\n        {\"serviceName\": serviceName},\n    )\n    return tok\n";
        let x = py(src);
        let ops = operations(&x);
        assert_eq!(ops.len(), 1, "{:?}", ops);
        assert_eq!(
            ops[0].stable_id,
            "graphql:mcp/server.py:operation:GenerateServiceToken"
        );
        assert_eq!(ops[0].metadata["operation_type"], "mutation");
        assert_eq!(
            ops[0].metadata["root_fields"],
            serde_json::json!(["Mutation.generateServiceToken"])
        );
        assert_eq!((ops[0].line_start, ops[0].line_end), (3, 5));
        assert_eq!(
            &src[ops[0].byte_start..ops[0].byte_start + 30],
            "mutation GenerateServiceToken("
        );
        assert!(x.relations.contains(&Relation {
            source: "python:mcp/server.py:function:issue".into(),
            kind: EdgeKind::Defines,
            target_name: "graphql:mcp/server.py:operation:GenerateServiceToken".into(),
        }));
        assert!(!x.symbols.iter().any(|s| s.name.starts_with("anon#")));
    }

    /// molecule-plugin mcp/server.py: `issue_service_token` and
    /// `issue_owner_service_token` each send `GetServiceSignInMessage`. The
    /// second copy is `…#2`, and each `Defines` names its own copy's id, so
    /// neither host loses its operation to a same-name tie.
    #[test]
    fn same_named_operations_keep_their_own_host() {
        let call = "    return labs_graphql_call(\n        \"query GetServiceSignInMessage($w: String!) \"\n        \"{ getServiceSignInMessage(walletAddress: $w) { message } }\",\n        {},\n    )\n";
        let src = format!(
            "def issue_service_token(w):\n{call}\n\ndef issue_owner_service_token(w):\n{call}"
        );
        let x = py(&src);
        let ids: Vec<&str> = operations(&x)
            .iter()
            .map(|s| s.stable_id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "graphql:mcp/server.py:operation:GetServiceSignInMessage",
                "graphql:mcp/server.py:operation:GetServiceSignInMessage#2"
            ]
        );
        let defines: Vec<(&str, &str)> = x
            .relations
            .iter()
            .filter(|r| r.kind == EdgeKind::Defines)
            .map(|r| (r.source.as_str(), r.target_name.as_str()))
            .collect();
        assert_eq!(
            defines,
            [
                (
                    "python:mcp/server.py:function:issue_service_token",
                    "graphql:mcp/server.py:operation:GetServiceSignInMessage"
                ),
                (
                    "python:mcp/server.py:function:issue_owner_service_token",
                    "graphql:mcp/server.py:operation:GetServiceSignInMessage#2"
                ),
            ]
        );
    }

    /// molecule-plugin mcp/server.py: an f-string piece with interpolated
    /// argument lists and `{{` escapes, then a plain piece.
    #[test]
    fn python_fstring_pieces_blank_interpolations() {
        let src = "def decrypt(arg_decls, arg_uses):\n    query = (\n        f\"mutation DecryptDataKey({', '.join(arg_decls)}) {{ decryptDataKey({', '.join(arg_uses)}) \"\n        \"{ isSuccess plaintextDEK iv message error { message code retryable } } }\"\n    )\n    return query\n";
        let x = py(src);
        let ops = operations(&x);
        assert_eq!(ops.len(), 1, "{:?}", ops);
        assert_eq!(ops[0].name, "DecryptDataKey");
        assert_eq!(
            ops[0].metadata["root_fields"],
            serde_json::json!(["Mutation.decryptDataKey"])
        );
        assert_eq!(ops[0].line_start, 3);
        // The interpolated argument lists leave no `( )` behind.
        assert!(x.warnings.is_empty(), "{:?}", x.warnings);
        // A lone f-string is still never a document.
        let lone = py("Q = f\"query {{ user(id: {uid}) {{ id }} }}\"\n");
        assert!(operations(&lone).is_empty());
    }

    /// Python joins `"q" "uery Q { a }"` with nothing between the pieces.
    #[test]
    fn python_tokens_split_across_pieces_are_joined() {
        let x = py("Q = gql(\"q\" \"uery Q { a }\")\n");
        let ops = operations(&x);
        assert_eq!(ops.len(), 1, "{:?}", ops);
        assert_eq!(ops[0].stable_id, "graphql:mcp/server.py:operation:Q");
        assert_eq!(
            ops[0].metadata["root_fields"],
            serde_json::json!(["Query.a"])
        );
        assert!(x.warnings.is_empty(), "{:?}", x.warnings);

        // Split names over several lines, an empty piece, an f-string piece:
        // the operation keeps its host lines; only the leading fragments move.
        let src = "def f(n):\n    doc = (\n        \"mutation Up\"\n        \"\"\n        \"date($id: I\"\n        f\"D!) {{ upd\"\n        \"ate(id: $id, n: 1) { id } }\"\n    )\n    return doc\n";
        let x = py(src);
        let ops = operations(&x);
        assert_eq!(ops.len(), 1, "{:?}", ops);
        assert_eq!(ops[0].name, "Update");
        assert_eq!(
            ops[0].metadata["root_fields"],
            serde_json::json!(["Mutation.update"])
        );
        assert_eq!((ops[0].line_start, ops[0].line_end), (3, 7));
        assert_eq!(&src[ops[0].byte_start..ops[0].byte_start + 8], "mutation");
        assert!(src[..ops[0].byte_end].ends_with('}'));
        assert!(x.warnings.is_empty(), "{:?}", x.warnings);

        // Pieces that end and start on a separator are untouched.
        assert!(splits_token(b'p', b'd') && splits_token(b'$', b'i'));
        assert!(!splits_token(b' ', b'a') && !splits_token(b'a', b'{'));
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
            target_name: "graphql:app/api.py:operation:Create".into(),
        }));
        assert!(x.relations.contains(&Relation {
            source: "file:app/api.py".into(),
            kind: EdgeKind::Defines,
            target_name: "graphql:app/api.py:operation:ListUsers".into(),
        }));
        assert!(x
            .relations
            .iter()
            .any(|r| r.target_name == "Mutation.createUser"));
    }
}
