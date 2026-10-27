//! `search`: the passages most similar to a question, with their text.

use super::{Kb, nonblank};
use crate::domain::note::Location;
use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchReport {
    pub hits: Vec<Found>,
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
        let query = nonblank(query, "query")?;
        self.ready()?;
        let hits = self
            .similar(query, limit)?
            .into_iter()
            .filter(|h| h.score >= min_score)
            .map(|h| Found {
                at: h.chunk.at,
                score: h.score,
                text: h.chunk.text,
            })
            .collect();
        Ok(SearchReport { hits })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{World, sample_notes};

    #[test]
    fn hits_come_best_first_with_text_and_place() {
        let world = World::synced();
        let report = world.kb().search("engine oil", 5, 0.0).unwrap();
        assert_eq!(report.hits.len(), 4, "every chunk, ranked");
        let top = &report.hits[0];
        assert_eq!(top.at.note.as_str(), "Car/Maintenance.md");
        assert_eq!(top.at.section, "Oil");
        assert_eq!(top.text, "I change the engine oil every 10000 km.");
        assert!(report.hits.windows(2).all(|w| w[0].score >= w[1].score));
    }

    #[test]
    fn limit_and_min_score_trim_the_list() {
        let world = World::synced();
        assert_eq!(
            world.kb().search("engine oil", 2, 0.0).unwrap().hits.len(),
            2
        );
        let strict = world.kb().search("engine oil", 5, 0.5).unwrap();
        assert_eq!(strict.hits.len(), 1);
        assert!(
            world
                .kb()
                .search("engine oil", 0, 0.0)
                .unwrap()
                .hits
                .is_empty()
        );
    }

    #[test]
    fn needs_a_synced_index_and_a_query() {
        let fresh = World::new(sample_notes());
        assert!(fresh.kb().search("oil", 5, 0.0).is_err());
        assert!(World::synced().kb().search(" ", 5, 0.0).is_err());
    }
}
