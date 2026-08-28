//! Table-driven language specifications for the generic tree-sitter walker.
//!
//! A language plugs in by providing a `LangSpec`: which grammar node types are
//! definitions (and what they become), two tree-sitter queries (imports and
//! calls), which node types carry doc comments / bodies, and optional
//! `ExtraPass`es that run after the generic walk (embedded GraphQL, user
//! surfaces, relations) with mutable access to the extraction — so new
//! language behaviour never touches `walker.rs`.

use super::{graphql, python, rust, solidity, typescript};
use crate::domain::model::{Language, NodeKind};
use crate::ports::extractor::{FileExtraction, FileInput};

/// One grammar node type that declares a symbol.
#[derive(Debug, Clone, Copy)]
pub struct DefRule {
    /// tree-sitter node type, e.g. `function_item`.
    pub node_type: &'static str,
    /// Kind the symbol gets (before `method_when_nested` adjustment).
    pub kind: NodeKind,
    /// Field holding the name node (`name`, `type`, …).
    pub name_field: &'static str,
    /// Whether nested definitions are qualified by (and parented to) this one.
    pub container: bool,
    /// Functions directly inside this container become `Method`s.
    pub methods_inside: bool,
    /// Becomes `NodeKind::Method` when nested in a `methods_inside` container.
    pub method_when_nested: bool,
}

/// A post-walk hook with full access to the tree and the extraction so far.
pub type ExtraPass = fn(&tree_sitter::Tree, &FileInput, &mut FileExtraction);

/// Everything the walker needs to extract one language.
pub struct LangSpec {
    /// Grammar constructor.
    pub language: fn() -> tree_sitter::Language,
    /// Definition rules, matched by node type.
    pub defs: &'static [DefRule],
    /// Query with `@specifier` (one per match) and `@name` / `@names` captures.
    /// `@names` is a list node whose named leaves are the imported names.
    pub import_query: &'static str,
    /// Query with `@callee` (required) and optional `@receiver` captures.
    pub call_query: &'static str,
    /// Node types that count as doc comments when they directly precede a def.
    pub doc_comment_types: &'static [&'static str],
    /// Node types that are bodies; their direct named children become segments.
    pub body_types: &'static [&'static str],
    /// Language-specific passes run after the generic walk, in order.
    pub extras: &'static [ExtraPass],
}

/// Resolve the spec for a language/path. `None` = file-level extraction only.
/// `Language::Markdown` is not tree-sitter and is dispatched in `mod.rs`.
pub fn spec_for(lang: Language, path: &str) -> Option<&'static LangSpec> {
    match lang {
        Language::Rust => Some(rust::spec()),
        Language::TypeScript | Language::JavaScript => typescript::spec(lang, path),
        Language::Python => python::spec(),
        Language::Solidity => solidity::spec(),
        Language::GraphQL => graphql::spec(),
        Language::Markdown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_has_spec_and_markdown_has_none() {
        assert!(spec_for(Language::Rust, "src/lib.rs").is_some());
        assert!(spec_for(Language::Markdown, "README.md").is_none());
    }
}
