//! Transcripts: timestamped text (a video, a podcast, a recorded call) cut
//! into time segments. Each segment is a `Section` symbol named by its start
//! time (`12:30`, `1:02:03`), so a `feature` answer points at a moment and the
//! entry points list the timeline. Not tree-sitter: subtitles have no grammar,
//! so a line scanner reads them (as `markdown.rs` uses pulldown-cmark).
//!
//! - `.srt` / `.vtt` files (`Language::Transcript`): every cue's start time.
//! - Markdown (`markdown.rs`): lines that start with a timestamp —
//!   `[00:12:30] text`, `12:30 text`, `- (1:02:03) text` — once a section holds
//!   at least [`MIN_CUES`] of them and they are most of its lines. A heading
//!   that starts with a timestamp (`## 12:30 Setup`) is a chapter mark.
//!
//! A segment closes once it spans [`SEGMENT_SECONDS`] or [`SEGMENT_BYTES`].
//! Segment metadata: `timestamp` (as shown), `seconds`, `end_seconds` (start
//! of its last cue), `label` (the first words), `transcript: true`.

use super::walker::file_level;
use crate::domain::model::NodeKind;
use crate::ports::extractor::{FileExtraction, FileInput, Symbol};
use serde_json::json;
use std::collections::HashMap;

/// Fewest timestamped lines that make a Markdown section a transcript.
pub const MIN_CUES: usize = 5;
/// A segment closes once it spans this many seconds…
pub const SEGMENT_SECONDS: u32 = 120;
/// …or this many bytes of text.
pub const SEGMENT_BYTES: usize = 2400;
/// Longest `label` kept (characters).
const MAX_LABEL: usize = 120;

/// One timestamped line or subtitle cue.
#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    pub seconds: u32,
    /// Byte range of the cue in the file (timing line through its text).
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// A timestamp at the very start of `s`: `[H:]MM:SS[.mmm]`, optionally in
/// `[ ]` or `( )`. Returns the seconds and the bytes it spans. The next char
/// must not continue a word (`12:30pm` is not a timestamp).
pub fn parse_timestamp(s: &str) -> Option<(u32, usize)> {
    let b = s.as_bytes();
    let (close, mut i) = match b.first() {
        Some(b'[') => (Some(b']'), 1),
        Some(b'(') => (Some(b')'), 1),
        _ => (None, 0),
    };
    let mut groups: Vec<u32> = Vec::new();
    loop {
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let len = i - start;
        if len == 0 || len > 2 {
            return None;
        }
        if !groups.is_empty() && len != 2 {
            return None;
        }
        groups.push(s[start..i].parse().ok()?);
        if i < b.len() && b[i] == b':' && groups.len() < 3 {
            i += 1;
            continue;
        }
        break;
    }
    if groups.len() < 2 || groups[1..].iter().any(|g| *g >= 60) {
        return None;
    }
    if i < b.len() && (b[i] == b'.' || b[i] == b',') {
        let start = i + 1;
        let mut j = start;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > start && j - start <= 3 {
            i = j;
        }
    }
    if let Some(c) = close {
        if b.get(i) != Some(&c) {
            return None;
        }
        i += 1;
    }
    if b.get(i).is_some_and(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let seconds = groups.iter().fold(0u32, |acc, g| acc * 60 + g);
    Some((seconds, i))
}

/// `75` → `01:15`, `3723` → `1:02:03`.
pub fn format_timestamp(seconds: u32) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// Text after a timestamp: separators (`-`, `–`, `—`, `|`, `:`) and spaces dropped.
fn after_separator(s: &str) -> &str {
    s.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '-' | '–' | '—' | '|' | ':'))
}

/// A heading that opens with a timestamp: `(seconds, title without it)`.
pub fn heading_timestamp(title: &str) -> Option<(u32, String)> {
    let (seconds, n) = parse_timestamp(title.trim_start())?;
    Some((
        seconds,
        after_separator(&title.trim_start()[n..]).trim().to_string(),
    ))
}

fn clip(s: &str, max: usize) -> String {
    let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() > max {
        let cut: String = t.chars().take(max - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        t
    }
}

/// Lines of `content[from..to]` that start with a timestamp (after optional
/// indentation and a `-` / `*` list marker).
pub fn line_cues(content: &str, from: usize, to: usize) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut at = from;
    for line in content[from..to].split_inclusive('\n') {
        let start = at;
        at += line.len();
        let body = line.trim_start();
        let body = body
            .strip_prefix("- ")
            .or_else(|| body.strip_prefix("* "))
            .unwrap_or(body)
            .trim_start();
        if let Some((seconds, n)) = parse_timestamp(body) {
            cues.push(Cue {
                seconds,
                start,
                end: start + line.trim_end().len(),
                text: after_separator(&body[n..]).trim().to_string(),
            });
        }
    }
    cues
}

/// Is `content[from..to]` a transcript: ≥ [`MIN_CUES`] cues that make up at
/// least half of its non-blank lines?
pub fn is_transcript(content: &str, from: usize, to: usize, cues: &[Cue]) -> bool {
    let lines = content[from..to]
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    cues.len() >= MIN_CUES && cues.len() * 2 >= lines
}

/// Cues of an `.srt` / `.vtt` file: one per `start --> end` timing line, its
/// text the lines up to the next blank line. A cue starts at its block (the
/// SRT index or VTT cue id above the timing line belongs to it).
pub fn subtitle_cues(content: &str) -> Vec<Cue> {
    let mut cues: Vec<Cue> = Vec::new();
    let mut at = 0usize;
    let mut open: Option<Cue> = None;
    let mut block: Option<usize> = None;
    for line in content.split_inclusive('\n') {
        let start = at;
        at += line.len();
        let t = line.trim();
        if t.is_empty() {
            block = None;
        } else if block.is_none() {
            block = Some(start);
        }
        if let Some((left, _)) = t.split_once("-->") {
            if let Some(c) = open.take() {
                cues.push(c);
            }
            if let Some((seconds, _)) = parse_timestamp(left.trim()) {
                open = Some(Cue {
                    seconds,
                    start: block.unwrap_or(start),
                    end: start + line.trim_end().len(),
                    text: String::new(),
                });
            }
            continue;
        }
        match open.as_mut() {
            Some(c) if !t.is_empty() => {
                if !c.text.is_empty() {
                    c.text.push(' ');
                }
                c.text.push_str(t);
                c.end = start + line.trim_end().len();
            }
            Some(_) => cues.extend(open.take()),
            None => {}
        }
    }
    cues.extend(open);
    cues
}

/// Cue index ranges, one per segment (see the module docs).
pub fn group(cues: &[Cue]) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut first = 0usize;
    for i in 1..cues.len() {
        let (a, c) = (&cues[first], &cues[i]);
        if c.seconds.saturating_sub(a.seconds) >= SEGMENT_SECONDS
            || c.start - a.start >= SEGMENT_BYTES
        {
            out.push(first..i);
            first = i;
        }
    }
    if !cues.is_empty() {
        out.push(first..cues.len());
    }
    out
}

/// The enclosing section of a segment run: `(stable_id, qualified_name, level)`.
pub struct Parent<'a> {
    pub stable_id: &'a str,
    pub qualified: &'a str,
    pub level: usize,
}

/// One `Section` symbol per segment of `cues`, ending at `end` (bytes).
/// `lang` prefixes the stable_id (`markdown`, `transcript`); `ids` dedups it.
pub fn segment_symbols(
    lang: &str,
    path: &str,
    content: &str,
    cues: &[Cue],
    parent: Option<Parent>,
    end: usize,
    ids: &mut HashMap<String, usize>,
) -> Vec<Symbol> {
    let groups = group(cues);
    let mut out = Vec::new();
    for (gi, g) in groups.iter().enumerate() {
        let first = &cues[g.start];
        let last = &cues[g.end - 1];
        let raw_end = groups
            .get(gi + 1)
            .map(|n| cues[n.start].start)
            .unwrap_or(end);
        let seg_end = content[..raw_end].trim_end().len().max(first.end);
        let name = format_timestamp(first.seconds);
        let qualified = match &parent {
            Some(p) => format!("{} > {name}", p.qualified),
            None => name.clone(),
        };
        let base = format!("{lang}:{path}:section:{qualified}");
        let n = ids.entry(base.clone()).or_insert(0);
        *n += 1;
        let stable_id = if *n == 1 { base } else { format!("{base}#{n}") };
        let words: String = cues[g.clone()]
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let label = clip(&words, MAX_LABEL);
        out.push(Symbol {
            stable_id,
            kind: NodeKind::Section,
            name: name.clone(),
            qualified_name: qualified,
            parent: parent.as_ref().map(|p| p.stable_id.to_string()),
            signature: Some(clip(&format!("{name} {label}"), 200)),
            doc: None,
            line_start: line_of(content, first.start),
            line_end: line_of(content, seg_end.saturating_sub(1).max(first.start)),
            byte_start: first.start,
            byte_end: seg_end,
            segments: cues[g.clone()]
                .iter()
                .map(|c| (c.start, c.end.min(seg_end)))
                .collect(),
            metadata: json!({
                "level": parent.as_ref().map(|p| p.level + 1).unwrap_or(1),
                "timestamp": name,
                "seconds": first.seconds,
                "end_seconds": last.seconds,
                "label": label,
                "transcript": true,
            }),
        });
    }
    out
}

/// 1-based line of byte offset `at`.
pub fn line_of(content: &str, at: usize) -> u32 {
    content.as_bytes()[..at.min(content.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count() as u32
        + 1
}

/// Extract an `.srt` / `.vtt` file: one section per time segment; the header
/// before the first cue (`WEBVTT`, notes) is the file head.
pub fn extract(file: &FileInput) -> FileExtraction {
    let mut out = file_level(file);
    let content = file.content.as_str();
    let cues = subtitle_cues(content);
    if cues.is_empty() {
        return out;
    }
    out.free_ranges.clear();
    let pre = content[..cues[0].start].trim_end().len();
    if !content[..pre].trim().is_empty() {
        out.free_ranges.push((0, pre));
    }
    out.symbols = segment_symbols(
        "transcript",
        &file.path,
        content,
        &cues,
        None,
        content.len(),
        &mut HashMap::new(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::Language;

    #[test]
    fn timestamps_parse_and_format() {
        assert_eq!(parse_timestamp("12:30 Setup"), Some((750, 5)));
        assert_eq!(parse_timestamp("[00:12:30] hi"), Some((750, 10)));
        assert_eq!(parse_timestamp("(1:02:03)"), Some((3723, 9)));
        assert_eq!(parse_timestamp("00:00:01,500 --> x"), Some((1, 12)));
        assert_eq!(parse_timestamp("00:01.250"), Some((1, 9)));
        assert_eq!(parse_timestamp("12:30pm lunch"), None);
        assert_eq!(parse_timestamp("12:75"), None);
        assert_eq!(parse_timestamp("1:2"), None);
        assert_eq!(parse_timestamp("[12:30 hi"), None);
        assert_eq!(parse_timestamp("v1.2"), None);
        assert_eq!(format_timestamp(75), "01:15");
        assert_eq!(format_timestamp(3723), "1:02:03");
        assert_eq!(
            heading_timestamp("03:12 – Setup"),
            Some((192, "Setup".into()))
        );
        assert_eq!(heading_timestamp("Setup"), None);
    }

    #[test]
    fn srt_cues_group_into_two_minute_segments() {
        let mut src = String::new();
        for i in 0..8u32 {
            let s = i * 40;
            src.push_str(&format!(
                "{}\n00:{:02}:{:02},000 --> 00:{:02}:{:02},500\nline {i} text\n\n",
                i + 1,
                s / 60,
                s % 60,
                s / 60,
                s % 60
            ));
        }
        let x = extract(&FileInput {
            path: "talks/keynote.srt".into(),
            language: Language::Transcript,
            content: src.clone(),
        });
        let names: Vec<&str> = x.symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["00:00", "02:00", "04:00"]);
        let first = &x.symbols[0];
        assert_eq!(
            first.stable_id,
            "transcript:talks/keynote.srt:section:00:00"
        );
        assert_eq!(
            first.metadata["label"],
            "line 0 text line 1 text line 2 text"
        );
        assert_eq!(first.metadata["end_seconds"], 80);
        assert_eq!(first.line_start, 1, "the SRT index line opens the cue");
        assert!(x.free_ranges.is_empty());
        // Segments tile the file: each ends where the next one's cue begins.
        assert!(x
            .symbols
            .windows(2)
            .all(|w| w[0].byte_end <= w[1].byte_start));
    }

    #[test]
    fn vtt_header_is_the_file_head() {
        let src =
            "WEBVTT\n\n00:00.000 --> 00:02.000\nHello\n\n00:02.000 --> 00:04.000\nWorld\nagain\n";
        let x = extract(&FileInput {
            path: "a.vtt".into(),
            language: Language::Transcript,
            content: src.into(),
        });
        assert_eq!(x.free_ranges, vec![(0, 6)]);
        assert_eq!(x.symbols.len(), 1);
        assert_eq!(x.symbols[0].metadata["label"], "Hello World again");
    }

    #[test]
    fn markdown_lines_need_enough_cues() {
        let text = "intro\n[00:00] a\n[00:10] b\n- 00:20 c\n[00:30] d\n[00:40] e\n";
        let cues = line_cues(text, 0, text.len());
        assert_eq!(cues.len(), 5);
        assert_eq!(cues[2].text, "c");
        assert!(is_transcript(text, 0, text.len(), &cues));
        let prose = "We met at 10:30 and again\n10:45 was late\nnotes\nmore\nmore\nmore\n";
        let c2 = line_cues(prose, 0, prose.len());
        assert!(!is_transcript(prose, 0, prose.len(), &c2));
    }
}
