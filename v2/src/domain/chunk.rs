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
