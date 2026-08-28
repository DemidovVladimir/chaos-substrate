//! Chunking policy: context-header format, size budget, and the segment merge
//! rules used for oversize units. Pure — vectors for the similarity merge are
//! passed in by the caller (the application layer owns the embedder).

use crate::domain::hash::sha256_hex;

/// Hard cap for a chunk (header + body), tuned for ~2048-token encoders.
pub const MAX_CHUNK_CHARS: usize = 6000;
/// Preferred upper size when merging or splitting.
pub const TARGET_CHUNK_CHARS: usize = 4500;
/// Segments smaller than this are merged into a neighbour before embedding.
pub const MIN_SEGMENT_CHARS: usize = 400;
/// Maximum size of the rendered context header.
pub const HEADER_MAX_CHARS: usize = 700;
/// Maximum size of the `file_head` chunk (imports + top-level statements).
pub const FILE_HEAD_MAX_CHARS: usize = 1500;
/// Default cosine threshold for merging adjacent segments of an oversize unit.
pub const DEFAULT_SPLIT_COS: f32 = 0.82;

const DOC_MAX_CHARS: usize = 300;

/// Everything the context header of a chunk is rendered from.
#[derive(Debug, Clone, Default)]
pub struct HeaderInput<'a> {
    pub path: &'a str,
    pub language: &'a str,
    pub line_start: usize,
    pub line_end: usize,
    pub container: Option<&'a str>,
    pub module: Option<&'a str>,
    pub kind: &'a str,
    pub name: &'a str,
    pub uses: &'a [String],
    pub calls: &'a [String],
    pub doc: Option<&'a str>,
}

/// Render the `// ...` context header prepended to every chunk body.
/// Empty lines are omitted; the result is capped at [`HEADER_MAX_CHARS`].
pub fn render_header(input: &HeaderInput) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(6);
    lines.push(format!(
        "// file: {} ({})  L{}-{}",
        input.path, input.language, input.line_start, input.line_end
    ));
    let container = input.container.filter(|c| !c.is_empty());
    let module = input.module.filter(|m| !m.is_empty());
    match (container, module) {
        (Some(c), Some(m)) => lines.push(format!("// in: {c}  |  module: {m}")),
        (Some(c), None) => lines.push(format!("// in: {c}")),
        (None, Some(m)) => lines.push(format!("// module: {m}")),
        (None, None) => {}
    }
    if !input.kind.is_empty() || !input.name.is_empty() {
        lines.push(format!("// kind: {}  name: {}", input.kind, input.name));
    }
    if !input.uses.is_empty() {
        lines.push(format!("// uses: {}", input.uses.join(", ")));
    }
    if !input.calls.is_empty() {
        lines.push(format!("// calls: {}", input.calls.join(", ")));
    }
    if let Some(doc) = input.doc {
        let doc = doc.trim();
        if !doc.is_empty() {
            let first: String = doc
                .chars()
                .take(DOC_MAX_CHARS)
                .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                .collect();
            lines.push(format!("// doc: {}", first.trim()));
        }
    }
    truncate_chars(&lines.join("\n"), HEADER_MAX_CHARS)
}

/// Content hash of a chunk: `sha256(header + "\n" + body)`.
pub fn content_hash(header: &str, body: &str) -> String {
    sha256_hex(&format!("{header}\n{body}"))
}

/// Stamp a part index on a header: `header + "\n// part i/n"`.
pub fn part_header(header: &str, i: usize, n: usize) -> String {
    format!("{header}\n// part {i}/{n}")
}

/// Turn raw child-statement ranges (byte offsets relative to `body`) into a
/// segment plan: covers the whole body (gaps merged into neighbours), every
/// segment ≥ [`MIN_SEGMENT_CHARS`] where possible, and nothing >
/// [`MAX_CHUNK_CHARS`] (line-split at ≤ [`TARGET_CHUNK_CHARS`]). All
/// boundaries are char boundaries.
pub fn plan_segments(body: &str, segments: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let len = body.len();
    if len == 0 {
        return Vec::new();
    }
    // Normalize: clamp, snap to char boundaries, drop empties, sort, de-overlap.
    let mut ranges: Vec<(usize, usize)> = segments
        .iter()
        .map(|&(s, e)| (floor_char(body, s.min(len)), floor_char(body, e.min(len))))
        .filter(|(s, e)| e > s)
        .collect();
    ranges.sort_unstable();
    let mut covering: Vec<(usize, usize)> = Vec::with_capacity(ranges.len() + 1);
    let mut cursor = 0usize;
    for (s, e) in ranges {
        let s = s.max(cursor);
        if e <= s {
            continue;
        }
        // Gap before this segment → extend the previous segment (or start here).
        if s > cursor {
            match covering.last_mut() {
                Some(last) => last.1 = s,
                None => covering.push((0, s)),
            }
        }
        covering.push((s, e));
        cursor = e;
    }
    if cursor < len {
        match covering.last_mut() {
            Some(last) => last.1 = len,
            None => covering.push((0, len)),
        }
    }
    // Merge small segments forward (into the next), and the trailing one backward.
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(covering.len());
    let mut pending: Option<(usize, usize)> = None;
    for (s, e) in covering {
        let (s, e) = match pending.take() {
            Some((ps, _)) => (ps, e),
            None => (s, e),
        };
        if e - s < MIN_SEGMENT_CHARS {
            pending = Some((s, e));
        } else {
            merged.push((s, e));
        }
    }
    if let Some((s, e)) = pending {
        match merged.last_mut() {
            Some(last) if e - s < MIN_SEGMENT_CHARS => last.1 = e,
            _ => merged.push((s, e)),
        }
    }
    // Split anything over the hard cap.
    let mut out = Vec::with_capacity(merged.len());
    for (s, e) in merged {
        if e - s > MAX_CHUNK_CHARS {
            for (ls, le) in line_split(&body[s..e], TARGET_CHUNK_CHARS) {
                out.push((s + ls, s + le));
            }
        } else {
            out.push((s, e));
        }
    }
    out
}

/// Greedy left-to-right merge of adjacent segments while their vectors' cosine
/// is ≥ `threshold` and the accumulated size stays ≤ [`TARGET_CHUNK_CHARS`].
/// `vectors[i]` is the embedding of `segments[i]`; a missing vector never merges.
pub fn merge_by_similarity(
    body: &str,
    segments: &[(usize, usize)],
    vectors: &[Vec<f32>],
    threshold: f32,
) -> Vec<(usize, usize)> {
    let len = body.len();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(segments.len());
    for (i, &(s, e)) in segments.iter().enumerate() {
        let (s, e) = (s.min(len), e.min(len));
        let Some(last) = out.last_mut() else {
            out.push((s, e));
            continue;
        };
        let contiguous = last.1 == s;
        let fits = e.saturating_sub(last.0) <= TARGET_CHUNK_CHARS;
        let similar = match (vectors.get(i - 1), vectors.get(i)) {
            (Some(a), Some(b)) => cosine(a, b) >= threshold,
            _ => false,
        };
        if contiguous && fits && similar {
            last.1 = e;
        } else {
            out.push((s, e));
        }
    }
    out
}

/// Fallback splitter: cut `body` into pieces of at most `max` bytes, preferring
/// the last newline before the limit; always on char boundaries.
pub fn line_split(body: &str, max: usize) -> Vec<(usize, usize)> {
    let max = max.max(1);
    let len = body.len();
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < len {
        if len - start <= max {
            out.push((start, len));
            break;
        }
        let limit = floor_char(body, start + max);
        let window = &body[start..limit];
        let cut = match window.rfind('\n') {
            Some(nl) if nl > 0 => start + nl + 1,
            _ => limit,
        };
        let cut = if cut <= start {
            floor_char(body, start + max).max(start + 1)
        } else {
            cut
        };
        let cut = floor_char(body, cut).max(next_char(body, start));
        out.push((start, cut));
        start = cut;
    }
    out
}

/// Cosine similarity; 0.0 for empty or zero-norm vectors.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

fn floor_char(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn next_char(s: &str, idx: usize) -> usize {
    let mut i = (idx + 1).min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    s[..floor_char(s, max)].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_renders_and_omits_empty_lines() {
        let uses = vec!["a".to_string(), "b".to_string()];
        let h = render_header(&HeaderInput {
            path: "src/x.rs",
            language: "rust",
            line_start: 3,
            line_end: 9,
            container: Some("Foo"),
            module: None,
            kind: "method",
            name: "bar",
            uses: &uses,
            calls: &[],
            doc: Some("  Does things.\nMore."),
        });
        assert_eq!(
            h,
            "// file: src/x.rs (rust)  L3-9\n// in: Foo\n// kind: method  name: bar\n// uses: a, b\n// doc: Does things. More."
        );
        let long_doc = "x".repeat(2000);
        let h = render_header(&HeaderInput {
            doc: Some(&long_doc),
            ..Default::default()
        });
        assert!(h.len() <= HEADER_MAX_CHARS);
        assert!(h.contains(&"x".repeat(300)));
        assert!(!h.contains(&"x".repeat(301)));
    }

    #[test]
    fn hash_and_part_header() {
        assert_eq!(content_hash("h", "b"), sha256_hex("h\nb"));
        assert_eq!(part_header("h", 2, 3), "h\n// part 2/3");
    }

    fn line_body(lines: usize) -> String {
        (0..lines)
            .map(|i| format!("line {i:04} some code here;\n"))
            .collect()
    }

    #[test]
    fn plan_covers_body_merges_small_and_splits_large() {
        let body = line_body(600); // ~15k chars
        let len = body.len();
        // Two tiny leading statements, one giant one, plus a gap at the tail.
        let segs = [(0, 30), (30, 60), (60, len - 100)];
        let plan = plan_segments(&body, &segs);
        assert_eq!(plan[0].0, 0);
        assert_eq!(plan.last().unwrap().1, len);
        for w in plan.windows(2) {
            assert_eq!(w[0].1, w[1].0, "contiguous");
        }
        for &(s, e) in &plan {
            assert!(e - s <= MAX_CHUNK_CHARS);
            assert!(body.is_char_boundary(s) && body.is_char_boundary(e));
        }
        // Tiny segments were merged: no segment under the minimum except possibly none.
        assert!(plan.iter().all(|(s, e)| e - s >= MIN_SEGMENT_CHARS));
        // Empty input still covers.
        assert_eq!(plan_segments("abc", &[]), vec![(0, 3)]);
        assert!(plan_segments("", &[(0, 0)]).is_empty());
    }

    #[test]
    fn similarity_merge_identical_until_cap_and_orthogonal_per_segment() {
        let body = "x".repeat(10_000);
        let segs: Vec<(usize, usize)> = (0..10).map(|i| (i * 1000, (i + 1) * 1000)).collect();
        let same: Vec<Vec<f32>> = (0..10).map(|_| vec![1.0, 0.0]).collect();
        let merged = merge_by_similarity(&body, &segs, &same, DEFAULT_SPLIT_COS);
        assert_eq!(merged, vec![(0, 4000), (4000, 8000), (8000, 10000)]);
        for &(s, e) in &merged {
            assert!(e - s <= TARGET_CHUNK_CHARS);
        }
        let ortho: Vec<Vec<f32>> = (0..10)
            .map(|i| {
                if i % 2 == 0 {
                    vec![1.0, 0.0]
                } else {
                    vec![0.0, 1.0]
                }
            })
            .collect();
        let merged = merge_by_similarity(&body, &segs, &ortho, DEFAULT_SPLIT_COS);
        assert_eq!(merged, segs);
        // Missing vectors never merge.
        assert_eq!(merge_by_similarity(&body, &segs, &[], 0.0), segs);
    }

    #[test]
    fn line_split_respects_char_boundaries_and_newlines() {
        let body: String = (0..300)
            .map(|i| format!("строка {i} — текст ünïcode\n"))
            .collect();
        let parts = line_split(&body, 500);
        assert_eq!(parts[0].0, 0);
        assert_eq!(parts.last().unwrap().1, body.len());
        for w in parts.windows(2) {
            assert_eq!(w[0].1, w[1].0);
        }
        for &(s, e) in &parts {
            assert!(body.is_char_boundary(s) && body.is_char_boundary(e));
            assert!(e - s <= 500);
            assert!(body[s..e].ends_with('\n') || e == body.len());
        }
        // No newlines at all: still splits on char boundaries.
        let solid = "é".repeat(1000);
        let parts = line_split(&solid, 7);
        assert_eq!(parts.last().unwrap().1, solid.len());
        for &(s, e) in &parts {
            assert!(solid.is_char_boundary(s) && solid.is_char_boundary(e) && e - s <= 7);
        }
        // Multibyte body through plan_segments stays on boundaries.
        let plan = plan_segments(&body, &[(1, 3), (3, 7000)]);
        for &(s, e) in &plan {
            assert!(body.is_char_boundary(s) && body.is_char_boundary(e));
        }
        assert_eq!(plan.last().unwrap().1, body.len());
    }
}
