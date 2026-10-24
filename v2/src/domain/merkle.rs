//! The Merkle tree of the vault. It mirrors the folders:
//!
//! ```text
//! root ("")  ── folder "Health/" ── note "Health/Meds.md" ── block fingerprints
//!            └─ note "inbox.md" ── block fingerprints
//! ```
//!
//! A note's hash covers its non-blank blocks in order (each labelled by its
//! heading path, so moving a fact under another heading changes the note); a
//! folder's hash covers its direct children (folders and notes) sorted by
//! name, each labelled by its name. Equal roots mean equal knowledge — one
//! comparison tells an agent nothing changed. Formatting-only edits keep every
//! hash (fingerprints ignore Markdown symbols).

use super::hash::{self, Digest};
use super::note::{Block, Note, section};
use super::text;
use serde::Serialize;
use std::collections::BTreeMap;

/// Hash of every folder and note, keyed by vault path: `""` is the root,
/// folder keys end with `/` (`"Health/"`), note keys are note paths
/// (`"Health/Meds.md"`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tree {
    nodes: BTreeMap<String, Digest>,
}

/// Notes that differ between two trees, each list sorted by path.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Changes {
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }
}

impl Tree {
    /// Build the tree of `notes` (any order; paths are unique). An empty vault
    /// has only the root, whose hash is `hash::node([])`.
    pub fn build(notes: &[Note]) -> Tree {
        let _ = notes;
        todo!()
    }

    /// Rebuild from stored `(path, hash)` pairs (the index's copy).
    pub fn from_nodes(nodes: impl IntoIterator<Item = (String, Digest)>) -> Tree {
        Tree {
            nodes: nodes.into_iter().collect(),
        }
    }

    /// Every `(path, hash)` pair, sorted by path.
    pub fn nodes(&self) -> impl Iterator<Item = (&str, Digest)> {
        self.nodes.iter().map(|(p, h)| (p.as_str(), *h))
    }

    /// The root hash: one value for the whole knowledge base.
    pub fn root(&self) -> Digest {
        todo!()
    }

    /// Hash of a folder or note, if the tree has it.
    pub fn get(&self, path: &str) -> Option<Digest> {
        self.nodes.get(path).copied()
    }

    /// Notes added, changed or removed going from `self` to `newer`.
    pub fn diff(&self, newer: &Tree) -> Changes {
        let _ = newer;
        todo!()
    }
}

/// A note's hash: its blocks' fingerprints in order, each labelled with its
/// heading path.
pub fn note_hash(blocks: &[Block]) -> Digest {
    let _ = (blocks, section, text::fingerprint);
    todo!()
}
