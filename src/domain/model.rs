//! Core data model. Pure data: no I/O, no async, no adapter types.
//!
//! Layering contract (enforced by `tests::layering` in `main.rs`):
//! `domain` depends on nothing inside the crate; `ports` on `domain`;
//! `application` on `domain` + `ports`; `adapters` implement ports; `entry`
//! wires adapters into application use-cases.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Generates `as_str` + `parse` for a unit enum from explicit variant ⇒ string
/// pairs — the strings persisted in Postgres and printed in chunk headers.
#[macro_export]
macro_rules! impl_str_conversions {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self { $(Self::$variant => $text,)+ }
            }
            pub fn parse(value: &str) -> Option<Self> {
                match value { $($text => Some(Self::$variant),)+ _ => None }
            }
            pub const ALL: &'static [$name] = &[$($name::$variant,)+];
        }
    };
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Rust,
    TypeScript,
    JavaScript,
    Python,
    Solidity,
    GraphQL,
    Markdown,
}

impl_str_conversions!(Language {
    Rust => "rust",
    TypeScript => "typescript",
    JavaScript => "javascript",
    Python => "python",
    Solidity => "solidity",
    GraphQL => "graphql",
    Markdown => "markdown",
});

impl Language {
    /// Detect a language from a file path (extension based). `None` = not indexed.
    pub fn from_path(path: &str) -> Option<Self> {
        let ext = path.rsplit('.').next()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Self::Rust,
            "ts" | "tsx" | "mts" | "cts" => Self::TypeScript,
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "py" => Self::Python,
            "sol" => Self::Solidity,
            "graphql" | "gql" | "graphqls" => Self::GraphQL,
            "md" | "mdx" => Self::Markdown,
            _ => return None,
        })
    }
}

/// Kinds of graph nodes. Language-neutral: a Solidity contract, a TS class and
/// a GraphQL object type are all `Type`; a Rust trait, TS interface and GraphQL
/// interface are all `Interface`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Repository,
    File,
    Module,
    Function,
    Method,
    Type,
    Interface,
    Enum,
    TypeAlias,
    /// GraphQL field definition, Solidity event.
    Field,
    /// GraphQL executable operation (query/mutation/subscription).
    Operation,
    /// GraphQL fragment.
    Fragment,
    /// Markdown heading section.
    Section,
    /// User-surface: environment variable read by a file.
    EnvVar,
    /// User-surface: HTTP route registered by a file.
    HttpRoute,
}

impl_str_conversions!(NodeKind {
    Repository => "repository",
    File => "file",
    Module => "module",
    Function => "function",
    Method => "method",
    Type => "type",
    Interface => "interface",
    Enum => "enum",
    TypeAlias => "type_alias",
    Field => "field",
    Operation => "operation",
    Fragment => "fragment",
    Section => "section",
    EnvVar => "env_var",
    HttpRoute => "http_route",
});

impl NodeKind {
    /// Kinds that carry code and get their own chunk.
    pub fn is_unit(&self) -> bool {
        matches!(
            self,
            Self::Function
                | Self::Method
                | Self::Type
                | Self::Interface
                | Self::Enum
                | Self::TypeAlias
                | Self::Operation
                | Self::Fragment
                | Self::Section
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// file → symbol, container → member.
    Contains,
    /// file → file (resolved import).
    Imports,
    /// caller → callee (resolved, never fuzzy).
    Calls,
    /// type → base type / contract → parent.
    Extends,
    /// type → interface/trait.
    Implements,
    /// host function/file → embedded GraphQL operation; file → surface node.
    Defines,
    /// GraphQL operation → SDL root field it selects.
    Queries,
}

impl_str_conversions!(EdgeKind {
    Contains => "contains",
    Imports => "imports",
    Calls => "calls",
    Extends => "extends",
    Implements => "implements",
    Defines => "defines",
    Queries => "queries",
});

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    /// One AST unit (function/type/section) with its context header.
    Unit,
    /// A unit too large for the embedder, split contextually: `part i/n`.
    Part,
    /// Imports + top-level statements not owned by any unit.
    FileHead,
}

impl_str_conversions!(ChunkKind {
    Unit => "unit",
    Part => "part",
    FileHead => "file_head",
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Repo {
    pub id: Uuid,
    pub name: String,
    pub root_path: String,
    pub commit_sha: Option<String>,
    pub root_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceFile {
    pub id: Uuid,
    pub repo_id: Uuid,
    pub path: String,
    pub language: Language,
    pub content_hash: String,
    pub line_count: i32,
    pub subtree_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Node {
    pub id: Uuid,
    pub repo_id: Uuid,
    pub file_id: Option<Uuid>,
    pub kind: NodeKind,
    /// `{lang}:{path}:{kind}:{container::chain::name}`; `file:{path}`; `repo`.
    pub stable_id: String,
    pub name: String,
    pub signature: Option<String>,
    pub doc: Option<String>,
    pub line_start: Option<i32>,
    pub line_end: Option<i32>,
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Edge {
    pub id: Uuid,
    pub repo_id: Uuid,
    pub source_node_id: Uuid,
    pub target_node_id: Uuid,
    pub kind: EdgeKind,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Chunk {
    pub id: Uuid,
    pub repo_id: Uuid,
    pub file_id: Option<Uuid>,
    pub node_id: Option<Uuid>,
    pub kind: ChunkKind,
    /// Context header (file, container, kind/name, uses, calls, doc) — ≤ 700 chars.
    pub header: String,
    pub body: String,
    /// sha256 over `header + "\n" + body`.
    pub content_hash: String,
    pub line_start: Option<i32>,
    pub line_end: Option<i32>,
    pub metadata: Value,
}

impl Chunk {
    /// The text that gets embedded and searched.
    pub fn text(&self) -> String {
        format!("{}\n{}", self.header, self.body)
    }
}

/// An L1 community ("feature"): a Louvain cluster of nodes with a templated,
/// hash-gated summary whose embedding routes feature questions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Community {
    pub id: Uuid,
    pub repo_id: Uuid,
    pub label: String,
    pub member_count: i32,
    /// Merkle roll-up of member files; gates summary recomputation.
    pub subtree_hash: String,
    pub summary: Option<String>,
    pub summary_hash: Option<String>,
    /// Top member symbol names (weighted degree order).
    pub top_members: Vec<String>,
    /// Member file paths, sorted.
    pub files: Vec<String>,
}

/// Everything the index stores persist for one repository (or, for `add`, for
/// the changed subset of its files).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexBundle {
    pub files: Vec<SourceFile>,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub chunks: Vec<Chunk>,
}

/// A vector plus the identity of the model that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRef {
    pub provider: String,
    pub model_id: String,
    pub dimensions: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_from_path() {
        assert_eq!(Language::from_path("a/b.tsx"), Some(Language::TypeScript));
        assert_eq!(Language::from_path("x.gql"), Some(Language::GraphQL));
        assert_eq!(Language::from_path("Cargo.toml"), None);
    }

    #[test]
    fn str_roundtrip() {
        for k in NodeKind::ALL {
            assert_eq!(NodeKind::parse(k.as_str()), Some(*k));
        }
        for k in EdgeKind::ALL {
            assert_eq!(EdgeKind::parse(k.as_str()), Some(*k));
        }
    }
}
