//! Chunks: the units that get embedded and searched. A chunk packs
//! consecutive blocks under the same heading path, up to a size limit, and
//! carries a short context line (note title and headings) for the embedder.

use super::hash::{self, Digest};
use super::note::{Location, Note, section};
use super::text;

/// Upper bound on a chunk's text, in characters (about 300 tokens — small
/// enough that one chunk is about one thing, far below the embedder's limit).
pub const MAX_CHUNK_CHARS: usize = 1200;

/// One searchable passage of a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub at: Location,
    /// `Title > Heading > Subheading` — the note title, then the section.
    pub context: String,
    /// The Markdown source of its blocks, joined by blank lines.
    pub text: String,
    /// Content key: hash of `context` and `text`. Equal keys mean an equal
    /// embedding input, so a stored vector can be reused.
    pub key: Digest,
}

impl Chunk {
    /// Build a chunk; the key is derived, never supplied.
    pub fn new(at: Location, context: String, text: String) -> Chunk {
        let key = hash::node([
            ("context", hash::leaf(context.as_bytes())),
            ("text", hash::leaf(text.as_bytes())),
        ]);
        Chunk {
            at,
            context,
            text,
            key,
        }
    }
}

/// Split `note` into chunks of at most `max_chars` characters of text:
/// - blocks whose text is blank (no letter or digit) are skipped;
/// - consecutive blocks with the same heading path are packed together
///   (sources joined by `"\n\n"`) while the result fits in `max_chars`;
/// - a single block longer than `max_chars` is cut at whitespace (at a hard
///   character boundary when a piece has no whitespace) into pieces of at most
///   `max_chars` characters, each its own chunk carrying the block's lines;
/// - `context` is the note title, plus ` > ` and the section when there is one;
/// - `at.lines` spans the first to the last block of the chunk.
pub fn chunks(note: &Note, max_chars: usize) -> Vec<Chunk> {
    let _ = (note, max_chars, section, text::is_blank);
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::note::Block;
    use crate::domain::path::NotePath;

    fn block(headings: &[&str], source: &str, lines: [u32; 2]) -> Block {
        Block {
            headings: headings.iter().map(|h| h.to_string()).collect(),
            text: source.to_string(),
            source: source.to_string(),
            lines,
        }
    }

    fn note(path: &str, blocks: Vec<Block>) -> Note {
        Note {
            path: NotePath::parse(path).unwrap(),
            blocks,
        }
    }

    fn texts(chunks: &[Chunk]) -> Vec<&str> {
        chunks.iter().map(|c| c.text.as_str()).collect()
    }

    fn squeeze(s: &str) -> String {
        s.chars().filter(|c| !c.is_whitespace()).collect()
    }

    #[test]
    fn packs_a_section_into_one_chunk() {
        let n = note(
            "Garage/Car maintenance.md",
            vec![
                block(&["Car", "Oil"], "Castrol 5W-30.", [3, 4]),
                block(&["Car", "Oil"], "- every 10k km", [6, 6]),
                block(&["Car", "Oil"], "Filter: Mann W 712.", [8, 10]),
            ],
        );
        let got = chunks(&n, MAX_CHUNK_CHARS);
        assert_eq!(got.len(), 1);
        let c = &got[0];
        assert_eq!(
            c.text,
            "Castrol 5W-30.\n\n- every 10k km\n\nFilter: Mann W 712."
        );
        assert_eq!(c.context, "Car maintenance > Car > Oil");
        assert_eq!(
            c.at,
            Location {
                note: n.path.clone(),
                section: "Car > Oil".to_string(),
                lines: [3, 10],
            }
        );
        assert_eq!(
            *c,
            Chunk::new(c.at.clone(), c.context.clone(), c.text.clone())
        );
    }

    #[test]
    fn chunk_text_is_the_markdown_source() {
        let mut b = block(&[], "- **Bold** fact", [1, 1]);
        b.text = "Bold fact".to_string();
        let got = chunks(&note("n.md", vec![b]), MAX_CHUNK_CHARS);
        assert_eq!(texts(&got), ["- **Bold** fact"]);
    }

    #[test]
    fn text_above_the_first_heading_has_the_title_as_context() {
        let got = chunks(
            &note("Health/Meds.md", vec![block(&[], "Vitamin D", [1, 1])]),
            MAX_CHUNK_CHARS,
        );
        assert_eq!(got[0].context, "Meds");
        assert_eq!(got[0].at.section, "");
    }

    #[test]
    fn packs_up_to_exactly_max_chars() {
        let n = note(
            "n.md",
            vec![
                block(&[], "aaaa", [1, 1]),
                block(&[], "bbbb", [3, 3]),
                block(&[], "cccc", [5, 5]),
            ],
        );
        // "aaaa\n\nbbbb" is exactly 10 characters.
        assert_eq!(texts(&chunks(&n, 10)), ["aaaa\n\nbbbb", "cccc"]);
        assert_eq!(texts(&chunks(&n, 9)), ["aaaa", "bbbb", "cccc"]);
        assert_eq!(texts(&chunks(&n, 16)), ["aaaa\n\nbbbb\n\ncccc"]);
        assert_eq!(texts(&chunks(&n, 15)), ["aaaa\n\nbbbb", "cccc"]);
        let spans: Vec<[u32; 2]> = chunks(&n, 10).iter().map(|c| c.at.lines).collect();
        assert_eq!(spans, [[1, 3], [5, 5]]);
    }

    #[test]
    fn measures_characters_not_bytes() {
        // 10 characters, 18 bytes.
        let n = note(
            "n.md",
            vec![block(&[], "жжжж", [1, 1]), block(&[], "ёёёё", [2, 2])],
        );
        assert_eq!(texts(&chunks(&n, 10)), ["жжжж\n\nёёёё"]);
        assert_eq!(texts(&chunks(&n, 4)), ["жжжж", "ёёёё"]);
    }

    #[test]
    fn a_heading_change_starts_a_new_chunk() {
        let n = note(
            "Car.md",
            vec![
                block(&["Oil"], "x", [2, 2]),
                block(&["Oil"], "y", [3, 3]),
                block(&["Tyres"], "z", [5, 5]),
                block(&["Oil"], "w", [7, 7]),
                block(&["Oil", "Brand"], "v", [9, 9]),
            ],
        );
        let got = chunks(&n, MAX_CHUNK_CHARS);
        assert_eq!(texts(&got), ["x\n\ny", "z", "w", "v"]);
        let sections: Vec<&str> = got.iter().map(|c| c.at.section.as_str()).collect();
        assert_eq!(sections, ["Oil", "Tyres", "Oil", "Oil > Brand"]);
        assert_eq!(got[3].context, "Car > Oil > Brand");
        let spans: Vec<[u32; 2]> = got.iter().map(|c| c.at.lines).collect();
        assert_eq!(spans, [[2, 3], [5, 5], [7, 7], [9, 9]]);
    }

    #[test]
    fn blank_blocks_are_skipped() {
        let mut image = block(&["A"], "![](photo.png)", [3, 3]);
        image.text = String::new();
        let n = note(
            "n.md",
            vec![
                block(&["A"], "first", [1, 1]),
                image,
                block(&["A"], "---", [5, 5]),
                block(&["A"], "second", [7, 7]),
            ],
        );
        let got = chunks(&n, MAX_CHUNK_CHARS);
        assert_eq!(texts(&got), ["first\n\nsecond"]);
        assert_eq!(got[0].at.lines, [1, 7]);
        let blank = note(
            "b.md",
            vec![block(&[], "***", [1, 1]), block(&[], " ", [2, 2])],
        );
        assert!(chunks(&blank, MAX_CHUNK_CHARS).is_empty());
        assert!(chunks(&note("e.md", vec![]), MAX_CHUNK_CHARS).is_empty());
    }

    #[test]
    fn an_oversize_block_is_cut_at_whitespace_into_its_own_chunks() {
        let n = note(
            "Notes.md",
            vec![
                block(&["A"], "pre", [1, 1]),
                block(&["A"], "alpha beta gamma delta", [3, 5]),
                block(&["A"], "post", [7, 7]),
            ],
        );
        let got = chunks(&n, 10);
        assert_eq!(texts(&got), ["pre", "alpha beta", "gamma", "delta", "post"]);
        for piece in &got[1..4] {
            assert_eq!(piece.at.lines, [3, 5]);
            assert_eq!(piece.at.section, "A");
            assert_eq!(piece.context, "Notes > A");
        }
    }

    #[test]
    fn a_cut_prefers_the_last_whitespace_that_fits() {
        let cut = |s: &str, max: usize| -> Vec<String> {
            let got = chunks(&note("n.md", vec![block(&[], s, [1, 1])]), max);
            got.into_iter().map(|c| c.text).collect()
        };
        assert_eq!(
            cut("abcde fgh", 5),
            ["abcde", "fgh"],
            "space right after the limit"
        );
        assert_eq!(cut("ab cd efgh ij", 8), ["ab cd", "efgh ij"]);
        assert_eq!(cut("aaaaaaaaaaaa bb", 5), ["aaaaa", "aaaaa", "aa bb"]);
        assert_eq!(cut("one\n\ntwo  three", 6), ["one", "two", "three"]);
    }

    #[test]
    fn a_block_without_whitespace_is_cut_on_character_boundaries() {
        let word = "жёлтый🙂".repeat(4); // 28 characters, 64 bytes
        let got = chunks(&note("n.md", vec![block(&[], &word, [2, 9])]), 5);
        let sizes: Vec<usize> = got.iter().map(|c| c.text.chars().count()).collect();
        assert_eq!(sizes, [5, 5, 5, 5, 5, 3]);
        assert_eq!(
            got.iter().map(|c| c.text.as_str()).collect::<String>(),
            word
        );
        assert!(got.iter().all(|c| c.at.lines == [2, 9]));
    }

    #[test]
    fn cut_pieces_fit_and_lose_nothing_but_whitespace() {
        let inputs = [
            "alpha beta gamma delta epsilon zeta",
            "жжжжжжжжжжжжжжж ёёё\n\nщщщщ   ыыы",
            "🙂🙂🙂 🙂🙂🙂🙂🙂🙂🙂🙂 🙂 東京タワー東京タワー",
            "line one\nline two\n\n    indented\tcode()",
            "x",
        ];
        for source in inputs {
            for max in [1, 2, 3, 5, 8, 13] {
                let got = chunks(&note("n.md", vec![block(&[], source, [1, 1])]), max);
                for c in &got {
                    let n = c.text.chars().count();
                    assert!(n >= 1 && n <= max, "{source:?} max {max}: {:?}", c.text);
                }
                let joined: String = got.iter().map(|c| squeeze(&c.text)).collect();
                assert_eq!(joined, squeeze(source), "{source:?} max {max}");
            }
        }
    }

    #[test]
    #[should_panic(expected = "max_chars")]
    fn zero_max_chars_is_a_bug() {
        chunks(&note("n.md", vec![block(&[], "x", [1, 1])]), 0);
    }

    #[test]
    fn equal_input_gives_equal_key() {
        let a = chunks(&note("x/Plan.md", vec![block(&["A"], "rest", [1, 1])]), 100);
        let b = chunks(&note("y/Plan.md", vec![block(&["A"], "rest", [9, 9])]), 100);
        assert_ne!(a[0].at, b[0].at);
        assert_eq!(a[0].key, b[0].key, "same title, section and text");
    }

    #[test]
    fn context_and_text_both_change_the_key() {
        let key = |path: &str, heading: &str, text: &str| {
            chunks(&note(path, vec![block(&[heading], text, [1, 1])]), 100)[0].key
        };
        let base = key("Plan.md", "A", "rest");
        assert_ne!(base, key("Other.md", "A", "rest"), "title");
        assert_ne!(base, key("Plan.md", "B", "rest"), "section");
        assert_ne!(base, key("Plan.md", "A", "rest more"), "text");
    }
}
