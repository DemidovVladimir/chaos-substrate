//! The Merkle tree of the vault, three levels deep:
//!
//! ```text
//! root ── note "Health/Meds.md" ── block fingerprints (the leaves)
//!      └─ note "inbox.md"       ── block fingerprints
//! ```
//!
//! A note's hash covers its non-blank blocks in order, each labelled by its
//! heading path, so moving a fact under another heading changes the note. The
//! root covers every note hash, labelled by the note's path. Equal roots mean
//! equal knowledge: one comparison tells an agent nothing changed.
//! Formatting-only edits keep every hash, because fingerprints and heading
//! labels ignore case, spacing and punctuation.

use super::hash::{self, Digest};
use super::note::{Block, Note};
use super::text;
use serde::Serialize;
use std::collections::BTreeMap;

/// The hash of every note, keyed by note path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tree {
    notes: BTreeMap<String, Digest>,
}

/// Notes that differ between two trees, each list sorted by path.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Changes {
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
}

impl Tree {
    /// Build the tree of `notes` (any order; paths are unique).
    pub fn build(notes: &[Note]) -> Tree {
        Tree::from_notes(
            notes
                .iter()
                .map(|n| (n.path.to_string(), note_hash(&n.blocks))),
        )
    }

    /// Rebuild from stored `(note path, note hash)` pairs (the index's copy).
    pub fn from_notes(notes: impl IntoIterator<Item = (String, Digest)>) -> Tree {
        Tree {
            notes: notes.into_iter().collect(),
        }
    }

    /// Every `(note path, note hash)` pair, sorted by path.
    pub fn notes(&self) -> impl Iterator<Item = (&str, Digest)> {
        self.notes.iter().map(|(p, h)| (p.as_str(), *h))
    }

    /// The root hash: one value for the whole knowledge base. An empty vault
    /// has the root `hash::node([])`.
    pub fn root(&self) -> Digest {
        hash::node(self.notes())
    }

    /// Notes added, changed or removed going from `self` to `newer`.
    pub fn diff(&self, newer: &Tree) -> Changes {
        let mut changes = Changes::default();
        for (path, hash) in newer.notes() {
            match self.notes.get(path) {
                None => changes.added.push(path.to_string()),
                Some(old) if *old != hash => changes.changed.push(path.to_string()),
                Some(_) => {}
            }
        }
        changes.removed = self
            .notes()
            .filter(|(path, _)| !newer.notes.contains_key(*path))
            .map(|(path, _)| path.to_string())
            .collect();
        changes
    }
}

/// A note's hash: its blocks' fingerprints in order, each labelled with its
/// heading path.
pub fn note_hash(blocks: &[Block]) -> Digest {
    let leaves: Vec<(String, Digest)> = blocks
        .iter()
        .filter_map(|b| Some((heading_label(&b.headings), text::fingerprint(&b.text)?)))
        .collect();
    hash::node(leaves.iter().map(|(label, leaf)| (label.as_str(), *leaf)))
}

/// A block's heading path as a Merkle label: each heading normalized like
/// block text (so `**Oil:**` labels like `Oil`), joined by a newline, which a
/// normalized heading never contains, so nesting stays unambiguous.
fn heading_label(headings: &[String]) -> String {
    let words: Vec<String> = headings.iter().map(|h| text::normalize(h)).collect();
    words.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::path::NotePath;

    fn block(headings: &[&str], text: &str) -> Block {
        Block {
            headings: headings.iter().map(|h| h.to_string()).collect(),
            text: text.to_string(),
            source: text.to_string(),
            lines: [1, 1],
        }
    }

    fn note(path: &str, blocks: Vec<Block>) -> Note {
        Note {
            path: NotePath::parse(path).unwrap(),
            blocks,
        }
    }

    fn one(path: &str, text: &str) -> Note {
        note(path, vec![block(&[], text)])
    }

    fn keys(tree: &Tree) -> Vec<&str> {
        tree.notes().map(|(p, _)| p).collect()
    }

    #[test]
    fn note_hash_labels_fingerprints_with_their_section() {
        let blocks = vec![
            block(&[], "Intro line"),
            block(&["Car", "Oil"], "Change every 10k km"),
        ];
        let expected = hash::node([
            ("", text::fingerprint("Intro line").unwrap()),
            (
                "car\noil",
                text::fingerprint("Change every 10k km").unwrap(),
            ),
        ]);
        assert_eq!(note_hash(&blocks), expected);
    }

    #[test]
    fn heading_labels_ignore_formatting_but_keep_nesting() {
        let fact = "Change every 10k km";
        let plain = note_hash(&[block(&["Car", "Oil"], fact)]);
        assert_eq!(note_hash(&[block(&["**car**", "Oil:"], fact)]), plain);
        assert_ne!(
            note_hash(&[block(&["Car > Oil"], fact)]),
            plain,
            "one heading named `Car > Oil` is not two nested headings"
        );
    }

    #[test]
    fn note_hash_skips_blank_blocks_and_keeps_order() {
        let (a, b) = (block(&["A"], "first fact"), block(&["A"], "second fact"));
        let plain = note_hash(&[a.clone(), b.clone()]);
        assert_eq!(
            note_hash(&[block(&["A"], "---"), a.clone(), block(&[], ""), b.clone()]),
            plain
        );
        assert_ne!(note_hash(&[b, a]), plain, "order is part of the note");
        assert_eq!(note_hash(&[]), hash::node([]));
        assert_eq!(note_hash(&[block(&["A"], "** --")]), hash::node([]));
    }

    #[test]
    fn note_hash_ignores_formatting_source_and_lines() {
        let plain = note_hash(&[block(&["Docs"], "Driving licence: B (2015)")]);
        let mut formatted = block(&["**Docs:**"], "driving licence — b, 2015.");
        formatted.source = "- **Driving licence:** _B_ (2015).".to_string();
        formatted.lines = [40, 41];
        assert_eq!(note_hash(&[formatted]), plain);
    }

    #[test]
    fn moving_a_block_under_another_heading_changes_note_and_root() {
        let before = vec![note("Car.md", vec![block(&["Oil"], "Castrol 5W-30")])];
        let after = vec![note("Car.md", vec![block(&["Tyres"], "Castrol 5W-30")])];
        let (old, new) = (Tree::build(&before), Tree::build(&after));
        assert_ne!(old.diff(&new).changed, Vec::<String>::new());
        assert_ne!(old.root(), new.root());
    }

    #[test]
    fn formatting_only_edits_keep_every_hash() {
        let before = vec![
            note(
                "Health/Meds.md",
                vec![block(&["Daily"], "Vitamin D 2000 IU")],
            ),
            one("inbox.md", "call the garage"),
        ];
        let after = vec![
            note(
                "Health/Meds.md",
                vec![block(&["**Daily:**"], "Vitamin D — 2000 IU.")],
            ),
            one("inbox.md", "Call the GARAGE!"),
        ];
        assert_eq!(Tree::build(&before), Tree::build(&after));
    }

    #[test]
    fn the_root_covers_every_note_hash_by_path() {
        let plan = note("Health/Treatment/Plan.md", vec![block(&["Week 1"], "rest")]);
        let meds = one("Health/Meds.md", "vitamin d");
        let inbox = one("inbox.md", "buy milk");
        let tree = Tree::build(&[inbox.clone(), plan.clone(), meds.clone()]);
        assert_eq!(
            keys(&tree),
            ["Health/Meds.md", "Health/Treatment/Plan.md", "inbox.md"]
        );
        let h = |n: &Note| note_hash(&n.blocks);
        let root = hash::node([
            ("Health/Meds.md", h(&meds)),
            ("Health/Treatment/Plan.md", h(&plan)),
            ("inbox.md", h(&inbox)),
        ]);
        assert_eq!(tree.root(), root);
    }

    #[test]
    fn build_ignores_input_order() {
        let notes = vec![one("b.md", "b"), one("x/a.md", "a"), one("a.md", "c")];
        let mut reversed = notes.clone();
        reversed.reverse();
        assert_eq!(Tree::build(&notes), Tree::build(&reversed));
    }

    #[test]
    fn empty_vault_has_the_empty_root() {
        let tree = Tree::build(&[]);
        assert_eq!(tree.root(), hash::node([]));
        assert_eq!(tree.notes().count(), 0);
        assert_eq!(Tree::default().root(), hash::node([]));
    }

    #[test]
    fn renaming_a_note_changes_the_root() {
        let old = Tree::build(&[one("inbox.md", "buy milk")]);
        let new = Tree::build(&[one("todo.md", "buy milk")]);
        assert_eq!(old.notes().next().unwrap().1, new.notes().next().unwrap().1);
        assert_ne!(old.root(), new.root());
    }

    #[test]
    fn editing_a_note_changes_that_note_and_the_root_only() {
        let notes = |dose: &str| {
            vec![
                one("Health/Treatment/Plan.md", dose),
                one("Health/Diet.md", "no sugar"),
                one("inbox.md", "buy milk"),
            ]
        };
        let (old, new) = (Tree::build(&notes("10 mg")), Tree::build(&notes("20 mg")));
        assert_ne!(old.root(), new.root());
        assert_eq!(old.diff(&new).changed, ["Health/Treatment/Plan.md"]);
    }

    #[test]
    fn from_notes_round_trips() {
        let tree = Tree::build(&[
            one("Health/Treatment/Plan.md", "rest"),
            one("inbox.md", "buy milk"),
        ]);
        let copy = Tree::from_notes(tree.notes().map(|(p, h)| (p.to_string(), h)));
        assert_eq!(copy, tree);
        assert_eq!(copy.root(), tree.root());
    }

    #[test]
    fn diff_lists_added_changed_and_removed_notes_sorted() {
        let old = Tree::build(&[
            one("a.md", "gone"),
            one("b.md", "old text"),
            one("Health/c.md", "same"),
            one("Health/d.md", "gone too"),
        ]);
        let new = Tree::build(&[
            one("z.md", "new"),
            one("b.md", "new text"),
            one("Health/c.md", "same"),
            one("Health/e.md", "new too"),
        ]);
        let changes = old.diff(&new);
        assert_eq!(changes.added, ["Health/e.md", "z.md"]);
        assert_eq!(changes.changed, ["b.md"]);
        assert_eq!(changes.removed, ["Health/d.md", "a.md"]);
    }

    #[test]
    fn diff_from_nothing_adds_every_note() {
        let tree = Tree::build(&[one("b.md", "b"), one("A/a.md", "a")]);
        let changes = Tree::default().diff(&tree);
        assert_eq!(changes.added, ["A/a.md", "b.md"]);
        assert!(changes.changed.is_empty() && changes.removed.is_empty());
        let back = tree.diff(&Tree::build(&[]));
        assert_eq!(back.removed, ["A/a.md", "b.md"]);
        assert!(back.added.is_empty() && back.changed.is_empty());
    }
}
