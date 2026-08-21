//! Knowledge-gap detection: deterministically flag indexed files whose text
//! carries too little semantic vocabulary to ever match a meaningful query.
//!
//! Retrieval can only be as good as the words a file brings to the index.
//! After identifier splitting (migration 008) most code contributes real
//! vocabulary ("listAllOnChainLabs" → "list all on chain labs"), but a file
//! of single-letter names, bare numbers, and zero comments gives every
//! retrieval method nothing to grip. No algorithm can conjure that meaning —
//! a human (or an agent asking the human) has to add it, as a file-top
//! docstring or a folder README, and then re-index with `chaos add`.
//!
//! The detector is corpus-driven, not word-list-driven: a word counts as
//! BACKGROUND when it appears in a large fraction of the repo's files
//! (import/export/const/return emerge from the data), and a file's signal is
//! its DISTINCTIVE vocabulary — words that actually discriminate it from the
//! rest of the repo. Embedder-free and read-only over persisted chunks.

use crate::storage::Storage;
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// What to do with a gap — the answer lives in the repo, not in a prompt.
/// This text is read by the CALLING AGENT on every report: it must draw the
/// boundary explicitly, because an eager agent otherwise treats "a chunking
/// bug exists" as license to open and edit the chaos-substrate source from a
/// target-repo session (observed live, twice).
const NEXT_INSTRUCTION: &str = "THE GAPS CHECK ENDS HERE — summarize these findings for the user and STOP. low-vocabulary files: offer to add a file-top docstring (or folder README) saying what the file is for, then chaos_add those paths; never block indexing on it. no-chunks files: re-run chaos_add on them once; if they stay chunkless that is a Chaos chunking-coverage BUG — tell the user so they can open a chaos-substrate session about it. Do NOT investigate, read, or edit the chaos-substrate source from this session, and do not re-check gaps with find/grep/git.";

/// Words shorter/longer than this are noise (single letters, hashes, blobs).
const MIN_WORD: usize = 3;
const MAX_WORD: usize = 24;
/// A word in at least this fraction of scanned files is repo background
/// vocabulary and carries no distinguishing signal.
const BACKGROUND_DOCUMENT_FREQUENCY: f64 = 0.25;
/// Background needs a real corpus: below this many files every word counts.
const MIN_CORPUS_FILES: usize = 8;
/// A code file with fewer distinctive words than this is flagged.
const MIN_DISTINCTIVE_WORDS: usize = 6;
/// Tiny files (re-export shims, generated stubs) are not worth a docstring.
const MIN_FLAGGABLE_LINES: i32 = 5;
/// Cap on flagged files in the COMPACT return (full count is reported).
const MAX_RETURNED_GAPS: usize = 30;
/// Sample of a flagged file's distinctive words shown as evidence.
const SAMPLE_WORDS: usize = 6;

/// Languages whose files are expected to carry identifier/docstring
/// vocabulary. Prose (markdown/pdf) and config (json) are never flagged.
const CODE_LANGUAGES: &[&str] = &[
    "typescript",
    "javascript",
    "rust",
    "python",
    "solidity",
    "graphql",
];

#[derive(Debug, Serialize)]
pub struct GapsReport {
    pub repo: String,
    pub files_scanned: usize,
    pub code_files_scored: usize,
    pub background_words: usize,
    pub flagged_total: usize,
    /// Files that produced NO chunks at all — unfindable by every retrieval
    /// method. An indexing-coverage gap (possibly a chunking bug, possibly
    /// by-design for configs), NOT fixable by wording. Total count; list
    /// capped at [`MAX_RETURNED_GAPS`].
    pub no_chunk_files: usize,
    pub coverage_gaps: Vec<GapFile>,
    /// Chunked code whose vocabulary can't match any meaningful query — the
    /// docstring/README candidates. Most opaque first; capped at
    /// [`MAX_RETURNED_GAPS`]; total = `flagged_total - no_chunk_files`.
    pub vocabulary_gaps: Vec<GapFile>,
    /// What to do with a gap — the answer lives in the repo, not in a prompt.
    pub next: &'static str,
}

#[derive(Debug, Serialize)]
pub struct GapFile {
    pub path: String,
    pub language: String,
    pub line_count: i32,
    /// `"no-chunks"` (never chunked — unfindable regardless of wording) or
    /// `"low-vocabulary"` (chunked, but nothing distinctive to match).
    pub reason: &'static str,
    /// Distinct non-background words the file contributes to the index.
    pub distinctive_words: usize,
    /// The strongest words it does have (evidence the score is fair).
    pub sample: Vec<String>,
}

/// Per-file text aggregated from persisted chunks (latest indexed version).
pub struct FileText {
    pub path: String,
    pub language: String,
    pub line_count: i32,
    pub text: String,
    pub chunk_count: i64,
}

/// Per-member caps in a PROJECT report keep the combined return compact
/// (one project = several member reports in one response).
const PROJECT_MEMBER_GAPS: usize = 10;

/// Cross-repo gaps: one report per PROJECT member repo, repo-tagged — the
/// project layer is the multi-repo answer (a sub-app inside one indexed repo
/// is the `folder` scope instead).
#[derive(Debug, Serialize)]
pub struct ProjectGapsReport {
    pub project: String,
    pub flagged_total: usize,
    pub no_chunk_files: usize,
    pub members: Vec<MemberGaps>,
    pub next: &'static str,
}

#[derive(Debug, Serialize)]
pub struct MemberGaps {
    pub alias: String,
    pub repo: String,
    pub report: GapsReport,
}

pub async fn build_gaps_report(
    storage: &Storage,
    repo_id: Uuid,
    repo_label: &str,
    folder: Option<&str>,
) -> Result<GapsReport> {
    let files = storage.load_file_texts(repo_id).await?;
    Ok(score_gaps_scoped(repo_label, &files, folder))
}

pub async fn build_project_gaps_report(
    storage: &Storage,
    project_name: &str,
) -> Result<ProjectGapsReport> {
    let project = storage
        .find_project(project_name)
        .await?
        .with_context(|| format!("project does not exist: {project_name}"))?;
    let members = storage.project_member_repos(project.id).await?;
    anyhow::ensure!(
        !members.is_empty(),
        "project {} has no repositories — add one with `chaos project add-repo {} <repo-path>`",
        project.name,
        project.name
    );
    let mut reports = Vec::new();
    let (mut flagged_total, mut no_chunk_files) = (0usize, 0usize);
    for member in &members {
        let files = storage.load_file_texts(member.repo.id).await?;
        // Background vocabulary stays PER member repo: what is boilerplate in
        // a Solidity contracts repo is distinctive in a TypeScript client.
        let mut report = score_gaps_scoped(&member.repo.name, &files, None);
        report.coverage_gaps.truncate(PROJECT_MEMBER_GAPS);
        report.vocabulary_gaps.truncate(PROJECT_MEMBER_GAPS);
        flagged_total += report.flagged_total;
        no_chunk_files += report.no_chunk_files;
        reports.push(MemberGaps {
            alias: member.alias.clone(),
            repo: member.repo.name.clone(),
            report,
        });
    }
    Ok(ProjectGapsReport {
        project: project.name,
        flagged_total,
        no_chunk_files,
        members: reports,
        next: NEXT_INSTRUCTION,
    })
}

/// `folder` restricts FLAGGING to files under that path prefix (a sub-app
/// inside a monorepo-indexed repo); background frequencies still come from
/// the whole corpus, which only makes the background more representative.
pub fn score_gaps_scoped(repo_label: &str, files: &[FileText], folder: Option<&str>) -> GapsReport {
    let vocab: Vec<HashSet<String>> = files
        .iter()
        .map(|file| identifier_words(&file.text))
        .collect();

    // Corpus-derived background: document frequency across ALL scanned files
    // (prose included — a word explained in the README is still background).
    let mut document_frequency: HashMap<&str, usize> = HashMap::new();
    for words in &vocab {
        for word in words {
            *document_frequency.entry(word).or_default() += 1;
        }
    }
    let threshold = if files.len() >= MIN_CORPUS_FILES {
        ((files.len() as f64) * BACKGROUND_DOCUMENT_FREQUENCY).ceil() as usize
    } else {
        usize::MAX
    };
    let background: HashSet<&str> = document_frequency
        .iter()
        .filter(|(_, df)| **df >= threshold)
        .map(|(word, _)| *word)
        .collect();

    let scope_prefix = folder
        .map(|f| f.trim_matches('/'))
        .filter(|f| !f.is_empty())
        .map(|f| format!("{f}/"));
    let mut code_files_scored = 0usize;
    let mut flagged: Vec<GapFile> = Vec::new();
    for (file, words) in files.iter().zip(&vocab) {
        if let Some(prefix) = &scope_prefix {
            if !file.path.starts_with(prefix) && file.path != prefix[..prefix.len() - 1] {
                continue;
            }
        }
        if !CODE_LANGUAGES.contains(&file.language.as_str())
            || file.line_count < MIN_FLAGGABLE_LINES
        {
            continue;
        }
        code_files_scored += 1;
        let mut distinctive: Vec<&String> = words
            .iter()
            .filter(|word| !background.contains(word.as_str()))
            .collect();
        if file.chunk_count > 0 && distinctive.len() >= MIN_DISTINCTIVE_WORDS {
            continue;
        }
        // Deterministic evidence sample: rarest first, then alphabetical.
        distinctive.sort_by_key(|word| {
            (
                document_frequency.get(word.as_str()).copied().unwrap_or(0),
                (*word).clone(),
            )
        });
        flagged.push(GapFile {
            path: file.path.clone(),
            language: file.language.clone(),
            line_count: file.line_count,
            reason: if file.chunk_count == 0 {
                "no-chunks"
            } else {
                "low-vocabulary"
            },
            distinctive_words: distinctive.len(),
            sample: distinctive
                .into_iter()
                .take(SAMPLE_WORDS)
                .cloned()
                .collect(),
        });
    }
    // Most opaque first; path tie-break keeps the order stable.
    flagged.sort_by(|a, b| {
        a.distinctive_words
            .cmp(&b.distinctive_words)
            .then_with(|| a.path.cmp(&b.path))
    });

    let flagged_total = flagged.len();
    let (mut coverage_gaps, mut vocabulary_gaps): (Vec<GapFile>, Vec<GapFile>) = flagged
        .into_iter()
        .partition(|gap| gap.reason == "no-chunks");
    let no_chunk_files = coverage_gaps.len();
    coverage_gaps.truncate(MAX_RETURNED_GAPS);
    vocabulary_gaps.truncate(MAX_RETURNED_GAPS);
    GapsReport {
        repo: repo_label.to_string(),
        files_scanned: files.len(),
        code_files_scored,
        background_words: background.len(),
        flagged_total,
        no_chunk_files,
        coverage_gaps,
        vocabulary_gaps,
        next: NEXT_INSTRUCTION,
    }
}

/// Distinct lowercase words after identifier splitting: camelCase /
/// PascalCase / ACRONYMWord boundaries and non-alphanumeric separators —
/// the Rust-side mirror of the SQL `chaos_identifier_text` (migration 008).
fn identifier_words(text: &str) -> HashSet<String> {
    let mut words = HashSet::new();
    for raw in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        for word in split_camel(raw) {
            let lower = word.to_ascii_lowercase();
            if (MIN_WORD..=MAX_WORD).contains(&lower.len())
                && lower.chars().all(|c| c.is_ascii_alphabetic())
            {
                words.insert(lower);
            }
        }
    }
    words
}

/// Split one identifier at camel boundaries: `listAllOnChainLabs` →
/// [list, All, On, Chain, Labs]; `OCLProcessor` → [OCL, Processor].
/// Shared with the L3 summary composer (`community_summary`), which renders
/// key symbols "in words" so a feature embeds near natural-language queries.
pub(crate) fn split_camel(token: &str) -> Vec<&str> {
    let bytes = token.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    for i in 1..bytes.len() {
        let prev = bytes[i - 1] as char;
        let cur = bytes[i] as char;
        let next_lower = bytes
            .get(i + 1)
            .map(|b| (*b as char).is_ascii_lowercase())
            .unwrap_or(false);
        // lower/digit → Upper, or ACRONYM → Word (last capital before a lower).
        if (prev.is_ascii_lowercase() || prev.is_ascii_digit()) && cur.is_ascii_uppercase()
            || prev.is_ascii_uppercase() && cur.is_ascii_uppercase() && next_lower
        {
            parts.push(&token[start..i]);
            start = i;
        }
    }
    parts.push(&token[start..]);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, language: &str, line_count: i32, text: &str) -> FileText {
        FileText {
            path: path.to_string(),
            language: language.to_string(),
            line_count,
            text: text.to_string(),
            chunk_count: 1,
        }
    }

    #[test]
    fn folder_scope_restricts_flagging_but_not_background() {
        let files = vec![
            file(
                "apps/client/opaque.ts",
                "typescript",
                40,
                "import const a b",
            ),
            file(
                "apps/server/opaque.ts",
                "typescript",
                40,
                "import const a b",
            ),
        ];
        let scoped = score_gaps_scoped("demo", &files, Some("apps/client"));
        assert_eq!(scoped.flagged_total, 1);
        assert_eq!(scoped.vocabulary_gaps[0].path, "apps/client/opaque.ts");
        let unscoped = score_gaps_scoped("demo", &files, None);
        assert_eq!(unscoped.flagged_total, 2);
    }

    #[test]
    fn unchunked_code_is_a_coverage_gap_and_sorts_first() {
        let mut unchunked = file("src/zz-spec.ts", "typescript", 200, "");
        unchunked.chunk_count = 0;
        let report = score_gaps_scoped(
            "demo",
            &[
                unchunked,
                file("src/opaque.ts", "typescript", 50, "import const a b"),
            ],
            None,
        );
        assert_eq!(report.flagged_total, 2);
        assert_eq!(report.no_chunk_files, 1);
        assert_eq!(report.coverage_gaps.len(), 1);
        assert_eq!(report.coverage_gaps[0].path, "src/zz-spec.ts");
        assert_eq!(report.coverage_gaps[0].reason, "no-chunks");
        assert_eq!(report.vocabulary_gaps.len(), 1);
        assert_eq!(report.vocabulary_gaps[0].reason, "low-vocabulary");
    }

    #[test]
    fn splits_camel_and_acronym_boundaries() {
        assert_eq!(
            split_camel("listAllOnChainLabs"),
            vec!["list", "All", "On", "Chain", "Labs"]
        );
        assert_eq!(split_camel("OCLProcessor"), vec!["OCL", "Processor"]);
        assert_eq!(split_camel("plain"), vec!["plain"]);
    }

    #[test]
    fn identifier_words_drop_noise() {
        let words = identifier_words("getUserById(id_7) // x %% 0xFF deadbeefcafebabe1234567890");
        assert!(words.contains("get") && words.contains("user"));
        assert!(!words.contains("id"), "too short");
        assert!(!words.contains("0xff"), "non-alphabetic");
        assert!(!words.contains("deadbeefcafebabe1234567890"), "too long");
    }

    #[test]
    fn flags_opaque_code_but_not_rich_code_or_prose() {
        // Enough files that background frequency kicks in; every code file
        // shares boilerplate, only some carry distinctive vocabulary. Each
        // rich file gets per-file-unique words (letter suffix — digits are
        // filtered out of vocabulary) so they stay below background frequency.
        let boilerplate = "import export const function return async await";
        let stems = ["aurora", "breeze", "canyon", "dune", "ember", "fjord"];
        let mut files: Vec<FileText> = (0..9)
            .map(|i| {
                let suffix = (b'a' + i as u8) as char;
                let unique = stems
                    .iter()
                    .map(|stem| format!("{stem}{suffix}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                file(
                    &format!("src/feature_{i}.ts"),
                    "typescript",
                    80,
                    &format!("{boilerplate} {unique}"),
                )
            })
            .collect();
        files.push(file(
            "src/q.ts",
            "typescript",
            120,
            "import export const function return async await a b c x1 y2",
        ));
        files.push(file("README.md", "markdown", 200, "short"));
        let report = score_gaps_scoped("demo", &files, None);
        assert_eq!(report.flagged_total, 1);
        assert_eq!(report.vocabulary_gaps[0].path, "src/q.ts");
        assert_eq!(report.vocabulary_gaps[0].distinctive_words, 0);
        assert_eq!(report.files_scanned, 11);
        assert_eq!(report.code_files_scored, 10);
    }

    #[test]
    fn tiny_files_and_small_corpora_are_not_flagged() {
        let report = score_gaps_scoped(
            "demo",
            &[
                file("src/shim.ts", "typescript", 2, "export x"),
                file("src/a.ts", "typescript", 50, "import const a b"),
            ],
            None,
        );
        // shim too small; corpus too small for background, so a.ts keeps all
        // its words ("import const") as distinctive — below the floor it
        // would flag, but only words ≥ MIN_WORD count and both qualify… the
        // file still has < 6 distinctive words and ≥ 5 lines, so it flags.
        assert_eq!(report.flagged_total, 1);
        assert_eq!(report.vocabulary_gaps[0].path, "src/a.ts");
    }
}
