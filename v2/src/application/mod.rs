//! The four use-cases — `sync`, `remember`, `check`, `search` — written
//! against the ports only.

pub mod check;
pub mod remember;
pub mod search;
pub mod sync;

use crate::ports::{Embedder, Hit, Index, Snapshot, Vault};
use anyhow::Result;

/// The knowledge base: one vault, its index, one embedder.
#[derive(Clone, Copy)]
pub struct Kb<'a> {
    pub vault: &'a dyn Vault,
    pub index: &'a dyn Index,
    pub embedder: &'a dyn Embedder,
}

impl Kb<'_> {
    /// The stored snapshot, when it was built by this embedder. Fails with an
    /// actionable message before the first sync or after a model change.
    fn ready(&self) -> Result<Snapshot> {
        todo!()
    }

    /// The `k` chunks nearest to `query` (embedded as a query, unit length).
    fn similar(&self, query: &str, k: usize) -> Result<Vec<Hit>> {
        let _ = (query, k);
        todo!()
    }
}
