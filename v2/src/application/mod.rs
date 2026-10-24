//! The four use-cases — `sync`, `remember`, `check`, `search` — written
//! against the ports only.

pub mod check;
pub mod remember;
pub mod search;
pub mod sync;
#[cfg(test)]
mod testing;

use crate::domain::note::Location;
use crate::domain::{text, vector};
use crate::ports::{Embedder, Hit, Index, Snapshot, Vault};
use anyhow::{Context, Result, bail, ensure};

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
        let snapshot = self
            .index
            .snapshot()?
            .context("the index is empty: run `chaos-kb sync` first")?;
        ensure!(
            snapshot.embedder == self.embedder.id(),
            "the index was built with embedder `{}` but `{}` is configured: run `chaos-kb sync` to re-embed",
            snapshot.embedder,
            self.embedder.id()
        );
        Ok(snapshot)
    }

    /// Where every block of `markdown` is stored word for word (read as the
    /// vault reads notes, looked up by fingerprint, no embedding); `None`
    /// when some block is not stored.
    fn stored(&self, markdown: &str) -> Result<Option<Vec<Location>>> {
        let mut found = Vec::new();
        let mut blocks = 0;
        for fingerprint in self
            .vault
            .texts(markdown)
            .iter()
            .filter_map(|t| text::fingerprint(t))
        {
            let at = self.index.exact(&fingerprint)?;
            if at.is_empty() {
                return Ok(None);
            }
            found.extend(at);
            blocks += 1;
        }
        found.sort();
        found.dedup();
        Ok((blocks > 0).then_some(found))
    }

    /// The `k` chunks nearest to `query` (embedded as a query, unit length).
    fn similar(&self, query: &str, k: usize) -> Result<Vec<Hit>> {
        let v = vector::unit(self.embedder.embed_query(query)?)
            .context("the embedder returned a zero vector for the query")?;
        self.index.nearest(&v, k)
    }
}

/// `input` trimmed, or an error when it holds no letter or digit.
fn nonblank<'t>(input: &'t str, what: &str) -> Result<&'t str> {
    if text::is_blank(input) {
        bail!("the {what} is empty (no letters or digits)");
    }
    Ok(input.trim())
}
