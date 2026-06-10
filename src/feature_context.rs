use crate::provenance::{source, Breadcrumb};
use crate::query::QueryResponse;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
};

const MANIFEST_START: &str = r#"<script type="application/json" id="chaos-feature-manifest">"#;
const MANIFEST_END: &str = "</script>";

#[derive(Debug, Serialize)]
pub struct FeatureContextResponse {
    pub task: String,
    pub postgres: QueryResponse,
    pub features_dir: PathBuf,
    pub warnings: Vec<String>,
    pub feature_matches: Vec<FeatureMatch>,
    /// Breadcrumbs recording how this evidence was gathered (retrieval pipeline,
    /// manifests scanned). See [`feature_context_provenance`].
    #[serde(default)]
    pub provenance: Vec<Breadcrumb>,
}

#[derive(Debug, Serialize)]
pub struct FeatureMatch {
    pub page: PathBuf,
    pub feature: FeatureDefinition,
    pub title: String,
    pub subtitle: String,
    pub score: usize,
    pub claims: Vec<FeatureClaim>,
    pub modes: Vec<FeatureMode>,
    pub story: Vec<FeatureStoryStep>,
    pub matched_nodes: Vec<FeatureContextNode>,
    pub related_edges: Vec<FeatureContextEdge>,
    /// How this prior page matched the task, plus the page's own generation
    /// breadcrumbs (carried through from its embedded manifest).
    #[serde(default)]
    pub provenance: Vec<Breadcrumb>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeatureManifest {
    #[serde(default = "default_schema_version")]
    pub schema_version: String,
    #[serde(default)]
    pub feature: FeatureDefinition,
    pub title: String,
    pub subtitle: String,
    #[serde(default)]
    pub claims: Vec<FeatureClaim>,
    #[serde(default)]
    pub modes: Vec<FeatureMode>,
    pub nodes: Vec<FeatureContextNode>,
    pub edges: Vec<FeatureContextEdge>,
    #[serde(default, deserialize_with = "deserialize_story_steps")]
    pub story: Vec<FeatureStoryStep>,
    /// Artifact-level breadcrumbs: how this page was generated (git diff,
    /// Postgres queries, file reads, AST/regex extraction, correlated manifests).
    /// Backward-compatible — older pages simply have none.
    #[serde(default)]
    pub provenance: Vec<Breadcrumb>,
    /// Previously generated feature pages this page correlates with (shared
    /// files/symbols), so a reader sees the related existing features.
    #[serde(default)]
    pub related_features: Vec<FeatureCorrelation>,
}

/// A previously generated feature page that overlaps the current change/feature
/// by shared files or symbols — the "this correlates with an existing feature"
/// signal. Produced by [`correlate_feature_manifests`].
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FeatureCorrelation {
    /// File name of the prior page (it lives under the same features directory).
    pub page: String,
    pub feature_id: String,
    pub title: String,
    pub domain: String,
    /// Files shared between this change/feature and the prior page.
    pub shared_files: Vec<String>,
    /// Symbols (node labels) shared with the prior page.
    pub shared_symbols: Vec<String>,
    /// Overlap strength: `shared_files * 2 + shared_symbols`.
    pub score: usize,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FeatureDefinition {
    pub id: String,
    pub title: String,
    pub domain: String,
    pub summary: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeatureClaim {
    pub id: String,
    pub title: String,
    pub body: String,
    pub confidence: f32,
    pub node_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeatureMode {
    pub id: String,
    pub title: String,
    pub node_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FeatureStoryStep {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub node_ids: Vec<String>,
    #[serde(default)]
    pub edge_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeatureContextNode {
    pub id: String,
    pub label: String,
    pub subtitle: String,
    pub group: String,
    pub file: String,
    pub lines: String,
    pub role: String,
    pub code: String,
    #[serde(default)]
    pub evidence: FeatureEvidence,
    #[serde(default)]
    pub confidence: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeatureContextEdge {
    pub source: String,
    pub target: String,
    pub label: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub evidence: FeatureEvidence,
    #[serde(default)]
    pub confidence: f32,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FeatureEvidence {
    pub source: String,
    pub method: String,
    pub notes: String,
}

fn default_schema_version() -> String {
    "legacy".to_string()
}

/// Cap on a matched node's code excerpt in TOOL RETURNS (full code stays in the
/// generated HTML pages and, of course, in the repo itself).
const MAX_RETURN_NODE_CODE_CHARS: usize = 600;

/// Trim chunk contents and node code for a TOOL RETURN. Call AFTER any HTML
/// write — generated pages keep the full evidence; the agent's context gets
/// excerpts plus pointers.
pub fn cap_response_for_return(response: &mut FeatureContextResponse) {
    crate::query::cap_hits_for_return(&mut response.postgres.hits);
    for feature in &mut response.feature_matches {
        for node in &mut feature.matched_nodes {
            node.code = crate::query::truncate_for_return(&node.code, MAX_RETURN_NODE_CODE_CHARS);
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FeatureStoryStepInput {
    Text(String),
    Step(FeatureStoryStep),
}

fn deserialize_story_steps<'de, D>(deserializer: D) -> Result<Vec<FeatureStoryStep>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let inputs = Vec::<FeatureStoryStepInput>::deserialize(deserializer)?;
    Ok(inputs
        .into_iter()
        .enumerate()
        .map(|(idx, input)| match input {
            FeatureStoryStepInput::Text(title) => FeatureStoryStep {
                id: format!("step-{}", idx + 1),
                title,
                ..FeatureStoryStep::default()
            },
            FeatureStoryStepInput::Step(step) => step,
        })
        .collect())
}

pub fn load_feature_matches(
    task: &str,
    features_dir: &Path,
    feature_limit: usize,
    nodes_per_feature: usize,
) -> Result<Vec<FeatureMatch>> {
    if !features_dir.exists() {
        return Ok(Vec::new());
    }

    let tokens = tokenize(task);
    let mut matches = Vec::new();
    for entry in fs::read_dir(features_dir)
        .with_context(|| format!("reading feature directory {}", features_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("html") {
            continue;
        }
        let Some(manifest) = read_feature_manifest(&path).unwrap_or(None) else {
            continue;
        };
        let scored = score_manifest(path, manifest, &tokens, nodes_per_feature);
        if scored.score > 0 {
            matches.push(scored);
        }
    }

    matches.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
    matches.truncate(feature_limit);
    Ok(matches)
}

pub fn build_feature_context_warnings(
    task: &str,
    repo_root: &Path,
    postgres: &QueryResponse,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for token in tokenize(task) {
        if token.len() < 4 || !repo_root.join(&token).exists() {
            continue;
        }
        let token_in_hits = postgres.hits.iter().any(|hit| {
            hit.file_path
                .as_deref()
                .is_some_and(|path| path.to_ascii_lowercase().contains(&token))
        });
        if !token_in_hits {
            warnings.push(format!(
                "filesystem path `{token}` exists under the repo, but no Postgres hits referenced it; the index may be stale or the feature context limit is too low"
            ));
        }
    }

    if repo_root.join("docs").exists() && !postgres.hits.iter().any(is_documentation_hit) {
        warnings.push(
            "repo has a docs directory, but this feature-context result contains no documentation hits; generated websites should re-query with stronger docs terms or re-index if docs were added recently"
                .to_string(),
        );
    }

    warnings
}

fn is_documentation_hit(hit: &crate::models::SearchHit) -> bool {
    hit.metadata
        .get("source_priority")
        .and_then(|v| v.as_str())
        .is_some_and(|priority| priority == "supplemental")
        || hit
            .metadata
            .get("kind")
            .and_then(|v| v.as_str())
            .is_some_and(|kind| kind == "documentation")
}

pub fn write_feature_context_html(path: &Path, response: &FeatureContextResponse) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(response)?;
    fs::write(
        path,
        CONTEXT_HTML
            .replace("__THEME__", crate::theme::THEME_CSS)
            .replace(
                "__BRAND_TOPBAR__",
                &crate::theme::render_brand(&crate::theme::Brand::default(), "topbar"),
            )
            .replace(
                "__BRAND_FOOTER__",
                &crate::theme::render_brand(&crate::theme::Brand::default(), "footer"),
            )
            .replace(
                "__CONTEXT__",
                &crate::export_util::escape_script_json(&json),
            ),
    )?;
    Ok(())
}

pub(crate) fn read_feature_manifest(path: &Path) -> Result<Option<FeatureManifest>> {
    let html = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    read_feature_manifest_from_html(&html)
        .with_context(|| format!("parsing feature manifest from {}", path.display()))
}

pub(crate) fn read_feature_manifest_from_html(html: &str) -> Result<Option<FeatureManifest>> {
    let Some(start) = html.find(MANIFEST_START) else {
        return Ok(None);
    };
    let json_start = start + MANIFEST_START.len();
    let Some(end) = html[json_start..].find(MANIFEST_END) else {
        return Ok(None);
    };
    let raw = &html[json_start..json_start + end];
    let manifest = serde_json::from_str(raw.trim())?;
    Ok(Some(manifest))
}

fn score_manifest(
    page: PathBuf,
    manifest: FeatureManifest,
    tokens: &[String],
    nodes_per_feature: usize,
) -> FeatureMatch {
    let claims_text = manifest
        .claims
        .iter()
        .map(|claim| format!("{} {}", claim.title, claim.body))
        .collect::<Vec<_>>()
        .join(" ");
    let modes_text = manifest
        .modes
        .iter()
        .map(|mode| mode.title.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let mut score = score_text(
        tokens,
        &[
            manifest.feature.id.as_str(),
            manifest.feature.title.as_str(),
            manifest.feature.domain.as_str(),
            manifest.feature.summary.as_str(),
            manifest.title.as_str(),
            manifest.subtitle.as_str(),
            &claims_text,
            &modes_text,
            &manifest
                .story
                .iter()
                .map(|step| format!("{} {} {}", step.title, step.body, step.node_ids.join(" ")))
                .collect::<Vec<_>>()
                .join(" "),
        ],
    ) * 3;

    let mut node_scores = manifest
        .nodes
        .iter()
        .map(|node| {
            let node_score = score_text(
                tokens,
                &[
                    node.label.as_str(),
                    node.subtitle.as_str(),
                    node.group.as_str(),
                    node.file.as_str(),
                    node.role.as_str(),
                    node.evidence.notes.as_str(),
                    node.code.as_str(),
                ],
            );
            (node_score, node)
        })
        .collect::<Vec<_>>();
    score += node_scores
        .iter()
        .map(|(node_score, _)| node_score)
        .sum::<usize>();
    node_scores.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)));

    let matched_nodes = node_scores
        .into_iter()
        .filter(|(node_score, _)| *node_score > 0)
        .take(nodes_per_feature)
        .map(|(_, node)| node.clone())
        .collect::<Vec<_>>();
    let selected_ids = matched_nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<HashSet<_>>();
    let related_edges = manifest
        .edges
        .iter()
        .filter(|edge| {
            selected_ids.contains(edge.source.as_str())
                || selected_ids.contains(edge.target.as_str())
        })
        .cloned()
        .collect();

    // Lead with a breadcrumb explaining the match, then carry through the page's
    // own generation breadcrumbs so the reader can audit both why it surfaced
    // and how it was originally built.
    let mut provenance = vec![Breadcrumb::new(
        source::MANIFEST,
        "score_manifest",
        format!("matched generated page by {score} shared token hit(s)"),
    )
    .with_locator(page.display().to_string())];
    provenance.extend(manifest.provenance);

    FeatureMatch {
        page,
        feature: manifest.feature,
        title: manifest.title,
        subtitle: manifest.subtitle,
        score,
        claims: manifest.claims,
        modes: manifest.modes,
        story: manifest.story,
        matched_nodes,
        related_edges,
        provenance,
    }
}

/// Scan `features_dir` for previously generated feature pages whose manifests
/// overlap the given `files` or `symbols`, so a *new* feature extraction can see
/// the existing features it correlates with. Overlap is computed on the prior
/// pages' manifest node files (strong signal) and node labels (secondary).
///
/// `exclude_slug` is the `feature.id` of the page currently being (re)written, so
/// a page never correlates with its own prior version. Results are sorted by
/// overlap strength and truncated to `limit`. A missing directory or empty
/// inputs yield an empty list (never an error).
pub fn correlate_feature_manifests(
    features_dir: &Path,
    files: &HashSet<String>,
    symbols: &HashSet<String>,
    exclude_slug: &str,
    limit: usize,
) -> Result<Vec<FeatureCorrelation>> {
    if !features_dir.exists() || (files.is_empty() && symbols.is_empty()) {
        return Ok(Vec::new());
    }
    let want_files: HashSet<String> = files.iter().map(|f| f.to_ascii_lowercase()).collect();
    let want_symbols: HashSet<String> = symbols.iter().map(|s| s.to_ascii_lowercase()).collect();

    let mut out: Vec<FeatureCorrelation> = Vec::new();
    for entry in fs::read_dir(features_dir)
        .with_context(|| format!("reading feature directory {}", features_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("html") {
            continue;
        }
        let Some(manifest) = read_feature_manifest(&path).unwrap_or(None) else {
            continue;
        };
        if !exclude_slug.is_empty() && manifest.feature.id == exclude_slug {
            continue;
        }

        let mut page_files: BTreeSet<String> = BTreeSet::new();
        let mut page_symbols: BTreeSet<String> = BTreeSet::new();
        for node in &manifest.nodes {
            if !node.file.is_empty() {
                page_files.insert(node.file.clone());
            }
            if !node.label.is_empty() {
                page_symbols.insert(node.label.clone());
            }
        }
        let shared_files: Vec<String> = page_files
            .into_iter()
            .filter(|f| want_files.contains(&f.to_ascii_lowercase()))
            .collect();
        let shared_symbols: Vec<String> = page_symbols
            .into_iter()
            .filter(|s| want_symbols.contains(&s.to_ascii_lowercase()))
            .collect();
        let score = shared_files.len() * 2 + shared_symbols.len();
        if score == 0 {
            continue;
        }
        let title = if manifest.feature.title.is_empty() {
            manifest.title
        } else {
            manifest.feature.title
        };
        out.push(FeatureCorrelation {
            page: path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string(),
            feature_id: manifest.feature.id,
            title,
            domain: manifest.feature.domain,
            shared_files,
            shared_symbols,
            score,
        });
    }

    out.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
    out.truncate(limit);
    Ok(out)
}

/// Build the artifact-level breadcrumbs for a feature-context / impact response:
/// how the evidence was retrieved (the hybrid Postgres pipeline, with a
/// per-method hit breakdown) and how many prior manifests were scanned/matched.
pub fn feature_context_provenance(
    postgres: &QueryResponse,
    features_dir: &Path,
    feature_matches: &[FeatureMatch],
) -> Vec<Breadcrumb> {
    let (mut semantic, mut keyword, mut literal) = (0usize, 0usize, 0usize);
    for hit in &postgres.hits {
        if let Some(methods) = hit.metadata.get("retrieved_by").and_then(|v| v.as_array()) {
            for method in methods {
                match method.as_str() {
                    Some("semantic") => semantic += 1,
                    Some("keyword") => keyword += 1,
                    Some("literal") => literal += 1,
                    _ => {}
                }
            }
        }
    }
    vec![
        Breadcrumb::new(
            source::POSTGRES,
            "query_feature_context_repo",
            format!(
                "hybrid retrieval over pgvector chunks → {} hit(s) (semantic {semantic}, keyword {keyword}, literal {literal})",
                postgres.hits.len()
            ),
        ),
        Breadcrumb::new(
            source::MANIFEST,
            "load_feature_matches",
            format!(
                "scanned generated feature pages → {} correlated manifest(s)",
                feature_matches.len()
            ),
        )
        .with_locator(features_dir.display().to_string()),
    ]
}

fn tokenize(value: &str) -> Vec<String> {
    value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|token| token.len() > 2 && !STOP_WORDS.contains(&token.as_str()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

fn score_text(tokens: &[String], haystacks: &[&str]) -> usize {
    let haystack = haystacks.join(" ").to_ascii_lowercase();
    tokens
        .iter()
        .map(|token| haystack.matches(token).count())
        .sum()
}

const STOP_WORDS: &[&str] = &[
    "and",
    "are",
    "for",
    "from",
    "how",
    "into",
    "the",
    "this",
    "that",
    "then",
    "with",
    "would",
    "should",
    "could",
    "feature",
    "implement",
    "implementation",
    "store",
];

const CONTEXT_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Chaos Feature Context</title>
<style>
__THEME__
/* ===== feature-context components (light editorial) ===== */
main{padding:48px 0 0;display:block}
.grid{display:grid;grid-template-columns:minmax(360px,.8fr) minmax(520px,1.2fr);gap:24px;margin-bottom:24px}
.panel{background:var(--color-surface-0);border:var(--border-hairline);border-radius:var(--radius-lg);box-shadow:var(--shadow-sm);padding:24px;margin-bottom:24px}
.grid .panel{margin-bottom:0}
.panel>h2{font:var(--type-h5);color:var(--color-ink-700);margin:0 0 14px;letter-spacing:-.01em}
.muted{color:var(--fg-tertiary)}
.item{border:var(--border-hairline);border-radius:var(--radius-md);background:var(--color-surface-1);padding:16px;margin-top:12px}
.item:first-child{margin-top:0}
.item strong{font:var(--type-h6);font-weight:500;color:var(--color-blue-700)}
.item.doc{border-color:var(--color-blue-300);background:var(--color-blue-50)}
.item.doc strong{color:var(--color-blue-800)}
.item.claim strong{color:var(--color-ink-700)}
.item>h2{font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.1em;color:var(--fg-tertiary);margin:16px 0 4px}
.meta{font:var(--type-body-sm);color:var(--fg-tertiary);line-height:1.5;overflow-wrap:anywhere;margin-top:4px}
.pill{display:inline-block;border:var(--border-hairline);border-radius:var(--radius-pill);padding:4px 11px;margin:6px 6px 0 0;color:var(--color-blue-700);background:var(--color-blue-100);font:500 12px/1.3 var(--font-body)}
.tag{display:inline-flex;border:var(--border-hairline);border-radius:var(--radius-pill);padding:3px 9px;margin-right:8px;font:var(--type-overline-sm);font-family:var(--font-mono);font-weight:500;color:var(--color-ink-500);background:var(--color-surface-2);text-transform:uppercase;letter-spacing:.06em}
.tag.doc{color:var(--color-blue-700);background:var(--color-blue-100);border-color:var(--color-blue-300)}
pre{margin:12px 0 0;padding:14px;border-radius:var(--radius-md);background:var(--color-ink-900);color:var(--color-blue-100);overflow:auto;font:var(--type-body-xs);font-family:var(--font-mono);line-height:1.55;border:var(--border-hairline);max-height:360px}
@media(max-width:1000px){.grid{grid-template-columns:1fr}}
</style>
</head>
<body>
<div class="topbar"><div class="wrap">__BRAND_TOPBAR__<span class="crumb">Feature context<span class="sep">&rsaquo;</span><b>evidence</b></span><span class="sp"></span><span class="pilltag">Feature context</span></div></div>
<header class="hero">
  <div class="wrap">
    <div>
      <div class="eyebrow">Feature context</div>
      <h1>Feature evidence</h1>
      <p class="lede" id="task"></p>
    </div>
  </div>
</header>
<main>
<div class="wrap">
<section class="grid">
<div class="panel"><h2>Feature Matches</h2><div id="features"></div></div>
<div class="panel"><h2>Matched Source</h2><div id="nodes"></div></div>
</section>
<section class="panel"><h2>How this was generated</h2><div class="meta" style="margin-bottom:8px">Provenance breadcrumbs &mdash; where each piece of this evidence came from.</div><div id="provenance"></div></section>
<section class="panel"><h2>Warnings</h2><div id="warnings"></div></section>
<section class="panel"><h2>Documentation Evidence</h2><div id="docs"></div></section>
<section class="panel"><h2>Postgres Retrieval</h2><div id="hits"></div></section>
</div>
</main>
<footer><div class="wrap">__BRAND_FOOTER__<span class="sp"></span><span class="meta">generated by Chaos Substrate</span></div></footer>
<script>
const data=__CONTEXT__;
function esc(v){return String(v??"").replaceAll("&","&amp;").replaceAll("<","&lt;").replaceAll(">","&gt;").replaceAll('"',"&quot;").replaceAll("'","&#039;")}
function isDoc(h){return h?.metadata?.source_priority==="supplemental"||h?.metadata?.kind==="documentation"}
function sourceTag(h){return isDoc(h)?'<span class="tag doc">docs</span>':'<span class="tag">code</span>'}
function retrievedTags(h){return ((h?.metadata?.retrieved_by)||[]).map(m=>`<span class="tag">${esc(m)}</span>`).join("")}
function crumbList(list){if(!list||!list.length)return '<div class="meta">No breadcrumbs recorded.</div>';return list.map(c=>`<div class="item"><strong>${esc(c.source)}</strong> <span class="tag">${esc(c.method)}</span><div class="meta">${esc(c.detail)}${c.locator?' &middot; <code>'+esc(c.locator)+'</code>':''}</div></div>`).join("")}
document.getElementById("task").textContent=data.task;
document.getElementById("provenance").innerHTML=crumbList(data.provenance);
const features=document.getElementById("features");
if(!data.feature_matches.length){features.innerHTML='<div class="meta">No generated feature manifests matched. Use Postgres hits below as starting context.</div>'}
data.feature_matches.forEach(f=>{const el=document.createElement("div");el.className="item";el.innerHTML=`<strong>${esc(f.feature?.title||f.title)}</strong><div class="meta">${esc(f.feature?.domain)} | score ${f.score} | ${esc(f.page)}</div><div class="meta">${(f.provenance||[]).map(c=>`<span class="tag">${esc(c.source)}</span>`).join("")}</div><div>${(f.modes||[]).map(m=>`<span class="pill">${esc(m.title)}</span>`).join("")}</div><h2 style="margin-top:14px">Claims</h2>${(f.claims||[]).map(c=>`<div class="item claim"><strong>${esc(c.title)}</strong><div>${esc(c.body)}</div><div class="meta">confidence ${Math.round((c.confidence||0)*100)}%</div></div>`).join("")}`;features.appendChild(el)});
const nodes=document.getElementById("nodes");
data.feature_matches.flatMap(f=>f.matched_nodes||[]).forEach(n=>{const el=document.createElement("div");el.className="item";el.innerHTML=`<strong>${esc(n.label)}</strong><div>${esc(n.role)}</div><div class="meta">${esc(n.file)} | lines ${esc(n.lines)} | confidence ${Math.round((n.confidence||0)*100)}%</div><pre><code>${esc(n.code)}</code></pre>`;nodes.appendChild(el)});
if(!nodes.children.length){nodes.innerHTML='<div class="meta">No feature-manifest nodes matched.</div>'}
const warnings=document.getElementById("warnings");
(data.warnings||[]).forEach(w=>{const el=document.createElement("div");el.className="item doc";el.innerHTML=`<strong>Context warning</strong><div>${esc(w)}</div>`;warnings.appendChild(el)});
if(!warnings.children.length){warnings.innerHTML='<div class="meta">No stale-index or missing-doc warnings detected.</div>'}
const docs=document.getElementById("docs");
(data.postgres?.hits||[]).filter(isDoc).forEach(h=>{const el=document.createElement("div");el.className="item doc";el.innerHTML=`<strong>${esc(h.file_path||"documentation")}</strong><div class="meta">${sourceTag(h)}${retrievedTags(h)} lines ${esc(h.line_start)}-${esc(h.line_end)} | score ${(h.score||0).toFixed(3)}</div><pre><code>${esc(h.content)}</code></pre>`;docs.appendChild(el)});
if(!docs.children.length){docs.innerHTML='<div class="meta">No matching docs were returned for this query. Re-index after adding Markdown/MDX docs, or raise --limit if the task is very code-specific.</div>'}
const hits=document.getElementById("hits");
(data.postgres?.hits||[]).forEach(h=>{const el=document.createElement("div");el.className=`item ${isDoc(h)?"doc":""}`;el.innerHTML=`<strong>${esc(h.file_path||"unknown file")}</strong><div class="meta">${sourceTag(h)}${retrievedTags(h)} lines ${esc(h.line_start)}-${esc(h.line_end)} | score ${(h.score||0).toFixed(3)}</div><pre><code>${esc(h.content)}</code></pre>`;hits.appendChild(el)});
</script>
</body>
</html>"##;

#[cfg(test)]
mod tests {
    use super::{
        correlate_feature_manifests, load_feature_matches, tokenize, FeatureManifest,
        MANIFEST_START,
    };
    use serde_json::json;
    use std::collections::HashSet;
    use std::fs;

    #[test]
    fn tokenizes_task_text() {
        let tokens = tokenize("implement store icon in secure upload");
        assert!(tokens.contains(&"secure".to_string()));
        assert!(tokens.contains(&"icon".to_string()));
        assert!(!tokens.contains(&"store".to_string()));
    }

    #[test]
    fn accepts_legacy_string_story_steps() {
        let manifest: FeatureManifest = serde_json::from_value(json!({
            "title": "Feature Map",
            "subtitle": "Legacy story",
            "nodes": [],
            "edges": [],
            "story": ["Start upload"]
        }))
        .unwrap();

        assert_eq!(manifest.story[0].id, "step-1");
        assert_eq!(manifest.story[0].title, "Start upload");
        assert!(manifest.story[0].node_ids.is_empty());
    }

    #[test]
    fn accepts_scoped_story_steps() {
        let manifest: FeatureManifest = serde_json::from_value(json!({
            "title": "Feature Map",
            "subtitle": "Scoped story",
            "nodes": [],
            "edges": [],
            "story": [{
                "id": "request-key",
                "title": "Client asks backend/KMS for a DEK",
                "node_ids": ["uploader", "generate-dek", "kms-key"],
                "edge_ids": ["uploader->generate-dek"]
            }]
        }))
        .unwrap();

        assert_eq!(manifest.story[0].id, "request-key");
        assert_eq!(
            manifest.story[0].node_ids,
            vec!["uploader", "generate-dek", "kms-key"]
        );
        assert_eq!(manifest.story[0].edge_ids, vec!["uploader->generate-dek"]);
    }

    #[test]
    fn correlates_prior_manifest_by_shared_file_and_excludes_self() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = json!({
            "feature": {"id": "feature-auth", "title": "Auth", "domain": "feature", "summary": ""},
            "title": "Auth",
            "subtitle": "",
            "nodes": [{
                "id": "n1", "label": "login", "subtitle": "function", "group": "backend",
                "file": "src/auth.rs", "lines": "1-10", "role": "", "code": ""
            }],
            "edges": []
        });
        fs::write(
            dir.path().join("feature-auth.html"),
            format!("{MANIFEST_START}\n{manifest}\n</script>"),
        )
        .unwrap();

        let files: HashSet<String> = ["src/auth.rs".to_string()].into_iter().collect();
        let symbols: HashSet<String> = HashSet::new();

        let correlations =
            correlate_feature_manifests(dir.path(), &files, &symbols, "feature-new", 5).unwrap();
        assert_eq!(correlations.len(), 1);
        assert_eq!(correlations[0].feature_id, "feature-auth");
        assert_eq!(
            correlations[0].shared_files,
            vec!["src/auth.rs".to_string()]
        );
        assert_eq!(correlations[0].score, 2);

        // A page never correlates with its own prior version.
        let self_excluded =
            correlate_feature_manifests(dir.path(), &files, &symbols, "feature-auth", 5).unwrap();
        assert!(self_excluded.is_empty());
    }

    #[test]
    fn skips_malformed_feature_manifests() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("broken.html"),
            format!("{MANIFEST_START}\n{{\"nodes\":[]}}\n</script>"),
        )
        .unwrap();

        let matches = load_feature_matches("OCL", dir.path(), 3, 8).unwrap();

        assert!(matches.is_empty());
    }
}
