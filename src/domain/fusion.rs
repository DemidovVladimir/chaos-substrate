//! Retrieval fusion: per-channel normalization, channel weights, the merge
//! rule for chunks surfaced by several channels, and community routing.

use std::collections::HashMap;
use uuid::Uuid;

/// A retrieval channel and its fixed weight in the fused score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Channel {
    /// Embedding cosine (weight 1.0).
    Semantic,
    /// Full-text `ts_rank_cd` (weight 0.6).
    Keyword,
    /// Case-insensitive substring match (weight 0.4).
    Literal,
}

impl Channel {
    /// Weight applied after per-channel max-normalization.
    pub fn weight(self) -> f32 {
        match self {
            Channel::Semantic => 1.0,
            Channel::Keyword => 0.6,
            Channel::Literal => 0.4,
        }
    }

    /// Stable lower-case name (`semantic` / `keyword` / `literal`).
    pub fn name(self) -> &'static str {
        match self {
            Channel::Semantic => "semantic",
            Channel::Keyword => "keyword",
            Channel::Literal => "literal",
        }
    }
}

/// One raw hit from one channel.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub key: Uuid,
    pub score: f32,
    pub channel: Channel,
}

/// A fused hit: the merged score and every channel that surfaced it.
#[derive(Debug, Clone, PartialEq)]
pub struct Fused {
    pub key: Uuid,
    pub score: f32,
    pub channels: Vec<Channel>,
}

/// Fuse channel result lists into one ranking.
///
/// Each channel is normalized by its own max score and multiplied by its
/// weight. Keys are merged: the first sighting sets the score, every later
/// sighting adds `0.25 × its weighted score`; keys seen in a single channel
/// only are damped by `0.75`. Sorted by score descending, ties broken by key.
pub fn fuse(channels: Vec<Vec<Candidate>>, limit: usize) -> Vec<Fused> {
    let mut merged: HashMap<Uuid, Fused> = HashMap::new();
    let mut order: Vec<Uuid> = Vec::new();
    for list in channels {
        let max = list.iter().map(|c| c.score).fold(0.0f32, f32::max);
        for cand in list {
            let normalized = if max > 0.0 {
                (cand.score / max).max(0.0)
            } else {
                0.0
            };
            let weighted = normalized * cand.channel.weight();
            match merged.get_mut(&cand.key) {
                Some(existing) => {
                    if existing.channels.contains(&cand.channel) {
                        // Duplicate inside one channel: keep the best score.
                        existing.score = existing.score.max(weighted);
                    } else {
                        existing.score += 0.25 * weighted;
                        existing.channels.push(cand.channel);
                    }
                }
                None => {
                    order.push(cand.key);
                    merged.insert(
                        cand.key,
                        Fused {
                            key: cand.key,
                            score: weighted,
                            channels: vec![cand.channel],
                        },
                    );
                }
            }
        }
    }
    let mut out: Vec<Fused> = order
        .into_iter()
        .filter_map(|k| merged.remove(&k))
        .map(|mut f| {
            if f.channels.len() == 1 {
                f.score *= 0.75;
            }
            f
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.key.cmp(&b.key))
    });
    out.truncate(limit);
    out
}

/// Hierarchical routing: keep every community scoring ≥ `floor`, but never
/// fewer than `min_keep` (when available) nor more than `max_keep`.
/// Deterministic: sorted by score descending, ties broken by id.
pub fn route_communities(
    scored: &[(Uuid, f32)],
    floor: f32,
    min_keep: usize,
    max_keep: usize,
) -> Vec<Uuid> {
    let mut ranked: Vec<(Uuid, f32)> = scored.to_vec();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked
        .into_iter()
        .enumerate()
        .take_while(|(i, (_, score))| *i < max_keep && (*score >= floor || *i < min_keep))
        .map(|(_, (id, _))| id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn normalizes_weights_and_merges() {
        let sem = vec![
            Candidate {
                key: id(1),
                score: 0.8,
                channel: Channel::Semantic,
            },
            Candidate {
                key: id(2),
                score: 0.4,
                channel: Channel::Semantic,
            },
        ];
        let kw = vec![
            Candidate {
                key: id(2),
                score: 5.0,
                channel: Channel::Keyword,
            },
            Candidate {
                key: id(3),
                score: 5.0,
                channel: Channel::Keyword,
            },
        ];
        let fused = fuse(vec![sem, kw], 10);
        let by: HashMap<Uuid, &Fused> = fused.iter().map(|f| (f.key, f)).collect();
        // id1: 1.0 * 1.0, single channel → 0.75
        assert!((by[&id(1)].score - 0.75).abs() < 1e-6);
        // id2: 0.5 + 0.25 * 0.6 = 0.65, two channels
        assert!((by[&id(2)].score - 0.65).abs() < 1e-6);
        assert_eq!(
            by[&id(2)].channels,
            vec![Channel::Semantic, Channel::Keyword]
        );
        // id3: 0.6 single → 0.45
        assert!((by[&id(3)].score - 0.45).abs() < 1e-6);
        assert_eq!(fused[0].key, id(1));
        assert_eq!(fused[1].key, id(2));
        assert_eq!(fused[2].key, id(3));
        assert_eq!(fuse(vec![], 5).len(), 0);
    }

    #[test]
    fn deterministic_tie_break_and_limit() {
        let lit = vec![
            Candidate {
                key: id(9),
                score: 1.0,
                channel: Channel::Literal,
            },
            Candidate {
                key: id(4),
                score: 1.0,
                channel: Channel::Literal,
            },
            Candidate {
                key: id(7),
                score: 1.0,
                channel: Channel::Literal,
            },
        ];
        let fused = fuse(vec![lit], 2);
        assert_eq!(
            fused.iter().map(|f| f.key).collect::<Vec<_>>(),
            vec![id(4), id(7)]
        );
        assert!((fused[0].score - 0.3).abs() < 1e-6);
    }

    #[test]
    fn routing_respects_floor_min_max() {
        let scored = vec![(id(1), 0.9), (id(2), 0.5), (id(3), 0.2), (id(4), 0.1)];
        assert_eq!(route_communities(&scored, 0.35, 2, 8), vec![id(1), id(2)]);
        assert_eq!(route_communities(&scored, 0.95, 2, 8), vec![id(1), id(2)]);
        assert_eq!(
            route_communities(&scored, 0.0, 2, 3),
            vec![id(1), id(2), id(3)]
        );
        assert_eq!(route_communities(&scored, 0.95, 1, 0), Vec::<Uuid>::new());
        assert_eq!(route_communities(&[], 0.1, 2, 5), Vec::<Uuid>::new());
    }
}
