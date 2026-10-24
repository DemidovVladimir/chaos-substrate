//! The vault: a folder of Markdown notes, as Obsidian keeps them. Notes are
//! parsed with pulldown-cmark into blocks; the folder is only ever written by
//! [`Vault::append`], atomically.

use crate::domain::note::Note;
use crate::domain::path::NotePath;
use crate::ports::Vault;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Notes larger than this are skipped (a knowledge note is never this big).
pub const MAX_NOTE_BYTES: u64 = 4 * 1024 * 1024;

pub struct ObsidianVault {
    root: PathBuf,
}

impl ObsidianVault {
    /// A vault rooted at an existing folder.
    pub fn new(root: &Path) -> Result<ObsidianVault> {
        let _ = root;
        todo!()
    }
}

/// Parse one note's Markdown into blocks (pure; no file access).
pub fn parse(path: NotePath, markdown: &str) -> Note {
    let _ = (path, markdown);
    todo!()
}

impl Vault for ObsidianVault {
    fn notes(&self) -> Result<Vec<Note>> {
        let _ = &self.root;
        todo!()
    }

    fn append(&self, note: &NotePath, heading: Option<&str>, text: &str) -> Result<()> {
        let _ = (note, heading, text);
        todo!()
    }
}
