//! L2 hash roll-up (Merkle) over chunk `content_hash` leaves.
//!
//! ```text
//! chunk.content_hash ──▶ file hash ──▶ community subtree_hash
//!                                  └──▶ repo root_hash
//! ```
//!
//! Every level hashes a *canonically ordered* list of child hashes, so the
//! result depends only on content — never on regenerated ids. Communities whose
//! `subtree_hash` did not move keep their summaries; a repo whose `root_hash`
//! did not move relinks nothing.

use crate::domain::hash;
use std::collections::HashMap;

/// Per-file hash from `(file path, chunk content_hash)` leaves: each file's
/// chunk hashes are sorted and joined, so chunk order in the input is irrelevant.
pub fn file_hashes(leaves: &[(String, String)]) -> HashMap<String, String> {
    let mut by_file: HashMap<&str, Vec<String>> = HashMap::new();
    for (path, chunk_hash) in leaves {
        by_file
            .entry(path.as_str())
            .or_default()
            .push(chunk_hash.clone());
    }
    by_file
        .into_iter()
        .map(|(path, mut hashes)| {
            hashes.sort();
            hashes.dedup();
            (path.to_string(), hash::combine(&hashes))
        })
        .collect()
}

/// Subtree hash of a community from its member file paths (any order). Files
/// without a known hash are skipped so a community over unchunked files still
/// gets a stable value.
pub fn community_hash(files: &[String], file_hashes: &HashMap<String, String>) -> String {
    let mut entries: Vec<(&String, &String)> = files
        .iter()
        .filter_map(|f| file_hashes.get(f).map(|h| (f, h)))
        .collect();
    entries.sort();
    entries.dedup();
    let ordered: Vec<String> = entries.into_iter().map(|(_, h)| h.clone()).collect();
    hash::combine(&ordered)
}

/// Repository root hash: every file hash ordered by path.
pub fn root_hash(file_hashes: &HashMap<String, String>) -> String {
    let mut entries: Vec<(&String, &String)> = file_hashes.iter().collect();
    entries.sort();
    let ordered: Vec<String> = entries.into_iter().map(|(_, h)| h.clone()).collect();
    hash::combine(&ordered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(p, h)| (p.to_string(), h.to_string()))
            .collect()
    }

    #[test]
    fn file_hashes_ignore_chunk_order() {
        let a = file_hashes(&leaves(&[("a.rs", "h1"), ("a.rs", "h2"), ("b.rs", "h3")]));
        let b = file_hashes(&leaves(&[("b.rs", "h3"), ("a.rs", "h2"), ("a.rs", "h1")]));
        assert_eq!(a, b);
        assert_eq!(a.len(), 2);
        assert_ne!(a["a.rs"], a["b.rs"]);
    }

    #[test]
    fn community_hash_is_order_insensitive_and_content_sensitive() {
        let fh = file_hashes(&leaves(&[("a.rs", "h1"), ("b.rs", "h2")]));
        let x = community_hash(&["a.rs".into(), "b.rs".into()], &fh);
        let y = community_hash(&["b.rs".into(), "a.rs".into(), "missing.rs".into()], &fh);
        assert_eq!(x, y);
        let fh2 = file_hashes(&leaves(&[("a.rs", "h1-changed"), ("b.rs", "h2")]));
        assert_ne!(x, community_hash(&["a.rs".into(), "b.rs".into()], &fh2));
    }

    #[test]
    fn root_hash_moves_only_with_content() {
        let fh = file_hashes(&leaves(&[("a.rs", "h1"), ("b.rs", "h2")]));
        assert_eq!(root_hash(&fh), root_hash(&fh.clone()));
        let fh2 = file_hashes(&leaves(&[("a.rs", "h1"), ("b.rs", "h2"), ("c.rs", "h3")]));
        assert_ne!(root_hash(&fh), root_hash(&fh2));
        assert_eq!(root_hash(&HashMap::new()), hash::combine(&[]));
    }
}
