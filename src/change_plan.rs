//! `chaos_change_plan` — decompose a change into the features it spans.
//!
//! This is the top-down primitive the hierarchical-memory thread was built for:
//! given a change *description* (and optionally a real git diff), match it
//! against the L1 community summary embeddings (P3) and/or the communities the
//! diff directly touches, then return the set of features the change spans —
//! each with its members, a dependency-aware **check order** (topo-sort over the
//! quotient graph), and a confidence.
//!
//! Like `chaos_impact`, it ALWAYS writes an interactive HTML report (light editorial theme)
//! and returns a COMPACT JSON summary, so an agent calling it over MCP doesn't
//! get its context flooded.

use crate::{
    embedding::Embedder,
    export_util::escape_script_json,
    feature_context::load_feature_matches,
    provenance::{source, Breadcrumb},
    storage::{CommunityMatch, Storage},
};
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use uuid::Uuid;

/// Semantic candidates below this cosine score are ignored (noise floor).
const MIN_SEMANTIC_SCORE: f64 = 0.30;
/// Default cap on features surfaced.
const DEFAULT_LIMIT: usize = 8;
/// Top member symbols shown per feature in the compact JSON.
const JSON_SYMBOLS: i64 = 6;
/// Top member symbols shown per feature in the HTML.
const HTML_SYMBOLS: i64 = 14;

#[derive(Debug, Default, Clone)]
pub struct ChangePlanOptions {
    pub output_html: Option<PathBuf>,
    /// Also seed from the files changed vs. this git ref (e.g. `HEAD`, `main`).
    pub diff_since: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannedFeature {
    pub id: Uuid,
    pub label: String,
    pub summary: Option<String>,
    pub member_count: i32,
    /// 0..1 — cosine match (semantic), 1.0 (diff-touched), or the stronger of both.
    pub confidence: f64,
    /// How it was surfaced, as a `+`-joined set of sources: e.g. `"semantic"`,
    /// `"diff"`, `"manifest"`, `"semantic+diff"`.
    pub via: String,
    /// 1-based position in the suggested check order.
    pub check_order: usize,
    pub top_symbols: Vec<PlannedSymbol>,
    /// Breadcrumbs: how this feature was matched (cosine score, git diff, prior
    /// feature page correlation).
    #[serde(default)]
    pub matched_by: Vec<Breadcrumb>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannedSymbol {
    pub name: String,
    pub kind: String,
    pub file: String,
}

/// Run the change plan: match → decompose → order → write HTML → compact JSON.
pub async fn run(
    storage: &Storage,
    embedder: &dyn Embedder,
    repo: &str,
    change: &str,
    opts: &ChangePlanOptions,
) -> Result<Value> {
    let repo = storage
        .find_repository(repo)
        .await?
        .with_context(|| format!("repository is not indexed: {repo}"))?;
    let repo_root = PathBuf::from(&repo.root_path);
    let limit = if opts.limit > 0 {
        opts.limit
    } else {
        DEFAULT_LIMIT
    };

    let mut warnings: Vec<String> = Vec::new();

    // 1. Semantic match against community summaries (top-down entry point).
    let query_embedding = embedder.embed(change).await?;
    let semantic = storage
        .community_semantic_search(
            repo.id,
            embedder.provider(),
            embedder.model_id(),
            embedder.dimensions(),
            &query_embedding,
            (limit * 3).max(12) as i64,
        )
        .await?;
    if semantic.is_empty() {
        warnings.push(
            "no community summaries matched — run chaos_analyze/chaos_add so the hierarchy (P1–P3) exists for this repo".into(),
        );
    }

    let semantic_count = semantic
        .iter()
        .filter(|m| m.member_count >= 2 && m.score >= MIN_SEMANTIC_SCORE)
        .count();

    // 2. Optional diff seeding: communities the actual change touches.
    let mut diff_ids: HashSet<Uuid> = HashSet::new();
    if let Some(since) = &opts.diff_since {
        let paths = git_changed_paths(&repo_root, since);
        if paths.is_empty() {
            warnings.push(format!(
                "no changed files found vs `{since}` (clean tree, bad ref, or non-git repo)"
            ));
        } else {
            for id in storage.communities_for_files(repo.id, &paths).await? {
                diff_ids.insert(id);
            }
        }
    }

    // 2b. Prior-manifest seeding: correlate the change against previously
    //     generated feature pages (token match), then map their files onto
    //     communities so a curated existing feature deepens the decomposition.
    let features_dir = repo_root.join("docs/features_memory");
    let manifest_matches =
        load_feature_matches(change, &features_dir, limit.max(3), 24).unwrap_or_default();
    let mut manifest_files: Vec<String> = manifest_matches
        .iter()
        .flat_map(|m| m.matched_nodes.iter())
        .filter(|n| !n.file.is_empty())
        .map(|n| n.file.clone())
        .collect();
    manifest_files.sort();
    manifest_files.dedup();
    let manifest_pages: Vec<String> = manifest_matches.iter().map(|m| m.title.clone()).collect();
    let mut manifest_ids: HashSet<Uuid> = HashSet::new();
    if !manifest_files.is_empty() {
        for id in storage
            .communities_for_files(repo.id, &manifest_files)
            .await?
        {
            manifest_ids.insert(id);
        }
    }

    // 3. Union semantic + diff + manifest into one scored set (feature
    //    communities only).
    let diff_briefs = storage
        .load_community_briefs(repo.id, &diff_ids.iter().copied().collect::<Vec<_>>())
        .await?;
    let manifest_briefs = storage
        .load_community_briefs(repo.id, &manifest_ids.iter().copied().collect::<Vec<_>>())
        .await?;

    struct Acc {
        m: CommunityMatch,
        semantic: f64,
        via_diff: bool,
        via_manifest: bool,
    }
    let mut by_id: HashMap<Uuid, Acc> = HashMap::new();
    for m in semantic {
        if m.member_count < 2 || m.score < MIN_SEMANTIC_SCORE {
            continue;
        }
        by_id.insert(
            m.id,
            Acc {
                semantic: m.score,
                via_diff: false,
                via_manifest: false,
                m,
            },
        );
    }
    for m in diff_briefs {
        if m.member_count < 2 {
            continue;
        }
        by_id
            .entry(m.id)
            .and_modify(|a| a.via_diff = true)
            .or_insert(Acc {
                semantic: 0.0,
                via_diff: true,
                via_manifest: false,
                m,
            });
    }
    for m in manifest_briefs {
        if m.member_count < 2 {
            continue;
        }
        by_id
            .entry(m.id)
            .and_modify(|a| a.via_manifest = true)
            .or_insert(Acc {
                semantic: 0.0,
                via_diff: false,
                via_manifest: true,
                m,
            });
    }

    // 4. Rank by confidence, cap to limit. A diff-touched feature is certain
    //    (1.0); a prior-manifest correlation is decent evidence (≥ 0.55 floor);
    //    otherwise the cosine score stands.
    let mut ranked: Vec<(Uuid, f64, String, Vec<Breadcrumb>, CommunityMatch)> = by_id
        .into_values()
        .map(|a| {
            let manifest_floor = if a.via_manifest { 0.55 } else { 0.0 };
            let confidence = if a.via_diff {
                1.0
            } else {
                a.semantic.max(manifest_floor)
            };
            let mut sources: Vec<&str> = Vec::new();
            let mut matched_by: Vec<Breadcrumb> = Vec::new();
            if a.semantic >= MIN_SEMANTIC_SCORE {
                sources.push("semantic");
                matched_by.push(Breadcrumb::new(
                    source::EMBEDDING,
                    "community_semantic_search",
                    format!(
                        "cosine {:.2} vs the community summary embedding",
                        a.semantic
                    ),
                ));
            }
            if a.via_diff {
                sources.push("diff");
                matched_by.push(Breadcrumb::new(
                    source::GIT,
                    "communities_for_files",
                    format!(
                        "touched by the git diff vs `{}`",
                        opts.diff_since.as_deref().unwrap_or("ref")
                    ),
                ));
            }
            if a.via_manifest {
                sources.push("manifest");
                matched_by.push(Breadcrumb::new(
                    source::MANIFEST,
                    "load_feature_matches",
                    "files overlap a previously generated feature page",
                ));
            }
            if sources.is_empty() {
                sources.push("semantic");
            }
            (a.m.id, confidence, sources.join("+"), matched_by, a.m)
        })
        .collect();
    ranked.sort_by(|x, y| {
        y.1.partial_cmp(&x.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(y.4.member_count.cmp(&x.4.member_count))
            .then(x.0.cmp(&y.0))
    });
    ranked.truncate(limit);

    let selected_ids: Vec<Uuid> = ranked.iter().map(|r| r.0).collect();

    // 5. Dependency-aware check order: topo-sort the quotient links, preferring
    //    the confidence ranking among independent features.
    let links = storage
        .directed_community_links(repo.id, &selected_ids)
        .await?;
    let order = topo_sort(&selected_ids, &links);
    let order_index: HashMap<Uuid, usize> = order
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, i + 1))
        .collect();

    // 6. Assemble features (in check order).
    let mut features: Vec<PlannedFeature> = Vec::new();
    for (id, confidence, via, matched_by, m) in &ranked {
        let symbols = storage
            .load_community_top_symbols(*id, HTML_SYMBOLS)
            .await?;
        features.push(PlannedFeature {
            id: *id,
            label: m.label.clone(),
            summary: m.summary.clone(),
            member_count: m.member_count,
            confidence: *confidence,
            via: via.clone(),
            check_order: order_index.get(id).copied().unwrap_or(usize::MAX),
            top_symbols: symbols
                .into_iter()
                .map(|(name, kind, file)| PlannedSymbol { name, kind, file })
                .collect(),
            matched_by: matched_by.clone(),
        });
    }
    features.sort_by(|a, b| a.check_order.cmp(&b.check_order));

    // Artifact-level breadcrumbs: how the whole plan was decomposed.
    let mut provenance = vec![Breadcrumb::new(
        source::EMBEDDING,
        "community_semantic_search",
        format!(
            "matched the change text against community summary embeddings → {semantic_count} feature(s) (cosine ≥ {MIN_SEMANTIC_SCORE:.2})"
        ),
    )];
    if let Some(since) = &opts.diff_since {
        provenance.push(Breadcrumb::new(
            source::GIT,
            "communities_for_files",
            format!(
                "seeded from files changed vs `{since}` → {} community(ies)",
                diff_ids.len()
            ),
        ));
    }
    if !manifest_matches.is_empty() {
        provenance.push(
            Breadcrumb::new(
                source::MANIFEST,
                "load_feature_matches",
                format!(
                    "correlated {} prior feature page(s) [{}] → {} community(ies)",
                    manifest_matches.len(),
                    manifest_pages.join(", "),
                    manifest_ids.len()
                ),
            )
            .with_locator(features_dir.display().to_string()),
        );
    }
    provenance.push(Breadcrumb::new(
        source::GRAPH,
        "topo_sort",
        format!(
            "ordered {} feature(s) by the quotient dependency graph",
            features.len()
        ),
    ));

    // 7. Always write the HTML report.
    let output = opts.output_html.clone().unwrap_or_else(|| {
        repo_root
            .join("docs/features_memory")
            .join(format!("{}-plan.html", safe_slug(change)))
    });
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    write_change_plan_html(&output, change, &features, &warnings, &provenance)?;

    // 8. Compact JSON return (full detail stays in the HTML).
    let compact_features: Vec<Value> = features
        .iter()
        .map(|f| {
            json!({
                "label": f.label,
                "member_count": f.member_count,
                "confidence": (f.confidence * 100.0).round() / 100.0,
                "via": f.via,
                "check_order": f.check_order,
                "top_symbols": f.top_symbols.iter().take(JSON_SYMBOLS as usize)
                    .map(|s| s.name.clone()).collect::<Vec<_>>(),
            })
        })
        .collect();

    Ok(json!({
        "status": "ok",
        "repo_id": repo.id,
        "change": change,
        "feature_count": features.len(),
        "features": compact_features,
        "provenance": provenance,
        "output_html": output,
        "warnings": warnings,
    }))
}

/// Deterministic topo-sort for the check order.
///
/// An L0 link `A → B` means A depends on B, so B (the dependency) should be
/// checked first; we emit a precedence `B ≺ A`. Kahn's algorithm runs over that
/// precedence graph; among ready (no-unmet-prerequisite) features it picks the
/// earliest in `priority` (the confidence ranking). Cycles are broken
/// deterministically by emitting the earliest-by-priority remaining feature.
pub fn topo_sort(priority: &[Uuid], links: &[(Uuid, Uuid, i64)]) -> Vec<Uuid> {
    let set: HashSet<Uuid> = priority.iter().copied().collect();
    // precedence edges B -> A (B before A) for each link A depends-on B.
    let mut after: BTreeMap<Uuid, Vec<Uuid>> = BTreeMap::new();
    let mut indegree: HashMap<Uuid, usize> = priority.iter().map(|id| (*id, 0)).collect();
    let mut seen_edge: HashSet<(Uuid, Uuid)> = HashSet::new();
    for (a, b, _) in links {
        if a == b || !set.contains(a) || !set.contains(b) {
            continue;
        }
        if !seen_edge.insert((*b, *a)) {
            continue;
        }
        after.entry(*b).or_default().push(*a);
        *indegree.entry(*a).or_insert(0) += 1;
    }

    let rank: HashMap<Uuid, usize> = priority
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, i))
        .collect();
    let pick_ready = |remaining: &HashSet<Uuid>, indeg: &HashMap<Uuid, usize>| -> Option<Uuid> {
        remaining
            .iter()
            .filter(|id| indeg.get(*id).copied().unwrap_or(0) == 0)
            .min_by_key(|id| rank.get(*id).copied().unwrap_or(usize::MAX))
            .copied()
    };

    let mut remaining: HashSet<Uuid> = set.clone();
    let mut out: Vec<Uuid> = Vec::with_capacity(priority.len());
    while !remaining.is_empty() {
        let next = pick_ready(&remaining, &indegree).unwrap_or_else(|| {
            // Cycle: break by the earliest-by-priority remaining feature.
            *remaining
                .iter()
                .min_by_key(|id| rank.get(*id).copied().unwrap_or(usize::MAX))
                .expect("non-empty")
        });
        remaining.remove(&next);
        if let Some(successors) = after.get(&next) {
            for s in successors {
                if let Some(d) = indegree.get_mut(s) {
                    *d = d.saturating_sub(1);
                }
            }
        }
        out.push(next);
    }
    out
}

fn git_changed_paths(root: &Path, since: &str) -> Vec<String> {
    let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--name-only", since])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
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
        "change-plan".to_string()
    } else {
        slug.chars().take(80).collect::<String>()
    }
}

fn write_change_plan_html(
    path: &Path,
    change: &str,
    features: &[PlannedFeature],
    warnings: &[String],
    provenance: &[Breadcrumb],
) -> Result<()> {
    let data = json!({
        "change": change,
        "feature_count": features.len(),
        "features": features,
        "provenance": provenance,
        "warnings": warnings,
    });
    let json = serde_json::to_string(&data)?;
    fs::write(
        path,
        PLAN_HTML
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

const PLAN_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Change Plan</title>
<style>
__THEME__
/* ===== change-plan components (light editorial) ===== */
header.plan{background:var(--bg-sky-soft);border-bottom:var(--border-hairline)}
header.plan .wrap{padding:48px 32px 36px}
header.plan .eyebrow{font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.16em;color:var(--color-blue-700);margin-bottom:16px;display:flex;align-items:center;gap:10px}
header.plan .eyebrow::before{content:"";width:22px;height:1px;background:var(--color-blue-500);display:inline-block}
header.plan h1{font:var(--type-display-lg);letter-spacing:-.01em;color:var(--color-ink-700);margin:0 0 10px}
#change{font:var(--type-body-lg);color:var(--color-ink-500);line-height:1.5}
.sub{color:var(--color-ink-400);max-width:72ch;margin-top:14px;font:var(--type-body-sm);line-height:1.6}
.sub b{color:var(--color-ink-600);font-weight:500}
main{padding:40px 0 64px;display:grid;gap:24px}
.panel{background:var(--color-surface-0);border:var(--border-hairline);border-radius:var(--radius-lg);box-shadow:var(--shadow-sm);padding:24px}
h2{font:var(--type-h4);color:var(--color-ink-700);margin:0 0 16px}
.muted{color:var(--fg-tertiary);line-height:1.5}
.stats{display:grid;grid-template-columns:repeat(auto-fit,minmax(160px,1fr));gap:16px;margin-bottom:2px}
.stat{border:var(--border-hairline);border-radius:var(--radius-md);background:var(--color-surface-2);padding:18px}
.stat b{display:block;font:var(--type-h2);font-family:var(--font-display);color:var(--color-ink-700);line-height:1}
.stat span{display:block;color:var(--fg-tertiary);font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.08em;margin-top:8px}
.feature{border:var(--border-hairline);border-radius:var(--radius-lg);background:var(--color-surface-0);padding:20px 22px;margin-top:14px;position:relative;box-shadow:var(--shadow-xs)}
.feature::before{content:"";position:absolute;left:0;top:0;bottom:0;width:3px;border-radius:var(--radius-lg) 0 0 var(--radius-lg);background:var(--color-blue-400)}
.feature h3{margin:0 0 4px;font:var(--type-h5);color:var(--color-ink-700);display:flex;align-items:center;flex-wrap:wrap;gap:8px}
.order{display:inline-flex;align-items:center;justify-content:center;width:28px;height:28px;border-radius:var(--radius-md);background:var(--color-ink-600);color:#fff;font:var(--type-overline-sm);font-family:var(--font-mono);font-weight:500}
.ring{position:absolute;top:18px;right:18px;font:var(--type-body-xs);font-family:var(--font-mono);font-weight:500;padding:4px 11px;border-radius:var(--radius-pill);border:var(--border-hairline);background:var(--color-surface-1)}
.ring.hi{color:#007f76;border-color:rgba(0,200,187,.4);background:rgba(0,200,187,.1)}
.ring.mid{color:var(--color-blue-700);border-color:var(--color-blue-300);background:var(--color-blue-50)}
.ring.lo{color:var(--fg-tertiary)}
.via{display:inline-flex;align-items:center;border-radius:var(--radius-pill);padding:3px 10px;font:var(--type-overline-sm);font-family:var(--font-mono);text-transform:uppercase;letter-spacing:.06em}
.via{margin-right:6px}
.via.semantic{color:var(--color-blue-700);background:var(--color-blue-100)}
.via.diff{color:var(--color-purple-500);background:var(--color-purple-100)}
.via.manifest{color:rgb(176,124,15);background:rgba(176,124,15,.12)}
.via.both{color:#007f76;background:rgba(0,200,187,.12)}
.matched{margin-top:10px;display:grid;gap:4px}
.matched div{color:var(--color-ink-500);font:var(--type-body-xs);line-height:1.5}
.matched b{color:var(--color-ink-700);font-weight:500;font-family:var(--font-mono);text-transform:uppercase;letter-spacing:.04em;font-size:10px}
.summary{color:var(--color-ink-500);font:var(--type-body-sm);line-height:1.6;margin:10px 0;white-space:pre-wrap;max-height:130px;overflow:auto}
.sym{display:inline-flex;align-items:center;gap:6px;border:var(--border-hairline);border-radius:var(--radius-pill);padding:4px 11px;margin:6px 6px 0 0;color:var(--color-ink-500);font:500 12px/1 var(--font-body);background:var(--color-surface-1)}
.sym .k{color:var(--fg-tertiary);font-family:var(--font-mono);font-size:10px;text-transform:uppercase;letter-spacing:.04em}
.item.warn{border:1px solid var(--color-blue-300);border-radius:var(--radius-md);background:var(--color-blue-50);padding:14px 16px;margin-top:12px}
.item.warn strong{color:var(--color-blue-700);font:var(--type-h6);display:block;margin-bottom:4px}
.item.warn div{color:var(--color-ink-500);font:var(--type-body-sm);line-height:1.5}
</style>
</head>
<body data-chaos-change-plan>
<div class="topbar"><div class="wrap">__BRAND_TOPBAR__<span class="crumb">Change plan<span class="sep">&rsaquo;</span><b>features</b></span><span class="sp"></span><span class="pilltag">Change plan</span></div></div>

<header class="plan">
  <div class="wrap">
    <div class="eyebrow">Change plan</div>
    <h1>Change Plan</h1>
    <div id="change"></div>
    <div class="sub">The features this change spans, in a suggested <b>check order</b> (dependencies first, derived from the feature quotient graph). Each feature is a community of related symbols with a confidence and how it was surfaced (semantic match, the actual diff, or both).</div>
  </div>
</header>

<main>
  <div class="wrap">
    <section class="panel"><div id="stats" class="stats"></div></section>
    <section class="panel" data-plan-features><h2>Features to check, in order</h2><div id="features"></div></section>
    <section class="panel" data-plan-provenance><h2>How this was generated</h2><div class="muted" style="margin-bottom:10px">Provenance breadcrumbs &mdash; the steps that produced this decomposition.</div><div id="provenance"></div></section>
    <section class="panel"><h2>Warnings</h2><div id="warnings"></div></section>
  </div>
</main>

<footer><div class="wrap">__BRAND_FOOTER__<span class="sp"></span><span class="meta">generated by Chaos Substrate</span></div></footer>

<script type="application/json" id="chaos-plan-data">__DATA__</script>
<script>
(function(){
var D=JSON.parse(document.getElementById("chaos-plan-data").textContent);
function esc(v){return String(v==null?"":v).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;").replace(/'/g,"&#039;");}
document.getElementById("change").textContent=D.change||"";
var F=D.features||[];
var avg=F.length?Math.round(100*F.reduce(function(a,f){return a+(f.confidence||0);},0)/F.length):0;
var stat=[[F.length,"features spanned"],[avg+"%","avg confidence"],[(D.warnings||[]).length,"warnings"]];
document.getElementById("stats").innerHTML=stat.map(function(s){return '<div class="stat"><b>'+s[0]+'</b><span>'+s[1]+'</span></div>';}).join("");
var host=document.getElementById("features");
F.forEach(function(f){
  var conf=Math.round((f.confidence||0)*100);
  var cls=conf>=75?"hi":conf>=45?"mid":"lo";
  var el=document.createElement("div");el.className="feature";
  var syms=(f.top_symbols||[]).map(function(s){return '<span class="sym">'+esc(s.name)+' <span class="k">'+esc(s.kind)+'</span></span>';}).join("");
  var via=(f.via||"").split("+").filter(Boolean).map(function(v){return '<span class="via '+esc(v)+'">'+esc(v)+'</span>';}).join("");
  var matched=(f.matched_by||[]).map(function(c){return '<div><b>'+esc(c.source)+'</b> '+esc(c.detail)+'</div>';}).join("");
  el.innerHTML='<div class="ring '+cls+'">'+conf+'%</div>'+
    '<h3><span class="order">'+(f.check_order||"?")+'</span>'+esc(f.label)+via+'</h3>'+
    '<div class="muted">'+f.member_count+' symbols</div>'+
    (f.summary?'<div class="summary">'+esc(f.summary)+'</div>':'')+
    '<div>'+syms+'</div>'+
    (matched?'<div class="matched">'+matched+'</div>':'');
  host.appendChild(el);
});
if(!host.children.length)host.innerHTML='<div class="muted">No features matched this change. Ensure the repo is indexed (chaos_analyze) so communities + summaries exist, and try a more specific description.</div>';
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

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn topo_sort_orders_dependencies_first() {
        // A depends on B, B depends on C ⇒ check C, then B, then A.
        let a = id(1);
        let b = id(2);
        let c = id(3);
        let priority = vec![a, b, c];
        let links = vec![(a, b, 1), (b, c, 1)];
        let order = topo_sort(&priority, &links);
        assert_eq!(order, vec![c, b, a]);
    }

    #[test]
    fn topo_sort_is_deterministic_and_total_on_cycles() {
        // A <-> B cycle plus independent C; must return all three, stably.
        let a = id(1);
        let b = id(2);
        let c = id(3);
        let order1 = topo_sort(&[a, b, c], &[(a, b, 1), (b, a, 1)]);
        let order2 = topo_sort(&[a, b, c], &[(a, b, 1), (b, a, 1)]);
        assert_eq!(order1, order2);
        assert_eq!(order1.len(), 3);
        let s: HashSet<Uuid> = order1.iter().copied().collect();
        assert!(s.contains(&a) && s.contains(&b) && s.contains(&c));
    }

    #[test]
    fn topo_sort_no_links_follows_priority() {
        let a = id(10);
        let b = id(20);
        let c = id(30);
        // priority order b, a, c with no links ⇒ same order.
        assert_eq!(topo_sort(&[b, a, c], &[]), vec![b, a, c]);
    }
}
