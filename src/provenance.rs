//! Provenance breadcrumbs — "how was this feature extraction generated?".
//!
//! Every generated feature artifact (the `chaos add` feature/bug page, the
//! `chaos_change_plan` plan, the `chaos_impact` report, and the
//! `chaos_feature_context` evidence) carries a list of [`Breadcrumb`]s recording
//! *where each piece of information came from*: a regex/AST extraction, a
//! Postgres query, a file read, an embedding match, a git diff, or a previously
//! generated feature manifest. The breadcrumbs are serialized into the embedded
//! manifest JSON / compact MCP return and rendered on the page, so a human (or
//! agent) can audit the derivation rather than trusting it blindly.
//!
//! This type is deliberately tiny and stringly-typed: it is a *record* of work
//! already done, not a control surface. The [`source`] constants give the coarse
//! origin buckets a consistent vocabulary across artifacts.

use serde::{Deserialize, Serialize};

/// One breadcrumb: a single source consulted while generating an artifact.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Breadcrumb {
    /// Coarse origin bucket — one of the [`source`] constants
    /// (`git`, `postgres`, `file`, `ast`, `regex`, `embedding`,
    /// `feature-manifest`, `merkle`, `graph`).
    pub source: String,
    /// The concrete operation, e.g. `"git diff"`, `"load_graph_export"`,
    /// `"read_snippet"`, `"community_semantic_search"`, `"load_feature_matches"`.
    pub method: String,
    /// Human-readable detail: the file + lines read, the table/query, the cosine
    /// score, the matched page, the number of rows, …
    pub detail: String,
    /// Optional machine locator (a repo-relative path, a table name, a page, or a
    /// git ref) so a consumer can jump to the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<String>,
}

impl Breadcrumb {
    /// A breadcrumb with no locator.
    pub fn new(
        source: impl Into<String>,
        method: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            source: source.into(),
            method: method.into(),
            detail: detail.into(),
            locator: None,
        }
    }

    /// Attach a machine locator (path / table / page / ref).
    pub fn with_locator(mut self, locator: impl Into<String>) -> Self {
        self.locator = Some(locator.into());
        self
    }
}

/// The coarse origin buckets a [`Breadcrumb`] can name. Keeping these as
/// constants (rather than free strings at call sites) keeps the vocabulary
/// consistent so the rendered legend and any future filtering stay stable. This
/// is the complete provenance taxonomy; not every bucket is emitted by every
/// artifact today, hence `allow(dead_code)`.
#[allow(dead_code)]
pub mod source {
    /// Derived from git (diff, changed-file detection, current commit).
    pub const GIT: &str = "git";
    /// Read from the persisted Postgres/pgvector index via a query.
    pub const POSTGRES: &str = "postgres";
    /// Read directly off disk (a source snippet, a generated page).
    pub const FILE: &str = "file";
    /// Produced by language AST extraction (syn, the JS/TS/Python/Solidity parsers).
    pub const AST: &str = "ast";
    /// Produced by a regex / lexical pass.
    pub const REGEX: &str = "regex";
    /// Produced by a real embedder similarity match (cosine over pgvector).
    pub const EMBEDDING: &str = "embedding";
    /// Correlated against a previously generated feature manifest (HTML page).
    pub const MANIFEST: &str = "feature-manifest";
    /// Derived from the L2 Merkle subtree-hash rollup.
    pub const MERKLE: &str = "merkle";
    /// Derived from the in-memory knowledge graph / community detection (L1).
    pub const GRAPH: &str = "graph";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locator_is_omitted_when_absent() {
        let crumb = Breadcrumb::new(source::GIT, "git diff", "4 changed files");
        let json = serde_json::to_string(&crumb).unwrap();
        assert!(
            !json.contains("locator"),
            "absent locator must not serialize"
        );
    }

    #[test]
    fn locator_round_trips() {
        let crumb =
            Breadcrumb::new(source::FILE, "read_snippet", "lines 10-40").with_locator("src/x.rs");
        let json = serde_json::to_string(&crumb).unwrap();
        let back: Breadcrumb = serde_json::from_str(&json).unwrap();
        assert_eq!(back, crumb);
        assert_eq!(back.locator.as_deref(), Some("src/x.rs"));
    }
}
