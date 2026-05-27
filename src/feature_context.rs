use crate::query::QueryResponse;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
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
    pub feature_matches: Vec<FeatureMatch>,
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
        let Some(manifest) = read_feature_manifest(&path)? else {
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

pub fn write_feature_context_html(path: &Path, response: &FeatureContextResponse) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(response)?;
    fs::write(
        path,
        CONTEXT_HTML.replace("__CONTEXT__", &escape_script_json(&json)),
    )?;
    Ok(())
}

fn read_feature_manifest(path: &Path) -> Result<Option<FeatureManifest>> {
    let html = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let Some(start) = html.find(MANIFEST_START) else {
        return Ok(None);
    };
    let json_start = start + MANIFEST_START.len();
    let Some(end) = html[json_start..].find(MANIFEST_END) else {
        return Ok(None);
    };
    let raw = &html[json_start..json_start + end];
    let manifest = serde_json::from_str(raw.trim())
        .with_context(|| format!("parsing feature manifest from {}", path.display()))?;
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
    }
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

fn escape_script_json(json: &str) -> String {
    json.replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
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
:root{--bg:#07080d;--panel:#10131d;--panel2:#151927;--ink:#f5f7fb;--muted:#8d9ab8;--line:#293047;--cyan:#32e6ff;--pink:#ff3d9a;--amber:#ffb000;--green:#3cff98;--red:#ff5a4e}
*{box-sizing:border-box}body{margin:0;background:radial-gradient(circle at 16% 0%,rgba(50,230,255,.18),transparent 28%),radial-gradient(circle at 82% 10%,rgba(255,61,154,.16),transparent 24%),linear-gradient(180deg,#090a12,#05060a);color:var(--ink);font-family:Inter,ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}
header{padding:30px 34px 22px;border-bottom:1px solid var(--line);background:linear-gradient(90deg,rgba(16,19,29,.94),rgba(16,19,29,.72));box-shadow:0 20px 70px rgba(0,0,0,.45)}h1{margin:0 0 8px;font-size:clamp(30px,4vw,54px);letter-spacing:0;text-shadow:0 0 28px rgba(50,230,255,.28)}.muted{color:var(--muted)}
main{padding:18px;display:grid;gap:18px}.grid{display:grid;grid-template-columns:minmax(360px,.8fr) minmax(520px,1.2fr);gap:18px}.panel{background:linear-gradient(180deg,rgba(21,25,39,.96),rgba(12,14,22,.96));border:1px solid var(--line);border-radius:8px;box-shadow:0 22px 80px rgba(0,0,0,.45),0 0 34px rgba(50,230,255,.08);padding:16px}h2{margin:0 0 12px;font-size:18px}.item{border:1px solid var(--line);border-radius:7px;background:#0b0e16;padding:12px;margin-top:10px}.item strong{color:var(--cyan)}.item.doc{border-color:rgba(255,176,0,.42);box-shadow:0 0 22px rgba(255,176,0,.08)}.item.doc strong{color:var(--amber)}.claim strong{color:var(--amber)}.meta{font-size:13px;color:var(--muted);line-height:1.45;overflow-wrap:anywhere}.pill{display:inline-block;border:1px solid var(--line);border-radius:999px;padding:3px 8px;margin:4px 5px 0 0;color:var(--cyan);font-size:12px}.tag{display:inline-flex;border:1px solid var(--line);border-radius:999px;padding:2px 7px;margin-right:6px;font-size:11px;font-weight:800;color:var(--green);text-transform:uppercase}.tag.doc{color:var(--amber)}pre{margin:10px 0 0;padding:14px;border-radius:8px;background:#030409;color:#d8e2ff;overflow:auto;font-size:12px;line-height:1.48;border:1px solid #242a3d;max-height:360px}
@media(max-width:1000px){.grid{grid-template-columns:1fr}header{padding:24px 18px}main{padding:10px}}
</style>
</head>
<body>
<header><h1>Chaos Feature Context</h1><div id="task" class="muted"></div></header>
<main>
<section class="grid">
<div class="panel"><h2>Feature Matches</h2><div id="features"></div></div>
<div class="panel"><h2>Matched Source</h2><div id="nodes"></div></div>
</section>
<section class="panel"><h2>Documentation Evidence</h2><div id="docs"></div></section>
<section class="panel"><h2>Postgres Retrieval</h2><div id="hits"></div></section>
</main>
<script>
const data=__CONTEXT__;
function esc(v){return String(v??"").replaceAll("&","&amp;").replaceAll("<","&lt;").replaceAll(">","&gt;").replaceAll('"',"&quot;").replaceAll("'","&#039;")}
function isDoc(h){return h?.metadata?.source_priority==="supplemental"||h?.metadata?.kind==="documentation"}
function sourceTag(h){return isDoc(h)?'<span class="tag doc">docs</span>':'<span class="tag">code</span>'}
document.getElementById("task").textContent=data.task;
const features=document.getElementById("features");
if(!data.feature_matches.length){features.innerHTML='<div class="meta">No generated feature manifests matched. Use Postgres hits below as starting context.</div>'}
data.feature_matches.forEach(f=>{const el=document.createElement("div");el.className="item";el.innerHTML=`<strong>${esc(f.feature?.title||f.title)}</strong><div class="meta">${esc(f.feature?.domain)} | score ${f.score} | ${esc(f.page)}</div><div>${(f.modes||[]).map(m=>`<span class="pill">${esc(m.title)}</span>`).join("")}</div><h2 style="margin-top:14px">Claims</h2>${(f.claims||[]).map(c=>`<div class="item claim"><strong>${esc(c.title)}</strong><div>${esc(c.body)}</div><div class="meta">confidence ${Math.round((c.confidence||0)*100)}%</div></div>`).join("")}`;features.appendChild(el)});
const nodes=document.getElementById("nodes");
data.feature_matches.flatMap(f=>f.matched_nodes||[]).forEach(n=>{const el=document.createElement("div");el.className="item";el.innerHTML=`<strong>${esc(n.label)}</strong><div>${esc(n.role)}</div><div class="meta">${esc(n.file)} | lines ${esc(n.lines)} | confidence ${Math.round((n.confidence||0)*100)}%</div><pre><code>${esc(n.code)}</code></pre>`;nodes.appendChild(el)});
if(!nodes.children.length){nodes.innerHTML='<div class="meta">No feature-manifest nodes matched.</div>'}
const docs=document.getElementById("docs");
(data.postgres?.hits||[]).filter(isDoc).forEach(h=>{const el=document.createElement("div");el.className="item doc";el.innerHTML=`<strong>${esc(h.file_path||"documentation")}</strong><div class="meta">${sourceTag(h)}lines ${esc(h.line_start)}-${esc(h.line_end)} | score ${(h.score||0).toFixed(3)}</div><pre><code>${esc(h.content)}</code></pre>`;docs.appendChild(el)});
if(!docs.children.length){docs.innerHTML='<div class="meta">No matching docs were returned for this query. Re-index after adding Markdown/MDX docs, or raise --limit if the task is very code-specific.</div>'}
const hits=document.getElementById("hits");
(data.postgres?.hits||[]).forEach(h=>{const el=document.createElement("div");el.className=`item ${isDoc(h)?"doc":""}`;el.innerHTML=`<strong>${esc(h.file_path||"unknown file")}</strong><div class="meta">${sourceTag(h)}lines ${esc(h.line_start)}-${esc(h.line_end)} | score ${(h.score||0).toFixed(3)}</div><pre><code>${esc(h.content)}</code></pre>`;hits.appendChild(el)});
</script>
</body>
</html>"##;

#[cfg(test)]
mod tests {
    use super::{tokenize, FeatureManifest};
    use serde_json::json;

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
}
