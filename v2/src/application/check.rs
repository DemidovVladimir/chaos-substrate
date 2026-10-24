//! `check`: is this already known? First the exact path — the query's
//! fingerprint against the Merkle leaves, no embedding — then the nearest
//! passages by cosine similarity.

use super::Kb;
use crate::domain::hash::Digest;
use crate::domain::note::Location;
use crate::domain::presence::Presence;
use anyhow::Result;
use serde::Serialize;

/// Passages compared on the similarity path.
pub const CHECK_K: usize = 3;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CheckReport {
    pub known: bool,
    pub presence: Presence,
    /// Best similarity found (absent on the exact path, which embeds nothing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    /// Where the knowledge sits: exact locations, or passages at or above
    /// `min_score`, best first.
    pub matches: Vec<Match>,
    /// Merkle root of the index that answered.
    pub root: Digest,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Match {
    #[serde(flatten)]
    pub at: Location,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
}

impl Kb<'_> {
    pub fn check(&self, query: &str, min_score: f32) -> Result<CheckReport> {
        let _ = (query, min_score);
        todo!()
    }
}
