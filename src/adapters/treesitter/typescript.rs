//! TypeScript / TSX / JavaScript `LangSpec`s.
//!
//! One shared rule table and query set drives all three grammars (they share
//! node names for everything queried here). Language-specific behaviour that
//! the generic walker cannot express lives in extra passes: function-valued
//! declarations (`const f = () => {}`, class-field arrows, anonymous default
//! exports), docs and ownership of `export`-wrapped declarations, class
//! heritage relations, call candidates and bare package names.

use super::lang::{DefRule, LangSpec};
use super::walker::{enclosing_symbol, file_id, text};
use crate::domain::model::{EdgeKind, Language, NodeKind};
use crate::ports::extractor::{CallCandidate, FileExtraction, FileInput, Relation, Symbol};
use tree_sitter::{Node, Query, QueryCursor, StreamingIterator, Tree};

const MAX_SIGNATURE: usize = 200;
const MAX_DOC: usize = 1000;

const fn rule(
    node_type: &'static str,
    kind: NodeKind,
    name_field: &'static str,
    container: bool,
) -> DefRule {
    DefRule {
        node_type,
        kind,
        name_field,
        container,
        methods_inside: container,
        method_when_nested: false,
    }
}

const DEFS: &[DefRule] = &[
    rule("function_declaration", NodeKind::Function, "name", false),
    rule(
        "generator_function_declaration",
        NodeKind::Function,
        "name",
        false,
    ),
    rule("class_declaration", NodeKind::Type, "name", true),
    rule("abstract_class_declaration", NodeKind::Type, "name", true),
    rule("method_definition", NodeKind::Method, "name", false),
    rule("interface_declaration", NodeKind::Interface, "name", true),
    rule("method_signature", NodeKind::Method, "name", false),
    rule("abstract_method_signature", NodeKind::Method, "name", false),
    rule("type_alias_declaration", NodeKind::TypeAlias, "name", false),
    rule("enum_declaration", NodeKind::Enum, "name", false),
];

/// `@names` on `named_imports` yields both names and aliases (`{a as b}` → a, b).
/// Default and namespace imports are whole-module imports (no names).
const IMPORT_QUERY: &str = r#"
(import_statement (import_clause . (named_imports) @names) source: (string (string_fragment) @specifier))
(import_statement (import_clause . (identifier) . (named_imports) @names) source: (string (string_fragment) @specifier))
(import_statement (import_clause . [(identifier) (namespace_import)] .) source: (string (string_fragment) @specifier))
(import_statement (import_clause . (identifier) . (namespace_import) .) source: (string (string_fragment) @specifier))
(import_statement . source: (string (string_fragment) @specifier))
(export_statement source: (string (string_fragment) @specifier))
(call_expression function: (identifier) @_fn arguments: (arguments (string (string_fragment) @specifier)) (#eq? @_fn "require"))
(call_expression function: (import) arguments: (arguments (string (string_fragment) @specifier)))
"#;

const CALL_QUERY: &str = r#"
(call_expression function: (identifier) @callee)
(call_expression function: (member_expression object: (_) @receiver property: (property_identifier) @callee))
(new_expression constructor: (identifier) @callee)
"#;

const BODY_TYPES: &[&str] = &[
    "statement_block",
    "class_body",
    "interface_body",
    "object_type",
    "enum_body",
];

const EXTRAS: &[super::lang::ExtraPass] = &[
    function_values,
    own_exports,
    calls,
    heritage_relations,
    packages,
    super::surface::typescript,
    super::graphql::embedded_ts,
];

macro_rules! spec {
    ($name:ident, $lang:expr) => {
        static $name: LangSpec = LangSpec {
            language: || $lang.into(),
            defs: DEFS,
            import_query: IMPORT_QUERY,
            call_query: "",
            doc_comment_types: &["comment"],
            body_types: BODY_TYPES,
            extras: EXTRAS,
        };
    };
}

spec!(TS, tree_sitter_typescript::LANGUAGE_TYPESCRIPT);
spec!(TSX, tree_sitter_typescript::LANGUAGE_TSX);
spec!(JS, tree_sitter_javascript::LANGUAGE);

/// Pick the grammar: `.tsx` → TSX, other TypeScript → TS, JavaScript → JS (JSX-capable).
pub fn spec(lang: Language, path: &str) -> Option<&'static LangSpec> {
    match lang {
        Language::TypeScript if path.to_ascii_lowercase().ends_with(".tsx") => Some(&TSX),
        Language::TypeScript => Some(&TS),
        Language::JavaScript => Some(&JS),
        _ => None,
    }
}

/// Pre-order, source-ordered traversal of named nodes.
fn walk_named<'t>(root: Node<'t>, mut f: impl FnMut(Node<'t>)) {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        f(n);
        let mut cursor = n.walk();
        let kids: Vec<Node> = n.named_children(&mut cursor).collect();
        stack.extend(kids.into_iter().rev());
    }
}

fn is_function_value(n: Node) -> bool {
    matches!(n.kind(), "arrow_function" | "function_expression")
}

/// Contiguous `comment` siblings directly above `node` (decorators skipped),
/// cleaned of markers. Returns the doc and the byte start of the first one.
fn leading_comments(node: Node, src: &str) -> (Option<String>, Option<usize>) {
    let mut lines: Vec<String> = Vec::new();
    let mut start = None;
    let mut expected_row = node.start_position().row;
    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev {
        if p.end_position().row + 1 < expected_row {
            break;
        }
        match p.kind() {
            "decorator" => start = Some(p.start_byte()),
            "comment" => {
                start = Some(p.start_byte());
                lines.extend(text(p, src).lines().rev().map(clean_comment_line));
            }
            _ => break,
        }
        expected_row = p.start_position().row;
        prev = p.prev_named_sibling();
    }
    lines.reverse();
    let doc = lines.join("\n").trim().to_string();
    let doc = (!doc.is_empty()).then(|| doc.chars().take(MAX_DOC).collect());
    (doc, start)
}

fn clean_comment_line(line: &str) -> String {
    let mut s = line.trim();
    for prefix in ["/**", "/*", "*/", "//", "*"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest;
            break;
        }
    }
    s.trim_end_matches("*/").trim().to_string()
}

/// First line of `outer` up to `body_start`, without trailing `=>`, `{` or `=`.
fn value_signature(outer: Node, body_start: usize, src: &str) -> String {
    let head = &src[outer.start_byte()..body_start.max(outer.start_byte())];
    let first: String = head
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(MAX_SIGNATURE)
        .collect();
    first
        .trim_end()
        .trim_end_matches("=>")
        .trim_end()
        .trim_end_matches('{')
        .trim_end()
        .trim_end_matches('=')
        .trim()
        .to_string()
}

fn body_segments(value: Node) -> Vec<(usize, usize)> {
    let Some(body) = value
        .child_by_field_name("body")
        .filter(|b| BODY_TYPES.contains(&b.kind()))
    else {
        return vec![];
    };
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .map(|c| (c.start_byte(), c.end_byte()))
        .collect()
}

/// Outer statement of a declarator: the `export_statement` if it wraps the declaration.
fn outer_statement(decl: Node) -> Node {
    match decl.parent() {
        Some(p) if p.kind() == "export_statement" => p,
        _ => decl,
    }
}

fn unique_id(symbols: &[Symbol], base: String) -> String {
    let n = symbols
        .iter()
        .filter(|s| s.stable_id == base || s.stable_id.starts_with(&format!("{base}#")))
        .count();
    if n == 0 {
        base
    } else {
        format!("{base}#{}", n + 1)
    }
}

/// A function-valued symbol the generic walker cannot see.
struct Pending<'t> {
    outer: Node<'t>,
    value: Node<'t>,
    name: String,
    kind: NodeKind,
}

/// `const f = () => {}` / `function` expressions bound to names, class-field
/// arrows (methods), and anonymous `export default` functions/classes.
fn function_values(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let mut found: Vec<Pending> = Vec::new();
    walk_named(tree.root_node(), |n| match n.kind() {
        "variable_declarator" => {
            let (Some(name), Some(value)) = (
                n.child_by_field_name("name"),
                n.child_by_field_name("value"),
            ) else {
                return;
            };
            if name.kind() != "identifier" || !is_function_value(value) {
                return;
            }
            let decl = n.parent().unwrap_or(n);
            found.push(Pending {
                outer: outer_statement(decl),
                value,
                name: text(name, src).to_string(),
                kind: NodeKind::Function,
            });
        }
        "public_field_definition" | "field_definition" => {
            let name = n
                .child_by_field_name("name")
                .or_else(|| n.child_by_field_name("property"));
            let (Some(name), Some(value)) = (name, n.child_by_field_name("value")) else {
                return;
            };
            if is_function_value(value) {
                found.push(Pending {
                    outer: n,
                    value,
                    name: text(name, src).to_string(),
                    kind: NodeKind::Method,
                });
            }
        }
        "export_statement" => {
            let Some(value) = n.child_by_field_name("value") else {
                return;
            };
            let kind = match value.kind() {
                "arrow_function" | "function_expression" => NodeKind::Function,
                "class" => NodeKind::Type,
                _ => return,
            };
            if value.child_by_field_name("name").is_some() {
                return;
            }
            found.push(Pending {
                outer: n,
                value,
                name: "default".into(),
                kind,
            });
        }
        _ => {}
    });
    for p in found {
        let parent = match p.kind {
            NodeKind::Method => {
                let id = enclosing_symbol(&out.symbols, &file.path, p.outer.start_byte());
                out.symbols
                    .iter()
                    .find(|s| s.stable_id == id && s.kind == NodeKind::Type)
                    .map(|s| (s.stable_id.clone(), s.qualified_name.clone()))
            }
            _ => None,
        };
        let qualified = match &parent {
            Some((_, q)) => format!("{q}::{}", p.name),
            None => p.name.clone(),
        };
        let base = format!(
            "{}:{}:{}:{}",
            file.language.as_str(),
            file.path,
            p.kind.as_str(),
            qualified
        );
        let body_start = p
            .value
            .child_by_field_name("body")
            .map(|b| b.start_byte())
            .unwrap_or(p.value.end_byte());
        let (doc, _) = leading_comments(p.outer, src);
        out.symbols.push(Symbol {
            stable_id: unique_id(&out.symbols, base),
            kind: p.kind,
            name: p.name,
            qualified_name: qualified,
            parent: parent.map(|(id, _)| id),
            signature: Some(value_signature(p.outer, body_start, src)),
            doc,
            line_start: p.outer.start_position().row as u32 + 1,
            line_end: p.outer.end_position().row as u32 + 1,
            byte_start: p.outer.start_byte(),
            byte_end: p.outer.end_byte(),
            segments: body_segments(p.value),
            metadata: serde_json::json!({}),
        });
    }
    out.symbols.sort_by_key(|s| s.byte_start);
}

/// Give `export`-wrapped declarations their leading doc comment and `export`
/// signature prefix, and make the
/// whole export statement (plus its docs/decorators) owned by the symbol, so
/// it never doubles as a free range.
fn own_exports(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let root = tree.root_node();
    let mut cursor = root.walk();
    let top: Vec<Node> = root.named_children(&mut cursor).collect();
    let mut owned: Vec<(usize, usize)> = Vec::new();
    for sym in out.symbols.iter_mut().filter(|s| s.parent.is_none()) {
        let Some(outer) = top
            .iter()
            .find(|n| n.start_byte() <= sym.byte_start && sym.byte_end <= n.end_byte())
        else {
            continue;
        };
        let (doc, start) = leading_comments(*outer, src);
        if outer.kind() == "export_statement" {
            if sym.doc.is_none() {
                sym.doc = doc;
            }
            // `export` / `export default` sits between the statement start and the declaration.
            let prefix = src[outer.start_byte()..sym.byte_start]
                .lines()
                .last()
                .unwrap_or("")
                .trim();
            if let Some(sig) = sym.signature.as_mut() {
                if prefix.starts_with("export") && !sig.starts_with("export") {
                    *sig = format!("{prefix} {sig}");
                }
            }
        }
        owned.push((start.unwrap_or(outer.start_byte()), outer.end_byte()));
    }
    out.free_ranges
        .retain(|(s, e)| !owned.iter().any(|(os, oe)| os <= s && e <= oe));
}

/// Call candidates, attributed after function-valued symbols exist.
fn calls(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let Ok(query) = Query::new(&tree.language(), CALL_QUERY) else {
        out.warnings.push("call query failed to compile".into());
        return;
    };
    let src = file.content.as_str();
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), src.as_bytes());
    let mut found = Vec::new();
    while let Some(m) = matches.next() {
        let mut callee = None;
        let mut receiver = None;
        let mut at = usize::MAX;
        for c in m.captures {
            at = at.min(c.node.start_byte());
            match names[c.index as usize] {
                "callee" => callee = Some((text(c.node, src), c.node.start_position().row)),
                "receiver" => receiver = Some(text(c.node, src).trim().to_string()),
                _ => {}
            }
        }
        if let Some((callee, row)) = callee {
            if callee == "require" {
                continue;
            }
            found.push(CallCandidate {
                caller: enclosing_symbol(&out.symbols, &file.path, at),
                callee: callee.to_string(),
                receiver,
                line: row as u32 + 1,
            });
        }
    }
    found.sort_by_key(|c| c.line);
    out.calls = found;
}

/// `class A extends B implements C, D` and `interface I extends J` relations.
fn heritage_relations(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let mut rels = Vec::new();
    walk_named(tree.root_node(), |n| {
        let mut cursor = n.walk();
        let source = || {
            let id = enclosing_symbol(&out.symbols, &file.path, n.start_byte());
            if id == file_id(&file.path) {
                None
            } else {
                Some(id)
            }
        };
        match n.kind() {
            "class_declaration" | "abstract_class_declaration" | "class" => {
                let Some(heritage) = n
                    .named_children(&mut cursor)
                    .find(|c| c.kind() == "class_heritage")
                else {
                    return;
                };
                let Some(source) = source() else { return };
                let mut hc = heritage.walk();
                for clause in heritage.named_children(&mut hc) {
                    match clause.kind() {
                        "extends_clause" => {
                            if let Some(v) = clause.child_by_field_name("value") {
                                rels.push((source.clone(), EdgeKind::Extends, text(v, src)));
                            }
                        }
                        "implements_clause" => {
                            let mut ic = clause.walk();
                            for t in clause.named_children(&mut ic) {
                                rels.push((source.clone(), EdgeKind::Implements, text(t, src)));
                            }
                        }
                        _ => rels.push((source.clone(), EdgeKind::Extends, text(clause, src))),
                    }
                }
            }
            "interface_declaration" => {
                let Some(clause) = n
                    .named_children(&mut cursor)
                    .find(|c| c.kind() == "extends_type_clause")
                else {
                    return;
                };
                let Some(source) = source() else { return };
                let mut ic = clause.walk();
                for t in clause.named_children(&mut ic) {
                    rels.push((source.clone(), EdgeKind::Extends, text(t, src)));
                }
            }
            _ => {}
        }
    });
    out.relations
        .extend(rels.into_iter().map(|(source, kind, target)| Relation {
            source,
            kind,
            target_name: target.trim().to_string(),
        }));
}

/// Bare package name of a specifier: `@scope/name` or the first path segment;
/// `None` for relative, absolute, `#` alias and `node:` builtin specifiers.
fn package_of(specifier: &str) -> Option<String> {
    let s = specifier.trim();
    if s.is_empty() || s.starts_with(['.', '/', '#']) || s.starts_with("node:") {
        return None;
    }
    let mut parts = s.split('/');
    let first = parts.next()?;
    if first.starts_with('@') {
        let second = parts.next().filter(|p| !p.is_empty())?;
        Some(format!("{first}/{second}"))
    } else {
        Some(first.to_string())
    }
}

/// Bare package specifiers imported by the file, deduped in source order.
fn packages(_tree: &Tree, _file: &FileInput, out: &mut FileExtraction) {
    for imp in &out.imports {
        if let Some(p) = package_of(&imp.specifier) {
            if !out.packages.contains(&p) {
                out.packages.push(p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::treesitter::walker::extract;

    fn run(path: &str, lang: Language, src: &str) -> FileExtraction {
        let file = FileInput {
            path: path.into(),
            language: lang,
            content: src.into(),
        };
        extract(spec(lang, path).unwrap(), &file)
    }

    fn ts(src: &str) -> FileExtraction {
        run("src/a.ts", Language::TypeScript, src)
    }

    fn js(src: &str) -> FileExtraction {
        run("src/a.js", Language::JavaScript, src)
    }

    fn ids(x: &FileExtraction) -> Vec<&str> {
        x.symbols.iter().map(|s| s.stable_id.as_str()).collect()
    }

    fn by_id<'a>(x: &'a FileExtraction, id: &str) -> &'a Symbol {
        x.symbols
            .iter()
            .find(|s| s.stable_id == id)
            .unwrap_or_else(|| panic!("missing {id}: {:?}", ids(x)))
    }

    #[test]
    fn spec_dispatch() {
        assert!(std::ptr::eq(
            spec(Language::TypeScript, "a.ts").unwrap(),
            &TS
        ));
        assert!(std::ptr::eq(
            spec(Language::TypeScript, "a.TSX").unwrap(),
            &TSX
        ));
        assert!(std::ptr::eq(
            spec(Language::JavaScript, "a.jsx").unwrap(),
            &JS
        ));
        assert!(spec(Language::Rust, "a.rs").is_none());
    }

    #[test]
    fn declarations_and_class_members() {
        let x = ts("/** Greets. */\nexport function greet(name: string): string { return name; }\nfunction* gen() { yield 1; }\nabstract class Base { abstract run(): void; }\nexport class Svc extends Base implements Runnable, Disposable {\n  private count = 0;\n  constructor(private x: number) { super(); }\n  /** Runs. */\n  run(): void { this.count++; }\n  handle = (e: Event) => { this.run(); };\n}\ninterface Runnable extends Disposable { run(): void }\ntype Id = string;\nenum Color { Red, Green }\n");
        assert_eq!(
            ids(&x),
            [
                "typescript:src/a.ts:function:greet",
                "typescript:src/a.ts:function:gen",
                "typescript:src/a.ts:type:Base",
                "typescript:src/a.ts:method:Base::run",
                "typescript:src/a.ts:type:Svc",
                "typescript:src/a.ts:method:Svc::constructor",
                "typescript:src/a.ts:method:Svc::run",
                "typescript:src/a.ts:method:Svc::handle",
                "typescript:src/a.ts:interface:Runnable",
                "typescript:src/a.ts:method:Runnable::run",
                "typescript:src/a.ts:type_alias:Id",
                "typescript:src/a.ts:enum:Color",
            ]
        );
        let greet = by_id(&x, "typescript:src/a.ts:function:greet");
        assert_eq!(greet.doc.as_deref(), Some("Greets."));
        assert_eq!(
            greet.signature.as_deref(),
            Some("export function greet(name: string): string")
        );
        assert_eq!(greet.kind, NodeKind::Function);
        let run = by_id(&x, "typescript:src/a.ts:method:Svc::run");
        assert_eq!(run.doc.as_deref(), Some("Runs."));
        assert_eq!(run.parent.as_deref(), Some("typescript:src/a.ts:type:Svc"));
        let handle = by_id(&x, "typescript:src/a.ts:method:Svc::handle");
        assert_eq!(handle.kind, NodeKind::Method);
        assert_eq!(
            handle.parent.as_deref(),
            Some("typescript:src/a.ts:type:Svc")
        );
        assert_eq!(handle.signature.as_deref(), Some("handle = (e: Event)"));
        assert_eq!(
            by_id(&x, "typescript:src/a.ts:enum:Color").segments.len(),
            2
        );
        assert_eq!(
            x.relations,
            vec![
                Relation {
                    source: "typescript:src/a.ts:type:Svc".into(),
                    kind: EdgeKind::Extends,
                    target_name: "Base".into()
                },
                Relation {
                    source: "typescript:src/a.ts:type:Svc".into(),
                    kind: EdgeKind::Implements,
                    target_name: "Runnable".into()
                },
                Relation {
                    source: "typescript:src/a.ts:type:Svc".into(),
                    kind: EdgeKind::Implements,
                    target_name: "Disposable".into()
                },
                Relation {
                    source: "typescript:src/a.ts:interface:Runnable".into(),
                    kind: EdgeKind::Extends,
                    target_name: "Disposable".into()
                },
            ]
        );
        assert!(x.free_ranges.is_empty(), "{:?}", x.free_ranges);
        assert!(x.warnings.is_empty(), "{:?}", x.warnings);
    }

    #[test]
    fn arrow_consts_and_default_exports() {
        let src = "import { z } from 'zod';\n\n// Adds two numbers.\nexport const add = (a: number, b: number): number => {\n  const inner = () => a;\n  return inner() + b;\n};\nconst mul = function (a: number, b: number) { return a * b; };\nconst LIMIT = 10;\nexport default async () => { add(1, 2); };\n";
        let x = ts(src);
        assert_eq!(
            ids(&x),
            [
                "typescript:src/a.ts:function:add",
                "typescript:src/a.ts:function:inner",
                "typescript:src/a.ts:function:mul",
                "typescript:src/a.ts:function:default",
            ]
        );
        let add = by_id(&x, "typescript:src/a.ts:function:add");
        assert_eq!(add.doc.as_deref(), Some("Adds two numbers."));
        assert_eq!(
            add.signature.as_deref(),
            Some("export const add = (a: number, b: number): number")
        );
        assert_eq!((add.line_start, add.line_end), (4, 7));
        assert_eq!(add.segments.len(), 2);
        assert!(add.parent.is_none());
        let calls: Vec<(&str, &str)> = x
            .calls
            .iter()
            .map(|c| (c.callee.as_str(), c.caller.as_str()))
            .collect();
        assert_eq!(
            calls,
            [
                ("inner", "typescript:src/a.ts:function:add"),
                ("add", "typescript:src/a.ts:function:default"),
            ]
        );
        let free: Vec<&str> = x.free_ranges.iter().map(|(s, e)| &src[*s..*e]).collect();
        assert_eq!(free, ["import { z } from 'zod';", "const LIMIT = 10;"]);
    }

    #[test]
    fn export_default_class_and_decorators() {
        let x = ts("/** Widget doc. */\n@Component({ selector: 'w' })\nexport default class Widget {\n  @Input() name = '';\n  @HostListener('click')\n  onClick() {}\n}\nexport default class {}\n");
        let w = by_id(&x, "typescript:src/a.ts:type:Widget");
        assert_eq!(w.doc.as_deref(), Some("Widget doc."));
        assert!(x
            .symbols
            .iter()
            .any(|s| s.stable_id == "typescript:src/a.ts:method:Widget::onClick"));
        let anon = by_id(&x, "typescript:src/a.ts:type:default");
        assert_eq!(anon.kind, NodeKind::Type);
        assert!(x.free_ranges.is_empty(), "{:?}", x.free_ranges);
    }

    #[test]
    fn imports_and_packages() {
        let x = ts("import React, { useState, useEffect as ue } from 'react';\nimport * as path from 'node:path';\nimport type { Foo } from './foo';\nimport '@scope/pkg/styles.css';\nimport { Client } from '@scope/pkg';\nexport { x } from '#alias/x';\nconst fs = require('fs');\nconst mod = await import('lodash/fp');\n");
        let specs: Vec<(&str, Vec<&str>)> = x
            .imports
            .iter()
            .map(|i| {
                (
                    i.specifier.as_str(),
                    i.names.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            specs,
            vec![
                ("react", vec!["useState", "useEffect", "ue"]),
                ("node:path", vec![]),
                ("./foo", vec!["Foo"]),
                ("@scope/pkg/styles.css", vec![]),
                ("@scope/pkg", vec!["Client"]),
                ("#alias/x", vec![]),
                ("fs", vec![]),
                ("lodash/fp", vec![]),
            ]
        );
        assert_eq!(x.imports[6].line, 7);
        assert_eq!(x.packages, ["react", "@scope/pkg", "fs", "lodash"]);
        assert!(!x.calls.iter().any(|c| c.callee == "require"));
    }

    #[test]
    fn calls_with_receivers() {
        let x = js("function main() {\n  helper();\n  db.users.find(1);\n  this.save();\n  new Client();\n}\nconsole.log('x');\n");
        let calls: Vec<(&str, Option<&str>, &str)> = x
            .calls
            .iter()
            .map(|c| (c.callee.as_str(), c.receiver.as_deref(), c.caller.as_str()))
            .collect();
        assert_eq!(
            calls,
            [
                ("helper", None, "javascript:src/a.js:function:main"),
                (
                    "find",
                    Some("db.users"),
                    "javascript:src/a.js:function:main"
                ),
                ("save", Some("this"), "javascript:src/a.js:function:main"),
                ("Client", None, "javascript:src/a.js:function:main"),
                ("log", Some("console"), "file:src/a.js"),
            ]
        );
    }

    #[test]
    fn javascript_classes_fields_and_require() {
        let x = js("const express = require('express');\n/**\n * A store.\n */\nclass Store extends EventEmitter {\n  static create() { return new Store(); }\n  onChange = () => {};\n  get size() { return 0; }\n}\nmodule.exports = { Store };\n");
        assert_eq!(
            ids(&x),
            [
                "javascript:src/a.js:type:Store",
                "javascript:src/a.js:method:Store::create",
                "javascript:src/a.js:method:Store::onChange",
                "javascript:src/a.js:method:Store::size",
            ]
        );
        assert_eq!(
            by_id(&x, "javascript:src/a.js:type:Store").doc.as_deref(),
            Some("A store.")
        );
        assert_eq!(
            x.relations,
            vec![Relation {
                source: "javascript:src/a.js:type:Store".into(),
                kind: EdgeKind::Extends,
                target_name: "EventEmitter".into()
            }]
        );
        assert_eq!(x.packages, ["express"]);
        assert_eq!(x.free_ranges.len(), 2);
    }

    #[test]
    fn tsx_components() {
        let x = run(
            "src/App.tsx",
            Language::TypeScript,
            "import React from 'react';\n\nexport const Button = ({ label }: { label: string }) => <button>{label}</button>;\n\nexport default function App() {\n  const [n, setN] = React.useState(0);\n  return <div onClick={() => setN(n + 1)}><Button label=\"go\" /></div>;\n}\n",
        );
        assert!(x.warnings.is_empty(), "{:?}", x.warnings);
        assert_eq!(
            ids(&x),
            [
                "typescript:src/App.tsx:function:Button",
                "typescript:src/App.tsx:function:App",
            ]
        );
        let button = by_id(&x, "typescript:src/App.tsx:function:Button");
        assert!(button.segments.is_empty());
        assert_eq!(
            button.signature.as_deref(),
            Some("export const Button = ({ label }: { label: string })")
        );
        let callers: Vec<&str> = x.calls.iter().map(|c| c.caller.as_str()).collect();
        assert!(callers
            .iter()
            .all(|c| *c == "typescript:src/App.tsx:function:App"));
        assert!(x.calls.iter().any(|c| c.callee == "useState"));
    }

    #[test]
    fn jsx_file_parses_with_javascript_grammar() {
        let x = run(
            "src/c.jsx",
            Language::JavaScript,
            "export function Card({ title }) { return <h1>{title}</h1>; }\n",
        );
        assert!(x.warnings.is_empty(), "{:?}", x.warnings);
        assert_eq!(ids(&x), ["javascript:src/c.jsx:function:Card"]);
    }

    #[test]
    fn duplicate_function_values_get_suffixes() {
        let x = js("const f = () => 1;\nconst f = () => 2;\n");
        assert_eq!(
            ids(&x),
            [
                "javascript:src/a.js:function:f",
                "javascript:src/a.js:function:f#2"
            ]
        );
    }

    #[test]
    fn package_names() {
        assert_eq!(package_of("react").as_deref(), Some("react"));
        assert_eq!(package_of("lodash/fp").as_deref(), Some("lodash"));
        assert_eq!(package_of("@scope/pkg/sub").as_deref(), Some("@scope/pkg"));
        assert_eq!(package_of("./x"), None);
        assert_eq!(package_of("/abs"), None);
        assert_eq!(package_of("#alias"), None);
        assert_eq!(package_of("node:fs"), None);
        assert_eq!(package_of("@scope"), None);
    }

    #[test]
    fn syntax_error_still_extracts() {
        let x = ts("export function ok() {}\nfunction broken( {\n");
        assert!(!x.warnings.is_empty());
        assert!(x.symbols.iter().any(|s| s.name == "ok"));
    }
}
