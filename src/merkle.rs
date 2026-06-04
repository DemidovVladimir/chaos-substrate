//! L2 hash-rollup (Merkle) layer.
//!
//! Rolls the `content_hash` leaves that already exist (`KnowledgeChunk` /
//! `SourceFile`) up through three deterministic levels:
//!
//! ```text
//! chunk.content_hash ──▶ file.subtree_hash ──▶ community.subtree_hash
//!                                          └──▶ repositories.repo_root_hash
//! ```
//!
//! Every level is `sha256` over a *canonically ordered* list of child hashes,
//! so the result is byte-identical across re-indexes (it depends only on
//! content, never on regenerated node/chunk UUIDs). This is what makes
//! incremental re-index cheap and gates expensive L1 summaries (P3): only a
//! community whose `subtree_hash` actually moved needs re-summarizing.
//!
//! The hash tree and the L1 summary tree are the same shape with different
//! payloads — see `docs/HIERARCHICAL_MEMORY_ROADMAP.md` §1.

use crate::extractor::hash;
use crate::storage::Storage;
use anyhow::Result;
use std::collections::HashMap;
use uuid::Uuid;

/// Hash an already-canonically-ordered list of child hashes. The join keeps the
/// implementation explicit (no hidden ordering); callers sort first.
pub fn rollup(ordered_child_hashes: &[String]) -> String {
    hash(&ordered_child_hashes.join("\n"))
}

/// Result of a rollup pass.
#[derive(Debug, Clone)]
pub struct MerkleOutcome {
    pub repo_root_hash: String,
    /// community id -> subtree hash (post-rollup).
    pub community_hashes: HashMap<Uuid, String>,
}

/// Compute and persist the full file → community → repo rollup for a repo.
/// Returns the new repo root and per-community hashes so callers can diff for
/// blast radius (`add`) or gate summaries (P3).
pub async fn compute_and_persist(storage: &Storage, repo_id: Uuid) -> Result<MerkleOutcome> {
    // 1. File leaves: sha256 over each file's canonically-ordered chunk hashes.
    let file_chunks = storage.load_file_chunk_hashes(repo_id).await?;
    let mut file_hash: HashMap<Uuid, String> = HashMap::with_capacity(file_chunks.len());
    // (path, hash) pairs for the repo root, sorted by path for stability.
    let mut path_hashes: Vec<(String, String)> = Vec::with_capacity(file_chunks.len());
    for (file_id, path, chunk_hashes) in &file_chunks {
        let h = rollup(chunk_hashes);
        file_hash.insert(*file_id, h.clone());
        path_hashes.push((path.clone(), h));
    }
    let file_updates: Vec<(Uuid, String)> =
        file_hash.iter().map(|(id, h)| (*id, h.clone())).collect();
    storage.update_file_subtree_hashes(&file_updates).await?;

    // 2. Community roots: sha256 over each community's member-file hashes,
    //    ordered by file path. A file shared across communities (its symbols
    //    landed in different features) flips every community it touches.
    let path_by_file: HashMap<Uuid, String> = file_chunks
        .iter()
        .map(|(id, path, _)| (*id, path.clone()))
        .collect();
    let community_files = storage.load_community_member_files(repo_id).await?;
    let mut community_hashes: HashMap<Uuid, String> = HashMap::with_capacity(community_files.len());
    for (community_id, file_ids) in &community_files {
        let mut entries: Vec<(String, String)> = file_ids
            .iter()
            .filter_map(|fid| {
                let path = path_by_file.get(fid)?;
                let h = file_hash.get(fid)?;
                Some((path.clone(), h.clone()))
            })
            .collect();
        entries.sort();
        let child: Vec<String> = entries.into_iter().map(|(_, h)| h).collect();
        community_hashes.insert(*community_id, rollup(&child));
    }
    let community_updates: Vec<(Uuid, String)> = community_hashes
        .iter()
        .map(|(id, h)| (*id, h.clone()))
        .collect();
    storage
        .update_community_subtree_hashes(&community_updates)
        .await?;

    // 3. Repo root: sha256 over every file hash, ordered by path. Content-
    //    addressed and independent of community structure, so it is the
    //    cleanest "did anything change" commitment.
    path_hashes.sort();
    let ordered: Vec<String> = path_hashes.into_iter().map(|(_, h)| h).collect();
    let repo_root_hash = rollup(&ordered);
    storage
        .update_repo_root_hash(repo_id, &repo_root_hash)
        .await?;

    Ok(MerkleOutcome {
        repo_root_hash,
        community_hashes,
    })
}

/// Diff two community-hash maps into the set of communities whose root changed
/// (new ids, or existing ids with a different hash) — the feature blast radius.
/// Returns ids sorted for deterministic output.
pub fn changed_communities(
    before: &HashMap<Uuid, String>,
    after: &HashMap<Uuid, String>,
) -> Vec<Uuid> {
    let mut changed: Vec<Uuid> = after
        .iter()
        .filter(|(id, hash)| before.get(*id).map(|b| b != *hash).unwrap_or(true))
        .map(|(id, _)| *id)
        .collect();
    changed.sort();
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollup_is_order_sensitive_and_stable() {
        let a = rollup(&["x".into(), "y".into()]);
        let b = rollup(&["x".into(), "y".into()]);
        assert_eq!(a, b, "same input => same hash");
        let c = rollup(&["y".into(), "x".into()]);
        assert_ne!(a, c, "order matters (callers must canonicalize)");
    }

    #[test]
    fn empty_rollup_is_constant() {
        assert_eq!(rollup(&[]), rollup(&[]));
        assert_eq!(rollup(&[]), hash(""));
    }

    #[test]
    fn changed_communities_detects_new_and_modified() {
        let mut before = HashMap::new();
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let c = Uuid::from_u128(3);
        before.insert(a, "h1".to_string());
        before.insert(b, "h2".to_string());

        let mut after = HashMap::new();
        after.insert(a, "h1".to_string()); // unchanged
        after.insert(b, "h2-new".to_string()); // modified
        after.insert(c, "h3".to_string()); // new

        let changed = changed_communities(&before, &after);
        assert!(changed.contains(&b));
        assert!(changed.contains(&c));
        assert!(!changed.contains(&a));
        assert_eq!(changed.len(), 2);
    }
}
