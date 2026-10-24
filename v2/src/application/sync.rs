//! `sync`: make the index match the vault. The Merkle tree is rebuilt from the
//! notes; only chunks whose content key has no stored vector are embedded;
//! vectors, chunks and fingerprints of deleted text are dropped.

use super::Kb;
use crate::domain::hash::Digest;
use crate::domain::merkle::Changes;
use anyhow::Result;
use serde::Serialize;

/// Passages sent to the embedder per request.
pub const EMBED_BATCH: usize = 32;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SyncReport {
    /// Merkle root of the vault after the sync.
    pub root: Digest,
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
    pub fn sync(&self, dry_run: bool) -> Result<SyncReport> {
        let _ = dry_run;
        todo!()
    }
}
