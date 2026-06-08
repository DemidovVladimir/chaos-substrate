//! Shared light **editorial** design system for every Chaos-generated HTML page.
//!
//! Ported from a Figma-derived design system and deliberately **de-branded** so
//! the generated pages can be used by any project, not one company: there are no
//! third-party logos, no licensed fonts bundled, and no vendor names. The colour
//! tokens, spacing scale, radii, shadows, and base resets live in [`THEME_CSS`];
//! the cross-page chrome (page wrap, sticky topbar with a **brand slot**, hero,
//! nutshell band, section scaffold, footer) lives there too so each page only
//! adds its own component CSS.
//!
//! Fonts are **never bundled** (one display family in the source is licensed):
//! the stacks name a preferred family first and fall back to Inter / the system
//! UI font and a system monospace, so nothing is downloaded and nothing is
//! redistributed.
//!
//! Branding is supplied per page via [`Brand`]; [`render_brand`] turns it into a
//! logo mark, falling back to a visible "Add your logo" placeholder that invites
//! the user to drop in their own company details. The **default** stays
//! de-branded; for convenience, named **brand presets** (see [`BrandPreset`] /
//! [`brand_preset`]) ship embedded in the binary so a page can opt in by name
//! (e.g. `brand_preset: "molecule"`) without any machine-local files. Presets are
//! the one place a vendor logo lives, and only when explicitly requested.

use serde::{Deserialize, Serialize};

/// Optional, code-free branding for a generated page. All fields default to
/// empty; an empty [`Brand`] renders a neutral "Add your logo" placeholder.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Brand {
    /// Company / project name shown as a text wordmark when no `logo_src` is
    /// given, and as the logo's alt text when one is.
    #[serde(default)]
    pub name: String,
    /// Short tagline shown beside the wordmark and in the footer meta line.
    #[serde(default)]
    pub tagline: String,
    /// Logo image: an `http(s)` URL, a relative path, or a `data:image/...`
    /// URI. Active-content schemes are rejected at render time.
    #[serde(default)]
    pub logo_src: String,
    /// Optional link target for the logo (defaults to no link).
    #[serde(default)]
    pub href: String,
}

impl Brand {
    /// True when the brand carries nothing renderable — drives the placeholder.
    pub fn is_empty(&self) -> bool {
        self.name.trim().is_empty()
            && self.tagline.trim().is_empty()
            && self.logo_src.trim().is_empty()
    }
}

/// A named, **shipped** brand preset: the [`Brand`] fields plus an optional
/// default hero image. Presets are embedded in the binary (`include_str!`) so
/// they are available to every install with no machine-local files — a page can
/// be branded by name (`brand_preset: "molecule"`) instead of inlining assets.
/// The core theme stays de-branded; presets are opt-in.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BrandPreset {
    #[serde(flatten)]
    pub brand: Brand,
    /// Default hero key-visual for the preset (image `src`: usually an inline
    /// `data:` URI so the preset needs no sidecar files).
    #[serde(default)]
    pub hero_image: String,
}

// Embedded preset payloads. Adding a brand preset = drop a JSON next to these
// and wire one match arm in `brand_preset`; it then ships inside the binary.
const PRESET_MOLECULE: &str = include_str!("../assets/brand-presets/molecule.json");

/// Look up a brand preset shipped with Chaos by name (case-insensitive).
/// Returns `Some(preset)` for a known name, `None` otherwise (so callers can
/// report an unknown preset rather than silently ignoring it).
pub fn brand_preset(name: &str) -> Option<BrandPreset> {
    let payload = match name.trim().to_ascii_lowercase().as_str() {
        "molecule" | "molecule-labs" | "molecule_labs" => PRESET_MOLECULE,
        _ => return None,
    };
    serde_json::from_str(payload).ok()
}

/// Names of every shipped brand preset (for help text / error messages).
pub const BRAND_PRESET_NAMES: &[&str] = &["molecule"];

/// Reject `logo_src` values that would smuggle active content into an HTML
/// attribute. Mirrors the storyboard preview check: static pages only, so only
/// `http(s)`, relative paths, and `data:image/...` are allowed.
pub fn brand_logo_src_ok(src: &str) -> bool {
    // Strip whitespace/control chars first so a scheme can't be smuggled past the
    // prefix check with embedded newlines (e.g. `java\nscript:`).
    let lower: String = src
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .flat_map(|c| c.to_lowercase())
        .collect();
    !(lower.starts_with("javascript:")
        || lower.starts_with("vbscript:")
        || (lower.starts_with("data:") && !lower.starts_with("data:image/")))
}

/// HTML-escape for text rendered into element bodies and double-quoted
/// attributes.
fn esc(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#039;")
}

/// A neutral, vendor-free mark used when a brand has a name but no logo image —
/// three linked nodes, echoing the "substrate / graph" idea without naming it.
const DEFAULT_MARK_SVG: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="5" cy="6" r="2.4"/><circle cx="19" cy="9" r="2.4"/><circle cx="11" cy="18" r="2.4"/><path d="M7 7 17 8.4M16.8 11 12.4 15.8M9 16 6 8.3"/></svg>"#;

/// Render the brand block for the topbar / footer. `context` is `"topbar"` or
/// `"footer"` (only used for the placeholder copy). When `brand` is empty a
/// dashed "Add your logo" slot is rendered so the gap is an explicit invitation
/// rather than a silent blank.
pub fn render_brand(brand: &Brand, context: &str) -> String {
    if brand.is_empty() {
        let hint = if context == "footer" {
            "Add your company"
        } else {
            "Add your logo"
        };
        return format!(
            r#"<span class="brand brand-placeholder" title="Pass a `brand` object (name, logo_src, tagline) to label this page">{mark}<span class="brand-text">{hint}</span></span>"#,
            mark = DEFAULT_MARK_SVG,
            hint = esc(hint),
        );
    }

    let logo = if !brand.logo_src.trim().is_empty() && brand_logo_src_ok(&brand.logo_src) {
        format!(
            r#"<img class="brand-img" src="{src}" alt="{alt}">"#,
            src = esc(brand.logo_src.trim()),
            alt = esc(if brand.name.trim().is_empty() {
                "logo"
            } else {
                brand.name.trim()
            }),
        )
    } else {
        // Name-only (or rejected logo src): mark + wordmark text.
        format!(
            r#"{mark}<span class="brand-text">{name}</span>"#,
            mark = DEFAULT_MARK_SVG,
            name = esc(if brand.name.trim().is_empty() {
                "Untitled"
            } else {
                brand.name.trim()
            }),
        )
    };

    let inner = format!(r#"<span class="brand">{logo}</span>"#);
    if brand.href.trim().is_empty() || !brand_logo_src_ok(&brand.href) {
        inner
    } else {
        format!(
            r#"<a class="brand-link" href="{href}" rel="noopener noreferrer">{inner}</a>"#,
            href = esc(brand.href.trim()),
        )
    }
}

/// The shared, de-branded design system: colour/spacing/type tokens, base
/// element resets, and the cross-page chrome (`.wrap`, `.topbar`, `.hero`,
/// `.nutshell`, `section.block`, `footer`). Pages embed this once, then add
/// their own component CSS. No `@font-face` / `@import`: fonts fall back to
/// Inter / system stacks so nothing is downloaded or redistributed.
pub const THEME_CSS: &str = r#":root{
  /* neutrals (warm near-blacks & cool greys) */
  --color-ink-900:rgb(2,9,21);--color-ink-800:rgb(7,20,31);--color-ink-700:rgb(15,39,63);
  --color-ink-600:rgb(18,31,47);--color-ink-500:rgb(30,50,74);--color-ink-400:rgb(43,69,101);
  --color-ink-300:rgb(68,117,154);--color-ink-200:rgb(127,138,152);--color-ink-100:rgb(180,190,205);
  /* borders + surfaces */
  --color-border:rgb(213,220,230);--color-border-soft:rgb(238,242,246);
  --color-surface-0:rgb(255,255,255);--color-surface-1:rgb(249,249,249);
  --color-surface-2:rgb(244,249,252);--color-surface-3:rgb(238,242,246);
  /* accent blues */
  --color-blue-50:rgb(244,248,253);--color-blue-100:rgb(233,241,250);--color-blue-150:rgb(223,238,255);
  --color-blue-200:rgb(221,233,248);--color-blue-300:rgb(193,219,250);--color-blue-400:rgb(132,178,233);
  --color-blue-500:rgb(130,165,206);--color-blue-600:rgb(68,117,154);--color-blue-700:rgb(27,88,156);
  --color-blue-800:rgb(13,69,113);--color-blue-900:rgb(3,22,49);
  /* secondary accents */
  --color-purple-500:rgb(112,70,198);--color-purple-100:rgb(241,234,255);
  --color-teal-500:rgb(0,200,187);--color-magenta-500:rgb(252,65,255);--color-violet-500:rgb(128,68,255);
  /* semantic */
  --fg-primary:var(--color-ink-600);--fg-secondary:var(--color-ink-400);--fg-tertiary:var(--color-ink-200);
  --fg-muted:var(--color-ink-100);--fg-accent:var(--color-blue-400);--fg-on-dark:var(--color-surface-0);
  --bg-page:var(--color-surface-0);--bg-surface:var(--color-surface-1);--bg-quiet:var(--color-surface-2);--bg-chip:var(--color-surface-3);
  --bg-sky-soft:linear-gradient(180deg,rgb(233,241,250) 0%,rgb(223,238,255) 100%);
  --bg-diamond:radial-gradient(circle,rgb(252,65,255) 0%,rgb(128,68,255) 49%,rgba(128,68,255,0) 100%);
  /* borders / radii */
  --border-hairline:1px solid var(--color-border);--border-soft:1px solid var(--color-border-soft);--border-accent:1px solid var(--color-blue-400);
  --radius-xs:4px;--radius-sm:8px;--radius-md:12px;--radius-lg:16px;--radius-xl:24px;--radius-pill:999px;
  /* spacing (4px base) */
  --space-1:4px;--space-2:8px;--space-3:12px;--space-4:16px;--space-5:24px;--space-6:32px;--space-7:48px;--space-8:64px;--space-9:80px;--space-10:128px;
  /* shadows (subtle, warm tint) */
  --shadow-xs:0 1px 2px rgba(96,0,228,.03);--shadow-sm:0 2px 8px rgba(96,0,228,.06);
  --shadow-md:0 8px 24px rgba(96,0,228,.06),0 1px 2px rgba(23,219,207,.02);
  --shadow-lg:0 20px 40px rgba(2,9,21,.08),0 2px 8px rgba(96,0,228,.06);
  --shadow-focus:0 0 0 3px rgba(132,178,233,.35);
  /* type families — no bundled fonts; first names are preferences only */
  --font-display:"Aeonik","Inter",-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;
  --font-body:"Geist","Inter",-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;
  --font-mono:"Geist Mono",ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;
  /* type scale */
  --type-display-xl:500 56px/1.1 var(--font-display);--type-display-lg:500 40px/1.2 var(--font-display);
  --type-h1:500 40px/1.2 var(--font-display);--type-h2:500 32px/1.2 var(--font-display);
  --type-h3:500 24px/1.2 var(--font-display);--type-h4:500 20px/1.2 var(--font-display);
  --type-h5:500 16px/1.3 var(--font-display);--type-h6:500 14px/1.3 var(--font-display);
  --type-body-lg:400 18px/1.4 var(--font-body);--type-body:400 16px/1.4 var(--font-body);
  --type-body-sm:400 14px/1.4 var(--font-body);--type-body-xs:400 12px/1.4 var(--font-body);
  --type-overline:500 14px/1.4 var(--font-mono);--type-overline-sm:500 12px/1.3 var(--font-mono);
  --type-label:500 12px/14px var(--font-body);
}
*{box-sizing:border-box}
html,body{margin:0;background:var(--bg-page);color:var(--fg-primary);font:var(--type-body);
  -webkit-font-smoothing:antialiased;-moz-osx-font-smoothing:grayscale}
h1{font:var(--type-h1);margin:0}h2{font:var(--type-h2);margin:0}h3{font:var(--type-h3);margin:0}
h4{font:var(--type-h4);margin:0}h5{font:var(--type-h5);margin:0}h6{font:var(--type-h6);margin:0}
p{font:var(--type-body);color:var(--fg-secondary);margin:0}
::selection{background:var(--color-blue-150)}
a{color:var(--fg-primary);text-decoration:underline;text-underline-offset:2px;text-decoration-thickness:1px}
a:hover{color:var(--color-blue-700)}
.mono{font-family:var(--font-mono)}
.overline{font:var(--type-overline);text-transform:uppercase;letter-spacing:.03em;color:var(--color-blue-500)}
code,kbd,samp{font:var(--type-body-sm);font-family:var(--font-mono);background:var(--bg-chip);padding:2px 6px;border-radius:var(--radius-xs)}

/* ---------- shared chrome ---------- */
.wrap{max-width:1040px;margin:0 auto;padding:0 32px}
.wrap.wide{max-width:1240px}

/* brand slot */
.brand{display:inline-flex;align-items:center;gap:9px;color:var(--color-ink-600);text-decoration:none}
.brand svg{width:22px;height:22px;display:block;color:var(--color-blue-700)}
.brand-img{height:24px;width:auto;display:block}
.brand-text{font:var(--type-h6);font-weight:500;color:var(--color-ink-700);letter-spacing:-.01em}
.brand-link{text-decoration:none}
.brand-placeholder{border:1px dashed var(--color-blue-300);border-radius:var(--radius-pill);
  padding:5px 13px 5px 11px;color:var(--color-blue-600)}
.brand-placeholder .brand-text{font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.08em;color:var(--color-blue-600)}
.brand-placeholder svg{width:16px;height:16px}

/* topbar */
.topbar{position:sticky;top:0;z-index:30;background:rgba(255,255,255,.86);
  backdrop-filter:saturate(1.2) blur(8px);border-bottom:var(--border-hairline)}
.topbar .wrap{display:flex;align-items:center;gap:16px;height:64px}
.topbar .crumb{font:var(--type-body-sm);color:var(--fg-tertiary);display:flex;gap:8px;align-items:center;white-space:nowrap}
.topbar .crumb b{color:var(--fg-secondary);font-weight:500}
.topbar .crumb span.sep{color:var(--color-ink-100)}
.topbar .sp{flex:1}
.topbar .pilltag{font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.08em;white-space:nowrap;
  color:var(--color-blue-700);background:var(--color-blue-100);padding:6px 12px;border-radius:var(--radius-pill)}
@media(max-width:620px){.topbar .crumb{display:none}}

/* scroll progress (sits under the topbar) */
.scrollbar{position:fixed;top:0;left:0;height:3px;width:0;z-index:40;
  background:linear-gradient(90deg,var(--color-blue-400),var(--color-blue-700));transition:width .12s linear}

/* hero */
.hero{background:var(--bg-sky-soft);border-bottom:var(--border-hairline);overflow:hidden;position:relative}
.hero .wrap{display:grid;grid-template-columns:1.15fr .85fr;gap:32px;align-items:center;padding:60px 32px}
.eyebrow{font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.16em;color:var(--color-blue-700);
  margin-bottom:18px;display:flex;align-items:center;gap:10px}
.eyebrow::before{content:"";width:22px;height:1px;background:var(--color-blue-500);display:inline-block}
.hero h1{font:var(--type-display-xl);letter-spacing:-.01em;color:var(--color-ink-700);margin:0 0 20px}
.hero .lede{font:var(--type-body-lg);color:var(--color-ink-500);max-width:42ch;line-height:1.5}
.hero .audience{margin-top:18px;font:var(--type-body-sm);color:var(--color-ink-400);max-width:46ch;line-height:1.5}
@media(max-width:860px){.hero .wrap{grid-template-columns:1fr;padding:44px 32px;gap:24px}.hero h1{font:var(--type-display-lg)}}

/* nutshell band */
.nutshell{background:var(--color-surface-2);border-bottom:var(--border-hairline)}
.nutshell .wrap{padding:28px 32px;display:flex;gap:20px;align-items:flex-start}
.nutshell .mark{flex:0 0 auto;width:40px;height:40px;border-radius:var(--radius-md);background:var(--color-ink-600);
  color:#fff;display:grid;place-items:center}
.nutshell .mark svg{width:22px;height:22px}
.nutshell p{font:var(--type-body-lg);color:var(--color-ink-600);line-height:1.55;margin:0}

/* section scaffold */
section.block{padding:64px 0;border-bottom:var(--border-hairline)}
section.block:last-of-type{border-bottom:0}
.kicker{font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.14em;color:var(--color-blue-500);
  margin-bottom:14px;display:flex;align-items:center;gap:12px}
.kicker .hint{color:var(--fg-tertiary);letter-spacing:.06em;text-transform:none;font-family:var(--font-body);font-size:12px}
h2.sec{font:var(--type-h2);letter-spacing:-.01em;color:var(--color-ink-700);margin:0 0 16px;max-width:24ch}
.sec-intro{font:var(--type-body-lg);color:var(--color-ink-400);max-width:64ch;line-height:1.55}

/* reveal-on-scroll (gated on .js so it never hides content when JS is off) */
.js .reveal{opacity:0;transform:translateY(14px);transition:opacity .6s ease,transform .6s ease}
.js .reveal.in{opacity:1;transform:none}
/* visually-hidden text for screen readers */
.sr-only{position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip:rect(0,0,0,0);white-space:nowrap;border:0}
/* honour reduced-motion across every generated animation/transition */
@media(prefers-reduced-motion:reduce){.js .reveal{opacity:1;transform:none}
  .js *,.js *::before,.js *::after{animation-duration:.01ms!important;animation-iteration-count:1!important;transition-duration:.01ms!important;scroll-behavior:auto!important}}

/* footer */
footer{background:var(--color-ink-900);color:var(--color-blue-100);padding:36px 0}
footer .wrap{display:flex;align-items:center;gap:16px;flex-wrap:wrap}
footer .brand{color:#fff}
footer .brand svg{color:var(--color-blue-300)}
footer .brand-text{color:#fff}
footer .brand-placeholder{border-color:rgba(255,255,255,.25);color:var(--color-blue-200)}
footer .brand-placeholder svg,footer .brand-placeholder .brand-text{color:var(--color-blue-200)}
footer .sp{flex:1}
footer .meta{font:var(--type-body-sm);color:var(--color-blue-500)}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_brand_renders_placeholder() {
        let html = render_brand(&Brand::default(), "topbar");
        assert!(html.contains("brand-placeholder"));
        assert!(html.contains("Add your logo"));
    }

    #[test]
    fn footer_placeholder_uses_company_copy() {
        let html = render_brand(&Brand::default(), "footer");
        assert!(html.contains("Add your company"));
    }

    #[test]
    fn name_only_brand_renders_wordmark_not_placeholder() {
        let brand = Brand {
            name: "Acme Labs".into(),
            ..Default::default()
        };
        let html = render_brand(&brand, "topbar");
        assert!(html.contains("Acme Labs"));
        assert!(!html.contains("brand-placeholder"));
        assert!(html.contains("brand-text"));
    }

    #[test]
    fn logo_src_renders_img_with_name_alt() {
        let brand = Brand {
            name: "Acme".into(),
            logo_src: "https://example.com/logo.svg".into(),
            ..Default::default()
        };
        let html = render_brand(&brand, "topbar");
        assert!(html.contains("brand-img"));
        assert!(html.contains("https://example.com/logo.svg"));
        assert!(html.contains("alt=\"Acme\""));
    }

    #[test]
    fn active_content_logo_src_is_rejected() {
        assert!(!brand_logo_src_ok("javascript:alert(1)"));
        assert!(!brand_logo_src_ok("data:text/html,<script>"));
        assert!(brand_logo_src_ok("data:image/png;base64,AAAA"));
        assert!(brand_logo_src_ok("https://x/y.png"));
        assert!(brand_logo_src_ok("assets/logo.svg"));
        // A rejected logo src must fall back to the wordmark, never emit the URI.
        let brand = Brand {
            name: "Acme".into(),
            logo_src: "javascript:alert(1)".into(),
            ..Default::default()
        };
        let html = render_brand(&brand, "topbar");
        assert!(!html.contains("javascript:"));
        assert!(html.contains("brand-text"));
    }

    #[test]
    fn href_wraps_brand_in_link() {
        let brand = Brand {
            name: "Acme".into(),
            href: "https://acme.example".into(),
            ..Default::default()
        };
        let html = render_brand(&brand, "topbar");
        assert!(html.contains("brand-link"));
        assert!(html.contains("href=\"https://acme.example\""));
    }
}
