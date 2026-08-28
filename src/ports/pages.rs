//! Port: writing generated HTML pages from a manifest (filesystem adapter owns
//! templates + styling; use-cases only supply data).

use anyhow::Result;
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    /// One `feature` answer persisted: question, features, excerpts, graph.
    Feature,
    /// Site index listing every community with links to per-feature pages.
    SiteIndex,
    /// One community's drill-down page (members, files, relations, summary).
    SitePage,
    /// Correlation report (intra + cross-repo).
    Correlations,
}

/// The element id of the embedded manifest block in every generated page.
pub const MANIFEST_ID: &str = "chaos-manifest";

pub trait PageStore: Send + Sync {
    /// Render `kind` with `manifest` (which MUST contain `content_hash`) to `path`,
    /// creating parents. Returns `false` (and writes nothing) when an existing
    /// page at `path` already carries the same `content_hash`.
    fn write_page(&self, path: &Path, kind: PageKind, manifest: &Value) -> Result<bool>;
    /// The embedded manifest of an existing page, if any.
    fn read_manifest(&self, path: &Path) -> Option<Value>;
}
