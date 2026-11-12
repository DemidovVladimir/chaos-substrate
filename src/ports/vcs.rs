//! Port: version control + source-tree listing (git CLI adapter).

use anyhow::Result;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
    /// `from` = old path (to be purged); the change's `path` is the new path.
    Renamed {
        from: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Repo-relative path with `/` separators.
    pub path: String,
    pub status: ChangeStatus,
}

/// Per-directory rule files the listing honours (besides `.git/info/exclude`
/// and the global excludes). A change to one can add or remove listed files
/// below its directory that git reports no change for.
pub const IGNORE_FILES: &[&str] = &[".gitignore", ".ignore"];

/// Every path is relative to `root` (the indexed directory, which may sit
/// below the git top level) with `/` separators, unquoted.
pub trait Vcs: Send + Sync {
    fn head_sha(&self, root: &Path) -> Result<Option<String>>;
    fn is_clean(&self, root: &Path) -> Result<bool>;
    /// Uncommitted changes: worktree and index vs HEAD, plus untracked
    /// non-ignored files (as `Added`). Deleted and renamed-away paths included.
    /// Errors when `root` is not inside a git repository.
    fn worktree_changes(&self, root: &Path) -> Result<Vec<Change>>;
    /// Two-tree diff `base` → `head` with rename detection. Ancestry is not
    /// required (a branch switch or a rebase diffs fine); both commits must exist.
    fn diff_commits(&self, root: &Path, base: &str, head: &str) -> Result<Vec<Change>>;
    /// The full sha of `rev` peeled to a commit; `None` when no such commit is
    /// in this clone (garbage-collected, shallow clone, another clone's sha).
    fn resolve_commit(&self, root: &Path, rev: &str) -> Result<Option<String>>;
    /// Every indexable file: regular files (symlinks are never followed) not
    /// excluded by [`IGNORE_FILES`], `.git/info/exclude` or the global
    /// excludes (tracked or not), minus `skip_dirs`. Sorted, repo-relative.
    fn list_files(&self, root: &Path, skip_dirs: &[String]) -> Result<Vec<PathBuf>>;
    /// [`Vcs::list_files`] restricted to `paths`, by the same rules: a path
    /// naming a file is kept when the full listing has it; a path naming a
    /// directory keeps every listed file below it (`""` = the whole root).
    /// Sorted, `/`-separated.
    fn list_paths(
        &self,
        root: &Path,
        paths: &[String],
        skip_dirs: &[String],
    ) -> Result<Vec<String>>;
}
