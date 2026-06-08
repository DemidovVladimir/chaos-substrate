//! L3 community ("god-node") summaries — hash-gated and really embedded.
//!
//! Each community gets a short "what this feature includes" summary, embedded by
//! the **real** OpenAI/Ollama embedder and stored in `community_embeddings`. A
//! summary is (re)computed **only when the community's L2 `subtree_hash` differs
//! from its stored `summary_hash`** (or it has no embedding yet) — so re-indexing
//! unchanged code makes **zero** embedder calls. This is the headline efficiency
//! property: L2 (hash tree) gates L3 (summary tree).
//!
//! # Summaries are extractive, not generated
//!
//! The crate has a real embedder but **no LLM text-generation client**, and the
//! hard rules forbid inventing one casually. So summaries are built
//! *extractively* — deterministically composed from the community's own member
//! symbols, files, languages, and representative code snippets. This keeps them
//! real (grounded in the code), reproducible (same content ⇒ same text ⇒ same
//! embedding), and free of any fabrication. A future LLM-generated variant can
//! slot in behind the same hash gate without changing the storage contract.
//!
//! # Fail closed
//!
//! Embedding goes through the real embedder; if it is unavailable the call
//! errors and **no placeholder vector is ever written** (invariant #1).

use crate::embedding::Embedder;
use crate::storage::{CommunitySummaryInputs, Storage};
use anyhow::Result;
use std::collections::BTreeMap;
use uuid::Uuid;

/// Hard cap on the composed summary length handed to the embedder.
const MAX_SUMMARY_CHARS: usize = 1600;
/// Representative snippet budget (chars) inside a summary.
const MAX_SNIPPET_CHARS: usize = 320;
/// How many key symbols to name.
const MAX_KEY_SYMBOLS: usize = 24;

/// Result of a summarize pass over a repo's communities.
#[derive(Debug, Clone, Default)]
pub struct SummaryOutcome {
    /// Communities (re)summarized this pass.
    pub summarized: usize,
    /// Communities skipped by the hash gate (unchanged content).
    pub skipped: usize,
    /// Actual embedder calls made (== summarized).
    pub embed_calls: usize,
}

/// Summarize every community that needs it (hash-gated), embedding each summary
/// with the real embedder. Runs after L1 (communities) and L2 (subtree hashes)
/// are persisted.
pub async fn summarize_repo(
    storage: &Storage,
    embedder: &dyn Embedder,
    repo_id: Uuid,
) -> Result<SummaryOutcome> {
    let total = storage.count_hashed_communities(repo_id).await? as usize;
    let pending = storage
        .communities_needing_summary(
            repo_id,
            embedder.provider(),
            embedder.model_id(),
            embedder.dimensions(),
        )
        .await?;

    let mut embed_calls = 0usize;
    for (community_id, subtree_hash) in &pending {
        let inputs = storage.load_community_summary_inputs(*community_id).await?;
        let summary = compose_summary(&inputs);
        // Real embedder — fail closed (no placeholder vector ever written).
        let embedding = embedder.embed(&summary).await?;
        storage
            .save_community_summary(
                *community_id,
                &summary,
                subtree_hash,
                embedder.provider(),
                embedder.model_id(),
                embedder.dimensions(),
                &embedding,
            )
            .await?;
        embed_calls += 1;
    }

    Ok(SummaryOutcome {
        summarized: pending.len(),
        skipped: total.saturating_sub(pending.len()),
        embed_calls,
    })
}

/// Deterministically compose an extractive summary from a community's members.
/// Pure: same inputs ⇒ identical text (so the embedding is reproducible).
pub fn compose_summary(inputs: &CommunitySummaryInputs) -> String {
    let mut out = String::new();
    out.push_str(&format!("Feature: {}\n", inputs.label));
    out.push_str(&format!(
        "{} symbols grouped as one community.\n",
        inputs.member_count
    ));

    // Language / kind mix (deterministic via BTreeMap).
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, kind, _) in &inputs.members {
        *kinds.entry(kind.as_str()).or_insert(0) += 1;
    }
    if !kinds.is_empty() {
        let kind_str = kinds
            .iter()
            .map(|(k, n)| format!("{n} {k}"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("Composition: {kind_str}.\n"));
    }

    // Distinct files (ordered).
    let mut files: BTreeMap<&str, ()> = BTreeMap::new();
    for (_, _, path) in &inputs.members {
        if !path.is_empty() {
            files.insert(path.as_str(), ());
        }
    }
    if !files.is_empty() {
        let file_list = files
            .keys()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("Files: {file_list}.\n"));
    }

    // Key symbols (skip file nodes — the symbol names are the signal).
    let key_symbols = inputs
        .members
        .iter()
        .filter(|(_, kind, _)| kind != "file")
        .map(|(name, _, _)| name.as_str())
        .take(MAX_KEY_SYMBOLS)
        .collect::<Vec<_>>();
    if !key_symbols.is_empty() {
        out.push_str(&format!("Key symbols: {}.\n", key_symbols.join(", ")));
    }

    // Representative code snippets.
    if !inputs.snippets.is_empty() {
        out.push_str("Representative code:\n");
        for snippet in &inputs.snippets {
            let trimmed: String = snippet.chars().take(MAX_SNIPPET_CHARS).collect();
            out.push_str(&trimmed);
            out.push('\n');
            if out.len() >= MAX_SUMMARY_CHARS {
                break;
            }
        }
    }

    out.chars().take(MAX_SUMMARY_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> CommunitySummaryInputs {
        CommunitySummaryInputs {
            label: "ipnft · Tokenizer".into(),
            member_count: 4,
            members: vec![
                (
                    "Tokenizer".into(),
                    "struct".into(),
                    "IPNFT/src/Tokenizer.sol".into(),
                ),
                (
                    "tokenizeIpnft".into(),
                    "function".into(),
                    "IPNFT/src/Tokenizer.sol".into(),
                ),
                (
                    "Tokenizer.sol".into(),
                    "file".into(),
                    "IPNFT/src/Tokenizer.sol".into(),
                ),
            ],
            snippets: vec!["function tokenizeIpnft(uint256 id) external { ... }".into()],
        }
    }

    #[test]
    fn summary_is_deterministic_and_grounded() {
        let a = compose_summary(&inputs());
        let b = compose_summary(&inputs());
        assert_eq!(a, b, "same inputs => identical summary text");
        assert!(a.contains("Tokenizer"));
        assert!(a.contains("tokenizeIpnft"));
        assert!(a.contains("IPNFT/src/Tokenizer.sol"));
        // File nodes are not listed as key symbols.
        assert!(a.contains("Key symbols: Tokenizer, tokenizeIpnft."));
    }

    #[test]
    fn summary_respects_length_cap() {
        let big = CommunitySummaryInputs {
            label: "x".into(),
            member_count: 1,
            members: vec![("f".into(), "function".into(), "a.rs".into())],
            snippets: vec!["A".repeat(10_000)],
        };
        assert!(compose_summary(&big).len() <= MAX_SUMMARY_CHARS);
    }
}
