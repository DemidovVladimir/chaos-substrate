//! `remember`: add one fact to the vault and the index — unless the same
//! words are already stored, which the Merkle leaves answer without embedding.

use super::Kb;
use crate::domain::hash::Digest;
use crate::domain::note::Location;
use crate::domain::path::NotePath;
use anyhow::Result;
use serde::Serialize;

/// Where facts go when no note is named.
pub const INBOX: &str = "Inbox.md";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RememberReport {
    /// False when the same words were already stored (nothing written).
    pub stored: bool,
    pub note: NotePath,
    /// Where the same words already sit (when `stored` is false).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub known_at: Vec<Location>,
    /// Merkle root after the call.
    pub root: Digest,
    /// Chunks embedded by the call (the new fact, plus any pending vault edits).
    pub embedded: usize,
}

impl Kb<'_> {
    pub fn remember(
        &self,
        text: &str,
        note: &NotePath,
        heading: Option<&str>,
    ) -> Result<RememberReport> {
        let _ = (text, note, heading);
        todo!()
    }
}
