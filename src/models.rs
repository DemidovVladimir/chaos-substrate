use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

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
}

impl Language {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::JavaScript => "javascript",
            Self::Python => "python",
            Self::Json => "json",
            Self::Markdown => "markdown",
            Self::Pdf => "pdf",
            Self::Solidity => "solidity",
        }
    }
}

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
}

impl NodeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Repository => "repository",
            Self::File => "file",
            Self::Module => "module",
            Self::Function => "function",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::Method => "method",
            Self::Test => "test",
            Self::Dependency => "dependency",
            Self::Concept => "concept",
            Self::Script => "script",
            Self::TypeAlias => "type_alias",
            Self::DeploymentResource => "deployment_resource",
        }
    }
}

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

impl EdgeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Contains => "contains",
            Self::Imports => "imports",
            Self::Calls => "calls",
            Self::UsesType => "uses_type",
            Self::Implements => "implements",
            Self::Defines => "defines",
            Self::Tests => "tests",
            Self::Documents => "documents",
            Self::Mentions => "mentions",
            Self::DependsOn => "depends_on",
            Self::Configures => "configures",
            Self::Deploys => "deploys",
            Self::SimilarTo => "similar_to",
            Self::PrerequisiteFor => "prerequisite_for",
        }
    }
}

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
