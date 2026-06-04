//! `chaos_change_plan` — decompose a change into the features it spans.
//!
//! This is the top-down primitive the hierarchical-memory thread was built for:
//! given a change *description* (and optionally a real git diff), match it
//! against the L1 community summary embeddings (P3) and/or the communities the
//! diff directly touches, then return the set of features the change spans —
//! each with its members, a dependency-aware **check order** (topo-sort over the
//! quotient graph), and a confidence.
//!
//! Like `chaos_impact`, it ALWAYS writes an interactive Blade-Runner HTML report
//! and returns a COMPACT JSON summary, so an agent calling it over MCP doesn't
//! get its context flooded.

use crate::{
    embedding::Embedder,
    export_util::escape_script_json,
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
    /// How it was surfaced: "semantic", "diff", or "both".
    pub via: String,
    /// 1-based position in the suggested check order.
    pub check_order: usize,
    pub top_symbols: Vec<PlannedSymbol>,
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

    // 3. Union semantic + diff into one scored set (feature communities only).
    let diff_briefs = storage
        .load_community_briefs(repo.id, &diff_ids.iter().copied().collect::<Vec<_>>())
        .await?;

    struct Acc {
        m: CommunityMatch,
        semantic: f64,
        via_diff: bool,
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
                m,
            });
    }

    // 4. Rank by confidence, cap to limit.
    let mut ranked: Vec<(Uuid, f64, String, CommunityMatch)> = by_id
        .into_values()
        .map(|a| {
            let confidence = if a.via_diff { 1.0 } else { a.semantic };
            let via = match (a.semantic >= MIN_SEMANTIC_SCORE, a.via_diff) {
                (true, true) => "both",
                (false, true) => "diff",
                _ => "semantic",
            };
            (a.m.id, confidence, via.to_string(), a.m)
        })
        .collect();
    ranked.sort_by(|x, y| {
        y.1.partial_cmp(&x.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(y.3.member_count.cmp(&x.3.member_count))
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
    for (id, confidence, via, m) in &ranked {
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
        });
    }
    features.sort_by(|a, b| a.check_order.cmp(&b.check_order));

    // 7. Always write the HTML report.
    let output = opts.output_html.clone().unwrap_or_else(|| {
        repo_root
            .join("docs/features_memory")
            .join(format!("{}-plan.html", safe_slug(change)))
    });
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    write_change_plan_html(&output, change, &features, &warnings)?;

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
) -> Result<()> {
    let data = json!({
        "change": change,
        "feature_count": features.len(),
        "features": features,
        "warnings": warnings,
    });
    let json = serde_json::to_string(&data)?;
    fs::write(
        path,
        PLAN_HTML.replace("__DATA__", &escape_script_json(&json)),
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
:root{--bg:#07080d;--panel:#10131d;--ink:#f5f7fb;--muted:#8d9ab8;--line:#293047;--cyan:#32e6ff;--pink:#ff3d9a;--amber:#ffb000;--green:#3cff98;--red:#ff5a4e}
*{box-sizing:border-box}body{margin:0;background:radial-gradient(circle at 16% 0%,rgba(50,230,255,.18),transparent 28%),radial-gradient(circle at 82% 10%,rgba(255,61,154,.16),transparent 24%),linear-gradient(180deg,#090a12,#05060a);color:var(--ink);font-family:Inter,ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif}
header{padding:30px 34px 22px;border-bottom:1px solid var(--line);background:linear-gradient(90deg,rgba(16,19,29,.94),rgba(16,19,29,.72))}
h1{margin:0 0 6px;font-size:clamp(28px,4vw,48px);text-shadow:0 0 28px rgba(50,230,255,.28)}
.muted{color:var(--muted);line-height:1.5}.sub{color:var(--muted);max-width:1100px;margin-top:8px;font-size:14px}
main{padding:18px;display:grid;gap:18px}
.panel{background:linear-gradient(180deg,rgba(21,25,39,.96),rgba(12,14,22,.96));border:1px solid var(--line);border-radius:8px;box-shadow:0 22px 80px rgba(0,0,0,.45);padding:16px}
h2{margin:0 0 12px;font-size:18px}
.stats{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:12px;margin-bottom:6px}
.stat{border:1px solid var(--line);border-radius:8px;background:#0b0e16;padding:12px}.stat b{display:block;font-size:26px}.stat span{color:var(--muted);font-size:12px}
.feature{border:1px solid var(--line);border-radius:8px;background:#0b0e16;padding:14px;margin-top:12px;position:relative}
.feature h3{margin:0 0 4px;font-size:16px;color:var(--cyan)}
.order{display:inline-flex;align-items:center;justify-content:center;width:26px;height:26px;border-radius:50%;background:var(--cyan);color:#05060a;font-weight:800;margin-right:8px;font-size:13px}
.ring{position:absolute;top:14px;right:14px;font-weight:800;font-size:13px;padding:3px 9px;border-radius:999px;border:1px solid var(--line)}
.ring.hi{color:var(--green);border-color:rgba(60,255,152,.5)}.ring.mid{color:var(--amber);border-color:rgba(255,176,0,.5)}.ring.lo{color:var(--muted)}
.via{display:inline-block;border-radius:999px;padding:2px 8px;font-size:11px;font-weight:800;text-transform:uppercase;margin-left:6px}
.via.semantic{color:#05060a;background:var(--cyan)}.via.diff{color:#05060a;background:var(--pink)}.via.both{color:#05060a;background:var(--green)}
.summary{color:var(--muted);font-size:13px;margin:8px 0;white-space:pre-wrap;max-height:120px;overflow:auto}
.sym{display:inline-block;border:1px solid var(--line);border-radius:999px;padding:3px 8px;margin:4px 5px 0 0;color:var(--ink);font-size:12px}
.sym .k{color:var(--muted);font-size:10px}
.item.warn{border:1px solid rgba(255,176,0,.45);border-radius:7px;background:#0b0e16;padding:12px;margin-top:10px}.item.warn strong{color:var(--amber)}
</style>
</head>
<body data-chaos-change-plan>
<header><h1>Change Plan</h1><div id="change" class="muted"></div>
<div class="sub">The features this change spans, in a suggested <b>check order</b> (dependencies first, derived from the feature quotient graph). Each feature is a community of related symbols with a confidence and how it was surfaced (semantic match, the actual diff, or both).</div></header>
<main>
<section class="panel"><div id="stats" class="stats"></div></section>
<section class="panel" data-plan-features><h2>Features to check, in order</h2><div id="features"></div></section>
<section class="panel"><h2>Warnings</h2><div id="warnings"></div></section>
</main>
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
  el.innerHTML='<div class="ring '+cls+'">'+conf+'%</div>'+
    '<h3><span class="order">'+(f.check_order||"?")+'</span>'+esc(f.label)+
    '<span class="via '+esc(f.via)+'">'+esc(f.via)+'</span></h3>'+
    '<div class="muted">'+f.member_count+' symbols</div>'+
    (f.summary?'<div class="summary">'+esc(f.summary)+'</div>':'')+
    '<div>'+syms+'</div>';
  host.appendChild(el);
});
if(!host.children.length)host.innerHTML='<div class="muted">No features matched this change. Ensure the repo is indexed (chaos_analyze) so communities + summaries exist, and try a more specific description.</div>';
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
