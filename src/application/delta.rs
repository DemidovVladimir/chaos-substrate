//! Use-case helper: the git delta that `add` and `reindex` re-read.
//!
//! Every successful `index`, `reindex` and `add` stamps the repository with
//! the HEAD it read, the paths that were uncommitted then and the listing
//! rules it used ([`Repo::commit_sha`], [`Repo::worktree_paths`],
//! [`Repo::listing_rules`]). The next incremental run looks at three sets of
//! paths only:
//!
//! 1. committed: the two-tree diff stamp → HEAD (plus `--since` → HEAD). No
//!    ancestry is needed, so a branch switch or a rebase diffs fine as long as
//!    the stamped commit is still in this clone;
//! 2. uncommitted now: worktree and index vs HEAD, untracked files included;
//! 3. uncommitted at the stamp, so a later revert or commit of them is seen.
//!
//! Git only names the candidates; the full listing decides what is in the
//! index. Each candidate — a file, a directory git reports only as a pointer
//! (submodule, nested repository), or the directory of a changed
//! `.gitignore` / `.ignore` — goes to [`Vcs::list_paths`], the full listing
//! restricted to it: the same ignore rules, `skip_dirs` and languages, and
//! symlinks never followed. Listed files are re-read unless their content
//! hash equals the indexed one; indexed files at or below a candidate that
//! the listing does not have are purged.
//!
//! [`collect`] refuses the delta — the caller then reconciles every file or
//! runs a full index — when the stamp is missing or incomplete, the listing
//! rules changed, or the stamped commit is not in this clone.

use crate::application::index::MAX_FILE_BYTES;
use crate::domain::hash;
use crate::domain::model::{in_skipped_dir, EdgeKind, Language, Repo};
use crate::ports::index_store::Graph;
use crate::ports::vcs::{Change, ChangeStatus, Vcs, IGNORE_FILES};
use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use uuid::Uuid;

/// Indexed files: repo-relative path → content hash, sorted so a directory's
/// files are one range.
pub type Stored = BTreeMap<String, String>;

/// What an incremental run looked at (reported by `add` and `reindex`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DeltaSummary {
    /// The stamped commit the delta starts from (`None`: never stamped).
    pub base: Option<String>,
    /// The `--since` commit, whose diff to HEAD is checked as well.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    pub head: Option<String>,
    /// Relevant paths changed between the base (or `since`) and HEAD; both
    /// sides of a rename count.
    pub committed: usize,
    /// Relevant paths uncommitted now (worktree + index vs HEAD, untracked).
    pub uncommitted: usize,
    /// Relevant paths that were uncommitted when the stamp was written.
    pub previous_worktree: usize,
    /// Directories reconciled below them: submodules, nested repositories and
    /// the directory of a changed `.gitignore` / `.ignore`.
    pub directories: usize,
    /// Listed files whose content hash equals the indexed one: no work.
    pub unchanged: usize,
    /// Unchanged files re-read because they import or reference a changed
    /// file (`reindex` only).
    pub dependents: usize,
    /// Set when the delta could not run: why the full path ran instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

/// The work of one incremental run plus the stamp it writes on success.
#[derive(Debug, Clone, Default)]
pub struct Delta {
    /// Listed files that are new or whose content changed.
    pub to_read: BTreeSet<String>,
    /// Indexed paths to delete before the merge: removed, renamed away, no
    /// longer listed, and every indexed path in `to_read` (re-inserted).
    pub purge: BTreeSet<String>,
    /// Stamp: the HEAD read at the start of the run …
    pub head: Option<String>,
    /// … and the paths uncommitted at that moment.
    pub worktree: Vec<String>,
    pub summary: DeltaSummary,
}

impl Delta {
    /// A run with nothing to read and nothing to purge.
    pub fn is_empty(&self) -> bool {
        self.to_read.is_empty() && self.purge.is_empty()
    }

    /// A file the listing has: re-read it unless the disk holds exactly the
    /// indexed content.
    fn check_listed(&mut self, root: &Path, path: &str, stored: &Stored) {
        if let Some(indexed) = stored.get(path) {
            if same_content(&root.join(path), indexed) {
                self.summary.unchanged += 1;
                return;
            }
            self.purge.insert(path.to_string());
        }
        self.to_read.insert(path.to_string());
    }
}

/// The file at `abs` hashes to `indexed` (what `index` stored for it).
fn same_content(abs: &Path, indexed: &str) -> bool {
    std::fs::metadata(abs).is_ok_and(|m| m.len() <= MAX_FILE_BYTES)
        && std::fs::read(abs)
            .is_ok_and(|b| hash::sha256_hex(&String::from_utf8_lossy(&b)) == indexed)
}

/// Git's side of the delta, gathered before the index is consulted.
#[derive(Debug, Clone)]
pub struct Pending {
    committed: Vec<Change>,
    uncommitted: Vec<Change>,
    previous: Vec<String>,
    summary: DeltaSummary,
}

/// Outcome of [`collect`].
#[derive(Debug, Clone)]
pub enum Collected {
    Ready(Pending),
    /// The delta cannot be trusted; the reason goes into the report.
    Fallback(String),
}

/// Ask git for the three path sets of the delta. `rules` is the
/// [`crate::domain::model::listing_rules`] of this run. Falls back (never
/// errors) when the repository has no HEAD, the stamp is missing or
/// incomplete, the listing rules changed, the stamped commit is not in this
/// clone, or git fails; errors only when `since` names no commit.
pub fn collect(
    vcs: &dyn Vcs,
    root: &Path,
    repo: &Repo,
    since: Option<&str>,
    rules: &str,
) -> Result<Collected> {
    let fallback = |why: String| Ok(Collected::Fallback(why));
    let head = match vcs.head_sha(root) {
        Ok(Some(h)) => h,
        Ok(None) => {
            return fallback("no HEAD commit (not a git repository, or nothing committed)".into())
        }
        Err(e) => return fallback(format!("git failed: {e:#}")),
    };
    let since = match since {
        Some(rev) => match vcs.resolve_commit(root, rev)? {
            Some(sha) => Some(sha),
            None => bail!("`since` {rev} is not a commit in {}", root.display()),
        },
        None => None,
    };
    let Some(previous) = repo.worktree_paths.clone() else {
        return fallback(
            "the stamp is incomplete: the last run did not finish, or the stamp predates worktree tracking"
                .into(),
        );
    };
    if repo.listing_rules.as_deref() != Some(rules) {
        return fallback(
            "the indexing rules changed since the stamp (skip_dirs or the indexable languages)"
                .into(),
        );
    }
    let Some(stamp) = repo.commit_sha.as_deref() else {
        return fallback("the repository has no stamped commit".into());
    };
    let base = match vcs.resolve_commit(root, stamp) {
        Ok(Some(b)) => b,
        Ok(None) => {
            return fallback(format!(
                "stamped commit {stamp} is not in this clone (garbage-collected, shallow clone or another clone)"
            ))
        }
        Err(e) => return fallback(format!("git failed: {e:#}")),
    };
    let mut committed = Vec::new();
    for from in std::iter::once(&base).chain(since.as_ref()) {
        if *from == head {
            continue;
        }
        match vcs.diff_commits(root, from, &head) {
            Ok(c) => committed.extend(c),
            Err(e) => return fallback(format!("git diff failed: {e:#}")),
        }
    }
    let uncommitted = match vcs.worktree_changes(root) {
        Ok(c) => c,
        Err(e) => return fallback(format!("git status failed: {e:#}")),
    };
    Ok(Collected::Ready(Pending {
        committed,
        uncommitted,
        previous,
        summary: DeltaSummary {
            base: Some(base),
            since,
            head: Some(head),
            ..Default::default()
        },
    }))
}

fn norm(path: &str) -> String {
    path.trim_end_matches('/').to_string()
}

/// Every path a change set names: both sides of a rename.
fn touched(changes: &[Change]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for c in changes {
        if let ChangeStatus::Renamed { from } = &c.status {
            out.insert(norm(from));
        }
        out.insert(norm(&c.path));
    }
    out
}

/// Indexed paths strictly below directory `dir` (`""`: all of them).
fn indexed_below<'a>(stored: &'a Stored, dir: &str) -> impl Iterator<Item = &'a String> + 'a {
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    stored
        .range(prefix.clone()..)
        .map(|(p, _)| p)
        .take_while(move |p| p.starts_with(&prefix))
}

/// Indexed paths at or below `scope`.
fn indexed_in<'a>(stored: &'a Stored, scope: &str) -> Vec<&'a String> {
    let mut out: Vec<&String> = stored
        .get_key_value(scope)
        .map(|(k, _)| k)
        .into_iter()
        .collect();
    out.extend(indexed_below(stored, scope));
    out
}

/// A real directory at `path` (a symlink to one is not), or a path the index
/// holds files below.
fn is_tree(root: &Path, path: &str, stored: &Stored) -> bool {
    path.is_empty()
        || std::fs::symlink_metadata(root.join(path)).is_ok_and(|m| m.is_dir())
        || indexed_below(stored, path).next().is_some()
}

fn is_ignore_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    IGNORE_FILES.contains(&name)
}

/// The listing scope a candidate stands for: a changed ignore file stands
/// for its whole directory (`""` = the root).
fn scope_of(path: &str) -> String {
    if is_ignore_file(path) {
        path.rsplit_once('/')
            .map(|(dir, _)| dir.to_string())
            .unwrap_or_default()
    } else {
        path.to_string()
    }
}

/// A path the delta cares about: something is indexed at or below it, or the
/// listing could have it (outside `skip_dirs`: a supported language, an
/// ignore file, or a real directory).
fn relevant(root: &Path, path: &str, stored: &Stored, skip_dirs: &[String]) -> bool {
    if stored.contains_key(path) || indexed_below(stored, path).next().is_some() {
        return true;
    }
    !in_skipped_dir(path, skip_dirs)
        && (Language::from_path(path).is_some()
            || is_ignore_file(path)
            || std::fs::symlink_metadata(root.join(path)).is_ok_and(|m| m.is_dir()))
}

/// The paths to stamp from uncommitted `changes`: both sides of renames,
/// filtered to what the delta cares about.
pub fn stamp_paths(
    changes: &[Change],
    root: &Path,
    skip_dirs: &[String],
    stored: &Stored,
) -> Vec<String> {
    touched(changes)
        .into_iter()
        .filter(|p| relevant(root, p, stored, skip_dirs))
        .collect()
}

/// Content-hash reconcile of `scopes` (`None` = the whole repository): listed
/// files that are new or differ from the index are re-read, indexed files at
/// or below a scope that the listing does not have are purged. Returns
/// `(listed, gone)`.
pub fn reconcile(
    vcs: &dyn Vcs,
    root: &Path,
    scopes: Option<&BTreeSet<String>>,
    stored: &Stored,
    skip_dirs: &[String],
    delta: &mut Delta,
) -> Result<(usize, usize)> {
    let listed: BTreeSet<String> = match scopes {
        None => vcs
            .list_files(root, skip_dirs)?
            .into_iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect(),
        Some(s) if s.is_empty() => BTreeSet::new(),
        Some(s) => {
            let paths: Vec<String> = s.iter().cloned().collect();
            vcs.list_paths(root, &paths, skip_dirs)?
                .into_iter()
                .collect()
        }
    };
    let indexed: Vec<&String> = match scopes {
        None => stored.keys().collect(),
        Some(s) => s.iter().flat_map(|s| indexed_in(stored, s)).collect(),
    };
    let mut gone = 0usize;
    for p in indexed {
        if !listed.contains(p) && delta.purge.insert(p.clone()) {
            gone += 1;
        }
    }
    for p in &listed {
        delta.check_listed(root, p, stored);
    }
    Ok((listed.len(), gone))
}

impl Pending {
    /// Nothing changed in git and nothing was uncommitted at the stamp.
    pub fn is_empty(&self) -> bool {
        self.committed.is_empty() && self.uncommitted.is_empty() && self.previous.is_empty()
    }

    /// Turn git's path sets into the work against the index in `stored`.
    pub fn resolve(
        self,
        vcs: &dyn Vcs,
        root: &Path,
        stored: &Stored,
        skip_dirs: &[String],
    ) -> Result<Delta> {
        let mut cache: HashMap<String, bool> = HashMap::new();
        let mut keep = |paths: BTreeSet<String>| -> BTreeSet<String> {
            paths
                .into_iter()
                .filter(|p| {
                    *cache
                        .entry(p.clone())
                        .or_insert_with(|| relevant(root, p, stored, skip_dirs))
                })
                .collect()
        };
        let committed = keep(touched(&self.committed));
        let uncommitted = keep(touched(&self.uncommitted));
        let previous = keep(self.previous.iter().map(|p| norm(p)).collect());

        let scopes: BTreeSet<String> = committed
            .iter()
            .chain(&uncommitted)
            .chain(&previous)
            .map(|p| scope_of(p))
            .collect();
        let mut summary = self.summary;
        summary.committed = committed.len();
        summary.uncommitted = uncommitted.len();
        summary.previous_worktree = previous.len();
        summary.directories = scopes.iter().filter(|s| is_tree(root, s, stored)).count();
        let mut delta = Delta {
            head: summary.head.clone(),
            worktree: uncommitted.into_iter().collect(),
            summary,
            ..Default::default()
        };
        reconcile(vcs, root, Some(&scopes), stored, skip_dirs, &mut delta)?;
        Ok(delta)
    }
}

/// Unchanged files one hop upstream of `changed` in the persisted graph: the
/// source file of any cross-file reference into a changed file — `imports`,
/// `calls`, `extends`, `implements`, `queries`, `emits`, `reverts`, `binds`,
/// `resolves` or `uses_abi`. Re-analyzing them lets their edges reach
/// symbols the change added.
pub fn dependents(graph: &Graph, changed: &BTreeSet<String>) -> BTreeSet<String> {
    let path_of: HashMap<Uuid, &str> = graph
        .files
        .iter()
        .map(|f| (f.id, f.path.as_str()))
        .collect();
    let file_of: HashMap<Uuid, &str> = graph
        .nodes
        .iter()
        .filter_map(|n| Some((n.id, *path_of.get(&n.file_id?)?)))
        .collect();
    let mut out = BTreeSet::new();
    for e in &graph.edges {
        // Exhaustive on purpose: a new edge kind does not compile until
        // someone decides whether it makes its source a dependent.
        match e.kind {
            EdgeKind::Imports
            | EdgeKind::Calls
            | EdgeKind::Extends
            | EdgeKind::Implements
            | EdgeKind::Queries
            | EdgeKind::Emits
            | EdgeKind::Reverts
            | EdgeKind::Binds
            | EdgeKind::Resolves
            | EdgeKind::UsesAbi => {}
            // Structure inside one file, not a reference into another.
            EdgeKind::Contains | EdgeKind::Defines => continue,
        }
        let (Some(src), Some(dst)) = (
            file_of.get(&e.source_node_id),
            file_of.get(&e.target_node_id),
        ) else {
            continue;
        };
        if changed.contains(*dst) && !changed.contains(*src) {
            out.insert(src.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::index::fakes::{edge, node, stored_file, FakeVcs};
    use crate::domain::model::{listing_rules, NodeKind};

    const BASE: &str = "base0000";
    const HEAD: &str = "head1111";

    fn ch(path: &str, status: ChangeStatus) -> Change {
        Change {
            path: path.into(),
            status,
        }
    }

    fn renamed(from: &str, to: &str) -> Change {
        ch(to, ChangeStatus::Renamed { from: from.into() })
    }

    fn rules() -> String {
        listing_rules(&skip(), &[])
    }

    fn repo(prev: &[&str]) -> Repo {
        Repo {
            id: Uuid::nil(),
            name: "r".into(),
            root_path: "/r".into(),
            commit_sha: Some(BASE.into()),
            worktree_paths: Some(prev.iter().map(|p| p.to_string()).collect()),
            listing_rules: Some(rules()),
        }
    }

    fn vcs(committed: Vec<Change>, uncommitted: Vec<Change>) -> FakeVcs {
        FakeVcs {
            head: Some(HEAD.into()),
            diffs: [(BASE.to_string(), committed)].into_iter().collect(),
            changes: uncommitted,
            ..Default::default()
        }
    }

    fn write(root: &Path, path: &str, content: &str) {
        let abs = root.join(path);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(abs, content).unwrap();
    }

    fn stored(entries: &[(&str, &str)]) -> Stored {
        entries
            .iter()
            .map(|(p, content)| (p.to_string(), hash::sha256_hex(content)))
            .collect()
    }

    fn skip() -> Vec<String> {
        vec!["node_modules".into(), "build".into()]
    }

    fn run(v: &FakeVcs, root: &Path, repo: &Repo, since: Option<&str>, stored: &Stored) -> Delta {
        match collect(v, root, repo, since, &rules()).unwrap() {
            Collected::Ready(p) => p.resolve(v, root, stored, &skip()).unwrap(),
            Collected::Fallback(why) => panic!("unexpected fallback: {why}"),
        }
    }

    fn set(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn committed_changes_rename_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/mod.rs", "fn changed() {}");
        write(r, "src/new.rs", "fn new() {}");
        write(r, "src/moved.rs", "fn m() {}");
        let st = stored(&[
            ("src/mod.rs", "fn old() {}"),
            ("src/gone.rs", "fn gone() {}"),
            ("src/old.rs", "fn m() {}"),
            ("src/keep.rs", "fn keep() {}"),
        ]);
        let v = vcs(
            vec![
                ch("src/mod.rs", ChangeStatus::Modified),
                ch("src/new.rs", ChangeStatus::Added),
                ch("src/gone.rs", ChangeStatus::Deleted),
                renamed("src/old.rs", "src/moved.rs"),
                ch("Cargo.toml", ChangeStatus::Modified),
            ],
            vec![],
        );
        let d = run(&v, r, &repo(&[]), None, &st);
        assert_eq!(
            d.to_read,
            set(&["src/mod.rs", "src/moved.rs", "src/new.rs"])
        );
        assert_eq!(d.purge, set(&["src/gone.rs", "src/mod.rs", "src/old.rs"]));
        assert_eq!(d.summary.committed, 5, "both rename sides; not Cargo.toml");
        assert_eq!(d.summary.base.as_deref(), Some(BASE));
        assert_eq!(d.summary.head.as_deref(), Some(HEAD));
        assert_eq!(d.head.as_deref(), Some(HEAD));
        assert!(d.worktree.is_empty());
        // Only the delta was looked at: no listing of the repository.
        assert!(!v.called("list_files"));
        assert!(v.called(&format!("diff_commits:{BASE}..{HEAD}")));
        assert!(v.called("list_paths:src/gone.rs,src/mod.rs,src/moved.rs,src/new.rs,src/old.rs"));
    }

    #[test]
    fn untracked_and_uncommitted_files_are_read_and_stamped() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/un tracked é.ts", "export const u = 1;");
        write(r, "src/edit.rs", "fn edited() {}");
        write(r, "notes.txt", "x");
        let st = stored(&[("src/edit.rs", "fn e() {}"), ("src/del.rs", "fn d() {}")]);
        let v = vcs(
            vec![],
            vec![
                ch("src/un tracked é.ts", ChangeStatus::Added),
                ch("src/edit.rs", ChangeStatus::Modified),
                ch("src/del.rs", ChangeStatus::Deleted),
                ch("notes.txt", ChangeStatus::Added),
            ],
        );
        let d = run(&v, r, &repo(&[]), None, &st);
        assert_eq!(d.to_read, set(&["src/edit.rs", "src/un tracked é.ts"]));
        assert_eq!(d.purge, set(&["src/del.rs", "src/edit.rs"]));
        assert_eq!(d.summary.uncommitted, 3);
        assert_eq!(
            d.worktree,
            vec!["src/del.rs", "src/edit.rs", "src/un tracked é.ts"],
            "indexable uncommitted paths are stamped, deletions included"
        );
    }

    #[test]
    fn skipped_dirs_and_files_leaving_the_indexable_set() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "node_modules/x/i.js", "1");
        write(r, "pkg/build/out.ts", "1");
        write(r, "src/a.txt", "fn a() {}");
        let st = stored(&[("src/a.rs", "fn a() {}"), ("pkg/build/old.ts", "1")]);
        let v = vcs(
            vec![
                ch("node_modules/x/i.js", ChangeStatus::Added),
                ch("pkg/build/out.ts", ChangeStatus::Added),
                ch("pkg/build/old.ts", ChangeStatus::Modified),
            ],
            vec![renamed("src/a.rs", "src/a.txt")],
        );
        let d = run(&v, r, &repo(&[]), None, &st);
        assert!(d.to_read.is_empty(), "{:?}", d.to_read);
        // An indexed file in a skipped directory and the renamed-away source.
        assert_eq!(d.purge, set(&["pkg/build/old.ts", "src/a.rs"]));
        assert_eq!(d.worktree, vec!["src/a.rs"]);
    }

    #[test]
    fn dirty_at_the_stamp_then_reverted_is_reread() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        // The index holds the dirty edit; the disk is back at HEAD.
        write(r, "src/a.rs", "fn clean() {}");
        write(r, "src/same.rs", "fn same() {}");
        let st = stored(&[
            ("src/a.rs", "fn dirty() {}"),
            ("src/same.rs", "fn same() {}"),
            ("src/tmp.rs", "fn tmp() {}"),
        ]);
        let v = FakeVcs {
            head: Some(BASE.into()),
            ..Default::default()
        };
        let prev = repo(&["src/a.rs", "src/same.rs", "src/tmp.rs"]);
        let d = run(&v, r, &prev, None, &st);
        assert_eq!(d.to_read, set(&["src/a.rs"]));
        // The untracked file from last time is gone from disk.
        assert_eq!(d.purge, set(&["src/a.rs", "src/tmp.rs"]));
        assert_eq!(d.summary.unchanged, 1, "src/same.rs: same content hash");
        assert_eq!(d.summary.previous_worktree, 3);
        assert!(d.worktree.is_empty(), "nothing uncommitted any more");
        // HEAD is the stamp: no diff needed.
        assert!(!v.called("diff_commits"));
    }

    #[test]
    fn unchanged_content_is_skipped_by_hash() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/a.rs", "fn a() {}");
        let st = stored(&[("src/a.rs", "fn a() {}")]);
        // Indexed while dirty, committed since: same bytes, nothing to do.
        let v = vcs(vec![ch("src/a.rs", ChangeStatus::Modified)], vec![]);
        let d = run(&v, r, &repo(&["src/a.rs"]), None, &st);
        assert!(d.is_empty());
        assert_eq!(d.summary.unchanged, 1);
    }

    #[test]
    fn the_listing_decides_presence_not_git() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        // `git rm --cached` of an ignored file: still on disk, but not listed.
        write(r, "src/gen.rs", "fn gen() {}");
        write(r, "src/back.rs", "fn back() {}");
        // Deleted in git but present on disk and not ignored: listed.
        write(r, "src/kept.rs", "fn kept() {}");
        let st = stored(&[("src/gen.rs", "fn gen() {}")]);
        let mut v = vcs(
            vec![
                ch("src/gen.rs", ChangeStatus::Added),
                ch("src/kept.rs", ChangeStatus::Deleted),
            ],
            vec![
                ch("src/gen.rs", ChangeStatus::Deleted),
                // Deleted from the index but re-added untracked: present.
                ch("src/back.rs", ChangeStatus::Deleted),
                ch("src/back.rs", ChangeStatus::Added),
            ],
        );
        v.ignored.push("src/gen.rs".into());
        let d = run(&v, r, &repo(&[]), None, &st);
        assert_eq!(d.purge, set(&["src/gen.rs"]));
        assert_eq!(d.to_read, set(&["src/back.rs", "src/kept.rs"]));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_ignored_files_are_never_read() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/b.ts", "export const b = 1;");
        write(r, "shared/lib.ts", "export const l = 1;");
        write(r, "gen/g.ts", "export const g = 2;");
        std::os::unix::fs::symlink("b.ts", r.join("src/link.ts")).unwrap();
        std::os::unix::fs::symlink("shared", r.join("alias")).unwrap();
        let st = stored(&[
            ("src/b.ts", "export const b = 1;"),
            ("shared/lib.ts", "export const l = 1;"),
            // Indexed by an older run through the symlinked directory.
            ("alias/lib.ts", "export const l = 1;"),
            ("gen/g.ts", "export const g = 1;"),
        ]);
        let mut v = vcs(
            vec![
                ch("alias", ChangeStatus::Added),
                ch("gen/g.ts", ChangeStatus::Modified),
                ch("src/link.ts", ChangeStatus::Added),
            ],
            vec![],
        );
        // A tracked file under a gitignored directory.
        v.ignored.push("gen".into());
        let d = run(&v, r, &repo(&[]), None, &st);
        assert!(d.to_read.is_empty(), "{:?}", d.to_read);
        assert_eq!(d.purge, set(&["alias/lib.ts", "gen/g.ts"]));
        assert_eq!(
            d.summary.directories, 1,
            "alias is a tree only through what the index holds below it"
        );
    }

    #[test]
    fn a_changed_ignore_file_reconciles_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "gen/g.ts", "export const g = 1;");
        write(r, "src/a.ts", "export const a = 1;");
        write(r, "docs/a.md", "# a");
        write(r, "docs/new.md", "# new");
        write(r, "docs/secret.md", "# s");
        let st = stored(&[
            ("gen/g.ts", "export const g = 1;"),
            ("src/a.ts", "export const a = 1;"),
            ("docs/a.md", "# a"),
            ("docs/secret.md", "# s"),
        ]);
        // `docs/.ignore` now hides secret.md and no longer hides new.md.
        let mut v = vcs(vec![ch("docs/.ignore", ChangeStatus::Modified)], vec![]);
        v.ignored.push("docs/secret.md".into());
        let d = run(&v, r, &repo(&[]), None, &st);
        assert_eq!(d.to_read, set(&["docs/new.md"]));
        assert_eq!(d.purge, set(&["docs/secret.md"]));
        assert_eq!(
            d.summary.unchanged, 1,
            "docs/a.md only: src/ is not looked at"
        );
        assert!(v.called("list_paths:docs"));
        // A root `.gitignore` edit, uncommitted: the whole repository, and the
        // edit is stamped so its revert is seen too.
        let mut v = vcs(vec![], vec![ch(".gitignore", ChangeStatus::Modified)]);
        v.ignored.push("gen".into());
        let d = run(&v, r, &repo(&[]), None, &st);
        assert_eq!(d.to_read, set(&["docs/new.md"]));
        assert_eq!(d.purge, set(&["gen/g.ts"]));
        assert_eq!(d.summary.unchanged, 3);
        assert_eq!(d.summary.directories, 1);
        assert_eq!(d.worktree, vec![".gitignore"]);
        assert!(v.calls.lock().unwrap().contains(&"list_paths:".to_string()));
    }

    #[test]
    fn branch_switch_diffs_two_trees_without_ancestry() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/right.rs", "fn right() {}");
        let st = stored(&[("src/left.rs", "fn left() {}")]);
        // The stamp is the tip of another branch: the fake answers the
        // two-tree diff; nothing asks for a merge base.
        let v = vcs(
            vec![
                ch("src/left.rs", ChangeStatus::Deleted),
                ch("src/right.rs", ChangeStatus::Added),
            ],
            vec![],
        );
        let d = run(&v, r, &repo(&[]), None, &st);
        assert_eq!(d.to_read, set(&["src/right.rs"]));
        assert_eq!(d.purge, set(&["src/left.rs"]));
        assert_eq!(
            v.calls.lock().unwrap().as_slice(),
            &[
                format!("diff_commits:{BASE}..{HEAD}"),
                "worktree_changes".to_string(),
                "list_paths:src/left.rs,src/right.rs".to_string()
            ]
        );
    }

    #[test]
    fn since_adds_its_range_to_the_stamp_delta() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/a.rs", "fn a2() {}");
        write(r, "src/b.rs", "fn b2() {}");
        let st = stored(&[("src/a.rs", "fn a() {}"), ("src/b.rs", "fn b() {}")]);
        let mut v = vcs(vec![ch("src/a.rs", ChangeStatus::Modified)], vec![]);
        v.diffs
            .insert("v1".into(), vec![ch("src/b.rs", ChangeStatus::Modified)]);
        let d = run(&v, r, &repo(&[]), Some("v1"), &st);
        assert_eq!(d.to_read, set(&["src/a.rs", "src/b.rs"]));
        assert_eq!(d.summary.since.as_deref(), Some("v1"));
        v.missing.push("nope".into());
        let err = collect(&v, r, &repo(&[]), Some("nope"), &rules()).unwrap_err();
        assert!(err.to_string().contains("not a commit"), "{err}");
    }

    #[test]
    fn unusable_stamp_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        let why = |v: &FakeVcs, repo: &Repo| match collect(v, r, repo, None, &rules()).unwrap() {
            Collected::Fallback(why) => why,
            Collected::Ready(_) => panic!("expected a fallback"),
        };
        let mut v = vcs(vec![], vec![]);
        v.missing.push(BASE.into());
        assert!(why(&v, &repo(&[])).contains("not in this clone"));
        let ok = vcs(vec![], vec![]);
        let unstamped = Repo {
            commit_sha: None,
            ..repo(&[])
        };
        assert!(why(&ok, &unstamped).contains("no stamped commit"));
        // A run that started writing and died, or a row from before 1.4.
        let incomplete = Repo {
            worktree_paths: None,
            ..repo(&[])
        };
        assert!(why(&ok, &incomplete).contains("the stamp is incomplete"));
        // skip_dirs changed (or a language was added) since the stamp.
        let other_rules = Repo {
            listing_rules: Some(listing_rules(&["tests".into()], &[])),
            ..repo(&[])
        };
        assert!(why(&ok, &other_rules).contains("indexing rules changed"));
        let no_rules = Repo {
            listing_rules: None,
            ..repo(&[])
        };
        assert!(why(&ok, &no_rules).contains("indexing rules changed"));
        assert!(why(&FakeVcs::default(), &repo(&[])).contains("no HEAD"));
        let broken = FakeVcs {
            broken: true,
            ..vcs(vec![], vec![])
        };
        assert!(why(&broken, &repo(&[])).contains("git diff failed"));
    }

    #[test]
    fn a_directory_is_reconciled_below_it() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        // A submodule moved its pointer: git names only the directory.
        write(r, "lib/dep/src/a.sol", "contract A2 {}");
        write(r, "lib/dep/src/same.sol", "contract S {}");
        write(r, "lib/dep/README", "not indexable");
        let st = stored(&[
            ("lib/dep/src/a.sol", "contract A {}"),
            ("lib/dep/src/same.sol", "contract S {}"),
            ("lib/dep/src/gone.sol", "contract G {}"),
            ("lib/depot.sol", "contract D {}"),
        ]);
        let v = vcs(vec![ch("lib/dep", ChangeStatus::Modified)], vec![]);
        let d = run(&v, r, &repo(&[]), None, &st);
        assert_eq!(d.summary.directories, 1);
        assert_eq!(d.to_read, set(&["lib/dep/src/a.sol"]));
        assert_eq!(d.purge, set(&["lib/dep/src/a.sol", "lib/dep/src/gone.sol"]));
        assert_eq!(d.summary.unchanged, 1);
        // An untracked nested repository is stamped as a directory.
        let v = vcs(vec![], vec![ch("lib/dep/", ChangeStatus::Added)]);
        let d = run(&v, r, &repo(&[]), None, &st);
        assert_eq!(d.worktree, vec!["lib/dep"]);
    }

    #[test]
    fn dependents_are_one_hop_upstream() {
        let repo_id = Uuid::nil();
        let files: Vec<_> = ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"]
            .iter()
            .map(|p| stored_file(repo_id, p, p))
            .collect();
        let file_node = |i: usize| crate::domain::model::Node {
            file_id: Some(files[i].id),
            ..node(
                repo_id,
                &format!("file:{}", files[i].path),
                NodeKind::File,
                None,
            )
        };
        let sym = |i: usize, name: &str| crate::domain::model::Node {
            file_id: Some(files[i].id),
            ..node(
                repo_id,
                &format!("rust:{}:function:{name}", files[i].path),
                NodeKind::Function,
                None,
            )
        };
        let (fa, fb, fc) = (file_node(0), file_node(1), file_node(2));
        let (a, b, d) = (sym(0, "a"), sym(1, "b"), sym(3, "d"));
        let graph = Graph {
            edges: vec![
                // a.rs imports b.rs; d calls a; c.rs contains-only.
                edge(repo_id, fa.id, fb.id, EdgeKind::Imports),
                edge(repo_id, d.id, a.id, EdgeKind::Calls),
                edge(repo_id, fc.id, b.id, EdgeKind::Contains),
            ],
            nodes: vec![fa, fb, fc, a, b, d],
            files,
        };
        assert_eq!(dependents(&graph, &set(&["src/b.rs"])), set(&["src/a.rs"]));
        assert_eq!(dependents(&graph, &set(&["src/a.rs"])), set(&["src/d.rs"]));
        // A dependent that changed itself is not listed again.
        assert!(dependents(&graph, &set(&["src/a.rs", "src/d.rs"])).is_empty());
    }

    /// The 1.4 edge kinds are cross-file references too: a changed schema,
    /// ABI, event/error declaration or resolver file re-reads the file that
    /// points into it.
    #[test]
    fn solidity_abi_and_graphql_edges_make_dependents() {
        let repo_id = Uuid::nil();
        let paths = [
            "schema.graphql",
            "lambda/index.ts",
            "cdk/config.ts",
            "abis/Token.json",
            "app/read.ts",
            "src/IEvents.sol",
            "src/A.sol",
        ];
        let files: Vec<_> = paths.iter().map(|p| stored_file(repo_id, p, p)).collect();
        let sym = |i: usize| crate::domain::model::Node {
            file_id: Some(files[i].id),
            ..node(
                repo_id,
                &format!("x:{}:sym", files[i].path),
                NodeKind::Function,
                None,
            )
        };
        let n: Vec<_> = (0..paths.len()).map(sym).collect();
        let graph = Graph {
            edges: vec![
                edge(repo_id, n[1].id, n[0].id, EdgeKind::Resolves),
                edge(repo_id, n[2].id, n[1].id, EdgeKind::Binds),
                edge(repo_id, n[4].id, n[3].id, EdgeKind::UsesAbi),
                edge(repo_id, n[6].id, n[5].id, EdgeKind::Emits),
                edge(repo_id, n[6].id, n[5].id, EdgeKind::Reverts),
            ],
            nodes: n,
            files,
        };
        let deps = |changed: &str| dependents(&graph, &set(&[changed]));
        assert_eq!(deps("schema.graphql"), set(&["lambda/index.ts"]));
        assert_eq!(deps("lambda/index.ts"), set(&["cdk/config.ts"]));
        assert_eq!(deps("abis/Token.json"), set(&["app/read.ts"]));
        assert_eq!(deps("src/IEvents.sol"), set(&["src/A.sol"]));
    }
}
