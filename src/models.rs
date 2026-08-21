use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Generates `as_str` + `from_str` for a unit enum from explicit
/// variant ⇒ string pairs (no case conversion) — the strings persisted in
/// Postgres and printed in chunk headers. Serde derives are deliberately NOT
/// generated or altered here: `Language`'s serde names (`type_script`)
/// intentionally diverge from its `as_str` (`typescript`), and the
/// `#[serde(rename_all)]` readback contract must not change.
macro_rules! impl_str_conversions {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $text,)+
                }
            }

            /// Inverse of [`Self::as_str`]. Returns `None` for unknown strings
            /// so callers pick their own fallback (see `storage::row_to_node`).
            #[allow(clippy::should_implement_trait)] // Option return, not FromStr's Result
            #[allow(dead_code)] // not every enum has a production caller yet
            pub fn from_str(value: &str) -> Option<Self> {
                match value {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Rust,
    TypeScript,
    JavaScript,
    Python,
    Json,
    Markdown,
    Pdf,
    Solidity,
    GraphQL,
}

impl_str_conversions!(Language {
    Rust => "rust",
    TypeScript => "typescript",
    JavaScript => "javascript",
    Python => "python",
    Json => "json",
    Markdown => "markdown",
    Pdf => "pdf",
    Solidity => "solidity",
    GraphQL => "graphql",
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Repository,
    File,
    Module,
    Function,
    Struct,
    Enum,
    Trait,
    Impl,
    Method,
    Test,
    Dependency,
    Concept,
    Script,
    TypeAlias,
    DeploymentResource,
    CliCommand,
    HttpRoute,
    EnvVar,
    GraphqlOperation,
    GraphqlFragment,
    GraphqlField,
}

impl_str_conversions!(NodeKind {
    Repository => "repository",
    File => "file",
    Module => "module",
    Function => "function",
    Struct => "struct",
    Enum => "enum",
    Trait => "trait",
    Impl => "impl",
    Method => "method",
    Test => "test",
    Dependency => "dependency",
    Concept => "concept",
    Script => "script",
    TypeAlias => "type_alias",
    DeploymentResource => "deployment_resource",
    CliCommand => "cli_command",
    HttpRoute => "http_route",
    EnvVar => "env_var",
    GraphqlOperation => "graphql_operation",
    GraphqlFragment => "graphql_fragment",
    GraphqlField => "graphql_field",
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Contains,
    Imports,
    Calls,
    UsesType,
    Implements,
    Defines,
    Tests,
    Documents,
    Mentions,
    DependsOn,
    Configures,
    Deploys,
    SimilarTo,
    PrerequisiteFor,
}

impl_str_conversions!(EdgeKind {
    Contains => "contains",
    Imports => "imports",
    Calls => "calls",
    UsesType => "uses_type",
    Implements => "implements",
    Defines => "defines",
    Tests => "tests",
    Documents => "documents",
    Mentions => "mentions",
    DependsOn => "depends_on",
    Configures => "configures",
    Deploys => "deploys",
    SimilarTo => "similar_to",
    PrerequisiteFor => "prerequisite_for",
});

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repository {
    pub id: Uuid,
    pub name: String,
    pub root_path: String,
    pub remote_url: Option<String>,
    pub current_commit_sha: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A named set of indexed repositories (client, backend, contracts, infra, …)
/// — the cross-repository grouping the P6 project layer hangs off.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

/// One member repository of a project, with its project-scoped alias and the
/// repo root hash recorded at the last successful cross-repo link run (the L2
/// gate for relinking).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRepo {
    pub repo: Repository,
    pub alias: String,
    pub linked_repo_hash: Option<String>,
    /// True when this member is a project-level DOCS source (registered via
    /// `project add-docs`) rather than a code repo. It still contributes
    /// searchable documentation chunks and communities, but is excluded from
    /// "code repos involved" counts.
    pub is_project_docs: bool,
}

/// A detected cross-repository link between two L1 communities (features) of
/// different member repos: consumer feature → provider feature. Produced by
/// the linkers in `src/linker.rs`, persisted in `cross_repo_links`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossRepoLink {
    pub id: Uuid,
    pub source_repo_id: Uuid,
    pub source_community_id: Uuid,
    pub target_repo_id: Uuid,
    pub target_community_id: Uuid,
    /// `package_dep` | `abi` | `graphql` | `http_route`.
    pub kind: String,
    /// Matched names/paths + provenance breadcrumbs.
    pub evidence: Value,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceFile {
    pub id: Uuid,
    pub repo_id: Uuid,
    pub commit_sha: Option<String>,
    pub path: String,
    pub language: Language,
    pub content: String,
    pub content_hash: String,
    pub line_count: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeNode {
    pub id: Uuid,
    pub repo_id: Uuid,
    pub file_id: Option<Uuid>,
    pub kind: NodeKind,
    pub stable_id: String,
    pub name: String,
    pub line_start: Option<i32>,
    pub line_end: Option<i32>,
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeEdge {
    pub id: Uuid,
    pub repo_id: Uuid,
    pub source_node_id: Uuid,
    pub target_node_id: Uuid,
    pub kind: EdgeKind,
    pub cost: f64,
    pub confidence: f64,
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeChunk {
    pub id: Uuid,
    pub repo_id: Uuid,
    pub file_id: Option<Uuid>,
    pub node_id: Option<Uuid>,
    pub chunk_type: String,
    pub content: String,
    pub content_hash: String,
    pub line_start: Option<i32>,
    pub line_end: Option<i32>,
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionResult {
    pub files: Vec<SourceFile>,
    pub nodes: Vec<KnowledgeNode>,
    pub edges: Vec<KnowledgeEdge>,
    pub chunks: Vec<KnowledgeChunk>,
}

impl ExtractionResult {
    pub fn empty() -> Self {
        Self {
            files: Vec::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
            chunks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub chunk_id: Uuid,
    pub node_id: Option<Uuid>,
    pub file_path: Option<String>,
    pub line_start: Option<i32>,
    pub line_end: Option<i32>,
    pub score: f64,
    pub content: String,
    pub metadata: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    const LANGUAGES: [Language; 9] = [
        Language::Rust,
        Language::TypeScript,
        Language::JavaScript,
        Language::Python,
        Language::Json,
        Language::Markdown,
        Language::Pdf,
        Language::Solidity,
        Language::GraphQL,
    ];

    const NODE_KINDS: [NodeKind; 21] = [
        NodeKind::Repository,
        NodeKind::File,
        NodeKind::Module,
        NodeKind::Function,
        NodeKind::Struct,
        NodeKind::Enum,
        NodeKind::Trait,
        NodeKind::Impl,
        NodeKind::Method,
        NodeKind::Test,
        NodeKind::Dependency,
        NodeKind::Concept,
        NodeKind::Script,
        NodeKind::TypeAlias,
        NodeKind::DeploymentResource,
        NodeKind::CliCommand,
        NodeKind::HttpRoute,
        NodeKind::EnvVar,
        NodeKind::GraphqlOperation,
        NodeKind::GraphqlFragment,
        NodeKind::GraphqlField,
    ];

    const EDGE_KINDS: [EdgeKind; 14] = [
        EdgeKind::Contains,
        EdgeKind::Imports,
        EdgeKind::Calls,
        EdgeKind::UsesType,
        EdgeKind::Implements,
        EdgeKind::Defines,
        EdgeKind::Tests,
        EdgeKind::Documents,
        EdgeKind::Mentions,
        EdgeKind::DependsOn,
        EdgeKind::Configures,
        EdgeKind::Deploys,
        EdgeKind::SimilarTo,
        EdgeKind::PrerequisiteFor,
    ];

    #[test]
    fn str_conversions_round_trip() {
        for language in LANGUAGES {
            assert_eq!(Language::from_str(language.as_str()), Some(language));
        }
        for kind in NODE_KINDS {
            assert_eq!(NodeKind::from_str(kind.as_str()), Some(kind.clone()));
        }
        for kind in EDGE_KINDS {
            assert_eq!(EdgeKind::from_str(kind.as_str()), Some(kind.clone()));
        }
        assert_eq!(Language::from_str("cobol"), None);
        assert_eq!(NodeKind::from_str("nonsense"), None);
        assert_eq!(EdgeKind::from_str("nonsense"), None);
    }

    /// `storage::row_to_node`/`row_to_edge` read back with `from_str` the same
    /// strings the serde derives persist; this pins that equality. `Language`
    /// is deliberately absent: its serde names (`type_script`) diverge from
    /// `as_str` (`typescript`).
    #[test]
    fn node_and_edge_kind_serde_strings_equal_as_str() {
        for kind in NODE_KINDS {
            assert_eq!(
                serde_json::to_value(&kind).unwrap(),
                Value::String(kind.as_str().into())
            );
        }
        for kind in EDGE_KINDS {
            assert_eq!(
                serde_json::to_value(&kind).unwrap(),
                Value::String(kind.as_str().into())
            );
        }
    }
}
