//! What the use-cases need from the outside world, as traits. Adapters
//! implement them; the application sees nothing else.

use crate::domain::chunk::Chunk;
use crate::domain::hash::Digest;
use crate::domain::merkle::Tree;
use crate::domain::note::{Location, Note};
use crate::domain::path::NotePath;
use anyhow::Result;
use std::collections::HashSet;
use std::fmt;

/// The knowledge source: a folder of Markdown notes (an Obsidian vault). It is
/// the only copy of the knowledge; everything else is derived from it.
pub trait Vault {
    /// Every note, parsed, sorted by path.
    fn notes(&self) -> Result<Vec<Note>>;

    /// The plain text of each block of a Markdown snippet (a fact or a query),
    /// read exactly as notes are read, so it fingerprints like stored text.
    fn texts(&self, markdown: &str) -> Vec<String>;

    /// Append `text` to `note`, creating the note when missing. With a
    /// `heading`, the text goes right after that heading's own text, before
    /// its first subheading, so it reads under exactly that heading (the
    /// heading is added at the end of the note when the note has no such
    /// heading); without one, at the end of the note. Existing text is never
    /// changed: an append that would change how existing text reads is
    /// refused.
    fn append(&self, note: &NotePath, heading: Option<&str>, text: &str) -> Result<()>;
}

/// Turns text into vectors. One embedder is one vector space.
pub trait Embedder {
    /// Identity of the vector space (model plus prompt format). Vectors made
    /// under different ids are never compared.
    fn id(&self) -> &str;

    /// One vector per `(context, text)` passage, in input order.
    fn embed_documents(&self, passages: &[(&str, &str)]) -> Result<Vec<Vec<f32>>>;

    /// The vector of a search query or question.
    fn embed_query(&self, query: &str) -> Result<Vec<f32>>;
}

/// What the last sync stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// [`Embedder::id`] of the stored vectors.
    pub embedder: String,
    pub tree: Tree,
}

/// The complete derived state written by one sync.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    /// The snapshot this update was computed from. [`Index::replace`] writes
    /// only while the index still holds it.
    pub base: Option<Snapshot>,
    pub embedder: String,
    pub tree: Tree,
    pub chunks: Vec<Chunk>,
    /// Fingerprint of every non-blank block, with where it sits.
    pub blocks: Vec<(Digest, Location)>,
    /// Unit vectors for chunk keys that have none stored yet.
    pub vectors: Vec<(Digest, Vec<f32>)>,
}

/// Another writer changed the index after this update's base was read.
#[derive(Debug)]
pub struct Stale;

impl fmt::Display for Stale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the index changed while this sync ran")
    }
}

impl std::error::Error for Stale {}

/// A chunk returned by a similarity search.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub chunk: Chunk,
    /// Cosine similarity to the query.
    pub score: f32,
}

/// The derived index: Merkle tree, block fingerprints, chunks and vectors.
pub trait Index {
    /// The stored snapshot; `None` before the first sync.
    fn snapshot(&self) -> Result<Option<Snapshot>>;

    /// Keys of every stored vector.
    fn vector_keys(&self) -> Result<HashSet<Digest>>;

    /// Atomically make the index equal to `update`: tree, chunks and blocks
    /// are replaced; vectors are added from `update.vectors` (overwriting the
    /// same key) and dropped when no chunk uses them. Fails, changing nothing,
    /// when a chunk would be left without a vector, and with [`Stale`] when
    /// the stored snapshot is no longer `update.base`.
    fn replace(&self, update: Update) -> Result<()>;

    /// Where blocks with this fingerprint sit, sorted.
    fn exact(&self, fingerprint: &Digest) -> Result<Vec<Location>>;

    /// The `k` chunks most similar to the unit vector `query`, best first.
    fn nearest(&self, query: &[f32], k: usize) -> Result<Vec<Hit>>;
}
