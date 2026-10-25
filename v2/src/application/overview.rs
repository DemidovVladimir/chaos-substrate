//! `overview`: what the dashboard draws. The Merkle tree of the notes as they
//! are on disk now, next to the tree the index holds: every note with both
//! hashes and every leaf (a block's fingerprint) with whether the index has
//! it. It reads only and embeds nothing.

use super::Kb;
use crate::domain::hash::Digest;
use crate::domain::merkle::{Changes, Tree, note_hash};
use crate::domain::note::section;
use crate::domain::text;
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Overview {
    /// Merkle root of the notes on disk now.
    pub root: Digest,
    /// Merkle root the index holds; `None` before the first sync.
    pub indexed_root: Option<Digest>,
    /// The embedder configured now.
    pub embedder: String,
    /// The embedder of the stored vectors; `None` before the first sync.
    pub indexed_embedder: Option<String>,
    /// What the next sync would take in: notes added, changed or removed
    /// since the index was built.
    pub pending: Changes,
    /// Passages the index holds, and their stored vectors.
    pub chunks: usize,
    pub vectors: usize,
    /// Every note on disk, then every note the index still holds that is gone
    /// from disk, sorted by path.
    pub notes: Vec<NoteNode>,
}

/// One note of the tree: an interior node over its leaves.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NoteNode {
    pub path: String,
    pub state: NoteState,
    /// Its hash now; `None` when the note is gone from disk.
    pub hash: Option<Digest>,
    /// The hash the index holds; `None` when the index does not hold it.
    pub indexed_hash: Option<Digest>,
    /// Passages of the note the index holds.
    pub chunks: usize,
    /// Its blocks with words, in reading order: the leaves of the tree.
    pub leaves: Vec<Leaf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NoteState {
    /// The index holds the note as it is on disk.
    Indexed,
    /// Edited since the last sync.
    Changed,
    /// On disk, not in the index yet.
    New,
    /// Gone from disk; the index holds it until the next sync.
    Removed,
}

/// One leaf: a block and its fingerprint.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Leaf {
    pub section: String,
    pub lines: [u32; 2],
    pub fingerprint: Digest,
    /// The plain words that were fingerprinted.
    pub text: String,
    /// The Markdown as written in the note.
    pub source: String,
    /// Whether the index holds these words in this note and section.
    pub indexed: bool,
}

impl Kb<'_> {
    pub fn overview(&self) -> Result<Overview> {
        let notes = self.vault.notes()?;
        let tree = Tree::build(&notes);
        let snapshot = self.index.snapshot()?;
        let stored = snapshot
            .as_ref()
            .map_or_else(Tree::default, |s| s.tree.clone());
        let indexed: BTreeMap<&str, Digest> = stored.notes().collect();
        let mut chunks: BTreeMap<String, usize> = BTreeMap::new();
        let all = self.index.chunks()?;
        for c in &all {
            *chunks.entry(c.at.note.to_string()).or_default() += 1;
        }
        let count = |path: &str| chunks.get(path).copied().unwrap_or(0);
        let mut nodes = Vec::with_capacity(notes.len());
        for note in &notes {
            let path = note.path.as_str();
            let hash = note_hash(&note.blocks);
            let state = match indexed.get(path) {
                None => NoteState::New,
                Some(h) if *h == hash => NoteState::Indexed,
                Some(_) => NoteState::Changed,
            };
            let mut leaves = Vec::new();
            for block in &note.blocks {
                let Some(fingerprint) = text::fingerprint(&block.text) else {
                    continue;
                };
                let section = section(&block.headings);
                let indexed = match state {
                    NoteState::Indexed => true,
                    NoteState::New | NoteState::Removed => false,
                    NoteState::Changed => (self.index.exact(&fingerprint)?.iter())
                        .any(|at| at.note == note.path && at.section == section),
                };
                leaves.push(Leaf {
                    section,
                    lines: block.lines,
                    fingerprint,
                    text: block.text.clone(),
                    source: block.source.clone(),
                    indexed,
                });
            }
            nodes.push(NoteNode {
                path: path.to_string(),
                state,
                hash: Some(hash),
                indexed_hash: indexed.get(path).copied(),
                chunks: count(path),
                leaves,
            });
        }
        let pending = stored.diff(&tree);
        for path in &pending.removed {
            nodes.push(NoteNode {
                path: path.clone(),
                state: NoteState::Removed,
                hash: None,
                indexed_hash: indexed.get(path.as_str()).copied(),
                chunks: count(path),
                leaves: Vec::new(),
            });
        }
        Ok(Overview {
            root: tree.root(),
            indexed_root: snapshot.as_ref().map(|s| s.tree.root()),
            embedder: self.embedder.id().to_string(),
            indexed_embedder: snapshot.map(|s| s.embedder),
            pending,
            chunks: all.len(),
            vectors: self.index.vector_keys()?.len(),
            notes: nodes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{World, note, sample_notes};
    use super::*;
    use crate::ports::Vault;

    fn node<'o>(overview: &'o Overview, path: &str) -> &'o NoteNode {
        let found = overview.notes.iter().find(|n| n.path == path);
        found.unwrap_or_else(|| panic!("no {path} in {overview:#?}"))
    }

    /// `(text, indexed)` of every leaf of a note.
    fn leaves<'o>(overview: &'o Overview, path: &str) -> Vec<(&'o str, bool)> {
        let leaves = node(overview, path).leaves.iter();
        leaves.map(|l| (l.text.as_str(), l.indexed)).collect()
    }

    #[test]
    fn before_the_first_sync_every_note_is_new() {
        let world = World::new(sample_notes());
        let o = world.kb().overview().unwrap();
        assert_eq!(o.root, Tree::build(&sample_notes()).root());
        assert_eq!(o.indexed_root, None);
        assert_eq!(o.indexed_embedder, None);
        assert_eq!(o.embedder, "stub|v1");
        assert_eq!(o.pending.added.len(), 3);
        assert_eq!((o.chunks, o.vectors), (0, 0));
        let paths: Vec<&str> = o.notes.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(
            paths,
            ["Car/Maintenance.md", "Education.md", "Health/Allergies.md"]
        );
        for n in &o.notes {
            assert_eq!(n.state, NoteState::New, "{}", n.path);
            assert!(n.hash.is_some() && n.indexed_hash.is_none());
            assert!(n.leaves.iter().all(|l| !l.indexed));
        }
    }

    #[test]
    fn after_a_sync_the_index_holds_every_leaf() {
        let world = World::synced();
        let o = world.kb().overview().unwrap();
        assert_eq!(o.indexed_root, Some(o.root));
        assert_eq!(o.indexed_embedder.as_deref(), Some("stub|v1"));
        assert_eq!(o.pending, Changes::default());
        assert_eq!((o.chunks, o.vectors), (4, 4));
        let car = node(&o, "Car/Maintenance.md");
        assert_eq!(car.state, NoteState::Indexed);
        assert_eq!(car.hash, car.indexed_hash);
        assert_eq!(car.chunks, 2);
        assert_eq!(
            leaves(&o, "Car/Maintenance.md"),
            [
                ("I change the engine oil every 10000 km.", true),
                ("Winter tyres go on in November.", true)
            ]
        );
        let oil = &car.leaves[0];
        assert_eq!(oil.section, "Oil");
        assert_eq!(oil.lines, [1, 1]);
        assert_eq!(
            Some(oil.fingerprint),
            crate::domain::text::fingerprint(&oil.text)
        );
    }

    #[test]
    fn note_hashes_are_the_merkle_nodes() {
        let world = World::synced();
        let o = world.kb().overview().unwrap();
        for n in world.vault.notes().unwrap() {
            assert_eq!(node(&o, n.path.as_str()).hash, Some(note_hash(&n.blocks)));
        }
    }

    #[test]
    fn edits_not_yet_synced_show_leaf_by_leaf() {
        let world = World::synced();
        world.vault.put(note(
            "Car/Maintenance.md",
            &[
                (&["Oil"], "I change the engine oil every 10000 km."),
                (&["Tyres"], "Summer tyres go on in April."),
            ],
        ));
        world.vault.remove("Education.md");
        world
            .vault
            .put(note("Inbox.md", &[(&[], "Call the garage."), (&[], "--")]));
        let o = world.kb().overview().unwrap();
        assert_ne!(o.indexed_root, Some(o.root));
        assert_eq!(o.pending.changed, ["Car/Maintenance.md"]);
        assert_eq!(o.pending.added, ["Inbox.md"]);
        assert_eq!(o.pending.removed, ["Education.md"]);

        let car = node(&o, "Car/Maintenance.md");
        assert_eq!(car.state, NoteState::Changed);
        assert_ne!(car.hash, car.indexed_hash);
        assert_eq!(
            leaves(&o, "Car/Maintenance.md"),
            [
                ("I change the engine oil every 10000 km.", true),
                ("Summer tyres go on in April.", false)
            ]
        );
        let inbox = node(&o, "Inbox.md");
        assert_eq!(inbox.state, NoteState::New);
        assert_eq!(
            leaves(&o, "Inbox.md"),
            [("Call the garage.", false)],
            "blank blocks are no leaves"
        );

        let gone = node(&o, "Education.md");
        assert_eq!(gone.state, NoteState::Removed);
        assert_eq!(gone.hash, None);
        assert!(gone.indexed_hash.is_some());
        assert_eq!(gone.chunks, 1);
        assert!(gone.leaves.is_empty());
        let last = o.notes.last().unwrap();
        assert_eq!(last.path, "Education.md", "removed notes come last");
    }

    #[test]
    fn embeds_nothing() {
        let world = World::synced();
        let (queries, embedded) = (world.embedder.queries.get(), world.embedder.embedded());
        world.kb().overview().unwrap();
        assert_eq!(world.embedder.queries.get(), queries);
        assert_eq!(world.embedder.embedded(), embedded);
    }
}
