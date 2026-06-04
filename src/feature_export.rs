use crate::{
    export_util::escape_script_json,
    feature_context::{read_feature_manifest, FeatureContextNode, FeatureManifest},
    graph_export::GraphExport,
    hierarchy_export::{write_hierarchy, CommunityHierarchy},
    obsidian_export::{write_obsidian_vault, ObsidianSummary},
};
use anyhow::Result;
use std::{
    fs,
    path::{Path, PathBuf},
};

pub struct RefreshSummary {
    pub obsidian: ObsidianSummary,
    pub feature_pages: Vec<PathBuf>,
    pub skipped_feature_pages: Vec<PathBuf>,
    /// Number of god-node (community) notes written into the vault.
    pub community_notes: usize,
    /// Path of the navigable feature-map HTML, if the hierarchy was present.
    pub feature_map_html: Option<PathBuf>,
}

pub fn refresh_project_exports(
    graph: &GraphExport,
    obsidian_output: &Path,
    features_dir: &Path,
    all_features: bool,
    repo_root: &Path,
    hierarchy: Option<&CommunityHierarchy>,
) -> Result<RefreshSummary> {
    let obsidian = write_obsidian_vault(obsidian_output, graph)?;

    // L1/L3 hierarchy views (god-node notes + feature map) — regenerated from
    // the persisted layers, no re-index and no embedder.
    let (community_notes, feature_map_html) = match hierarchy {
        Some(h) if !h.is_empty() => {
            let summary = write_hierarchy(obsidian_output, features_dir, h)?;
            (summary.community_notes, summary.feature_map_html)
        }
        _ => (0, None),
    };

    let mut feature_pages = Vec::new();
    let mut skipped_feature_pages = Vec::new();
    if all_features && features_dir.exists() {
        let mut pages: Vec<PathBuf> = fs::read_dir(features_dir)?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("html"))
            .collect();
        pages.sort();
        for path in pages {
            match refresh_feature_page(repo_root, &path) {
                Ok(true) => feature_pages.push(path),
                Ok(false) => skipped_feature_pages.push(path),
                Err(err) => {
                    eprintln!("skip feature page {}: {err}", path.display());
                    skipped_feature_pages.push(path);
                }
            }
        }
    }

    Ok(RefreshSummary {
        obsidian,
        feature_pages,
        skipped_feature_pages,
        community_notes,
        feature_map_html,
    })
}

/// Re-render a single feature page from its embedded manifest, refreshing each
/// node's source snippet from the current repository. Returns `Ok(false)` when
/// the page has no `chaos-feature-manifest` block (nothing to refresh).
pub(crate) fn refresh_feature_page(repo_root: &Path, path: &Path) -> Result<bool> {
    // Bespoke LLM-composed explanation narratives (written by the
    // chaos_write_feature_website path as `*-explanation.html`) are not
    // deterministic manifest renders and must never be overwritten by refresh.
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with("-explanation.html"))
    {
        return Ok(false);
    }
    let Some(mut manifest) = read_feature_manifest(path)? else {
        return Ok(false);
    };
    for node in manifest.nodes.iter_mut() {
        if let Some(code) = refresh_node_code(repo_root, node) {
            node.code = code;
        }
    }
    fs::write(path, render_feature_website(&manifest)?)?;
    Ok(true)
}

/// Read the `node.file` source range named by `node.lines` from the current
/// repository and format it with a line-number gutter. `None` when the file or
/// range cannot be read (the existing snippet is then kept).
fn refresh_node_code(repo_root: &Path, node: &FeatureContextNode) -> Option<String> {
    let (start, end) = parse_line_range(&node.lines)?;
    let content = fs::read_to_string(repo_root.join(&node.file)).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    if start == 0 || start > lines.len() {
        return None;
    }
    let end = end.min(lines.len());
    let mut out = String::new();
    for (offset, line) in lines[start - 1..end].iter().enumerate() {
        out.push_str(&format!("{:>4}  {}\n", start + offset, line));
    }
    Some(out)
}

/// Parse a `"start-end"` (or single `"line"`) range, 1-based and inclusive.
fn parse_line_range(spec: &str) -> Option<(usize, usize)> {
    let spec = spec.trim();
    if let Some((a, b)) = spec.split_once('-') {
        let a: usize = a.trim().parse().ok()?;
        let b: usize = b.trim().parse().ok()?;
        if a == 0 || b < a {
            return None;
        }
        Some((a, b))
    } else {
        let a: usize = spec.parse().ok()?;
        if a == 0 {
            None
        } else {
            Some((a, a))
        }
    }
}

/// Render an interactive feature-map website from a manifest. The output
/// satisfies the feature-website contract (interactive graph/story/code surface
/// with an embedded `chaos-feature-manifest`).
pub(crate) fn render_feature_website(manifest: &FeatureManifest) -> Result<String> {
    let manifest_json = serde_json::to_string(manifest)?;
    Ok(FEATURE_HTML
        .replace("__TITLE__", &html_escape(&manifest.title))
        .replace("__SUBTITLE__", &html_escape(&manifest.subtitle))
        .replace("__FEATURE_ID__", &html_escape(&manifest.feature.id))
        .replace("__MANIFEST__", &escape_script_json(&manifest_json)))
}

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

const FEATURE_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__TITLE__</title>
<style>
:root{--bg:#07080d;--panel:#10131d;--ink:#f5f7fb;--muted:#8d9ab8;--line:#293047;--cyan:#32e6ff;--pink:#ff3d9a;--amber:#ffb000;--green:#3cff98;--red:#ff5a4e;--violet:#9f6bff}
*{box-sizing:border-box}body{margin:0;background:radial-gradient(circle at 18% 0%,rgba(50,230,255,.18),transparent 28%),radial-gradient(circle at 84% 14%,rgba(255,61,154,.16),transparent 24%),linear-gradient(180deg,#090a12,#05060a);color:var(--ink);font-family:Inter,ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif}
header{padding:30px 34px 20px;border-bottom:1px solid var(--line);background:linear-gradient(90deg,rgba(16,19,29,.92),rgba(16,19,29,.72))}
h1{margin:0 0 8px;font-size:clamp(28px,4vw,48px);text-shadow:0 0 28px rgba(50,230,255,.28)}a{color:var(--cyan);font-weight:800;text-decoration:none}.subtitle{max-width:1100px;color:var(--muted);line-height:1.55}
.layout{display:grid;grid-template-columns:minmax(440px,1.3fr) minmax(360px,.7fr);gap:18px;padding:18px}
.panel,.sidebox{background:linear-gradient(180deg,rgba(21,25,39,.96),rgba(12,14,22,.96));border:1px solid var(--line);border-radius:8px}
.toolbar{display:flex;justify-content:space-between;gap:12px;padding:14px 16px;border-bottom:1px solid var(--line)}button{border:1px solid var(--line);background:#0b0e16;color:var(--ink);border-radius:7px;padding:8px 10px;font-weight:750;cursor:pointer;font-family:inherit}button:hover,button.active{border-color:var(--cyan);color:var(--cyan)}
.legend{display:flex;flex-wrap:wrap;gap:8px 12px;color:var(--muted);font-size:13px}.legend span{display:inline-flex;gap:6px;align-items:center}.dot{width:10px;height:10px;border-radius:50%;display:inline-block}
svg{width:100%;min-height:690px;display:block;background:linear-gradient(rgba(50,230,255,.04) 1px,transparent 1px),linear-gradient(90deg,rgba(255,61,154,.04) 1px,transparent 1px);background-size:30px 30px}
.edge{stroke:#59627a;stroke-width:2;fill:none;marker-end:url(#arrow);opacity:.86}.edge.active{stroke:var(--cyan);stroke-width:3.5}.edge.dim{opacity:.16}.edge-label{font-size:11px;fill:#b7c1dd;paint-order:stroke;stroke:#07080d;stroke-width:4}
.node rect{stroke-width:2;rx:8;cursor:pointer}.node text{pointer-events:none;font-size:13px;fill:var(--ink);font-weight:800}.node .small{font-size:11px;fill:var(--muted);font-weight:700}.node.active rect{stroke:#fff;stroke-width:3}.node.dim{opacity:.26}
.right{display:grid;gap:18px;align-content:start}.sidebox{padding:16px}.sidebox h2{margin:0 0 10px;font-size:18px}.steps,.claims,.modes,.flow{display:grid;gap:8px}.steps{padding:0;margin:0;list-style:none}.steps button,.modes button{text-align:left;line-height:1.35;background:#0b0e16;width:100%}.claim{padding:10px;border:1px solid var(--line);border-radius:7px;background:#0b0e16;cursor:pointer}.claim strong{color:var(--cyan)}.claim small{display:block;color:var(--muted);margin-top:6px}
.inspector{padding:0;overflow:hidden}.head{padding:16px;border-bottom:1px solid var(--line)}.badge{display:inline-flex;border-radius:999px;color:#05060a;padding:5px 9px;font-size:12px;font-weight:900;margin-bottom:10px;background:var(--cyan)}.meta{color:var(--muted);font-size:13px;line-height:1.45;overflow-wrap:anywhere}.body{padding:15px 16px 16px;overflow:auto;max-height:610px}.explain{line-height:1.55}.section{color:var(--muted);font-size:12px;font-weight:900;text-transform:uppercase;letter-spacing:.05em;margin-top:14px}.relation{padding:9px 10px;border:1px solid var(--line);border-radius:7px;background:#0b0e16;font-size:13px;margin-top:8px;cursor:pointer}.relation strong{color:var(--cyan)}
pre{margin:10px 0 0;padding:14px;border-radius:8px;background:#030409;color:#d8e2ff;overflow:auto;font-size:12px;line-height:1.48;border:1px solid #242a3d}
@media(max-width:1050px){.layout{grid-template-columns:1fr}svg{min-height:600px}}
</style>
</head>
<body>
<div data-chaos-feature-website data-feature-id="__FEATURE_ID__">
<header><h1>__TITLE__</h1><div class="subtitle">__SUBTITLE__</div></header>
<main class="layout">
<section class="panel"><div class="toolbar"><div class="legend"><span><i class="dot" style="background:var(--cyan)"></i>api/read</span><span><i class="dot" style="background:var(--green)"></i>ui/crypto</span><span><i class="dot" style="background:var(--amber)"></i>infra</span><span><i class="dot" style="background:var(--red)"></i>backend</span><span><i class="dot" style="background:var(--violet)"></i>data</span></div><button id="resetBtn" type="button">Reset</button></div>
<svg data-chaos-graph id="graph" viewBox="0 0 1180 760"><defs><marker id="arrow" markerWidth="10" markerHeight="10" refX="9" refY="3" orient="auto" markerUnits="strokeWidth"><path d="M0,0 L0,6 L9,3 z" fill="#59627a"></path></marker></defs></svg></section>
<aside class="right">
<section class="sidebox" data-chaos-evidence><h2>Claims &amp; evidence</h2><div id="claims" class="claims"></div></section>
<section class="sidebox" data-chaos-architecture><h2>Architecture &amp; modes</h2><div id="modes" class="modes"></div></section>
<section class="sidebox" data-chaos-flow><h2>Flow</h2><div id="flow" class="flow"></div></section>
<section class="sidebox" data-chaos-story><h2>Story</h2><ol id="steps" class="steps"></ol></section>
<section class="sidebox inspector"><div class="head"><span id="badge" class="badge">node</span><h2 id="title">Select a node</h2><div id="meta" class="meta"></div></div>
<div class="body"><div id="role" class="explain"></div><div class="section">Evidence</div><div id="nodeEvidence" class="meta"></div><div class="section">Relations</div><div id="relations"></div><div class="section">Source</div><div data-chaos-code><pre><code id="code"></code></pre></div></div></section>
</aside>
</main>
</div>
<script type="application/json" id="chaos-feature-manifest">
__MANIFEST__
</script>
<script>
(function(){
var M=JSON.parse(document.getElementById("chaos-feature-manifest").textContent);
var NODES=M.nodes||[],EDGES=M.edges||[],CLAIMS=M.claims||[],MODES=M.modes||[],STORY=M.story||[];
var colors={ui:"#3cff98",crypto:"#3cff98",frontend:"#3cff98",api:"#32e6ff",read:"#32e6ff",backend:"#ff5a4e",infra:"#ffb000",data:"#9f6bff"};
var NS="http://www.w3.org/2000/svg";
var graph=document.querySelector("[data-chaos-graph]");
var byId={};
NODES.forEach(function(n,i){byId[n.id]=Object.assign({},n,{x:70+(i%5)*220,y:70+Math.floor(i/5)*165,w:170,h:74});});
var active=null;
function esc(v){return String(v==null?"":v).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;").replace(/'/g,"&#039;");}
function center(n){return {x:n.x+n.w/2,y:n.y+n.h/2};}
function edgePath(a,b){var ac=center(a),bc=center(b),dx=Math.max(45,Math.abs(bc.x-ac.x)*0.38);return "M "+ac.x+" "+ac.y+" C "+(ac.x+dx)+" "+ac.y+", "+(bc.x-dx)+" "+bc.y+", "+bc.x+" "+bc.y;}
function drawGraph(){
var defs=graph.querySelector("defs");graph.textContent="";if(defs)graph.appendChild(defs);
EDGES.forEach(function(e){var a=byId[e.source],b=byId[e.target];if(!a||!b)return;
var p=document.createElementNS(NS,"path");p.setAttribute("class","edge");p.setAttribute("d",edgePath(a,b));p.setAttribute("data-source",e.source);p.setAttribute("data-target",e.target);graph.appendChild(p);
var ac=center(a),bc=center(b),t=document.createElementNS(NS,"text");t.setAttribute("class","edge-label");t.setAttribute("x",(ac.x+bc.x)/2);t.setAttribute("y",(ac.y+bc.y)/2-8);t.setAttribute("text-anchor","middle");t.textContent=e.label;graph.appendChild(t);});
Object.keys(byId).forEach(function(id){var n=byId[id];
var g=document.createElementNS(NS,"g");g.setAttribute("class","node");g.setAttribute("data-node-id",n.id);g.setAttribute("tabindex","0");
var r=document.createElementNS(NS,"rect");r.setAttribute("x",n.x);r.setAttribute("y",n.y);r.setAttribute("width",n.w);r.setAttribute("height",n.h);r.setAttribute("fill","#10131d");r.setAttribute("stroke",colors[n.group]||"#32e6ff");g.appendChild(r);
var band=document.createElementNS(NS,"rect");band.setAttribute("x",n.x);band.setAttribute("y",n.y);band.setAttribute("width",8);band.setAttribute("height",n.h);band.setAttribute("rx",7);band.setAttribute("fill",colors[n.group]||"#32e6ff");g.appendChild(band);
var t1=document.createElementNS(NS,"text");t1.setAttribute("x",n.x+18);t1.setAttribute("y",n.y+31);t1.textContent=n.label;g.appendChild(t1);
var t2=document.createElementNS(NS,"text");t2.setAttribute("x",n.x+18);t2.setAttribute("y",n.y+53);t2.setAttribute("class","small");t2.textContent=n.subtitle||"";g.appendChild(t2);
g.addEventListener("click",function(){select(n.id);});
g.addEventListener("keydown",function(ev){if(ev.key==="Enter"||ev.key===" "){ev.preventDefault();select(n.id);}});
graph.appendChild(g);});}
function relations(id){var rows=[];EDGES.forEach(function(e){if(e.source===id&&byId[e.target])rows.push(["To",byId[e.target],e.label]);if(e.target===id&&byId[e.source])rows.push(["From",byId[e.source],e.label]);});
if(!rows.length)return '<div class="meta">No direct relations.</div>';
return rows.map(function(r){return '<div class="relation" data-target="'+esc(r[1].id)+'"><strong>'+r[0]+" "+esc(r[1].label)+"</strong><br>"+esc(r[2])+"</div>";}).join("");}
function select(id,focus){if(!byId[id])return;active=id;var n=byId[id];
document.getElementById("title").textContent=n.label;
document.getElementById("meta").textContent=n.file+" | lines "+n.lines;
var b=document.getElementById("badge");b.textContent=n.group;b.style.background=colors[n.group]||"#32e6ff";
document.getElementById("role").textContent=n.role||"";
var ev=n.evidence||{};document.getElementById("nodeEvidence").textContent=(ev.method||"curated")+" | "+(ev.source||"feature-map")+" | confidence "+Math.round((n.confidence||0)*100)+"%";
document.getElementById("relations").innerHTML=relations(id);
document.getElementById("code").textContent=n.code||"";
document.querySelectorAll("#relations .relation").forEach(function(el){el.addEventListener("click",function(){select(el.getAttribute("data-target"));});});
update(focus||new Set([id]));}
function update(focus){var f=new Set(focus||[active]);
document.querySelectorAll(".node").forEach(function(g){var on=active&&f.has(g.getAttribute("data-node-id"));g.classList.toggle("active",!!on);g.classList.toggle("dim",!!(active&&!on));});
document.querySelectorAll(".edge").forEach(function(e){var on=f.has(e.getAttribute("data-source"))&&f.has(e.getAttribute("data-target"));e.classList.toggle("active",!!on);e.classList.toggle("dim",!!(active&&!on));});}
function buildEvidence(){var root=document.getElementById("claims");CLAIMS.forEach(function(c){var el=document.createElement("div");el.className="claim";el.tabIndex=0;el.setAttribute("data-claim-id",c.id||"");
el.innerHTML="<strong>"+esc(c.title)+"</strong><br>"+esc(c.body)+"<small>confidence "+Math.round((c.confidence||0)*100)+"%</small>";
el.addEventListener("click",function(){var ids=c.node_ids||[];if(ids.length){active=ids[0];select(ids[0],new Set(ids));}});root.appendChild(el);});}
function buildModes(){var root=document.getElementById("modes");MODES.forEach(function(m){var b=document.createElement("button");b.type="button";b.setAttribute("data-mode-id",m.id||"");b.textContent=m.title;
b.addEventListener("click",function(){var ids=m.node_ids||[];if(ids.length){active=ids[0];select(ids[0],new Set(ids));}});root.appendChild(b);});}
function buildFlow(){var root=document.getElementById("flow");EDGES.forEach(function(e){var el=document.createElement("div");el.className="relation";el.setAttribute("data-target",e.target);
el.innerHTML="<strong>"+esc((byId[e.source]||{}).label||e.source)+"</strong> &rarr; <strong>"+esc((byId[e.target]||{}).label||e.target)+"</strong><br>"+esc(e.label)+" &middot; "+esc(e.kind||"");
el.addEventListener("click",function(){select(e.target,new Set([e.source,e.target]));});root.appendChild(el);});}
function storyIds(step,i){var ids=(step.node_ids||[]).filter(function(id){return byId[id];});if(ids.length)return ids;var k=Object.keys(byId);return k.length?[k[Math.min(i,k.length-1)]]:[];}
function buildStory(){var root=document.getElementById("steps");STORY.forEach(function(s,i){var li=document.createElement("li");var btn=document.createElement("button");btn.type="button";btn.setAttribute("data-story-step",String(i));btn.innerHTML="<strong>"+(i+1)+".</strong> "+esc(s.title);
btn.addEventListener("click",function(){var ids=storyIds(s,i);if(ids.length)select(ids[0],new Set(ids));document.querySelectorAll("[data-story-step]").forEach(function(x){x.classList.toggle("active",x===btn);});});li.appendChild(btn);root.appendChild(li);});}
drawGraph();buildEvidence();buildModes();buildFlow();buildStory();
var resetBtn=document.getElementById("resetBtn");if(resetBtn)resetBtn.addEventListener("click",function(){var k=Object.keys(byId);if(k.length)select(k[0]);});
var keys=Object.keys(byId);if(keys.length)select(keys[0]);
})();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature_context::{
        FeatureClaim, FeatureContextEdge, FeatureContextNode, FeatureDefinition, FeatureManifest,
        FeatureMode, FeatureStoryStep,
    };

    fn node(id: &str, file: &str, lines: &str, code: &str) -> FeatureContextNode {
        FeatureContextNode {
            id: id.into(),
            label: id.into(),
            subtitle: "sub".into(),
            group: "backend".into(),
            file: file.into(),
            lines: lines.into(),
            role: "role".into(),
            code: code.into(),
            evidence: Default::default(),
            confidence: 0.9,
        }
    }

    fn edge(source: &str, target: &str) -> FeatureContextEdge {
        FeatureContextEdge {
            source: source.into(),
            target: target.into(),
            label: "rel".into(),
            kind: "call-flow".into(),
            evidence: Default::default(),
            confidence: 0.86,
        }
    }

    fn sample_manifest() -> FeatureManifest {
        FeatureManifest {
            schema_version: "1".into(),
            feature: FeatureDefinition {
                id: "sample".into(),
                title: "Sample".into(),
                domain: "security".into(),
                summary: "summary".into(),
            },
            title: "Sample Feature Map".into(),
            subtitle: "subtitle".into(),
            claims: vec![
                FeatureClaim {
                    id: "c1".into(),
                    title: "Claim 1".into(),
                    body: "b".into(),
                    confidence: 0.9,
                    node_ids: vec!["a".into()],
                },
                FeatureClaim {
                    id: "c2".into(),
                    title: "Claim 2".into(),
                    body: "b".into(),
                    confidence: 0.9,
                    node_ids: vec!["b".into()],
                },
                FeatureClaim {
                    id: "c3".into(),
                    title: "Claim 3".into(),
                    body: "b".into(),
                    confidence: 0.9,
                    node_ids: vec!["c".into()],
                },
            ],
            modes: vec![
                FeatureMode {
                    id: "m1".into(),
                    title: "Mode 1".into(),
                    node_ids: vec!["a".into(), "b".into()],
                },
                FeatureMode {
                    id: "m2".into(),
                    title: "Mode 2".into(),
                    node_ids: vec!["c".into(), "d".into()],
                },
            ],
            nodes: vec![
                node("a", "src/a.ts", "1-1", "code a"),
                node("b", "src/b.ts", "1-1", "code b"),
                node("c", "src/c.ts", "1-1", "code c"),
                node("d", "src/d.ts", "1-1", "code d"),
                node("e", "src/e.ts", "1-1", "code e"),
            ],
            edges: vec![edge("a", "b"), edge("b", "c"), edge("c", "d")],
            story: vec![
                FeatureStoryStep {
                    id: "s1".into(),
                    title: "Step 1".into(),
                    node_ids: vec!["a".into()],
                    ..Default::default()
                },
                FeatureStoryStep {
                    id: "s2".into(),
                    title: "Step 2".into(),
                    node_ids: vec!["b".into()],
                    ..Default::default()
                },
                FeatureStoryStep {
                    id: "s3".into(),
                    title: "Step 3".into(),
                    node_ids: vec!["c".into()],
                    ..Default::default()
                },
            ],
        }
    }

    #[test]
    fn rendered_feature_website_satisfies_contract() {
        let manifest = sample_manifest();
        let html = render_feature_website(&manifest).unwrap();
        let value = serde_json::to_value(&manifest).unwrap();
        // Must satisfy the same contract the LLM write path enforces.
        crate::mcp::validate_feature_website_contract(&html, &value).unwrap();
        // And it must embed the manifest so it can be refreshed again later.
        assert!(html.contains(r#"id="chaos-feature-manifest""#));
        let parsed = crate::feature_context::read_feature_manifest_from_html(&html)
            .unwrap()
            .expect("rendered page exposes a manifest");
        assert_eq!(parsed.nodes.len(), manifest.nodes.len());
    }

    #[test]
    fn refresh_feature_page_rewrites_node_code_from_current_source() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(
            repo.join("src/lib.ts"),
            "alpha\nbeta\nfn target() { return 7 }\ndelta\n",
        )
        .unwrap();

        let features = repo.join("docs/features_memory");
        fs::create_dir_all(&features).unwrap();

        let mut manifest = sample_manifest();
        manifest.nodes[0] = node("target", "src/lib.ts", "3-3", "STALE PLACEHOLDER");
        let page_path = features.join("sample-feature-map.html");
        fs::write(&page_path, render_feature_website(&manifest).unwrap()).unwrap();

        let refreshed = refresh_feature_page(repo, &page_path).unwrap();
        assert!(refreshed, "page with a manifest should be refreshed");

        let after = read_feature_manifest(&page_path).unwrap().unwrap();
        let target = after.nodes.iter().find(|n| n.id == "target").unwrap();
        assert!(
            target.code.contains("fn target() { return 7 }"),
            "code refreshed from source, got: {}",
            target.code
        );
        assert!(!target.code.contains("STALE PLACEHOLDER"));
    }

    #[test]
    fn refresh_feature_page_without_manifest_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.html");
        fs::write(&path, "<html><body>no manifest here</body></html>").unwrap();
        assert!(!refresh_feature_page(dir.path(), &path).unwrap());
    }

    #[test]
    fn refresh_leaves_llm_explanation_pages_untouched() {
        // `*-explanation.html` pages are bespoke LLM narratives (written by the
        // chaos_write_feature_website path), not deterministic map renders, so
        // refresh must never overwrite them even though they carry a manifest.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth-and-rbac-explanation.html");
        fs::write(&path, render_feature_website(&sample_manifest()).unwrap()).unwrap();
        let before = fs::read_to_string(&path).unwrap();
        assert!(!refresh_feature_page(dir.path(), &path).unwrap());
        assert_eq!(before, fs::read_to_string(&path).unwrap());
    }
}
