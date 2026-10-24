//! `remember`: add one fact to the vault and the index — unless the same
//! words are already stored, which the Merkle leaves answer without embedding.

use super::{Kb, nonblank};
use crate::domain::hash::Digest;
use crate::domain::note::Location;
use crate::domain::path::NotePath;
use crate::domain::text::fingerprint;
use anyhow::Result;
use serde::Serialize;

/// Where facts go when no note is named.
pub const INBOX: &str = "Inbox.md";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RememberReport {
    /// False when the same words were already stored (nothing written).
    pub stored: bool,
    pub note: NotePath,
    /// Where the same words already sit (when `stored` is false).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub known_at: Vec<Location>,
    /// Merkle root after the call.
    pub root: Digest,
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
        let known_at = match fingerprint(text) {
            Some(fp) => self.index.exact(&fp)?,
            None => Vec::new(),
        };
        if !known_at.is_empty() {
            return Ok(RememberReport {
                stored: false,
                note: note.clone(),
                known_at,
                root: before.root,
                embedded: before.embedded,
            });
        }
        self.vault.append(note, heading, text)?;
        let after = self.sync(false)?;
        Ok(RememberReport {
            stored: true,
            note: note.clone(),
            known_at,
            root: after.root,
            embedded: before.embedded + after.embedded,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::World;
    use super::*;
    use crate::domain::text;
    use crate::ports::Index;

    fn inbox() -> NotePath {
        NotePath::parse(INBOX).unwrap()
    }

    #[test]
    fn a_new_fact_is_written_and_indexed() {
        let world = World::synced();
        let before = world.index.snapshot().unwrap().unwrap().tree.root();
        let report = world
            .kb()
            .remember("Driving licence category B since 2015.", &inbox(), None)
            .unwrap();
        assert!(report.stored);
        assert_eq!(report.note, inbox());
        assert!(report.known_at.is_empty());
        assert_eq!(report.embedded, 1);
        assert_ne!(report.root, before);
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
    fn blank_text_is_refused() {
        let world = World::synced();
        for blank in ["", "   ", "--- !!"] {
            let err = world.kb().remember(blank, &inbox(), None).unwrap_err();
            assert!(err.to_string().contains("empty"), "{err}");
        }
        assert!(world.vault.note(INBOX).is_none());
    }
}
