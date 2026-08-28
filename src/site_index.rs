//! Site index + shared navigation for a generated feature-memory directory.
//!
//! Every surfacing tool (`compose`, `impact`, `change-plan`, `usage`,
//! `feature-context`, `features`, `components`, `stack`, `storyboard`,
//! `feature-story`, …) writes its own standalone page — and `compose
//! --feature-pages` / `feature-story` each write a whole sub-site. Without a
//! hub the only way in was `chaos pages` or `ls`. This module gives the
//! directory ONE front door and stitches every page to it:
//!
//! * [`build`] writes `<dir>/index.html`: every recognised page grouped by
//!   kind (composed / story / plan / impact / …), newest first, with title,
//!   subtitle, modified time, the writing tool, and — for a page that fronts a
//!   sub-site directory (`foo-composed.html` ↔ `foo-composed/`) — the number of
//!   pages behind it. A small client-side filter box narrows the cards. The
//!   page embeds a `chaos-site-index-manifest` JSON block like every other
//!   chaos page, so `chaos pages` recognises it and agents can read it.
//! * [`build`] then **stamps a navigation bar** into every page in the
//!   directory and one level of sub-site directories: `Index` (relative), the
//!   page's kind, and — inside a sub-site — the site's front page plus
//!   prev / next siblings. The bar is self-styled and delimited by HTML
//!   comment markers, so re-stamping is idempotent and a page is rewritten
//!   only when its bar actually changes (a no-op run touches nothing, keeping
//!   mtimes — and therefore `chaos pages` ordering — stable).
//! * [`refresh_quietly`] is the hook every writer calls after its final
//!   `fs::write`: it rebuilds the index and re-stamps navigation, and turns
//!   any failure into a `tracing::warn!` — a broken hub must never fail the
//!   page write that triggered it.
//!
//! Hash gates elsewhere read a page's manifest block by id; the navigation bar
//! lives outside those blocks, so stamping never invalidates a content hash.

use crate::export_util::{escape_script_json, html_escape_full as esc};
use crate::pages::{read_page_entry, PageEntry};
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// File name of the hub page inside a features directory.
pub(crate) const INDEX_FILE: &str = "index.html";
/// Embedded JSON block id of the hub page.
pub(crate) const MANIFEST_ID: &str = "chaos-site-index-manifest";

const NAV_START: &str = "<!--chaos-nav-->";
const NAV_END: &str = "<!--/chaos-nav-->";

/// Display order + one-line meaning per page kind. Kinds not listed here
/// (including `other`) sort after these, alphabetically.
const KIND_ORDER: &[(&str, &str, &str)] = &[
    ("composed", "Composed pages & sites", "Audience-tailored pages composed from features, correlations and the tech stack; a site links one page per feature."),
    ("feature-story", "Cross-repo stories", "How one feature works across every repo of a project, ordered client → backend → contracts."),
    ("feature", "Feature deep-dives", "Curated explanations of a feature with its code, persisted from a drill-down or written by `chaos add`."),
    ("story", "User storyboards", "Code-free, stakeholder-facing walkthroughs of a feature."),
    ("change-plan", "Change plans", "Which features a described change spans, with a dependency-aware check order."),
    ("impact", "Impact reports", "How a proposed feature maps onto the code as it is today."),
    ("context", "Feature contexts", "Retrieval evidence for a task, with the verbatim code bodies embedded."),
    ("usage", "Usage reports", "Who consumes a symbol, env var, header or route, grouped by subfolder."),
    ("components", "Component overviews", "Core components of an area with a proposed read order."),
    ("features", "Feature inventories", "Exhaustive list of feature communities grouped by journey layer."),
    ("stack", "Tech stack", "Declared dependencies, scripts, deployment resources and languages."),
    ("sui-migration", "Sui migration impact", "Source-stack detection mapped onto Sui / Walrus / Seal concepts."),
    ("feature-map", "Feature maps", "Interactive maps written by refresh / analyze."),
];

#[derive(Debug, Serialize)]
pub struct SiteIndexSummary {
    /// Absolute path of the written hub page.
    pub index_html: PathBuf,
    /// Pages listed on the hub (top-level pages only; sub-site pages are
    /// counted on their front page's card).
    pub listed: usize,
    pub by_kind: BTreeMap<String, usize>,
    /// Pages whose navigation bar was (re)written this run.
    pub nav_stamped: usize,
    /// Pages whose bar was already current and were left untouched.
    pub nav_unchanged: usize,
    pub warnings: Vec<String>,
}

/// One card on the hub page.
#[derive(Debug, Clone, Serialize)]
struct IndexCard {
    file: String,
    kind: String,
    tool: String,
    title: String,
    subtitle: String,
    modified: Option<String>,
    /// Pages inside the sub-site this page fronts (`<stem>/`), when any.
    #[serde(skip_serializing_if = "Option::is_none")]
    site_pages: Option<usize>,
    /// Directory name of that sub-site.
    #[serde(skip_serializing_if = "Option::is_none")]
    site_dir: Option<String>,
}

/// Build the hub page and stamp navigation into every page under `dir`.
/// `label` names the directory's owner on the page (repo or project name).
pub(crate) fn build(dir: &Path, label: &str) -> Result<SiteIndexSummary> {
    anyhow::ensure!(
        dir.is_dir(),
        "features directory does not exist yet: {}",
        dir.display()
    );
    let mut warnings = Vec::new();

    // 1. Scan top-level pages and sub-site directories.
    let mut cards: Vec<IndexCard> = Vec::new();
    let mut site_dirs: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if path.is_dir() {
            let mut pages: Vec<PathBuf> = fs::read_dir(&path)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("html"))
                .collect();
            if !pages.is_empty() {
                pages.sort();
                site_dirs.insert(name, pages);
            }
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("html") || name == INDEX_FILE {
            continue;
        }
        match read_page_entry(&path) {
            Ok(page) => cards.push(card_from(page)),
            Err(err) => warnings.push(format!("{}: {err:#}", path.display())),
        }
    }
    for card in cards.iter_mut() {
        let stem = card.file.trim_end_matches(".html");
        if let Some(pages) = site_dirs.get(stem) {
            card.site_pages = Some(pages.len());
            card.site_dir = Some(stem.to_string());
        }
    }
    cards.sort_by(|a, b| {
        kind_rank(&a.kind)
            .cmp(&kind_rank(&b.kind))
            .then_with(|| b.modified.cmp(&a.modified))
            .then_with(|| a.file.cmp(&b.file))
    });
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for card in &cards {
        *by_kind.entry(card.kind.clone()).or_default() += 1;
    }

    // 2. Write the hub.
    let index_path = dir.join(INDEX_FILE);
    let generated_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let manifest = json!({
        "schema_version": "site-index-1",
        "title": format!("{label} · feature memory"),
        "subtitle": format!("{} generated page(s) across {} kind(s)", cards.len(), by_kind.len()),
        "label": label,
        "generated_at": generated_at,
        "total": cards.len(),
        "by_kind": by_kind,
        "pages": cards,
        "warnings": warnings,
    });
    fs::write(&index_path, render_index_html(&manifest, label, &cards))
        .with_context(|| format!("writing {}", index_path.display()))?;

    // 3. Stamp navigation. Front pages of sub-sites, then their inner pages.
    let mut stamped = 0usize;
    let mut unchanged = 0usize;
    let front_titles: BTreeMap<String, String> = cards
        .iter()
        .filter_map(|c| c.site_dir.clone().map(|d| (d, c.title.clone())))
        .collect();
    for card in &cards {
        let nav = NavLinks {
            index_href: INDEX_FILE.to_string(),
            kind: card.kind.clone(),
            site: None,
            prev: None,
            next: None,
        };
        match stamp_nav(&dir.join(&card.file), &nav) {
            Ok(true) => stamped += 1,
            Ok(false) => unchanged += 1,
            Err(err) => warnings.push(format!("{}: {err:#}", card.file)),
        }
    }
    for (site_dir, pages) in &site_dirs {
        let front_title = front_titles
            .get(site_dir)
            .cloned()
            .unwrap_or_else(|| site_dir.clone());
        let front_href = format!("../{site_dir}.html");
        let has_front = dir.join(format!("{site_dir}.html")).is_file();
        for (i, page) in pages.iter().enumerate() {
            let sibling = |j: usize| {
                pages.get(j).and_then(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .map(|f| NavLink {
                            href: f.clone(),
                            label: f.trim_end_matches(".html").to_string(),
                        })
                })
            };
            let nav = NavLinks {
                index_href: format!("../{INDEX_FILE}"),
                kind: format!("{site_dir} · {}/{}", i + 1, pages.len()),
                site: has_front.then(|| NavLink {
                    href: front_href.clone(),
                    label: front_title.clone(),
                }),
                prev: if i > 0 { sibling(i - 1) } else { None },
                next: sibling(i + 1),
            };
            match stamp_nav(page, &nav) {
                Ok(true) => stamped += 1,
                Ok(false) => unchanged += 1,
                Err(err) => warnings.push(format!("{}: {err:#}", page.display())),
            }
        }
    }

    Ok(SiteIndexSummary {
        index_html: index_path,
        listed: manifest["total"].as_u64().unwrap_or_default() as usize,
        by_kind: manifest["by_kind"]
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_u64().unwrap_or_default() as usize))
                    .collect()
            })
            .unwrap_or_default(),
        nav_stamped: stamped,
        nav_unchanged: unchanged,
        warnings,
    })
}

/// Writer hook: rebuild the hub of the directory that contains `written_page`
/// (or of `written_page` itself when it is a directory). Never fails — a
/// broken hub is logged, the page write that triggered it already succeeded.
pub(crate) fn refresh_quietly(written_page: &Path) {
    let dir = if written_page.is_dir() {
        written_page.to_path_buf()
    } else {
        match written_page.parent() {
            Some(parent) => parent.to_path_buf(),
            None => return,
        }
    };
    let label = dir
        .parent()
        .and_then(|docs| docs.parent())
        .and_then(|root| root.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string());
    if let Err(err) = build(&dir, &label) {
        tracing::warn!(dir = %dir.display(), error = %format!("{err:#}"), "site index refresh failed");
    }
}

fn card_from(page: PageEntry) -> IndexCard {
    IndexCard {
        file: page.file,
        kind: page.kind,
        tool: page.tool,
        title: page.title,
        subtitle: page.subtitle,
        modified: page.modified,
        site_pages: None,
        site_dir: None,
    }
}

fn kind_rank(kind: &str) -> (usize, String) {
    KIND_ORDER
        .iter()
        .position(|(k, _, _)| *k == kind)
        .map(|i| (i, String::new()))
        .unwrap_or((KIND_ORDER.len(), kind.to_string()))
}

fn kind_meta(kind: &str) -> (String, String) {
    KIND_ORDER
        .iter()
        .find(|(k, _, _)| *k == kind)
        .map(|(_, title, blurb)| (title.to_string(), blurb.to_string()))
        .unwrap_or_else(|| {
            (
                if kind == "other" {
                    "Other HTML files".to_string()
                } else {
                    kind.to_string()
                },
                "Files in this directory without a recognised chaos manifest block — listed, never hidden.".to_string(),
            )
        })
}

// ---------------------------------------------------------------------------
// Navigation stamping
// ---------------------------------------------------------------------------

struct NavLink {
    href: String,
    label: String,
}

struct NavLinks {
    index_href: String,
    /// Short descriptor shown at the right: the page kind, or `site · i/n`.
    kind: String,
    site: Option<NavLink>,
    prev: Option<NavLink>,
    next: Option<NavLink>,
}

/// Self-contained bar (own `<style>`, no dependency on the page's CSS) so it
/// renders identically on every page kind, including `other` files.
const NAV_CSS: &str = ".chaos-nav{position:sticky;top:0;z-index:50;display:flex;flex-wrap:wrap;align-items:center;gap:6px 14px;padding:6px 16px;font:500 12px/1.4 system-ui,-apple-system,'Segoe UI',Inter,sans-serif;background:#0f172a;color:#cbd5e1;border-bottom:1px solid #1e293b}.chaos-nav a{color:#e2e8f0;text-decoration:none;padding:3px 8px;border-radius:999px;background:#1e293b}.chaos-nav a:hover{background:#334155;color:#fff}.chaos-nav .chaos-nav-kind{margin-left:auto;text-transform:uppercase;letter-spacing:.06em;font-size:11px;color:#94a3b8}.chaos-nav .chaos-nav-site{max-width:40ch;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}";

fn render_nav(nav: &NavLinks) -> String {
    let mut out = String::new();
    out.push_str(NAV_START);
    out.push_str("<style>");
    out.push_str(NAV_CSS);
    out.push_str("</style><nav class=\"chaos-nav\" aria-label=\"Chaos feature-memory navigation\">");
    out.push_str(&format!(
        "<a href=\"{}\" title=\"All generated pages\">&#8962; Index</a>",
        esc(&nav.index_href)
    ));
    if let Some(site) = &nav.site {
        out.push_str(&format!(
            "<a class=\"chaos-nav-site\" href=\"{}\" title=\"Front page of this site\">&#8593; {}</a>",
            esc(&site.href),
            esc(&site.label)
        ));
    }
    if let Some(prev) = &nav.prev {
        out.push_str(&format!(
            "<a href=\"{}\" title=\"{}\">&larr; Prev</a>",
            esc(&prev.href),
            esc(&prev.label)
        ));
    }
    if let Some(next) = &nav.next {
        out.push_str(&format!(
            "<a href=\"{}\" title=\"{}\">Next &rarr;</a>",
            esc(&next.href),
            esc(&next.label)
        ));
    }
    out.push_str(&format!(
        "<span class=\"chaos-nav-kind\">{}</span></nav>",
        esc(&nav.kind)
    ));
    out.push_str(NAV_END);
    out
}

/// Insert or replace the navigation bar right after the opening `<body>` tag.
/// Returns `Ok(true)` when the file was rewritten, `Ok(false)` when the bar
/// was already identical. Pages without a `<body>` tag are left alone.
fn stamp_nav(path: &Path, nav: &NavLinks) -> Result<bool> {
    let html = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let bar = render_nav(nav);
    let updated = match (html.find(NAV_START), html.find(NAV_END)) {
        (Some(start), Some(end)) if end > start => {
            let end = end + NAV_END.len();
            if html[start..end] == bar {
                return Ok(false);
            }
            format!("{}{}{}", &html[..start], bar, &html[end..])
        }
        _ => {
            let Some(body_open) = find_body_open(&html) else {
                return Ok(false);
            };
            format!("{}{}{}", &html[..body_open], bar, &html[body_open..])
        }
    };
    fs::write(path, updated).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// Byte offset just past the `<body ...>` opening tag, when present.
fn find_body_open(html: &str) -> Option<usize> {
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find("<body") {
        let at = from + rel;
        let after = lower.as_bytes().get(at + 5).copied();
        if matches!(after, Some(b'>') | Some(b' ') | Some(b'\n') | Some(b'\t')) {
            let close = lower[at..].find('>')?;
            return Some(at + close + 1);
        }
        from = at + 5;
    }
    None
}

// ---------------------------------------------------------------------------
// Hub page
// ---------------------------------------------------------------------------

fn render_index_html(manifest: &serde_json::Value, label: &str, cards: &[IndexCard]) -> String {
    let mut sections = String::new();
    let mut current_kind: Option<&str> = None;
    for card in cards {
        if current_kind != Some(card.kind.as_str()) {
            if current_kind.is_some() {
                sections.push_str("</div></section>");
            }
            let (title, blurb) = kind_meta(&card.kind);
            let count = cards.iter().filter(|c| c.kind == card.kind).count();
            sections.push_str(&format!(
                "<section class=\"kind\" data-kind=\"{k}\"><h2>{t} <span class=\"count\">{n}</span></h2><p class=\"blurb\">{b}</p><div class=\"cards\">",
                k = esc(&card.kind),
                t = esc(&title),
                n = count,
                b = esc(&blurb)
            ));
            current_kind = Some(card.kind.as_str());
        }
        let site = match (&card.site_dir, card.site_pages) {
            (Some(dir), Some(n)) => format!(
                "<span class=\"tag site\" title=\"{d}/\">site · {n} page{s}</span>",
                d = esc(dir),
                s = if n == 1 { "" } else { "s" }
            ),
            _ => String::new(),
        };
        let tool = if card.tool.is_empty() {
            String::new()
        } else {
            format!("<span class=\"tag\">{}</span>", esc(&card.tool))
        };
        let modified = card
            .modified
            .as_deref()
            .map(|m| format!("<span class=\"when\">{}</span>", esc(m)))
            .unwrap_or_default();
        let subtitle = if card.subtitle.is_empty() {
            String::new()
        } else {
            format!("<p>{}</p>", esc(&card.subtitle))
        };
        sections.push_str(&format!(
            "<article class=\"card\" data-search=\"{search}\"><h3><a href=\"{href}\">{title}</a></h3>{subtitle}<div class=\"meta\">{site}{tool}{modified}<span class=\"file\">{file}</span></div></article>",
            search = esc(&format!("{} {} {} {}", card.title, card.subtitle, card.file, card.kind).to_lowercase()),
            href = esc(&card.file),
            title = esc(&card.title),
            file = esc(&card.file),
        ));
    }
    if current_kind.is_some() {
        sections.push_str("</div></section>");
    }
    if cards.is_empty() {
        sections.push_str("<p class=\"empty\">No generated pages yet — run any chaos surfacing tool (compose, impact, change-plan, usage, feature-context, …) and this hub fills itself.</p>");
    }

    INDEX_HTML
        .replace("__THEME__", crate::theme::THEME_CSS)
        .replace(
            "__BRAND_TOPBAR__",
            &crate::theme::render_brand(&crate::theme::Brand::default(), "topbar"),
        )
        .replace(
            "__BRAND_FOOTER__",
            &crate::theme::render_brand(&crate::theme::Brand::default(), "footer"),
        )
        .replace("__LABEL__", &esc(label))
        .replace("__TOTAL__", &cards.len().to_string())
        .replace("__KINDS__", &manifest["by_kind"].as_object().map(|m| m.len()).unwrap_or(0).to_string())
        .replace("__GENERATED__", &esc(manifest["generated_at"].as_str().unwrap_or("")))
        .replace("__SECTIONS__", &sections)
        .replace("__MANIFEST__", &escape_script_json(&manifest.to_string()))
}

const INDEX_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__LABEL__ · feature memory</title>
<style>
__THEME__
main.wrap{padding:8px 0 40px}
.toolbar{display:flex;gap:12px;align-items:center;margin:18px 0 6px}
.toolbar input{flex:1;max-width:520px;padding:10px 14px;border:var(--border-hairline);border-radius:var(--radius-pill);font:var(--type-body);background:var(--color-surface-0);color:var(--fg-primary)}
.toolbar .hint{color:var(--fg-tertiary);font:var(--type-body-sm)}
section.kind{padding:26px 0;border-bottom:var(--border-hairline)}section.kind:last-of-type{border-bottom:0}
section.kind>h2{margin:0 0 4px;font:var(--type-h4);color:var(--color-ink-700);display:flex;align-items:center;gap:10px}
section.kind .count{display:inline-flex;border-radius:var(--radius-pill);padding:2px 10px;font:var(--type-overline-sm);background:var(--color-surface-2);color:var(--color-ink-500)}
section.kind .blurb{margin:0 0 14px;color:var(--fg-tertiary);font:var(--type-body-sm)}
.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(320px,1fr));gap:14px}
.card{background:var(--color-surface-0);border:var(--border-hairline);border-radius:var(--radius-lg);box-shadow:var(--shadow-sm);padding:14px 16px}
.card h3{margin:0;font:var(--type-h5);color:var(--color-ink-700);overflow-wrap:anywhere}
.card h3 a{color:var(--color-blue-700);text-decoration:none}.card h3 a:hover{text-decoration:underline}
.card p{margin:6px 0 0;color:var(--color-ink-500);font:var(--type-body-sm);line-height:1.5;overflow-wrap:anywhere}
.card .meta{display:flex;flex-wrap:wrap;gap:6px;align-items:center;margin-top:10px}
.tag{display:inline-flex;border-radius:var(--radius-pill);padding:2px 9px;font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.05em;background:var(--color-surface-2);color:var(--color-ink-400)}
.tag.site{background:var(--color-blue-700);color:#fff}
.when,.file{font-family:var(--font-mono);font-size:11px;color:var(--fg-tertiary)}
.file{overflow-wrap:anywhere}
.empty{color:var(--fg-tertiary);padding:30px 0}
[hidden]{display:none!important}
</style>
</head>
<body>
<div class="topbar"><div class="wrap">__BRAND_TOPBAR__<span class="sp"></span><span class="pilltag">Feature memory</span></div></div>
<header class="hero"><div class="wrap"><div><div class="eyebrow">Chaos Substrate · generated pages</div><h1>__LABEL__</h1><div class="subtitle">__TOTAL__ page(s) across __KINDS__ kind(s) · index regenerated __GENERATED__</div></div></div></header>
<main class="wrap">
<div class="toolbar"><input id="q" type="search" placeholder="Filter pages by title, file, kind…" autocomplete="off"><span class="hint" id="hint"></span></div>
__SECTIONS__
</main>
<footer><div class="wrap">__BRAND_FOOTER__<span class="sp"></span><span class="meta">generated by Chaos Substrate · <code>chaos pages --index</code></span></div></footer>
<script type="application/json" id="chaos-site-index-manifest">
__MANIFEST__
</script>
<script>
(function(){
  var q=document.getElementById('q'),hint=document.getElementById('hint');
  var cards=Array.prototype.slice.call(document.querySelectorAll('.card'));
  var sections=Array.prototype.slice.call(document.querySelectorAll('section.kind'));
  function apply(){
    var t=(q.value||'').trim().toLowerCase();var shown=0;
    cards.forEach(function(c){var ok=!t||c.getAttribute('data-search').indexOf(t)>=0;c.hidden=!ok;if(ok)shown++;});
    sections.forEach(function(s){s.hidden=!s.querySelector('.card:not([hidden])');});
    hint.textContent=t?shown+' of '+cards.length+' shown':'';
  }
  q.addEventListener('input',apply);
})();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export_util::extract_json_block;

    fn page(title: &str, manifest_id: &str) -> String {
        format!(
            r#"<!doctype html><html><head><title>{title}</title></head><body class="x"><h1>{title}</h1><script type="application/json" id="{manifest_id}">{{"title":"{title}","subtitle":"sub"}}</script></body></html>"#
        )
    }

    #[test]
    fn builds_index_and_stamps_nav_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a-composed.html"), page("Site A", "chaos-composed-manifest")).unwrap();
        fs::write(root.join("b-plan.html"), page("Plan B", "chaos-plan-data")).unwrap();
        fs::write(root.join("legacy.html"), "<html><head><title>Old</title></head><body>x</body></html>").unwrap();
        fs::create_dir_all(root.join("a-composed")).unwrap();
        fs::write(root.join("a-composed/f1.html"), page("F1", "chaos-composed-manifest")).unwrap();
        fs::write(root.join("a-composed/f2.html"), page("F2", "chaos-composed-manifest")).unwrap();

        let first = build(root, "repo-x").unwrap();
        assert_eq!(first.listed, 3);
        assert_eq!(first.nav_stamped, 5);
        assert_eq!(first.nav_unchanged, 0);
        assert!(first.warnings.is_empty(), "{:?}", first.warnings);

        let index = fs::read_to_string(root.join(INDEX_FILE)).unwrap();
        assert!(index.contains("site · 2 pages"));
        assert!(index.contains(r#"href="a-composed.html""#));
        assert!(index.contains(r#"href="legacy.html""#), "other files are listed, never hidden");
        let manifest = extract_json_block(&index, MANIFEST_ID).unwrap();
        assert_eq!(manifest["total"], 3);
        assert_eq!(manifest["by_kind"]["composed"], 1);
        assert_eq!(manifest["pages"][0]["site_pages"], 2);

        // Front page: Index only. Inner page: index up one level, site front, prev/next.
        let front = fs::read_to_string(root.join("a-composed.html")).unwrap();
        assert!(front.contains(r#"<body class="x"><!--chaos-nav-->"#));
        assert!(front.contains(r#"href="index.html""#));
        assert!(!front.contains("Next"));
        let f1 = fs::read_to_string(root.join("a-composed/f1.html")).unwrap();
        assert!(f1.contains(r#"href="../index.html""#));
        assert!(f1.contains(r#"href="../a-composed.html""#));
        assert!(f1.contains(r#"href="f2.html""#));
        assert!(!f1.contains("Prev"));
        assert!(f1.contains("1/2"));
        // The manifest block is untouched by stamping.
        assert!(extract_json_block(&f1, "chaos-composed-manifest").is_some());

        // Second run: nothing changes, nothing rewritten.
        let second = build(root, "repo-x").unwrap();
        assert_eq!(second.nav_stamped, 0);
        assert_eq!(second.nav_unchanged, 5);
        let f1_again = fs::read_to_string(root.join("a-composed/f1.html")).unwrap();
        assert_eq!(f1, f1_again);
        assert_eq!(f1.matches(NAV_START).count(), 1);
    }

    #[test]
    fn restamps_when_links_change_and_skips_bodyless_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("only.html"), page("Only", "chaos-usage-manifest")).unwrap();
        fs::write(root.join("fragment.html"), "<div>no body tag</div>").unwrap();
        let first = build(root, "r").unwrap();
        assert_eq!(first.nav_stamped, 1);
        assert_eq!(first.nav_unchanged, 1, "bodyless file is left alone");

        // Rename changes the kind label → the bar differs → rewritten once.
        let html = fs::read_to_string(root.join("only.html")).unwrap();
        let swapped = html.replace("chaos-usage-manifest", "chaos-impact-data");
        fs::write(root.join("only.html"), swapped).unwrap();
        let second = build(root, "r").unwrap();
        assert_eq!(second.nav_stamped, 1);
        let html = fs::read_to_string(root.join("only.html")).unwrap();
        assert_eq!(html.matches(NAV_START).count(), 1);
        assert!(html.contains(">impact<"));
    }

    #[test]
    fn index_page_is_recognised_by_pages_scan() {
        let dir = tempfile::tempdir().unwrap();
        build(dir.path(), "r").unwrap();
        let entry = read_page_entry(&dir.path().join(INDEX_FILE)).unwrap();
        assert_eq!(entry.kind, "index");
        assert_eq!(entry.title, "r · feature memory");
    }

    #[test]
    fn find_body_open_handles_attributes_and_case() {
        assert_eq!(find_body_open("<BODY>hi"), Some(6));
        assert_eq!(find_body_open("<body class=\"a\"\n data-x>hi"), Some(24));
        assert_eq!(find_body_open("<bodyguard>"), None);
        assert_eq!(find_body_open("no body"), None);
    }
}
