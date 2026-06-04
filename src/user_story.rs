//! `chaos storyboard` / `chaos_write_storyboard` — a client/user-facing feature
//! storyboard.
//!
//! Where `feature_export`/`impact` describe a feature for engineers (graphs,
//! files, symbols, source), this module renders the *same* feature for a client
//! or end user: a UI/UX **user story** with **no code**. The feature is broken
//! into clickable **frames** (click a frame → read its detail), the **user
//! stories** are spelled out ("As a … I want … so that …"), and every frame,
//! story, and outcome carries a **confidence** percentage plus an overall ring.
//!
//! The agent supplies only the structured, code-free [`StoryboardManifest`]; the
//! Rust side owns the rendering, so the dark Blade Runner styling and the
//! click-a-frame interactivity are guaranteed every time. The manifest is
//! embedded back into the page under the id `chaos-storyboard-manifest` for
//! agentic reads. That id is deliberately *different* from the feature-map
//! `chaos-feature-manifest`, so `refresh --all-features` and `load_feature_matches`
//! ignore storyboard pages (see the isolation test).

use crate::{export_util::escape_script_json, feature_context::FeatureDefinition};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

fn default_schema_version() -> String {
    "storyboard-1".to_string()
}

/// A code-free, user-facing decomposition of a feature. Composed by an agent and
/// rendered by [`render_storyboard_html`] into an interactive Blade Runner page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoryboardManifest {
    #[serde(default = "default_schema_version")]
    pub schema_version: String,
    #[serde(default)]
    pub feature: FeatureDefinition,
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
    /// Who this page is for, e.g. "Members uploading sensitive documents".
    #[serde(default)]
    pub audience: String,
    /// Overall confidence in this storyboard, 0.0–1.0.
    #[serde(default)]
    pub overall_confidence: f32,
    #[serde(default)]
    pub personas: Vec<Persona>,
    #[serde(default)]
    pub stories: Vec<UserStory>,
    #[serde(default)]
    pub frames: Vec<Frame>,
    #[serde(default)]
    pub outcomes: Vec<Outcome>,
}

/// A kind of user the feature serves.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Persona {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// What this persona is trying to achieve.
    #[serde(default)]
    pub goal: String,
}

/// A user story: "As a {persona}, I want {want}, so that {benefit}".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserStory {
    pub id: String,
    #[serde(default)]
    pub persona_id: String,
    pub want: String,
    #[serde(default)]
    pub benefit: String,
    /// Plain-language acceptance criteria (no Gherkin/code required).
    #[serde(default)]
    pub acceptance: Vec<String>,
    #[serde(default)]
    pub confidence: f32,
    /// Frames that realize this story (must reference real `frames[].id`).
    #[serde(default)]
    pub frame_ids: Vec<String>,
}

/// One clickable step/screen in the user's experience of the feature.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Frame {
    pub id: String,
    pub title: String,
    /// Phase grouping, e.g. "Discover" / "Act" / "Confirm". Lanes preserve the
    /// order stages are first seen.
    #[serde(default)]
    pub stage: String,
    /// One-line of what the user sees/does (shown on the card).
    #[serde(default)]
    pub summary: String,
    /// The longer, user-perspective explanation revealed when the frame is
    /// clicked. NO code.
    #[serde(default)]
    pub detail: String,
    /// Why this step matters to the user.
    #[serde(default)]
    pub user_value: String,
    /// Optional UI location hint, e.g. "Dashboard › Upload".
    #[serde(default)]
    pub ui_hint: String,
    #[serde(default)]
    pub confidence: f32,
    #[serde(default)]
    pub persona_ids: Vec<String>,
    /// Optional rendered preview of the real client UI for this frame — the
    /// actual experience, not code. Chaos only embeds it; the artifact is
    /// produced elsewhere (a screenshot the agent captured, or a route on the
    /// user's running dev server).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<FramePreview>,
}

/// An optional rendered preview of the real client UI involved in a frame.
///
/// Chaos is Rust-only and never runs a browser, so it cannot render an app
/// itself — it only embeds an artifact produced/served elsewhere. `Image` is a
/// snapshot the agent captured (offline, leaks nothing); `Iframe` is a live
/// embed of a running app route (interactive, but only while that server is up).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FramePreview {
    /// A captured screenshot or animated clip. `src` may be a relative path next
    /// to the page, an absolute path, or a URL.
    Image {
        src: String,
        #[serde(default)]
        alt: String,
        #[serde(default)]
        caption: String,
    },
    /// A live embed of a running app route (e.g. a local dev server). Sandboxed
    /// in the page; renders only while that server is reachable.
    Iframe {
        url: String,
        #[serde(default)]
        caption: String,
    },
}

/// A user-facing outcome / definition of success.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Outcome {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub confidence: f32,
}

/// Validate a storyboard before it is rendered. Enforces useful minimums, that
/// every confidence is a real number in `[0,1]`, and that story/frame references
/// resolve (so no click in the rendered page leads nowhere).
pub fn validate_storyboard(manifest: &StoryboardManifest) -> Result<()> {
    if manifest.title.trim().is_empty() {
        bail!("storyboard title must not be empty");
    }

    let minimums = [
        ("personas", manifest.personas.len(), 1usize),
        ("stories", manifest.stories.len(), 2),
        ("frames", manifest.frames.len(), 3),
        ("outcomes", manifest.outcomes.len(), 1),
    ];
    for (field, count, minimum) in minimums {
        if count < minimum {
            bail!(
                "storyboard.{field} must contain at least {minimum} item(s) for a useful client-facing page; got {count}"
            );
        }
    }

    check_confidence("overall", manifest.overall_confidence)?;
    for persona in &manifest.personas {
        if persona.id.trim().is_empty() {
            bail!("every persona needs a non-empty id");
        }
    }
    for frame in &manifest.frames {
        if frame.id.trim().is_empty() {
            bail!("every frame needs a non-empty id");
        }
        if frame.title.trim().is_empty() {
            bail!("frame `{}` needs a title", frame.id);
        }
        check_confidence(&format!("frame `{}`", frame.id), frame.confidence)?;
        if let Some(preview) = &frame.preview {
            check_preview(&frame.id, preview)?;
        }
    }
    for story in &manifest.stories {
        check_confidence(&format!("story `{}`", story.id), story.confidence)?;
    }
    for outcome in &manifest.outcomes {
        check_confidence(&format!("outcome `{}`", outcome.id), outcome.confidence)?;
    }

    let frame_ids: HashSet<&str> = manifest.frames.iter().map(|f| f.id.as_str()).collect();
    let persona_ids: HashSet<&str> = manifest.personas.iter().map(|p| p.id.as_str()).collect();
    for story in &manifest.stories {
        if !story.persona_id.is_empty() && !persona_ids.contains(story.persona_id.as_str()) {
            bail!(
                "story `{}` references unknown persona_id `{}`",
                story.id,
                story.persona_id
            );
        }
        for frame_id in &story.frame_ids {
            if !frame_ids.contains(frame_id.as_str()) {
                bail!(
                    "story `{}` references unknown frame_id `{}`",
                    story.id,
                    frame_id
                );
            }
        }
    }
    for frame in &manifest.frames {
        for persona_id in &frame.persona_ids {
            if !persona_ids.contains(persona_id.as_str()) {
                bail!(
                    "frame `{}` references unknown persona_id `{}`",
                    frame.id,
                    persona_id
                );
            }
        }
    }

    Ok(())
}

fn check_confidence(label: &str, value: f32) -> Result<()> {
    if !(0.0..=1.0).contains(&value) {
        bail!("{label} confidence must be between 0.0 and 1.0; got {value}");
    }
    Ok(())
}

fn check_preview(frame_id: &str, preview: &FramePreview) -> Result<()> {
    let (label, value) = match preview {
        FramePreview::Image { src, .. } => ("image src", src.as_str()),
        FramePreview::Iframe { url, .. } => ("iframe url", url.as_str()),
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("frame `{frame_id}` preview {label} must not be empty");
    }
    // Defense-in-depth: the value is rendered into an HTML attribute, so reject
    // active-content schemes even though the page is static.
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("javascript:")
        || lower.starts_with("vbscript:")
        || lower.starts_with("data:text/html")
    {
        bail!(
            "frame `{frame_id}` preview {label} uses a disallowed scheme; use http(s), a relative path, or a data:image URL"
        );
    }
    Ok(())
}

/// Render the interactive Blade Runner storyboard page from a manifest. The
/// manifest is embedded under `chaos-storyboard-manifest` for agentic reads.
pub fn render_storyboard_html(manifest: &StoryboardManifest) -> Result<String> {
    let json = serde_json::to_string(manifest)?;
    Ok(STORYBOARD_HTML
        .replace("__TITLE__", &html_escape(&manifest.title))
        .replace("__SUBTITLE__", &html_escape(&manifest.subtitle))
        .replace("__AUDIENCE__", &html_escape(&manifest.audience))
        .replace("__MANIFEST__", &escape_script_json(&json)))
}

/// Validate + render + write a storyboard to the default
/// `docs/features_memory/<slug>-story.html` path.
pub fn write_storyboard(
    repo_root: &Path,
    manifest: &StoryboardManifest,
    slug: &str,
    title: &str,
) -> Result<PathBuf> {
    let output = repo_root
        .join("docs/features_memory")
        .join(format!("{}-story.html", safe_slug(slug)));
    write_storyboard_to(&output, manifest, title)
}

/// Validate + render + write a storyboard to an explicit path. `title` fills in
/// the page/feature title only when the manifest omits one.
pub fn write_storyboard_to(
    output: &Path,
    manifest: &StoryboardManifest,
    title: &str,
) -> Result<PathBuf> {
    let mut manifest = manifest.clone();
    if manifest.title.trim().is_empty() {
        manifest.title = title.to_string();
    }
    if manifest.feature.title.trim().is_empty() {
        manifest.feature.title = manifest.title.clone();
    }
    validate_storyboard(&manifest)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output, render_storyboard_html(&manifest)?)?;
    Ok(output.to_path_buf())
}

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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
        "storyboard".to_string()
    } else {
        slug.chars().take(80).collect::<String>()
    }
}

const STORYBOARD_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__TITLE__ &middot; Storyboard</title>
<style>
:root{--bg:#05060c;--panel:#0c1018;--ink:#eaf2ff;--soft:#c6d3ef;--muted:#7d8aa8;--line:#1d2740;--cyan:#27e7ff;--pink:#ff2e88;--amber:#ffb01f;--green:#36ff9e;--violet:#9b6bff;--glow:0 0 22px rgba(39,231,255,.35);--mono:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}
*{box-sizing:border-box}
body{margin:0;color:var(--ink);font-family:Inter,ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif;background:radial-gradient(900px 520px at 12% -8%,rgba(39,231,255,.16),transparent 60%),radial-gradient(820px 540px at 92% 2%,rgba(255,46,136,.16),transparent 58%),radial-gradient(760px 620px at 50% 122%,rgba(155,107,255,.12),transparent 60%),linear-gradient(180deg,#05060c,#03040a);background-attachment:fixed;min-height:100vh}
.scan{position:fixed;inset:0;pointer-events:none;z-index:60;background:repeating-linear-gradient(180deg,rgba(0,0,0,0) 0 2px,rgba(0,0,0,.16) 2px 3px);mix-blend-mode:overlay;opacity:.55}
.scan:after{content:"";position:fixed;inset:0;background:radial-gradient(125% 95% at 50% 28%,transparent 55%,rgba(0,0,0,.55))}
.muted{color:var(--muted)}
header{position:relative;display:flex;justify-content:space-between;align-items:center;gap:26px;flex-wrap:wrap;padding:36px 40px 28px;border-bottom:1px solid var(--line);background:linear-gradient(90deg,rgba(10,13,21,.94),rgba(10,13,21,.5))}
.eyebrow{font-family:var(--mono);font-size:12px;letter-spacing:.34em;text-transform:uppercase;color:var(--cyan);opacity:.85;margin-bottom:12px}
h1{margin:0 0 10px;font-size:clamp(30px,4.4vw,56px);line-height:1.03;letter-spacing:.5px;text-shadow:0 0 34px rgba(39,231,255,.34),0 0 4px rgba(39,231,255,.4)}
.subtitle{max-width:780px;color:var(--soft);line-height:1.55;font-size:16px}
.audience{margin-top:12px;font-family:var(--mono);font-size:12.5px;color:var(--amber);letter-spacing:.04em}
.ring{width:130px;height:130px;border-radius:50%;display:grid;place-items:center;flex:0 0 auto;box-shadow:0 0 42px rgba(39,231,255,.18),inset 0 0 20px rgba(0,0,0,.6);position:relative;animation:pulse 4.5s ease-in-out infinite}
@keyframes pulse{50%{box-shadow:0 0 62px rgba(39,231,255,.34),inset 0 0 20px rgba(0,0,0,.6)}}
.ring-inner{width:98px;height:98px;border-radius:50%;background:radial-gradient(circle,#0a0d15,#070912);display:grid;place-items:center;border:1px solid var(--line);text-align:center}
.ring-inner b{font-size:29px;font-family:var(--mono);color:#fff;text-shadow:var(--glow)}
.ring-inner span{display:block;font-size:9.5px;letter-spacing:.22em;text-transform:uppercase;color:var(--muted);margin-top:2px}
main{padding:22px;display:grid;gap:20px;max-width:1340px;margin:0 auto}
h2{margin:0 0 14px;font-size:17px;letter-spacing:.02em;display:flex;align-items:baseline;gap:12px}
h2 .hint{font-family:var(--mono);font-size:11px;text-transform:uppercase;letter-spacing:.18em;color:var(--muted);font-weight:400}
.panel,.band{background:linear-gradient(180deg,rgba(14,18,28,.94),rgba(8,10,18,.94));border:1px solid var(--line);border-radius:12px;padding:18px;box-shadow:0 24px 80px rgba(0,0,0,.5)}
.personas{display:flex;flex-wrap:wrap;gap:12px}
.persona{flex:1 1 200px;min-width:190px;border:1px solid var(--line);border-left:3px solid var(--cyan);border-radius:10px;background:#0a0e17;padding:12px 14px;display:flex;flex-direction:column;gap:4px}
.persona b{color:var(--cyan);font-size:14px}
.persona span{color:var(--soft);font-size:13px}
.persona small{color:var(--muted);font-size:12px;line-height:1.45}
.grid{display:grid;grid-template-columns:minmax(0,1.55fr) minmax(300px,.95fr);gap:20px;align-items:start}
.lane{margin-bottom:18px}
.lane:last-child{margin-bottom:0}
.lane-label{font-family:var(--mono);font-size:11px;text-transform:uppercase;letter-spacing:.22em;color:var(--pink);margin:0 0 10px;display:flex;align-items:center;gap:10px}
.lane-label:before{content:"";width:8px;height:8px;border-radius:2px;background:var(--pink);box-shadow:0 0 10px var(--pink)}
.lane-row{display:grid;grid-template-columns:repeat(auto-fill,minmax(220px,1fr));gap:12px}
.frame{text-align:left;cursor:pointer;border:1px solid var(--line);border-radius:11px;background:linear-gradient(180deg,#0b0f19,#090c14);color:var(--ink);padding:13px 14px;display:flex;flex-direction:column;gap:8px;transition:transform .15s,border-color .15s,box-shadow .15s;position:relative;outline:none}
.frame:hover{border-color:var(--cyan);transform:translateY(-2px);box-shadow:0 10px 30px rgba(0,0,0,.5),0 0 22px rgba(39,231,255,.12)}
.frame:focus-visible{border-color:var(--cyan);box-shadow:0 0 0 2px rgba(39,231,255,.5)}
.frame.active{border-color:var(--cyan);box-shadow:0 0 0 1px var(--cyan),0 0 26px rgba(39,231,255,.26)}
.frame.linked{border-color:var(--amber);box-shadow:0 0 0 1px var(--amber),0 0 22px rgba(255,176,31,.2)}
.frame-top{display:flex;justify-content:space-between;align-items:center}
.frame-stage{font-family:var(--mono);font-size:9.5px;letter-spacing:.16em;text-transform:uppercase;color:var(--muted)}
.frame-pct{font-family:var(--mono);font-size:11px;color:var(--cyan)}
.frame h3{margin:0;font-size:15px;line-height:1.25}
.frame p{margin:0;color:#a9b6d6;font-size:12.5px;line-height:1.45}
.conf{display:flex;align-items:center;gap:8px;margin-top:8px}
.conf span{font-family:var(--mono);font-size:11px;color:var(--muted);min-width:34px;text-align:right}
.conf-track{flex:1;height:6px;border-radius:999px;background:rgba(120,140,180,.16);overflow:hidden}
.conf-track.sm{height:4px;margin-top:2px}
.conf-fill{height:100%;border-radius:999px;background:linear-gradient(90deg,var(--cyan),var(--green));box-shadow:0 0 10px rgba(39,231,255,.5)}
.detail{position:sticky;top:18px;min-height:280px}
.d-stage{font-family:var(--mono);font-size:10px;letter-spacing:.22em;text-transform:uppercase;color:var(--pink)}
.detail h2{margin:6px 0 10px;font-size:21px;display:block}
.d-ui{font-family:var(--mono);font-size:12px;color:var(--amber);margin-bottom:10px}
.d-sec{font-family:var(--mono);font-size:10.5px;letter-spacing:.18em;text-transform:uppercase;color:var(--muted);margin:16px 0 6px}
.detail p{margin:0;color:var(--soft);line-height:1.6;font-size:14px}
.chip{display:inline-block;border:1px solid var(--line);border-radius:999px;padding:3px 10px;margin:3px 5px 0 0;color:var(--cyan);font-size:12px;background:#0a0e17}
.rel{border-left:2px solid var(--violet);padding:6px 0 6px 10px;margin-top:8px;font-size:13px;color:var(--soft)}
.rel b{color:var(--violet)}
.story{border:1px solid var(--line);border-left:3px solid var(--green);border-radius:10px;background:#0a0e17;padding:14px 16px;margin-top:12px;cursor:pointer;transition:border-color .15s,box-shadow .15s;outline:none}
.story:first-of-type{margin-top:0}
.story:hover{border-color:var(--green);box-shadow:0 0 22px rgba(54,255,158,.12)}
.story:focus-visible{box-shadow:0 0 0 2px rgba(54,255,158,.5)}
.story-line{font-size:15px;line-height:1.55;color:#eaf2ff}
.kw{font-family:var(--mono);font-size:11px;letter-spacing:.1em;text-transform:uppercase;color:var(--green);padding:0 2px}
.acc{margin:10px 0 4px;padding-left:18px;color:#aebbd9;font-size:13px;line-height:1.6}
.outcome{border:1px solid var(--line);border-top:3px solid var(--amber);border-radius:10px;background:#0a0e17;padding:14px 16px;margin-top:12px}
.outcome:first-of-type{margin-top:0}
.outcome b{color:var(--amber);font-size:15px}
.outcome p{margin:6px 0 0;color:var(--soft);font-size:13.5px;line-height:1.55}
footer{padding:18px 4px 34px;text-align:center;color:var(--muted);font-family:var(--mono);font-size:11px;letter-spacing:.08em}
.preview-wrap{margin:12px 0 2px;border:1px solid var(--line);border-radius:10px;overflow:hidden;background:#04060c;position:relative;box-shadow:0 0 24px rgba(39,231,255,.08)}
.preview-wrap .tagline{position:absolute;top:8px;left:8px;z-index:2;font-family:var(--mono);font-size:9px;letter-spacing:.16em;text-transform:uppercase;color:#04060c;background:var(--cyan);border-radius:999px;padding:2px 8px;box-shadow:0 0 12px rgba(39,231,255,.5)}
.preview-wrap.live .tagline{background:var(--green);box-shadow:0 0 12px rgba(54,255,158,.5)}
img.preview{display:block;width:100%;height:auto;cursor:zoom-in}
iframe.preview-frame{display:block;width:100%;height:430px;border:0;background:#fff}
.preview-cap{display:flex;justify-content:space-between;align-items:center;gap:10px;padding:8px 11px;font-size:12px;color:var(--muted);border-top:1px solid var(--line);background:#080b13}
.preview-cap a{color:var(--cyan);font-family:var(--mono);font-size:11px;white-space:nowrap;text-decoration:none}
.frame-pct .pv{color:var(--violet)}
.lightbox{position:fixed;inset:0;z-index:90;display:none;align-items:center;justify-content:center;padding:28px;background:rgba(2,3,8,.93);cursor:zoom-out}
.lightbox.open{display:flex}
.lightbox img{max-width:96vw;max-height:92vh;border:1px solid var(--line);border-radius:8px;box-shadow:0 0 70px rgba(39,231,255,.28)}
@media(max-width:980px){.grid{grid-template-columns:1fr}.detail{position:static}header{padding:24px 18px}iframe.preview-frame{height:320px}}
</style>
</head>
<body data-chaos-storyboard>
<div class="scan"></div>
<div class="lightbox" id="lightbox" aria-hidden="true"><img id="lightboxImg" alt="preview"></div>
<header>
<div class="head-main">
<div class="eyebrow">// Chaos Substrate &middot; Feature Storyboard</div>
<h1>__TITLE__</h1>
<div class="subtitle">__SUBTITLE__</div>
<div class="audience" id="audience">__AUDIENCE__</div>
</div>
<div class="ring" id="ring" role="img" aria-label="overall confidence"><div class="ring-inner"><b id="ringPct">0%</b><span>confidence</span></div></div>
</header>
<main>
<section class="band" data-chaos-personas><h2>Who it's for</h2><div id="personas" class="personas"></div></section>
<section class="grid">
<div class="panel" data-chaos-frames><h2>Walkthrough <span class="hint">click a frame for detail</span></h2><div id="frames"></div></div>
<aside class="panel detail" data-chaos-detail><div id="detail"></div></aside>
</section>
<section class="panel" data-chaos-stories><h2>User stories <span class="hint">click to spotlight frames</span></h2><div id="stories"></div></section>
<section class="panel" data-chaos-outcomes><h2>What success looks like</h2><div id="outcomes"></div></section>
<footer>Confidence values are estimates &middot; user perspective, no code &middot; generated by Chaos Substrate</footer>
</main>
<script type="application/json" id="chaos-storyboard-manifest">__MANIFEST__</script>
<script>
(function(){
var M=JSON.parse(document.getElementById("chaos-storyboard-manifest").textContent);
var FRAMES=M.frames||[],STORIES=M.stories||[],PERSONAS=M.personas||[],OUTCOMES=M.outcomes||[];
var personaById={};PERSONAS.forEach(function(p){personaById[p.id]=p;});
var frameById={};FRAMES.forEach(function(f){frameById[f.id]=f;});
var active=null;
function esc(v){return String(v==null?"":v).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;").replace(/'/g,"&#039;");}
function pct(c){var n=Math.round((Number(c)||0)*100);return Math.max(0,Math.min(100,n));}
function conf(c){var p=pct(c);return '<div class="conf"><div class="conf-track"><div class="conf-fill" style="width:'+p+'%"></div></div><span>'+p+'%</span></div>';}
function personaName(id){var p=personaById[id];return p?p.name:(id||"Someone");}

var op=pct(M.overall_confidence);
document.getElementById("ring").style.background='conic-gradient(var(--cyan) '+(op*3.6)+'deg, rgba(120,140,180,.14) '+(op*3.6)+'deg)';
document.getElementById("ringPct").textContent=op+'%';
var aud=document.getElementById("audience");if(!aud.textContent.trim())aud.style.display="none";

var personaRoot=document.getElementById("personas");
PERSONAS.forEach(function(p){var el=document.createElement("div");el.className="persona";el.innerHTML='<b>'+esc(p.name)+'</b>'+(p.goal?'<span>'+esc(p.goal)+'</span>':"")+(p.description?'<small>'+esc(p.description)+'</small>':"");personaRoot.appendChild(el);});
if(!personaRoot.children.length)personaRoot.innerHTML='<div class="muted">No personas provided.</div>';

var frameRoot=document.getElementById("frames");
var stages=[],byStage={};
FRAMES.forEach(function(f){var s=(f.stage||"").trim()||"Flow";if(!byStage[s]){byStage[s]=[];stages.push(s);}byStage[s].push(f);});
stages.forEach(function(s){
var lane=document.createElement("div");lane.className="lane";lane.innerHTML='<div class="lane-label">'+esc(s)+'</div>';
var row=document.createElement("div");row.className="lane-row";
byStage[s].forEach(function(f){
var card=document.createElement("div");card.className="frame";card.setAttribute("role","button");card.setAttribute("tabindex","0");card.setAttribute("data-frame-id",f.id);
card.innerHTML='<div class="frame-top"><span class="frame-stage">'+esc(s)+'</span><span class="frame-pct">'+(f.preview?'<span class="pv">&#9656;</span> ':'')+pct(f.confidence)+'%</span></div><h3>'+esc(f.title)+'</h3><p>'+esc(f.summary||"")+'</p><div class="conf-track sm"><div class="conf-fill" style="width:'+pct(f.confidence)+'%"></div></div>';
card.addEventListener("click",function(){select(f.id);});
card.addEventListener("keydown",function(ev){if(ev.key==="Enter"||ev.key===" "){ev.preventDefault();select(f.id);}});
row.appendChild(card);
});
lane.appendChild(row);frameRoot.appendChild(lane);
});

function storiesForFrame(id){return STORIES.filter(function(s){return (s.frame_ids||[]).indexOf(id)>=0;});}
function previewHtml(p){
if(!p||!p.kind)return "";
if(p.kind==="image")return '<div class="preview-wrap"><span class="tagline">snapshot</span><img class="preview" src="'+esc(p.src)+'" alt="'+esc(p.alt||"")+'" loading="lazy" data-zoom="'+esc(p.src)+'">'+(p.caption?'<div class="preview-cap"><span>'+esc(p.caption)+'</span></div>':"")+'</div>';
if(p.kind==="iframe")return '<div class="preview-wrap live"><span class="tagline">live</span><iframe class="preview-frame" src="'+esc(p.url)+'" loading="lazy" referrerpolicy="no-referrer" sandbox="allow-scripts allow-same-origin allow-forms allow-popups"></iframe><div class="preview-cap"><span>'+esc(p.caption||"live embed &middot; needs the app running")+'</span><a href="'+esc(p.url)+'" target="_blank" rel="noopener noreferrer">open &#8599;</a></div></div>';
return "";
}

function select(id){
var f=frameById[id];if(!f)return;active=id;
var personas=(f.persona_ids||[]).map(function(pid){return '<span class="chip">'+esc(personaName(pid))+'</span>';}).join("");
var rel=storiesForFrame(id).map(function(s){return '<div class="rel"><b>'+esc(personaName(s.persona_id))+'</b> wants '+esc(s.want)+'</div>';}).join("");
document.getElementById("detail").innerHTML='<div class="d-stage">'+esc(f.stage||"Flow")+'</div><h2>'+esc(f.title)+'</h2>'+(f.ui_hint?'<div class="d-ui">&#9670; '+esc(f.ui_hint)+'</div>':"")+conf(f.confidence)+previewHtml(f.preview)+'<div class="d-sec">What happens</div><p>'+esc(f.detail||f.summary||"")+'</p>'+(f.user_value?'<div class="d-sec">Why it matters</div><p>'+esc(f.user_value)+'</p>':"")+(personas?'<div class="d-sec">For</div><div>'+personas+'</div>':"")+(rel?'<div class="d-sec">Related stories</div>'+rel:"");
var pv=document.querySelector("#detail img.preview[data-zoom]");
if(pv)pv.addEventListener("click",function(){var lb=document.getElementById("lightbox");document.getElementById("lightboxImg").src=pv.getAttribute("data-zoom");lb.classList.add("open");lb.setAttribute("aria-hidden","false");});
document.querySelectorAll(".frame").forEach(function(c){c.classList.toggle("active",c.getAttribute("data-frame-id")===id);});
}

function highlight(ids){var set={};ids.forEach(function(i){set[i]=1;});document.querySelectorAll(".frame").forEach(function(c){c.classList.toggle("linked",!!set[c.getAttribute("data-frame-id")]);});}

var storyRoot=document.getElementById("stories");
STORIES.forEach(function(s){
var el=document.createElement("div");el.className="story";el.setAttribute("role","button");el.setAttribute("tabindex","0");
var acc=(s.acceptance||[]).map(function(a){return '<li>'+esc(a)+'</li>';}).join("");
el.innerHTML='<div class="story-line"><span class="kw">As a</span> '+esc(personaName(s.persona_id))+' <span class="kw">I want</span> '+esc(s.want)+(s.benefit?' <span class="kw">so that</span> '+esc(s.benefit):"")+'</div>'+(acc?'<ul class="acc">'+acc+'</ul>':"")+conf(s.confidence);
function go(){var ids=(s.frame_ids||[]);highlight(ids);if(ids.length)select(ids[0]);}
el.addEventListener("click",go);
el.addEventListener("keydown",function(ev){if(ev.key==="Enter"||ev.key===" "){ev.preventDefault();go();}});
storyRoot.appendChild(el);
});
if(!storyRoot.children.length)storyRoot.innerHTML='<div class="muted">No user stories provided.</div>';

var outcomeRoot=document.getElementById("outcomes");
OUTCOMES.forEach(function(o){var el=document.createElement("div");el.className="outcome";el.innerHTML='<b>'+esc(o.title)+'</b>'+(o.body?'<p>'+esc(o.body)+'</p>':"")+conf(o.confidence);outcomeRoot.appendChild(el);});
if(!outcomeRoot.children.length)outcomeRoot.innerHTML='<div class="muted">No outcomes provided.</div>';

var lb=document.getElementById("lightbox");
function closeLb(){lb.classList.remove("open");lb.setAttribute("aria-hidden","true");document.getElementById("lightboxImg").src="";}
lb.addEventListener("click",closeLb);
document.addEventListener("keydown",function(ev){if(ev.key==="Escape")closeLb();});
if(FRAMES.length)select(FRAMES[0].id);
})();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StoryboardManifest {
        StoryboardManifest {
            schema_version: default_schema_version(),
            feature: FeatureDefinition {
                id: "secure-upload".into(),
                title: "Secure Document Upload".into(),
                domain: "feature".into(),
                summary: "Members upload documents that are protected before storage.".into(),
            },
            title: "Secure Document Upload".into(),
            subtitle: "How a member uploads a file and gets a confidential, shareable record.".into(),
            audience: "Members uploading sensitive documents".into(),
            overall_confidence: 0.78,
            personas: vec![Persona {
                id: "member".into(),
                name: "Member".into(),
                description: "A signed-in user with documents to protect.".into(),
                goal: "Store a document safely and share it later.".into(),
            }],
            stories: vec![
                UserStory {
                    id: "st-upload".into(),
                    persona_id: "member".into(),
                    want: "to upload a document from my dashboard".into(),
                    benefit: "I can keep it safe without emailing it around".into(),
                    acceptance: vec![
                        "I can pick a file from my device".into(),
                        "I see progress while it uploads".into(),
                    ],
                    confidence: 0.82,
                    frame_ids: vec!["pick".into(), "encrypt".into()],
                },
                UserStory {
                    id: "st-share".into(),
                    persona_id: "member".into(),
                    want: "to share the stored document with a teammate".into(),
                    benefit: "they can read it without a separate copy".into(),
                    acceptance: vec!["I can generate a private link".into()],
                    confidence: 0.6,
                    frame_ids: vec!["confirm".into()],
                },
            ],
            frames: vec![
                Frame {
                    id: "pick".into(),
                    title: "Choose a file".into(),
                    stage: "Discover".into(),
                    summary: "The member selects a document to protect.".into(),
                    detail: "From the dashboard the member taps Upload and picks a file from their device.".into(),
                    user_value: "A familiar, one-tap start.".into(),
                    ui_hint: "Dashboard \u{203a} Upload".into(),
                    confidence: 0.9,
                    persona_ids: vec!["member".into()],
                    preview: Some(FramePreview::Image {
                        src: "previews/upload-picker.png".into(),
                        alt: "The upload picker on the dashboard".into(),
                        caption: "Dashboard › Upload (captured 1440×900)".into(),
                    }),
                },
                Frame {
                    id: "encrypt".into(),
                    title: "It gets protected".into(),
                    stage: "Act".into(),
                    summary: "The file is locked before it is stored.".into(),
                    detail: "While uploading, the document is locked so only authorized people can open it.".into(),
                    user_value: "Peace of mind that the file stays private.".into(),
                    ui_hint: String::new(),
                    confidence: 0.7,
                    persona_ids: vec!["member".into()],
                    preview: Some(FramePreview::Iframe {
                        url: "http://localhost:5173/upload?state=encrypting".into(),
                        caption: String::new(),
                    }),
                },
                Frame {
                    id: "confirm".into(),
                    title: "See it's saved".into(),
                    stage: "Confirm".into(),
                    summary: "A confirmation and a shareable record appear.".into(),
                    detail: "A success screen confirms the document is stored and offers a private share link.".into(),
                    user_value: "Clear proof the task is done.".into(),
                    ui_hint: String::new(),
                    confidence: 0.75,
                    persona_ids: vec![],
                    preview: None,
                },
            ],
            outcomes: vec![Outcome {
                id: "o-trust".into(),
                title: "Documents are private by default".into(),
                body: "Members trust the product with sensitive files.".into(),
                confidence: 0.8,
            }],
        }
    }

    #[test]
    fn rendered_storyboard_embeds_manifest_and_markers() {
        let html = render_storyboard_html(&sample()).unwrap();
        assert!(html.contains(r#"id="chaos-storyboard-manifest""#));
        assert!(html.contains("data-chaos-storyboard"));
        assert!(html.contains("data-frame-id"));
        assert!(html.contains("data-chaos-personas"));
        assert!(html.contains("data-chaos-detail"));
        assert!(html.contains("addEventListener"));
        assert!(html.contains("Secure Document Upload"));

        // The embedded block is still valid JSON (escape_script_json only swaps
        // & < > for \uXXXX inside string values), so it round-trips back out.
        let marker = r#"id="chaos-storyboard-manifest">"#;
        let start = html.find(marker).unwrap() + marker.len();
        let end = html[start..].find("</script>").unwrap();
        let raw = html[start..start + end].trim();
        let back: StoryboardManifest = serde_json::from_str(raw).unwrap();
        assert_eq!(back.frames.len(), 3);
        assert_eq!(back.stories.len(), 2);
        // The frame previews survive the round-trip (tagged by `kind`).
        assert!(matches!(
            back.frames[0].preview,
            Some(FramePreview::Image { .. })
        ));
        assert!(matches!(
            back.frames[1].preview,
            Some(FramePreview::Iframe { .. })
        ));
        assert!(back.frames[2].preview.is_none());
    }

    #[test]
    fn rendered_storyboard_embeds_frame_previews() {
        let html = render_storyboard_html(&sample()).unwrap();
        // Image + iframe preview sources reach the page, plus the lightbox and
        // the renderer that mounts them.
        assert!(html.contains("previews/upload-picker.png"));
        assert!(html.contains("http://localhost:5173/upload?state=encrypting"));
        assert!(html.contains("class=\"lightbox\""));
        assert!(html.contains("function previewHtml"));
        assert!(html.contains("preview-frame"));
    }

    #[test]
    fn validate_rejects_empty_preview_src() {
        let mut manifest = sample();
        manifest.frames[0].preview = Some(FramePreview::Image {
            src: "   ".into(),
            alt: String::new(),
            caption: String::new(),
        });
        assert!(validate_storyboard(&manifest).is_err());
    }

    #[test]
    fn validate_rejects_active_content_preview_scheme() {
        let mut manifest = sample();
        manifest.frames[1].preview = Some(FramePreview::Iframe {
            url: "javascript:alert(1)".into(),
            caption: String::new(),
        });
        assert!(validate_storyboard(&manifest).is_err());
    }

    #[test]
    fn validate_rejects_thin_manifest() {
        let mut manifest = sample();
        manifest.frames.truncate(1);
        assert!(validate_storyboard(&manifest).is_err());
    }

    #[test]
    fn validate_rejects_dangling_frame_id() {
        let mut manifest = sample();
        manifest.stories[0].frame_ids = vec!["nope".into()];
        assert!(validate_storyboard(&manifest).is_err());
    }

    #[test]
    fn validate_rejects_out_of_range_confidence() {
        let mut manifest = sample();
        manifest.frames[0].confidence = 1.5;
        assert!(validate_storyboard(&manifest).is_err());
    }

    #[test]
    fn validate_accepts_sample() {
        validate_storyboard(&sample()).unwrap();
    }

    #[test]
    fn write_storyboard_is_ignored_by_feature_loaders() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let path =
            write_storyboard(repo, &sample(), "secure upload", "Secure Document Upload").unwrap();
        assert!(path.ends_with("docs/features_memory/secure-upload-story.html"));

        // A storyboard page uses `chaos-storyboard-manifest`, not
        // `chaos-feature-manifest`, so the feature-map loaders never see it.
        let features_dir = repo.join("docs/features_memory");
        let matches = crate::feature_context::load_feature_matches(
            "secure upload document",
            &features_dir,
            3,
            8,
        )
        .unwrap();
        assert!(matches.is_empty());

        // ...and `refresh --all-features` skips it (no feature manifest to render).
        assert!(!crate::feature_export::refresh_feature_page(repo, &path).unwrap());
    }
}
