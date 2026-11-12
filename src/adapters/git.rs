//! Adapter: `git` CLI + `ignore` walker implementing [`Vcs`].
//!
//! Every method shells out to the `git` binary found on `PATH`; nothing is
//! cached, so results always reflect the working tree at call time.

use crate::domain::model::Language;
use crate::ports::vcs::{Change, ChangeStatus, Vcs};
use anyhow::{bail, Context, Result};
use ignore::WalkBuilder;
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Version-control adapter backed by the system `git` executable.
#[derive(Debug, Default, Clone, Copy)]
pub struct GitCli;

/// Run `git <args>` inside `root`; raw stdout, `Ok(None)` when git exits non-zero.
fn git_raw(root: &Path, args: &[&str]) -> Result<Option<Vec<u8>>> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| format!("failed to spawn git {}", args.join(" ")))?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(out.stdout))
}

/// Run `git <args>` inside `root`; `Ok(None)` when git exits non-zero.
fn git(root: &Path, args: &[&str]) -> Result<Option<String>> {
    Ok(git_raw(root, args)?.map(|b| String::from_utf8_lossy(&b).into_owned()))
}

/// Split NUL-terminated `-z` output into paths (no quoting to undo).
fn z_fields(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| String::from_utf8_lossy(f).replace('\\', "/"))
        .collect()
}

fn mk(path: String, status: ChangeStatus) -> Change {
    Change { path, status }
}

/// Parse `git diff --name-status -M -z`: `STATUS\0path\0` or, for a rename
/// or copy, `R<score>\0old\0new\0`. A copy leaves its source in place, so the
/// new path is `Added`.
fn parse_name_status_z(bytes: &[u8]) -> Vec<Change> {
    let mut out = Vec::new();
    let mut it = z_fields(bytes).into_iter();
    while let Some(code) = it.next() {
        let Some(a) = it.next() else { break };
        out.push(match code.chars().next().unwrap_or(' ') {
            'A' => mk(a, ChangeStatus::Added),
            'D' => mk(a, ChangeStatus::Deleted),
            'R' => match it.next() {
                Some(new) => mk(new, ChangeStatus::Renamed { from: a }),
                None => mk(a, ChangeStatus::Added),
            },
            'C' => mk(it.next().unwrap_or(a), ChangeStatus::Added),
            _ => mk(a, ChangeStatus::Modified),
        });
    }
    out
}

/// Parse `git status --porcelain=v1 -z -uall`: `XY path\0`, and for a rename
/// or copy `XY new\0old\0`. Paths are relative to the git top level; only
/// those under `prefix` (the indexed root's place in the repository) are
/// kept, with the prefix stripped. Unmerged entries count as modified, so the
/// file on disk decides.
fn parse_porcelain_z(bytes: &[u8], prefix: &str) -> Vec<Change> {
    let rel = |p: &str| p.strip_prefix(prefix).map(str::to_string);
    let mut out = Vec::new();
    let mut it = z_fields(bytes).into_iter();
    while let Some(entry) = it.next() {
        if entry.len() < 4 {
            continue;
        }
        let (xy, path) = entry.split_at(3);
        let (x, y) = (xy.as_bytes()[0] as char, xy.as_bytes()[1] as char);
        let orig = if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
            it.next()
        } else {
            None
        };
        let Some(path) = rel(path) else { continue };
        let unmerged = x == 'U' || y == 'U' || (x == y && matches!(x, 'A' | 'D'));
        let status = if &xy[..2] == "??" {
            ChangeStatus::Added
        } else if unmerged {
            ChangeStatus::Modified
        } else if x == 'R' || y == 'R' {
            match orig.as_deref().and_then(rel) {
                Some(from) => ChangeStatus::Renamed { from },
                None => ChangeStatus::Added,
            }
        } else if x == 'D' || y == 'D' {
            ChangeStatus::Deleted
        } else if x == 'C' || y == 'C' || x == 'A' {
            ChangeStatus::Added
        } else {
            ChangeStatus::Modified
        };
        out.push(mk(path, status));
    }
    out
}

/// The walker behind both listings: hidden files included; `.gitignore`,
/// `.ignore`, `.git/info/exclude` and the global excludes honoured (tracked
/// files too); symlinks not followed.
fn walker(root: &Path) -> WalkBuilder {
    let mut w = WalkBuilder::new(root);
    w.hidden(false);
    w
}

/// The regular, indexable files `walk` yields, relative to `root`. A symlink
/// is never a regular file here (`follow_links` is off), so it is skipped.
fn listed(walk: ignore::Walk, root: &Path, skip_dirs: &[String]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in walk {
        let entry = entry?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if Language::indexable(&rel_str, skip_dirs).is_none() {
            continue;
        }
        files.push(rel.to_path_buf());
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn slashed(paths: Vec<PathBuf>) -> Vec<String> {
    paths
        .into_iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect()
}

/// `rel` is one of `scopes` or lies below one.
fn within(scopes: &BTreeSet<String>, rel: &str) -> bool {
    let mut cur = rel;
    loop {
        if scopes.contains(cur) {
            return true;
        }
        match cur.rsplit_once('/') {
            Some((parent, _)) => cur = parent,
            None => return false,
        }
    }
}

fn finish(mut changes: Vec<Change>) -> Vec<Change> {
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes.dedup();
    changes
}

impl Vcs for GitCli {
    fn head_sha(&self, root: &Path) -> Result<Option<String>> {
        Ok(git(root, &["rev-parse", "--verify", "HEAD"])?
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()))
    }

    fn is_clean(&self, root: &Path) -> Result<bool> {
        match git(root, &["status", "--porcelain"])? {
            Some(s) => Ok(s.trim().is_empty()),
            None => bail!("{} is not a git repository", root.display()),
        }
    }

    fn worktree_changes(&self, root: &Path) -> Result<Vec<Change>> {
        let not_git = || format!("{} is not a git repository", root.display());
        let prefix = git(root, &["rev-parse", "--show-prefix"])?.with_context(not_git)?;
        let bytes = git_raw(
            root,
            &["status", "--porcelain=v1", "-z", "-uall", "--", "."],
        )?
        .with_context(not_git)?;
        Ok(finish(parse_porcelain_z(
            &bytes,
            prefix.trim_end_matches('\n'),
        )))
    }

    fn diff_commits(&self, root: &Path, base: &str, head: &str) -> Result<Vec<Change>> {
        let bytes = git_raw(
            root,
            &[
                "diff",
                "--name-status",
                "-M",
                "-z",
                "--no-color",
                "--no-ext-diff",
                "--relative",
                "--end-of-options",
                base,
                head,
                "--",
            ],
        )?
        .with_context(|| format!("git diff {base} {head} failed in {}", root.display()))?;
        Ok(finish(parse_name_status_z(&bytes)))
    }

    fn resolve_commit(&self, root: &Path, rev: &str) -> Result<Option<String>> {
        let peeled = format!("{rev}^{{commit}}");
        Ok(git(
            root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &peeled,
            ],
        )?
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()))
    }

    fn list_files(&self, root: &Path, skip_dirs: &[String]) -> Result<Vec<PathBuf>> {
        listed(walker(root).build(), root, skip_dirs)
    }

    /// The full listing's walk from `root`, pruned to the directories on the
    /// way to each scope and the scopes themselves, so ignore files of every
    /// ancestor apply exactly as in [`Vcs::list_files`] and an ignored or
    /// symlinked ancestor hides the scope just the same.
    fn list_paths(
        &self,
        root: &Path,
        paths: &[String],
        skip_dirs: &[String],
    ) -> Result<Vec<String>> {
        let scopes: BTreeSet<String> = paths
            .iter()
            .map(|p| p.trim_matches('/').to_string())
            .collect();
        if scopes.is_empty() {
            return Ok(Vec::new());
        }
        if scopes.contains("") {
            return Ok(slashed(self.list_files(root, skip_dirs)?));
        }
        let mut ancestors: HashSet<String> = HashSet::new();
        for s in &scopes {
            let mut cur = s.as_str();
            while let Some((parent, _)) = cur.rsplit_once('/') {
                if !ancestors.insert(parent.to_string()) {
                    break;
                }
                cur = parent;
            }
        }
        let base = root.to_path_buf();
        let mut walk = walker(root);
        walk.filter_entry(move |e| {
            let rel = e.path().strip_prefix(&base).unwrap_or(e.path());
            let rel = rel.to_string_lossy().replace('\\', "/");
            ancestors.contains(&rel) || within(&scopes, &rel)
        });
        Ok(slashed(listed(walk.build(), root, skip_dirs)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sh(root: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap()
            .status;
        assert!(ok.success(), "git {:?} failed", args);
    }

    fn repo() -> Option<tempfile::TempDir> {
        if Command::new("git").arg("--version").output().is_err() {
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        sh(r, &["init", "-q"]);
        sh(r, &["config", "user.email", "t@t"]);
        sh(r, &["config", "user.name", "t"]);
        fs::create_dir_all(r.join("src")).unwrap();
        for (f, body) in [
            ("keep", "fn keep() {}"),
            ("mod", "fn a() {}\nfn b() {}"),
            ("gone", "fn gone() {}"),
            ("old", "fn old_name() { let x = 1; let y = 2; }"),
        ] {
            fs::write(r.join(format!("src/{f}.rs")), format!("{body}\n")).unwrap();
        }
        sh(r, &["add", "."]);
        sh(r, &["commit", "-qm", "init"]);
        Some(dir)
    }

    fn mutate(r: &Path) {
        fs::write(r.join("src/new.rs"), "fn new() {}\n").unwrap();
        fs::write(r.join("src/mod.rs"), "fn a() {}\nfn b() {}\n// edit\n").unwrap();
        fs::remove_file(r.join("src/gone.rs")).unwrap();
        fs::rename(r.join("src/old.rs"), r.join("src/renamed.rs")).unwrap();
    }

    fn find<'a>(c: &'a [Change], p: &str) -> &'a ChangeStatus {
        &c.iter()
            .find(|c| c.path == p)
            .unwrap_or_else(|| panic!("{p} missing"))
            .status
    }
    fn renamed(from: &str) -> ChangeStatus {
        ChangeStatus::Renamed { from: from.into() }
    }

    #[test]
    fn head_and_clean() {
        let Some(dir) = repo() else { return };
        let r = dir.path();
        assert_eq!(GitCli.head_sha(r).unwrap().map(|s| s.len()), Some(40));
        assert!(GitCli.is_clean(r).unwrap());
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(GitCli.head_sha(plain.path()).unwrap(), None);
    }

    fn rev(r: &Path, what: &str) -> String {
        GitCli.resolve_commit(r, what).unwrap().unwrap()
    }

    #[test]
    fn diff_commits_two_trees() {
        let Some(dir) = repo() else { return };
        let r = dir.path();
        let base = rev(r, "HEAD");
        mutate(r);
        fs::write(r.join("src/a b é.rs"), "fn spaced() {}\n").unwrap();
        sh(r, &["add", "-A"]);
        sh(r, &["commit", "-qm", "second"]);
        let head = rev(r, "HEAD");
        let c = GitCli.diff_commits(r, &base, &head).unwrap();
        assert_eq!(find(&c, "src/new.rs"), &ChangeStatus::Added);
        assert_eq!(find(&c, "src/a b é.rs"), &ChangeStatus::Added);
        assert_eq!(find(&c, "src/mod.rs"), &ChangeStatus::Modified);
        assert_eq!(find(&c, "src/gone.rs"), &ChangeStatus::Deleted);
        assert_eq!(find(&c, "src/renamed.rs"), &renamed("src/old.rs"));
        assert!(!c.iter().any(|c| c.path == "src/old.rs"));
        assert!(c.windows(2).all(|w| w[0].path < w[1].path));
        // Reversed: the same two trees, the other way round.
        let back = GitCli.diff_commits(r, &head, &base).unwrap();
        assert_eq!(find(&back, "src/gone.rs"), &ChangeStatus::Added);
        assert_eq!(find(&back, "src/old.rs"), &renamed("src/renamed.rs"));
        assert!(GitCli.diff_commits(r, &head, &head).unwrap().is_empty());
    }

    #[test]
    fn diff_commits_across_branches_without_ancestry() {
        let Some(dir) = repo() else { return };
        let r = dir.path();
        sh(r, &["checkout", "-qb", "left"]);
        fs::write(r.join("src/left.rs"), "fn left() {}\n").unwrap();
        sh(r, &["add", "-A"]);
        sh(r, &["commit", "-qm", "left"]);
        let left = rev(r, "HEAD");
        sh(r, &["checkout", "-q", "HEAD~1"]);
        sh(r, &["checkout", "-qb", "right"]);
        fs::write(r.join("src/right.rs"), "fn right() {}\n").unwrap();
        sh(r, &["add", "-A"]);
        sh(r, &["commit", "-qm", "right"]);
        let right = rev(r, "HEAD");
        // Neither commit is an ancestor of the other.
        let c = GitCli.diff_commits(r, &left, &right).unwrap();
        assert_eq!(find(&c, "src/left.rs"), &ChangeStatus::Deleted);
        assert_eq!(find(&c, "src/right.rs"), &ChangeStatus::Added);
        assert_eq!(c.len(), 2, "{c:?}");
    }

    #[test]
    fn resolve_commit_checks_existence() {
        let Some(dir) = repo() else { return };
        let r = dir.path();
        let head = GitCli.head_sha(r).unwrap().unwrap();
        assert_eq!(GitCli.resolve_commit(r, &head).unwrap(), Some(head.clone()));
        assert_eq!(GitCli.resolve_commit(r, "HEAD").unwrap(), Some(head));
        // A well-formed sha this clone does not have (gc'd, another clone).
        let absent = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(GitCli.resolve_commit(r, absent).unwrap(), None);
        assert_eq!(GitCli.resolve_commit(r, "no-such-branch").unwrap(), None);
        // Never read as an option.
        assert_eq!(GitCli.resolve_commit(r, "--output=x").unwrap(), None);
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(GitCli.resolve_commit(plain.path(), "HEAD").unwrap(), None);
    }

    #[test]
    fn worktree_changes_lists_uncommitted() {
        let Some(dir) = repo() else { return };
        let r = dir.path();
        mutate(r);
        fs::write(r.join("src/un tracked é.rs"), "fn u() {}\n").unwrap();
        // Untracked new file appears as Added before any staging.
        let c = GitCli.worktree_changes(r).unwrap();
        assert_eq!(find(&c, "src/new.rs"), &ChangeStatus::Added);
        assert_eq!(find(&c, "src/un tracked é.rs"), &ChangeStatus::Added);
        assert_eq!(find(&c, "src/mod.rs"), &ChangeStatus::Modified);
        assert_eq!(find(&c, "src/gone.rs"), &ChangeStatus::Deleted);
        assert!(!GitCli.is_clean(r).unwrap());
        sh(r, &["add", "-A"]);
        let c = GitCli.worktree_changes(r).unwrap();
        assert_eq!(find(&c, "src/renamed.rs"), &renamed("src/old.rs"));
        assert!(!c.iter().any(|c| c.path == "src/old.rs"));
        // `git rm --cached` keeps the file on disk: deleted AND untracked.
        sh(r, &["commit", "-qm", "second"]);
        sh(r, &["rm", "-q", "--cached", "src/keep.rs"]);
        let c = GitCli.worktree_changes(r).unwrap();
        let keep: Vec<&ChangeStatus> = c
            .iter()
            .filter(|c| c.path == "src/keep.rs")
            .map(|c| &c.status)
            .collect();
        assert_eq!(keep.len(), 2, "{keep:?}");
        assert!(keep.contains(&&ChangeStatus::Added) && keep.contains(&&ChangeStatus::Deleted));
        let plain = tempfile::tempdir().unwrap();
        assert!(GitCli.worktree_changes(plain.path()).is_err());
    }

    #[test]
    fn paths_are_relative_to_an_indexed_subdirectory() {
        let Some(dir) = repo() else { return };
        let r = dir.path();
        let base = rev(r, "HEAD");
        fs::write(r.join("src/mod.rs"), "fn changed() {}\n").unwrap();
        fs::write(r.join("top.rs"), "fn top() {}\n").unwrap();
        sh(r, &["add", "-A"]);
        sh(r, &["commit", "-qm", "second"]);
        let head = rev(r, "HEAD");
        let sub = r.join("src");
        let c = GitCli.diff_commits(&sub, &base, &head).unwrap();
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(find(&c, "mod.rs"), &ChangeStatus::Modified);
        fs::write(r.join("src/keep.rs"), "fn edited() {}\n").unwrap();
        fs::write(r.join("other.rs"), "fn other() {}\n").unwrap();
        let c = GitCli.worktree_changes(&sub).unwrap();
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(find(&c, "keep.rs"), &ChangeStatus::Modified);
    }

    #[test]
    fn porcelain_z_parsing() {
        let raw = b"R  new name.rs\0old name.rs\0 D gone.rs\0?? sub/x\xc3\xa9.rs\0UU both.rs\0AD tmp.rs\0";
        let c = parse_porcelain_z(raw, "");
        assert_eq!(find(&c, "new name.rs"), &renamed("old name.rs"));
        assert_eq!(find(&c, "gone.rs"), &ChangeStatus::Deleted);
        assert_eq!(find(&c, "sub/xé.rs"), &ChangeStatus::Added);
        assert_eq!(find(&c, "both.rs"), &ChangeStatus::Modified);
        assert_eq!(find(&c, "tmp.rs"), &ChangeStatus::Deleted);
        let scoped = parse_porcelain_z(raw, "sub/");
        assert_eq!(scoped.len(), 1);
        assert_eq!(find(&scoped, "xé.rs"), &ChangeStatus::Added);
        let d = parse_name_status_z(b"R087\0a b.rs\0c d.rs\0C100\0src.rs\0copy.rs\0T\0t.rs\0");
        assert_eq!(find(&d, "c d.rs"), &renamed("a b.rs"));
        assert_eq!(find(&d, "copy.rs"), &ChangeStatus::Added);
        assert!(!d.iter().any(|c| c.path == "src.rs"));
        assert_eq!(find(&d, "t.rs"), &ChangeStatus::Modified);
    }

    /// The delta asks `list_paths` instead of re-implementing the listing:
    /// asked about any path, it answers exactly what the full listing has.
    #[cfg(unix)]
    #[test]
    fn list_paths_is_the_full_listing_restricted() {
        let Some(dir) = repo() else { return };
        let r = dir.path();
        // Tracked but gitignored; `.ignore`d; skipped; symlinks to a file and
        // to a directory; a path with a space.
        fs::create_dir_all(r.join("gen")).unwrap();
        fs::write(r.join("gen/g.rs"), "fn g() {}\n").unwrap();
        fs::write(r.join(".gitignore"), "gen/\n").unwrap();
        sh(r, &["add", "-f", "gen/g.rs", ".gitignore"]);
        sh(r, &["commit", "-qm", "gen"]);
        fs::create_dir_all(r.join("docs/sub dir")).unwrap();
        fs::write(r.join("docs/sub dir/a b.md"), "# a\n").unwrap();
        fs::write(r.join("docs/secret.md"), "# s\n").unwrap();
        fs::write(r.join("docs/.ignore"), "secret.md\n").unwrap();
        fs::create_dir_all(r.join("node_modules")).unwrap();
        fs::write(r.join("node_modules/x.js"), "1").unwrap();
        std::os::unix::fs::symlink("keep.rs", r.join("src/link.rs")).unwrap();
        std::os::unix::fs::symlink("src", r.join("alias")).unwrap();
        let skip = vec!["node_modules".to_string()];
        let full = slashed(GitCli.list_files(r, &skip).unwrap());
        for hidden in [
            "gen/g.rs",
            "docs/secret.md",
            "node_modules/x.js",
            "src/link.rs",
            "alias/keep.rs",
        ] {
            assert!(!full.iter().any(|p| p == hidden), "{hidden} in {full:?}");
        }
        assert!(
            full.contains(&"docs/sub dir/a b.md".to_string()),
            "{full:?}"
        );
        let ask = |paths: &[&str]| {
            let paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
            GitCli.list_paths(r, &paths, &skip).unwrap()
        };
        let expected = |scope: &str| -> Vec<String> {
            full.iter()
                .filter(|p| *p == scope || p.starts_with(&format!("{scope}/")))
                .cloned()
                .collect()
        };
        for scope in [
            "gen/g.rs",
            "gen",
            "alias",
            "alias/keep.rs",
            "src/link.rs",
            "src/keep.rs",
            "src/missing.rs",
            "src",
            "docs",
            "docs/secret.md",
            "docs/sub dir",
            "docs/sub dir/a b.md",
            "node_modules/x.js",
            ".gitignore",
        ] {
            assert_eq!(ask(&[scope]), expected(scope), "{scope}");
        }
        assert_eq!(ask(&["docs"]), vec!["docs/sub dir/a b.md"]);
        assert_eq!(ask(&["src/", "src/keep.rs"]), expected("src"));
        assert_eq!(ask(&[""]), full);
        assert!(ask(&[]).is_empty());
    }

    #[test]
    fn list_files_skips_and_ignores() {
        let Some(dir) = repo() else { return };
        let r = dir.path();
        fs::create_dir_all(r.join("node_modules/x")).unwrap();
        fs::write(r.join("node_modules/x/i.js"), "1").unwrap();
        fs::create_dir_all(r.join("build")).unwrap();
        fs::write(r.join("build/out.ts"), "1").unwrap();
        fs::write(r.join(".gitignore"), "build/\n").unwrap();
        fs::write(r.join("notes.txt"), "1").unwrap();
        fs::write(r.join("README.md"), "# hi").unwrap();
        let files = GitCli.list_files(r, &["node_modules".to_string()]).unwrap();
        let s: Vec<String> = files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(s.contains(&"src/keep.rs".to_string()));
        assert!(s.contains(&"README.md".to_string()));
        assert!(!s
            .iter()
            .any(|p| p.starts_with("node_modules") || p.starts_with("build")));
        assert!(!s.contains(&"notes.txt".to_string()));
        assert!(s.windows(2).all(|w| w[0] < w[1]));
    }
}
