//! Markdown / MDX extraction (pulldown-cmark, not tree-sitter).
//!
//! Every H1–H3 heading opens a `Section` symbol that runs until the next
//! heading of depth ≤ 3 (deeper headings stay inside their section).
//! `qualified_name` is the heading path joined by `" > "`, `parent` is the
//! enclosing shallower section, segments are the section's top-level blocks
//! (paragraphs, lists, code blocks). The preamble before the first heading
//! becomes a free range (→ the file_head chunk); a file without headings has
//! no symbols at all, so the file_head chunk covers it.

use super::walker::file_level;
use crate::domain::model::NodeKind;
use crate::ports::extractor::{FileExtraction, FileInput, Symbol};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use std::collections::HashMap;

const SECTION_DEPTH: usize = 3;

struct Heading {
    start: usize,
    level: usize,
    title: String,
}

/// Headings (≤ H3) and the byte ranges of every depth-0 non-heading block.
fn scan(content: &str) -> (Vec<Heading>, Vec<(usize, usize)>) {
    let mut headings = Vec::new();
    let mut blocks = Vec::new();
    let mut iter = Parser::new(content).into_offset_iter();
    let mut depth = 0usize;
    while let Some((event, range)) = iter.next() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let level = level as usize;
                let mut title = String::new();
                for (inner, _) in iter.by_ref() {
                    match inner {
                        Event::End(TagEnd::Heading(_)) => break,
                        Event::Text(t) | Event::Code(t) => title.push_str(&t),
                        _ => {}
                    }
                }
                if level <= SECTION_DEPTH {
                    headings.push(Heading {
                        start: range.start,
                        level,
                        title: title.trim().to_string(),
                    });
                } else if depth == 0 {
                    blocks.push((range.start, range.end));
                }
            }
            Event::Start(_) => {
                if depth == 0 {
                    blocks.push((range.start, range.end));
                }
                depth += 1;
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            Event::Rule | Event::Html(_) if depth == 0 => blocks.push((range.start, range.end)),
            _ => {}
        }
    }
    (headings, blocks)
}

/// 1-based line of byte offset `at`. Counts on bytes, so `at` may fall inside
/// a multi-byte char (callers pass `end - 1`).
fn line_of(content: &str, at: usize) -> u32 {
    content.as_bytes()[..at.min(content.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count() as u32
        + 1
}

/// Extract heading sections from a Markdown/MDX file.
pub fn extract(file: &FileInput) -> FileExtraction {
    let mut out = file_level(file);
    let content = file.content.as_str();
    let (headings, blocks) = scan(content);
    if headings.is_empty() {
        return out;
    }
    out.free_ranges.clear();
    let preamble_end = content[..headings[0].start].trim_end().len();
    if !content[..preamble_end].trim().is_empty() {
        out.free_ranges.push((0, preamble_end));
    }
    let mut ids: HashMap<String, usize> = HashMap::new();
    // (level, symbol index) stack of enclosing sections.
    let mut path: Vec<(usize, usize)> = Vec::new();
    for (i, h) in headings.iter().enumerate() {
        let raw_end = headings
            .get(i + 1)
            .map(|n| n.start)
            .unwrap_or(content.len());
        let end = content[..raw_end].trim_end().len().max(h.start);
        path.retain(|(l, _)| *l < h.level);
        let parent = path
            .last()
            .map(|(_, idx)| out.symbols[*idx].stable_id.clone());
        let qualified = path
            .iter()
            .map(|(_, idx)| out.symbols[*idx].name.as_str())
            .chain(std::iter::once(h.title.as_str()))
            .collect::<Vec<_>>()
            .join(" > ");
        let base = format!("markdown:{}:section:{qualified}", file.path);
        let n = ids.entry(base.clone()).or_insert(0);
        *n += 1;
        let stable_id = if *n == 1 { base } else { format!("{base}#{n}") };
        let heading_line = content[h.start..end].lines().next().unwrap_or("").trim();
        let segments = blocks
            .iter()
            .filter(|(s, _)| *s >= h.start && *s < end)
            .map(|(s, e)| (*s, content[..(*e).min(end)].trim_end().len().max(*s)))
            .collect();
        out.symbols.push(Symbol {
            stable_id,
            kind: NodeKind::Section,
            name: h.title.clone(),
            qualified_name: qualified,
            parent,
            signature: Some(heading_line.chars().take(200).collect()),
            doc: None,
            line_start: line_of(content, h.start),
            line_end: line_of(content, end.saturating_sub(1).max(h.start)),
            byte_start: h.start,
            byte_end: end,
            segments,
            metadata: serde_json::json!({ "level": h.level }),
        });
        path.push((h.level, out.symbols.len() - 1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::Language;

    fn md(src: &str) -> FileExtraction {
        extract(&FileInput {
            path: "docs/guide.md".into(),
            language: Language::Markdown,
            content: src.into(),
        })
    }

    const DOC: &str = "Intro paragraph.\n\nSecond line.\n\n# Guide\n\nWelcome.\n\n## Install\n\nRun `cargo build`.\n\n- a\n- b\n\n### Linux\n\napt.\n\n#### Debian\n\ndeb.\n\n## Usage\n\nUse it.\n\n# Appendix\n\nEnd.\n";

    #[test]
    fn heading_nesting_and_preamble() {
        let x = md(DOC);
        let q: Vec<&str> = x
            .symbols
            .iter()
            .map(|s| s.qualified_name.as_str())
            .collect();
        assert_eq!(
            q,
            [
                "Guide",
                "Guide > Install",
                "Guide > Install > Linux",
                "Guide > Usage",
                "Appendix"
            ]
        );
        assert_eq!(
            x.free_ranges,
            [(0, "Intro paragraph.\n\nSecond line.".len())]
        );
        let install = &x.symbols[1];
        assert_eq!(
            install.stable_id,
            "markdown:docs/guide.md:section:Guide > Install"
        );
        assert_eq!(
            install.parent.as_deref(),
            Some("markdown:docs/guide.md:section:Guide")
        );
        assert_eq!(install.line_start, 9);
        assert_eq!(
            &DOC[install.byte_start..install.byte_end],
            "## Install\n\nRun `cargo build`.\n\n- a\n- b"
        );
        assert_eq!(install.segments, [(63, 81), (83, 90)], "paragraph + list");
        let linux = &x.symbols[2];
        assert!(DOC[linux.byte_start..linux.byte_end].contains("#### Debian"));
        assert_eq!(linux.parent.as_deref(), Some(install.stable_id.as_str()));
        assert_eq!(
            x.symbols[3].parent.as_deref(),
            Some("markdown:docs/guide.md:section:Guide")
        );
        assert_eq!(x.symbols[4].parent, None);
        assert_eq!(x.symbols[4].line_end, 30);
        assert!(x.symbols.iter().all(|s| s.kind == NodeKind::Section));
    }

    #[test]
    fn no_headings_means_no_symbols() {
        let x = md("just text\n\nmore text\n");
        assert!(x.symbols.is_empty());
        assert_eq!(x.free_ranges, [(0, "just text\n\nmore text\n".len())]);
        assert_eq!(x.language, Some(Language::Markdown));
    }

    #[test]
    fn section_ending_in_multibyte_char() {
        let x = md("# A\n\nversion —\n\n# B\n\nend —");
        assert_eq!(x.symbols.len(), 2);
        assert_eq!((x.symbols[0].line_start, x.symbols[0].line_end), (1, 3));
        assert_eq!((x.symbols[1].line_start, x.symbols[1].line_end), (5, 7));
    }

    #[test]
    fn duplicate_titles_get_suffixes() {
        let x = md("# A\n\n## Notes\n\n# B\n\n## Notes\n\n# A\n");
        let ids: Vec<&str> = x.symbols.iter().map(|s| s.stable_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "markdown:docs/guide.md:section:A",
                "markdown:docs/guide.md:section:A > Notes",
                "markdown:docs/guide.md:section:B",
                "markdown:docs/guide.md:section:B > Notes",
                "markdown:docs/guide.md:section:A#2",
            ]
        );
    }
}
