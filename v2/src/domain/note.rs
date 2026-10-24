//! What the knowledge base is made of: notes, the blocks inside them, and
//! where a block or chunk sits.

use super::path::NotePath;
use anyhow::{Context, Result, ensure};
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
/// quote, an HTML block, or the front matter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// Heading texts above this block, outermost first (`["Car", "Oil"]`).
    pub headings: Vec<String>,
    /// Plain text: Markdown syntax removed; a link reads as its label then its
    /// destination (once when they are the same); images keep their alt text.
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

impl Location {
    /// The passage's id, short and readable: `note:first-last`
    /// (`Car/Maintenance.md:3-5`).
    pub fn id(&self) -> String {
        format!("{}:{}-{}", self.note.as_str(), self.lines[0], self.lines[1])
    }
}

/// Split a passage id (see [`Location::id`]) into its note and line range.
/// Note paths never hold `:` (see [`NotePath::parse`]), so the split is exact.
pub fn parse_id(id: &str) -> Result<(NotePath, [u32; 2])> {
    let id = id.trim();
    let shape = || format!("`{id}` is not a passage id (note:first-last, like `Car.md:8-8`)");
    let (note, range) = id.rsplit_once(':').with_context(shape)?;
    let (first, last) = range.split_once('-').with_context(shape)?;
    let line = |n: &str| n.parse::<u32>().ok().filter(|&n| n > 0);
    let (Some(first), Some(last)) = (line(first), line(last)) else {
        anyhow::bail!(shape());
    };
    ensure!(first <= last, shape());
    let note = NotePath::parse(note).with_context(shape)?;
    Ok((note, [first, last]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(note: &str, lines: [u32; 2]) -> Location {
        Location {
            note: NotePath::parse(note).unwrap(),
            section: "Oil".into(),
            lines,
        }
    }

    #[test]
    fn an_id_is_note_and_lines() {
        assert_eq!(
            at("Car/Maintenance.md", [3, 5]).id(),
            "Car/Maintenance.md:3-5"
        );
        assert_eq!(at("Inbox.md", [7, 7]).id(), "Inbox.md:7-7");
    }

    #[test]
    fn ids_parse_back() {
        for (note, lines) in [
            ("Car/Maintenance.md", [3, 5]),
            ("Health/Sleep notes.md", [1, 12]),
        ] {
            let parsed = parse_id(&at(note, lines).id()).unwrap();
            assert_eq!(parsed, (NotePath::parse(note).unwrap(), lines));
        }
        assert_eq!(
            parse_id(" Food.md:3-6 ").unwrap(),
            (NotePath::parse("Food.md").unwrap(), [3, 6]),
            "surrounding spaces are not part of the id"
        );
    }

    #[test]
    fn a_malformed_id_says_what_an_id_looks_like() {
        for bad in [
            "Food.md",
            "Food.md:",
            "Food.md:3",
            "Food.md:6-3",
            "Food.md:0-2",
            "Food.md:a-b",
            ":3-5",
        ] {
            let e = parse_id(bad).unwrap_err().to_string();
            assert!(e.contains("note:first-last"), "{bad}: {e}");
        }
    }
}
