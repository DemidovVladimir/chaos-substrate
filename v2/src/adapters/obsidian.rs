//! The vault: a folder of Markdown notes, as Obsidian keeps them. Notes are
//! parsed with pulldown-cmark into blocks; the folder is only ever written by
//! [`Vault::append`], atomically.

use crate::domain::note::{Block, Note};
use crate::domain::path::NotePath;
use crate::ports::Vault;
use anyhow::{Context, Result, anyhow, ensure};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::fs;
use std::io::{ErrorKind, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Notes larger than this are skipped (a knowledge note is never this big).
pub const MAX_NOTE_BYTES: u64 = 4 * 1024 * 1024;

/// The Markdown dialect: Obsidian's tables, tasks, front matter and wikilinks.
const OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_TASKLISTS)
    .union(Options::ENABLE_YAML_STYLE_METADATA_BLOCKS)
    .union(Options::ENABLE_WIKILINKS);

pub struct ObsidianVault {
    root: PathBuf,
}

impl ObsidianVault {
    /// A vault rooted at an existing folder.
    pub fn new(root: &Path) -> Result<ObsidianVault> {
        let root = fs::canonicalize(root)
            .with_context(|| format!("vault folder {} not found", root.display()))?;
        ensure!(root.is_dir(), "vault {} is not a folder", root.display());
        Ok(ObsidianVault { root })
    }

    /// The file of `note`, its missing folders created. Refuses folders that
    /// are symlinks (the walk never follows them) and a folder that resolves
    /// outside the vault.
    fn file(&self, note: &NotePath) -> Result<PathBuf> {
        for folder in note.folders() {
            let dir = self.root.join(folder);
            match fs::symlink_metadata(&dir) {
                Ok(meta) => ensure!(
                    meta.is_dir(),
                    "refusing to write {note}: {folder} is a symlink or not a folder"
                ),
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    fs::create_dir(&dir).with_context(|| format!("creating folder {folder}"))?
                }
                Err(e) => return Err(e).with_context(|| format!("checking folder {folder}")),
            }
        }
        let file = self.root.join(note.as_str());
        let name = file.file_name().context("note path has no file name")?;
        let parent = fs::canonicalize(file.parent().context("note path has no folder")?)?;
        ensure!(
            parent.starts_with(&self.root),
            "refusing to write {note}: its folder resolves outside the vault"
        );
        Ok(parent.join(name))
    }
}

/// The visible Markdown files under `dir`, whose vault-relative path is
/// `prefix`. Hidden names and symlinks are skipped, as are names
/// [`NotePath::parse`] rejects or would change (`notes.txt` would become
/// `notes.txt.md`, so it is no note).
fn walk(dir: &Path, prefix: &str, found: &mut Vec<(NotePath, PathBuf)>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|n| !n.starts_with('.')) else {
            continue;
        };
        let rel = match prefix {
            "" => name.to_string(),
            _ => format!("{prefix}/{name}"),
        };
        // `DirEntry::file_type` does not follow symlinks; a symlink is neither.
        let kind = entry.file_type()?;
        if kind.is_dir() {
            walk(&entry.path(), &rel, found)?;
        } else if kind.is_file()
            && entry.metadata()?.len() <= MAX_NOTE_BYTES
            && let Ok(path) = NotePath::parse(&rel)
            && path.as_str() == rel
        {
            found.push((path, entry.path()));
        }
    }
    Ok(())
}

/// The file's text; invalid UTF-8 is replaced, never fatal.
fn read_lossy(file: &Path) -> Result<String> {
    let bytes = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    Ok(String::from_utf8(bytes)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

/// Parse one note's Markdown into blocks (pure; no file access).
pub fn blocks(markdown: &str) -> Vec<Block> {
    parse(markdown).done
}

/// Run pulldown-cmark over one note; offsets are those of `markdown`.
fn parse(markdown: &str) -> Blocks<'_> {
    // A byte order mark hides front matter from pulldown-cmark. It holds no
    // newline, so line numbers are the same without it.
    let body = markdown.strip_prefix('\u{feff}').unwrap_or(markdown);
    let skipped = markdown.len() - body.len();
    let mut blocks = Blocks::new(markdown);
    for (event, range) in Parser::new_ext(body, OPTIONS).into_offset_iter() {
        blocks.event(event, range.start + skipped..range.end + skipped);
    }
    blocks
}

/// How the text of an open block is gathered.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// A heading of this level: it sets the heading path and is no block.
    Heading(usize),
    /// Code or front matter: the text is kept verbatim.
    Verbatim,
    /// A list item: it ends where its first nested list begins.
    Item,
    /// Paragraphs, quotes, tables, HTML: plain text, one line per part.
    Prose,
}

/// The block being gathered.
struct Open {
    kind: Kind,
    start: usize,
    /// Nesting depth of its start tag; the end tag at this depth closes it.
    depth: usize,
    text: String,
    /// The link being read: where its label starts in `text`, and where it
    /// points.
    link: Option<(usize, String)>,
}

/// Turns pulldown-cmark events into blocks.
struct Blocks<'a> {
    markdown: &'a str,
    /// Byte offset where each line starts.
    line_starts: Vec<usize>,
    /// `(level, text)` of the headings above, outermost first.
    headings: Vec<(usize, String)>,
    /// `(offset, level, text)` of every heading, in document order.
    outline: Vec<(usize, usize, String)>,
    open: Option<Open>,
    depth: usize,
    /// Inside an HTML tag: `<` seen, `>` not yet.
    in_tag: bool,
    done: Vec<Block>,
}

impl<'a> Blocks<'a> {
    fn new(markdown: &'a str) -> Blocks<'a> {
        // A line ends at `\n`, at `\r\n`, or at a lone `\r`, as in CommonMark.
        let ends = markdown
            .match_indices(['\n', '\r'])
            .filter(|&(i, end)| end == "\n" || !markdown[i + 1..].starts_with('\n'));
        Blocks {
            markdown,
            line_starts: std::iter::once(0).chain(ends.map(|(i, _)| i + 1)).collect(),
            headings: Vec::new(),
            outline: Vec::new(),
            open: None,
            depth: 0,
            in_tag: false,
            done: Vec::new(),
        }
    }

    fn event(&mut self, event: Event<'_>, range: Range<usize>) {
        match event {
            Event::Start(tag) => {
                self.start(&tag, range.start);
                self.depth += 1;
            }
            Event::End(tag) => {
                self.depth -= 1;
                if tag == TagEnd::Link {
                    self.end_link();
                }
                if self.open.as_ref().is_some_and(|o| o.depth == self.depth) {
                    self.close(range.end);
                }
            }
            Event::Text(text) | Event::Code(text) => self.push(&text),
            Event::Html(html) => self.push_html(&html),
            Event::SoftBreak | Event::HardBreak => self.push(" "),
            // Task markers, rules and inline HTML tags carry no text.
            _ => {}
        }
    }

    /// Open a block at a top-level tag, or mark a part inside the open one.
    fn start(&mut self, tag: &Tag<'_>, offset: usize) {
        let Some(open) = &mut self.open else {
            let kind = match tag {
                Tag::Heading { level, .. } => Kind::Heading(*level as usize),
                Tag::CodeBlock(_) | Tag::MetadataBlock(_) => Kind::Verbatim,
                Tag::Item => Kind::Item,
                Tag::Paragraph | Tag::BlockQuote(_) | Tag::Table(_) | Tag::HtmlBlock => Kind::Prose,
                _ => return,
            };
            self.open = Some(Open {
                kind,
                start: offset,
                depth: self.depth,
                text: String::new(),
                link: None,
            });
            return;
        };
        match tag {
            Tag::List(_) if open.kind == Kind::Item => self.close(offset),
            Tag::Paragraph
            | Tag::Heading { .. }
            | Tag::CodeBlock(_)
            | Tag::HtmlBlock
            | Tag::Item
            | Tag::TableHead
            | Tag::TableRow => open.text.push('\n'),
            Tag::TableCell if !open.text.is_empty() && !open.text.ends_with('\n') => {
                open.text.push_str(" | ")
            }
            // A heading keeps link labels only: it names a section.
            Tag::Link { dest_url, .. } if !matches!(open.kind, Kind::Heading(_)) => {
                open.link = Some((open.text.len(), dest_url.to_string()))
            }
            _ => {}
        }
    }

    /// After a link's label, where it points, so a new target is new text;
    /// said once when the label already says it (`<https://x.org>`,
    /// `[[Car service]]`).
    fn end_link(&mut self) {
        let Some(open) = &mut self.open else { return };
        let Some((label, target)) = open.link.take() else {
            return;
        };
        if target.is_empty() || open.text[label..] == target {
            return;
        }
        if open.text.len() > label {
            open.text.push(' ');
        }
        open.text.push_str(&target);
    }

    fn push(&mut self, text: &str) {
        if let Some(open) = &mut self.open {
            open.text.push_str(text);
        }
    }

    /// HTML block lines, their tags removed.
    fn push_html(&mut self, html: &str) {
        let Some(open) = &mut self.open else { return };
        for c in html.chars() {
            match c {
                '<' => self.in_tag = true,
                '>' if self.in_tag => self.in_tag = false,
                _ if !self.in_tag => open.text.push(c),
                _ => {}
            }
        }
    }

    /// Close the open block at byte `end`: a heading updates the path, any
    /// other block with text is kept.
    fn close(&mut self, end: usize) {
        let Some(open) = self.open.take() else { return };
        let text = match open.kind {
            Kind::Verbatim => open.text.trim_end().to_string(),
            _ => prose(&open.text),
        };
        if let Kind::Heading(level) = open.kind {
            self.outline.push((open.start, level, text.clone()));
            self.headings.retain(|(above, _)| *above < level);
            if !text.is_empty() {
                self.headings.push((level, text));
            }
            return;
        }
        if text.is_empty() {
            return;
        }
        let source = self.markdown[open.start..end].trim_end();
        let last = open.start + source.len().saturating_sub(1);
        self.done.push(Block {
            headings: self.headings.iter().map(|(_, h)| h.clone()).collect(),
            text,
            source: source.to_string(),
            lines: [self.line(open.start), self.line(last)],
        });
    }

    /// 1-based line of a byte offset.
    fn line(&self, offset: usize) -> u32 {
        self.line_starts.partition_point(|&start| start <= offset) as u32
    }
}

/// Every line trimmed, blank lines dropped.
fn prose(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    lines.join("\n")
}

/// `existing` with `text` placed as [`Vault::append`] describes (pure): at the
/// end of the section under the first heading titled `heading` (any case, the
/// heading read as [`blocks`] reads it), under a new `## heading` at the end
/// when there is no such heading, or at the end of the note. One blank line
/// separates the text from its neighbours; the existing bytes are kept as
/// they are.
fn place(existing: &str, heading: Option<&str>, text: &str) -> String {
    let text = text.trim_end().trim_start_matches(['\r', '\n']);
    let Some(heading) = heading.map(str::trim).filter(|h| !h.is_empty()) else {
        return format!("{existing}{}{text}\n", gap(existing));
    };
    match section_end(existing, heading) {
        Some(end) => {
            let (before, after) = existing.split_at(end);
            let blank = if after.is_empty() { "" } else { "\n" };
            format!("{before}{}{text}\n{blank}{after}", gap(before))
        }
        None => format!("{existing}{}## {heading}\n\n{text}\n", gap(existing)),
    }
}

/// The newlines that put one blank line after `before` (none at the start of
/// a note, and none added when `before` already ends with a blank line).
fn gap(before: &str) -> &'static str {
    let body = before.trim_end();
    if body.is_empty() {
        return "";
    }
    match before[body.len()..].matches('\n').count() {
        0 => "\n\n",
        1 => "\n",
        _ => "",
    }
}

/// Where the section under the first heading titled `title` ends: where the
/// line of the next heading of the same or a higher level starts, or at the
/// end of the note.
fn section_end(markdown: &str, title: &str) -> Option<usize> {
    let title = title.to_lowercase();
    let outline = parse(markdown).outline;
    let at = outline.iter().position(|h| h.2.to_lowercase() == title)?;
    let level = outline[at].1;
    let Some(&(next, ..)) = outline[at + 1..].iter().find(|h| h.1 <= level) else {
        return Some(markdown.len());
    };
    // An indented heading starts after its indentation.
    Some(markdown[..next].rfind(['\n', '\r']).map_or(0, |i| i + 1))
}

/// Write `bytes` to `file` through a hidden temp file in the same folder and a
/// rename, so readers see the old note or the new one, never half of one.
fn write_atomic(file: &Path, bytes: &[u8], permissions: Option<fs::Permissions>) -> Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = file.with_file_name(format!(".{name}.{}-{n}.tmp", std::process::id()));
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .with_context(|| format!("creating {}", temp.display()))?;
    let written = (|| {
        out.write_all(bytes)?;
        if let Some(permissions) = permissions {
            out.set_permissions(permissions)?;
        }
        out.sync_all()?;
        fs::rename(&temp, file)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written.with_context(|| format!("writing {}", file.display()))
}

impl Vault for ObsidianVault {
    fn notes(&self) -> Result<Vec<Note>> {
        let mut files = Vec::new();
        walk(&self.root, "", &mut files)?;
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
            .into_iter()
            .map(|(path, file)| {
                let blocks = blocks(&read_lossy(&file)?);
                Ok(Note { path, blocks })
            })
            .collect()
    }

    fn texts(&self, markdown: &str) -> Vec<String> {
        blocks(markdown).into_iter().map(|b| b.text).collect()
    }

    fn append(&self, note: &NotePath, heading: Option<&str>, text: &str) -> Result<()> {
        ensure!(
            !text.trim().is_empty(),
            "nothing to append: the text is empty"
        );
        ensure!(
            !heading.is_some_and(|h| h.contains(['\n', '\r'])),
            "a heading must be one line"
        );
        let file = self.file(note)?;
        let (existing, permissions) = match fs::symlink_metadata(&file) {
            Ok(meta) => {
                ensure!(
                    meta.is_file(),
                    "refusing to write {note}: it is a symlink or not a file"
                );
                let bytes = fs::read(&file).with_context(|| format!("reading {note}"))?;
                let text = String::from_utf8(bytes)
                    .map_err(|_| anyhow!("refusing to rewrite {note}: it is not valid UTF-8"))?;
                (text, Some(meta.permissions()))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => (String::new(), None),
            Err(e) => return Err(e).with_context(|| format!("reading {note}")),
        };
        write_atomic(
            &file,
            place(&existing, heading, text).as_bytes(),
            permissions,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::note::{Block, section};
    use std::fs;
    use tempfile::tempdir;

    fn doc(markdown: &str) -> Note {
        Note {
            path: NotePath::parse("test.md").unwrap(),
            blocks: blocks(markdown),
        }
    }

    fn texts(note: &Note) -> Vec<&str> {
        note.blocks.iter().map(|b| b.text.as_str()).collect()
    }

    /// `(text, section)` of every block.
    fn outline(note: &Note) -> Vec<(&str, String)> {
        note.blocks
            .iter()
            .map(|b| (b.text.as_str(), section(&b.headings)))
            .collect()
    }

    /// `(source, lines)` of every block.
    fn spans(note: &Note) -> Vec<(&str, [u32; 2])> {
        note.blocks
            .iter()
            .map(|b| (b.source.as_str(), b.lines))
            .collect()
    }

    // ---- parse -----------------------------------------------------------

    #[test]
    fn headings_nest_and_reset() {
        let note = doc(
            "above\n\n# Car\n\nintro\n\n## Oil\n\noil text\n\n### Brand\n\nbrand text\n\n\
             ## Tyres\n\ntyre text\n\n# House\n\nhouse text\n",
        );
        assert_eq!(
            outline(&note),
            [
                ("above", String::new()),
                ("intro", "Car".into()),
                ("oil text", "Car > Oil".into()),
                ("brand text", "Car > Oil > Brand".into()),
                ("tyre text", "Car > Tyres".into()),
                ("house text", "House".into()),
            ]
        );
    }

    #[test]
    fn a_heading_pops_to_its_own_level_when_levels_are_skipped() {
        let note = doc("# A\n\n### B\n\nb\n\n### C\n\nc\n\n## D\n\nd\n");
        assert_eq!(
            outline(&note),
            [
                ("b", "A > B".to_string()),
                ("c", "A > C".into()),
                ("d", "A > D".into()),
            ]
        );
    }

    #[test]
    fn heading_text_is_plain() {
        let note = doc("## **Bold** `code` [[Target|alias]]\n\ntext\n\nSetext\n======\n\nmore\n");
        assert_eq!(
            outline(&note),
            [
                ("text", "Bold code alias".to_string()),
                ("more", "Setext".into()),
            ]
        );
    }

    #[test]
    fn list_items_are_blocks_without_their_nested_items() {
        let note = doc(
            "- parent item\n  - child item\n    more child\n  - second child\n\
             - [ ] task item\n- [x] done **bold**\n",
        );
        assert_eq!(
            texts(&note),
            [
                "parent item",
                "child item more child",
                "second child",
                "task item",
                "done bold",
            ]
        );
        assert_eq!(
            spans(&note),
            [
                ("- parent item", [1, 1]),
                ("- child item\n    more child", [2, 3]),
                ("- second child", [4, 4]),
                ("- [ ] task item", [5, 5]),
                ("- [x] done **bold**", [6, 6]),
            ]
        );
    }

    #[test]
    fn a_loose_item_keeps_its_paragraphs_and_text_after_a_nested_list_is_kept() {
        let note =
            doc("1. first\n\n   more of first\n2. second\n\n   - nested\n\n   after nested\n");
        assert_eq!(
            texts(&note),
            ["first\nmore of first", "second", "nested", "after nested"]
        );
        assert_eq!(
            spans(&note),
            [
                ("1. first\n\n   more of first", [1, 3]),
                ("2. second", [4, 4]),
                ("- nested", [6, 6]),
                ("after nested", [8, 8]),
            ]
        );
    }

    #[test]
    fn a_hash_inside_a_code_fence_is_code_not_a_heading() {
        let note =
            doc("## Setup\n\n```sh\n# install\nmake\n```\n\nafter\n\n    indented\n    code\n");
        assert_eq!(
            outline(&note),
            [
                ("# install\nmake", "Setup".to_string()),
                ("after", "Setup".into()),
                ("indented\ncode", "Setup".into()),
            ]
        );
        assert_eq!(spans(&note)[0], ("```sh\n# install\nmake\n```", [3, 6]));
        assert_eq!(note.blocks[2].lines, [10, 11]);
    }

    #[test]
    fn front_matter_is_one_block() {
        let note = doc("---\ntitle: Car\ntags: [a, b]\n---\n# Car\n\nbody\n");
        assert_eq!(
            note.blocks[0],
            Block {
                headings: vec![],
                text: "title: Car\ntags: [a, b]".into(),
                source: "---\ntitle: Car\ntags: [a, b]\n---".into(),
                lines: [1, 4],
            }
        );
        assert_eq!(outline(&note)[1], ("body", "Car".to_string()));
        assert_eq!(note.blocks[1].lines, [7, 7]);
    }

    #[test]
    fn front_matter_after_a_byte_order_mark() {
        let note = doc("\u{feff}---\nk: v\n---\nbody\n");
        assert_eq!(spans(&note), [("---\nk: v\n---", [1, 3]), ("body", [4, 4])]);
        assert_eq!(texts(&note), ["k: v", "body"]);
    }

    #[test]
    fn a_metadata_block_mid_note_is_an_ordinary_block() {
        let note = doc("# H\n\npara\n\n---\nk: v\n---\n");
        assert_eq!(
            outline(&note),
            [("para", "H".to_string()), ("k: v", "H".into())]
        );
        assert_eq!(spans(&note)[1], ("---\nk: v\n---", [5, 7]));
    }

    #[test]
    fn plain_text_drops_markup() {
        let note = doc(
            "See [[Oil change|the oil]] and [[Tyres]], ![[pic.png]], [site](https://x.y), \
             ![alt](a.png) and `cfg` ~~old~~ *em* <span>raw</span>.\nnext line  \nhard\\\nbreak\n",
        );
        assert_eq!(
            texts(&note),
            [
                "See the oil Oil change and Tyres, pic.png, site https://x.y, alt and cfg old em raw. \
                 next line hard break"
            ]
        );
    }

    #[test]
    fn a_link_reads_as_its_label_then_its_destination() {
        let note = doc("Call [Dr. Brown](tel:+15550001111) at [[Car service|the garage]].\n");
        assert_eq!(
            texts(&note),
            ["Call Dr. Brown tel:+15550001111 at the garage Car service."]
        );
        let edited = doc("Call [Dr. Brown](tel:+15550002222) at [[Car service|the garage]].\n");
        assert_ne!(texts(&edited), texts(&note), "a new target is new text");
    }

    #[test]
    fn a_link_whose_label_is_its_destination_reads_once() {
        let note = doc("See <https://x.org>, <a@b.co>, [[Car service]] and [](https://y.org).\n");
        assert_eq!(
            texts(&note),
            ["See https://x.org, a@b.co, Car service and https://y.org."]
        );
    }

    #[test]
    fn a_cyrillic_note() {
        let note = doc(
            "# Здоровье\n\n## Лекарства\n\n- Аспирин 100 мг\n  - утром\n\n\
             Принимать [[Врач|после еды]].\n",
        );
        assert_eq!(
            outline(&note),
            [
                ("Аспирин 100 мг", "Здоровье > Лекарства".to_string()),
                ("утром", "Здоровье > Лекарства".into()),
                ("Принимать после еды Врач.", "Здоровье > Лекарства".into()),
            ]
        );
        assert_eq!(
            spans(&note),
            [
                ("- Аспирин 100 мг", [5, 5]),
                ("- утром", [6, 6]),
                ("Принимать [[Врач|после еды]].", [8, 8]),
            ]
        );
    }

    #[test]
    fn a_table_is_one_block() {
        let table =
            "| Name | Dose |\n|------|------|\n| Aspirin | 100 mg |\n| Ibuprofen | 200 mg |";
        let note = doc(&format!("# T\n\n{table}\n\nafter\n"));
        assert_eq!(
            texts(&note),
            ["Name | Dose\nAspirin | 100 mg\nIbuprofen | 200 mg", "after"]
        );
        assert_eq!(spans(&note), [(table, [3, 6]), ("after", [8, 8])]);
    }

    #[test]
    fn a_blockquote_is_one_block_with_its_contents() {
        let quote = "> quote line\n> continues\n>\n> - quoted item\n> - another";
        let note = doc(&format!("# Q\n\n{quote}\n\nafter\n"));
        assert_eq!(
            outline(&note),
            [
                (
                    "quote line continues\nquoted item\nanother",
                    "Q".to_string()
                ),
                ("after", "Q".into()),
            ]
        );
        assert_eq!(spans(&note), [(quote, [3, 7]), ("after", [9, 9])]);
    }

    #[test]
    fn an_html_block_is_one_block_without_tags() {
        let note = doc("<div>\nhello <b>world</b>\n</div>\n\nafter\n");
        assert_eq!(texts(&note), ["hello world", "after"]);
        assert_eq!(
            spans(&note)[0],
            ("<div>\nhello <b>world</b>\n</div>", [1, 3])
        );
    }

    #[test]
    fn html_text_keeps_a_greater_than_sign() {
        assert_eq!(texts(&doc("<div>5 > 3</div>\n")), ["5 > 3"]);
    }

    #[test]
    fn code_keeps_its_indentation_and_blank_lines() {
        let note = doc("```rust\nfn a() {\n    x();\n\n}\n```\n");
        assert_eq!(texts(&note), ["fn a() {\n    x();\n\n}"]);
    }

    #[test]
    fn a_snippet_reads_like_a_note() {
        let dir = tempdir().unwrap();
        let texts = open(dir.path()).texts("- **Allergic** to *penicillin*.\n\n1. Vitamin D\n");
        assert_eq!(texts, ["Allergic to penicillin.", "Vitamin D"]);
    }

    #[test]
    fn empty_blocks_and_rules_are_not_blocks() {
        let note = doc("- \n- real\n\n---\n\n![](x.png)\n\nend\n");
        assert_eq!(texts(&note), ["real", "end"]);
    }

    #[test]
    fn crlf_lines_and_sources() {
        let note = doc("# A\r\n\r\npara one\r\nline two\r\n\r\n- item\r\n");
        assert_eq!(
            outline(&note),
            [("para one line two", "A".to_string()), ("item", "A".into())]
        );
        assert_eq!(
            spans(&note),
            [("para one\r\nline two", [3, 4]), ("- item", [6, 6])]
        );
    }

    #[test]
    fn lone_carriage_returns_end_lines() {
        let note = doc("first\r\rsecond\r\rthird\r");
        assert_eq!(
            spans(&note),
            [("first", [1, 1]), ("second", [3, 3]), ("third", [5, 5])]
        );
        let note = doc("one\rtwo\r\r- item\r");
        assert_eq!(texts(&note), ["one two", "item"]);
        assert_eq!(spans(&note), [("one\rtwo", [1, 2]), ("- item", [4, 4])]);
    }

    // ---- notes() ---------------------------------------------------------

    fn write(root: &Path, rel: &str, content: impl AsRef<[u8]>) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn open(root: &Path) -> Box<dyn Vault> {
        Box::new(ObsidianVault::new(root).unwrap())
    }

    fn paths(vault: &dyn Vault) -> Vec<String> {
        let notes = vault.notes().unwrap();
        notes.into_iter().map(|n| n.path.to_string()).collect()
    }

    fn note(raw: &str) -> NotePath {
        NotePath::parse(raw).unwrap()
    }

    #[test]
    fn new_needs_an_existing_folder() {
        let dir = tempdir().unwrap();
        assert!(ObsidianVault::new(&dir.path().join("missing")).is_err());
        write(dir.path(), "file.md", "x");
        assert!(ObsidianVault::new(&dir.path().join("file.md")).is_err());
        assert!(ObsidianVault::new(dir.path()).is_ok());
    }

    #[test]
    fn notes_are_the_visible_markdown_files_sorted_by_path() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "z.md", "zulu");
        write(root, "B/Deep/d.MD", "delta");
        write(root, "B/c.md", "# C\n\ngamma");
        write(root, "a.md", "alpha");
        for hidden in [
            ".obsidian/app.md",
            ".trash/old.md",
            ".chaos-kb/index.md",
            ".git/x.md",
            ".hidden.md",
            "B/.secret.md",
            "B/.drafts/y.md",
        ] {
            write(root, hidden, "hidden");
        }
        for other in ["notes.txt", "image.png", "README", "md", "B/data.json"] {
            write(root, other, "other");
        }
        write(root, "bad:name.md", "rejected by NotePath");
        write(root, " lead.md", "NotePath would rename it");

        let notes = open(root).notes().unwrap();
        let listed: Vec<_> = notes.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(listed, ["B/Deep/d.MD", "B/c.md", "a.md", "z.md"]);
        assert_eq!(outline(&notes[1]), [("gamma", "C".to_string())]);
        assert_eq!(texts(&notes[2]), ["alpha"]);
    }

    #[cfg(unix)]
    #[test]
    fn notes_never_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        write(outside.path(), "secret.md", "outside");
        write(dir.path(), "real.md", "real");
        write(dir.path(), "Folder/inner.md", "inner");
        symlink(dir.path().join("real.md"), dir.path().join("alias.md")).unwrap();
        symlink(dir.path().join("Folder"), dir.path().join("FolderLink")).unwrap();
        symlink(outside.path(), dir.path().join("Out")).unwrap();
        symlink(outside.path().join("secret.md"), dir.path().join("s.md")).unwrap();
        assert_eq!(
            paths(open(dir.path()).as_ref()),
            ["Folder/inner.md", "real.md"]
        );
    }

    #[test]
    fn notes_skip_files_over_the_size_limit() {
        let dir = tempdir().unwrap();
        let max = usize::try_from(MAX_NOTE_BYTES).unwrap();
        write(dir.path(), "max.md", "x".repeat(max));
        write(dir.path(), "over.md", "x".repeat(max + 1));
        assert_eq!(paths(open(dir.path()).as_ref()), ["max.md"]);
    }

    #[test]
    fn notes_read_invalid_utf8_lossily() {
        let dir = tempdir().unwrap();
        write(dir.path(), "bad.md", b"ok \xff here");
        let notes = open(dir.path()).notes().unwrap();
        assert_eq!(texts(&notes[0]), ["ok \u{fffd} here"]);
    }

    // ---- place -----------------------------------------------------------

    const CAR: &str = "# Car\n\n## Oil\n\nChange every 10k.\n\n### Brand\n\nCastrol.\n\n\
                       ## Tyres\n\nWinter set.\n";

    #[test]
    fn place_into_a_new_note() {
        assert_eq!(place("", None, "Fact."), "Fact.\n");
        assert_eq!(place("", Some("Oil"), "Fact."), "## Oil\n\nFact.\n");
        assert_eq!(place("", None, "\n\n  Fact.\n\n"), "  Fact.\n");
    }

    #[test]
    fn place_without_heading_goes_at_the_end_after_one_blank_line() {
        assert_eq!(place("intro", None, "new"), "intro\n\nnew\n");
        assert_eq!(place("intro\n", None, "new"), "intro\n\nnew\n");
        assert_eq!(place("intro\n\n", None, "new"), "intro\n\nnew\n");
        assert_eq!(place("intro\n\n\n", None, "new"), "intro\n\n\nnew\n");
        assert_eq!(place(CAR, None, "new"), format!("{CAR}\nnew\n"));
    }

    #[test]
    fn place_at_the_end_of_a_section_before_the_next_heading_of_its_level() {
        assert_eq!(
            place(CAR, Some("Oil"), "Check monthly."),
            "# Car\n\n## Oil\n\nChange every 10k.\n\n### Brand\n\nCastrol.\n\nCheck monthly.\n\n\
             ## Tyres\n\nWinter set.\n"
        );
        assert_eq!(
            place(CAR, Some("  bRaNd "), "Or Mobil."),
            "# Car\n\n## Oil\n\nChange every 10k.\n\n### Brand\n\nCastrol.\n\nOr Mobil.\n\n\
             ## Tyres\n\nWinter set.\n"
        );
        assert_eq!(
            place(CAR, Some("Tyres"), "Summer set."),
            format!("{CAR}\nSummer set.\n")
        );
        assert_eq!(place(CAR, Some("car"), "Sold."), format!("{CAR}\nSold.\n"));
        assert_eq!(
            place("## A\ntext\n## B\nb\n", Some("A"), "new"),
            "## A\ntext\n\nnew\n\n## B\nb\n"
        );
        assert_eq!(
            place("## A\n## B\n", Some("A"), "new"),
            "## A\n\nnew\n\n## B\n"
        );
        assert_eq!(
            place("## Oil ##\nx\n", Some("oil"), "y"),
            "## Oil ##\nx\n\ny\n"
        );
    }

    #[test]
    fn place_adds_a_missing_heading_at_the_end() {
        assert_eq!(
            place("# Car\n\nintro\n", Some("Oil"), "Fact."),
            "# Car\n\nintro\n\n## Oil\n\nFact.\n"
        );
        assert_eq!(
            place("# Car", Some(" Oil "), "Fact."),
            "# Car\n\n## Oil\n\nFact.\n"
        );
        assert_eq!(
            place("#Oil\n", Some("Oil"), "Fact."),
            "#Oil\n\n## Oil\n\nFact.\n",
            "`#Oil` is not a heading"
        );
        assert_eq!(
            place("    ## Oil\n", Some("Oil"), "Fact."),
            "    ## Oil\n\n## Oil\n\nFact.\n",
            "an indented `## Oil` is code"
        );
    }

    #[test]
    fn place_ignores_headings_inside_code_fences() {
        let fenced = "# Notes\n\n```md\n## Oil\ninside\n```\n\n~~~\n## Oil\n~~~\n\ntail\n";
        assert_eq!(
            place(fenced, Some("Oil"), "Fact."),
            format!("{fenced}\n## Oil\n\nFact.\n")
        );
        let long = "````\n```\n## Oil\n````\n## Oil\nreal\n";
        assert_eq!(
            place(long, Some("Oil"), "Fact."),
            format!("{long}\nFact.\n"),
            "three backticks do not close a four-backtick fence"
        );
        let hidden_next = "## A\n```\n## B\n```\n";
        assert_eq!(
            place(hidden_next, Some("A"), "new"),
            format!("{hidden_next}\nnew\n")
        );
    }

    #[test]
    fn place_closes_a_fence_only_with_a_bare_marker_of_its_kind_and_length() {
        // In each note the first `## A` sits inside a fence, so the fact goes
        // into the real `## A` section, before `## B`.
        for fence in [
            "````\n```\n## A\n````\n",    // a shorter marker does not close
            "```\n~~~\n## A\n```\n",      // another character does not close
            "```\n``` rust\n## A\n```\n", // a marker with text does not close
            "~~~ a`b\n## A\n~~~\n",       // a tilde fence may name backticks
        ] {
            let markdown = format!("{fence}## A\nreal\n## B\nend\n");
            let placed = place(&markdown, Some("A"), "new");
            let expected = format!("{fence}## A\nreal\n\nnew\n\n## B\nend\n");
            assert_eq!(placed, expected, "{fence:?}");
        }
        // Two marks are no fence, so the heading after them is found.
        let markdown = "~~x\n## A\nreal\n## B\n";
        assert_eq!(
            place(markdown, Some("A"), "new"),
            "~~x\n## A\nreal\n\nnew\n\n## B\n"
        );
    }

    #[test]
    fn place_finds_a_setext_heading() {
        assert_eq!(
            place("Oil\n===\n\nx\n\nTyres\n=====\n\ny\n", Some("oil"), "new"),
            "Oil\n===\n\nx\n\nnew\n\nTyres\n=====\n\ny\n"
        );
        assert_eq!(
            place("Oil\n===\n\nx\n\nBrand\n-----\n\ny\n", Some("oil"), "new"),
            "Oil\n===\n\nx\n\nBrand\n-----\n\ny\n\nnew\n",
            "a `-----` heading is level 2, inside the `===` section"
        );
    }

    #[test]
    fn place_matches_a_heading_by_its_plain_text() {
        assert_eq!(
            place("## **Oil**\n\nx\n\n## Tyres\n", Some("Oil"), "new"),
            "## **Oil**\n\nx\n\nnew\n\n## Tyres\n"
        );
    }

    #[test]
    fn place_ignores_a_hash_line_inside_front_matter() {
        let front = "---\ntitle: Car\n# Oil\n---\n\nbody\n";
        assert_eq!(
            place(front, Some("Oil"), "new"),
            format!("{front}\n## Oil\n\nnew\n")
        );
    }

    #[test]
    fn place_finds_a_heading_after_a_byte_order_mark() {
        assert_eq!(
            place("\u{feff}# Oil\n\nx\n\n# Tyres\n", Some("Oil"), "new"),
            "\u{feff}# Oil\n\nx\n\nnew\n\n# Tyres\n"
        );
    }

    #[test]
    fn place_ends_a_section_where_the_line_of_an_indented_heading_starts() {
        assert_eq!(
            place("## A\nx\n   ## B\n", Some("A"), "new"),
            "## A\nx\n\nnew\n\n   ## B\n"
        );
    }

    #[test]
    fn place_keeps_existing_bytes() {
        let existing = "# Здоровье  \r\n\r\n## Лекарства\t\r\n\r\n- Аспирин\r\n\r\n\r\n\
                        ## Врачи\r\nТерапевт   ";
        assert_eq!(
            place(existing, Some("лекарства"), "Витамин D"),
            "# Здоровье  \r\n\r\n## Лекарства\t\r\n\r\n- Аспирин\r\n\r\n\r\nВитамин D\n\n\
             ## Врачи\r\nТерапевт   "
        );
        for (heading, text) in [(None, "x"), (Some("ВРАЧИ"), "Хирург"), (Some("Новое"), "y")]
        {
            let placed = place(existing, heading, text);
            let (a, b) = (existing.as_bytes(), placed.as_bytes());
            let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
            let suffix = a.iter().rev().zip(b.iter().rev());
            let suffix = suffix.take_while(|(x, y)| x == y).count();
            assert!(
                prefix + suffix >= a.len() && placed.contains(text),
                "{heading:?}: {placed:?}"
            );
        }
    }

    // ---- append ------------------------------------------------------------

    /// Every entry under `root` whose name starts with `.` (temp files).
    fn dot_entries(root: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy().starts_with('.') {
                found.push(entry.path());
            } else if entry.file_type().unwrap().is_dir() {
                found.extend(dot_entries(&entry.path()));
            }
        }
        found
    }

    #[test]
    fn append_creates_the_note_and_its_folders_then_extends_the_section() {
        let dir = tempdir().unwrap();
        let vault = open(dir.path());
        let list = note("Health/Meds/List");
        vault
            .append(&list, Some("Daily"), "Aspirin 100 mg")
            .unwrap();
        vault.append(&list, Some("daily"), "Vitamin D").unwrap();
        vault.append(&list, None, "Ask the doctor.").unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("Health/Meds/List.md")).unwrap(),
            "## Daily\n\nAspirin 100 mg\n\nVitamin D\n\nAsk the doctor.\n"
        );
        let notes = vault.notes().unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(
            texts(&notes[0]),
            ["Aspirin 100 mg", "Vitamin D", "Ask the doctor."]
        );
        assert_eq!(dot_entries(dir.path()), Vec::<PathBuf>::new());
    }

    #[test]
    fn append_rejects_empty_text_and_multiline_headings() {
        let dir = tempdir().unwrap();
        let vault = open(dir.path());
        assert!(vault.append(&note("inbox"), None, " \n\t ").is_err());
        assert!(
            vault
                .append(&note("inbox"), Some("A\n# B"), "text")
                .is_err()
        );
        assert!(!dir.path().join("inbox.md").exists());
    }

    #[test]
    fn append_refuses_to_rewrite_a_note_that_is_not_utf8() {
        let dir = tempdir().unwrap();
        write(dir.path(), "bin.md", b"ok \xff");
        assert!(open(dir.path()).append(&note("bin"), None, "x").is_err());
        assert_eq!(fs::read(dir.path().join("bin.md")).unwrap(), b"ok \xff");
        assert_eq!(dot_entries(dir.path()), Vec::<PathBuf>::new());
    }

    #[cfg(unix)]
    #[test]
    fn append_refuses_a_symlinked_folder_that_leaves_the_vault() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        symlink(outside.path(), dir.path().join("Escape")).unwrap();
        let vault = open(dir.path());
        assert!(vault.append(&note("Escape/x"), None, "leak").is_err());
        assert!(vault.append(&note("Escape/Sub/x"), None, "leak").is_err());
        assert_eq!(
            fs::read_dir(outside.path()).unwrap().count(),
            0,
            "nothing may be created outside the vault"
        );
        assert_eq!(dot_entries(dir.path()), Vec::<PathBuf>::new());
    }

    #[cfg(unix)]
    #[test]
    fn append_refuses_a_symlinked_note() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        write(outside.path(), "target.md", "original");
        symlink(outside.path().join("target.md"), dir.path().join("link.md")).unwrap();
        assert!(open(dir.path()).append(&note("link"), None, "x").is_err());
        let target = fs::read_to_string(outside.path().join("target.md")).unwrap();
        assert_eq!(target, "original");
        let link = fs::symlink_metadata(dir.path().join("link.md")).unwrap();
        assert!(link.file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn append_refuses_a_note_it_cannot_read() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let file = dir.path().join("locked.md");
        write(dir.path(), "locked.md", "keep\n");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o200)).unwrap();
        let result = open(dir.path()).append(&note("locked"), None, "x");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep\n");
    }

    #[cfg(unix)]
    #[test]
    fn append_keeps_the_note_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let file = dir.path().join("private.md");
        write(dir.path(), "private.md", "secret\n");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        open(dir.path())
            .append(&note("private"), None, "more")
            .unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "secret\n\nmore\n");
        let mode = fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn a_vault_opened_through_a_symlink_writes_into_its_real_folder() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let links = tempdir().unwrap();
        symlink(dir.path(), links.path().join("vault")).unwrap();
        let vault = open(&links.path().join("vault"));
        vault.append(&note("A/b"), None, "fact").unwrap();
        let written = fs::read_to_string(dir.path().join("A/b.md")).unwrap();
        assert_eq!(written, "fact\n");
    }
}
