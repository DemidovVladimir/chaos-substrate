//! In-memory test doubles of the ports, shared by the use-case tests. They
//! exist only under `cfg(test)`; runtime code never sees a fake vector.

use super::Kb;
use crate::domain::chunk::Chunk;
use crate::domain::hash::Digest;
use crate::domain::note::{Block, Location, Note};
use crate::domain::path::NotePath;
use crate::domain::{text, vector};
use crate::ports::{Edit, Embedder, Hit, Index, Snapshot, Stale, Update, Vault};
use anyhow::{Context, Result, anyhow, bail, ensure};
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
    /// A note whose blocks [`Vault::remove`] refuses to remove.
    pub refuse: RefCell<Option<String>>,
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

    /// The edit that turns `before` into `after`.
    fn edit(before: Option<&Note>, after: &Note) -> Edit {
        Edit {
            note: after.path.clone(),
            before: before.map(encode),
            after: encode(after),
            folders: Vec::new(),
        }
    }
}

/// A note as text, for [`Edit`]: blocks split by U+1D, their fields by U+1F.
fn encode(note: &Note) -> String {
    let block = |b: &Block| {
        let lines = format!("{}\u{1f}{}", b.lines[0], b.lines[1]);
        [
            b.headings.join("\u{1e}"),
            b.text.clone(),
            b.source.clone(),
            lines,
        ]
        .join("\u{1f}")
    };
    note.blocks
        .iter()
        .map(block)
        .collect::<Vec<_>>()
        .join("\u{1d}")
}

/// The note [`encode`] wrote.
fn decode(path: &NotePath, text: &str) -> Note {
    let block = |raw: &str| {
        let f: Vec<&str> = raw.split('\u{1f}').collect();
        Block {
            headings: f[0]
                .split('\u{1e}')
                .filter(|h| !h.is_empty())
                .map(str::to_string)
                .collect(),
            text: f[1].to_string(),
            source: f[2].to_string(),
            lines: [f[3].parse().unwrap(), f[4].parse().unwrap()],
        }
    };
    let blocks = text
        .split('\u{1d}')
        .filter(|b| !b.is_empty())
        .map(block)
        .collect();
    Note {
        path: path.clone(),
        blocks,
    }
}

impl Vault for MemVault {
    fn notes(&self) -> Result<Vec<Note>> {
        let mut notes = self.notes.borrow().clone();
        notes.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(notes)
    }

    /// Blocks are separated by blank lines; text is taken as written.
    fn texts(&self, markdown: &str) -> Vec<String> {
        markdown
            .split("\n\n")
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn append(&self, path: &NotePath, heading: Option<&str>, text: &str) -> Result<Edit> {
        let before = self.note(path.as_str());
        let mut note = before.clone().unwrap_or(Note {
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
        let edit = MemVault::edit(before.as_ref(), &note);
        self.put(note);
        Ok(edit)
    }

    fn remove(&self, path: &NotePath, blocks: &[([u32; 2], Digest)]) -> Result<Edit> {
        if self.refuse.borrow().as_deref() == Some(path.as_str()) {
            bail!("refusing to remove from {path}");
        }
        let before = self.note(path.as_str()).context("no such note")?;
        let mut note = before.clone();
        for (lines, fingerprint) in blocks {
            let at = note.blocks.iter().position(|b| {
                b.lines == *lines && text::fingerprint(&b.text).as_ref() == Some(fingerprint)
            });
            note.blocks
                .remove(at.context("those lines no longer hold that text")?);
        }
        let edit = MemVault::edit(Some(&before), &note);
        self.put(note);
        Ok(edit)
    }

    fn undo(&self, edit: &Edit) -> Result<()> {
        let Some(now) = self.note(edit.note.as_str()) else {
            ensure!(edit.before.is_none(), "{} is gone", edit.note);
            return Ok(());
        };
        if encode(&now) != edit.after {
            bail!("{} changed since", edit.note);
        }
        match &edit.before {
            Some(before) => self.put(decode(&edit.note, before)),
            None => self.notes.borrow_mut().retain(|n| n.path != edit.note),
        }
        Ok(())
    }
}

/// An index held in memory, with the [`Index::replace`] contract.
#[derive(Default)]
pub struct MemIndex {
    /// The last update, its vectors moved into `vectors`.
    state: RefCell<Option<Update>>,
    vectors: RefCell<HashMap<Digest, Vec<f32>>>,
    pub writes: Cell<usize>,
    /// How many coming writes another writer beats (refused as [`Stale`]).
    pub beaten: Cell<usize>,
}

impl MemIndex {
    pub fn vector_count(&self) -> usize {
        self.vectors.borrow().len()
    }
}

impl Index for MemIndex {
    fn snapshot(&self) -> Result<Option<Snapshot>> {
        Ok(self.state.borrow().as_ref().map(|u| Snapshot {
            embedder: u.embedder.clone(),
            notes: u.notes.clone(),
        }))
    }

    fn vector_keys(&self) -> Result<HashSet<Digest>> {
        Ok(self.vectors.borrow().keys().copied().collect())
    }

    fn replace(&self, mut update: Update) -> Result<()> {
        if self.beaten.get() > 0 || self.snapshot()? != update.base {
            self.beaten.set(self.beaten.get().saturating_sub(1));
            return Err(Stale.into());
        }
        let mut vectors = self.vectors.borrow_mut();
        let supplied: HashSet<Digest> = update.vectors.iter().map(|(k, _)| *k).collect();
        if let Some(c) = update
            .chunks
            .iter()
            .find(|c| !supplied.contains(&c.key) && !vectors.contains_key(&c.key))
        {
            bail!("chunk {} has no vector", c.key);
        }
        vectors.extend(std::mem::take(&mut update.vectors));
        let used: HashSet<Digest> = update.chunks.iter().map(|c| c.key).collect();
        vectors.retain(|k, _| used.contains(k));
        *self.state.borrow_mut() = Some(update);
        self.writes.set(self.writes.get() + 1);
        Ok(())
    }

    fn exact(&self, fingerprint: &Digest) -> Result<Vec<Location>> {
        let state = self.state.borrow();
        let mut out: Vec<Location> = state
            .iter()
            .flat_map(|u| u.blocks.iter())
            .filter(|(fp, _)| fp == fingerprint)
            .map(|(_, at)| at.clone())
            .collect();
        out.sort();
        Ok(out)
    }

    fn nearest(&self, query: &[f32], k: usize) -> Result<Vec<Hit>> {
        let state = self.state.borrow();
        let vectors = self.vectors.borrow();
        let chunks: Vec<&Chunk> = state.iter().flat_map(|u| u.chunks.iter()).collect();
        let candidates = chunks.iter().map(|c| (*c, vectors[&c.key].as_slice()));
        Ok(vector::top_k(query, candidates, k)
            .into_iter()
            .map(|(chunk, score)| Hit {
                chunk: chunk.clone(),
                score,
            })
            .collect())
    }

    fn chunks(&self) -> Result<Vec<Chunk>> {
        let mut out: Vec<Chunk> = self
            .state
            .borrow()
            .iter()
            .flat_map(|u| u.chunks.clone())
            .collect();
        out.sort_by(|a, b| (&a.at.note, a.at.lines).cmp(&(&b.at.note, b.at.lines)));
        Ok(out)
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
    /// Every call fails, as an embedder that cannot be reached does.
    pub down: Cell<bool>,
}

impl StubEmbedder {
    pub fn new(keywords: &[&'static str]) -> StubEmbedder {
        StubEmbedder {
            id: "stub|v1".into(),
            keywords: keywords.to_vec(),
            batches: RefCell::new(Vec::new()),
            queries: Cell::new(0),
            zero: Cell::new(false),
            down: Cell::new(false),
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
            .map(|k| {
                if self.zero.get() || !text.contains(k) {
                    0.0
                } else {
                    1.0
                }
            })
            .chain([bias])
            .collect()
    }
}

impl Embedder for StubEmbedder {
    fn id(&self) -> &str {
        &self.id
    }

    fn embed_documents(&self, passages: &[(&str, &str)]) -> Result<Vec<Vec<f32>>> {
        if self.down.get() {
            return Err(anyhow!("the embedder is down"));
        }
        self.batches.borrow_mut().push(passages.len());
        Ok(passages
            .iter()
            .map(|(context, text)| self.vector(&format!("{context} {text}")))
            .collect())
    }

    fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        if self.down.get() {
            return Err(anyhow!("the embedder is down"));
        }
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
        note("Health/Allergies.md", &[(&[], "Allergy: penicillin.")]),
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
