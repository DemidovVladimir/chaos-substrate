//! In-memory test doubles of the ports, shared by the use-case tests. They
//! exist only under `cfg(test)`; runtime code never sees a fake vector.

use super::Kb;
use crate::domain::chunk::Chunk;
use crate::domain::hash::Digest;
use crate::domain::merkle::Tree;
use crate::domain::note::{Block, Location, Note};
use crate::domain::path::NotePath;
use crate::domain::vector;
use crate::ports::{Embedder, Hit, Index, Snapshot, Update, Vault};
use anyhow::{Result, bail};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

/// `(headings, text)` of one block.
pub type Spec<'a> = (&'a [&'a str], &'a str);

pub fn note(path: &str, blocks: &[Spec]) -> Note {
    Note {
        path: NotePath::parse(path).unwrap(),
        blocks: blocks
            .iter()
            .enumerate()
            .map(|(i, (headings, text))| Block {
                headings: headings.iter().map(|h| h.to_string()).collect(),
                text: text.to_string(),
                source: text.to_string(),
                lines: [i as u32 * 2 + 1, i as u32 * 2 + 1],
            })
            .collect(),
    }
}

/// A vault held in memory.
#[derive(Default)]
pub struct MemVault {
    notes: RefCell<Vec<Note>>,
}

impl MemVault {
    pub fn with(notes: Vec<Note>) -> MemVault {
        let vault = MemVault::default();
        notes.into_iter().for_each(|n| vault.put(n));
        vault
    }

    /// Add or replace a note.
    pub fn put(&self, note: Note) {
        let mut notes = self.notes.borrow_mut();
        notes.retain(|n| n.path != note.path);
        notes.push(note);
    }

    pub fn remove(&self, path: &str) {
        self.notes.borrow_mut().retain(|n| n.path.as_str() != path);
    }

    pub fn note(&self, path: &str) -> Option<Note> {
        self.notes
            .borrow()
            .iter()
            .find(|n| n.path.as_str() == path)
            .cloned()
    }
}

impl Vault for MemVault {
    fn notes(&self) -> Result<Vec<Note>> {
        let mut notes = self.notes.borrow().clone();
        notes.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(notes)
    }

    fn append(&self, path: &NotePath, heading: Option<&str>, text: &str) -> Result<()> {
        let mut note = self.note(path.as_str()).unwrap_or(Note {
            path: path.clone(),
            blocks: Vec::new(),
        });
        let line = note.blocks.last().map_or(1, |b| b.lines[1] + 2);
        note.blocks.push(Block {
            headings: heading.map(|h| vec![h.to_string()]).unwrap_or_default(),
            text: text.to_string(),
            source: text.to_string(),
            lines: [line, line],
        });
        self.put(note);
        Ok(())
    }
}

/// An index held in memory, with the [`Index::replace`] contract.
#[derive(Default)]
pub struct MemIndex {
    state: RefCell<Option<(String, Tree, Vec<Chunk>, Vec<(Digest, Location)>)>>,
    vectors: RefCell<HashMap<Digest, Vec<f32>>>,
    pub writes: Cell<usize>,
}

impl MemIndex {
    pub fn vector_count(&self) -> usize {
        self.vectors.borrow().len()
    }
}

impl Index for MemIndex {
    fn snapshot(&self) -> Result<Option<Snapshot>> {
        Ok(self.state.borrow().as_ref().map(|(embedder, tree, ..)| Snapshot {
            embedder: embedder.clone(),
            tree: tree.clone(),
        }))
    }

    fn vector_keys(&self) -> Result<HashSet<Digest>> {
        Ok(self.vectors.borrow().keys().copied().collect())
    }

    fn replace(&self, update: Update) -> Result<()> {
        let mut vectors = self.vectors.borrow_mut();
        let supplied: HashSet<Digest> = update.vectors.iter().map(|(k, _)| *k).collect();
        if let Some(c) = update
            .chunks
            .iter()
            .find(|c| !supplied.contains(&c.key) && !vectors.contains_key(&c.key))
        {
            bail!("chunk {} has no vector", c.key);
        }
        vectors.extend(update.vectors);
        let used: HashSet<Digest> = update.chunks.iter().map(|c| c.key).collect();
        vectors.retain(|k, _| used.contains(k));
        *self.state.borrow_mut() = Some((update.embedder, update.tree, update.chunks, update.blocks));
        self.writes.set(self.writes.get() + 1);
        Ok(())
    }

    fn exact(&self, fingerprint: &Digest) -> Result<Vec<Location>> {
        let state = self.state.borrow();
        let mut out: Vec<Location> = state
            .iter()
            .flat_map(|(.., blocks)| blocks.iter())
            .filter(|(fp, _)| fp == fingerprint)
            .map(|(_, at)| at.clone())
            .collect();
        out.sort();
        Ok(out)
    }

    fn nearest(&self, query: &[f32], k: usize) -> Result<Vec<Hit>> {
        let state = self.state.borrow();
        let vectors = self.vectors.borrow();
        let chunks: Vec<&Chunk> = state.iter().flat_map(|(_, _, c, _)| c.iter()).collect();
        let candidates = chunks.iter().map(|c| (*c, vectors[&c.key].as_slice()));
        Ok(vector::top_k(query, candidates, k)
            .into_iter()
            .map(|(chunk, score)| Hit {
                chunk: chunk.clone(),
                score,
            })
            .collect())
    }
}

/// A deterministic embedder for tests: one axis per keyword (1.0 when the
/// lowercased input contains it) plus a small constant axis, and counters.
pub struct StubEmbedder {
    pub id: String,
    keywords: Vec<&'static str>,
    pub batches: RefCell<Vec<usize>>,
    pub queries: Cell<usize>,
    pub zero: Cell<bool>,
}

impl StubEmbedder {
    pub fn new(keywords: &[&'static str]) -> StubEmbedder {
        StubEmbedder {
            id: "stub|v1".into(),
            keywords: keywords.to_vec(),
            batches: RefCell::new(Vec::new()),
            queries: Cell::new(0),
            zero: Cell::new(false),
        }
    }

    /// Passages embedded so far.
    pub fn embedded(&self) -> usize {
        self.batches.borrow().iter().sum()
    }

    fn vector(&self, text: &str) -> Vec<f32> {
        let text = text.to_lowercase();
        let bias = if self.zero.get() { 0.0 } else { 0.1 };
        self.keywords
            .iter()
            .map(|k| if self.zero.get() || !text.contains(k) { 0.0 } else { 1.0 })
            .chain([bias])
            .collect()
    }
}

impl Embedder for StubEmbedder {
    fn id(&self) -> &str {
        &self.id
    }

    fn embed_documents(&self, passages: &[(&str, &str)]) -> Result<Vec<Vec<f32>>> {
        self.batches.borrow_mut().push(passages.len());
        Ok(passages
            .iter()
            .map(|(context, text)| self.vector(&format!("{context} {text}")))
            .collect())
    }

    fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        self.queries.set(self.queries.get() + 1);
        Ok(self.vector(query))
    }
}

/// Keywords the stub embedder separates.
pub const KEYWORDS: [&str; 5] = ["oil", "tyres", "physics", "licence", "allergy"];

/// A small personal vault: car care, education, health.
pub fn sample_notes() -> Vec<Note> {
    vec![
        note(
            "Car/Maintenance.md",
            &[
                (&["Oil"], "I change the engine oil every 10000 km."),
                (&["Tyres"], "Winter tyres go on in November."),
            ],
        ),
        note(
            "Education.md",
            &[(&["University"], "Master of Science in physics, 2014.")],
        ),
        note(
            "Health/Allergies.md",
            &[(&[], "Allergy: penicillin.")],
        ),
    ]
}

/// One vault, index and embedder wired into a [`Kb`].
pub struct World {
    pub vault: MemVault,
    pub index: MemIndex,
    pub embedder: StubEmbedder,
}

impl World {
    pub fn new(notes: Vec<Note>) -> World {
        World {
            vault: MemVault::with(notes),
            index: MemIndex::default(),
            embedder: StubEmbedder::new(&KEYWORDS),
        }
    }

    /// A world already synced once.
    pub fn synced() -> World {
        let world = World::new(sample_notes());
        world.kb().sync(false).unwrap();
        world
    }

    pub fn kb(&self) -> Kb<'_> {
        Kb {
            vault: &self.vault,
            index: &self.index,
            embedder: &self.embedder,
        }
    }
}
