//! Token packer for the `feature` answer: splits a token budget into shares
//! (summaries / excerpts / graph+surfaces / envelope) and decides how much of
//! each excerpt survives. 4 chars ≈ 1 token.

/// Characters per token used for all estimates.
pub const CHARS_PER_TOKEN: usize = 4;
/// Minimum characters a clipped excerpt body must keep to be worth emitting.
pub const MIN_CLIP_CHARS: usize = 600;

const SUMMARY_SHARE: f64 = 0.15;
const EXCERPT_SHARE: f64 = 0.70;
const GRAPH_SHARE: f64 = 0.10;

/// How much of one excerpt is emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExcerptMode {
    /// Header + whole body.
    Full,
    /// Header + the first `n` chars of the body (cut with [`clip_at_line`]).
    Clipped(usize),
    /// Header line only — keeps symbol coverage wide when the body won't fit.
    HeaderOnly,
}

/// Size of a candidate excerpt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item {
    pub header_chars: usize,
    pub body_chars: usize,
}

/// Packing decision. Indices refer to the input slices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packed {
    /// Indices of the summaries that fit, in input order.
    pub summaries: Vec<usize>,
    /// `(index, mode)` for every excerpt that survived, in input order.
    pub excerpts: Vec<(usize, ExcerptMode)>,
    /// Characters granted to the graph + surfaces section.
    pub graph_chars: usize,
    /// Estimated tokens consumed (including the 5% envelope share).
    pub tokens_used: usize,
}

/// Pack summaries and excerpts under `budget_tokens`.
///
/// Shares: summaries 15%, excerpts 70%, graph+surfaces 10%, envelope 5%.
/// Summaries are taken in order while they fit; their unused share flows to
/// excerpts. Excerpts are taken in order: `Full` if it fits, else `Clipped`
/// to the remaining share when ≥ [`MIN_CLIP_CHARS`] remain, else
/// `HeaderOnly` if the header fits, else skipped.
pub fn pack(
    budget_tokens: usize,
    summaries: &[usize],
    excerpts: &[Item],
    graph_chars: usize,
) -> Packed {
    let total_chars = budget_tokens * CHARS_PER_TOKEN;
    let share = |f: f64| (total_chars as f64 * f).floor() as usize;
    let summary_cap = share(SUMMARY_SHARE);
    let graph_cap = share(GRAPH_SHARE);
    let envelope_chars = total_chars.saturating_sub(summary_cap + share(EXCERPT_SHARE) + graph_cap);

    let mut summary_used = 0usize;
    let mut kept_summaries = Vec::new();
    for (i, &chars) in summaries.iter().enumerate() {
        if summary_used + chars <= summary_cap {
            summary_used += chars;
            kept_summaries.push(i);
        }
    }

    let mut remaining = share(EXCERPT_SHARE) + (summary_cap - summary_used);
    let mut kept_excerpts = Vec::new();
    let mut excerpt_used = 0usize;
    for (i, item) in excerpts.iter().enumerate() {
        let full = item.header_chars + item.body_chars;
        let mode = if full <= remaining {
            Some((ExcerptMode::Full, full))
        } else if remaining > item.header_chars && remaining - item.header_chars >= MIN_CLIP_CHARS {
            let clip = remaining - item.header_chars;
            Some((ExcerptMode::Clipped(clip), item.header_chars + clip))
        } else if item.header_chars <= remaining {
            Some((ExcerptMode::HeaderOnly, item.header_chars))
        } else {
            None
        };
        if let Some((mode, cost)) = mode {
            remaining -= cost;
            excerpt_used += cost;
            kept_excerpts.push((i, mode));
        }
    }

    let graph_granted = graph_chars.min(graph_cap);
    let used_chars = summary_used + excerpt_used + graph_granted + envelope_chars;
    Packed {
        summaries: kept_summaries,
        excerpts: kept_excerpts,
        graph_chars: graph_granted,
        tokens_used: used_chars.div_ceil(CHARS_PER_TOKEN),
    }
}

/// Clip `body` to at most `max_chars` bytes, cutting at the last newline
/// before the limit when there is one; always on a char boundary.
pub fn clip_at_line(body: &str, max_chars: usize) -> &str {
    if body.len() <= max_chars {
        return body;
    }
    let mut idx = max_chars;
    while idx > 0 && !body.is_char_boundary(idx) {
        idx -= 1;
    }
    match body[..idx].rfind('\n') {
        Some(nl) if nl > 0 => &body[..nl],
        _ => &body[..idx],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_and_modes() {
        // 1000 tokens = 4000 chars: summaries 600, excerpts 2800, graph 400, envelope 200.
        let summaries = [300, 200, 200];
        let excerpts = [
            Item {
                header_chars: 100,
                body_chars: 1000,
            },
            Item {
                header_chars: 100,
                body_chars: 2000,
            },
            Item {
                header_chars: 100,
                body_chars: 5000,
            },
            Item {
                header_chars: 100,
                body_chars: 50,
            },
            Item {
                header_chars: 9000,
                body_chars: 1,
            },
        ];
        let p = pack(1000, &summaries, &excerpts, 1000);
        assert_eq!(p.summaries, vec![0, 1]); // third summary would exceed 600
                                             // Excerpt share = 2800 + 100 unused summary = 2900.
                                             // Full (1100 → 1800 left), Clipped (2100 doesn't fit; 1700 ≥ 600 → 0 left),
                                             // then nothing fits: even a 100-char header is skipped.
        assert_eq!(
            p.excerpts,
            vec![(0, ExcerptMode::Full), (1, ExcerptMode::Clipped(1700))]
        );
        assert_eq!(p.graph_chars, 400);
        // 500 + 2900 + 400 + 200 = 4000 chars = 1000 tokens
        assert_eq!(p.tokens_used, 1000);
    }

    #[test]
    fn header_only_and_skip() {
        // 250 tokens = 1000 chars: excerpts 700 (+150 summary unused = 850).
        let excerpts = [
            Item {
                header_chars: 50,
                body_chars: 500,
            }, // Full → 300 left
            Item {
                header_chars: 50,
                body_chars: 900,
            }, // 250 < 600 clip → HeaderOnly → 250 left
            Item {
                header_chars: 300,
                body_chars: 10,
            }, // header 300 > 250 → skipped
            Item {
                header_chars: 250,
                body_chars: 10,
            }, // HeaderOnly → 0 left
        ];
        let p = pack(250, &[], &excerpts, 0);
        assert_eq!(
            p.excerpts,
            vec![
                (0, ExcerptMode::Full),
                (1, ExcerptMode::HeaderOnly),
                (3, ExcerptMode::HeaderOnly)
            ]
        );
        assert!(p.summaries.is_empty());
        assert_eq!(p.graph_chars, 0);
        assert!(p.tokens_used <= 250);
        let empty = pack(0, &[10], &excerpts, 10);
        assert!(empty.summaries.is_empty() && empty.excerpts.is_empty());
        assert_eq!(empty.tokens_used, 0);
    }

    #[test]
    fn clip_is_line_and_char_safe() {
        assert_eq!(clip_at_line("abc", 10), "abc");
        assert_eq!(clip_at_line("line1\nline2\nline3", 9), "line1");
        assert_eq!(clip_at_line("nonewline-here", 5), "nonew");
        let s = "ééé\nééé";
        let c = clip_at_line(s, 5);
        assert!(s.is_char_boundary(c.len()));
        assert_eq!(c, "éé");
        let c = clip_at_line("é\nééé", 3);
        assert_eq!(c, "é");
    }
}
