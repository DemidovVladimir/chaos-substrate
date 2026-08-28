//! Port: version control + source-tree listing (git CLI adapter).

use anyhow::Result;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
    /// `from` = old path (to be purged); the change's `path` is the new path.
    Renamed { from: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Repo-relative path with `/` separators.
    pub path: String,
    pub status: ChangeStatus,
}

pub trait Vcs: Send + Sync {
    fn head_sha(&self, root: &Path) -> Result<Option<String>>;
    fn is_clean(&self, root: &Path) -> Result<bool>;
    /// Changes since `since` (a ref; None = the working tree vs HEAD, including
    /// untracked non-ignored files). Deleted and renamed-away paths included.
    fn changes(&self, root: &Path, since: Option<&str>) -> Result<Vec<Change>>;
    /// Every indexable file: tracked + untracked-not-ignored, minus `skip_dirs`.
    /// Sorted, repo-relative.
    fn list_files(&self, root: &Path, skip_dirs: &[String]) -> Result<Vec<PathBuf>>;
}
