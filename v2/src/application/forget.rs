//! `forget`: take a fact out of the vault and the index. Every block with the
//! same words goes, looked up by fingerprint as `remember` looks; a passage
//! that only means the same is never touched. When one note refuses, or the
//! index cannot follow, every note is put back.

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
                embedded: before.embedded,
            });
        }
        let mut edits = Vec::new();
        for group in places.chunk_by(|a, b| a.0.note == b.0.note) {
            let blocks: Vec<([u32; 2], Digest)> =
                group.iter().map(|(at, fp)| (at.lines, *fp)).collect();
            match self.vault.remove(&group[0].0.note, &blocks) {
                Ok(edit) => edits.push(edit),
                Err(e) => return Err(self.put_back(&edits, e)),
            }
        }
        let after = self.sync_after(&edits)?;
        Ok(ForgetReport {
            removed: places.into_iter().map(|(at, _)| at).collect(),
            not_found,
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
        let before = world.index.snapshot().unwrap().unwrap().notes;
        let report = world
            .kb()
            .forget("  i CHANGE the engine oil, every 10000 km  ", None)
            .unwrap();
        assert_eq!(report.removed.len(), 1);
        assert_eq!(report.removed[0].note.as_str(), "Car/Maintenance.md");
        assert_eq!(report.removed[0].section, "Oil");
        assert!(report.not_found.is_empty());
        assert_ne!(world.index.snapshot().unwrap().unwrap().notes, before);
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
        let before = world.index.snapshot().unwrap().unwrap().notes;
        let writes = world.index.writes.get();
        let report = world.kb().forget("I own a motorbike.", None).unwrap();
        assert!(report.removed.is_empty());
        assert_eq!(report.not_found, ["I own a motorbike."]);
        assert_eq!(world.index.snapshot().unwrap().unwrap().notes, before);
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
    fn an_index_that_cannot_follow_puts_the_note_back() {
        // What is left of the passage needs a new vector, which never comes.
        let plan = || {
            vec![note(
                "Plan.md",
                &[(&["Plan"], "First."), (&["Plan"], "Second.")],
            )]
        };
        let world = World::new(plan());
        world.kb().sync(false).unwrap();
        world.embedder.down.set(true);
        let err = world.kb().forget("First.", None).unwrap_err();
        let err = format!("{err:#}");
        assert!(err.starts_with("nothing was changed: "), "{err}");
        assert_eq!(world.vault.notes().unwrap(), plan());
        let first = text::fingerprint("First.").unwrap();
        assert_eq!(
            world.index.exact(&first).unwrap().len(),
            1,
            "index and vault agree"
        );
    }

    #[test]
    fn a_refused_note_puts_back_the_notes_already_changed() {
        let same = "Shared line.";
        let world = World::new(vec![
            note("a.md", &[(&[], same), (&[], "Keep me.")]),
            note("b.md", &[(&[], same)]),
        ]);
        world.vault.refuse.replace(Some("b.md".into()));
        let err = world.kb().forget(same, None).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "nothing was changed: refusing to remove from b.md"
        );
        assert_eq!(texts(&world, "a.md"), [same, "Keep me."]);
        assert_eq!(texts(&world, "b.md"), [same]);

        // Refused before anything was written: the refusal speaks for itself.
        let err = world.kb().forget(same, Some(&path("b"))).unwrap_err();
        assert_eq!(format!("{err:#}"), "refusing to remove from b.md");
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
