//! `remember`: add one fact to the vault and the index, unless every block of
//! it is already stored word for word (looked up by fingerprint, without
//! embedding). When the index cannot take it, the note is put back.

use super::{Kb, nonblank};
use crate::domain::note::Location;
use crate::domain::path::NotePath;
use anyhow::Result;
use serde::Serialize;

/// Where facts go when no note is named.
pub const INBOX: &str = "Inbox.md";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RememberReport {
    /// False when every block was already stored (nothing written).
    pub stored: bool,
    pub note: NotePath,
    /// Where its blocks already sit (when `stored` is false).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub known_at: Vec<Location>,
    /// Chunks embedded by the call (the new fact, plus any pending vault edits).
    pub embedded: usize,
}

impl Kb<'_> {
    pub fn remember(
        &self,
        text: &str,
        note: &NotePath,
        heading: Option<&str>,
    ) -> Result<RememberReport> {
        let text = nonblank(text, "text")?;
        let heading = heading.map(str::trim).filter(|h| !h.is_empty());
        // Sync first so facts typed into the vault since the last sync count.
        let before = self.sync(false)?;
        if let Some(known_at) = self.stored(text)? {
            return Ok(RememberReport {
                stored: false,
                note: note.clone(),
                known_at,
                embedded: before.embedded,
            });
        }
        let edit = self.vault.append(note, heading, text)?;
        let after = self.sync_after(&[edit])?;
        Ok(RememberReport {
            stored: true,
            note: note.clone(),
            known_at: Vec::new(),
            embedded: before.embedded + after.embedded,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{World, sample_notes};
    use super::*;
    use crate::domain::text;
    use crate::ports::{Index, Vault};

    fn inbox() -> NotePath {
        NotePath::parse(INBOX).unwrap()
    }

    #[test]
    fn a_new_fact_is_written_and_indexed() {
        let world = World::synced();
        let before = world.index.snapshot().unwrap().unwrap().notes;
        let report = world
            .kb()
            .remember("Driving licence category B since 2015.", &inbox(), None)
            .unwrap();
        assert!(report.stored);
        assert_eq!(report.note, inbox());
        assert!(report.known_at.is_empty());
        assert_eq!(report.embedded, 1);
        assert_ne!(world.index.snapshot().unwrap().unwrap().notes, before);
        let written = world.vault.note(INBOX).unwrap();
        assert_eq!(written.blocks.len(), 1);
        assert_eq!(
            written.blocks[0].text,
            "Driving licence category B since 2015."
        );
        let fp = text::fingerprint("driving licence category b since 2015").unwrap();
        assert_eq!(world.index.exact(&fp).unwrap().len(), 1);
    }

    #[test]
    fn the_same_words_are_not_stored_twice() {
        let world = World::synced();
        let report = world
            .kb()
            .remember(
                "  i CHANGE the engine oil, every 10000 km  ",
                &inbox(),
                None,
            )
            .unwrap();
        assert!(!report.stored);
        assert_eq!(report.known_at.len(), 1);
        assert_eq!(report.known_at[0].note.as_str(), "Car/Maintenance.md");
        assert_eq!(report.known_at[0].section, "Oil");
        assert!(world.vault.note(INBOX).is_none(), "nothing written");
        assert_eq!(report.embedded, 0);
    }

    #[test]
    fn a_text_whose_every_block_is_stored_is_not_stored_again() {
        let world = World::synced();
        let both = "Allergy: penicillin.\n\nWinter tyres go on in November.";
        let report = world.kb().remember(both, &inbox(), None).unwrap();
        assert!(!report.stored);
        assert_eq!(report.known_at.len(), 2);
        assert!(world.vault.note(INBOX).is_none());
        let half = "Allergy: penicillin.\n\nMy blood type is A+.";
        assert!(world.kb().remember(half, &inbox(), None).unwrap().stored);
    }

    #[test]
    fn pending_vault_edits_are_synced_first() {
        // A fact typed into the vault but not yet synced still counts.
        let world = World::new(super::super::testing::sample_notes());
        let report = world
            .kb()
            .remember("Allergy: penicillin", &inbox(), None)
            .unwrap();
        assert!(!report.stored);
        assert_eq!(report.known_at[0].note.as_str(), "Health/Allergies.md");
    }

    #[test]
    fn heading_is_passed_to_the_vault_and_blank_heading_ignored() {
        let world = World::synced();
        let note = NotePath::parse("Car/Maintenance").unwrap();
        world
            .kb()
            .remember("Brake fluid every two years.", &note, Some(" Brakes "))
            .unwrap();
        world
            .kb()
            .remember("Wiper blades each autumn.", &note, Some("  "))
            .unwrap();
        let blocks = world.vault.note("Car/Maintenance.md").unwrap().blocks;
        assert_eq!(blocks[2].headings, ["Brakes"]);
        assert!(blocks[3].headings.is_empty());
    }

    #[test]
    fn an_index_that_cannot_follow_puts_the_note_back() {
        // The note is written before the index can fail; nothing may stay.
        let world = World::synced();
        let before = world.index.snapshot().unwrap();
        world.embedder.down.set(true);
        for note in [inbox(), NotePath::parse("Car/Maintenance").unwrap()] {
            let err = world
                .kb()
                .remember("Brake fluid every two years.", &note, Some("Brakes"))
                .unwrap_err();
            let err = format!("{err:#}");
            assert!(err.starts_with("nothing was changed: "), "{err}");
            assert!(err.ends_with("the embedder is down"), "{err}");
        }
        assert_eq!(world.vault.notes().unwrap(), sample_notes());
        assert_eq!(world.index.snapshot().unwrap(), before);
    }

    #[test]
    fn blank_text_is_refused() {
        let world = World::synced();
        for blank in ["", "   ", "--- !!"] {
            let err = world.kb().remember(blank, &inbox(), None).unwrap_err();
            assert!(err.to_string().contains("empty"), "{err}");
        }
        assert!(world.vault.note(INBOX).is_none());
    }
}
