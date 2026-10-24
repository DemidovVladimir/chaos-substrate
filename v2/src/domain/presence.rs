//! The answer to "is this already in the knowledge base?".

use serde::Serialize;

/// Cosine similarity from which a passage counts as covering a query.
/// Calibrated for `embeddinggemma` on the sample vault in `tests/e2e.rs`.
pub const DEFAULT_MIN_SCORE: f32 = 0.5;

/// How a query relates to what is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Presence {
    /// The same words are stored (fingerprint match in the Merkle leaves).
    Exact,
    /// A stored passage is at least `min_score` similar.
    Similar,
    /// Nothing stored comes close enough.
    Absent,
}

impl Presence {
    pub fn is_known(self) -> bool {
        self != Presence::Absent
    }
}

/// Decide presence: an exact match wins; otherwise the best similarity must
/// reach `min_score`.
pub fn judge(exact: bool, best: Option<f32>, min_score: f32) -> Presence {
    let _ = (exact, best, min_score);
    todo!()
}
