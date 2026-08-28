//! Rust `LangSpec`: definitions, `use` imports, calls, `impl Trait for Type`
//! relations and bare package names.

use super::lang::{DefRule, LangSpec};
use super::walker::{enclosing_symbol, text};
use crate::domain::model::{EdgeKind, NodeKind};
use crate::ports::extractor::{FileExtraction, FileInput, Relation};
use tree_sitter::{Node, Tree};

const fn rule(
    node_type: &'static str,
    kind: NodeKind,
    name_field: &'static str,
    container: bool,
    methods_inside: bool,
    method_when_nested: bool,
) -> DefRule {
    DefRule {
        node_type,
        kind,
        name_field,
        container,
        methods_inside,
        method_when_nested,
    }
}

const DEFS: &[DefRule] = &[
    rule(
        "function_item",
        NodeKind::Function,
        "name",
        false,
        false,
        true,
    ),
    rule(
        "function_signature_item",
        NodeKind::Method,
        "name",
        false,
        false,
        false,
    ),
    rule("impl_item", NodeKind::Module, "type", true, true, false),
    rule("struct_item", NodeKind::Type, "name", false, false, false),
    rule("enum_item", NodeKind::Enum, "name", false, false, false),
    rule("trait_item", NodeKind::Interface, "name", true, true, false),
    rule(
        "type_item",
        NodeKind::TypeAlias,
        "name",
        false,
        false,
        false,
    ),
    rule("mod_item", NodeKind::Module, "name", true, false, false),
];

const IMPORT_QUERY: &str = r#"
(use_declaration argument: (scoped_use_list path: (_) @specifier list: (use_list) @names))
(use_declaration argument: (use_list) @names)
(use_declaration argument: (scoped_identifier) @specifier)
(use_declaration argument: (identifier) @specifier)
(use_declaration argument: (use_as_clause path: (_) @specifier))
(use_declaration argument: (use_wildcard (_) @specifier))
"#;

const CALL_QUERY: &str = r#"
(call_expression function: (identifier) @callee)
(call_expression function: (generic_function function: (identifier) @callee))
(call_expression function: (scoped_identifier path: (_) @receiver name: (identifier) @callee))
(call_expression function: (generic_function function: (scoped_identifier path: (_) @receiver name: (identifier) @callee)))
(call_expression function: (field_expression value: (_) @receiver field: (field_identifier) @callee))
(call_expression function: (generic_function function: (field_expression value: (_) @receiver field: (field_identifier) @callee)))
"#;

static SPEC: LangSpec = LangSpec {
    language: || tree_sitter_rust::LANGUAGE.into(),
    defs: DEFS,
    import_query: IMPORT_QUERY,
    call_query: CALL_QUERY,
    doc_comment_types: &["line_comment", "block_comment"],
    body_types: &[
        "block",
        "declaration_list",
        "field_declaration_list",
        "enum_variant_list",
    ],
    extras: &[impl_relations, packages, super::surface::rust],
};

/// The Rust language specification.
pub fn spec() -> &'static LangSpec {
    &SPEC
}

const BUILTIN_ROOTS: &[&str] = &["crate", "self", "super", "std", "core", "alloc"];

/// `impl Trait for Type` → `Implements` relation + `metadata.trait` on the impl symbol.
fn impl_relations(tree: &Tree, file: &FileInput, out: &mut FileExtraction) {
    let src = file.content.as_str();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        if n.kind() == "impl_item" {
            if let (Some(tr), Some(ty)) = (
                n.child_by_field_name("trait"),
                n.child_by_field_name("type"),
            ) {
                let trait_name = text(tr, src).trim().to_string();
                let type_name = text(ty, src).trim().to_string();
                let impl_id = enclosing_symbol(&out.symbols, &file.path, n.start_byte());
                if let Some(sym) = out.symbols.iter_mut().find(|s| s.stable_id == impl_id) {
                    sym.metadata["trait"] = serde_json::Value::String(trait_name.clone());
                }
                let source = out
                    .symbols
                    .iter()
                    .find(|s| {
                        s.qualified_name == type_name
                            && matches!(s.kind, NodeKind::Type | NodeKind::Enum)
                    })
                    .map(|s| s.stable_id.clone())
                    .unwrap_or(impl_id);
                out.relations.push(Relation {
                    source,
                    kind: EdgeKind::Implements,
                    target_name: trait_name,
                });
            }
        }
        let mut cursor = n.walk();
        let kids: Vec<Node> = n.named_children(&mut cursor).collect();
        stack.extend(kids.into_iter().rev());
    }
}

/// First segment of every `use` path that is not a built-in root.
fn packages(_tree: &Tree, _file: &FileInput, out: &mut FileExtraction) {
    for imp in &out.imports {
        let root = imp.specifier.split("::").next().unwrap_or("").trim();
        if root.is_empty()
            || BUILTIN_ROOTS.contains(&root)
            || out.packages.iter().any(|p| p == root)
        {
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

    fn rs(src: &str) -> FileExtraction {
        let file = FileInput {
            path: "src/x.rs".into(),
            language: Language::Rust,
            content: src.into(),
        };
        extract(spec(), &file)
    }

    #[test]
    fn stable_ids_and_impl_parent_chain() {
        let x = rs("pub struct Foo;\nimpl Foo {\n    pub fn new() -> Self { Foo }\n    fn go(&self) {}\n}\n");
        let by_id = |id: &str| {
            x.symbols
                .iter()
                .find(|s| s.stable_id == id)
                .unwrap_or_else(|| panic!("{id}"))
        };
        assert_eq!(by_id("rust:src/x.rs:type:Foo").kind, NodeKind::Type);
        let imp = by_id("rust:src/x.rs:module:Foo");
        assert!(imp.parent.is_none());
        let new = by_id("rust:src/x.rs:method:Foo::new");
        assert_eq!(new.kind, NodeKind::Method);
        assert_eq!(new.parent.as_deref(), Some("rust:src/x.rs:module:Foo"));
        assert_eq!(new.qualified_name, "Foo::new");
        assert_eq!(new.signature.as_deref(), Some("pub fn new() -> Self"));
    }

    #[test]
    fn trait_impl_relation_and_metadata() {
        let x = rs("use std::fmt;\npub struct Foo;\nimpl fmt::Display for Foo { fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { Ok(()) } }\nimpl Clone for Bar { fn clone(&self) -> Self { todo!() } }\ntrait T { fn sig(&self); fn dflt(&self) {} }\n");
        assert_eq!(
            x.relations,
            vec![
                Relation {
                    source: "rust:src/x.rs:type:Foo".into(),
                    kind: EdgeKind::Implements,
                    target_name: "fmt::Display".into()
                },
                Relation {
                    source: "rust:src/x.rs:module:Bar".into(),
                    kind: EdgeKind::Implements,
                    target_name: "Clone".into()
                },
            ]
        );
        let imp = x
            .symbols
            .iter()
            .find(|s| s.stable_id == "rust:src/x.rs:module:Foo")
            .unwrap();
        assert_eq!(imp.metadata["trait"], "fmt::Display");
        assert!(x
            .symbols
            .iter()
            .any(|s| s.stable_id == "rust:src/x.rs:method:T::sig"));
        assert!(x
            .symbols
            .iter()
            .any(|s| s.stable_id == "rust:src/x.rs:method:T::dflt"));
        assert!(x
            .symbols
            .iter()
            .any(|s| s.stable_id == "rust:src/x.rs:interface:T"));
    }

    #[test]
    fn imports_with_names_and_packages() {
        let x = rs("use std::collections::{HashMap, HashSet};\nuse serde::Serialize;\nuse crate::a::b as c;\nuse tokio::*;\nuse anyhow::{self, Result};\n");
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
                ("std::collections", vec!["HashMap", "HashSet"]),
                ("serde::Serialize", vec![]),
                ("crate::a::b", vec![]),
                ("tokio", vec![]),
                ("anyhow", vec!["Result"]),
            ]
        );
        assert_eq!(x.imports[1].line, 2);
        assert_eq!(x.packages, vec!["serde", "tokio", "anyhow"]);
    }

    #[test]
    fn calls_with_receiver_and_caller() {
        let x = rs("fn main() {\n    helper();\n    Storage::open();\n    self.save();\n    cfg.load::<u8>();\n}\nfn top() {}\nstatic Z: fn() = || { init(); };\n");
        let calls: Vec<(&str, Option<&str>, &str)> = x
            .calls
            .iter()
            .map(|c| (c.callee.as_str(), c.receiver.as_deref(), c.caller.as_str()))
            .collect();
        assert_eq!(
            calls,
            vec![
                ("helper", None, "rust:src/x.rs:function:main"),
                ("open", Some("Storage"), "rust:src/x.rs:function:main"),
                ("save", Some("self"), "rust:src/x.rs:function:main"),
                ("load", Some("cfg"), "rust:src/x.rs:function:main"),
                ("init", None, "file:src/x.rs"),
            ]
        );
    }

    #[test]
    fn enums_aliases_modules() {
        let x = rs("enum E { A, B }\ntype Id = u32;\nmod inner { pub struct S; }\n");
        let kinds: Vec<(NodeKind, &str)> = x
            .symbols
            .iter()
            .map(|s| (s.kind, s.qualified_name.as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (NodeKind::Enum, "E"),
                (NodeKind::TypeAlias, "Id"),
                (NodeKind::Module, "inner"),
                (NodeKind::Type, "inner::S")
            ]
        );
        assert_eq!(x.symbols[0].segments.len(), 2);
    }
}
