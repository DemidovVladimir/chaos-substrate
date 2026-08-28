//! Port: per-file structural extraction (tree-sitter adapter implements it).
//!
//! Extraction is FILE-LOCAL and pure: it never sees other files. Imports and
//! call candidates are left unresolved; `application::analyze` resolves them
//! project-wide. Everything here is in source order and deterministic.

use crate::domain::model::{EdgeKind, Language, NodeKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct FileInput {
    /// Repo-relative path with `/` separators.
    pub path: String,
    pub language: Language,
    pub content: String,
}

/// A declared symbol. `stable_id` = `{lang}:{path}:{kind}:{container::chain::name}`
/// (`#2`, `#3` suffix for same-name duplicates in source order).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Symbol {
    pub stable_id: String,
    pub kind: NodeKind,
    pub name: String,
    /// `container::chain::name` (no lang/path prefix).
    pub qualified_name: String,
    /// stable_id of the enclosing symbol (impl/class/contract/type), if any.
    pub parent: Option<String>,
    /// First line of the declaration (e.g. `fn verify(&self, t: &str) -> bool`).
    pub signature: Option<String>,
    /// Leading doc comment / docstring, trimmed (≤ 1000 chars).
    pub doc: Option<String>,
    /// 1-based inclusive lines.
    pub line_start: u32,
    pub line_end: u32,
    /// Byte range in `FileInput.content`.
    pub byte_start: usize,
    pub byte_end: usize,
    /// Byte ranges of the direct child statements of the body block, in order —
    /// the candidate split points for oversize units. Empty when unknown.
    pub segments: Vec<(usize, usize)>,
    #[serde(default)]
    pub metadata: Value,
}

/// A raw import as written. Resolution happens in `application::analyze`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImportRef {
    /// `crate::a::b`, `./x`, `react`, `os.path`, `./IERC20.sol`.
    pub specifier: String,
    /// Imported names (`{ a, b }`, `use x::{a, b}`); empty for whole-module imports.
    pub names: Vec<String>,
    pub line: u32,
}

/// A call site whose target is not yet resolved.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CallCandidate {
    /// stable_id of the enclosing symbol (or the file's `file:{path}` id).
    pub caller: String,
    /// Bare callee name (`foo`, `bar` for `x.bar()`).
    pub callee: String,
    /// Receiver text when present (`self`, `Storage`, `this`, a variable name).
    pub receiver: Option<String>,
    pub line: u32,
}

/// A by-name relation between symbols (extends/implements/queries), resolved later.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Relation {
    pub source: String,
    pub kind: EdgeKind,
    /// Target NAME (not stable_id) — e.g. `IERC20`, `Query.user`.
    pub target_name: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceKind {
    EnvVar,
    HttpRoute,
}

/// A user-facing surface found in the file (env var read, HTTP route registered).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Surface {
    pub kind: SurfaceKind,
    /// `DATABASE_URL`, or `GET /users/:id` (method upper-case, path as written).
    pub name: String,
    pub line: u32,
    /// stable_id of the enclosing symbol, if any.
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FileExtraction {
    pub path: String,
    pub language: Option<Language>,
    pub content_hash: String,
    pub line_count: u32,
    pub symbols: Vec<Symbol>,
    pub imports: Vec<ImportRef>,
    pub calls: Vec<CallCandidate>,
    pub relations: Vec<Relation>,
    pub surfaces: Vec<Surface>,
    /// Bare package specifiers imported (`react`, `serde`) — for cross-repo package_dep.
    pub packages: Vec<String>,
    /// Byte ranges NOT owned by any symbol (imports, top-level statements) → file_head chunk.
    pub free_ranges: Vec<(usize, usize)>,
    /// Non-fatal parse problems (file still indexed at file level).
    pub warnings: Vec<String>,
}

pub trait SourceExtractor: Send + Sync {
    /// Extract one file. Must never panic or fail: on parse errors return a
    /// `FileExtraction` with `warnings` and whatever symbols were recovered.
    fn extract(&self, file: &FileInput) -> FileExtraction;

    /// Extract many files (adapter parallelises with rayon). Output order = input order.
    fn extract_all(&self, files: &[FileInput]) -> Vec<FileExtraction> {
        files.iter().map(|f| self.extract(f)).collect()
    }
}
