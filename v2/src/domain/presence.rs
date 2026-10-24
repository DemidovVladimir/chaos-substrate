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
    if exact {
        Presence::Exact
    } else if best.is_some_and(|score| score >= min_score) {
        Presence::Similar
    } else {
        Presence::Absent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exact_match_wins_whatever_the_score() {
        for best in [None, Some(-1.0), Some(0.1), Some(0.99)] {
            assert_eq!(judge(true, best, DEFAULT_MIN_SCORE), Presence::Exact);
        }
    }

    #[test]
    fn similar_from_min_score_inclusive() {
        assert_eq!(judge(false, Some(0.5), 0.5), Presence::Similar);
        assert_eq!(judge(false, Some(0.93), 0.5), Presence::Similar);
        assert_eq!(judge(false, Some(0.8), 0.8), Presence::Similar);
        assert_eq!(
            judge(false, Some(0.5_f32.next_down()), 0.5),
            Presence::Absent
        );
    }

    #[test]
    fn absent_without_a_close_enough_passage() {
        assert_eq!(judge(false, None, DEFAULT_MIN_SCORE), Presence::Absent);
        assert_eq!(
            judge(false, None, -1.0),
            Presence::Absent,
            "none is never similar"
        );
        assert_eq!(judge(false, Some(0.2), 0.5), Presence::Absent);
        assert_eq!(judge(false, Some(-1.0), 0.5), Presence::Absent);
        assert_eq!(judge(false, Some(f32::NAN), 0.5), Presence::Absent);
    }

    #[test]
    fn only_absent_is_unknown_and_names_are_lowercase() {
        assert!(Presence::Exact.is_known());
        assert!(Presence::Similar.is_known());
        assert!(!Presence::Absent.is_known());
        assert_eq!(serde_json::to_value(Presence::Similar).unwrap(), "similar");
    }
}
