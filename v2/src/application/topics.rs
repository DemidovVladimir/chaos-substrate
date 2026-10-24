//! `topics`: what the knowledge base holds that is near a task, as a short
//! ranked list (id, section, first words) without the passages' text. A
//! searcher reads it, keeps the ids that matter and fetches them with `read`.

use super::{Kb, nonblank};
use crate::domain::hash::Digest;
use crate::domain::text;
use anyhow::Result;
use serde::Serialize;

/// Topics listed when the caller names no limit.
pub const DEFAULT_TOPICS: usize = 8;

/// Longest gist, in characters: about one line.
pub const GIST_CHARS: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TopicsReport {
    pub topics: Vec<Topic>,
    /// Merkle root of the index that answered.
    pub root: Digest,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Topic {
    /// Passage id for `read`: `note:first-last`.
    pub id: String,
    pub section: String,
    pub score: f32,
    /// The passage's first words, on one line.
    pub gist: String,
}

impl Kb<'_> {
    /// Up to `limit` passages nearest to `task`, best first, each id once.
    pub fn topics(&self, task: &str, limit: usize) -> Result<TopicsReport> {
        let task = nonblank(task, "task")?;
        let root = self.ready()?.tree.root();
        let mut topics: Vec<Topic> = Vec::new();
        for hit in self.similar(task, limit)? {
            let id = hit.chunk.at.id();
            if topics.iter().any(|t| t.id == id) {
                continue; // another piece of a cut block
            }
            topics.push(Topic {
                id,
                section: hit.chunk.at.section,
                score: hit.score,
                gist: text::gist(&hit.chunk.text, GIST_CHARS),
            });
        }
        Ok(TopicsReport { topics, root })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{World, note, sample_notes};
    use super::*;
    use crate::domain::chunk::MAX_CHUNK_CHARS;

    #[test]
    fn topics_come_best_first_as_id_section_and_gist() {
        let world = World::synced();
        let report = world.kb().topics("engine oil", DEFAULT_TOPICS).unwrap();
        assert_eq!(report.topics.len(), 4, "every passage, ranked");
        let top = &report.topics[0];
        assert_eq!(top.id, "Car/Maintenance.md:1-1");
        assert_eq!(top.section, "Oil");
        assert_eq!(top.gist, "I change the engine oil every 10000 km.");
        assert!(report.topics.windows(2).all(|w| w[0].score >= w[1].score));
        assert_eq!(report.root, world.kb().sync(true).unwrap().root);
    }

    #[test]
    fn a_long_passage_shows_only_its_first_words() {
        let long = format!("Oil {}", "and more words ".repeat(20));
        let world = World::new(vec![note("Car.md", &[(&["Oil"], &long)])]);
        world.kb().sync(false).unwrap();
        let gist = &world.kb().topics("oil", 1).unwrap().topics[0].gist;
        assert!(gist.starts_with("Oil and more words"));
        assert!(gist.ends_with('…'));
        assert!(gist.chars().count() <= GIST_CHARS);
    }

    #[test]
    fn a_block_cut_into_pieces_is_listed_once() {
        let long = "oil ".repeat(MAX_CHUNK_CHARS / 2);
        let world = World::new(vec![note("Car.md", &[(&["Oil"], &long)])]);
        world.kb().sync(false).unwrap();
        let ids: Vec<String> = world
            .kb()
            .topics("oil", DEFAULT_TOPICS)
            .unwrap()
            .topics
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, ["Car.md:1-1"], "two pieces, one id");
    }

    #[test]
    fn the_limit_trims_the_list() {
        let world = World::synced();
        assert_eq!(world.kb().topics("engine oil", 2).unwrap().topics.len(), 2);
        assert!(
            world
                .kb()
                .topics("engine oil", 0)
                .unwrap()
                .topics
                .is_empty()
        );
    }

    #[test]
    fn needs_a_synced_index_and_a_task() {
        let fresh = World::new(sample_notes());
        assert!(fresh.kb().topics("oil", 8).is_err());
        assert!(World::synced().kb().topics(" ", 8).is_err());
    }
}
