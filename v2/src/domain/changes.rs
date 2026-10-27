//! What changed in the vault since the last sync: every note's hash, and the
//! notes added, changed or removed between two sets of them.
//!
//! A note's hash covers its non-blank blocks in order, each labelled by its
//! heading path, so moving a fact under another heading changes the note.
//! Formatting-only edits keep the hash, because fingerprints and heading
//! labels ignore case, spacing and punctuation. Nothing is rolled up above
//! the notes: `sync` needs to know which notes changed, not one value for all.

use super::hash::{self, Digest};
use super::note::{Block, Note};
use super::text;
use serde::Serialize;
use std::collections::BTreeMap;

/// The hash of every note, keyed by note path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NoteHashes {
    notes: BTreeMap<String, Digest>,
}

/// Notes that differ between two sets of note hashes, each list sorted by path.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Changes {
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
}

impl NoteHashes {
    /// The hash of every one of `notes` (any order; paths are unique).
    pub fn build(notes: &[Note]) -> NoteHashes {
        NoteHashes::from_notes(
            notes
                .iter()
                .map(|n| (n.path.to_string(), note_hash(&n.blocks))),
        )
    }

    /// Rebuild from stored `(note path, note hash)` pairs (the index's copy).
    pub fn from_notes(notes: impl IntoIterator<Item = (String, Digest)>) -> NoteHashes {
        NoteHashes {
            notes: notes.into_iter().collect(),
        }
    }

    /// Every `(note path, note hash)` pair, sorted by path.
    pub fn notes(&self) -> impl Iterator<Item = (&str, Digest)> {
        self.notes.iter().map(|(p, h)| (p.as_str(), *h))
    }

    /// Notes added, changed or removed going from `self` to `newer`.
    pub fn diff(&self, newer: &NoteHashes) -> Changes {
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
    hash::list(leaves.iter().map(|(label, leaf)| (label.as_str(), *leaf)))
}

/// A block's heading path as a hash label: each heading normalized like
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

    fn keys(hashes: &NoteHashes) -> Vec<&str> {
        hashes.notes().map(|(p, _)| p).collect()
    }

    #[test]
    fn note_hash_labels_fingerprints_with_their_section() {
        let blocks = vec![
            block(&[], "Intro line"),
            block(&["Car", "Oil"], "Change every 10k km"),
        ];
        let expected = hash::list([
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
        assert_eq!(note_hash(&[]), hash::list([]));
        assert_eq!(note_hash(&[block(&["A"], "** --")]), hash::list([]));
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
    fn moving_a_block_under_another_heading_changes_the_note() {
        let before = vec![note("Car.md", vec![block(&["Oil"], "Castrol 5W-30")])];
        let after = vec![note("Car.md", vec![block(&["Tyres"], "Castrol 5W-30")])];
        let (old, new) = (NoteHashes::build(&before), NoteHashes::build(&after));
        assert_eq!(old.diff(&new).changed, ["Car.md"]);
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
        assert_eq!(NoteHashes::build(&before), NoteHashes::build(&after));
    }

    #[test]
    fn build_keys_every_note_hash_by_path() {
        let plan = note("Health/Treatment/Plan.md", vec![block(&["Week 1"], "rest")]);
        let meds = one("Health/Meds.md", "vitamin d");
        let inbox = one("inbox.md", "buy milk");
        let hashes = NoteHashes::build(&[inbox.clone(), plan.clone(), meds.clone()]);
        assert_eq!(
            keys(&hashes),
            ["Health/Meds.md", "Health/Treatment/Plan.md", "inbox.md"]
        );
        let h = |n: &Note| note_hash(&n.blocks);
        let stored: Vec<Digest> = hashes.notes().map(|(_, d)| d).collect();
        assert_eq!(stored, [h(&meds), h(&plan), h(&inbox)]);
    }

    #[test]
    fn build_ignores_input_order() {
        let notes = vec![one("b.md", "b"), one("x/a.md", "a"), one("a.md", "c")];
        let mut reversed = notes.clone();
        reversed.reverse();
        assert_eq!(NoteHashes::build(&notes), NoteHashes::build(&reversed));
    }

    #[test]
    fn renaming_a_note_keeps_its_hash_but_shows_as_removed_and_added() {
        let old = NoteHashes::build(&[one("inbox.md", "buy milk")]);
        let new = NoteHashes::build(&[one("todo.md", "buy milk")]);
        assert_eq!(old.notes().next().unwrap().1, new.notes().next().unwrap().1);
        let changes = old.diff(&new);
        assert_eq!(
            (changes.added, changes.removed),
            (vec!["todo.md".to_string()], vec!["inbox.md".to_string()])
        );
    }

    #[test]
    fn editing_a_note_changes_that_note_only() {
        let notes = |dose: &str| {
            vec![
                one("Health/Treatment/Plan.md", dose),
                one("Health/Diet.md", "no sugar"),
                one("inbox.md", "buy milk"),
            ]
        };
        let (old, new) = (
            NoteHashes::build(&notes("10 mg")),
            NoteHashes::build(&notes("20 mg")),
        );
        assert_eq!(old.diff(&new).changed, ["Health/Treatment/Plan.md"]);
        assert_eq!(old.diff(&old), Changes::default());
    }

    #[test]
    fn from_notes_round_trips() {
        let hashes = NoteHashes::build(&[
            one("Health/Treatment/Plan.md", "rest"),
            one("inbox.md", "buy milk"),
        ]);
        let copy = NoteHashes::from_notes(hashes.notes().map(|(p, h)| (p.to_string(), h)));
        assert_eq!(copy, hashes);
    }

    #[test]
    fn diff_lists_added_changed_and_removed_notes_sorted() {
        let old = NoteHashes::build(&[
            one("a.md", "gone"),
            one("b.md", "old text"),
            one("Health/c.md", "same"),
            one("Health/d.md", "gone too"),
        ]);
        let new = NoteHashes::build(&[
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
        let hashes = NoteHashes::build(&[one("b.md", "b"), one("A/a.md", "a")]);
        let changes = NoteHashes::default().diff(&hashes);
        assert_eq!(changes.added, ["A/a.md", "b.md"]);
        assert!(changes.changed.is_empty() && changes.removed.is_empty());
        let back = hashes.diff(&NoteHashes::build(&[]));
        assert_eq!(back.removed, ["A/a.md", "b.md"]);
        assert!(back.added.is_empty() && back.changed.is_empty());
    }
}
