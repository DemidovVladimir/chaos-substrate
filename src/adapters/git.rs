//! Adapter: `git` CLI + `ignore` walker implementing [`Vcs`].
//!
//! Every method shells out to the `git` binary found on `PATH`; nothing is
//! cached, so results always reflect the working tree at call time.

use crate::domain::model::Language;
use crate::ports::vcs::{Change, ChangeStatus, Vcs};
use anyhow::{bail, Context, Result};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Version-control adapter backed by the system `git` executable.
#[derive(Debug, Default, Clone, Copy)]
pub struct GitCli;

/// Run `git <args>` inside `root`; `Ok(None)` when git exits non-zero.
fn git(root: &Path, args: &[&str]) -> Result<Option<String>> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| format!("failed to spawn git {}", args.join(" ")))?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// Normalise a git-reported path: strip quotes, force `/` separators.
fn norm(p: &str) -> String {
    p.trim().trim_matches('"').replace('\\', "/")
}

fn mk(path: String, status: ChangeStatus) -> Change {
    Change { path, status }
}

/// Parse `git diff --name-status -M` output.
fn parse_name_status(text: &str) -> Vec<Change> {
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut cols = line.split('\t');
        let code = cols.next().unwrap_or("");
        let a = cols.next().map(norm);
        let b = cols.next().map(norm);
        let (Some(a), kind) = (a, code.chars().next().unwrap_or(' ')) else {
            continue;
        };
        out.push(match (kind, b) {
            ('A', _) => mk(a, ChangeStatus::Added),
            ('D', _) => mk(a, ChangeStatus::Deleted),
            ('R', Some(new)) | ('C', Some(new)) => mk(new, ChangeStatus::Renamed { from: a }),
            _ => mk(a, ChangeStatus::Modified),
        });
    }
    out
}

/// Parse `git status --porcelain=v1 -uall` output (XY<space>path[ -> new]).
fn parse_porcelain(text: &str) -> Vec<Change> {
    let mut out = Vec::new();
    for line in text.lines().filter(|l| l.len() > 3) {
        let (xy, rest) = line.split_at(2);
        let rest = rest.trim_start();
        let (x, y) = (xy.as_bytes()[0] as char, xy.as_bytes()[1] as char);
        out.push(if xy == "??" {
            mk(norm(rest), ChangeStatus::Added)
        } else if x == 'R' || y == 'R' || x == 'C' {
            let (from, to) = rest.split_once(" -> ").unwrap_or((rest, rest));
            mk(norm(to), ChangeStatus::Renamed { from: norm(from) })
        } else if x == 'D' || y == 'D' {
            mk(norm(rest), ChangeStatus::Deleted)
        } else if x == 'A' {
            mk(norm(rest), ChangeStatus::Added)
        } else {
            mk(norm(rest), ChangeStatus::Modified)
        });
    }
    out
}

fn finish(mut changes: Vec<Change>) -> Vec<Change> {
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes.dedup_by(|a, b| a.path == b.path);
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

    fn changes(&self, root: &Path, since: Option<&str>) -> Result<Vec<Change>> {
        let parsed = match since {
            Some(rev) => {
                let text = git(root, &["diff", "--name-status", "-M", rev])?
                    .with_context(|| format!("git diff against `{rev}` failed"))?;
                parse_name_status(&text)
            }
            None => {
                let text = git(root, &["status", "--porcelain=v1", "-uall"])?
                    .with_context(|| format!("{} is not a git repository", root.display()))?;
                parse_porcelain(&text)
            }
        };
        Ok(finish(parsed))
    }

    fn list_files(&self, root: &Path, skip_dirs: &[String]) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for entry in WalkBuilder::new(root).hidden(false).build() {
            let entry = entry?;
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
            let skip = rel.components().any(|c| {
                let s = c.as_os_str().to_string_lossy();
                s == ".git" || skip_dirs.iter().any(|d| d == s.as_ref())
            });
            if skip || Language::from_path(&rel.to_string_lossy()).is_none() {
                continue;
            }
            files.push(rel.to_path_buf());
        }
        files.sort();
        files.dedup();
        Ok(files)
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

    #[test]
    fn changes_with_since() {
        let Some(dir) = repo() else { return };
        let r = dir.path();
        mutate(r);
        sh(r, &["add", "-A"]);
        sh(r, &["commit", "-qm", "second"]);
        let c = GitCli.changes(r, Some("HEAD~1")).unwrap();
        assert_eq!(find(&c, "src/new.rs"), &ChangeStatus::Added);
        assert_eq!(find(&c, "src/mod.rs"), &ChangeStatus::Modified);
        assert_eq!(find(&c, "src/gone.rs"), &ChangeStatus::Deleted);
        assert_eq!(find(&c, "src/renamed.rs"), &renamed("src/old.rs"));
        assert!(c.windows(2).all(|w| w[0].path < w[1].path));
    }

    #[test]
    fn changes_working_tree() {
        let Some(dir) = repo() else { return };
        let r = dir.path();
        mutate(r);
        // Untracked new file appears as Added before any staging.
        let c = GitCli.changes(r, None).unwrap();
        assert_eq!(find(&c, "src/new.rs"), &ChangeStatus::Added);
        assert_eq!(find(&c, "src/mod.rs"), &ChangeStatus::Modified);
        assert_eq!(find(&c, "src/gone.rs"), &ChangeStatus::Deleted);
        assert!(!GitCli.is_clean(r).unwrap());
        sh(r, &["add", "-A"]);
        let c = GitCli.changes(r, None).unwrap();
        assert_eq!(find(&c, "src/renamed.rs"), &renamed("src/old.rs"));
        assert!(!c.iter().any(|c| c.path == "src/old.rs"));
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
