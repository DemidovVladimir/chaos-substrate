//! Markdown / MDX extraction (pulldown-cmark, not tree-sitter).
//!
//! Every H1–H3 heading opens a `Section` symbol that runs until the next
//! heading of depth ≤ 3 (deeper headings stay inside their section).
//! `qualified_name` is the heading path joined by `" > "`, `parent` is the
//! enclosing shallower section, segments are the section's top-level blocks
//! (paragraphs, lists, code blocks). The preamble before the first heading
//! becomes a free range (→ the file_head chunk); a file without headings has
//! no symbols at all, so the file_head chunk covers it.
//!
//! Transcripts (see `transcript.rs`): a heading that opens with a timestamp
//! (`## 12:30 Setup`) records `timestamp`, `seconds` and `label`; a section —
//! or a heading-less file — that is mostly timestamped lines keeps its heading
//! and intro, and its lines become time-segment sections under it.

use super::transcript::{self, line_of, Parent};
use super::walker::file_level;
use crate::domain::model::NodeKind;
use crate::ports::extractor::{FileExtraction, FileInput, Symbol};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::collections::HashMap;

const SECTION_DEPTH: usize = 3;

struct Heading {
    start: usize,
    level: usize,
    title: String,
}

/// `title:` of a YAML front-matter block (top-level key, quotes stripped).
fn front_matter_title(yaml: &str) -> Option<String> {
    yaml.lines().find_map(|l| {
        let v = l.strip_prefix("title:")?.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|x| x.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')))
            .unwrap_or(v)
            .trim();
        (!v.is_empty()).then(|| v.to_string())
    })
}

/// Headings (≤ H3), the byte ranges of every depth-0 non-heading block, and
/// the front matter's `title:`. A YAML front-matter block (`---` … `---` at
/// the top) is one block, never headings: its `title: x` line followed by
/// `---` is not a setext heading and its `# comments` are not H1s.
fn scan(content: &str) -> (Vec<Heading>, Vec<(usize, usize)>, Option<String>) {
    let mut headings = Vec::new();
    let mut blocks = Vec::new();
    let mut title = None;
    let mut iter =
        Parser::new_ext(content, Options::ENABLE_YAML_STYLE_METADATA_BLOCKS).into_offset_iter();
    let mut depth = 0usize;
    while let Some((event, range)) = iter.next() {
        match event {
            Event::Start(Tag::MetadataBlock(_)) => {
                let mut yaml = String::new();
                for (inner, _) in iter.by_ref() {
                    match inner {
                        Event::End(TagEnd::MetadataBlock(_)) => break,
                        Event::Text(t) => yaml.push_str(&t),
                        _ => {}
                    }
                }
                title = front_matter_title(&yaml);
                if depth == 0 {
                    blocks.push((range.start, range.end));
                }
            }
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
    (headings, blocks, title)
}

/// Extract heading sections from a Markdown/MDX file.
pub fn extract(file: &FileInput) -> FileExtraction {
    let mut out = file_level(file);
    let content = file.content.as_str();
    let (headings, blocks, title) = scan(content);
    out.title = title;
    let mut ids: HashMap<String, usize> = HashMap::new();
    if headings.is_empty() {
        let cues = transcript::line_cues(content, 0, content.len());
        if transcript::is_transcript(content, 0, content.len(), &cues) {
            out.free_ranges.clear();
            let pre = content[..cues[0].start].trim_end().len();
            if !content[..pre].trim().is_empty() {
                out.free_ranges.push((0, pre));
            }
            out.symbols = transcript::segment_symbols(
                "markdown",
                &file.path,
                content,
                &cues,
                None,
                content.len(),
                &mut ids,
            );
        }
        return out;
    }
    out.free_ranges.clear();
    let preamble_end = content[..headings[0].start].trim_end().len();
    if !content[..preamble_end].trim().is_empty() {
        out.free_ranges.push((0, preamble_end));
    }
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
            metadata: heading_metadata(h),
        });
        path.push((h.level, out.symbols.len() - 1));
    }

    // Transcript sections: the heading and intro stay, cue lines become segments.
    for mut s in std::mem::take(&mut out.symbols) {
        let body = content[s.byte_start..s.byte_end]
            .find('\n')
            .map(|k| s.byte_start + k + 1)
            .unwrap_or(s.byte_end);
        let cues = transcript::line_cues(content, body, s.byte_end);
        if !transcript::is_transcript(content, body, s.byte_end, &cues) {
            out.symbols.push(s);
            continue;
        }
        let level = s.metadata["level"].as_u64().unwrap_or(1) as usize;
        let segments = transcript::segment_symbols(
            "markdown",
            &file.path,
            content,
            &cues,
            Some(Parent {
                stable_id: &s.stable_id,
                qualified: &s.qualified_name,
                level,
            }),
            s.byte_end,
            &mut ids,
        );
        let cut = content[..cues[0].start]
            .trim_end()
            .len()
            .max(content[..body].trim_end().len())
            .max(s.byte_start);
        s.byte_end = cut;
        s.line_end = line_of(content, cut.saturating_sub(1).max(s.byte_start));
        s.segments = s
            .segments
            .into_iter()
            .filter(|(a, _)| *a < cut)
            .map(|(a, e)| (a, e.min(cut)))
            .collect();
        out.symbols.push(s);
        out.symbols.extend(segments);
    }
    out
}

/// `level`, plus `timestamp` / `seconds` / `label` for a heading that opens
/// with a timestamp (a chapter mark).
fn heading_metadata(h: &Heading) -> serde_json::Value {
    let mut m = serde_json::json!({ "level": h.level });
    if let Some((seconds, label)) = transcript::heading_timestamp(&h.title) {
        m["timestamp"] = serde_json::json!(transcript::format_timestamp(seconds));
        m["seconds"] = serde_json::json!(seconds);
        m["label"] = serde_json::json!(label);
    }
    m
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

    #[test]
    fn timestamped_headings_are_chapter_marks() {
        let x = md("# Talk\n\n## 00:00 Intro\nhello\n\n## 12:30 – Setup\nsteps\n");
        let marks: Vec<(&str, &serde_json::Value, &serde_json::Value)> = x
            .symbols
            .iter()
            .map(|s| {
                (
                    s.name.as_str(),
                    &s.metadata["timestamp"],
                    &s.metadata["label"],
                )
            })
            .collect();
        assert_eq!(marks[1].1, "00:00");
        assert_eq!(marks[2].1, "12:30");
        assert_eq!(marks[2].2, "Setup");
        assert!(marks[0].1.is_null());
    }

    #[test]
    fn transcript_sections_split_into_time_segments() {
        let mut src = String::from("# Keynote\nRecorded live.\n\n");
        for i in 0..9u32 {
            src.push_str(&format!("[00:{:02}:00] point {i}\n", i));
        }
        src.push_str("\n## Q&A\nquestions\n");
        let x = md(&src);
        let rows: Vec<(&str, &str, Option<&str>)> = x
            .symbols
            .iter()
            .map(|s| {
                (
                    s.name.as_str(),
                    s.qualified_name.as_str(),
                    s.parent.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("Keynote", "Keynote", None),
                (
                    "00:00",
                    "Keynote > 00:00",
                    Some("markdown:docs/guide.md:section:Keynote")
                ),
                (
                    "02:00",
                    "Keynote > 02:00",
                    Some("markdown:docs/guide.md:section:Keynote")
                ),
                (
                    "04:00",
                    "Keynote > 04:00",
                    Some("markdown:docs/guide.md:section:Keynote")
                ),
                (
                    "06:00",
                    "Keynote > 06:00",
                    Some("markdown:docs/guide.md:section:Keynote")
                ),
                (
                    "08:00",
                    "Keynote > 08:00",
                    Some("markdown:docs/guide.md:section:Keynote")
                ),
                (
                    "Q&A",
                    "Keynote > Q&A",
                    Some("markdown:docs/guide.md:section:Keynote")
                ),
            ]
        );
        let head = &x.symbols[0];
        assert_eq!(
            &src[head.byte_start..head.byte_end],
            "# Keynote\nRecorded live."
        );
        let seg = &x.symbols[1];
        assert_eq!(seg.metadata["label"], "point 0 point 1");
        assert_eq!(seg.metadata["level"], 2);
        assert_eq!(seg.line_start, 4);
        // Nothing overlaps: every byte belongs to one section at most.
        assert!(x
            .symbols
            .windows(2)
            .all(|w| w[0].byte_end <= w[1].byte_start));
    }

    #[test]
    fn heading_less_transcript_gets_segments() {
        let src = "Source: a video\n\n00:00 hi\n00:30 one\n01:00 two\n02:10 three\n02:40 four\n";
        let x = md(src);
        let names: Vec<&str> = x.symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["00:00", "02:10"]);
        assert_eq!(x.free_ranges, vec![(0, 15)]);
        assert_eq!(
            x.symbols[0].stable_id,
            "markdown:docs/guide.md:section:00:00"
        );
    }

    #[test]
    fn front_matter_is_one_block_and_gives_the_title() {
        let src = "---\ntitle: \"Why Molecule?\"\ndescription: Mission\n# a yaml comment\n---\n\nIntro.\n\n## Vision\ntext\n";
        let x = md(src);
        assert_eq!(x.title.as_deref(), Some("Why Molecule?"));
        let names: Vec<&str> = x.symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Vision"]);
        assert_eq!(
            &src[x.free_ranges[0].0..x.free_ranges[0].1],
            &src[..src.find("\n\n## Vision").unwrap()]
        );
        assert_eq!(md("# T\ntext").title, None);
    }
}
