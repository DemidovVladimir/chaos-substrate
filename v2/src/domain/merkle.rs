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
use std::cmp::Reverse;
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
        let mut nodes = BTreeMap::new();
        // Direct children of every folder (root `""` included), by name.
        let mut folders: BTreeMap<String, BTreeMap<String, Digest>> = BTreeMap::new();
        folders.insert(String::new(), BTreeMap::new());
        for note in notes {
            for folder in note.path.folders() {
                folders.entry(format!("{folder}/")).or_default();
            }
            let hash = note_hash(&note.blocks);
            adopt(&mut folders, note.path.as_str(), hash);
            nodes.insert(note.path.to_string(), hash);
        }
        // Deepest first, so every folder is final before its parent is hashed.
        let mut keys: Vec<String> = folders.keys().cloned().collect();
        keys.sort_by_key(|key| Reverse(key.matches('/').count()));
        for key in keys {
            let hash = hash::node(folders[&key].iter().map(|(name, h)| (name.as_str(), *h)));
            if let Some(folder) = key.strip_suffix('/') {
                adopt(&mut folders, folder, hash);
            }
            nodes.insert(key, hash);
        }
        Tree { nodes }
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
        self.get("").unwrap_or_else(|| hash::node([]))
    }

    /// Hash of a folder or note, if the tree has it.
    pub fn get(&self, path: &str) -> Option<Digest> {
        self.nodes.get(path).copied()
    }

    /// Notes added, changed or removed going from `self` to `newer`.
    pub fn diff(&self, newer: &Tree) -> Changes {
        let mut changes = Changes::default();
        for (path, hash) in newer.notes() {
            match self.get(path) {
                None => changes.added.push(path.to_string()),
                Some(old) if old != hash => changes.changed.push(path.to_string()),
                Some(_) => {}
            }
        }
        changes.removed = self
            .notes()
            .filter(|(path, _)| newer.get(path).is_none())
            .map(|(path, _)| path.to_string())
            .collect();
        changes
    }

    /// Note entries only: neither the root nor a folder.
    fn notes(&self) -> impl Iterator<Item = (&str, Digest)> {
        self.nodes()
            .filter(|(path, _)| !path.is_empty() && !path.ends_with('/'))
    }
}

/// Record `hash` as a child of the folder holding `path` (a note path, or a
/// folder path without its slash), labelled by its own name:
/// `"a/b/c.md"` goes to `"a/b/"` as `"c.md"`, `"a"` to the root as `"a"`.
fn adopt(folders: &mut BTreeMap<String, BTreeMap<String, Digest>>, path: &str, hash: Digest) {
    let (parent, name) = match path.rfind('/') {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    };
    folders
        .entry(parent.to_string())
        .or_default()
        .insert(name.to_string(), hash);
}

/// A note's hash: its blocks' fingerprints in order, each labelled with its
/// heading path.
pub fn note_hash(blocks: &[Block]) -> Digest {
    let leaves: Vec<(String, Digest)> = blocks
        .iter()
        .filter(|block| !text::is_blank(&block.text))
        .map(|block| (section(&block.headings), text::fingerprint(&block.text)))
        .collect();
    hash::node(leaves.iter().map(|(label, leaf)| (label.as_str(), *leaf)))
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
        tree.nodes().map(|(p, _)| p).collect()
    }

    #[test]
    fn note_hash_labels_fingerprints_with_their_section() {
        let blocks = vec![
            block(&[], "Intro line"),
            block(&["Car", "Oil"], "Change every 10k km"),
        ];
        let expected = hash::node([
            ("", text::fingerprint("Intro line")),
            ("Car > Oil", text::fingerprint("Change every 10k km")),
        ]);
        assert_eq!(note_hash(&blocks), expected);
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
        let mut formatted = block(&["Docs"], "- **Driving licence:** _B_ (2015).");
        formatted.source = "- **Driving licence:** _B_ (2015).".to_string();
        formatted.lines = [40, 41];
        assert_eq!(note_hash(&[formatted]), plain);
    }

    #[test]
    fn moving_a_block_under_another_heading_changes_note_and_root() {
        let before = vec![note("Car.md", vec![block(&["Oil"], "Castrol 5W-30")])];
        let after = vec![note("Car.md", vec![block(&["Tyres"], "Castrol 5W-30")])];
        let (old, new) = (Tree::build(&before), Tree::build(&after));
        assert_ne!(old.get("Car.md"), new.get("Car.md"));
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
                vec![block(&["Daily"], "- **Vitamin D** — 2000 IU.")],
            ),
            one("inbox.md", "* [ ] Call the GARAGE!"),
        ];
        assert_eq!(Tree::build(&before), Tree::build(&after));
    }

    #[test]
    fn build_hashes_folders_from_children_sorted_by_name() {
        let plan = note("Health/Treatment/Plan.md", vec![block(&["Week 1"], "rest")]);
        let meds = one("Health/Meds.md", "vitamin d");
        let folder_note = one("Health.md", "about my health");
        let inbox = one("inbox.md", "buy milk");
        let tree = Tree::build(&[
            inbox.clone(),
            plan.clone(),
            folder_note.clone(),
            meds.clone(),
        ]);

        assert_eq!(
            keys(&tree),
            [
                "",
                "Health.md",
                "Health/",
                "Health/Meds.md",
                "Health/Treatment/",
                "Health/Treatment/Plan.md",
                "inbox.md",
            ]
        );
        let h = |n: &Note| note_hash(&n.blocks);
        assert_eq!(tree.get("Health/Meds.md"), Some(h(&meds)));
        let treatment = hash::node([("Plan.md", h(&plan))]);
        assert_eq!(tree.get("Health/Treatment/"), Some(treatment));
        let health = hash::node([("Meds.md", h(&meds)), ("Treatment", treatment)]);
        assert_eq!(tree.get("Health/"), Some(health));
        // By name, the folder "Health" sorts before the note "Health.md".
        let root = hash::node([
            ("Health", health),
            ("Health.md", h(&folder_note)),
            ("inbox.md", h(&inbox)),
        ]);
        assert_eq!(tree.get(""), Some(root));
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
    fn empty_vault_has_only_the_empty_root() {
        let tree = Tree::build(&[]);
        assert_eq!(tree.root(), hash::node([]));
        assert_eq!(tree.nodes().collect::<Vec<_>>(), [("", hash::node([]))]);
        assert_eq!(Tree::default().root(), hash::node([]));
        assert_eq!(Tree::from_nodes([]).root(), hash::node([]));
    }

    #[test]
    fn renaming_a_note_changes_the_root() {
        let old = Tree::build(&[one("inbox.md", "buy milk")]);
        let new = Tree::build(&[one("todo.md", "buy milk")]);
        assert_eq!(old.get("inbox.md"), new.get("todo.md"));
        assert_ne!(old.root(), new.root());
    }

    #[test]
    fn editing_a_note_changes_its_folders_and_the_root_only() {
        let notes = |dose: &str| {
            vec![
                one("Health/Treatment/Plan.md", dose),
                one("Health/Diet.md", "no sugar"),
                one("Car/Oil.md", "5W-30"),
                one("inbox.md", "buy milk"),
            ]
        };
        let old = Tree::build(&notes("10 mg"));
        let new = Tree::build(&notes("20 mg"));
        for changed in [
            "",
            "Health/",
            "Health/Treatment/",
            "Health/Treatment/Plan.md",
        ] {
            assert_ne!(old.get(changed), new.get(changed), "{changed:?} changes");
        }
        for same in ["Health/Diet.md", "Car/", "Car/Oil.md", "inbox.md"] {
            assert!(old.get(same).is_some());
            assert_eq!(old.get(same), new.get(same), "{same:?} stays");
        }
    }

    #[test]
    fn from_nodes_round_trips() {
        let tree = Tree::build(&[
            one("Health/Treatment/Plan.md", "rest"),
            one("inbox.md", "buy milk"),
        ]);
        let copy = Tree::from_nodes(tree.nodes().map(|(p, h)| (p.to_string(), h)));
        assert_eq!(copy, tree);
        assert_eq!(copy.root(), tree.root());
    }

    #[test]
    fn get_knows_folder_keys_by_their_slash() {
        let tree = Tree::build(&[one("Health/Meds.md", "vitamin d")]);
        assert!(tree.get("Health/").is_some());
        assert_eq!(tree.get("Health"), None);
        assert_eq!(tree.get("Meds.md"), None);
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
        assert!(!changes.is_empty());
    }

    #[test]
    fn diff_ignores_unchanged_notes_and_folder_keys() {
        let tree = Tree::build(&[one("Health/c.md", "same"), one("inbox.md", "x")]);
        assert!(tree.diff(&tree.clone()).is_empty());
        let (h1, h2) = (hash::leaf(b"1"), hash::leaf(b"2"));
        let old = Tree::from_nodes([(String::new(), h1), ("x/".to_string(), h1)]);
        let new = Tree::from_nodes([(String::new(), h2), ("x/".to_string(), h2)]);
        assert_eq!(old.diff(&new), Changes::default());
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
