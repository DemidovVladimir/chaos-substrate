//! The index as one redb file (pure-Rust, ACID). The database is opened per
//! operation and closed right after, so a CLI call and a running MCP server
//! can share the file; vectors are stored as little-endian `f32` bytes.

use crate::domain::hash::Digest;
use crate::domain::note::Location;
use crate::ports::{Hit, Index, Snapshot, Update};
use anyhow::Result;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub struct RedbIndex {
    path: PathBuf,
}

impl RedbIndex {
    /// An index stored at `path` (created with its parent folder on first write).
    pub fn new(path: &Path) -> RedbIndex {
        RedbIndex {
            path: path.to_path_buf(),
        }
    }
}

impl Index for RedbIndex {
    fn snapshot(&self) -> Result<Option<Snapshot>> {
        let _ = &self.path;
        todo!()
    }

    fn vector_keys(&self) -> Result<HashSet<Digest>> {
        todo!()
    }

    fn replace(&self, update: Update) -> Result<()> {
        let _ = update;
        todo!()
    }

    fn exact(&self, fingerprint: &Digest) -> Result<Vec<Location>> {
        let _ = fingerprint;
        todo!()
    }

    fn nearest(&self, query: &[f32], k: usize) -> Result<Vec<Hit>> {
        let _ = (query, k);
        todo!()
    }
}
