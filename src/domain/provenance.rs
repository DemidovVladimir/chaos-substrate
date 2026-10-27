//! Provenance breadcrumbs: where each piece of a generated answer came from.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Breadcrumb {
    /// One of the [`source`] constants.
    pub source: &'static str,
    /// Short method name (`louvain`, `cosine`, `tsvector`, `git diff` …).
    pub method: String,
    /// Human-readable detail.
    pub detail: String,
    /// Optional locator (path, sql table, url).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locator: Option<String>,
}

impl Breadcrumb {
    pub fn new(source: &'static str, method: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            source,
            method: method.into(),
            detail: detail.into(),
            locator: None,
        }
    }

    pub fn with_locator(mut self, locator: impl Into<String>) -> Self {
        self.locator = Some(locator.into());
        self
    }
}

pub mod source {
    pub const GIT: &str = "git";
    pub const POSTGRES: &str = "postgres";
    pub const FILE: &str = "file";
    pub const TREE_SITTER: &str = "tree-sitter";
    pub const EMBEDDING: &str = "embedding";
    pub const GRAPH: &str = "graph";
    pub const PAGE: &str = "page";
}
