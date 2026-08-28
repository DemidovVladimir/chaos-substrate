//! Generic, language-agnostic tree-sitter walk driven by a `LangSpec`.
//!
//! Produces symbols (with stable ids, parents, signatures, docs, segments),
//! unresolved imports and call candidates (from queries), and the byte ranges
//! not owned by any symbol. All offsets are tree-sitter byte offsets, which
//! are always char-boundary safe for valid UTF-8 input.

use super::lang::{DefRule, LangSpec};
use crate::domain::hash::sha256_hex;
use crate::domain::model::NodeKind;
use crate::ports::extractor::{CallCandidate, FileExtraction, FileInput, ImportRef, Symbol};
use std::collections::HashMap;
use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

const MAX_SIGNATURE: usize = 200;
const MAX_DOC: usize = 1000;
/// Nodes that may sit between a doc comment and its declaration.
const DECORATION_TYPES: &[&str] = &["attribute_item", "decorator"];

/// The `file:{path}` id used as the caller of top-level calls.
pub fn file_id(path: &str) -> String {
    format!("file:{path}")
}

/// A `FileExtraction` with no symbols: the whole file is one free range.
pub fn file_level(file: &FileInput) -> FileExtraction {
    FileExtraction {
        path: file.path.clone(),
        language: Some(file.language),
        content_hash: sha256_hex(&file.content),
        line_count: file.content.lines().count() as u32,
        free_ranges: if file.content.is_empty() {
            vec![]
        } else {
            vec![(0, file.content.len())]
        },
        ..Default::default()
    }
}

/// Parse `file` with `spec` and run the generic walk plus the spec's extras.
pub fn extract(spec: &'static LangSpec, file: &FileInput) -> FileExtraction {
    let mut out = file_level(file);
    out.free_ranges.clear();
    let mut parser = Parser::new();
    if parser.set_language(&(spec.language)()).is_err() {
        out.warnings.push("grammar failed to load".into());
        out.free_ranges = file_level(file).free_ranges;
        return out;
    }
    let Some(tree) = parser.parse(&file.content, None) else {
        out.warnings.push("parser returned no tree".into());
        out.free_ranges = file_level(file).free_ranges;
        return out;
    };
    let root = tree.root_node();
    if root.has_error() {
        out.warnings.push(format!(
            "syntax errors in {} (partial extraction)",
            file.path
        ));
    }
    let mut walker = Walker {
        spec,
        file,
        symbols: Vec::new(),
        owned: Vec::new(),
        ids: HashMap::new(),
        containers: Vec::new(),
    };
    walker.visit(root);
    out.free_ranges = walker.free_ranges(root);
    out.symbols = walker.symbols;
    out.imports = run_imports(spec, &tree, file);
    out.calls = run_calls(spec, &tree, file, &out.symbols);
    for pass in spec.extras {
        pass(&tree, file, &mut out);
    }
    out
}

/// Text of a node, sliced by tree-sitter byte offsets.
pub fn text<'a>(node: Node, src: &'a str) -> &'a str {
    &src[node.byte_range()]
}

/// stable_id of the innermost symbol containing byte offset `at`, else `file:{path}`.
pub fn enclosing_symbol(symbols: &[Symbol], path: &str, at: usize) -> String {
    symbols
        .iter()
        .filter(|s| s.byte_start <= at && at < s.byte_end)
        .min_by_key(|s| s.byte_end - s.byte_start)
        .map(|s| s.stable_id.clone())
        .unwrap_or_else(|| file_id(path))
}

struct Walker<'a> {
    spec: &'static LangSpec,
    file: &'a FileInput,
    symbols: Vec<Symbol>,
    /// (start including docs/decorations, end) per top-level symbol.
    owned: Vec<(usize, usize)>,
    ids: HashMap<String, usize>,
    /// (symbol index, rule) stack of enclosing containers.
    containers: Vec<(usize, DefRule)>,
}

impl Walker<'_> {
    fn rule_for(&self, node: Node) -> Option<DefRule> {
        self.spec
            .defs
            .iter()
            .copied()
            .find(|r| r.node_type == node.kind())
    }

    fn visit(&mut self, node: Node) {
        if let Some(rule) = self.rule_for(node) {
            let Some(name_node) = node.child_by_field_name(rule.name_field) else {
                self.visit_children(node);
                return;
            };
            let src = &self.file.content;
            let name = text(name_node, src).trim().to_string();
            let inside_methods = self
                .containers
                .last()
                .is_some_and(|(_, c)| c.methods_inside);
            let kind = if rule.method_when_nested && inside_methods {
                NodeKind::Method
            } else {
                rule.kind
            };
            let chain: Vec<&str> = self
                .containers
                .iter()
                .map(|(i, _)| self.symbols[*i].name.as_str())
                .collect();
            let qualified = if chain.is_empty() {
                name.clone()
            } else {
                format!("{}::{}", chain.join("::"), name)
            };
            let base = format!(
                "{}:{}:{}:{}",
                self.file.language.as_str(),
                self.file.path,
                kind.as_str(),
                qualified
            );
            let n = self.ids.entry(base.clone()).or_insert(0);
            *n += 1;
            let stable_id = if *n == 1 { base } else { format!("{base}#{n}") };
            let (doc, doc_start) = leading_doc(node, self.spec, src);
            let symbol = Symbol {
                stable_id,
                kind,
                name,
                qualified_name: qualified,
                parent: self
                    .containers
                    .last()
                    .map(|(i, _)| self.symbols[*i].stable_id.clone()),
                signature: Some(signature(node, src)),
                doc,
                line_start: node.start_position().row as u32 + 1,
                line_end: node.end_position().row as u32 + 1,
                byte_start: node.start_byte(),
                byte_end: node.end_byte(),
                segments: segments(node, self.spec),
                metadata: serde_json::json!({}),
            };
            if self.containers.is_empty() {
                self.owned
                    .push((doc_start.unwrap_or(node.start_byte()), node.end_byte()));
            }
            self.symbols.push(symbol);
            let idx = self.symbols.len() - 1;
            if rule.container {
                self.containers.push((idx, rule));
                self.visit_children(node);
                self.containers.pop();
            } else {
                self.visit_children(node);
            }
        } else {
            self.visit_children(node);
        }
    }

    fn visit_children(&mut self, node: Node) {
        let mut cursor = node.walk();
        let children: Vec<Node> = node.named_children(&mut cursor).collect();
        for child in children {
            self.visit(child);
        }
    }

    /// Top-level named nodes not covered by any owned symbol range.
    fn free_ranges(&self, root: Node) -> Vec<(usize, usize)> {
        let mut cursor = root.walk();
        root.named_children(&mut cursor)
            .filter(|n| {
                !self
                    .owned
                    .iter()
                    .any(|(s, e)| n.start_byte() >= *s && n.end_byte() <= *e)
            })
            .map(|n| (n.start_byte(), n.end_byte()))
            .collect()
    }
}

/// Declaration text up to its body (or its first line), trimmed to `MAX_SIGNATURE` chars.
fn signature(node: Node, src: &str) -> String {
    let end = node
        .child_by_field_name("body")
        .map(|b| b.start_byte())
        .unwrap_or(node.end_byte());
    let head = &src[node.start_byte()..end];
    let first = head.lines().next().unwrap_or("").trim();
    let sig: String = first.chars().take(MAX_SIGNATURE).collect();
    sig.trim_end_matches('{').trim().to_string()
}

/// Contiguous doc comments directly above `node` (skipping attributes/decorators).
/// Returns the cleaned doc and the byte start of the first doc/decoration node.
fn leading_doc(node: Node, spec: &LangSpec, src: &str) -> (Option<String>, Option<usize>) {
    let mut lines: Vec<String> = Vec::new();
    let mut start = None;
    let mut expected_row = node.start_position().row;
    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev {
        if p.end_position().row + 1 < expected_row {
            break;
        }
        if DECORATION_TYPES.contains(&p.kind()) {
            start = Some(p.start_byte());
        } else if spec.doc_comment_types.contains(&p.kind()) {
            start = Some(p.start_byte());
            let cleaned: Vec<String> = text(p, src).lines().rev().map(clean_comment_line).collect();
            lines.extend(cleaned);
        } else {
            break;
        }
        expected_row = p.start_position().row;
        prev = p.prev_named_sibling();
    }
    lines.reverse();
    let doc = lines.join("\n").trim().to_string();
    let doc = if doc.is_empty() {
        None
    } else {
        Some(doc.chars().take(MAX_DOC).collect())
    };
    (doc, start)
}

fn clean_comment_line(line: &str) -> String {
    let mut s = line.trim();
    for prefix in ["///", "//!", "//", "/**", "/*", "*/", "*", "#"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest;
            break;
        }
    }
    s.trim_end_matches("*/").trim().to_string()
}

/// Byte ranges of the direct named children of the body block.
fn segments(node: Node, spec: &LangSpec) -> Vec<(usize, usize)> {
    let mut cursor = node.walk();
    let body = node.child_by_field_name("body").or_else(|| {
        node.named_children(&mut cursor)
            .find(|c| spec.body_types.contains(&c.kind()))
    });
    let Some(body) = body.filter(|b| spec.body_types.contains(&b.kind())) else {
        return vec![];
    };
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .map(|c| (c.start_byte(), c.end_byte()))
        .collect()
}

fn compile(spec: &LangSpec, query: &str) -> Option<Query> {
    if query.trim().is_empty() {
        return None;
    }
    Query::new(&(spec.language)(), query).ok()
}

fn run_imports(spec: &LangSpec, tree: &Tree, file: &FileInput) -> Vec<ImportRef> {
    let Some(query) = compile(spec, spec.import_query) else {
        return vec![];
    };
    let src = file.content.as_str();
    let names = query.capture_names();
    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), src.as_bytes());
    while let Some(m) = matches.next() {
        let mut spec_text = String::new();
        let mut imported = Vec::new();
        let mut line = u32::MAX;
        for c in m.captures {
            line = line.min(c.node.start_position().row as u32 + 1);
            match names[c.index as usize] {
                "specifier" => spec_text = text(c.node, src).trim().to_string(),
                "name" => imported.push(text(c.node, src).trim().to_string()),
                "names" => imported.extend(named_leaves(c.node, src)),
                _ => {}
            }
        }
        if spec_text.is_empty() && imported.is_empty() {
            continue;
        }
        imported.dedup();
        out.push(ImportRef {
            specifier: spec_text,
            names: imported,
            line,
        });
    }
    out.sort_by_key(|i| i.line);
    out
}

/// Text of every named leaf below `node`, in source order, minus `self`.
fn named_leaves(node: Node, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.named_child_count() == 0 {
            let t = text(n, src).trim();
            if !t.is_empty() && t != "self" {
                out.push(t.to_string());
            }
        } else {
            let mut cursor = n.walk();
            let kids: Vec<Node> = n.named_children(&mut cursor).collect();
            stack.extend(kids.into_iter().rev());
        }
    }
    out
}

fn run_calls(
    spec: &LangSpec,
    tree: &Tree,
    file: &FileInput,
    symbols: &[Symbol],
) -> Vec<CallCandidate> {
    let Some(query) = compile(spec, spec.call_query) else {
        return vec![];
    };
    let src = file.content.as_str();
    let names = query.capture_names();
    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), src.as_bytes());
    while let Some(m) = matches.next() {
        let mut callee = None;
        let mut receiver = None;
        let mut at = usize::MAX;
        for c in m.captures {
            at = at.min(c.node.start_byte());
            match names[c.index as usize] {
                "callee" => {
                    callee = Some((
                        text(c.node, src).to_string(),
                        c.node.start_position().row as u32 + 1,
                    ))
                }
                "receiver" => receiver = Some(text(c.node, src).trim().to_string()),
                _ => {}
            }
        }
        if let Some((callee, line)) = callee {
            out.push(CallCandidate {
                caller: enclosing_symbol(symbols, &file.path, at),
                callee,
                receiver,
                line,
            });
        }
    }
    out.sort_by_key(|c| c.line);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::Language;

    fn rs(src: &str) -> FileExtraction {
        let file = FileInput {
            path: "src/a.rs".into(),
            language: Language::Rust,
            content: src.into(),
        };
        extract(super::super::rust::spec(), &file)
    }

    #[test]
    fn duplicate_names_get_suffixes_in_source_order() {
        let x = rs("fn a() {}\nmod m { fn a() {} }\nfn a() {}\n");
        let ids: Vec<&str> = x.symbols.iter().map(|s| s.stable_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "rust:src/a.rs:function:a",
                "rust:src/a.rs:module:m",
                "rust:src/a.rs:function:m::a",
                "rust:src/a.rs:function:a#2"
            ]
        );
    }

    #[test]
    fn segments_and_doc_and_signature() {
        let x = rs("/// Adds.\n/// Twice.\n#[inline]\npub fn add(a: i32, b: i32) -> i32 {\n    let c = a;\n    let d = b;\n    c + d\n}\n");
        let s = &x.symbols[0];
        assert_eq!(s.doc.as_deref(), Some("Adds.\nTwice."));
        assert_eq!(
            s.signature.as_deref(),
            Some("pub fn add(a: i32, b: i32) -> i32")
        );
        assert_eq!(s.segments.len(), 3);
        assert_eq!((s.line_start, s.line_end), (4, 8));
        assert!(
            x.free_ranges.is_empty(),
            "docs+attrs belong to the symbol: {:?}",
            x.free_ranges
        );
    }

    #[test]
    fn free_ranges_cover_use_block_and_statics() {
        let src = "use std::fmt;\nuse serde::Serialize;\n\nstatic X: u8 = 1;\n\nfn f() {}\n";
        let x = rs(src);
        let texts: Vec<&str> = x.free_ranges.iter().map(|(s, e)| &src[*s..*e]).collect();
        assert_eq!(
            texts,
            [
                "use std::fmt;",
                "use serde::Serialize;",
                "static X: u8 = 1;"
            ]
        );
    }

    #[test]
    fn syntax_error_warns_but_still_extracts() {
        let x = rs("fn ok() {}\nfn broken( {\n");
        assert!(!x.warnings.is_empty());
        assert!(x.symbols.iter().any(|s| s.name == "ok"));
    }

    #[test]
    fn utf8_content_is_safe() {
        let x = rs("// héllo wörld ✓\nfn ünïcode() { let s = \"日本語\"; }\n");
        assert_eq!(x.symbols[0].name, "ünïcode");
        assert_eq!(x.content_hash.len(), 64);
    }

    #[test]
    fn empty_file_level_has_no_ranges() {
        let f = FileInput {
            path: "x.md".into(),
            language: Language::Markdown,
            content: String::new(),
        };
        assert!(file_level(&f).free_ranges.is_empty());
    }
}
