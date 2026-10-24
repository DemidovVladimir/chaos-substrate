//! `read`: the exact passages behind ids from `topics`, with their text. It
//! looks ids up in the index and embeds nothing.

use super::Kb;
use crate::domain::hash::Digest;
use crate::domain::note::parse_id;
use anyhow::{Result, ensure};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReadReport {
    /// In the order asked for; a block cut into pieces gives every piece.
    pub passages: Vec<Passage>,
    /// Ids the index does not hold (the note changed or is gone: run
    /// `topics` again).
    pub missing: Vec<String>,
    /// Merkle root of the index that answered.
    pub root: Digest,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Passage {
    pub id: String,
    pub section: String,
    pub text: String,
}

impl Kb<'_> {
    /// The passages with these ids (`note:first-last`), each once.
    pub fn read(&self, ids: &[String]) -> Result<ReadReport> {
        ensure!(
            !ids.is_empty(),
            "no passage ids given: pass the ids `topics` listed"
        );
        let mut wanted = Vec::new();
        for id in ids {
            let place = (id.trim(), parse_id(id)?);
            if !wanted.contains(&place) {
                wanted.push(place);
            }
        }
        let root = self.indexed()?.tree.root();
        let stored = self.index.chunks()?;
        let mut passages = Vec::new();
        let mut missing = Vec::new();
        for (id, (note, lines)) in wanted {
            let before = passages.len();
            passages.extend(
                stored
                    .iter()
                    .filter(|c| c.at.note == note && c.at.lines == lines)
                    .map(|c| Passage {
                        id: c.at.id(),
                        section: c.at.section.clone(),
                        text: c.text.clone(),
                    }),
            );
            if passages.len() == before {
                missing.push(id.to_string());
            }
        }
        Ok(ReadReport {
            passages,
            missing,
            root,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{World, note, sample_notes};
    use crate::domain::chunk::MAX_CHUNK_CHARS;

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn reads_passages_in_the_order_asked() {
        let world = World::synced();
        let report = world
            .kb()
            .read(&ids(&["Education.md:1-1", "Car/Maintenance.md:1-1"]))
            .unwrap();
        let got: Vec<(&str, &str, &str)> = report
            .passages
            .iter()
            .map(|p| (p.id.as_str(), p.section.as_str(), p.text.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "Education.md:1-1",
                    "University",
                    "Master of Science in physics, 2014."
                ),
                (
                    "Car/Maintenance.md:1-1",
                    "Oil",
                    "I change the engine oil every 10000 km."
                ),
            ]
        );
        assert!(report.missing.is_empty());
        assert_eq!(report.root, world.kb().sync(true).unwrap().root);
    }

    #[test]
    fn embeds_nothing() {
        let world = World::synced();
        let before = world.embedder.queries.get();
        world.kb().read(&ids(&["Education.md:1-1"])).unwrap();
        assert_eq!(world.embedder.queries.get(), before);
    }

    #[test]
    fn unknown_ids_are_reported_and_repeats_read_once() {
        let world = World::synced();
        let report = world
            .kb()
            .read(&ids(&[
                "Education.md:1-1",
                "Gone.md:1-1",
                "Education.md:1-1",
                "Education.md:2-9",
            ]))
            .unwrap();
        assert_eq!(report.passages.len(), 1);
        assert_eq!(report.missing, ["Gone.md:1-1", "Education.md:2-9"]);
    }

    #[test]
    fn a_block_cut_into_pieces_reads_whole() {
        let long = "oil ".repeat(MAX_CHUNK_CHARS / 2);
        let world = World::new(vec![note("Car.md", &[(&["Oil"], &long)])]);
        world.kb().sync(false).unwrap();
        let report = world.kb().read(&ids(&["Car.md:1-1"])).unwrap();
        assert!(report.passages.len() > 1, "every piece");
        let words: usize = report
            .passages
            .iter()
            .map(|p| p.text.split_whitespace().count())
            .sum();
        assert_eq!(words, MAX_CHUNK_CHARS / 2);
    }

    #[test]
    fn needs_a_synced_index_and_well_formed_ids() {
        let fresh = World::new(sample_notes());
        assert!(fresh.kb().read(&ids(&["Education.md:1-1"])).is_err());
        let world = World::synced();
        assert!(world.kb().read(&[]).is_err(), "no ids");
        let bad = world.kb().read(&ids(&["Education.md"])).unwrap_err();
        assert!(format!("{bad:#}").contains("note:first-last"));
    }
}
