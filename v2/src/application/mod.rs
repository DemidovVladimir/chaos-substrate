//! The use-cases — `sync`, `remember`, `forget`, `check`, `search`,
//! `topics`, `read`, and the `overview` behind the dashboard — written
//! against the ports only.

pub mod check;
pub mod forget;
pub mod overview;
pub mod read;
pub mod remember;
pub mod search;
pub mod sync;
#[cfg(test)]
mod testing;
pub mod topics;

use crate::domain::note::Location;
use crate::domain::{text, vector};
use crate::ports::{Edit, Embedder, Hit, Index, Snapshot, Vault};
use anyhow::{Context, Result, bail, ensure};
use sync::SyncReport;

/// The knowledge base: one vault, its index, one embedder.
#[derive(Clone, Copy)]
pub struct Kb<'a> {
    pub vault: &'a dyn Vault,
    pub index: &'a dyn Index,
    pub embedder: &'a dyn Embedder,
}

impl Kb<'_> {
    /// The stored snapshot. Fails with an actionable message before the first
    /// sync.
    fn indexed(&self) -> Result<Snapshot> {
        self.index
            .snapshot()?
            .context("the index is empty: run `chaos-kb sync` first")
    }

    /// The stored snapshot, when it was built by this embedder: needed before
    /// comparing vectors. Fails with an actionable message after a model
    /// change.
    fn ready(&self) -> Result<Snapshot> {
        let snapshot = self.indexed()?;
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

    /// Sync after `edits`. When the index cannot follow (the embedder is
    /// down, say), the notes are put back, so an error from `remember` or
    /// `forget` always means nothing changed and the index still agrees with
    /// the vault.
    fn sync_after(&self, edits: &[Edit]) -> Result<SyncReport> {
        self.sync(false).map_err(|e| self.put_back(edits, e))
    }

    /// `cause`, once the notes of `edits` are put back, newest first. With no
    /// edits, nothing was written and `cause` says so on its own.
    fn put_back(&self, edits: &[Edit], cause: anyhow::Error) -> anyhow::Error {
        if edits.is_empty() {
            return cause;
        }
        let stuck: Vec<String> = (edits.iter().rev())
            .filter_map(|e| {
                self.vault
                    .undo(e)
                    .err()
                    .map(|u| format!("{} ({u:#})", e.note))
            })
            .collect();
        if stuck.is_empty() {
            return cause.context("nothing was changed");
        }
        cause.context(format!(
            "could not put back {}: check those notes, then run `chaos-kb sync`",
            stuck.join(", ")
        ))
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
