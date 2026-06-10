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

/// Bump this whenever [`compose_summary`] changes the embedded text. The summary
/// hash gate commits to `subtree_hash || algo_tag()`, so a bumped version
/// re-summarizes every community **once** on the next analyze even though the
/// underlying code content (and its `subtree_hash`) is unchanged. v2 dropped the
/// shared `"Feature:"` boilerplate prefix and front-loads label + key symbols so
/// summaries embed closer to natural-language queries instead of clustering. v3
/// adds a human role (journey layer), prefers real defined symbols over import
/// references for "Key symbols", and names the related features it connects to —
/// so a summary explains *what it is and where it sits*, not just lists imports.
pub const SUMMARY_ALGO_VERSION: u32 = 3;

/// Tag folded into the summary hash gate (see [`SUMMARY_ALGO_VERSION`]).
pub fn algo_tag() -> String {
    format!("-sum{SUMMARY_ALGO_VERSION}")
}

/// Result of a summarize pass over a repo's communities.
#[derive(Debug, Clone, Default)]
pub struct SummaryOutcome {
    /// Communities (re)summarized this pass.
    pub summarized: usize,
    /// Communities skipped by the hash gate (unchanged content).
    pub skipped: usize,
    /// Actual embedder calls made (== summarized − reused).
    pub embed_calls: usize,
    /// Communities restored from the content-addressed summary cache (identical
    /// content under a new community id) — zero embedder calls each.
    pub reused: usize,
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
    let tag = algo_tag();
    let pending = storage
        .communities_needing_summary(
            repo_id,
            embedder.provider(),
            embedder.model_id(),
            embedder.dimensions(),
            &tag,
        )
        .await?;

    let mut embed_calls = 0usize;
    let mut reused = 0usize;
    for (community_id, subtree_hash) in &pending {
        // Content-addressed cache first: a partition shuffle that renamed an
        // unchanged community (new id, same member content) restores the prior
        // summary + embedding with zero embedder calls.
        if storage
            .restore_cached_summary(
                *community_id,
                subtree_hash,
                &tag,
                embedder.provider(),
                embedder.model_id(),
                embedder.dimensions(),
            )
            .await?
        {
            reused += 1;
            continue;
        }
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
                &tag,
            )
            .await?;
        embed_calls += 1;
    }

    Ok(SummaryOutcome {
        summarized: pending.len(),
        skipped: total.saturating_sub(pending.len()),
        embed_calls,
        reused,
    })
}

/// Deterministically compose an extractive summary from a community's members.
/// Pure: same inputs ⇒ identical text (so the embedding is reproducible).
pub fn compose_summary(inputs: &CommunitySummaryInputs) -> String {
    let mut out = String::new();

    // Lead with the DOMAIN signal — the humanized label and the key symbols.
    // The first tokens dominate the embedding, so the old shared prefix
    // (`"Feature: …"` + `"N symbols grouped as one community"`) pulled every
    // community's vector toward a common point and away from a natural-language
    // query like "how does OCL work". Front-loading label + symbols makes the
    // vector reflect what the feature actually *is*.
    out.push_str(&humanize_label(&inputs.label));
    out.push('\n');

    // Role — where this feature sits in the user journey. Tells a reader (and the
    // embedder) *what kind of thing* it is before any symbol names.
    let role = role_phrase(crate::layering::classify_community(&inputs.members));
    out.push_str(&format!("Role: {role}.\n"));

    // Key symbols — prefer the feature's own DEFINITIONS (functions, types,
    // components) over import-reference nodes, so the line reads as "what this
    // provides" rather than a list of import paths. Falls back to any non-file
    // member when a feature has no captured definitions.
    let definitions = inputs
        .members
        .iter()
        .filter(|(_, kind, _)| is_definition_kind(kind))
        .map(|(name, _, _)| name.as_str())
        .take(MAX_KEY_SYMBOLS)
        .collect::<Vec<_>>();
    let key_symbols = if definitions.is_empty() {
        inputs
            .members
            .iter()
            .filter(|(_, kind, _)| kind != "file")
            .map(|(name, _, _)| name.as_str())
            .take(MAX_KEY_SYMBOLS)
            .collect::<Vec<_>>()
    } else {
        definitions
    };
    if !key_symbols.is_empty() {
        out.push_str(&format!("Key symbols: {}.\n", key_symbols.join(", ")));
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

    // Related features — what this connects to in the rest of the codebase. The
    // "where it sits / where it's used" context, grounded in the quotient graph.
    if !inputs.related.is_empty() {
        out.push_str(&format!(
            "Related features: {}.\n",
            inputs.related.join(", ")
        ));
    }

    // Composition (supporting detail, after the signal). Deterministic via
    // BTreeMap.
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
        out.push_str(&format!(
            "Composition: {kind_str} ({} members).\n",
            inputs.member_count
        ));
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

/// Node kinds that are real *definitions* a feature provides — its API surface —
/// as opposed to import references (`dependency`), `concept`/`use` nodes, file
/// nodes, or manifest scripts. Used to keep "Key symbols" meaningful.
fn is_definition_kind(kind: &str) -> bool {
    matches!(
        kind,
        "function"
            | "struct"
            | "enum"
            | "trait"
            | "impl"
            | "method"
            | "module"
            | "type_alias"
            | "deployment_resource"
    )
}

/// A short, human phrase for a feature's journey layer — what kind of thing it is.
fn role_phrase(layer: crate::layering::Layer) -> &'static str {
    use crate::layering::Layer;
    match layer {
        Layer::Entry => "entry point — user-facing UI, client, CLI or pages",
        Layer::Interface => "interface — the API surface (resolvers, controllers, routes)",
        Layer::Core => "core — business logic and data access (services, domain, repositories)",
        Layer::Foundation => "foundation — contracts, infrastructure, config or low-level types",
        Layer::Unknown => "supporting code (no distinct journey layer)",
    }
}

/// Expose both the raw label and its separator-split words so the embedder sees
/// the natural tokens (`onchainlabs/src/Foo.sol` → also `onchainlabs src Foo
/// sol`). Pure and deterministic.
fn humanize_label(label: &str) -> String {
    let words = label
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if words.is_empty() || words == label {
        label.to_string()
    } else {
        format!("{label} ({words})")
    }
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
            // Sorted by label, as the loader's query returns them.
            related: vec!["access · AccessResolver".into(), "ipnft · CrowdSale".into()],
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
        // Key symbols list real definitions (struct/function), not file nodes.
        assert!(a.contains("Key symbols: Tokenizer, tokenizeIpnft."));
        // v2: no shared structural prefix — leads with the domain signal (label).
        assert!(!a.starts_with("Feature:"));
        assert!(a.starts_with("ipnft"));
        // Humanized label exposes the separator-split words for the embedder.
        assert!(a.contains("ipnft Tokenizer"));
        // v3: states a role and names the related features it connects to.
        assert!(a.contains("Role: foundation"), "summary states the role");
        assert!(
            a.contains("Related features: access · AccessResolver, ipnft · CrowdSale."),
            "summary names related features"
        );
    }

    #[test]
    fn key_symbols_prefer_definitions_over_imports() {
        // A feature whose members are mostly import references plus one real
        // component: the summary should name the component, not the imports.
        let mixed = CommunitySummaryInputs {
            label: "labs · TokenPanel".into(),
            member_count: 4,
            members: vec![
                (
                    "TokenPanel".into(),
                    "function".into(),
                    "labs/token-panel.tsx".into(),
                ),
                (
                    "@/app/ipnft-provider".into(),
                    "dependency".into(),
                    String::new(),
                ),
                (
                    "./columns".into(),
                    "dependency".into(),
                    "labs/token-panel.tsx".into(),
                ),
                (
                    "token-panel.tsx".into(),
                    "file".into(),
                    "labs/token-panel.tsx".into(),
                ),
            ],
            snippets: vec![],
            related: vec![],
        };
        let s = compose_summary(&mixed);
        assert!(
            s.contains("Key symbols: TokenPanel."),
            "names the definition: {s}"
        );
        assert!(
            !s.contains("Key symbols: TokenPanel, @/app"),
            "imports excluded"
        );
    }

    #[test]
    fn algo_tag_tracks_version() {
        assert_eq!(algo_tag(), format!("-sum{SUMMARY_ALGO_VERSION}"));
    }

    #[test]
    fn summary_respects_length_cap() {
        let big = CommunitySummaryInputs {
            label: "x".into(),
            member_count: 1,
            members: vec![("f".into(), "function".into(), "a.rs".into())],
            snippets: vec!["A".repeat(10_000)],
            related: vec![],
        };
        assert!(compose_summary(&big).len() <= MAX_SUMMARY_CHARS);
    }
}
