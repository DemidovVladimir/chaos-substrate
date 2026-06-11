//! `chaos pages` — list the GENERATED feature-memory pages of a repository.
//!
//! Answers *"what has chaos already extracted here?"* without `ls` or grep:
//! scans the features directory (default `docs/features_memory/`), recognises
//! every chaos-generated HTML page by its embedded manifest/data block, and
//! lists each page with its KIND (feature / story / components / features /
//! stack / impact / change-plan / feature-map), the tool that writes that
//! kind, its title, and its modified time. HTML files without a recognised
//! block are still listed (kind `other`, title from `<title>`) — nothing in
//! the directory is hidden.
//!
//! Read-only and embedder-free. The repo argument is resolved against the
//! index first (name or path of an indexed repository); a plain directory
//! path that is not indexed works too, since the scan is pure filesystem.

use crate::provenance::{source, Breadcrumb};
use crate::storage::Storage;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// Embedded JSON block ids that mark a chaos-generated page: `(id, kind,
/// writing tool)`. Order is the match order; a page carries exactly one.
const PAGE_MARKERS: &[(&str, &str, &str)] = &[
    (
        "chaos-feature-manifest",
        "feature",
        "chaos_write_feature_website / chaos add",
    ),
    (
        "chaos-storyboard-manifest",
        "story",
        "chaos_write_storyboard",
    ),
    (
        "chaos-components-manifest",
        "components",
        "chaos_components",
    ),
    ("chaos-features-manifest", "features", "chaos_features"),
    ("chaos-stack-manifest", "stack", "chaos_stack"),
    ("chaos-impact-data", "impact", "chaos_impact"),
    ("chaos-plan-data", "change-plan", "chaos_change_plan"),
    (
        "chaos-feature-map-data",
        "feature-map",
        "chaos refresh / analyze",
    ),
];

#[derive(Debug, Default, Clone)]
pub struct PagesOptions {
    /// Scan this directory instead of `<repo>/docs/features_memory`.
    pub features_dir: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct PageEntry {
    /// File name within the features directory.
    pub file: String,
    /// `feature` | `story` | `components` | `features` | `stack` | `impact`
    /// | `change-plan` | `feature-map` | `other`.
    pub kind: String,
    /// The chaos tool that writes this kind of page (empty for `other`).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tool: String,
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub subtitle: String,
    /// RFC3339 UTC file modification time, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PagesSummary {
    pub repo: String,
    pub features_dir: PathBuf,
    pub total: usize,
    pub by_kind: BTreeMap<String, usize>,
    /// Newest first; ties broken by file name.
    pub pages: Vec<PageEntry>,
    pub provenance: Vec<Breadcrumb>,
    pub warnings: Vec<String>,
}

pub async fn run(storage: &Storage, repo: &str, opts: &PagesOptions) -> Result<PagesSummary> {
    let (repo_name, repo_root) = match storage.find_repository(repo).await? {
        Some(repository) => (repository.name, PathBuf::from(repository.root_path)),
        None => {
            let path = PathBuf::from(repo);
            anyhow::ensure!(
                path.is_dir(),
                "repository is not indexed and is not a directory on disk: {repo}"
            );
            (repo.to_string(), path)
        }
    };
    let features_dir = opts
        .features_dir
        .clone()
        .unwrap_or_else(|| repo_root.join("docs/features_memory"));

    let mut warnings = Vec::new();
    let mut pages = Vec::new();
    let mut scanned = 0usize;
    if features_dir.is_dir() {
        for entry in fs::read_dir(&features_dir)
            .with_context(|| format!("reading features directory {}", features_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("html") {
                continue;
            }
            scanned += 1;
            match read_page_entry(&path) {
                Ok(page) => pages.push(page),
                Err(err) => warnings.push(format!("{}: {err:#}", path.display())),
            }
        }
    } else {
        warnings.push(format!(
            "features directory {} does not exist — no pages generated yet (run chaos analyze, then any surfacing tool)",
            features_dir.display()
        ));
    }

    pages.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.file.cmp(&b.file))
    });
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for page in &pages {
        *by_kind.entry(page.kind.clone()).or_default() += 1;
    }

    let provenance = vec![Breadcrumb::new(
        source::FILE,
        "scan_features_dir",
        format!(
            "{scanned} HTML file(s) scanned, {} recognised as chaos pages by embedded manifest block",
            pages.iter().filter(|p| p.kind != "other").count()
        ),
    )
    .with_locator(features_dir.display().to_string())];

    Ok(PagesSummary {
        repo: repo_name,
        features_dir,
        total: pages.len(),
        by_kind,
        pages,
        provenance,
        warnings,
    })
}

fn read_page_entry(path: &Path) -> Result<PageEntry> {
    let html = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let file = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let modified = fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .map(|time| {
            chrono::DateTime::<chrono::Utc>::from(time)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        });

    for (id, kind, tool) in PAGE_MARKERS {
        let Some(data) = extract_json_block(&html, id) else {
            continue;
        };
        let title = json_title(&data)
            .or_else(|| html_title(&html))
            .unwrap_or_else(|| file.clone());
        let subtitle = data
            .get("subtitle")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        return Ok(PageEntry {
            file,
            kind: (*kind).to_string(),
            tool: (*tool).to_string(),
            title,
            subtitle,
            modified,
        });
    }

    Ok(PageEntry {
        file: file.clone(),
        kind: "other".to_string(),
        tool: String::new(),
        title: html_title(&html).unwrap_or(file),
        subtitle: String::new(),
        modified,
    })
}

/// Pull the embedded `<script type="application/json" id="...">` block.
fn extract_json_block(html: &str, id: &str) -> Option<Value> {
    let marker = format!("id=\"{id}\">");
    let start = html.find(&marker)? + marker.len();
    let end = html[start..].find("</script>")?;
    serde_json::from_str(html[start..start + end].trim()).ok()
}

/// The page's display title, whichever field this manifest kind uses:
/// `title` (most), `task` (impact), `change` (change-plan).
fn json_title(data: &Value) -> Option<String> {
    ["title", "task", "change"].iter().find_map(|key| {
        data.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

fn html_title(html: &str) -> Option<String> {
    let start = html.find("<title>")? + "<title>".len();
    let end = html[start..].find("</title>")?;
    let title = html[start..start + end].trim();
    (!title.is_empty()).then(|| title.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_each_marker_and_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (
                "auth-feature.html",
                r#"<html><head><title>x</title></head><body><script type="application/json" id="chaos-feature-manifest">{"title":"Auth flow","subtitle":"login + sessions"}</script></body></html>"#,
                "feature",
                "Auth flow",
            ),
            (
                "pay-impact.html",
                r#"<html><body><script type="application/json" id="chaos-impact-data">{"task":"add payments"}</script></body></html>"#,
                "impact",
                "add payments",
            ),
            (
                "rate-plan.html",
                r#"<html><body><script type="application/json" id="chaos-plan-data">{"change":"rate limiting"}</script></body></html>"#,
                "change-plan",
                "rate limiting",
            ),
            (
                "legacy.html",
                "<html><head><title>Old page</title></head><body>no manifest</body></html>",
                "other",
                "Old page",
            ),
        ];
        for (name, html, _, _) in &cases {
            fs::write(dir.path().join(name), html).unwrap();
        }
        for (name, _, kind, title) in &cases {
            let entry = read_page_entry(&dir.path().join(name)).unwrap();
            assert_eq!(entry.kind, *kind, "{name}");
            assert_eq!(entry.title, *title, "{name}");
        }
    }

    #[test]
    fn escaped_manifest_json_still_parses() {
        // escape_script_json turns < > & into \u escapes — still valid JSON.
        let raw = serde_json::json!({"title": "Stack <dev>", "subtitle": ""}).to_string();
        let html = format!(
            r#"<script type="application/json" id="chaos-stack-manifest">{}</script>"#,
            crate::export_util::escape_script_json(&raw)
        );
        let data = extract_json_block(&html, "chaos-stack-manifest").unwrap();
        assert_eq!(json_title(&data).unwrap(), "Stack <dev>");
    }
}
