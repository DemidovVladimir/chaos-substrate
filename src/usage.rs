//! `chaos_usage` — who CONSUMES a symbol or surface string across the repo.
//!
//! The cross-folder "who uses this?" answer, served entirely from the persisted
//! index so an agent never has to fall back to `rg` over the target repo. Given a
//! `target` (a function/method/struct name, an env-var name, an HTTP header or
//! route string, …) it gathers every use site from three embedder-free sources
//! and groups them by top-level subfolder:
//!
//! 1. **User-surface nodes** (`env_var` / `http_route` / `cli_command`) named
//!    exactly the target — AST-extracted real reads/registrations, no false hits
//!    from comments or prose. Every consumer across folders shares the same name.
//! 2. **Reverse graph edges** — the target resolves to its definition node(s) and
//!    `consumers_of_nodes` returns who `calls`/`imports`/`uses_type`/… it,
//!    cross-file (index-backed by `edges_target_node_idx`).
//! 3. **Literal chunk sweep** — an exact substring search across all chunks, the
//!    catch-all for cross-language string references (e.g. an `x-service-token`
//!    header) that are not first-class nodes. Sampled (≤2 hits/file).
//!
//! Like `chaos_impact` / `chaos_components` it ALWAYS writes an interactive HTML
//! page (manifest embedded under `id="chaos-usage-manifest"`) and returns a
//! COMPACT JSON summary, so an MCP caller's context is never flooded. Read-only
//! and embedder-free — it runs even when no embedder is configured.

use crate::{
    export_util::escape_script_json,
    provenance::{source, Breadcrumb},
    storage::{ConsumerRow, NodeRef, Storage},
};
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

/// Default cap on use sites listed per folder in the COMPACT return (the HTML
/// holds them all).
const DEFAULT_LIMIT: usize = 6;
/// Hard ceiling on use sites per folder in the compact return, even if the caller
/// passes a large `limit` — keeps the inline payload bounded.
const MAX_SITES_PER_FOLDER_RETURN: usize = 25;
/// Cap on folders listed in the compact return.
const MAX_FOLDERS_IN_RETURN: usize = 24;
/// Cap on definition pointers in the compact return — a common name (`new`,
/// `get`) resolves to many nodes; the full set stays in the HTML manifest.
const MAX_DEFINITIONS_IN_RETURN: usize = 12;
/// Per-term literal sweep budget (the storage layer additionally caps ≤2/file).
const LITERAL_BUDGET: i64 = 200;
/// User-surface node kinds — these ARE use sites, not definitions.
const SURFACE_KINDS: &[&str] = &["env_var", "http_route", "cli_command"];
/// Edge kinds that count as CONSUMPTION of a target (excludes structural
/// `contains`/`defines`/`documents`).
const CONSUMER_EDGE_KINDS: &[&str] = &[
    "calls",
    "imports",
    "uses_type",
    "implements",
    "tests",
    "depends_on",
];

#[derive(Debug, Default, Clone)]
pub struct UsageOptions {
    pub output_html: Option<PathBuf>,
    /// Use sites listed per folder in the compact return.
    pub limit: usize,
}

/// The embedded + (compacted) manifest describing where a target is used.
#[derive(Debug, Clone, Serialize)]
pub struct UsageManifest {
    pub schema_version: String,
    pub repo_name: String,
    pub target: String,
    pub title: String,
    pub subtitle: String,
    pub overview: String,
    /// Where the target itself is defined (context — not a use site).
    pub definitions: Vec<UsageDefinition>,
    /// Use sites grouped by top-level subfolder, most-used folder first.
    pub folders: Vec<UsageFolder>,
    pub provenance: Vec<Breadcrumb>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageDefinition {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: Option<i32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageFolder {
    pub folder: String,
    pub site_count: usize,
    pub languages: Vec<String>,
    pub sites: Vec<UsageSite>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageSite {
    pub file: String,
    pub line: Option<i32>,
    /// How the target is used here: `reads env var`, `calls`, `imports`,
    /// `registers route`, `references (literal)`, …
    pub mechanism: String,
    /// The consuming symbol (empty for a literal reference).
    pub consumer: String,
    /// The consuming node's kind (or `chunk` for a literal reference).
    pub kind: String,
    pub language: String,
    /// Retrieval channels for a literal site (empty for graph-derived sites).
    pub retrieved_by: Vec<String>,
}

/// Run the usage report: resolve target → gather sites (surface nodes + reverse
/// edges + literal sweep) → group by subfolder → write HTML → compact JSON.
/// Embedder-free.
pub async fn run(
    storage: &Storage,
    repo: &str,
    target: &str,
    opts: &UsageOptions,
) -> Result<Value> {
    let target = target.trim();
    if target.is_empty() {
        anyhow::bail!("target is required (the symbol or surface string whose consumers to find)");
    }
    let repo = storage
        .find_repository(repo)
        .await?
        .with_context(|| format!("repository is not indexed: {repo}"))?;
    let repo_root = PathBuf::from(&repo.root_path);
    // Hard upper bound so a large `limit` can't blow the compact-return budget
    // (the HTML always holds every site regardless).
    let limit = if opts.limit > 0 {
        opts.limit.min(MAX_SITES_PER_FOLDER_RETURN)
    } else {
        DEFAULT_LIMIT
    };

    let mut provenance: Vec<Breadcrumb> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // 1. Resolve the target to graph nodes, partitioned into user-surface use
    //    sites and code definitions (whose consumers come from reverse edges).
    let nodes: Vec<NodeRef> = storage.nodes_by_name_exact(repo.id, target).await?;
    let mut definitions: Vec<UsageDefinition> = Vec::new();
    let mut def_ids: Vec<uuid::Uuid> = Vec::new();
    let mut surface_sites: Vec<UsageSite> = Vec::new();
    for n in &nodes {
        if SURFACE_KINDS.contains(&n.kind.as_str()) {
            let file = n.file.clone().unwrap_or_default();
            surface_sites.push(UsageSite {
                language: language_for(&file),
                file,
                line: n.line_start,
                mechanism: surface_mechanism(&n.kind),
                consumer: n.name.clone(),
                kind: n.kind.clone(),
                retrieved_by: Vec::new(),
            });
        } else {
            def_ids.push(n.id);
            definitions.push(UsageDefinition {
                name: n.name.clone(),
                kind: n.kind.clone(),
                file: n.file.clone().unwrap_or_default(),
                line: n.line_start,
            });
        }
    }
    provenance.push(Breadcrumb::new(
        source::POSTGRES,
        "nodes_by_name_exact",
        format!(
            "resolved `{target}` → {} graph node(s): {} definition(s), {} user-surface use site(s)",
            nodes.len(),
            def_ids.len(),
            surface_sites.len()
        ),
    ));

    // 2. Reverse-edge consumers of the definition node(s).
    let consumer_kinds: Vec<String> = CONSUMER_EDGE_KINDS.iter().map(|s| s.to_string()).collect();
    let consumers: Vec<ConsumerRow> = storage
        .consumers_of_nodes(repo.id, &def_ids, &consumer_kinds)
        .await?;
    if !def_ids.is_empty() {
        provenance.push(Breadcrumb::new(
            source::GRAPH,
            "consumers_of_nodes",
            format!(
                "reverse-edge lookup over the persisted graph → {} consumer reference(s) ({}) of the definition node(s)",
                consumers.len(),
                CONSUMER_EDGE_KINDS.join("/")
            ),
        ));
    }
    let consumer_sites: Vec<UsageSite> = consumers
        .iter()
        .map(|c| {
            let file = c.consumer_file.clone().unwrap_or_default();
            UsageSite {
                language: language_for(&file),
                file,
                line: c.line_start,
                mechanism: edge_mechanism(&c.edge_kind),
                consumer: c.consumer_name.clone(),
                kind: c.consumer_kind.clone(),
                retrieved_by: Vec::new(),
            }
        })
        .collect();

    // 3. Literal chunk sweep — the cross-language catch-all (headers, strings).
    //    The target is passed verbatim so hyphenated/underscored surface strings
    //    match exactly; sampled ≤2 hits/file by the storage layer.
    let literal_hits = storage
        .literal_search(repo.id, target, LITERAL_BUDGET)
        .await?;
    let literal_sites: Vec<UsageSite> = literal_hits
        .iter()
        .map(|h| {
            let file = h.file_path.clone().unwrap_or_default();
            let retrieved_by = h
                .metadata
                .get("retrieved_by")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_else(|| vec!["literal".to_string()]);
            UsageSite {
                language: language_for(&file),
                file,
                line: h.line_start,
                mechanism: "references (literal)".to_string(),
                consumer: String::new(),
                kind: "chunk".to_string(),
                retrieved_by,
            }
        })
        .collect();
    provenance.push(Breadcrumb::new(
        source::POSTGRES,
        "literal_search",
        format!(
            "literal chunk sweep for `{target}` → {} reference(s) (sampled ≤2/file)",
            literal_sites.len()
        ),
    ));

    // 4. Merge + dedup by (file, line), preferring structured sites (surface,
    //    then reverse-edge) over a literal reference at the same spot.
    let mut seen: HashSet<(String, Option<i32>)> = HashSet::new();
    let mut all_sites: Vec<UsageSite> = Vec::new();
    for site in surface_sites
        .into_iter()
        .chain(consumer_sites)
        .chain(literal_sites)
    {
        if site.file.is_empty() {
            continue;
        }
        if seen.insert((site.file.clone(), site.line)) {
            all_sites.push(site);
        }
    }

    // The target's own definition lines are not "uses" — drop any site that lands
    // exactly on a definition (e.g. a literal hit on the defining line).
    let def_lines: HashSet<(String, Option<i32>)> = definitions
        .iter()
        .map(|d| (d.file.clone(), d.line))
        .collect();
    all_sites.retain(|s| !def_lines.contains(&(s.file.clone(), s.line)));

    let total_sites = all_sites.len();

    // 5. Group by top-level subfolder.
    let mut by_folder: BTreeMap<String, Vec<UsageSite>> = BTreeMap::new();
    for site in all_sites {
        by_folder
            .entry(top_folder(&site.file))
            .or_default()
            .push(site);
    }
    let mut folders: Vec<UsageFolder> = by_folder
        .into_iter()
        .map(|(folder, mut sites)| {
            sites.sort_by(|a, b| {
                a.file
                    .cmp(&b.file)
                    .then_with(|| a.line.cmp(&b.line))
                    .then_with(|| a.mechanism.cmp(&b.mechanism))
            });
            let languages = distinct_languages(&sites);
            UsageFolder {
                folder,
                site_count: sites.len(),
                languages,
                sites,
            }
        })
        .collect();
    // Most-used folder first.
    folders.sort_by(|a, b| {
        b.site_count
            .cmp(&a.site_count)
            .then_with(|| a.folder.cmp(&b.folder))
    });

    provenance.push(Breadcrumb::new(
        source::GRAPH,
        "usage_aggregation",
        format!(
            "grouped {total_sites} use site(s) into {} subfolder(s)",
            folders.len()
        ),
    ));

    // Honesty: ambiguous-name call/import edges resolve cross-file only for
    // repo-unique names; the literal sweep backstops but is sampled per file.
    if !def_ids.is_empty() {
        warnings.push(
            "Reverse-edge consumers (calls/imports) resolve cross-file only when the symbol name is repo-unique; ambiguously-named consumers may be undercounted. The literal sweep backstops this but samples ≤2 hits per file."
                .to_string(),
        );
    }
    if total_sites == 0 {
        warnings.push(format!(
            "No use sites found for `{target}`. Check the exact name/string, or run chaos_analyze/chaos_add so the index covers the consuming files."
        ));
    }

    let overview = compose_overview(target, &repo.name, &definitions, total_sites, &folders);
    let title = format!("{target} — usage across {}", repo.name);
    let subtitle = "Every place this symbol or surface string is consumed, grouped by subfolder — from user-surface nodes (env vars, routes), reverse code edges (calls/imports), and a literal sweep. Resolved entirely from the index; no repo grep.".to_string();

    let manifest = UsageManifest {
        schema_version: "usage-1".to_string(),
        repo_name: repo.name.clone(),
        target: target.to_string(),
        title,
        subtitle,
        overview,
        definitions,
        folders,
        provenance,
        warnings: warnings.clone(),
    };

    // 6. Always write the HTML report.
    let output = opts.output_html.clone().unwrap_or_else(|| {
        repo_root
            .join("docs/features_memory")
            .join(format!("{}-usage.html", safe_slug(target)))
    });
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    write_usage_html(&output, &manifest)?;

    // 7. Compact JSON return — capped per folder and per definition; full detail
    //    stays in the HTML.
    let folder_count = manifest.folders.len();
    let definition_count = manifest.definitions.len();
    let shown_definitions: Vec<&UsageDefinition> = manifest
        .definitions
        .iter()
        .take(MAX_DEFINITIONS_IN_RETURN)
        .collect();
    let definitions_omitted = definition_count.saturating_sub(shown_definitions.len());
    let compact_folders: Vec<Value> = manifest
        .folders
        .iter()
        .take(MAX_FOLDERS_IN_RETURN)
        .map(|f| {
            let shown: Vec<Value> = f
                .sites
                .iter()
                .take(limit)
                .map(|s| {
                    json!({
                        "file": s.file,
                        "line": s.line,
                        "mechanism": s.mechanism,
                        "consumer": s.consumer,
                        "kind": s.kind,
                    })
                })
                .collect();
            json!({
                "folder": f.folder,
                "site_count": f.site_count,
                "languages": f.languages,
                "sites": shown,
                "sites_omitted": f.site_count.saturating_sub(shown.len()),
            })
        })
        .collect();

    Ok(json!({
        "status": "ok",
        "repo_id": repo.id,
        "target": manifest.target,
        "overview": manifest.overview,
        "definition_count": definition_count,
        "definitions": shown_definitions,
        "definitions_omitted": definitions_omitted,
        "site_count": total_sites,
        "folder_count": folder_count,
        "folders_omitted": folder_count.saturating_sub(compact_folders.len()),
        "folders": compact_folders,
        "provenance": manifest.provenance,
        "output_html": output,
        "warnings": warnings,
    }))
}

fn surface_mechanism(kind: &str) -> String {
    match kind {
        "env_var" => "reads env var",
        "http_route" => "registers route",
        "cli_command" => "defines CLI command",
        other => other,
    }
    .to_string()
}

fn edge_mechanism(edge_kind: &str) -> String {
    match edge_kind {
        "calls" => "calls",
        "imports" => "imports",
        "uses_type" => "uses type",
        "implements" => "implements",
        "tests" => "tests",
        "depends_on" => "depends on",
        other => other,
    }
    .to_string()
}

/// Top-level subfolder of a path (`(root)` for a root-level file).
fn top_folder(path: &str) -> String {
    match path.split_once('/') {
        Some((head, _)) if !head.is_empty() => head.to_string(),
        _ => "(root)".to_string(),
    }
}

fn distinct_languages(sites: &[UsageSite]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for s in sites {
        if !s.language.is_empty() && seen.insert(s.language.clone()) {
            out.push(s.language.clone());
        }
    }
    out.sort();
    out
}

fn language_for(path: &str) -> String {
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
        "md" | "mdx" => "Markdown",
        "pdf" => "PDF",
        "json" => "JSON",
        "toml" => "TOML",
        "yaml" | "yml" => "YAML",
        _ => "",
    }
    .to_string()
}

/// Deterministic extractive overview paragraph (pure — same inputs ⇒ same text).
fn compose_overview(
    target: &str,
    repo_name: &str,
    definitions: &[UsageDefinition],
    total_sites: usize,
    folders: &[UsageFolder],
) -> String {
    if total_sites == 0 {
        return format!(
            "No use sites for `{target}` were found in {repo_name}. Verify the exact name/string, or index the consuming files (chaos_analyze/chaos_add)."
        );
    }
    let def_line = match definitions.len() {
        0 => format!("`{target}` is used"),
        1 => format!("`{target}` is defined in {} and used", definitions[0].file),
        n => format!("`{target}` ({n} definitions) is used"),
    };
    let folder_names: Vec<&str> = folders.iter().map(|f| f.folder.as_str()).collect();
    format!(
        "{def_line} at {total_sites} site(s) across {} subfolder(s): {}.",
        folders.len(),
        join_human(&folder_names)
    )
}

fn join_human(items: &[&str]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].to_string(),
        2 => format!("{} and {}", items[0], items[1]),
        _ => {
            let (last, head) = items.split_last().unwrap();
            format!("{}, and {}", head.join(", "), last)
        }
    }
}

fn safe_slug(input: &str) -> String {
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
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "usage".to_string()
    } else {
        slug.chars().take(80).collect::<String>()
    }
}

fn write_usage_html(path: &Path, manifest: &UsageManifest) -> Result<()> {
    let json = serde_json::to_string(manifest)?;
    fs::write(
        path,
        USAGE_HTML
            .replace("__THEME__", crate::theme::THEME_CSS)
            .replace(
                "__BRAND_TOPBAR__",
                &crate::theme::render_brand(&crate::theme::Brand::default(), "topbar"),
            )
            .replace(
                "__BRAND_FOOTER__",
                &crate::theme::render_brand(&crate::theme::Brand::default(), "footer"),
            )
            .replace("__DATA__", &escape_script_json(&json)),
    )?;
    Ok(())
}

const USAGE_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Usage</title>
<style>
__THEME__
/* ===== usage report (light editorial) ===== */
header.ov{background:var(--bg-sky-soft);border-bottom:var(--border-hairline)}
header.ov .wrap{padding:48px 32px 36px}
header.ov .eyebrow{font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.16em;color:var(--color-blue-700);margin-bottom:16px}
header.ov h1{font:var(--type-display-lg);letter-spacing:-.01em;color:var(--color-ink-700);margin:0 0 10px;font-family:var(--font-mono)}
#overview{font:var(--type-body-lg);color:var(--color-ink-500);line-height:1.55;max-width:78ch}
.sub{color:var(--color-ink-400);max-width:74ch;margin-top:14px;font:var(--type-body-sm);line-height:1.6}
main{padding:40px 0 64px;display:grid;gap:24px}
.panel{background:var(--color-surface-0);border:var(--border-hairline);border-radius:var(--radius-lg);box-shadow:var(--shadow-sm);padding:24px}
h2{font:var(--type-h4);color:var(--color-ink-700);margin:0 0 16px}
.muted{color:var(--fg-tertiary);line-height:1.5}
.stats{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:16px}
.stat{border:var(--border-hairline);border-radius:var(--radius-md);background:var(--color-surface-2);padding:18px}
.stat b{display:block;font:var(--type-h2);font-family:var(--font-display);color:var(--color-ink-700);line-height:1}
.stat span{display:block;color:var(--fg-tertiary);font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.08em;margin-top:8px}
.folder{border:var(--border-hairline);border-radius:var(--radius-lg);background:var(--color-surface-0);padding:18px 20px;margin-top:14px;box-shadow:var(--shadow-xs)}
.folder h3{margin:0 0 6px;font:var(--type-h5);color:var(--color-ink-700);font-family:var(--font-mono);display:flex;align-items:center;gap:10px;flex-wrap:wrap}
.folder .count{display:inline-flex;align-items:center;justify-content:center;min-width:26px;height:24px;padding:0 8px;border-radius:var(--radius-pill);background:var(--color-ink-600);color:#fff;font:var(--type-overline-sm);font-family:var(--font-mono)}
.lang{display:inline-flex;border-radius:var(--radius-pill);padding:3px 10px;margin:0 4px 0 0;font:var(--type-overline-sm);font-family:var(--font-mono);background:var(--color-blue-50);color:var(--color-blue-700)}
.site{display:flex;justify-content:space-between;gap:12px;align-items:baseline;border-top:var(--border-hairline);padding:8px 0;font:var(--type-body-sm)}
.site:first-of-type{border-top:0}
.site code{color:var(--color-ink-600);font:var(--type-body-xs);font-family:var(--font-mono);overflow-wrap:anywhere}
.site .mech{display:inline-block;border-radius:var(--radius-pill);padding:2px 9px;font:var(--type-overline-sm);font-family:var(--font-mono);background:var(--color-surface-2);color:var(--color-ink-500);white-space:nowrap}
.site .mech.env{background:rgba(0,200,187,.12);color:#007f76}
.site .mech.route{background:rgba(255,193,7,.16);color:#9a6700}
.site .mech.lit{background:var(--color-surface-1);color:var(--fg-tertiary)}
.site .who{color:var(--fg-tertiary);font:var(--type-body-xs);font-family:var(--font-mono)}
.def{border:var(--border-hairline);border-radius:var(--radius-md);background:var(--color-surface-1);padding:10px 14px;margin-top:8px;font:var(--type-body-sm)}
.def code{font-family:var(--font-mono);color:var(--color-blue-700)}
.matched div{color:var(--color-ink-500);font:var(--type-body-xs);line-height:1.5}
.matched b{color:var(--color-ink-700);font-weight:500;font-family:var(--font-mono);text-transform:uppercase;letter-spacing:.04em;font-size:10px}
.item.warn{border:1px solid var(--color-blue-300);border-radius:var(--radius-md);background:var(--color-blue-50);padding:14px 16px;margin-top:12px}
.item.warn strong{color:var(--color-blue-700);font:var(--type-h6);display:block;margin-bottom:4px}
.item.warn div{color:var(--color-ink-500);font:var(--type-body-sm);line-height:1.5}
</style>
</head>
<body data-chaos-usage>
<div class="topbar"><div class="wrap">__BRAND_TOPBAR__<span class="crumb">Usage<span class="sep">&rsaquo;</span><b>consumers</b></span><span class="sp"></span><span class="pilltag">Usage</span></div></div>
<header class="ov">
  <div class="wrap">
    <div class="eyebrow">Who consumes this</div>
    <h1 id="title">Usage</h1>
    <div id="overview"></div>
    <div class="sub" id="subtitle"></div>
  </div>
</header>
<main>
  <div class="wrap">
    <section class="panel"><div id="stats" class="stats"></div></section>
    <section class="panel" data-usage-defs><h2>Defined at</h2><div class="muted" style="margin-bottom:10px">Where the target itself lives (context &mdash; not a use site).</div><div id="definitions"></div></section>
    <section class="panel" data-usage-folders><h2>Use sites by subfolder</h2><div class="muted" style="margin-bottom:10px">Most-used folder first. Mechanism shows HOW each site uses the target.</div><div id="folders"></div></section>
    <section class="panel" data-usage-provenance><h2>How this was generated</h2><div class="muted" style="margin-bottom:10px">Provenance breadcrumbs &mdash; the index queries that produced this report.</div><div id="provenance"></div></section>
    <section class="panel"><h2>Warnings</h2><div id="warnings"></div></section>
  </div>
</main>
<footer><div class="wrap">__BRAND_FOOTER__<span class="sp"></span><span class="meta">generated by Chaos Substrate</span></div></footer>
<script type="application/json" id="chaos-usage-manifest">__DATA__</script>
<script>
(function(){
var D=JSON.parse(document.getElementById("chaos-usage-manifest").textContent);
function esc(v){return String(v==null?"":v).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;").replace(/'/g,"&#039;");}
document.getElementById("title").textContent=D.title||"Usage";
document.getElementById("overview").textContent=D.overview||"";
document.getElementById("subtitle").textContent=D.subtitle||"";
var F=D.folders||[];
var totalSites=F.reduce(function(a,f){return a+(f.site_count||0);},0);
var stat=[[totalSites,"use sites"],[F.length,"subfolders"],[(D.definitions||[]).length,"definitions"],[(D.warnings||[]).length,"warnings"]];
document.getElementById("stats").innerHTML=stat.map(function(s){return '<div class="stat"><b>'+s[0]+'</b><span>'+s[1]+'</span></div>';}).join("");
var defs=document.getElementById("definitions");
(D.definitions||[]).forEach(function(d){var el=document.createElement("div");el.className="def";el.innerHTML='<code>'+esc(d.name)+'</code> <span class="who">'+esc(d.kind)+'</span> &middot; <code>'+esc(d.file)+(d.line?':'+d.line:'')+'</code>';defs.appendChild(el);});
if(!defs.children.length)defs.innerHTML='<div class="muted">No definition node found in the index (the target may be an external symbol or a surface string).</div>';
function mechClass(m){if(/env/.test(m))return 'env';if(/route/.test(m))return 'route';if(/literal/.test(m))return 'lit';return '';}
var host=document.getElementById("folders");
F.forEach(function(f){
  var el=document.createElement("div");el.className="folder";
  var langs=(f.languages||[]).map(function(l){return '<span class="lang">'+esc(l)+'</span>';}).join("");
  var sites=(f.sites||[]).map(function(s){
    return '<div class="site"><span><span class="mech '+mechClass(s.mechanism)+'">'+esc(s.mechanism)+'</span> <code>'+esc(s.file)+(s.line?':'+s.line:'')+'</code></span>'+(s.consumer?'<span class="who">'+esc(s.consumer)+' &middot; '+esc(s.kind)+'</span>':'<span class="who">'+esc(s.kind)+'</span>')+'</div>';
  }).join("");
  el.innerHTML='<h3><span class="count">'+(f.site_count||0)+'</span>'+esc(f.folder)+' '+langs+'</h3>'+sites;
  host.appendChild(el);
});
if(!host.children.length)host.innerHTML='<div class="muted">No use sites found. Verify the exact name/string, or index the consuming files (chaos_analyze).</div>';
var prov=document.getElementById("provenance");
(D.provenance||[]).forEach(function(c){var el=document.createElement("div");el.className="matched";el.innerHTML='<div><b>'+esc(c.source)+'</b> '+esc(c.method)+'</div><div class="muted">'+esc(c.detail)+(c.locator?' &middot; '+esc(c.locator):'')+'</div>';prov.appendChild(el);});
if(!prov.children.length)prov.innerHTML='<div class="muted">No breadcrumbs recorded.</div>';
var w=document.getElementById("warnings");
(D.warnings||[]).forEach(function(x){var el=document.createElement("div");el.className="item warn";el.innerHTML='<strong>Note</strong><div>'+esc(x)+'</div>';w.appendChild(el);});
if(!w.children.length)w.innerHTML='<div class="muted">No warnings.</div>';
})();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_folder_buckets_root_and_nested() {
        assert_eq!(top_folder("desci-infra/lambda/x.ts"), "desci-infra");
        assert_eq!(top_folder("README.md"), "(root)");
        assert_eq!(top_folder(""), "(root)");
    }

    #[test]
    fn mechanisms_are_human_readable() {
        assert_eq!(surface_mechanism("env_var"), "reads env var");
        assert_eq!(surface_mechanism("http_route"), "registers route");
        assert_eq!(edge_mechanism("uses_type"), "uses type");
        assert_eq!(edge_mechanism("calls"), "calls");
    }

    #[test]
    fn language_for_maps_known_extensions() {
        assert_eq!(language_for("a/b/c.ts"), "TypeScript");
        assert_eq!(language_for("x.py"), "Python");
        assert_eq!(language_for("Contract.sol"), "Solidity");
        assert_eq!(language_for("noext"), "");
    }

    #[test]
    fn overview_is_deterministic_and_grounded() {
        let defs = vec![UsageDefinition {
            name: "authenticateServiceToken".into(),
            kind: "function".into(),
            file: "desci-infra/auth.ts".into(),
            line: Some(341),
        }];
        let folders = vec![
            UsageFolder {
                folder: "skills".into(),
                site_count: 3,
                languages: vec!["Python".into()],
                sites: vec![],
            },
            UsageFolder {
                folder: "desci-infra".into(),
                site_count: 2,
                languages: vec!["TypeScript".into()],
                sites: vec![],
            },
        ];
        let a = compose_overview(
            "authenticateServiceToken",
            "molecule_core",
            &defs,
            5,
            &folders,
        );
        let b = compose_overview(
            "authenticateServiceToken",
            "molecule_core",
            &defs,
            5,
            &folders,
        );
        assert_eq!(a, b);
        assert!(a.contains("authenticateServiceToken"));
        assert!(a.contains("desci-infra/auth.ts"));
        assert!(a.contains("5 site(s)"));
        assert!(a.contains("skills") && a.contains("desci-infra"));
    }

    #[test]
    fn overview_handles_empty() {
        let text = compose_overview("missing", "repo", &[], 0, &[]);
        assert!(text.contains("No use sites"));
    }

    #[test]
    fn usage_html_embeds_idtagged_manifest() {
        let manifest = UsageManifest {
            schema_version: "usage-1".into(),
            repo_name: "repo".into(),
            target: "X".into(),
            title: "X — usage across repo".into(),
            subtitle: "s".into(),
            overview: "o".into(),
            definitions: vec![],
            folders: vec![],
            provenance: vec![],
            warnings: vec![],
        };
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("x-usage.html");
        write_usage_html(&out, &manifest).unwrap();
        let html = fs::read_to_string(&out).unwrap();
        assert!(html.contains(r#"id="chaos-usage-manifest""#));
        assert!(html.contains("data-chaos-usage"));
        assert!(html.contains("JSON.parse"));
    }
}
