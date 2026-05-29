//! Centralized edge weights for the knowledge multigraph.
//!
//! Every relationship carries a `cost` and a `confidence`:
//!
//! * `cost` is the traversal weight used by the shortest-path router in
//!   [`crate::graph`]. Lower means "cheaper to cross", so high-certainty
//!   structural relations get small costs and the router prefers them when
//!   connecting two search hits:
//!   `defines` (0.08) < `contains` code (0.10) < `configures`/dependency
//!   (0.12-0.25) < imports (0.30-0.40) < heuristic `calls` (0.35). Supplemental
//!   context (Markdown 0.45, PDF 0.55) is deliberately the most expensive so
//!   docs only enter a path when no source-level route exists.
//! * `confidence` (0.0..=1.0) records how trustworthy the extraction is. Edges
//!   produced by real parsers (`syn`, dependency manifests) are ~1.0; regex- or
//!   name-based heuristics such as the `calls` detector (0.55) and inheritance
//!   (0.75) are lower so downstream ranking can discount them.
//!
//! Keeping the numbers here means routing stays comparable across languages and
//! the whole weighting model can be tuned in one place.

/// Traversal cost and extraction confidence for a single knowledge edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeWeight {
    pub cost: f64,
    pub confidence: f64,
}

impl EdgeWeight {
    const fn new(cost: f64, confidence: f64) -> Self {
        Self { cost, confidence }
    }
}

/// Repository/file/module owns a code symbol (parser-certain).
pub const CONTAINS_CODE: EdgeWeight = EdgeWeight::new(0.10, 1.00);
/// Container owns a member resolved heuristically (e.g. Solidity members).
pub const CONTAINS_MEMBER: EdgeWeight = EdgeWeight::new(0.10, 0.95);
/// Repository owns a Markdown/MDX document (supplemental, costly to traverse).
pub const CONTAINS_DOC: EdgeWeight = EdgeWeight::new(0.45, 0.80);
/// Repository owns extracted PDF text (supplemental, most costly).
pub const CONTAINS_PDF: EdgeWeight = EdgeWeight::new(0.55, 0.75);
/// Manifest declares a third-party dependency.
pub const DEPENDS_ON: EdgeWeight = EdgeWeight::new(0.20, 1.00);
/// Manifest defines a runnable script (e.g. an npm script).
pub const DEFINES_SCRIPT: EdgeWeight = EdgeWeight::new(0.25, 1.00);
/// File defines a top-level type/contract/stack (parser-certain).
pub const DEFINES_SYMBOL: EdgeWeight = EdgeWeight::new(0.08, 1.00);
/// Code configures a deployment resource.
pub const CONFIGURES: EdgeWeight = EdgeWeight::new(0.12, 0.95);
/// Config file configures an application entrypoint.
pub const CONFIGURES_APP: EdgeWeight = EdgeWeight::new(0.15, 1.00);
/// Rust `use` import.
pub const IMPORTS_RUST: EdgeWeight = EdgeWeight::new(0.40, 0.80);
/// JS/TS/Python module import.
pub const IMPORTS_MODULE: EdgeWeight = EdgeWeight::new(0.35, 0.90);
/// Solidity import.
pub const IMPORTS_SOLIDITY: EdgeWeight = EdgeWeight::new(0.30, 0.90);
/// Name-based call detection (heuristic, low confidence).
pub const CALLS_HEURISTIC: EdgeWeight = EdgeWeight::new(0.35, 0.55);
/// Type/contract inheritance or trait implementation (heuristic).
pub const IMPLEMENTS: EdgeWeight = EdgeWeight::new(0.20, 0.75);
