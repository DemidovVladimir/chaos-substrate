//! Vault-relative note paths. Every path that reaches the vault — from disk or
//! from an agent — is parsed here first, so nothing can name a file outside
//! the vault or inside a hidden folder (`.obsidian`, `.chaos-kb`, `.git`).

use serde::Serialize;
use std::fmt;

/// A validated note path: `/`-separated, relative, every segment visible
/// (no leading `.`), ending in `.md`. Example: `Health/Medications.md`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct NotePath(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("note path is empty")]
    Empty,
    #[error("note path must be relative to the vault: {0}")]
    Absolute(String),
    #[error(
        "note path segment {0:?} is not allowed (empty, `.`/`..`, hidden, or has `\\`, `:` or a control character)"
    )]
    Segment(String),
}

impl NotePath {
    /// Parse a relative path; `.md` is appended when the last segment has no
    /// `.md` extension (`Health/Meds` → `Health/Meds.md`).
    pub fn parse(raw: &str) -> Result<NotePath, PathError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(PathError::Empty);
        }
        if raw.starts_with('/') {
            return Err(PathError::Absolute(raw.to_string()));
        }
        for segment in raw.split('/') {
            let bad = segment.is_empty()
                || segment.starts_with('.')
                || segment
                    .chars()
                    .any(|c| c == '\\' || c == ':' || c.is_control());
            if bad {
                return Err(PathError::Segment(segment.to_string()));
            }
        }
        let has_md = raw
            .rsplit_once('.')
            .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("md"));
        Ok(NotePath(if has_md {
            raw.to_string()
        } else {
            format!("{raw}.md")
        }))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The note's title: its file name without `.md` (Obsidian's convention).
    pub fn title(&self) -> &str {
        let name = self.0.rsplit('/').next().unwrap_or(&self.0);
        &name[..name.len() - ".md".len()]
    }

    /// The folders above the note, outermost first: `a/b/c.md` → `["a", "a/b"]`.
    pub fn folders(&self) -> Vec<&str> {
        self.0
            .match_indices('/')
            .map(|(i, _)| &self.0[..i])
            .collect()
    }
}

impl fmt::Display for NotePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(raw: &str) -> String {
        NotePath::parse(raw).unwrap().as_str().to_string()
    }

    #[test]
    fn accepts_relative_markdown_paths() {
        assert_eq!(ok("Health/Medications.md"), "Health/Medications.md");
        assert_eq!(ok("inbox.md"), "inbox.md");
        assert_eq!(ok("Здоровье/Лекарства.md"), "Здоровье/Лекарства.md");
        assert_eq!(ok("Car maintenance.md"), "Car maintenance.md");
    }

    #[test]
    fn appends_md_extension() {
        assert_eq!(ok("Health/Meds"), "Health/Meds.md");
        assert_eq!(ok("notes.txt"), "notes.txt.md");
        assert_eq!(ok("Readme.MD"), "Readme.MD", "extension check ignores case");
    }

    #[test]
    fn rejects_escapes_and_hidden_segments() {
        assert_eq!(NotePath::parse(""), Err(PathError::Empty));
        assert_eq!(NotePath::parse("   "), Err(PathError::Empty));
        assert!(matches!(
            NotePath::parse("/etc/passwd"),
            Err(PathError::Absolute(_))
        ));
        for bad in [
            "../x.md",
            "a/../../x.md",
            "a/./x.md",
            "a//x.md",
            "a/",
            ".obsidian/app.md",
            ".chaos-kb/index.md",
            "a/.hidden.md",
            "a\\b.md",
            "C:x.md",
            "a/\u{0}b.md",
            "a/b\n.md",
        ] {
            assert!(
                matches!(NotePath::parse(bad), Err(PathError::Segment(_))),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn title_and_folders() {
        let p = NotePath::parse("Health/Treatment/Plan 2026.md").unwrap();
        assert_eq!(p.title(), "Plan 2026");
        assert_eq!(p.folders(), ["Health", "Health/Treatment"]);
        let top = NotePath::parse("inbox.md").unwrap();
        assert_eq!(top.title(), "inbox");
        assert!(top.folders().is_empty());
        assert_eq!(NotePath::parse("Readme.MD").unwrap().title(), "Readme");
    }
}
