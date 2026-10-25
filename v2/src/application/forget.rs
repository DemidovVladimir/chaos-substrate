//! `forget`: take a fact out of the vault and the index. Every block with the
//! same words goes, looked up by fingerprint as `remember` looks; a passage
//! that only means the same is never touched.

use super::{Kb, nonblank};
use crate::domain::hash::Digest;
use crate::domain::note::Location;
use crate::domain::path::NotePath;
use crate::domain::text;
use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ForgetReport {
    /// Where the removed blocks sat, sorted. Empty when no block had these
    /// words (nothing written).
    pub removed: Vec<Location>,
    /// Blocks of the text stored nowhere (in `note`, when one is named).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_found: Vec<String>,
    /// Merkle root after the call.
    pub root: Digest,
    /// Chunks embedded by the call: what is left of an edited passage, plus
    /// any pending vault edits.
    pub embedded: usize,
}

impl Kb<'_> {
    /// Remove every block stored with the words of a block of `text`, only
    /// in `note` when one is named.
    pub fn forget(&self, text: &str, note: Option<&NotePath>) -> Result<ForgetReport> {
        let text = nonblank(text, "text")?;
        // Sync first so the stored places are the ones on disk now.
        let before = self.sync(false)?;
        let mut places: Vec<(Location, Digest)> = Vec::new();
        let mut not_found = Vec::new();
        for block in self.vault.texts(text) {
            let Some(fingerprint) = text::fingerprint(&block) else {
                continue;
            };
            let at: Vec<Location> = (self.index.exact(&fingerprint)?.into_iter())
                .filter(|at| note.is_none_or(|n| at.note == *n))
                .collect();
            if at.is_empty() {
                not_found.push(block);
            }
            places.extend(at.into_iter().map(|at| (at, fingerprint)));
        }
        places.sort();
        places.dedup_by(|a, b| a.0 == b.0);
        if places.is_empty() {
            return Ok(ForgetReport {
                removed: Vec::new(),
                not_found,
                root: before.root,
                embedded: before.embedded,
            });
        }
        for group in places.chunk_by(|a, b| a.0.note == b.0.note) {
            let blocks: Vec<([u32; 2], Digest)> =
                group.iter().map(|(at, fp)| (at.lines, *fp)).collect();
            self.vault.remove(&group[0].0.note, &blocks)?;
        }
        let after = self.sync(false)?;
        Ok(ForgetReport {
            removed: places.into_iter().map(|(at, _)| at).collect(),
            not_found,
            root: after.root,
            embedded: before.embedded + after.embedded,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{World, note, sample_notes};
    use super::*;
    use crate::ports::{Index, Vault};

    fn path(raw: &str) -> NotePath {
        NotePath::parse(raw).unwrap()
    }

    fn texts(world: &World, path: &str) -> Vec<String> {
        let note = world.vault.note(path).unwrap();
        note.blocks.into_iter().map(|b| b.text).collect()
    }

    #[test]
    fn the_same_words_are_removed_from_the_vault_and_the_index() {
        let world = World::synced();
        let before = world.index.snapshot().unwrap().unwrap().tree.root();
        let report = world
            .kb()
            .forget("  i CHANGE the engine oil, every 10000 km  ", None)
            .unwrap();
        assert_eq!(report.removed.len(), 1);
        assert_eq!(report.removed[0].note.as_str(), "Car/Maintenance.md");
        assert_eq!(report.removed[0].section, "Oil");
        assert!(report.not_found.is_empty());
        assert_ne!(report.root, before);
        assert_eq!(report.root, world.kb().sync(true).unwrap().root);
        assert_eq!(
            texts(&world, "Car/Maintenance.md"),
            ["Winter tyres go on in November."]
        );
        let fact = text::fingerprint("I change the engine oil every 10000 km.").unwrap();
        assert!(world.index.exact(&fact).unwrap().is_empty());
        assert_eq!(world.index.vector_count(), 3, "its vector is dropped");
    }

    #[test]
    fn words_stored_nowhere_change_nothing() {
        let world = World::synced();
        let before = world.index.snapshot().unwrap().unwrap().tree.root();
        let writes = world.index.writes.get();
        let report = world.kb().forget("I own a motorbike.", None).unwrap();
        assert!(report.removed.is_empty());
        assert_eq!(report.not_found, ["I own a motorbike."]);
        assert_eq!(report.root, before);
        assert_eq!(world.index.writes.get(), writes + 1, "only the first sync");
        assert_eq!(world.vault.notes().unwrap(), sample_notes());
    }

    #[test]
    fn a_meaning_is_never_enough() {
        // The stub embedder would call these the same; forget asks for words.
        let world = World::synced();
        let report = world.kb().forget("engine oil", None).unwrap();
        assert!(report.removed.is_empty());
        assert_eq!(texts(&world, "Car/Maintenance.md").len(), 2);
    }

    #[test]
    fn every_copy_goes_unless_a_note_is_named() {
        let same = "Shared line.";
        let notes = || {
            vec![
                note("a.md", &[(&["Plan"], same), (&["Plan"], "Keep me.")]),
                note("b.md", &[(&[], same)]),
            ]
        };
        let world = World::new(notes());
        let report = world.kb().forget(same, Some(&path("b"))).unwrap();
        let at: Vec<&str> = report.removed.iter().map(|l| l.note.as_str()).collect();
        assert_eq!(at, ["b.md"]);
        assert_eq!(texts(&world, "a.md"), [same, "Keep me."]);

        let world = World::new(notes());
        let report = world.kb().forget(same, None).unwrap();
        let at: Vec<&str> = report.removed.iter().map(|l| l.note.as_str()).collect();
        assert_eq!(at, ["a.md", "b.md"]);
        assert_eq!(texts(&world, "a.md"), ["Keep me."]);
        assert!(texts(&world, "b.md").is_empty());
    }

    #[test]
    fn each_block_of_the_text_is_looked_up_on_its_own() {
        let world = World::synced();
        let both = "Allergy: penicillin.\n\nI own a motorbike.";
        let report = world.kb().forget(both, None).unwrap();
        assert_eq!(report.removed.len(), 1);
        assert_eq!(report.removed[0].note.as_str(), "Health/Allergies.md");
        assert_eq!(report.not_found, ["I own a motorbike."]);
    }

    #[test]
    fn pending_vault_edits_are_synced_first() {
        // A fact typed into the vault but not yet synced can be forgotten.
        let world = World::synced();
        world
            .vault
            .put(note("Inbox.md", &[(&[], "Dentist every six months.")]));
        let report = world
            .kb()
            .forget("Dentist every six months.", None)
            .unwrap();
        assert_eq!(report.removed[0].note.as_str(), "Inbox.md");
        assert!(texts(&world, "Inbox.md").is_empty());
    }

    #[test]
    fn blank_text_is_refused() {
        let world = World::synced();
        for blank in ["", "   ", "--- !!"] {
            let err = world.kb().forget(blank, None).unwrap_err();
            assert!(err.to_string().contains("empty"), "{err}");
        }
    }
}
