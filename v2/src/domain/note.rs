//! What the knowledge base is made of: notes, the blocks inside them, and
//! where a block or chunk sits.

use super::path::NotePath;
use serde::Serialize;

/// One Markdown note of the vault, already parsed into blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub path: NotePath,
    /// Content blocks in document order. Headings are not blocks; they are the
    /// `headings` path of the blocks below them.
    pub blocks: Vec<Block>,
}

/// The smallest unit of knowledge: a paragraph, one list item (without its
/// nested items, which are blocks of their own), a code block, a table, a
/// quote, or the front matter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// Heading texts above this block, outermost first (`["Car", "Oil"]`).
    pub headings: Vec<String>,
    /// Plain text: Markdown syntax removed, wikilinks reduced to their label.
    pub text: String,
    /// The block's Markdown source, exactly as written in the file.
    pub source: String,
    /// 1-based first and last line in the file.
    pub lines: [u32; 2],
}

/// Where something sits in the vault: note, heading path, lines.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Location {
    pub note: NotePath,
    /// Heading path joined with ` > `; empty for text above the first heading.
    pub section: String,
    pub lines: [u32; 2],
}

/// Join a heading path for display: `["Car", "Oil"]` → `Car > Oil`.
pub fn section(headings: &[String]) -> String {
    headings.join(" > ")
}
