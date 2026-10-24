//! `check`: is this already known? First the exact path — the query's
//! fingerprint against the Merkle leaves, no embedding — then the nearest
//! passages by cosine similarity.

use super::{Kb, nonblank};
use crate::domain::hash::Digest;
use crate::domain::note::Location;
use crate::domain::presence::{self, Presence};
use crate::domain::text::fingerprint;
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
        let query = nonblank(query, "query")?;
        let root = self.ready()?.tree.root();
        let exact = match fingerprint(query) {
            Some(fp) => self.index.exact(&fp)?,
            None => Vec::new(),
        };
        if !exact.is_empty() {
            return Ok(CheckReport {
                known: true,
                presence: Presence::Exact,
                score: None,
                matches: exact
                    .into_iter()
                    .map(|at| Match { at, score: None })
                    .collect(),
                root,
            });
        }
        let hits = self.similar(query, CHECK_K)?;
        let best = hits.first().map(|h| h.score);
        let presence = presence::judge(false, best, min_score);
        let matches = hits
            .into_iter()
            .filter(|h| h.score >= min_score)
            .map(|h| Match {
                at: h.chunk.at,
                score: Some(h.score),
            })
            .collect();
        Ok(CheckReport {
            known: presence.is_known(),
            presence,
            score: best,
            matches,
            root,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{World, sample_notes};
    use super::*;
    use crate::domain::presence::DEFAULT_MIN_SCORE;

    #[test]
    fn the_same_words_are_found_without_embedding() {
        let world = World::synced();
        let report = world
            .kb()
            .check("allergy — PENICILLIN!", DEFAULT_MIN_SCORE)
            .unwrap();
        assert!(report.known);
        assert_eq!(report.presence, Presence::Exact);
        assert_eq!(report.score, None);
        assert_eq!(report.matches.len(), 1);
        assert_eq!(report.matches[0].at.note.as_str(), "Health/Allergies.md");
        assert_eq!(report.matches[0].score, None);
        assert_eq!(
            world.embedder.queries.get(),
            0,
            "no embedding on the exact path"
        );
        let root = world.kb().sync(true).unwrap().root;
        assert_eq!(report.root, root);
    }

    #[test]
    fn a_related_question_is_similar() {
        let world = World::synced();
        let report = world
            .kb()
            .check("how often do I replace the oil?", DEFAULT_MIN_SCORE)
            .unwrap();
        assert!(report.known);
        assert_eq!(report.presence, Presence::Similar);
        let best = report.score.unwrap();
        assert!(best >= DEFAULT_MIN_SCORE);
        assert_eq!(report.matches[0].at.note.as_str(), "Car/Maintenance.md");
        assert_eq!(report.matches[0].score, Some(best));
        assert!(
            report
                .matches
                .iter()
                .all(|m| m.score.unwrap() >= DEFAULT_MIN_SCORE)
        );
        assert_eq!(world.embedder.queries.get(), 1);
    }

    #[test]
    fn an_unrelated_question_is_absent_but_reports_the_best_score() {
        let world = World::synced();
        let report = world
            .kb()
            .check("what is my favourite colour", DEFAULT_MIN_SCORE)
            .unwrap();
        assert!(!report.known);
        assert_eq!(report.presence, Presence::Absent);
        assert!(report.score.unwrap() < DEFAULT_MIN_SCORE);
        assert!(report.matches.is_empty());
    }

    #[test]
    fn min_score_moves_the_line() {
        let world = World::synced();
        let strict = world.kb().check("oil", 1.01).unwrap();
        assert_eq!(strict.presence, Presence::Absent);
        let lenient = world.kb().check("favourite colour", -1.0).unwrap();
        assert_eq!(lenient.presence, Presence::Similar);
        assert_eq!(lenient.matches.len(), CHECK_K);
    }

    #[test]
    fn needs_a_synced_index_from_the_same_embedder() {
        let fresh = World::new(sample_notes());
        let err = fresh.kb().check("oil", DEFAULT_MIN_SCORE).unwrap_err();
        assert!(err.to_string().contains("chaos-kb sync"), "{err}");
        let mut moved = World::synced();
        moved.embedder.id = "other|v1".into();
        let err = moved.kb().check("oil", DEFAULT_MIN_SCORE).unwrap_err();
        assert!(err.to_string().contains("chaos-kb sync"), "{err}");
    }

    #[test]
    fn a_blank_query_is_refused() {
        let world = World::synced();
        assert!(world.kb().check("  ?! ", DEFAULT_MIN_SCORE).is_err());
    }
}
