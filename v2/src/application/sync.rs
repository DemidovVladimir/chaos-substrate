//! `sync`: make the index match the vault. Each note's hash is compared with
//! the stored one to report what changed; only chunks whose content key has no
//! stored vector are embedded;
//! vectors, chunks and fingerprints of deleted text are dropped. The write
//! goes through only if the index is still the one this sync read.

use super::Kb;
use crate::domain::changes::{Changes, NoteHashes};
use crate::domain::chunk::{self, Chunk, MAX_CHUNK_CHARS};
use crate::domain::hash::Digest;
use crate::domain::note::{Location, Note, section};
use crate::domain::{text, vector};
use crate::ports::{Stale, Update};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::collections::HashSet;

/// Passages sent to the embedder per request.
pub const EMBED_BATCH: usize = 32;

/// Attempts in all, the first one included, when another writer keeps
/// changing the index first.
pub const SYNC_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SyncReport {
    pub notes: usize,
    pub chunks: usize,
    /// Chunks embedded by this run (new or edited text). With `dry_run`, the
    /// number that would be embedded.
    pub embedded: usize,
    #[serde(flatten)]
    pub changes: Changes,
    pub dry_run: bool,
}

impl Kb<'_> {
    /// Sync, starting over when another writer changed the index while this
    /// one was embedding ([`SYNC_ATTEMPTS`] attempts in all).
    pub fn sync(&self, dry_run: bool) -> Result<SyncReport> {
        let mut attempt = 1;
        loop {
            match self.sync_once(dry_run) {
                Err(e) if e.is::<Stale>() && attempt < SYNC_ATTEMPTS => attempt += 1,
                outcome => return outcome,
            }
        }
    }

    fn sync_once(&self, dry_run: bool) -> Result<SyncReport> {
        let notes = self.vault.notes()?;
        let hashes = NoteHashes::build(&notes);
        let previous = self.index.snapshot()?;
        let changes = previous
            .as_ref()
            .map_or_else(NoteHashes::default, |s| s.notes.clone())
            .diff(&hashes);
        let chunks: Vec<Chunk> = notes
            .iter()
            .flat_map(|n| chunk::chunks(n, MAX_CHUNK_CHARS))
            .collect();
        // Vectors from another embedder live in another space: never reuse them.
        let stored = match &previous {
            Some(s) if s.embedder == self.embedder.id() => self.index.vector_keys()?,
            _ => HashSet::new(),
        };
        let mut queued = HashSet::new();
        let missing: Vec<&Chunk> = chunks
            .iter()
            .filter(|c| !stored.contains(&c.key) && queued.insert(c.key))
            .collect();
        let report = SyncReport {
            notes: notes.len(),
            chunks: chunks.len(),
            embedded: missing.len(),
            changes,
            dry_run,
        };
        if dry_run {
            return Ok(report);
        }
        let vectors = self.embed(&missing)?;
        self.index.replace(Update {
            base: previous,
            embedder: self.embedder.id().to_string(),
            notes: hashes,
            blocks: fingerprints(&notes),
            chunks,
            vectors,
        })?;
        Ok(report)
    }

    /// Unit vectors for `chunks`, in batches of [`EMBED_BATCH`].
    fn embed(&self, chunks: &[&Chunk]) -> Result<Vec<(Digest, Vec<f32>)>> {
        let mut out = Vec::with_capacity(chunks.len());
        for batch in chunks.chunks(EMBED_BATCH) {
            let passages: Vec<(&str, &str)> = batch
                .iter()
                .map(|c| (c.context.as_str(), c.text.as_str()))
                .collect();
            let vectors = self.embedder.embed_documents(&passages)?;
            ensure!(
                vectors.len() == batch.len(),
                "the embedder returned {} vectors for {} passages",
                vectors.len(),
                batch.len()
            );
            for (c, v) in batch.iter().zip(vectors) {
                let v = vector::unit(v).with_context(|| {
                    format!("the embedder returned a zero vector for {}", c.at.note)
                })?;
                out.push((c.key, v));
            }
        }
        Ok(out)
    }
}

/// Fingerprint and location of every non-blank block: the exact-match index.
fn fingerprints(notes: &[Note]) -> Vec<(Digest, Location)> {
    notes
        .iter()
        .flat_map(|n| {
            n.blocks.iter().filter_map(|b| {
                let at = Location {
                    note: n.path.clone(),
                    section: section(&b.headings),
                    lines: b.lines,
                };
                Some((text::fingerprint(&b.text)?, at))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::testing::{World, note, sample_notes};
    use super::*;
    use crate::domain::changes::NoteHashes;
    use crate::domain::text;
    use crate::ports::{Index, Vault};

    #[test]
    fn first_sync_embeds_everything_and_stores_every_note_hash() {
        let world = World::new(sample_notes());
        let report = world.kb().sync(false).unwrap();
        let notes = world.vault.notes().unwrap();
        assert_eq!(report.notes, 3);
        assert_eq!(report.chunks, 4);
        assert_eq!(report.embedded, 4);
        assert_eq!(
            report.changes.added,
            ["Car/Maintenance.md", "Education.md", "Health/Allergies.md"]
        );
        assert!(report.changes.changed.is_empty() && report.changes.removed.is_empty());
        assert!(!report.dry_run);
        assert_eq!(world.index.vector_count(), 4);
        let snapshot = world.index.snapshot().unwrap().unwrap();
        assert_eq!(snapshot.embedder, "stub|v1");
        assert_eq!(snapshot.notes, NoteHashes::build(&notes));
    }

    #[test]
    fn unchanged_vault_embeds_nothing() {
        let world = World::synced();
        let before = world.embedder.embedded();
        let report = world.kb().sync(false).unwrap();
        assert_eq!(report.embedded, 0);
        assert_eq!(world.embedder.embedded(), before);
        assert_eq!(report.changes, Changes::default());
    }

    #[test]
    fn an_edit_embeds_only_the_edited_chunk() {
        let world = World::synced();
        let first = world.index.snapshot().unwrap().unwrap().notes;
        world.vault.put(note(
            "Education.md",
            &[(&["University"], "Master of Science in physics, 2015.")],
        ));
        let report = world.kb().sync(false).unwrap();
        assert_eq!(report.embedded, 1);
        assert_eq!(report.changes.changed, ["Education.md"]);
        assert_ne!(world.index.snapshot().unwrap().unwrap().notes, first);
        assert_eq!(world.index.vector_count(), 4, "old vector dropped");
    }

    #[test]
    fn a_deleted_note_is_purged() {
        let world = World::synced();
        let fact = text::fingerprint("Allergy: penicillin.").unwrap();
        assert_eq!(world.index.exact(&fact).unwrap().len(), 1);
        world.vault.remove("Health/Allergies.md");
        let report = world.kb().sync(false).unwrap();
        assert_eq!(report.changes.removed, ["Health/Allergies.md"]);
        assert_eq!(report.notes, 2);
        assert_eq!(world.index.vector_count(), 3);
        assert!(world.index.exact(&fact).unwrap().is_empty());
    }

    #[test]
    fn dry_run_writes_and_embeds_nothing() {
        let world = World::new(sample_notes());
        let report = world.kb().sync(true).unwrap();
        assert!(report.dry_run);
        assert_eq!(report.embedded, 4, "would embed");
        assert_eq!(world.embedder.embedded(), 0);
        assert_eq!(world.index.writes.get(), 0);
        assert!(world.index.snapshot().unwrap().is_none());
    }

    #[test]
    fn a_new_embedder_re_embeds_everything() {
        let mut world = World::synced();
        world.embedder.id = "other|v1".into();
        world.embedder.batches.borrow_mut().clear();
        let report = world.kb().sync(false).unwrap();
        assert_eq!(report.embedded, 4);
        assert_eq!(world.embedder.embedded(), 4);
        assert_eq!(
            world.index.snapshot().unwrap().unwrap().embedder,
            "other|v1"
        );
    }

    #[test]
    fn identical_chunks_are_embedded_once() {
        // Same title, section and text in two folders: one embedding input.
        let same = [(&["Plan"][..], "Shared line.")];
        let world = World::new(vec![note("x/a.md", &same), note("y/a.md", &same)]);
        let report = world.kb().sync(false).unwrap();
        assert_eq!(report.chunks, 2);
        assert_eq!(report.embedded, 1);
        assert_eq!(world.embedder.embedded(), 1);
    }

    #[test]
    fn batches_respect_the_limit() {
        let texts: Vec<String> = (0..EMBED_BATCH + 5).map(|i| format!("fact {i}")).collect();
        let notes: Vec<_> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| note(&format!("n{i}.md"), &[(&[], t.as_str())]))
            .collect();
        let world = World::new(notes);
        world.kb().sync(false).unwrap();
        assert_eq!(*world.embedder.batches.borrow(), [EMBED_BATCH, 5]);
    }

    #[test]
    fn a_sync_beaten_by_another_writer_starts_over() {
        let world = World::new(sample_notes());
        world.index.beaten.set(SYNC_ATTEMPTS - 1);
        let report = world.kb().sync(false).unwrap();
        assert_eq!(report.notes, 3);
        assert_eq!(world.index.writes.get(), 1);
        assert_eq!(
            world.index.snapshot().unwrap().unwrap().notes,
            NoteHashes::build(&world.vault.notes().unwrap())
        );
    }

    #[test]
    fn a_sync_beaten_every_time_gives_up_with_stale() {
        let world = World::new(sample_notes());
        world.index.beaten.set(SYNC_ATTEMPTS);
        let err = world.kb().sync(false).unwrap_err();
        assert!(err.is::<Stale>(), "{err:#}");
        assert_eq!(world.index.writes.get(), 0);
    }

    #[test]
    fn a_zero_vector_fails_without_writing() {
        let world = World::new(sample_notes());
        world.embedder.zero.set(true);
        let err = world.kb().sync(false).unwrap_err();
        assert!(format!("{err:#}").contains("zero vector"), "{err:#}");
        assert_eq!(world.index.writes.get(), 0);
    }
}
