//! Templated L2 community summaries (deterministic, embedder-free text) and
//! the key their embeddings are stored under: the hash of the text itself.

use crate::domain::hash::sha256_hex;
use std::collections::BTreeSet;

/// Hard cap on the composed summary text.
pub const SUMMARY_MAX_CHARS: usize = 1600;

const MAX_SYMBOLS: usize = 12;
const MAX_DOCS: usize = 4;
const MAX_FILES: usize = 8;
const MAX_NEIGHBOURS: usize = 6;
const DOC_SENTENCE_MAX: usize = 160;

/// Everything a summary is composed from (all already persisted index data).
#[derive(Debug, Clone, Default)]
pub struct SummaryInput {
    pub label: String,
    pub files: Vec<String>,
    /// `(name, kind)` of the most-connected member nodes, best first.
    pub top_members: Vec<(String, String)>,
    /// Doc comments / markdown leads of the members.
    pub docs: Vec<String>,
    /// Labels of the neighbouring communities in the quotient graph.
    pub neighbours: Vec<String>,
}

/// Compose the summary text (≤ [`SUMMARY_MAX_CHARS`]). Sections are emitted
/// only when they have content, in a fixed order.
pub fn compose(input: &SummaryInput) -> String {
    let mut sections: Vec<String> = Vec::new();
    let label = input.label.trim();
    sections.push(format!(
        "Feature: {}",
        if label.is_empty() {
            "(unlabelled)"
        } else {
            label
        }
    ));

    let symbols: Vec<String> = input
        .top_members
        .iter()
        .filter(|(n, _)| !n.trim().is_empty())
        .take(MAX_SYMBOLS)
        .map(|(n, k)| {
            if k.trim().is_empty() {
                n.trim().to_string()
            } else {
                format!("{} ({})", n.trim(), k.trim())
            }
        })
        .collect();
    if !symbols.is_empty() {
        sections.push(format!("Key symbols: {}", symbols.join(", ")));
    }

    let mut seen = BTreeSet::new();
    let mut words: Vec<String> = Vec::new();
    for name in std::iter::once(label.to_string()).chain(
        input
            .top_members
            .iter()
            .take(MAX_SYMBOLS)
            .map(|(n, _)| n.clone()),
    ) {
        for w in split_identifier(&name) {
            if seen.insert(w.clone()) {
                words.push(w);
            }
        }
    }
    if !words.is_empty() {
        sections.push(format!("In words: {}", words.join(" ")));
    }

    let docs: Vec<String> = input
        .docs
        .iter()
        .map(|d| first_sentence(d))
        .filter(|s| !s.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(MAX_DOCS)
        .collect();
    if !docs.is_empty() {
        sections.push(format!("Docs: {}", docs.join(" ")));
    }

    let files: Vec<&str> = input
        .files
        .iter()
        .map(|f| f.trim())
        .filter(|f| !f.is_empty())
        .take(MAX_FILES)
        .collect();
    if !files.is_empty() {
        let more = input.files.len().saturating_sub(files.len());
        let suffix = if more > 0 {
            format!(" (+{more} more)")
        } else {
            String::new()
        };
        sections.push(format!("Files: {}{suffix}", files.join(", ")));
    }

    let neighbours: Vec<&str> = input
        .neighbours
        .iter()
        .map(|n| n.trim())
        .filter(|n| !n.is_empty())
        .take(MAX_NEIGHBOURS)
        .collect();
    if !neighbours.is_empty() {
        sections.push(format!("Related features: {}", neighbours.join(", ")));
    }

    truncate(&sections.join("\n"), SUMMARY_MAX_CHARS)
}

/// Key of a summary's embedding: the hash of the composed text and the model.
/// An embedding stored under the same key is the same vector, so it is reused;
/// a template change changes the text and therefore the key.
pub fn summary_hash(text: &str, model_id: &str) -> String {
    sha256_hex(&format!("{model_id}\n{text}"))
}

/// Split an identifier into lower-case words: `camelCase`, `PascalCase`,
/// `snake_case`, `kebab-case`, `path::sep`, digits-as-words. Single-letter
/// fragments are dropped.
pub fn split_identifier(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            flush(&mut cur, &mut words);
            continue;
        }
        if i > 0 && !cur.is_empty() {
            let prev = chars[i - 1];
            let next = chars.get(i + 1).copied();
            let camel = c.is_uppercase() && (prev.is_lowercase() || prev.is_ascii_digit());
            let acronym_end =
                c.is_uppercase() && prev.is_uppercase() && next.is_some_and(|n| n.is_lowercase());
            let digit_edge = c.is_ascii_digit() != prev.is_ascii_digit();
            if camel || acronym_end || digit_edge {
                flush(&mut cur, &mut words);
            }
        }
        cur.push(c);
    }
    flush(&mut cur, &mut words);
    words
}

fn flush(cur: &mut String, words: &mut Vec<String>) {
    if cur.chars().count() > 1 {
        words.push(cur.to_lowercase());
    }
    cur.clear();
}

fn first_sentence(doc: &str) -> String {
    let text: String = doc.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.trim_start_matches(['/', '*', '#', '!', ' ']);
    let end = text
        .char_indices()
        .find(|&(i, c)| {
            matches!(c, '.' | '!' | '?') && text[i + c.len_utf8()..].starts_with(' ') || c == '\n'
        })
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(text.len());
    truncate(text[..end].trim(), DOC_SENTENCE_MAX)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut idx = max;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    s[..idx].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> SummaryInput {
        SummaryInput {
            label: "storage/index".into(),
            files: vec!["src/storage/index.rs".into(), "src/storage/mod.rs".into()],
            top_members: vec![
                ("replaceRepoIndex".into(), "function".into()),
                ("insert_chunk".into(), "function".into()),
                ("HTTPClient".into(), "struct".into()),
            ],
            docs: vec![
                "Replace the whole index of a repo. Then more.".into(),
                "/// Insert one chunk!  Extra".into(),
            ],
            neighbours: vec!["embedding".into(), "community".into()],
        }
    }

    #[test]
    fn composes_all_sections_deterministically() {
        let a = compose(&input());
        let b = compose(&input());
        assert_eq!(a, b);
        assert!(a.starts_with("Feature: storage/index\n"));
        assert!(a.contains("Key symbols: replaceRepoIndex (function), insert_chunk (function), HTTPClient (struct)"));
        assert!(a.contains("In words: storage index replace repo insert chunk http client"));
        assert!(a.contains("Docs: Insert one chunk! Replace the whole index of a repo."));
        assert!(a.contains("Files: src/storage/index.rs, src/storage/mod.rs"));
        assert!(a.contains("Related features: embedding, community"));
        let minimal = compose(&SummaryInput::default());
        assert_eq!(minimal, "Feature: (unlabelled)");
    }

    #[test]
    fn identifier_splitting() {
        assert_eq!(
            split_identifier("replaceRepoIndex"),
            vec!["replace", "repo", "index"]
        );
        assert_eq!(
            split_identifier("HTTPClient2Go"),
            vec!["http", "client", "go"]
        );
        assert_eq!(
            split_identifier("snake_case_name"),
            vec!["snake", "case", "name"]
        );
        assert_eq!(split_identifier("a::b::Cd"), vec!["cd"]);
        assert_eq!(split_identifier("v2Api"), vec!["api"]);
        assert!(split_identifier("x").is_empty());
    }

    #[test]
    fn respects_length_cap_and_gate() {
        let mut big = input();
        big.docs = (0..50)
            .map(|i| format!("Sentence number {i} is quite long and descriptive. Tail"))
            .collect();
        big.files = (0..100)
            .map(|i| format!("src/very/long/path/number/{i}/file.rs"))
            .collect();
        big.top_members = (0..100)
            .map(|i| (format!("someVeryLongSymbolName{i}"), "function".into()))
            .collect();
        let s = compose(&big);
        assert!(s.len() <= SUMMARY_MAX_CHARS);
        assert!(s.contains("(+92 more)"));
        assert_eq!(summary_hash("t", "m"), sha256_hex("m\nt"));
        assert_ne!(summary_hash("t", "m"), summary_hash("t", "n"));
        assert_ne!(summary_hash("t", "m"), summary_hash("u", "m"));
    }
}
