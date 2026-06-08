//! `chaos storyboard` / `chaos_write_storyboard` — a client/user-facing
//! **"Feature guide"**.
//!
//! Where `feature_export`/`impact` describe a feature for engineers (graphs,
//! files, symbols, source), this module renders the *same* feature for a client
//! or end user: a UI/UX **user story** with **no code**. The feature is broken
//! into **frames** that render as an alternating, scroll-driven **walkthrough**
//! (each step paired with a device mockup built from the frame's real `preview`,
//! or an honest "add a screenshot" placeholder when none — Chaos never fakes the
//! client UI), and the **user stories** are spelled out ("As a … I want … so
//! that …"). `confidence` values are optional metadata and are *not* rendered to
//! the end user. Optional sections — a hero key-visual, branding, a
//! role/permission `matrix`, an agent-style `callout`, and an end-of-page `game`
//! — let a manifest match the full editorial look.
//!
//! The agent supplies only the structured, code-free [`StoryboardManifest`]; the
//! Rust side owns the rendering, so the shared light editorial theme and the
//! scroll-unlock gamification are guaranteed every time. The manifest is
//! embedded back into the page under the id `chaos-storyboard-manifest` for
//! agentic reads. That id is deliberately *different* from the feature-map
//! `chaos-feature-manifest`, so `refresh --all-features` and `load_feature_matches`
//! ignore storyboard pages (see the isolation test).

use crate::{
    export_util::escape_script_json,
    feature_context::FeatureDefinition,
    theme::{self, Brand},
};
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
/// rendered by [`render_storyboard_html`] into an interactive light editorial
/// "Feature guide" page.
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
    /// Optional hero key-visual banner shown beside the title (image `src`: a
    /// relative path, an `http(s)` URL, or a `data:image/...` URI). Empty hides
    /// the banner and the hero spans full width.
    #[serde(default)]
    pub hero_image: String,
    #[serde(default)]
    pub personas: Vec<Persona>,
    /// Optional "who can do what" comparison matrix (the role/permission table).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matrix: Option<Matrix>,
    /// Optional highlighted callout section (e.g. an AI-agent spotlight), echoing
    /// the Access-Control "agents, on a timer" band.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callout: Option<Callout>,
    #[serde(default)]
    pub stories: Vec<UserStory>,
    #[serde(default)]
    pub frames: Vec<Frame>,
    #[serde(default)]
    pub outcomes: Vec<Outcome>,
    /// Optional end-of-page interactive mini-game — a short, click-to-check
    /// "did you get the rules?" challenge that gamifies the guide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<Game>,
    /// Optional, code-free branding for the page chrome (logo, name, tagline,
    /// link). Empty renders a neutral "Add your logo" placeholder.
    #[serde(default)]
    pub brand: Brand,
    /// Optional name of a brand preset shipped with Chaos (e.g. "molecule").
    /// When set, it fills any empty `brand`/`hero_image` fields from the embedded
    /// preset, so a page can be branded by name without inlining assets. Explicit
    /// manifest values always win. Validated against the known preset names.
    #[serde(default)]
    pub brand_preset: String,
}

/// A kind of user the feature serves. Rendered as an Access-Control-style role
/// card; the optional fields drive the icon, the "who" line, the "includes …"
/// relationship, and the role-ladder diagram.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Persona {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// What this persona is trying to achieve.
    #[serde(default)]
    pub goal: String,
    /// Optional short audience line, e.g. "Reviewers, partners, investors".
    #[serde(default)]
    pub who: String,
    /// Optional built-in icon keyword (eye, file, crown, agent, key, user,
    /// shield, ...). Unknown/empty falls back to the persona initial.
    #[serde(default)]
    pub icon: String,
    /// Optional name of a lower persona this one builds on; renders an
    /// "Includes {includes}" chip on the card.
    #[serde(default)]
    pub includes: String,
    /// Optional authority tier (higher = more powerful) used to order the
    /// role-ladder diagram. Personas with tier 0 are omitted from the ladder.
    #[serde(default)]
    pub tier: i32,
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

/// A "who can do what" comparison table — capabilities down the rows, roles
/// across the columns, a check/blank in each cell.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Matrix {
    /// Column headers (usually persona/role names).
    pub columns: Vec<String>,
    /// One row per capability.
    pub rows: Vec<MatrixRow>,
    /// Optional caption shown under the table.
    #[serde(default)]
    pub caption: String,
}

/// One capability row of a [`Matrix`]: a label plus one allowed-flag per column.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MatrixRow {
    pub capability: String,
    /// One flag per column (`true` = a check). Validation requires exactly one
    /// flag per `columns` entry, so the table can never silently drop or blank a
    /// cell.
    #[serde(default)]
    pub allowed: Vec<bool>,
}

/// A highlighted callout section — its own kicker/heading/intro plus a single
/// emphasised card with feature pills. Mirrors the Access-Control "AI agents,
/// on a timer" band.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Callout {
    /// Section kicker (overline), e.g. "AI agents".
    #[serde(default)]
    pub kicker: String,
    /// Section heading, e.g. "Bring in AI agents, on a timer".
    #[serde(default)]
    pub heading: String,
    /// Section intro paragraph.
    #[serde(default)]
    pub intro: String,
    /// Card title.
    pub title: String,
    /// Card body.
    #[serde(default)]
    pub body: String,
    /// Short feature pills shown under the card body.
    #[serde(default)]
    pub points: Vec<String>,
}

/// An end-of-page interactive mini-game: a sequence of short scenarios the
/// reader judges (e.g. "allow or deny?"), with instant feedback, a running
/// score, and a win message — a click-to-check recap that gamifies the guide.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Game {
    /// Section kicker (overline), e.g. "Test yourself".
    #[serde(default)]
    pub kicker: String,
    /// Section heading, e.g. "Can you run the door?".
    #[serde(default)]
    pub heading: String,
    /// Section intro paragraph.
    #[serde(default)]
    pub intro: String,
    /// One-line instructions shown on the game card.
    #[serde(default)]
    pub instructions: String,
    /// The scenarios, played in order.
    pub rounds: Vec<GameRound>,
    /// Message shown when the player finishes.
    #[serde(default)]
    pub win_message: String,
}

/// One scenario in a [`Game`]: a prompt, optional context chips, and the
/// options the reader chooses between.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GameRound {
    /// The scenario question, e.g. "An expired agent opens a confidential file."
    pub prompt: String,
    /// Optional context chips describing the scenario (role, file, status…).
    #[serde(default)]
    pub context: Vec<String>,
    /// The choices; at least one must be marked `correct`.
    pub options: Vec<GameOption>,
}

/// One choice in a [`GameRound`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GameOption {
    pub label: String,
    /// Whether picking this is the right call.
    #[serde(default)]
    pub correct: bool,
    /// Optional explanation revealed after the pick.
    #[serde(default)]
    pub explain: String,
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
    // Frame ids must be unique: the walkthrough keys its per-step state and its
    // explored/total progress count by frame id, so a duplicate would leave the
    // progress HUD permanently stuck below 100%.
    let mut seen_frame_ids: HashSet<&str> = HashSet::new();
    for frame in &manifest.frames {
        if frame.id.trim().is_empty() {
            bail!("every frame needs a non-empty id");
        }
        if !seen_frame_ids.insert(frame.id.as_str()) {
            bail!(
                "duplicate frame id `{}`; frame ids must be unique",
                frame.id
            );
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

    if !manifest.hero_image.trim().is_empty() && !media_src_ok(&manifest.hero_image) {
        bail!("hero_image uses a disallowed scheme; use http(s), a relative path, or a data:image URI");
    }

    if let Some(matrix) = &manifest.matrix {
        if matrix.columns.is_empty() {
            bail!("matrix.columns must not be empty when a matrix is provided");
        }
        if matrix.rows.is_empty() {
            bail!("matrix.rows must not be empty when a matrix is provided");
        }
        for (i, row) in matrix.rows.iter().enumerate() {
            if row.capability.trim().is_empty() {
                bail!("matrix.rows[{i}] needs a capability label");
            }
            if row.allowed.len() != matrix.columns.len() {
                bail!(
                    "matrix.rows[{i}].allowed has {} flag(s) but there are {} column(s); provide exactly one per column",
                    row.allowed.len(),
                    matrix.columns.len()
                );
            }
        }
    }

    if let Some(callout) = &manifest.callout {
        if callout.title.trim().is_empty() {
            bail!("callout.title must not be empty when a callout is provided");
        }
    }

    if let Some(game) = &manifest.game {
        if game.rounds.is_empty() {
            bail!("game.rounds must not be empty when a game is provided");
        }
        for (i, round) in game.rounds.iter().enumerate() {
            if round.prompt.trim().is_empty() {
                bail!("game.rounds[{i}] needs a prompt");
            }
            if round.options.len() < 2 {
                bail!("game.rounds[{i}] needs at least two options");
            }
            if !round.options.iter().any(|o| o.correct) {
                bail!("game.rounds[{i}] needs at least one option marked correct");
            }
            for (j, opt) in round.options.iter().enumerate() {
                if opt.label.trim().is_empty() {
                    bail!("game.rounds[{i}].options[{j}] needs a label");
                }
            }
        }
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
    match preview {
        FramePreview::Image { src, .. } => {
            let trimmed = src.trim();
            if trimmed.is_empty() {
                bail!("frame `{frame_id}` preview image src must not be empty");
            }
            // The image is rendered into an inert `<img>`; reject active-content
            // schemes defensively (newline-safe).
            if !media_src_ok(trimmed) {
                bail!(
                    "frame `{frame_id}` preview image src uses a disallowed scheme; use http(s), a relative path, or a data:image URL"
                );
            }
        }
        FramePreview::Iframe { url, .. } => {
            let trimmed = url.trim();
            if trimmed.is_empty() {
                bail!("frame `{frame_id}` preview iframe url must not be empty");
            }
            // The url is loaded in a script-enabled `<iframe>` and also used as an
            // `<a href>` — both can execute a non-http(s) scheme, so allow-list
            // strictly: only http(s) or a relative path.
            if !iframe_url_ok(trimmed) {
                bail!(
                    "frame `{frame_id}` preview iframe url must be an http(s) URL or a relative path (no javascript:/data:/blob:/other schemes)"
                );
            }
        }
    }
    Ok(())
}

/// Defense-in-depth for a value rendered into an inert `src` attribute (an
/// `<img>` or the hero banner): reject active-content schemes (`javascript:`,
/// `vbscript:`, `data:text/html`). `http(s)`, relative paths, and
/// `data:image/...` are allowed. Whitespace/control chars are stripped first so
/// a scheme can't be smuggled past the prefix check (e.g. `java\nscript:`).
fn media_src_ok(value: &str) -> bool {
    let lower = strip_url_noise(value);
    !(lower.starts_with("javascript:")
        || lower.starts_with("vbscript:")
        || lower.starts_with("data:text/html"))
}

/// Strict allow-list for a URL that will be loaded in a script-enabled
/// `<iframe>` (and offered as a clickable `<a href>`): only `http(s)://` or a
/// scheme-less relative path is allowed; every explicit non-http scheme
/// (`data:`, `blob:`, `javascript:`, …) is rejected. Whitespace/control chars
/// are stripped first so a scheme can't be hidden with embedded newlines.
fn iframe_url_ok(value: &str) -> bool {
    let lower = strip_url_noise(value);
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return true;
    }
    // A scheme is text before a ':' that precedes the first '/', '?' or '#'. If
    // one is present it isn't http(s) (handled above), so reject; otherwise the
    // value is a relative path and is allowed.
    let path_break = lower.find(['/', '?', '#']).unwrap_or(lower.len());
    !lower[..path_break].contains(':')
}

/// Lower-case a candidate URL with ASCII whitespace and control characters
/// removed — the HTML/URL parsers ignore those, so they must not survive a
/// scheme check.
fn strip_url_noise(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Render the interactive **feature-guide** page from a manifest. The manifest
/// is embedded under `chaos-storyboard-manifest` for agentic reads; everything
/// dynamic (personas, matrix, callout, the scrollytelling walkthrough, stories,
/// outcomes, and the end mini-game) is rendered client-side from it. The shared
/// light editorial theme and the scroll-driven gamification are baked into the
/// template, so they are guaranteed every time regardless of what the agent
/// supplies.
pub fn render_storyboard_html(manifest: &StoryboardManifest) -> Result<String> {
    let json = serde_json::to_string(manifest)?;
    let domain = manifest.feature.domain.trim();
    let eyebrow = if domain.is_empty() {
        "Feature guide".to_string()
    } else {
        domain.to_string()
    };
    // Breadcrumb is intentional markup (tags are not escaped); only the dynamic
    // text segments are escaped.
    let crumb = if domain.is_empty() {
        format!(
            "Feature guide<span class=\"sep\">&rsaquo;</span><b>{}</b>",
            html_escape(&manifest.title)
        )
    } else {
        format!(
            "{}<span class=\"sep\">&rsaquo;</span><b>{}</b>",
            html_escape(domain),
            html_escape(&manifest.title)
        )
    };
    // The nutshell band uses the feature summary, falling back to the subtitle.
    let summary = if manifest.feature.summary.trim().is_empty() {
        manifest.subtitle.clone()
    } else {
        manifest.feature.summary.clone()
    };
    // Optional hero key-visual: validated upstream, escaped here.
    let hero_image = manifest.hero_image.trim();
    let hero_art = if hero_image.is_empty() {
        String::new()
    } else {
        format!(
            r#"<div class="hero-art"><img src="{src}" alt="{alt}" loading="eager"></div>"#,
            src = html_escape(hero_image),
            alt = html_escape(&format!("{} — key visual", manifest.title)),
        )
    };
    let hero_class = if hero_image.is_empty() {
        ""
    } else {
        " has-art"
    };
    // Single-pass fill so a dynamic value that happens to contain a literal
    // `__TOKEN__` can never be re-scanned and corrupted by a later substitution.
    Ok(fill_template(
        STORYBOARD_HTML,
        &[
            ("__THEME__", theme::THEME_CSS),
            ("__EYEBROW__", &html_escape(&eyebrow)),
            ("__SUMMARY__", &html_escape(&summary)),
            ("__SUBTITLE__", &html_escape(&manifest.subtitle)),
            ("__AUDIENCE__", &html_escape(&manifest.audience)),
            ("__HERO_CLASS__", hero_class),
            ("__HERO_ART__", &hero_art),
            ("__TITLE__", &html_escape(&manifest.title)),
            ("__CRUMB__", &crumb),
            (
                "__BRAND_TOPBAR__",
                &theme::render_brand(&manifest.brand, "topbar"),
            ),
            (
                "__BRAND_FOOTER__",
                &theme::render_brand(&manifest.brand, "footer"),
            ),
            ("__MANIFEST__", &escape_script_json(&json)),
        ],
    ))
}

/// Replace every `__TOKEN__` placeholder in `template` in a single left-to-right
/// pass, so a substituted value that itself contains a `__TOKEN__` sequence is
/// never re-scanned (the chained `str::replace` approach could corrupt the page
/// if, say, a subtitle literally contained `__MANIFEST__`).
fn fill_template(template: &str, subs: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 64 * 1024);
    let mut rest = template;
    while let Some(pos) = rest.find("__") {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        match subs.iter().find(|(tok, _)| tail.starts_with(*tok)) {
            Some((tok, val)) => {
                out.push_str(val);
                rest = &tail[tok.len()..];
            }
            None => {
                // A bare `__` that isn't a known token: emit it and move past so
                // we don't loop forever on the same position.
                out.push_str("__");
                rest = &tail[2..];
            }
        }
    }
    out.push_str(rest);
    out
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
    apply_brand_preset(&mut manifest)?;
    validate_storyboard(&manifest)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output, render_storyboard_html(&manifest)?)?;
    Ok(output.to_path_buf())
}

/// Resolve `manifest.brand_preset` (a name like "molecule") against the brand
/// presets shipped in the binary, filling any **empty** `brand`/`hero_image`
/// fields from the preset. Explicit manifest values always win, so a page can
/// take the preset's logo but override the tagline, etc. An unknown preset name
/// is an error (so a typo fails loudly rather than rendering unbranded).
fn apply_brand_preset(manifest: &mut StoryboardManifest) -> Result<()> {
    let name = manifest.brand_preset.trim();
    if name.is_empty() {
        return Ok(());
    }
    let preset = theme::brand_preset(name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown brand_preset `{name}`; shipped presets are: {}",
            theme::BRAND_PRESET_NAMES.join(", ")
        )
    })?;
    let b = &mut manifest.brand;
    if b.name.trim().is_empty() {
        b.name = preset.brand.name;
    }
    if b.tagline.trim().is_empty() {
        b.tagline = preset.brand.tagline;
    }
    if b.logo_src.trim().is_empty() {
        b.logo_src = preset.brand.logo_src;
    }
    if b.href.trim().is_empty() {
        b.href = preset.brand.href;
    }
    if manifest.hero_image.trim().is_empty() {
        manifest.hero_image = preset.hero_image;
    }
    Ok(())
}

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#039;")
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
<title>__TITLE__ &middot; Feature guide</title>
<style>
__THEME__
/* ===== feature-guide components (light editorial, Access-Control lineage) ===== */
/* contrast: darken the small-text tokens to clear WCAG AA on white (decorative
   uses are unaffected — these tokens drive captions/overlines, not large fills) */
:root{--fg-tertiary:var(--color-ink-300);--color-blue-500:var(--color-blue-600)}
/* visible focus ring on every interactive control */
.opt-btn:focus-visible,.btn-primary:focus-visible,.btn-ghost:focus-visible,.rail-stage:focus-visible,.lb-close:focus-visible,img.preview:focus-visible,.story:focus-visible{outline:none;box-shadow:var(--shadow-focus)}
.btn-primary{display:inline-flex;align-items:center;gap:8px;font:var(--type-body-sm);font-weight:500;
  border:0;border-radius:var(--radius-pill);padding:11px 20px;background:var(--color-ink-600);color:#fff;
  text-decoration:none;cursor:pointer;transition:background .15s,transform .15s,box-shadow .15s;box-shadow:var(--shadow-sm)}
.btn-primary:hover{background:var(--color-blue-700);color:#fff;transform:translateY(-1px);box-shadow:var(--shadow-md)}
.btn-ghost{display:inline-flex;align-items:center;gap:8px;font:var(--type-body-sm);font-weight:500;
  border:var(--border-hairline);border-radius:var(--radius-pill);padding:10px 18px;background:#fff;color:var(--color-ink-600);
  text-decoration:none;cursor:pointer;transition:border-color .15s,color .15s}
.btn-ghost:hover{border-color:var(--color-blue-400);color:var(--color-blue-700)}

/* hero: copy + full-bleed key visual (floats on the sky gradient, no card) */
.hero .wrap{align-items:center;grid-template-columns:1fr}
.hero.has-art .wrap{grid-template-columns:1.08fr .92fr}
.hero:not(.has-art) .hero-aside{display:none}
.hero-copy{min-width:0}
.hero-cta{display:flex;align-items:center;gap:16px;margin-top:26px;flex-wrap:wrap}
.hero-meta{font:var(--type-body-sm);color:var(--color-ink-400)}
.hero-meta b{font-family:var(--font-mono);color:var(--color-blue-700);font-weight:500}
.hero-aside{position:relative}
.hero-art{position:relative}
.hero-art img{display:block;width:112%;max-width:none;height:auto;margin:0 -6% 0 0;
  filter:drop-shadow(0 28px 48px rgba(13,69,113,.20))}
@media(max-width:860px){.hero-art img{width:100%;margin:0}}

/* sticky game HUD */
.hud{position:sticky;top:64px;z-index:25;display:none;background:rgba(255,255,255,.9);
  backdrop-filter:saturate(1.2) blur(8px);border-bottom:var(--border-hairline)}
.hud.show{display:block}
.hud .wrap{display:flex;align-items:center;gap:18px;height:52px}
.hud-label{font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.1em;color:var(--fg-tertiary);white-space:nowrap}
.hud-count{font:var(--type-body-sm);color:var(--color-ink-500);white-space:nowrap}
.hud-count b{font-family:var(--font-mono);color:var(--color-blue-700)}
.hud-pips{display:flex;gap:6px;margin-left:2px}
.hud-pip{width:9px;height:9px;border-radius:3px;background:var(--color-surface-3);border:1px solid var(--color-border);transition:background .3s,border-color .3s,transform .3s}
.hud-pip.done{background:var(--color-teal-500);border-color:transparent;transform:scale(1.05)}
.hud-pip.active{border-color:var(--color-blue-500);box-shadow:0 0 0 2px rgba(132,178,233,.3)}
.hud .sp{flex:1}
.hud-bar{flex:0 0 160px;height:6px;border-radius:var(--radius-pill);background:var(--color-surface-3);overflow:hidden}
.hud-fill{height:100%;width:0;border-radius:var(--radius-pill);background:linear-gradient(90deg,var(--color-blue-400),var(--color-blue-700));transition:width .5s cubic-bezier(.2,.8,.2,1)}
.hud.complete .hud-fill{background:linear-gradient(90deg,var(--color-teal-500),var(--color-blue-600))}
@media(max-width:620px){.hud-label,.hud-pips{display:none}.hud-bar{flex-basis:90px}}

/* role cards */
.roles{display:grid;grid-template-columns:repeat(auto-fill,minmax(248px,1fr));gap:16px;margin-top:36px}
.role{border:var(--border-hairline);border-radius:var(--radius-lg);background:#fff;padding:24px;display:flex;
  flex-direction:column;gap:12px;box-shadow:var(--shadow-sm);position:relative}
.role .ic{width:40px;height:40px;border-radius:var(--radius-md);display:grid;place-items:center;color:#fff;
  background:linear-gradient(135deg,var(--color-blue-400),var(--color-blue-700))}
/* semantic icon ramp: higher authority = darker; AI agents get the violet diamond */
.role-t1 .ic{background:linear-gradient(135deg,var(--color-blue-300),var(--color-blue-500))}
.role-t2 .ic{background:linear-gradient(135deg,var(--color-blue-500),var(--color-blue-700))}
.role-t3 .ic{background:linear-gradient(135deg,var(--color-blue-700),var(--color-ink-600))}
.role-agent .ic{background:linear-gradient(135deg,var(--color-violet-500),var(--color-purple-500))}
.role .ic svg{width:21px;height:21px}
.role .ic b{font:var(--type-h5);font-family:var(--font-display)}
.role h3{font:var(--type-h5);color:var(--color-ink-700);margin:0}
.role .who{font:var(--type-body-sm);color:var(--fg-tertiary);margin:-6px 0 0}
.role p{font:var(--type-body-sm);color:var(--color-ink-400);line-height:1.55;margin:0}
.role .incl{margin-top:auto;padding-top:10px;font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.06em;
  color:var(--color-blue-600);display:flex;align-items:center;gap:7px}
.role .incl svg{width:13px;height:13px}

/* role ladder */
.hier{margin-top:34px;display:flex;flex-direction:column;gap:0;max-width:600px}
.hier .lvl{border-radius:var(--radius-lg);padding:15px 20px;color:#fff;display:flex;align-items:baseline;gap:10px}
.hier .lvl:nth-child(1){background:var(--color-ink-600)}
.hier .lvl:nth-child(2){background:var(--color-blue-700);margin:8px 0 0 26px}
.hier .lvl:nth-child(3){background:var(--color-blue-600);margin:8px 0 0 52px}
.hier .lvl:nth-child(4){background:var(--color-blue-400);margin:8px 0 0 78px}
.hier .lvl b{font:var(--type-h6);font-weight:500}
.hier .lvl small{font:var(--type-body-xs);color:rgba(255,255,255,.8)}
.hier-note{font:var(--type-body-sm);color:var(--fg-tertiary);margin-top:14px;display:flex;gap:9px;align-items:center}
.hier-note svg{width:15px;height:15px;color:var(--color-blue-400)}

/* permission matrix */
.matrix-wrap{margin-top:34px;border:1px solid var(--color-border-soft);border-radius:var(--radius-lg);
  overflow:hidden;background:#fff;box-shadow:var(--shadow-sm)}
table.matrix{width:100%;border-collapse:collapse}
table.matrix th,table.matrix td{text-align:left;padding:15px 20px}
table.matrix thead th{background:#E9EFF7;font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.05em;color:var(--color-ink-400)}
table.matrix thead th.col{text-align:center;width:130px}
table.matrix tbody td{border-top:1px solid var(--color-border-soft);font:var(--type-body-sm);color:var(--color-ink-600)}
table.matrix tbody td.cap{font-weight:500}
table.matrix tbody td.mk{text-align:center}
table.matrix tbody tr:hover td{background:var(--color-surface-2)}
.ck{display:inline-grid;place-items:center;width:22px;height:22px;border-radius:var(--radius-pill);background:rgba(0,200,187,.14);color:#007f76}
.ck svg{width:12px;height:12px}
.no{display:inline-block;width:14px;height:2px;border-radius:2px;background:var(--color-ink-100)}
.matrix-cap{font:var(--type-body-sm);color:var(--fg-tertiary);margin-top:12px}

/* agent-style callout */
.callout{margin-top:36px;border:var(--border-hairline);border-radius:var(--radius-xl);
  background:linear-gradient(180deg,#fff,var(--color-surface-2));padding:32px;display:grid;
  grid-template-columns:auto 1fr;gap:24px;align-items:center}
.callout .diamond{width:56px;height:56px;border-radius:14px;background:var(--bg-diamond);display:grid;place-items:center;
  box-shadow:0 8px 24px rgba(128,68,255,.22)}
.callout .diamond svg{width:26px;height:26px;color:#fff}
.callout h3{font:var(--type-h4);color:var(--color-ink-700);margin:0 0 8px}
.callout p{font:var(--type-body);color:var(--color-ink-400);line-height:1.55;margin:0;max-width:70ch}
.callout .feats{display:flex;gap:10px;margin-top:14px;flex-wrap:wrap}
.callout .feat{font:var(--type-body-sm);color:var(--color-ink-500);background:#fff;border:var(--border-hairline);
  border-radius:var(--radius-pill);padding:6px 13px;display:flex;align-items:center;gap:7px}
.callout .feat svg{width:14px;height:14px;color:var(--color-violet-500)}
@media(max-width:640px){.callout{grid-template-columns:1fr}}

/* ===== walkthrough: scrollytelling journey ===== */
.journey{display:grid;grid-template-columns:168px minmax(0,1fr);gap:36px;margin-top:30px;align-items:start}
.rail{position:sticky;top:128px;align-self:start;display:flex;flex-direction:column;gap:0}
.rail-stage{position:relative;padding:0 0 22px 22px;font:var(--type-body-sm);color:var(--fg-tertiary);
  line-height:1.3;cursor:pointer;transition:color .2s}
.rail-stage:last-child{padding-bottom:0}
.rail-stage::before{content:"";position:absolute;left:5px;top:3px;bottom:-3px;width:2px;background:var(--color-border)}
.rail-stage:last-child::before{display:none}
.rail-stage::after{content:"";position:absolute;left:0;top:2px;width:12px;height:12px;border-radius:50%;
  background:#fff;border:2px solid var(--color-border);transition:background .2s,border-color .2s,transform .2s}
.rail-stage.active{color:var(--color-ink-700);font-weight:500}
.rail-stage.active::after{border-color:var(--color-blue-700);transform:scale(1.15)}
.rail-stage.done::after{background:var(--color-teal-500);border-color:var(--color-teal-500)}
.rail-stage small{display:block;font:var(--type-overline-sm);font-family:var(--font-mono);color:var(--color-blue-500);letter-spacing:.08em}

.steps{min-width:0}
.stage-head{display:flex;align-items:center;gap:12px;margin:14px 0 6px;padding-top:24px}
.steps>.stage-head:first-child{padding-top:0}
.stage-dot{width:8px;height:8px;border-radius:2px;background:var(--color-blue-400);flex:0 0 auto}
.stage-name{font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.14em;color:var(--color-blue-500)}
.stage-badge{margin-left:auto;display:inline-flex;align-items:center;gap:6px;font:var(--type-overline-sm);
  text-transform:uppercase;letter-spacing:.08em;color:var(--fg-muted);background:var(--color-surface-2);
  border:var(--border-hairline);border-radius:var(--radius-pill);padding:4px 11px;transition:all .3s}
.stage-badge svg{width:12px;height:12px}
.stage-badge.done{color:#007f76;background:rgba(0,200,187,.13);border-color:transparent}

.step{display:grid;grid-template-columns:1fr 1fr;gap:32px;align-items:center;padding:30px 0;
  border-top:var(--border-hairline)}
.step:first-of-type{border-top:0}
.step.flip .step-art{order:-1}
.step-num{display:flex;align-items:center;gap:12px;margin-bottom:14px}
.num-badge{flex:0 0 auto;width:36px;height:36px;border-radius:var(--radius-md);background:var(--color-surface-2);
  border:var(--border-hairline);display:grid;place-items:center;font:var(--type-overline);font-family:var(--font-mono);
  color:var(--color-ink-500);transition:background .3s,color .3s,border-color .3s}
.step.explored .num-badge{background:rgba(0,200,187,.14);border-color:transparent;color:#007f76}
.step-stage{font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.12em;color:var(--color-blue-500)}
.step h3{font:var(--type-h3);color:var(--color-ink-700);margin:0 0 12px;letter-spacing:-.01em}
.step .d-ui{font:var(--type-body-sm);font-family:var(--font-mono);color:var(--color-blue-700);background:var(--color-blue-50);
  border:1px solid var(--color-blue-100);border-radius:var(--radius-pill);padding:5px 12px;display:inline-block;margin-bottom:12px}
.step-detail{font:var(--type-body);color:var(--color-ink-400);line-height:1.6;margin:0;max-width:48ch}
.step .d-sec{font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.1em;color:var(--fg-tertiary);margin:16px 0 6px}
.step-why{font:var(--type-body-sm);color:var(--color-ink-500);line-height:1.55;margin:0;max-width:48ch}
.step-personas{margin-top:14px}
.chip{display:inline-flex;align-items:center;font:500 12px/1 var(--font-body);border:var(--border-hairline);
  border-radius:var(--radius-pill);padding:6px 11px;margin:4px 6px 0 0;color:var(--color-ink-500);background:var(--color-surface-1)}
.step .rel{border-left:2px solid var(--color-violet-500);padding:4px 0 4px 12px;margin-top:10px;font:var(--type-body-sm);color:var(--color-ink-500)}
.step .rel b{color:var(--color-violet-500)}
.step.linked{background:linear-gradient(90deg,rgba(128,68,255,.05),transparent);border-radius:var(--radius-lg)}
.step.linked .num-badge{border-color:var(--color-violet-500);color:var(--color-violet-500)}

/* device mockup */
.mock{border:var(--border-hairline);border-radius:var(--radius-lg);background:#fff;box-shadow:var(--shadow-md);overflow:hidden}
.mock-bar{display:flex;align-items:center;gap:6px;padding:11px 14px;border-bottom:var(--border-hairline);background:var(--color-surface-1)}
.mock-bar i{width:9px;height:9px;border-radius:50%;background:var(--color-border);display:block}
.mock-bar span{margin-left:8px;font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.08em;color:var(--fg-tertiary);
  white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.mock-screen{position:relative;min-height:210px;padding:24px;display:flex;flex-direction:column;justify-content:center;gap:14px;
  background:radial-gradient(120% 100% at 15% 0,var(--color-blue-50),#fff 60%);overflow:hidden}
.mock-screen .wm{position:absolute;right:14px;bottom:2px;font:600 110px/1 var(--font-display);color:var(--color-blue-100);
  letter-spacing:-.04em;user-select:none}
.mock-screen .mock-ic{width:44px;height:44px;border-radius:var(--radius-md);display:grid;place-items:center;color:#fff;
  background:linear-gradient(135deg,var(--color-blue-400),var(--color-blue-700));box-shadow:var(--shadow-sm);position:relative;z-index:1}
.mock-screen .mock-ic svg{width:23px;height:23px}
.mock-screen .mock-title{font:var(--type-h6);color:var(--color-ink-600);position:relative;z-index:1;max-width:80%}
.mock-screen .mock-lines{display:flex;flex-direction:column;gap:8px;position:relative;z-index:1}
.mock-screen .mock-lines span{height:8px;border-radius:var(--radius-pill);background:var(--color-surface-3)}
.mock-screen .mock-lines span:nth-child(1){width:78%}
.mock-screen .mock-lines span:nth-child(2){width:92%}
.mock-screen .mock-lines span:nth-child(3){width:64%}
.mock-screen .mock-tag{position:relative;z-index:1;align-self:flex-start;font:var(--type-overline-sm);font-family:var(--font-mono);
  text-transform:uppercase;letter-spacing:.1em;color:var(--color-blue-600);background:var(--color-blue-100);
  border-radius:var(--radius-pill);padding:4px 11px}

/* honest "no preview yet" placeholder — invites a real screenshot/route rather
   than faking the client UI */
.mock-empty{margin:16px;min-height:188px;border:1.5px dashed var(--color-blue-300);border-radius:var(--radius-md);
  display:flex;flex-direction:column;align-items:center;justify-content:center;gap:9px;text-align:center;padding:26px;
  background:var(--color-surface-2)}
.mock-empty .mock-empty-ic{width:38px;height:38px;color:var(--color-blue-400)}
.mock-empty .mock-empty-ic svg{width:38px;height:38px}
.mock-empty b{font:var(--type-h6);color:var(--color-ink-500)}
.mock-empty span{font:var(--type-body-sm);color:var(--fg-tertiary);max-width:34ch;line-height:1.5}

/* previews inside a mock */
.preview-wrap{position:relative;background:var(--color-surface-1)}
.preview-wrap .tagline{position:absolute;top:9px;left:9px;z-index:2;font:var(--type-overline-sm);text-transform:uppercase;
  letter-spacing:.08em;color:#fff;background:var(--color-blue-700);border-radius:var(--radius-pill);padding:3px 9px}
.preview-wrap.live .tagline{background:var(--color-teal-500);color:#04231f}
img.preview{display:block;width:100%;height:auto;cursor:zoom-in}
iframe.preview-frame{display:block;width:100%;height:380px;border:0;background:#fff}
.preview-cap{display:flex;justify-content:space-between;align-items:center;gap:10px;padding:9px 12px;font:var(--type-body-xs);
  color:var(--fg-tertiary);border-top:var(--border-hairline);background:#fff}
.preview-cap a{color:var(--color-blue-700);font-family:var(--font-mono);font-size:11px;white-space:nowrap;text-decoration:none}

/* per-step confidence bar */
.conf{display:flex;align-items:center;gap:10px;margin-top:18px}
.conf-track{flex:0 0 120px;height:6px;border-radius:var(--radius-pill);background:var(--color-surface-3);overflow:hidden}
.conf-fill{height:100%;width:0;border-radius:var(--radius-pill);background:linear-gradient(90deg,var(--color-blue-400),var(--color-blue-700));transition:width .9s cubic-bezier(.2,.8,.2,1)}
.conf span{font:var(--type-body-xs);font-family:var(--font-mono);color:var(--fg-tertiary)}
.js .step{opacity:0;transform:translateY(20px);transition:opacity .6s ease,transform .6s ease}
.js .step.in{opacity:1;transform:none}
@media(prefers-reduced-motion:reduce){.js .step{opacity:1;transform:none;transition:none}}
@media(max-width:900px){.journey{grid-template-columns:1fr}.rail{display:none}}
@media(max-width:760px){.step{grid-template-columns:1fr;gap:20px}.step.flip .step-art{order:0}.step h3{font:var(--type-h4)}}

/* stories */
.story{border:var(--border-hairline);border-left:3px solid var(--color-blue-400);border-radius:var(--radius-lg);background:#fff;
  padding:20px 22px;margin-top:14px;cursor:pointer;transition:border-color .15s,box-shadow .15s,transform .15s;box-shadow:var(--shadow-xs);outline:none}
.story:first-of-type{margin-top:30px}
.story:hover{border-left-color:var(--color-blue-700);box-shadow:var(--shadow-sm);transform:translateY(-1px)}
.story:focus-visible{box-shadow:var(--shadow-focus)}
.story.on{border-left-color:var(--color-violet-500);box-shadow:0 0 0 1px var(--color-violet-500),var(--shadow-sm)}
.story-line{font:var(--type-body-lg);line-height:1.5;color:var(--color-ink-700)}
.kw{font:var(--type-overline-sm);font-family:var(--font-mono);text-transform:uppercase;letter-spacing:.06em;color:var(--color-blue-600);padding:0 3px}
.acc{margin:14px 0 4px;padding:0;list-style:none;display:flex;flex-direction:column;gap:9px}
.acc li{font:var(--type-body-sm);color:var(--color-ink-400);display:flex;gap:10px;line-height:1.45}
.acc li svg{flex:0 0 auto;width:16px;height:16px;color:var(--color-blue-400);margin-top:1px}

/* outcomes */
.outcomes{display:grid;grid-template-columns:repeat(auto-fill,minmax(264px,1fr));gap:16px;margin-top:30px}
.outcome{border:var(--border-hairline);border-radius:var(--radius-lg);background:#fff;padding:26px;box-shadow:var(--shadow-sm)}
.outcome .oic{width:40px;height:40px;border-radius:var(--radius-md);background:var(--color-blue-100);color:var(--color-blue-700);display:grid;place-items:center;margin-bottom:16px}
.outcome .oic svg{width:21px;height:21px}
.outcome h3{font:var(--type-h5);color:var(--color-ink-700);margin:0 0 9px}
.outcome p{font:var(--type-body-sm);color:var(--color-ink-400);line-height:1.6;margin:0 0 12px}

/* ===== end mini-game ===== */
.game-card{margin-top:30px;border:var(--border-hairline);border-radius:var(--radius-xl);background:#fff;
  box-shadow:var(--shadow-md);overflow:hidden}
.game-head{padding:22px 26px;border-bottom:var(--border-hairline);background:linear-gradient(180deg,var(--color-blue-50),#fff);
  display:flex;align-items:center;gap:16px;flex-wrap:wrap}
.game-head .gtitle{font:var(--type-h4);color:var(--color-ink-700);margin:0}
.game-head .ginstr{font:var(--type-body-sm);color:var(--color-ink-400);margin:2px 0 0}
.game-head .sp{flex:1}
.game-score{display:flex;align-items:center;gap:16px;flex:0 0 auto}
.game-score .sc{text-align:center}
.game-score .sc b{display:block;font:var(--type-h4);font-family:var(--font-mono);color:var(--color-blue-700);line-height:1}
.game-score .sc.streak b{color:var(--color-violet-500)}
.game-score .sc span{font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.08em;color:var(--fg-tertiary)}
.game-body{padding:26px}
.game-progress{display:flex;gap:6px;margin-bottom:20px}
.game-progress i{flex:1;height:5px;border-radius:var(--radius-pill);background:var(--color-surface-3)}
.game-progress i.right{background:var(--color-teal-500)}
.game-progress i.wrong{background:#E2876F}
.game-progress i.cur{background:var(--color-blue-400)}
.round-stage{font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.12em;color:var(--color-blue-500);margin-bottom:8px}
.round-prompt{font:var(--type-h4);color:var(--color-ink-700);margin:0 0 16px;line-height:1.3;max-width:44ch}
.round-ctx{display:flex;flex-wrap:wrap;gap:8px;margin-bottom:22px}
.round-ctx .ctx{font:var(--type-body-sm);color:var(--color-ink-500);background:var(--color-surface-2);border:var(--border-hairline);
  border-radius:var(--radius-pill);padding:6px 13px;display:inline-flex;align-items:center;gap:7px}
.round-opts{display:flex;gap:12px;flex-wrap:wrap}
.opt-btn{flex:1 1 180px;text-align:left;border:var(--border-hairline);border-radius:var(--radius-lg);background:#fff;
  padding:16px 18px;font:var(--type-body);color:var(--color-ink-600);cursor:pointer;transition:border-color .15s,box-shadow .15s,transform .15s}
.opt-btn:hover:not(:disabled){border-color:var(--color-blue-400);box-shadow:var(--shadow-sm);transform:translateY(-1px)}
.opt-btn:disabled{cursor:default}
.opt-btn.right{border-color:var(--color-teal-500);background:rgba(0,200,187,.08);color:#06504a;box-shadow:0 0 0 1px var(--color-teal-500)}
.opt-btn.wrong{border-color:#E2876F;background:#FCF1EF;color:#9F3A24;box-shadow:0 0 0 1px #E2876F}
.opt-btn .ob-ic{display:inline-grid;place-items:center;width:20px;height:20px;border-radius:50%;margin-right:9px;vertical-align:-4px}
.opt-btn.right .ob-ic{background:var(--color-teal-500);color:#fff}
.opt-btn.wrong .ob-ic{background:#C0492F;color:#fff}
.opt-btn .ob-ic svg{width:11px;height:11px}
.round-feedback{margin-top:18px;border-radius:var(--radius-md);padding:14px 16px;font:var(--type-body-sm);line-height:1.55;display:none}
.round-feedback.show{display:block}
.round-feedback.good{background:rgba(0,200,187,.1);border:1px solid rgba(0,200,187,.3);color:#06504a}
.round-feedback.bad{background:#FCF1EF;border:1px solid #F2C8C0;color:#9F3A24}
.round-feedback b{font-weight:600}
.game-nav{display:flex;align-items:center;justify-content:space-between;gap:12px;margin-top:22px}
.game-final{text-align:center;padding:34px 26px}
.game-final .medal{width:74px;height:74px;border-radius:50%;margin:0 auto 16px;display:grid;place-items:center;color:#fff;
  background:radial-gradient(circle at 50% 35%,var(--color-teal-500),var(--color-blue-700));box-shadow:var(--shadow-md)}
.game-final .medal svg{width:34px;height:34px}
.game-final h3{font:var(--type-h3);color:var(--color-ink-700);margin:0 0 8px}
.game-final p{font:var(--type-body);color:var(--color-ink-400);margin:0 auto 18px;max-width:48ch}
.game-final .fscore{font:var(--type-h2);font-family:var(--font-mono);color:var(--color-blue-700)}

/* lightbox + toast */
.lightbox{position:fixed;inset:0;z-index:90;display:none;align-items:center;justify-content:center;padding:28px;
  background:rgba(2,9,21,.6);backdrop-filter:blur(4px);cursor:zoom-out}
.lightbox.open{display:flex}
.lightbox img{max-width:96vw;max-height:92vh;border-radius:var(--radius-md);box-shadow:var(--shadow-lg);background:#fff}
.lb-close{position:absolute;top:18px;right:20px;width:42px;height:42px;border-radius:50%;border:0;cursor:pointer;
  background:rgba(255,255,255,.92);color:var(--color-ink-700);font:400 24px/1 var(--font-body);display:grid;place-items:center;box-shadow:var(--shadow-sm)}
.lb-close:hover{background:#fff}
.toast{position:fixed;left:50%;bottom:28px;transform:translateX(-50%) translateY(20px);z-index:95;opacity:0;pointer-events:none;
  background:var(--color-ink-900);color:#fff;border-radius:var(--radius-pill);padding:13px 22px;box-shadow:var(--shadow-lg);
  font:var(--type-body-sm);font-weight:500;display:flex;align-items:center;gap:10px;transition:opacity .3s,transform .3s}
.toast.show{opacity:1;transform:translateX(-50%) translateY(0)}
.toast svg{width:18px;height:18px;color:var(--color-teal-500)}
.muted{color:var(--fg-tertiary)}
</style>
</head>
<body data-chaos-storyboard>
<div class="scrollbar" id="scrollbar"></div>

<div class="topbar">
  <div class="wrap">
    __BRAND_TOPBAR__
    <span class="crumb">__CRUMB__</span>
    <span class="sp"></span>
    <span class="pilltag">Feature guide</span>
  </div>
</div>

<div class="hud" id="hud">
  <div class="wrap">
    <span class="hud-label">Your progress</span>
    <span class="hud-count"><b id="hudCount">0</b> / <span id="hudTotal">0</span> steps</span>
    <div class="hud-pips" id="hudPips"></div>
    <span class="sp"></span>
    <div class="hud-bar"><div class="hud-fill" id="hudFill"></div></div>
  </div>
</div>

<header class="hero__HERO_CLASS__">
  <div class="wrap">
    <div class="hero-copy">
      <div class="eyebrow">__EYEBROW__</div>
      <h1>__TITLE__</h1>
      <p class="lede">__SUBTITLE__</p>
      <p class="audience" id="audience">__AUDIENCE__</p>
      <div class="hero-cta">
        <a class="btn-primary" href="#walkthrough">Start the walkthrough &darr;</a>
        <span class="hero-meta" id="heroMeta"></span>
      </div>
    </div>
    <div class="hero-aside">__HERO_ART__</div>
  </div>
</header>

<div class="nutshell">
  <div class="wrap">
    <div class="mark" aria-hidden="true"><svg viewBox="0 0 256 256" fill="none" stroke="currentColor" stroke-width="18" stroke-linecap="round" stroke-linejoin="round"><rect x="40" y="88" width="176" height="128" rx="12"/><path d="M88 88V56a40 40 0 0 1 80 0v32"/></svg></div>
    <p id="nutshell">__SUMMARY__</p>
  </div>
</div>

<main>
<section class="block reveal" data-chaos-personas>
  <div class="wrap">
    <div class="kicker">Who it's for</div>
    <h2 class="sec">The people and agents in this story</h2>
    <div id="personas" class="roles"></div>
    <div id="hierarchy"></div>
  </div>
</section>

<section class="block reveal" id="matrixSection" data-chaos-matrix hidden>
  <div class="wrap">
    <div class="kicker">At a glance</div>
    <h2 class="sec" id="matrixHeading">What each role can do</h2>
    <div id="matrix"></div>
  </div>
</section>

<section class="block reveal" id="calloutSection" data-chaos-callout hidden>
  <div class="wrap" id="calloutWrap"></div>
</section>

<section class="block reveal" id="walkthrough" data-chaos-frames data-chaos-journey>
  <div class="wrap">
    <div class="kicker">How it works <span class="hint">&mdash; scroll the journey; steps unlock as you reach them</span></div>
    <h2 class="sec">From start to finish, step by step</h2>
    <div class="journey">
      <nav class="rail" id="rail" aria-label="stages"></nav>
      <div id="steps" class="steps"></div>
    </div>
  </div>
</section>

<section class="block reveal" data-chaos-stories>
  <div class="wrap">
    <div class="kicker">User stories <span class="hint">&mdash; click to spotlight the steps that deliver it</span></div>
    <h2 class="sec">What each person wants</h2>
    <div id="stories"></div>
  </div>
</section>

<section class="block reveal" data-chaos-outcomes>
  <div class="wrap">
    <div class="kicker">Why it matters</div>
    <h2 class="sec">What success looks like</h2>
    <div id="outcomes" class="outcomes"></div>
  </div>
</section>

<section class="block reveal" id="gameSection" data-chaos-game hidden>
  <div class="wrap">
    <div class="kicker" id="gameKicker">Test yourself</div>
    <h2 class="sec" id="gameHeading">Did the rules stick?</h2>
    <p class="sec-intro" id="gameIntro"></p>
    <div id="game"></div>
  </div>
</section>
</main>

<footer>
  <div class="wrap">
    __BRAND_FOOTER__
    <span class="sp"></span>
    <span class="meta">Confidence values are estimates &middot; user perspective, no code &middot; generated by Chaos Substrate</span>
  </div>
</footer>

<div class="lightbox" id="lightbox" role="dialog" aria-modal="true" aria-label="Snapshot preview" aria-hidden="true"><button class="lb-close" id="lbClose" type="button" aria-label="Close preview">&times;</button><img id="lightboxImg" alt=""></div>
<div class="toast" id="toast" role="status"><svg viewBox="0 0 256 256" fill="currentColor"><path d="M128 24a104 104 0 1 0 104 104A104.11 104.11 0 0 0 128 24Zm45 87-56 56a8 8 0 0 1-11 0l-28-28a8 8 0 0 1 11-11l22 22 50-50a8 8 0 0 1 12 11Z"/></svg><span id="toastMsg">You explored the whole journey</span></div>
<script type="application/json" id="chaos-storyboard-manifest">__MANIFEST__</script>
<script>
document.documentElement.className+=" js";
(function(){
var M=JSON.parse(document.getElementById("chaos-storyboard-manifest").textContent);
var FRAMES=M.frames||[],STORIES=M.stories||[],PERSONAS=M.personas||[],OUTCOMES=M.outcomes||[];
var personaById={};PERSONAS.forEach(function(p){personaById[p.id]=p;});
function esc(v){return String(v==null?"":v).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;").replace(/'/g,"&#039;");}
function personaName(id){var p=personaById[id];return p?p.name:(id||"Someone");}

/* ---- icon set ---- */
var CK='<svg viewBox="0 0 256 256" fill="currentColor"><path d="M232.49 80.49l-128 128a12 12 0 0 1-17 0l-56-56a12 12 0 1 1 17-17L96 183 215.51 63.51a12 12 0 0 1 17 17Z"/></svg>';
var X='<svg viewBox="0 0 256 256" fill="currentColor"><path d="M205.66 194.34a8 8 0 0 1-11.32 11.32L128 139.31l-66.34 66.35a8 8 0 0 1-11.32-11.32L116.69 128 50.34 61.66a8 8 0 0 1 11.32-11.32L128 116.69l66.34-66.35a8 8 0 0 1 11.32 11.32L139.31 128Z"/></svg>';
var OIC='<svg viewBox="0 0 256 256" fill="currentColor"><path d="M128 24a104 104 0 1 0 104 104A104.11 104.11 0 0 0 128 24Zm45 87-56 56a8 8 0 0 1-11 0l-28-28a8 8 0 0 1 11-11l22 22 50-50a8 8 0 0 1 12 11Z"/></svg>';
var ICONS={
 eye:'<svg viewBox="0 0 256 256" fill="none" stroke="currentColor" stroke-width="18" stroke-linecap="round" stroke-linejoin="round"><path d="M24 128s40-72 104-72 104 72 104 72-40 72-104 72S24 128 24 128Z"/><circle cx="128" cy="128" r="32"/></svg>',
 file:'<svg viewBox="0 0 256 256" fill="none" stroke="currentColor" stroke-width="18" stroke-linecap="round" stroke-linejoin="round"><path d="M48 216V40a8 8 0 0 1 8-8h120l32 32v152a8 8 0 0 1-8 8H56a8 8 0 0 1-8-8Z"/><path d="M168 32v40h40"/></svg>',
 crown:'<svg viewBox="0 0 256 256" fill="none" stroke="currentColor" stroke-width="18" stroke-linecap="round" stroke-linejoin="round"><path d="M40 60l24 132h128l24-132-48 36-40-64-40 64Z"/></svg>',
 agent:'<svg viewBox="0 0 256 256" fill="currentColor"><path d="M128 24l40 64 64 40-64 40-40 64-40-64-64-40 64-40Z"/></svg>',
 key:'<svg viewBox="0 0 256 256" fill="none" stroke="currentColor" stroke-width="18" stroke-linecap="round" stroke-linejoin="round"><circle cx="92" cy="164" r="44"/><path d="M123 133 208 48M180 76l24 24M152 104l24 24"/></svg>',
 user:'<svg viewBox="0 0 256 256" fill="none" stroke="currentColor" stroke-width="18" stroke-linecap="round" stroke-linejoin="round"><circle cx="128" cy="96" r="56"/><path d="M24 216a112 112 0 0 1 208 0"/></svg>',
 users:'<svg viewBox="0 0 256 256" fill="none" stroke="currentColor" stroke-width="16" stroke-linecap="round" stroke-linejoin="round"><circle cx="100" cy="100" r="44"/><path d="M28 200a80 80 0 0 1 144 0M180 60a44 44 0 0 1 0 84M208 196a80 80 0 0 0-28-44"/></svg>',
 shield:'<svg viewBox="0 0 256 256" fill="none" stroke="currentColor" stroke-width="18" stroke-linecap="round" stroke-linejoin="round"><path d="M40 56l88-32 88 32v60c0 80-88 116-88 116S40 196 40 116Z"/></svg>',
 lock:'<svg viewBox="0 0 256 256" fill="none" stroke="currentColor" stroke-width="18" stroke-linecap="round" stroke-linejoin="round"><rect x="40" y="88" width="176" height="128" rx="12"/><path d="M88 88V56a40 40 0 0 1 80 0v32"/></svg>',
 clock:'<svg viewBox="0 0 256 256" fill="none" stroke="currentColor" stroke-width="18" stroke-linecap="round" stroke-linejoin="round"><circle cx="128" cy="128" r="96"/><path d="M128 72v56h56"/></svg>',
 bolt:'<svg viewBox="0 0 256 256" fill="currentColor"><path d="M215.79 118.17 88 232a8 8 0 0 1-13-8.39L102.18 152H40a8 8 0 0 1-6-13.28l128-144A8 8 0 0 1 176 8L148.4 96H208a8 8 0 0 1 7.79 22.17Z"/></svg>',
 doc:'<svg viewBox="0 0 256 256" fill="none" stroke="currentColor" stroke-width="18" stroke-linecap="round" stroke-linejoin="round"><path d="M48 216V40a8 8 0 0 1 8-8h120l32 32v152a8 8 0 0 1-8 8H56a8 8 0 0 1-8-8Z"/></svg>',
 grant:'<svg viewBox="0 0 256 256" fill="none" stroke="currentColor" stroke-width="16" stroke-linecap="round" stroke-linejoin="round"><circle cx="108" cy="100" r="48"/><path d="M28 208a92 92 0 0 1 160 0M200 88v48M224 112h-48"/></svg>',
 revoke:'<svg viewBox="0 0 256 256" fill="none" stroke="currentColor" stroke-width="18" stroke-linecap="round" stroke-linejoin="round"><circle cx="128" cy="128" r="96"/><path d="M60 60l136 136"/></svg>',
 flag:'<svg viewBox="0 0 256 256" fill="none" stroke="currentColor" stroke-width="18" stroke-linecap="round" stroke-linejoin="round"><path d="M40 224V40s24-16 56-16 56 24 88 24 40-8 40-8v112s-16 8-40 8-56-24-88-24-56 16-56 16Z"/></svg>',
 spark:'<svg viewBox="0 0 256 256" fill="currentColor"><path d="M208 144a15.78 15.78 0 0 1-10.42 14.94l-51.65 19-19 51.61a16 16 0 0 1-29.88 0l-19-51.65-51.61-19a16 16 0 0 1 0-29.88l51.65-19 19-51.61a16 16 0 0 1 29.88 0l19 51.65 51.61 19A15.78 15.78 0 0 1 208 144Z"/></svg>',
 image:'<svg viewBox="0 0 256 256" fill="none" stroke="currentColor" stroke-width="16" stroke-linecap="round" stroke-linejoin="round"><rect x="32" y="48" width="192" height="160" rx="12"/><circle cx="92" cy="104" r="20"/><path d="M40 184l54-54 56 56M150 154l34-34 42 42"/></svg>'
};
function icon(name){return ICONS[name]||"";}

/* ============ personas as role cards ============ */
var personaRoot=document.getElementById("personas");
PERSONAS.forEach(function(p){
 var el=document.createElement("div");
 var rc="role";if(p.icon==="agent")rc+=" role-agent";else if(Number(p.tier)>0)rc+=" role-t"+Math.max(1,Math.min(3,Number(p.tier)));
 el.className=rc;
 var ic=icon(p.icon);
 var mark=ic?ic:'<b>'+esc(((p.name||"?").trim().charAt(0)||"?").toUpperCase())+'</b>';
 el.innerHTML='<div class="ic" aria-hidden="true">'+mark+'</div>'
  +'<div><h3>'+esc(p.name)+'</h3>'+(p.who?'<p class="who">'+esc(p.who)+'</p>':"")+'</div>'
  +(p.goal?'<p>'+esc(p.goal)+'</p>':"")
  +(p.description?'<p class="muted">'+esc(p.description)+'</p>':"")
  +(p.includes?'<div class="incl">'+CK+' Includes '+esc(p.includes)+'</div>':'<div class="incl">Starting point</div>');
 personaRoot.appendChild(el);
});
if(!personaRoot.children.length)personaRoot.innerHTML='<div class="muted">No personas provided.</div>';

/* ---- role ladder from tiers ---- */
var ladder=PERSONAS.filter(function(p){return Number(p.tier)>0;}).sort(function(a,b){return Number(b.tier)-Number(a.tier);});
if(ladder.length>=2){
 var h=document.getElementById("hierarchy");
 var rows=ladder.map(function(p){return '<div class="lvl"><b>'+esc(p.name)+'</b>'+(p.who?'<small>&mdash; '+esc(p.who)+'</small>':"")+'</div>';}).join("");
 h.innerHTML='<div class="hier">'+rows+'</div><div class="hier-note"><svg viewBox="0 0 256 256" fill="currentColor"><path d="M128 24a104 104 0 1 0 104 104A104.11 104.11 0 0 0 128 24Zm12 152a12 12 0 1 1-12-12 12 12 0 0 1 12 12Zm-12-40a8 8 0 0 1-8-8V88a8 8 0 0 1 16 0v40a8 8 0 0 1-8 8Z"/></svg> A higher level always includes everything the levels beneath it can do.</div>';
}

/* ============ permission matrix ============ */
(function(){
 var mx=M.matrix;if(!mx||!mx.columns||!mx.rows)return;
 var sec=document.getElementById("matrixSection");sec.hidden=false;
 var cols=mx.columns;
 var head='<tr><th>What they can do</th>'+cols.map(function(c){return '<th class="col">'+esc(c)+'</th>';}).join("")+'</tr>';
 var body=mx.rows.map(function(r){
  var cells=cols.map(function(_,i){var ok=(r.allowed||[])[i];return '<td class="mk">'+(ok?'<span class="ck" role="img" aria-label="Allowed">'+CK+'</span>':'<span class="no" role="img" aria-label="Not allowed"></span>')+'</td>';}).join("");
  return '<tr><td class="cap">'+esc(r.capability)+'</td>'+cells+'</tr>';
 }).join("");
 document.getElementById("matrix").innerHTML='<div class="matrix-wrap"><table class="matrix"><thead>'+head+'</thead><tbody>'+body+'</tbody></table></div>'+(mx.caption?'<p class="matrix-cap">'+esc(mx.caption)+'</p>':"");
})();

/* ============ callout ============ */
(function(){
 var c=M.callout;if(!c||!c.title)return;
 var sec=document.getElementById("calloutSection");sec.hidden=false;
 var feats=(c.points||[]).map(function(t){return '<span class="feat">'+ICONS.spark+esc(t)+'</span>';}).join("");
 document.getElementById("calloutWrap").innerHTML=
  (c.kicker?'<div class="kicker">'+esc(c.kicker)+'</div>':"")
  +(c.heading?'<h2 class="sec">'+esc(c.heading)+'</h2>':"")
  +(c.intro?'<p class="sec-intro">'+esc(c.intro)+'</p>':"")
  +'<div class="callout"><div class="diamond" aria-hidden="true">'+ICONS.agent+'</div><div><h3>'+esc(c.title)+'</h3>'+(c.body?'<p>'+esc(c.body)+'</p>':"")+(feats?'<div class="feats">'+feats+'</div>':"")+'</div></div>';
})();

/* ============ walkthrough: scrollytelling journey ============ */
function previewHtml(p,label){
 if(!p||!p.kind)return "";
 if(p.kind==="image")return '<div class="preview-wrap"><span class="tagline">snapshot</span><img class="preview" tabindex="0" role="button" aria-label="Enlarge snapshot" src="'+esc(p.src)+'" alt="'+esc(p.alt||label||"UI snapshot")+'" loading="lazy" data-zoom="'+esc(p.src)+'">'+(p.caption?'<div class="preview-cap"><span>'+esc(p.caption)+'</span></div>':"")+'</div>';
 if(p.kind==="iframe")return '<div class="preview-wrap live"><span class="tagline">live</span><iframe class="preview-frame" title="'+esc(p.caption||label||"Live preview")+'" src="'+esc(p.url)+'" loading="lazy" referrerpolicy="no-referrer" sandbox="allow-scripts allow-same-origin allow-forms allow-popups"></iframe><div class="preview-cap"><span>'+esc(p.caption||"live embed &middot; needs the app running")+'</span><a href="'+esc(p.url)+'" target="_blank" rel="noopener noreferrer">open &#8599;</a></div></div>';
 return "";
}
function mockArt(f){
 var title=(f.ui_hint||f.title||f.stage||"Screen");
 var bar='<div class="mock-bar"><i></i><i></i><i></i><span>'+esc(title)+'</span></div>';
 if(f.preview&&f.preview.kind)return '<div class="mock">'+bar+previewHtml(f.preview,f.title)+'</div>';
 // No real UI supplied — Chaos can't synthesise the client's screens, so show an
 // honest placeholder that invites a real screenshot or live route instead of a
 // fake mock that doesn't match the product.
 return '<div class="mock">'+bar+'<div class="mock-empty"><span class="mock-empty-ic">'+ICONS.image+'</span><b>No preview yet</b><span>Add a screenshot of this screen — or point to a live route — via this step’s preview.</span></div></div>';
}

var stepRoot=document.getElementById("steps");
var railRoot=document.getElementById("rail");
var stages=[],byStage={};
FRAMES.forEach(function(f){var s=(f.stage||"").trim()||"Flow";if(!byStage[s]){byStage[s]=[];stages.push(s);}byStage[s].push(f);});
var stageIndex={};stages.forEach(function(s,i){stageIndex[s]=i;});
var stageEls={},stepEls={},stageDone={};
var n=0;
stages.forEach(function(s){
 var head=document.createElement("div");head.className="stage-head";head.setAttribute("data-stage",s);head.id="stage-"+stageIndex[s];
 head.innerHTML='<span class="stage-dot"></span><span class="stage-name">'+esc(s)+'</span><span class="stage-badge" data-badge="'+esc(s)+'">'+(byStage[s].length)+' steps</span>';
 stepRoot.appendChild(head);
 byStage[s].forEach(function(f){
  n++;var num=("0"+n).slice(-2);
  var art=mockArt(f);
  var copy='<div class="step-copy"><div class="step-num"><span class="num-badge">'+num+'</span></div><h3>'+esc(f.title)+'</h3>'+(f.ui_hint?'<div class="d-ui">'+esc(f.ui_hint)+'</div>':"")+'<p class="step-detail">'+esc(f.detail||f.summary||"")+'</p>'+(f.user_value?'<div class="d-sec">Why it matters</div><p class="step-why">'+esc(f.user_value)+'</p>':"")+(((f.persona_ids||[]).length)?'<div class="step-personas">'+(f.persona_ids||[]).map(function(pid){return '<span class="chip">'+esc(personaName(pid))+'</span>';}).join("")+'</div>':"")+'</div>';
  var artCol='<div class="step-art">'+art+'</div>';
  var step=document.createElement("article");
  step.className="step"+(n%2===0?" flip":"");
  step.setAttribute("data-frame-id",f.id);step.setAttribute("data-stage",s);step.setAttribute("data-index",n-1);
  step.innerHTML=(n%2===0?artCol+copy:copy+artCol);
  stepRoot.appendChild(step);
  stepEls[f.id]=step;
 });
 // rail entry — real fragment so it works without JS; smooth-scroll is enhancement
 var r=document.createElement("a");r.className="rail-stage";r.href="#stage-"+stageIndex[s];r.setAttribute("data-stage",s);
 r.innerHTML='<small>Stage '+(stageIndex[s]+1)+'</small>'+esc(s);
 r.addEventListener("click",function(ev){ev.preventDefault();var h=document.getElementById("stage-"+stageIndex[s]);if(h)h.scrollIntoView({behavior:"smooth",block:"start"});});
 railRoot.appendChild(r);
 stageEls[s]=r;stageDone[s]=false;
});
function cssEsc(v){return String(v).replace(/["\\]/g,"\\$&");}

/* zoomable snapshots — mouse + keyboard */
stepRoot.querySelectorAll("img.preview[data-zoom]").forEach(function(pv){
 function open(){openLb(pv.getAttribute("data-zoom"),pv.getAttribute("alt"),pv);}
 pv.addEventListener("click",open);
 pv.addEventListener("keydown",function(ev){if(ev.key==="Enter"||ev.key===" "){ev.preventDefault();open();}});
});

/* ============ gamified progress ============ */
var explored={},celebrated=false;
var total=FRAMES.length;
document.getElementById("hudTotal").textContent=total;
var hud=document.getElementById("hud");
// hud stage pips
var pipRoot=document.getElementById("hudPips");
stages.forEach(function(s){var pip=document.createElement("span");pip.className="hud-pip";pip.setAttribute("data-pip",s);pip.title=s;pipRoot.appendChild(pip);});
// hero meta
var roleCount=PERSONAS.length;
var mins=Math.max(1,Math.round(total*0.5));
document.getElementById("heroMeta").innerHTML='<b>'+total+'</b> steps &middot; <b>'+roleCount+'</b> roles &middot; ~<b>'+mins+'</b> min read';

function stageComplete(s){return byStage[s].every(function(f){return explored[f.id];});}
function refreshProgress(){
 var c=Object.keys(explored).length;
 var p=total?Math.round(c/total*100):0;
 document.getElementById("hudCount").textContent=c;
 document.getElementById("hudFill").style.width=p+"%";
 stages.forEach(function(s){
  if(!stageDone[s]&&stageComplete(s)){
   stageDone[s]=true;
   var badge=stepRoot.querySelector('.stage-badge[data-badge="'+cssEsc(s)+'"]');
   if(badge){badge.classList.add("done");badge.innerHTML=CK+' cleared';}
   if(stageEls[s])stageEls[s].classList.add("done");
   var pip=pipRoot.querySelector('.hud-pip[data-pip="'+cssEsc(s)+'"]');if(pip){pip.classList.remove("active");pip.classList.add("done");}
   if(!(c>=total))toast("Stage cleared: "+s);
  }
 });
 if(total&&c>=total&&!celebrated){celebrated=true;hud.classList.add("complete");celebrate();}
}
function markExplored(id){if(explored[id])return;explored[id]=1;var st=stepEls[id];if(st)st.classList.add("explored");refreshProgress();}

function toast(msg){var t=document.getElementById("toast");document.getElementById("toastMsg").textContent=msg;t.classList.add("show");clearTimeout(toast._t);toast._t=setTimeout(function(){t.classList.remove("show");},2600);}
function celebrate(){toast("You explored the whole journey — nice");var g=document.getElementById("gameSection");if(g&&!g.hidden){setTimeout(function(){toast("Mini-game unlocked below ↓");},2800);}}

/* ============ stories ============ */
var storyRoot=document.getElementById("stories");
function clearLinks(){stepRoot.querySelectorAll(".step.linked").forEach(function(s){s.classList.remove("linked");});}
STORIES.forEach(function(s){
 var el=document.createElement("div");el.className="story";el.setAttribute("role","button");el.setAttribute("tabindex","0");el.setAttribute("aria-pressed","false");
 var acc=(s.acceptance||[]).map(function(a){return '<li>'+CK+esc(a)+'</li>';}).join("");
 el.innerHTML='<div class="story-line"><span class="kw">As a</span> '+esc(personaName(s.persona_id))+' <span class="kw">I want</span> '+esc(s.want)+(s.benefit?' <span class="kw">so that</span> '+esc(s.benefit):"")+'</div>'+(acc?'<ul class="acc">'+acc+'</ul>':"");
 function go(){
  storyRoot.querySelectorAll(".story").forEach(function(o){o.classList.remove("on");o.setAttribute("aria-pressed","false");});el.classList.add("on");el.setAttribute("aria-pressed","true");
  clearLinks();var ids=(s.frame_ids||[]);var first=null;
  ids.forEach(function(id){var st=stepEls[id];if(st){st.classList.add("linked");if(!first)first=st;}});
  if(first)first.scrollIntoView({behavior:"smooth",block:"center"});
 }
 el.addEventListener("click",go);
 el.addEventListener("keydown",function(ev){if(ev.key==="Enter"||ev.key===" "){ev.preventDefault();go();}});
 storyRoot.appendChild(el);
});
if(!storyRoot.children.length)storyRoot.innerHTML='<div class="muted">No user stories provided.</div>';

/* ============ outcomes ============ */
var outcomeRoot=document.getElementById("outcomes");
OUTCOMES.forEach(function(o){var el=document.createElement("div");el.className="outcome";el.innerHTML='<div class="oic">'+OIC+'</div><h3>'+esc(o.title)+'</h3>'+(o.body?'<p>'+esc(o.body)+'</p>':"");outcomeRoot.appendChild(el);});
if(!outcomeRoot.children.length)outcomeRoot.innerHTML='<div class="muted">No outcomes provided.</div>';

/* ============ mini-game ============ */
(function(){
 var G=M.game;if(!G||!G.rounds||!G.rounds.length)return;
 var sec=document.getElementById("gameSection");sec.hidden=false;
 if(G.kicker)document.getElementById("gameKicker").textContent=G.kicker;
 if(G.heading)document.getElementById("gameHeading").textContent=G.heading;
 var intro=document.getElementById("gameIntro");if(G.intro)intro.textContent=G.intro;else intro.style.display="none";
 var rounds=G.rounds,i=0,score=0,streak=0,answered=false,results=[];
 var root=document.getElementById("game");
 function progressBar(){return '<div class="game-progress">'+rounds.map(function(_,k){var cls=k<results.length?(results[k]?"right":"wrong"):(k===i?"cur":"");return '<i class="'+cls+'"></i>';}).join("")+'</div>';}
 function render(){
  answered=false;var r=rounds[i];
  var ctx=(r.context||[]).map(function(t){return '<span class="ctx">'+ICONS.spark+esc(t)+'</span>';}).join("");
  var opts=r.options.map(function(o,k){return '<button class="opt-btn" data-k="'+k+'">'+esc(o.label)+'</button>';}).join("");
  root.innerHTML='<div class="game-card"><div class="game-head"><div><h3 class="gtitle">'+esc(G.heading||"Test yourself")+'</h3><p class="ginstr">'+esc(G.instructions||"Pick the right call. Instant feedback, no penalty for trying.")+'</p></div><span class="sp"></span><div class="game-score"><div class="sc"><b id="gScore">'+score+'</b><span>score</span></div><div class="sc streak"><b id="gStreak">'+streak+'</b><span>streak</span></div></div></div><div class="game-body">'+progressBar()+'<div class="round-stage">Round '+(i+1)+' of '+rounds.length+'</div><h3 class="round-prompt">'+esc(r.prompt)+'</h3>'+(ctx?'<div class="round-ctx">'+ctx+'</div>':"")+'<div class="round-opts">'+opts+'</div><div class="round-feedback" id="gFeed"></div><div class="game-nav"><span></span><button class="btn-primary" id="gNext" style="display:none">'+(i>=rounds.length-1?"See result":"Next round")+' &rarr;</button></div></div></div>';
  root.querySelectorAll(".opt-btn").forEach(function(b){b.addEventListener("click",function(){pick(parseInt(b.getAttribute("data-k"),10),b);});});
  var nx=document.getElementById("gNext");if(nx)nx.addEventListener("click",next);
 }
 function pick(k,btn){
  if(answered)return;answered=true;var r=rounds[i];var opt=r.options[k];var ok=!!opt.correct;
  results[i]=ok;
  if(ok){score+=10+streak*2;streak++;}else{streak=0;}
  root.querySelectorAll(".opt-btn").forEach(function(b){b.disabled=true;var ki=parseInt(b.getAttribute("data-k"),10);if(r.options[ki].correct)b.classList.add("right");});
  if(ok)btn.innerHTML='<span class="ob-ic">'+CK+'</span>'+esc(opt.label);
  else{btn.classList.add("wrong");btn.innerHTML='<span class="ob-ic">'+X+'</span>'+esc(opt.label);}
  var correctOpt=r.options.filter(function(o){return o.correct;})[0]||{};
  var feed=document.getElementById("gFeed");
  feed.className="round-feedback show "+(ok?"good":"bad");
  feed.innerHTML='<b>'+(ok?"Correct.":"Not quite.")+'</b> '+esc(opt.explain||correctOpt.explain||"");
  document.getElementById("gScore").textContent=score;document.getElementById("gStreak").textContent=streak;
  document.getElementById("gNext").style.display="inline-flex";
 }
 function next(){if(i>=rounds.length-1){finish();return;}i++;render();sec.scrollIntoView({behavior:"smooth",block:"start"});}
 function finish(){
  var right=results.filter(Boolean).length;
  var msg=G.win_message||"You've got the rules down.";
  root.innerHTML='<div class="game-card"><div class="game-final"><div class="medal">'+ICONS.spark+'</div><h3>'+esc(right===rounds.length?"Flawless run!":"Nicely done")+'</h3><div class="fscore">'+right+' / '+rounds.length+'</div><p>'+esc(msg)+'</p><div class="game-nav" style="justify-content:center"><button class="btn-ghost" id="gReplay">Play again</button></div></div></div>';
  document.getElementById("gReplay").addEventListener("click",function(){i=0;score=0;streak=0;results=[];render();sec.scrollIntoView({behavior:"smooth",block:"start"});});
  toast(right===rounds.length?"Perfect score — ✨":"Challenge complete");
 }
 render();
})();

/* hide empty audience / nutshell */
var aud=document.getElementById("audience");if(aud&&!aud.textContent.trim())aud.style.display="none";
var nut=document.getElementById("nutshell");if(nut&&!nut.textContent.trim()){var band=nut.closest(".nutshell");if(band)band.style.display="none";}

/* scroll progress bar + HUD reveal */
var sb=document.getElementById("scrollbar");var heroH=function(){var h=document.querySelector(".hero");return h?h.offsetHeight:300;};
function onScroll(){var h=document.documentElement.scrollHeight-window.innerHeight;var p=h>0?(window.scrollY/h)*100:0;sb.style.width=p+"%";
 if(window.scrollY>heroH()-80)hud.classList.add("show");else hud.classList.remove("show");}
window.addEventListener("scroll",onScroll,{passive:true});onScroll();

/* reveal blocks */
if("IntersectionObserver" in window){var io=new IntersectionObserver(function(ents){ents.forEach(function(e){if(e.isIntersecting){e.target.classList.add("in");io.unobserve(e.target);}});},{threshold:.12});
 document.querySelectorAll(".reveal").forEach(function(el){io.observe(el);});}
else document.querySelectorAll(".reveal").forEach(function(el){el.classList.add("in");});

/* step reveal + scroll-unlock + active stage + conf bars */
function setActiveStage(s){stages.forEach(function(name){var el=stageEls[name];if(el){var on=name===s;el.classList.toggle("active",on);if(on)el.setAttribute("aria-current","true");else el.removeAttribute("aria-current");}});var pip=pipRoot.querySelector('.hud-pip.active');if(pip)pip.classList.remove("active");var cur=pipRoot.querySelector('.hud-pip[data-pip="'+cssEsc(s)+'"]');if(cur&&!cur.classList.contains("done"))cur.classList.add("active");}
if("IntersectionObserver" in window){
 // A multi-stop threshold + a top-past-60%-viewport fallback so steps taller than
 // the viewport still get marked explored (a fixed 0.35 ratio is unreachable for them).
 var so=new IntersectionObserver(function(ents){ents.forEach(function(e){if(!e.isIntersecting)return;var st=e.target;st.classList.add("in");setActiveStage(st.getAttribute("data-stage"));if(e.intersectionRatio>=0.2||e.boundingClientRect.top<=window.innerHeight*0.6){markExplored(st.getAttribute("data-frame-id"));}});},{threshold:[0,0.2,0.55],rootMargin:"0px 0px -15% 0px"});
 Object.keys(stepEls).forEach(function(id){so.observe(stepEls[id]);});
}else{Object.keys(stepEls).forEach(function(id){stepEls[id].classList.add("in");markExplored(id);});}

/* lightbox + keyboard (dialog with focus management) */
var lb=document.getElementById("lightbox"),lbImg=document.getElementById("lightboxImg"),lbClose=document.getElementById("lbClose"),lbTrigger=null;
function openLb(src,alt,trigger){if(!src)return;lbTrigger=trigger||null;lbImg.src=src;lbImg.alt=alt||"Enlarged snapshot";lb.classList.add("open");lb.setAttribute("aria-hidden","false");if(lbClose)lbClose.focus();}
function closeLb(){if(!lb.classList.contains("open"))return;lb.classList.remove("open");lb.setAttribute("aria-hidden","true");lbImg.src="";lbImg.alt="";if(lbTrigger&&lbTrigger.focus)lbTrigger.focus();lbTrigger=null;}
lb.addEventListener("click",function(ev){if(ev.target===lb)closeLb();});
if(lbClose)lbClose.addEventListener("click",closeLb);
document.addEventListener("keydown",function(ev){if(ev.key==="Escape")closeLb();});

refreshProgress();
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
            hero_image: String::new(),
            personas: vec![Persona {
                id: "member".into(),
                name: "Member".into(),
                description: "A signed-in user with documents to protect.".into(),
                goal: "Store a document safely and share it later.".into(),
                who: "Anyone with files to keep".into(),
                icon: "user".into(),
                includes: String::new(),
                tier: 0,
            }],
            matrix: None,
            callout: None,
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
            game: None,
            brand: Brand::default(),
            brand_preset: String::new(),
        }
    }

    /// `sample()` enriched with every optional Access-Control-style field, used
    /// to exercise the matrix / callout / game / hero-image rendering paths.
    fn sample_rich() -> StoryboardManifest {
        let mut m = sample();
        m.hero_image = "assets/keyvis.jpg".into();
        m.personas[0].includes = "Viewer".into();
        m.personas[0].tier = 2;
        m.personas.push(Persona {
            id: "viewer".into(),
            name: "Viewer".into(),
            who: "Reviewers".into(),
            icon: "eye".into(),
            tier: 1,
            ..Default::default()
        });
        m.matrix = Some(Matrix {
            columns: vec!["Viewer".into(), "Member".into()],
            rows: vec![
                MatrixRow {
                    capability: "Read files".into(),
                    allowed: vec![true, true],
                },
                MatrixRow {
                    capability: "Upload files".into(),
                    allowed: vec![false, true],
                },
            ],
            caption: "Higher roles include the lower ones.".into(),
        });
        m.callout = Some(Callout {
            kicker: "AI agents".into(),
            heading: "Bring in agents".into(),
            intro: "Agents get a role too.".into(),
            title: "Time-boxed".into(),
            body: "They expire on their own.".into(),
            points: vec!["Expires".into(), "Labelled".into()],
        });
        m.game = Some(Game {
            kicker: "Test yourself".into(),
            heading: "Allow or deny?".into(),
            intro: "Judge each request.".into(),
            instructions: "Pick the right call.".into(),
            rounds: vec![GameRound {
                prompt: "An expired agent opens a sealed file.".into(),
                context: vec!["Status: Expired".into()],
                options: vec![
                    GameOption {
                        label: "Allow".into(),
                        correct: false,
                        explain: "Expired grants are inactive.".into(),
                    },
                    GameOption {
                        label: "Deny".into(),
                        correct: true,
                        explain: "No key is released after expiry.".into(),
                    },
                ],
            }],
            win_message: "You know the rules.".into(),
        });
        m
    }

    #[test]
    fn rendered_storyboard_embeds_manifest_and_markers() {
        let html = render_storyboard_html(&sample()).unwrap();
        assert!(html.contains(r#"id="chaos-storyboard-manifest""#));
        assert!(html.contains("data-chaos-storyboard"));
        assert!(html.contains("data-frame-id"));
        assert!(html.contains("data-chaos-personas"));
        // The walkthrough is now a scrollytelling journey (no sticky detail pane).
        assert!(html.contains("data-chaos-journey"));
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
    fn rendered_storyboard_uses_light_theme_and_brand_placeholder() {
        let html = render_storyboard_html(&sample()).unwrap();
        // The shared design system is injected (light editorial chrome).
        assert!(html.contains("--color-blue-700"));
        assert!(html.contains("class=\"topbar\""));
        assert!(html.contains("class=\"hero\""));
        assert!(html.contains("class=\"nutshell\""));
        // No brand supplied -> the "Add your logo" placeholder invites one.
        // (Match the rendered class attribute, not the `.brand-placeholder` CSS.)
        assert!(html.contains(r#"class="brand brand-placeholder""#));
        assert!(html.contains("Add your logo"));
        // Every template placeholder was substituted.
        for token in [
            "__THEME__",
            "__TITLE__",
            "__SUBTITLE__",
            "__AUDIENCE__",
            "__EYEBROW__",
            "__CRUMB__",
            "__SUMMARY__",
            "__HERO_CLASS__",
            "__HERO_ART__",
            "__BRAND_TOPBAR__",
            "__BRAND_FOOTER__",
            "__MANIFEST__",
        ] {
            assert!(!html.contains(token), "unsubstituted placeholder {token}");
        }
        // The pilltag and page title read "Feature guide", not "storyboard".
        assert!(html.contains(r#"<span class="pilltag">Feature guide</span>"#));
        assert!(html.contains("&middot; Feature guide</title>"));
    }

    #[test]
    fn rendered_storyboard_renders_supplied_brand() {
        let mut manifest = sample();
        manifest.brand = Brand {
            name: "Acme Labs".into(),
            logo_src: "assets/acme.svg".into(),
            tagline: "Engineering docs".into(),
            href: "https://acme.example".into(),
        };
        let html = render_storyboard_html(&manifest).unwrap();
        assert!(html.contains("assets/acme.svg"));
        assert!(html.contains("brand-link"));
        // The placeholder text/markup must be gone when a brand is supplied.
        assert!(!html.contains(r#"class="brand brand-placeholder""#));
        assert!(!html.contains("Add your logo"));
        // Brand round-trips through the embedded manifest too.
        let marker = r#"id="chaos-storyboard-manifest">"#;
        let start = html.find(marker).unwrap() + marker.len();
        let end = html[start..].find("</script>").unwrap();
        let back: StoryboardManifest =
            serde_json::from_str(html[start..start + end].trim()).unwrap();
        assert_eq!(back.brand.name, "Acme Labs");
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
    fn validate_accepts_rich_sample() {
        validate_storyboard(&sample_rich()).unwrap();
    }

    #[test]
    fn brand_preset_molecule_fills_brand_and_hero_and_ships_in_binary() {
        let mut m = sample();
        m.brand = Brand::default();
        m.hero_image = String::new();
        m.brand_preset = "molecule".into();
        apply_brand_preset(&mut m).unwrap();
        assert_eq!(m.brand.name, "Molecule Labs");
        assert!(m.brand.logo_src.starts_with("data:image/svg+xml"));
        assert!(m.hero_image.starts_with("data:image/webp"));
        // ...and it renders branded (no placeholder), proving the preset is
        // embedded and usable with zero machine-local files.
        let html = render_storyboard_html(&m).unwrap();
        assert!(!html.contains("Add your logo"));
        assert!(html.contains("Molecule Labs"));
    }

    #[test]
    fn brand_preset_does_not_override_explicit_fields() {
        let mut m = sample();
        m.brand = Brand {
            name: "Acme".into(),
            ..Default::default()
        };
        m.brand_preset = "molecule".into();
        apply_brand_preset(&mut m).unwrap();
        assert_eq!(m.brand.name, "Acme"); // explicit value wins
        assert!(m.brand.logo_src.starts_with("data:image/svg+xml")); // empty filled from preset
    }

    #[test]
    fn unknown_brand_preset_is_an_error() {
        let mut m = sample();
        m.brand_preset = "nope".into();
        assert!(apply_brand_preset(&mut m).is_err());
    }

    #[test]
    fn rendered_storyboard_embeds_rich_sections() {
        let html = render_storyboard_html(&sample_rich()).unwrap();
        // Hero key-visual + its overlay-able hero class are rendered.
        assert!(html.contains("class=\"hero-art\""));
        assert!(html.contains("assets/keyvis.jpg"));
        assert!(html.contains("class=\"hero has-art\""));
        // The optional sections exist in the chrome and have renderers.
        assert!(html.contains("data-chaos-matrix"));
        assert!(html.contains("data-chaos-callout"));
        assert!(html.contains("data-chaos-game"));
        // The data round-trips so the client renderer can mount it.
        let marker = r#"id="chaos-storyboard-manifest">"#;
        let start = html.find(marker).unwrap() + marker.len();
        let end = html[start..].find("</script>").unwrap();
        let back: StoryboardManifest =
            serde_json::from_str(html[start..start + end].trim()).unwrap();
        assert!(back.matrix.is_some());
        assert!(back.callout.is_some());
        assert_eq!(back.game.as_ref().unwrap().rounds.len(), 1);
        assert_eq!(back.personas[0].tier, 2);
    }

    #[test]
    fn validate_rejects_bad_hero_image_scheme() {
        let mut manifest = sample();
        manifest.hero_image = "javascript:alert(1)".into();
        assert!(validate_storyboard(&manifest).is_err());
        // A data:image URI is allowed.
        manifest.hero_image = "data:image/png;base64,AAAA".into();
        validate_storyboard(&manifest).unwrap();
    }

    #[test]
    fn validate_rejects_empty_matrix_columns() {
        let mut manifest = sample_rich();
        manifest.matrix.as_mut().unwrap().columns.clear();
        assert!(validate_storyboard(&manifest).is_err());
    }

    #[test]
    fn validate_rejects_callout_without_title() {
        let mut manifest = sample_rich();
        manifest.callout.as_mut().unwrap().title = "  ".into();
        assert!(validate_storyboard(&manifest).is_err());
    }

    #[test]
    fn validate_rejects_game_round_without_correct_option() {
        let mut manifest = sample_rich();
        for opt in &mut manifest.game.as_mut().unwrap().rounds[0].options {
            opt.correct = false;
        }
        assert!(validate_storyboard(&manifest).is_err());
    }

    #[test]
    fn validate_rejects_game_round_with_one_option() {
        let mut manifest = sample_rich();
        manifest.game.as_mut().unwrap().rounds[0]
            .options
            .truncate(1);
        assert!(validate_storyboard(&manifest).is_err());
    }

    #[test]
    fn validate_rejects_duplicate_frame_id() {
        let mut manifest = sample();
        let dup = manifest.frames[0].id.clone();
        manifest.frames[1].id = dup;
        assert!(validate_storyboard(&manifest).is_err());
    }

    #[test]
    fn validate_rejects_matrix_row_width_mismatch() {
        let mut manifest = sample_rich();
        // 2 columns but a row with 3 flags.
        manifest.matrix.as_mut().unwrap().rows[0].allowed = vec![true, false, true];
        assert!(validate_storyboard(&manifest).is_err());
    }

    #[test]
    fn validate_rejects_non_http_iframe_url() {
        let mut manifest = sample();
        // A script-capable scheme that the old denylist let through.
        manifest.frames[1].preview = Some(FramePreview::Iframe {
            url: "data:image/svg+xml,<svg/onload=alert(1)>".into(),
            caption: String::new(),
        });
        assert!(validate_storyboard(&manifest).is_err());
        // ...and a newline can't smuggle a scheme past the check either.
        manifest.frames[1].preview = Some(FramePreview::Iframe {
            url: "java\nscript:alert(1)".into(),
            caption: String::new(),
        });
        assert!(validate_storyboard(&manifest).is_err());
    }

    #[test]
    fn validate_accepts_relative_iframe_url() {
        let mut manifest = sample();
        manifest.frames[1].preview = Some(FramePreview::Iframe {
            url: "/app/route?state=ready".into(),
            caption: String::new(),
        });
        validate_storyboard(&manifest).unwrap();
    }

    #[test]
    fn template_fill_survives_token_in_dynamic_value() {
        // A subtitle that literally contains a placeholder token must not be
        // re-expanded by a later substitution.
        let mut manifest = sample();
        manifest.subtitle = "before __MANIFEST__ after".into();
        let html = render_storyboard_html(&manifest).unwrap();
        assert!(html.contains(r#"<p class="lede">before __MANIFEST__ after</p>"#));
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
