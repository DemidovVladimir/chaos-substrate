//! Python `LangSpec`: `def`/`class` definitions (decorators folded into the
//! symbol), docstrings, `import` / `from … import` imports, calls with
//! receivers, class bases as `Extends` relations and absolute-import packages.

use super::lang::{DefRule, LangSpec};
use super::walker::text;
use crate::domain::model::{EdgeKind, NodeKind};
use crate::ports::extractor::{FileExtraction, FileInput, ImportRef, Relation};
use tree_sitter::{Node, Tree};

const MAX_DOC: usize = 1000;

const DEFS: &[DefRule] = &[
    DefRule {
        node_type: "function_definition",
        kind: NodeKind::Function,
        name_field: "name",
        container: false,
        methods_inside: false,
        method_when_nested: true,
    },
    DefRule {
        node_type: "class_definition",
        kind: NodeKind::Type,
        name_field: "name",
        container: true,
        methods_inside: true,
        method_when_nested: false,
    },
];

/// One match per imported name; `merge_imports` folds them back per statement.
const IMPORT_QUERY: &str = r#"
(import_statement name: (dotted_name) @specifier)
(import_statement name: (aliased_import name: (dotted_name) @specifier))
(import_from_statement module_name: (_) @specifier name: (dotted_name) @name)
(import_from_statement module_name: (_) @specifier name: (aliased_import name: (dotted_name) @name))
(import_from_statement module_name: (_) @specifier (wildcard_import))
"#;

const CALL_QUERY: &str = r#"
(call function: (identifier) @callee)
(call function: (attribute object: (_) @receiver attribute: (identifier) @callee))
"#;

static SPEC: LangSpec = LangSpec {
    language: || tree_sitter_python::LANGUAGE.into(),
    defs: DEFS,
    import_query: IMPORT_QUERY,
    call_query: CALL_QUERY,
    doc_comment_types: &[],
    body_types: &["block"],
    extras: &[
        definitions,
        merge_imports,
        packages,
        super::surface::python,
        super::graphql::embedded_python,
    ],
};

/// The Python language specification.
pub fn spec() -> Option<&'static LangSpec> {
    Some(&SPEC)
}

fn walk(tree: &Tree, mut f: impl FnMut(Node)) {
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        f(n);
        let mut cursor = n.walk();
        let kids: Vec<Node> = n.named_children(&mut cursor).collect();
        stack.extend(kids.into_iter().rev());
    }
}

/// The docstring of a `def`/`class` body: its first statement when it is a bare string.
fn docstring(node: Node, src: &str) -> Option<String> {
    let body = node.child_by_field_name("body")?;
    let first = body.named_child(0)?;
    if first.kind() != "expression_statement" {
        return None;
    }
    let s = first.named_child(0)?;
    if s.kind() != "string" {
        return None;
    }
    let raw = text(s, src);
    let inner = raw
        .trim_start_matches(|c: char| c.is_ascii_alphabetic())
        .trim_start_matches(['"', '\''])
        .trim_end_matches(['"', '\'']);
    let lines: Vec<&str> = inner.lines().map(str::trim).collect();
    let doc = lines.join("\n").trim().to_string();
    (!doc.is_empty()).then(|| doc.chars().take(MAX_DOC).collect())
}

/// Docstrings, decorator folding (range + `metadata.decorators`), `:`-less
/// signatures and class bases → `Extends`.
fn definitions(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    walk(tree, |n| {
        let is_def = matches!(n.kind(), "function_definition" | "class_definition");
        if !is_def {
            return;
        }
        let Some(idx) = out
            .symbols
            .iter()
            .position(|s| s.byte_start == n.start_byte())
        else {
            return;
        };
        if let Some(doc) = docstring(n, src) {
            out.symbols[idx].doc = Some(doc);
        }
        if let Some(sig) = out.symbols[idx].signature.take() {
            out.symbols[idx].signature = Some(sig.trim_end_matches(':').trim().to_string());
        }
        if let Some(p) = n.parent().filter(|p| p.kind() == "decorated_definition") {
            let mut cursor = p.walk();
            let decorators: Vec<serde_json::Value> = p
                .named_children(&mut cursor)
                .filter(|c| c.kind() == "decorator")
                .map(|c| text(c, src).trim_start_matches('@').trim().into())
                .collect();
            let sym = &mut out.symbols[idx];
            sym.byte_start = p.start_byte();
            sym.line_start = p.start_position().row as u32 + 1;
            sym.metadata["decorators"] = serde_json::Value::Array(decorators);
        }
        if n.kind() == "class_definition" {
            if let Some(bases) = n.child_by_field_name("superclasses") {
                let source = out.symbols[idx].stable_id.clone();
                let mut cursor = bases.walk();
                for b in bases.named_children(&mut cursor) {
                    if matches!(b.kind(), "identifier" | "attribute") {
                        out.relations.push(Relation {
                            source: source.clone(),
                            kind: EdgeKind::Extends,
                            target_name: text(b, src).to_string(),
                        });
                    }
                }
            }
        }
    });
    // A decorated top-level definition is not covered by the walker's owned
    // range (decorators are siblings of the def inside `decorated_definition`).
    let symbols = &out.symbols;
    out.free_ranges.retain(|(s, e)| {
        !symbols
            .iter()
            .any(|sym| sym.byte_start >= *s && sym.byte_end <= *e)
    });
}

/// Fold per-name matches of one `from x import a, b` back into one `ImportRef`.
fn merge_imports(_tree: &Tree, _file: &FileInput, out: &mut FileExtraction) {
    let mut merged: Vec<ImportRef> = Vec::new();
    for imp in out.imports.drain(..) {
        match merged
            .last_mut()
            .filter(|m| m.line == imp.line && m.specifier == imp.specifier)
        {
            Some(m) => m.names.extend(imp.names),
            None => merged.push(imp),
        }
    }
    out.imports = merged;
}

/// First dotted segment of every absolute import, deduped in source order.
fn packages(_tree: &Tree, _file: &FileInput, out: &mut FileExtraction) {
    for imp in &out.imports {
        if imp.specifier.starts_with('.') {
            continue;
        }
        let root = imp.specifier.split('.').next().unwrap_or("").trim();
        if root.is_empty() || out.packages.iter().any(|p| p == root) {
            continue;
        }
        out.packages.push(root.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::treesitter::walker::extract;
    use crate::domain::model::Language;

    const MODULE: &str = r#""""Module doc."""
import os
import numpy as np
from typing import List, Optional
from . import sibling
from ..pkg.util import helper as h
from os.path import *


def top(x):
    """Top-level helper.

    Second line.
    """
    return os.path.join(x, "y")


class Account(Base, mixins.Audit, metaclass=Meta):
    """An account."""

    @property
    def balance(self):
        'Current balance.'
        return self._balance

    @staticmethod
    @cached(ttl=5)
    def load(repo):
        return repo.fetch(1)

    def deposit(self, amount):
        self.log.info("x")
        top(amount)


@dataclass
class Plain:
    pass
"#;

    fn py() -> FileExtraction {
        let file = FileInput {
            path: "app/acct.py".into(),
            language: Language::Python,
            content: MODULE.into(),
        };
        extract(spec().unwrap(), &file)
    }

    #[test]
    fn symbols_kinds_ids_and_docstrings() {
        let x = py();
        assert!(x.warnings.is_empty(), "{:?}", x.warnings);
        let ids: Vec<&str> = x.symbols.iter().map(|s| s.stable_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "python:app/acct.py:function:top",
                "python:app/acct.py:type:Account",
                "python:app/acct.py:method:Account::balance",
                "python:app/acct.py:method:Account::load",
                "python:app/acct.py:method:Account::deposit",
                "python:app/acct.py:type:Plain",
            ]
        );
        let top = &x.symbols[0];
        assert_eq!(
            top.doc.as_deref(),
            Some("Top-level helper.\n\nSecond line.")
        );
        assert_eq!(top.signature.as_deref(), Some("def top(x)"));
        assert_eq!(top.segments.len(), 2);
        assert_eq!(x.symbols[1].doc.as_deref(), Some("An account."));
        assert_eq!(x.symbols[2].doc.as_deref(), Some("Current balance."));
        assert_eq!(x.symbols[2].kind, NodeKind::Method);
        assert_eq!(
            x.symbols[2].parent.as_deref(),
            Some("python:app/acct.py:type:Account")
        );
        assert!(x.symbols[3].doc.is_none());
    }

    #[test]
    fn decorators_fold_into_symbol_range() {
        let x = py();
        let load = &x.symbols[3];
        assert_eq!(
            load.metadata["decorators"],
            serde_json::json!(["staticmethod", "cached(ttl=5)"])
        );
        assert!(MODULE[load.byte_start..load.byte_end].starts_with("@staticmethod"));
        let plain = &x.symbols[5];
        assert!(MODULE[plain.byte_start..plain.byte_end].starts_with("@dataclass"));
        assert_eq!(plain.line_start, 36);
        // Free ranges: module docstring + imports only, never a decorated def.
        let free: Vec<&str> = x.free_ranges.iter().map(|(s, e)| &MODULE[*s..*e]).collect();
        assert_eq!(free[0], "\"\"\"Module doc.\"\"\"");
        assert_eq!(free.len(), 7, "{free:?}");
    }

    #[test]
    fn imports_relative_absolute_and_packages() {
        let x = py();
        let imports: Vec<(&str, Vec<&str>)> = x
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
            imports,
            vec![
                ("os", vec![]),
                ("numpy", vec![]),
                ("typing", vec!["List", "Optional"]),
                (".", vec!["sibling"]),
                ("..pkg.util", vec!["helper"]),
                ("os.path", vec![]),
            ]
        );
        assert_eq!(x.imports[2].line, 4);
        assert_eq!(x.packages, vec!["os", "numpy", "typing"]);
    }

    #[test]
    fn calls_with_receivers_and_callers() {
        let x = py();
        let calls: Vec<(&str, Option<&str>, &str)> = x
            .calls
            .iter()
            .map(|c| (c.callee.as_str(), c.receiver.as_deref(), c.caller.as_str()))
            .collect();
        assert_eq!(
            calls,
            vec![
                ("join", Some("os.path"), "python:app/acct.py:function:top"),
                ("cached", None, "python:app/acct.py:type:Account"),
                (
                    "fetch",
                    Some("repo"),
                    "python:app/acct.py:method:Account::load"
                ),
                (
                    "info",
                    Some("self.log"),
                    "python:app/acct.py:method:Account::deposit"
                ),
                ("top", None, "python:app/acct.py:method:Account::deposit"),
            ]
        );
    }

    #[test]
    fn class_bases_become_extends() {
        let x = py();
        assert_eq!(
            x.relations,
            vec![
                Relation {
                    source: "python:app/acct.py:type:Account".into(),
                    kind: EdgeKind::Extends,
                    target_name: "Base".into()
                },
                Relation {
                    source: "python:app/acct.py:type:Account".into(),
                    kind: EdgeKind::Extends,
                    target_name: "mixins.Audit".into()
                },
            ]
        );
    }
}
