//! `chaos impact` — feature-vs-existing-code impact report.
//!
//! Runs the same focused retrieval as `feature-context`, then ALWAYS writes an
//! interactive HTML that leads with an **impact summary** — the existing files
//! and symbols this feature touches, i.e. the codebase as it is *before* the
//! feature — followed by the evidence (feature matches, retrieval hits,
//! warnings). CLI `chaos impact` and the MCP `chaos_impact` tool both call
//! [`run`].
//!
//! Unlike `feature_context`, [`run`] returns a COMPACT JSON summary (counts +
//! capped affected-file/symbol lists + the output path), never the full hit
//! payload — the heavy evidence lives only in the written HTML, so an agent
//! calling it over MCP doesn't blow its context.

use crate::{
    embedding::Embedder,
    export_util::escape_script_json,
    feature_context::{
        build_feature_context_warnings, load_feature_matches, FeatureContextResponse,
    },
    models::SearchHit,
    query::query_feature_context_repo,
    storage::Storage,
};
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

const MAX_AFFECTED_FILES: usize = 24;
const MAX_AFFECTED_SYMBOLS: usize = 20;

/// Options for [`run`], shared by the CLI and MCP surfaces. Zero values fall
/// back to the same defaults as `feature-context` (limit 10, feature_limit 3,
/// nodes_per_feature 8).
#[derive(Debug, Default, Clone)]
pub struct ImpactOptions {
    pub features_dir: Option<PathBuf>,
    pub output_html: Option<PathBuf>,
    pub limit: i64,
    pub feature_limit: usize,
    pub nodes_per_feature: usize,
}

/// Run `chaos impact`: focused retrieval → impact summary → always-written HTML.
pub async fn run(
    storage: &Storage,
    embedder: &dyn Embedder,
    repo: &str,
    feature: &str,
    opts: &ImpactOptions,
) -> Result<Value> {
    let repo = storage
        .find_repository(repo)
        .await?
        .with_context(|| format!("repository is not indexed: {repo}"))?;
    let repo_root = PathBuf::from(&repo.root_path);
    let features_dir = opts
        .features_dir
        .clone()
        .unwrap_or_else(|| repo_root.join("docs/features_memory"));
    let limit = if opts.limit > 0 { opts.limit } else { 10 };
    let feature_limit = if opts.feature_limit > 0 {
        opts.feature_limit
    } else {
        3
    };
    let nodes_per_feature = if opts.nodes_per_feature > 0 {
        opts.nodes_per_feature
    } else {
        8
    };

    let postgres = query_feature_context_repo(storage, repo.id, embedder, feature, limit).await?;
    let warnings = build_feature_context_warnings(feature, &repo_root, &postgres);
    let feature_matches =
        load_feature_matches(feature, &features_dir, feature_limit, nodes_per_feature)?;
    let response = FeatureContextResponse {
        task: feature.to_string(),
        postgres,
        features_dir,
        warnings,
        feature_matches,
    };

    let summary = ImpactSummary::from_response(&response);

    let output = opts.output_html.clone().unwrap_or_else(|| {
        repo_root
            .join("docs/features_memory")
            .join(format!("{}-impact.html", safe_slug(feature)))
    });
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    write_impact_html(&output, &response, &summary)?;

    // Compact return — the full evidence is in the HTML, not this payload.
    Ok(json!({
        "status": "ok",
        "repo_id": repo.id,
        "feature": feature,
        "output_html": output,
        "impact": summary,
        "warnings": response.warnings,
    }))
}

#[derive(Debug, Serialize)]
pub struct ImpactSummary {
    /// Distinct existing files the feature retrieval touched.
    pub affected_file_count: usize,
    /// Distinct existing code symbols the feature retrieval touched.
    pub affected_symbol_count: usize,
    pub code_hits: usize,
    pub doc_hits: usize,
    pub warnings: usize,
    pub affected_files: Vec<AffectedFile>,
    pub affected_symbols: Vec<AffectedSymbol>,
}

#[derive(Debug, Serialize)]
pub struct AffectedFile {
    pub path: String,
    /// `code` or `docs`.
    pub kind: String,
    pub hits: usize,
    pub top_score: f64,
}

#[derive(Debug, Serialize)]
pub struct AffectedSymbol {
    pub name: String,
    pub file: String,
    pub lines: String,
    pub symbol_kind: String,
    pub score: f64,
}

impl ImpactSummary {
    fn from_response(response: &FeatureContextResponse) -> Self {
        // (hits, top_score, is_code) per file path.
        let mut files: HashMap<String, (usize, f64, bool)> = HashMap::new();
        let mut symbols: Vec<AffectedSymbol> = Vec::new();
        let mut code_hits = 0usize;
        let mut doc_hits = 0usize;

        for hit in &response.postgres.hits {
            let doc = is_doc_hit(hit);
            if doc {
                doc_hits += 1;
            } else {
                code_hits += 1;
            }
            if let Some(path) = &hit.file_path {
                let entry = files.entry(path.clone()).or_insert((0, hit.score, !doc));
                entry.0 += 1;
                if hit.score > entry.1 {
                    entry.1 = hit.score;
                }
                // A file counts as code if ANY of its hits is code.
                if !doc {
                    entry.2 = true;
                }
            }
            if let Some(symbol) = hit.metadata.get("symbol").and_then(Value::as_str) {
                symbols.push(AffectedSymbol {
                    name: symbol.to_string(),
                    file: hit.file_path.clone().unwrap_or_default(),
                    lines: line_range(hit.line_start, hit.line_end),
                    symbol_kind: hit
                        .metadata
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("symbol")
                        .to_string(),
                    score: hit.score,
                });
            }
        }

        let mut affected_files: Vec<AffectedFile> = files
            .into_iter()
            .map(|(path, (hits, top_score, is_code))| AffectedFile {
                path,
                kind: if is_code {
                    "code".into()
                } else {
                    "docs".into()
                },
                hits,
                top_score,
            })
            .collect();
        // Code before docs; then most-hit, then highest score.
        affected_files.sort_by(|a, b| {
            let ak = u8::from(a.kind != "code");
            let bk = u8::from(b.kind != "code");
            ak.cmp(&bk)
                .then(b.hits.cmp(&a.hits))
                .then(
                    b.top_score
                        .partial_cmp(&a.top_score)
                        .unwrap_or(Ordering::Equal),
                )
                .then(a.path.cmp(&b.path))
        });

        // Highest-scoring symbol per (name, file).
        symbols.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
        let mut seen: HashSet<(String, String)> = HashSet::new();
        symbols.retain(|s| seen.insert((s.name.clone(), s.file.clone())));

        let affected_file_count = affected_files.len();
        let affected_symbol_count = symbols.len();
        affected_files.truncate(MAX_AFFECTED_FILES);
        symbols.truncate(MAX_AFFECTED_SYMBOLS);

        ImpactSummary {
            affected_file_count,
            affected_symbol_count,
            code_hits,
            doc_hits,
            warnings: response.warnings.len(),
            affected_files,
            affected_symbols: symbols,
        }
    }
}

fn is_doc_hit(hit: &SearchHit) -> bool {
    hit.metadata
        .get("source_priority")
        .and_then(Value::as_str)
        .is_some_and(|p| p == "supplemental")
        || hit
            .metadata
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|k| k == "documentation")
}

fn line_range(start: Option<i32>, end: Option<i32>) -> String {
    match (start, end) {
        (Some(s), Some(e)) if s != e => format!("{s}-{e}"),
        (Some(s), _) => s.to_string(),
        _ => "n/a".into(),
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
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "feature".to_string()
    } else {
        slug.chars().take(80).collect::<String>()
    }
}

fn write_impact_html(
    path: &Path,
    response: &FeatureContextResponse,
    summary: &ImpactSummary,
) -> Result<()> {
    let data = json!({
        "task": response.task,
        "impact": summary,
        "warnings": response.warnings,
        "feature_matches": response.feature_matches,
        "hits": response.postgres.hits,
    });
    let json = serde_json::to_string(&data)?;
    fs::write(
        path,
        IMPACT_HTML.replace("__DATA__", &escape_script_json(&json)),
    )?;
    Ok(())
}

const IMPACT_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Feature Impact</title>
<style>
:root{--bg:#07080d;--panel:#10131d;--ink:#f5f7fb;--muted:#8d9ab8;--line:#293047;--cyan:#32e6ff;--pink:#ff3d9a;--amber:#ffb000;--green:#3cff98;--red:#ff5a4e}
*{box-sizing:border-box}body{margin:0;background:radial-gradient(circle at 16% 0%,rgba(50,230,255,.18),transparent 28%),radial-gradient(circle at 82% 10%,rgba(255,61,154,.16),transparent 24%),linear-gradient(180deg,#090a12,#05060a);color:var(--ink);font-family:Inter,ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif}
header{padding:30px 34px 22px;border-bottom:1px solid var(--line);background:linear-gradient(90deg,rgba(16,19,29,.94),rgba(16,19,29,.72))}
h1{margin:0 0 6px;font-size:clamp(28px,4vw,48px);text-shadow:0 0 28px rgba(50,230,255,.28)}
.muted{color:var(--muted);line-height:1.5}.sub{color:var(--muted);max-width:1100px;margin-top:8px;font-size:14px}
main{padding:18px;display:grid;gap:18px}
.panel{background:linear-gradient(180deg,rgba(21,25,39,.96),rgba(12,14,22,.96));border:1px solid var(--line);border-radius:8px;box-shadow:0 22px 80px rgba(0,0,0,.45);padding:16px}
h2{margin:0 0 12px;font-size:18px}h3{margin:14px 0 8px;font-size:14px;color:var(--muted);text-transform:uppercase;letter-spacing:.04em}
.stats{display:grid;grid-template-columns:repeat(auto-fit,minmax(140px,1fr));gap:12px;margin-bottom:6px}
.stat{border:1px solid var(--line);border-radius:8px;background:#0b0e16;padding:12px}.stat b{display:block;font-size:26px}.stat span{color:var(--muted);font-size:12px}
.grid2{display:grid;grid-template-columns:1fr 1fr;gap:18px}@media(max-width:900px){.grid2{grid-template-columns:1fr}}
.row{display:flex;justify-content:space-between;gap:10px;align-items:center;border:1px solid var(--line);border-radius:7px;background:#0b0e16;padding:9px 11px;margin-top:7px;font-size:13px;overflow-wrap:anywhere}
.row .meta{color:var(--muted);font-size:12px;white-space:nowrap}
.tag{display:inline-block;border-radius:999px;padding:2px 8px;font-size:11px;font-weight:800;text-transform:uppercase}.tag.code{color:#05060a;background:var(--cyan)}.tag.docs{color:#05060a;background:var(--amber)}
.item{border:1px solid var(--line);border-radius:7px;background:#0b0e16;padding:12px;margin-top:10px}.item strong{color:var(--cyan)}.item.doc strong{color:var(--amber)}.item.warn{border-color:rgba(255,176,0,.45)}.item.warn strong{color:var(--amber)}
pre{margin:10px 0 0;padding:14px;border-radius:8px;background:#030409;color:#d8e2ff;overflow:auto;font-size:12px;line-height:1.48;border:1px solid #242a3d;max-height:340px}
.pill{display:inline-block;border:1px solid var(--line);border-radius:999px;padding:3px 8px;margin:4px 5px 0 0;color:var(--cyan);font-size:12px}
</style>
</head>
<body data-chaos-impact>
<header><h1>Feature Impact</h1><div id="task" class="muted"></div><div class="sub">Existing files and symbols this feature touches &mdash; the codebase as it is <b>today, before this feature</b>. Verify the alignment, then implement against these nodes.</div></header>
<main>
<section class="panel" data-impact-summary><h2>Impact summary</h2>
<div id="stats" class="stats"></div>
<div class="grid2">
<div><h3>Existing files affected</h3><div id="files"></div></div>
<div><h3>Existing symbols affected</h3><div id="symbols"></div></div>
</div>
</section>
<section class="panel"><h2>Warnings</h2><div id="warnings"></div></section>
<section class="panel"><h2>Feature matches</h2><div id="features"></div></section>
<section class="panel"><h2>Retrieval evidence</h2><div id="hits"></div></section>
</main>
<script type="application/json" id="chaos-impact-data">__DATA__</script>
<script>
(function(){
var D=JSON.parse(document.getElementById("chaos-impact-data").textContent);
var I=D.impact||{};
function esc(v){return String(v==null?"":v).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;").replace(/'/g,"&#039;");}
function isDoc(h){var m=h&&h.metadata||{};return m.source_priority==="supplemental"||m.kind==="documentation";}
document.getElementById("task").textContent=D.task||"";
var stat=[["affected_file_count","files affected"],["affected_symbol_count","symbols affected"],["code_hits","code hits"],["doc_hits","doc hits"],["warnings","warnings"]];
document.getElementById("stats").innerHTML=stat.map(function(s){return '<div class="stat"><b>'+(I[s[0]]||0)+'</b><span>'+s[1]+'</span></div>';}).join("");
var files=document.getElementById("files");
(I.affected_files||[]).forEach(function(f){var el=document.createElement("div");el.className="row";el.innerHTML='<span><span class="tag '+esc(f.kind)+'">'+esc(f.kind)+'</span> '+esc(f.path)+'</span><span class="meta">'+f.hits+' hit'+(f.hits===1?"":"s")+' &middot; '+(f.top_score||0).toFixed(2)+'</span>';files.appendChild(el);});
if(!files.children.length)files.innerHTML='<div class="muted">No existing files matched. The index may be missing the relevant code, or the feature text needs stronger terms.</div>';
var symbols=document.getElementById("symbols");
(I.affected_symbols||[]).forEach(function(s){var el=document.createElement("div");el.className="row";el.innerHTML='<span><strong>'+esc(s.name)+'</strong> <span class="meta">'+esc(s.symbol_kind)+'</span><br><span class="meta">'+esc(s.file)+' : '+esc(s.lines)+'</span></span><span class="meta">'+(s.score||0).toFixed(2)+'</span>';symbols.appendChild(el);});
if(!symbols.children.length)symbols.innerHTML='<div class="muted">No code symbols surfaced (the retrieval may have matched docs only).</div>';
var warnings=document.getElementById("warnings");
(D.warnings||[]).forEach(function(w){var el=document.createElement("div");el.className="item warn";el.innerHTML='<strong>Context warning</strong><div>'+esc(w)+'</div>';warnings.appendChild(el);});
if(!warnings.children.length)warnings.innerHTML='<div class="muted">No stale-index or missing-doc warnings.</div>';
var features=document.getElementById("features");
(D.feature_matches||[]).forEach(function(f){var el=document.createElement("div");el.className="item";el.innerHTML='<strong>'+esc((f.feature&&f.feature.title)||f.title)+'</strong><div class="muted">'+esc(f.feature&&f.feature.domain)+' &middot; score '+f.score+' &middot; '+esc(f.page)+'</div>'+(f.modes||[]).map(function(m){return '<span class="pill">'+esc(m.title)+'</span>';}).join("")+(f.matched_nodes||[]).map(function(n){return '<div class="row"><span><strong>'+esc(n.label)+'</strong><br><span class="meta">'+esc(n.file)+' : '+esc(n.lines)+'</span></span><span class="meta">'+esc(n.group)+'</span></div>';}).join("");features.appendChild(el);});
if(!features.children.length)features.innerHTML='<div class="muted">No generated feature manifests matched. Impact is derived from retrieval evidence below.</div>';
var hits=document.getElementById("hits");
(D.hits||[]).forEach(function(h){var el=document.createElement("div");el.className="item"+(isDoc(h)?" doc":"");el.innerHTML='<strong>'+esc(h.file_path||"unknown file")+'</strong><div class="muted">'+(isDoc(h)?"docs":"code")+' &middot; lines '+esc(h.line_start)+'-'+esc(h.line_end)+' &middot; score '+(h.score||0).toFixed(3)+'</div><pre><code>'+esc(h.content)+'</code></pre>';hits.appendChild(el);});
if(!hits.children.length)hits.innerHTML='<div class="muted">No retrieval hits.</div>';
})();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::SearchHit;
    use crate::query::QueryResponse;
    use serde_json::json;
    use uuid::Uuid;

    fn hit(file: &str, score: f64, metadata: Value) -> SearchHit {
        SearchHit {
            chunk_id: Uuid::new_v4(),
            node_id: Some(Uuid::new_v4()),
            file_path: Some(file.into()),
            line_start: Some(10),
            line_end: Some(40),
            score,
            content: "snippet".into(),
            metadata,
        }
    }

    fn response(hits: Vec<SearchHit>, warnings: Vec<String>) -> FeatureContextResponse {
        FeatureContextResponse {
            task: "port tokenizer to OCL".into(),
            postgres: QueryResponse {
                hits,
                context_paths: vec![],
            },
            features_dir: PathBuf::from("/tmp/x"),
            warnings,
            feature_matches: vec![],
        }
    }

    #[test]
    fn impact_summary_ranks_code_before_docs_and_dedups_symbols() {
        let resp = response(
            vec![
                hit(
                    "IPNFT/src/Tokenizer.sol",
                    0.9,
                    json!({"symbol":"tokenizeIpnft","kind":"function"}),
                ),
                hit(
                    "IPNFT/src/Tokenizer.sol",
                    0.7,
                    json!({"symbol":"tokenizeIpnft","kind":"function"}),
                ),
                hit(
                    "docs/spec.md",
                    0.95,
                    json!({"source_priority":"supplemental","kind":"documentation"}),
                ),
            ],
            vec!["index may be stale".into()],
        );
        let s = ImpactSummary::from_response(&resp);

        assert_eq!(s.affected_file_count, 2);
        assert_eq!(s.code_hits, 2);
        assert_eq!(s.doc_hits, 1);
        assert_eq!(s.warnings, 1);
        // Code file first, even though the doc hit scored higher.
        assert_eq!(s.affected_files[0].path, "IPNFT/src/Tokenizer.sol");
        assert_eq!(s.affected_files[0].kind, "code");
        assert_eq!(s.affected_files[0].hits, 2);
        assert_eq!(s.affected_files[1].kind, "docs");
        // The repeated symbol is deduped to its highest-scoring hit.
        assert_eq!(s.affected_symbol_count, 1);
        assert_eq!(s.affected_symbols[0].name, "tokenizeIpnft");
        assert!((s.affected_symbols[0].score - 0.9).abs() < 1e-9);
    }

    #[test]
    fn impact_html_embeds_data_and_markers() {
        let resp = response(
            vec![hit(
                "src/a.rs",
                0.5,
                json!({"symbol":"f","kind":"function"}),
            )],
            vec![],
        );
        let summary = ImpactSummary::from_response(&resp);
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("x-impact.html");
        write_impact_html(&out, &resp, &summary).unwrap();
        let html = fs::read_to_string(&out).unwrap();
        assert!(html.contains("data-chaos-impact"));
        assert!(html.contains("data-impact-summary"));
        assert!(html.contains(r#"id="chaos-impact-data""#));
        assert!(html.contains("Feature Impact"));
    }
}
