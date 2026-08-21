//! Shared tool-module utilities: HTML/JSON escaping, slugs, the report-page
//! template fill, embedded-manifest extraction, and the "repository is not
//! indexed" preamble every report tool starts with.

use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{models::Repository, storage::Storage};

/// Escape a JSON string so that it is safe to embed inside a `<script>` tag.
///
/// Replaces `&`, `<`, and `>` with their Unicode escape sequences (`&`,
/// `<`, `>`). This prevents the browser's HTML parser from
/// prematurely terminating the script block when the JSON contains `</script`
/// or other HTML-significant characters.
pub(crate) fn escape_script_json(json: &str) -> String {
    json.replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
}

/// Escape text for embedding in HTML body content or double-quoted attributes.
///
/// Replacement order matters: `&` is escaped first so the entities introduced
/// by the later replacements are not themselves re-escaped. Pages that emit
/// single-quoted attribute values need [`html_escape_full`] instead.
pub(crate) fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The 5-entity HTML escape: [`html_escape`] plus `'` (`&#039;`), for pages
/// that emit single-quoted attribute values. The apostrophe pass runs last,
/// matching the shared helper's `&`-first ordering.
pub(crate) fn html_escape_full(input: &str) -> String {
    html_escape(input).replace('\'', "&#039;")
}

/// Lowercase, hyphen-joined, ASCII-alphanumeric slug capped at 80 chars;
/// `fallback` when nothing alphanumeric survives. Never empty.
pub(crate) fn safe_slug(input: &str, fallback: &str) -> String {
    let slug = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        fallback.to_string()
    } else {
        slug.chars().take(80).collect()
    }
}

/// Fill the shared report-page placeholders (`__THEME__`, `__REPORT_CSS__`,
/// `__REPORT_JS__`, `__BRAND_TOPBAR__`, `__BRAND_FOOTER__`, `__DATA__`) in
/// `template` and write the page, creating the parent directory as needed.
/// `manifest_json` is script-escaped before it lands in the `__DATA__` block.
pub(crate) fn write_report_page(path: &Path, template: &str, manifest_json: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        template
            .replace("__THEME__", crate::theme::THEME_CSS)
            .replace("__REPORT_CSS__", crate::theme::REPORT_CSS)
            .replace("__REPORT_JS__", crate::theme::REPORT_JS)
            .replace(
                "__BRAND_TOPBAR__",
                &crate::theme::render_brand(&crate::theme::Brand::default(), "topbar"),
            )
            .replace(
                "__BRAND_FOOTER__",
                &crate::theme::render_brand(&crate::theme::Brand::default(), "footer"),
            )
            .replace("__DATA__", &escape_script_json(manifest_json)),
    )?;
    Ok(())
}

/// Pull the embedded `<script type="application/json" id="...">` block.
pub(crate) fn extract_json_block(html: &str, id: &str) -> Option<Value> {
    let marker = format!("id=\"{id}\">");
    let start = html.find(&marker)? + marker.len();
    let end = html[start..].find("</script>")?;
    serde_json::from_str(html[start..start + end].trim()).ok()
}

/// Read `content_hash` out of an existing generated page's embedded manifest
/// block (`manifest_id` is the block's element id), if any.
pub(crate) fn existing_content_hash(path: &Path, manifest_id: &str) -> Option<String> {
    let html = fs::read_to_string(path).ok()?;
    extract_json_block(&html, manifest_id)?
        .get("content_hash")
        .and_then(Value::as_str)
        .map(String::from)
}

/// The shared tool preamble: resolve an indexed repository (by name or path)
/// and its root directory, or fail with the canonical "not indexed" error.
pub(crate) async fn resolve_indexed_repo(
    storage: &Storage,
    repo: &str,
) -> Result<(Repository, PathBuf)> {
    let repository = storage
        .find_repository(repo)
        .await?
        .with_context(|| format!("repository is not indexed: {repo}"))?;
    let root = PathBuf::from(&repository.root_path);
    Ok((repository, root))
}

/// The generated-pages directory inside a repository.
pub(crate) fn features_memory_dir(repo_root: &Path) -> PathBuf {
    repo_root.join("docs/features_memory")
}

/// Human list join: "a", "a and b", "a, b, and c".
pub(crate) fn join_human<S: AsRef<str>>(items: &[S]) -> String {
    match items {
        [] => String::new(),
        [only] => only.as_ref().to_string(),
        [a, b] => format!("{} and {}", a.as_ref(), b.as_ref()),
        [head @ .., last] => format!(
            "{}, and {}",
            head.iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>()
                .join(", "),
            last.as_ref()
        ),
    }
}

/// Render a line span as "12", "12-34", or `missing` when the start line is
/// unknown.
pub(crate) fn line_range(start: Option<i32>, end: Option<i32>, missing: &str) -> String {
    match (start, end) {
        (Some(s), Some(e)) if s != e => format!("{s}-{e}"),
        (Some(s), _) => s.to_string(),
        _ => missing.to_string(),
    }
}

/// Display language for a file path by extension — the core map shared by the
/// report tools (`usage` layers its config-file arms on top).
pub(crate) fn language_for(path: &str) -> String {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => "Rust",
        "ts" | "tsx" => "TypeScript",
        "js" | "jsx" | "mjs" | "cjs" => "JavaScript",
        "py" => "Python",
        "sol" => "Solidity",
        "graphql" | "gql" | "graphqls" => "GraphQL",
        "md" | "mdx" => "Markdown",
        "pdf" => "PDF",
        _ => "",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_slug_lowercases_hyphenates_and_bounds() {
        assert_eq!(safe_slug("Feature: Add Export!", "x"), "feature-add-export");
        assert_eq!(safe_slug("***", "fallback-slug"), "fallback-slug");
        assert_eq!(safe_slug("", "fallback-slug"), "fallback-slug");
        let long = "a".repeat(200);
        assert_eq!(safe_slug(&long, "x").chars().count(), 80);
    }

    #[test]
    fn report_page_round_trips_through_extract_json_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/report.html");
        let template = r#"<html><style>__THEME__</style><body>__BRAND_TOPBAR__
<script type="application/json" id="chaos-test-manifest">__DATA__</script>
__BRAND_FOOTER__</body></html>"#;
        let manifest = serde_json::json!({
            "title": "Round <trip> & co",
            "content_hash": "cafebabe",
        });
        write_report_page(&path, template, &manifest.to_string()).unwrap();

        let html = fs::read_to_string(&path).unwrap();
        // The escape must keep the block parseable while defusing `</script`.
        assert!(!html.contains("<trip>"));
        let parsed = extract_json_block(&html, "chaos-test-manifest").unwrap();
        assert_eq!(parsed["title"].as_str().unwrap(), "Round <trip> & co");
        assert_eq!(
            existing_content_hash(&path, "chaos-test-manifest").as_deref(),
            Some("cafebabe")
        );
        assert_eq!(existing_content_hash(&path, "chaos-other-manifest"), None);
    }

    #[test]
    fn join_human_reads_naturally_for_strs_and_strings() {
        assert_eq!(join_human::<&str>(&[]), "");
        assert_eq!(join_human(&["a"]), "a");
        assert_eq!(join_human(&["a", "b"]), "a and b");
        let owned = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(join_human(&owned), "a, b, and c");
    }

    #[test]
    fn line_range_uses_the_missing_placeholder() {
        assert_eq!(line_range(Some(3), Some(9), "n/a"), "3-9");
        assert_eq!(line_range(Some(3), Some(3), "n/a"), "3");
        assert_eq!(line_range(None, Some(9), "unknown"), "unknown");
    }
}
