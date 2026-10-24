//! `search`: the passages most similar to a question, with their text.

use super::Kb;
use crate::domain::hash::Digest;
use crate::domain::note::Location;
use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchReport {
    pub hits: Vec<Found>,
    /// Merkle root of the index that answered.
    pub root: Digest,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Found {
    #[serde(flatten)]
    pub at: Location,
    pub score: f32,
    pub text: String,
}

impl Kb<'_> {
    /// Up to `limit` passages with similarity at least `min_score`, best first.
    pub fn search(&self, query: &str, limit: usize, min_score: f32) -> Result<SearchReport> {
        let _ = (query, limit, min_score);
        todo!()
    }
}
