//! `chaos compose` — build ONE page from knowledge-base-backed sections
//! instead of generating a bunch of similar standalone pages.
//!
//! The caller picks the sections (`features`, `correlations`, `stack`), an
//! audience (free-text `persona`, routed to a detail level by prototype
//! embeddings — or an explicit `level`, embedder-free), and a `style` preset
//! (`editorial` light default, `blade-runner` dark neon), and Chaos assembles
//! the page from the persisted index and prior generated manifests ONLY.
//!
//! Hard honesty rules:
//! - Every section resolves from the chaos knowledge base (Postgres index +
//!   `docs/features_memory` manifests). Nothing here reads source files.
//! - A section that cannot be served (no index, no L1 hierarchy, unknown
//!   section/style name) is a loud error naming what is missing and the
//!   command that fixes it — never a silent fallback.
//! - The composed manifest is content-hashed; recomposing the same request
//!   over unchanged data is a cached no-op (`cached: true`, no write), so an
//!   agent can skip re-ingesting a memory it already holds.

use crate::{
    embedding::Embedder,
    export_util::escape_script_json,
    extractor::hash,
    feature_context::correlate_feature_manifests,
    feature_inventory::{self, FeatureInventoryOptions},
    provenance::{source, Breadcrumb},
    stack,
    storage::Storage,
    theme,
};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

/// Section vocabulary — every section is backed by persisted chaos data.
/// Unknown names are an error listing this list; there is no free-form section.
const SECTIONS: &[(&str, &str)] = &[
    (
        "features",
        "the feature inventory (L1 communities) with each feature's concise L3 explanation",
    ),
    (
        "correlations",
        "file correlations: files shared between the listed features, plus prior generated pages that overlap them",
    ),
    (
        "stack",
        "the declared tech stack (dependencies, scripts, deployment resources, configs)",
    ),
];

const MANIFEST_START: &str = r#"<script type="application/json" id="chaos-composed-manifest">"#;
const MANIFEST_END: &str = "</script>";

/// Detail levels a persona resolves to. Explicit `level` is embedder-free;
/// free-text `persona` is routed by prototype-embedding cosine (no keyword
/// list), mirroring the feature-inventory layer routing.
const LEVELS: &[&str] = &["beginner", "practitioner", "expert"];

/// Prototype phrasings per detail level — the anchors for SEMANTIC persona
/// routing. A persona like "a very beginner software engineer who has no idea
/// about the stack" lands on `beginner` by meaning, not by matching the word
/// "beginner".
const PERSONA_PROTOTYPES: &[(&str, &str)] = &[
    (
        "beginner",
        "a beginner software engineer who has no idea about this project's stack or features",
    ),
    (
        "beginner",
        "a newcomer onboarding to the codebase for the first time",
    ),
    (
        "beginner",
        "a junior developer who needs plain language explanations and a place to start",
    ),
    (
        "beginner",
        "a non-technical stakeholder who wants the big picture",
    ),
    (
        "practitioner",
        "a developer who works in this codebase regularly",
    ),
    (
        "practitioner",
        "an engineer joining the team who knows the stack but not this repository",
    ),
    (
        "practitioner",
        "a code reviewer who wants a structured overview",
    ),
    (
        "expert",
        "a senior engineer or architect who wants dense technical detail",
    ),
    (
        "expert",
        "a maintainer who wants symbols, files and internals",
    ),
    (
        "expert",
        "a staff engineer auditing the architecture of the system",
    ),
];

/// Floor the best level's max-pooled cosine must clear; below it the level
/// defaults to `practitioner` WITH an explicit warning (recorded, never
/// hidden). Personas are always descriptions of some audience, so the floor is
/// lower than the layer-routing floor.
const PERSONA_FLOOR: f64 = 0.45;

#[derive(Debug, Default)]
pub struct ComposeOptions {
    /// Requested sections, in render order. Required, non-empty.
    pub sections: Vec<String>,
    /// Free-text audience description (embedder-routed to a level).
    pub persona: Option<String>,
    /// Explicit detail level (`beginner`/`practitioner`/`expert`), embedder-free.
    pub level: Option<String>,
    /// Style preset (`editorial` default, `blade-runner`).
    pub style: Option<String>,
    /// Brand preset shipped inside Chaos (e.g. `molecule`).
    pub brand_preset: Option<String>,
    /// Feature filter passed to the inventory (folder | layer | topic, auto-detected).
    pub filter: Option<String>,
    pub title: Option<String>,
    pub slug: Option<String>,
    pub output_html: Option<PathBuf>,
    /// Cap on features in the features section. 0 = all.
    pub limit: usize,
    /// SITE MODE: also write one page per feature (under
    /// `<slug>-composed/`) and make the index's feature cards link to them.
    /// Each per-feature page carries its own manifest + content hash, so an
    /// unchanged feature's page is never rewritten (and an agent seeing the
    /// same hash should not re-ingest it).
    pub feature_pages: bool,
}

#[derive(Debug, Serialize)]
struct ComposedSection {
    kind: String,
    title: String,
    data: Value,
}

#[derive(Debug, Serialize)]
struct ComposedManifest {
    schema_version: String,
    repo_name: String,
    title: String,
    subtitle: String,
    /// What was asked for — sections, persona, level, style, filter — so an
    /// agent reading the manifest knows exactly what this page covers.
    request: Value,
    sections: Vec<ComposedSection>,
    /// Site mode: the per-feature pages this index links to, each with its own
    /// content hash (`{dir, pages: [{feature_id, label, page, content_hash}]}`).
    #[serde(skip_serializing_if = "Option::is_none")]
    site: Option<Value>,
    /// sha256 over (request + section data). The dedup key: same request over
    /// unchanged knowledge = same hash = cached no-op.
    content_hash: String,
    provenance: Vec<Breadcrumb>,
    warnings: Vec<String>,
}

/// One per-feature page of a composed site, ready to write.
struct SitePage {
    id: uuid::Uuid,
    label: String,
    /// Path relative to the features dir (`<slug>-composed/<fslug>.html`).
    rel_path: String,
    manifest: Value,
    content_hash: String,
}

/// Compose the page: resolve persona + style → resolve each section from the
/// knowledge base (loud error when one cannot be served) → hash → skip when
/// the existing page already holds this exact composition → render + write.
pub async fn run(
    storage: &Storage,
    embedder: Option<&dyn Embedder>,
    repo: &str,
    opts: &ComposeOptions,
) -> Result<Value> {
    let repo = storage
        .find_repository(repo)
        .await?
        .with_context(|| format!("repository is not indexed: {repo} — run chaos_analyze first; compose uses ONLY the chaos knowledge base"))?;
    let repo_root = PathBuf::from(&repo.root_path);
    let features_dir = repo_root.join("docs/features_memory");

    let mut provenance: Vec<Breadcrumb> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // ---- Validate the request up front: unknown names fail loudly. ----
    let section_kinds = normalize_sections(&opts.sections)?;
    let style = opts.style.clone().unwrap_or_default();
    let style_css = theme::style_preset(&style).with_context(|| {
        format!(
            "unknown style preset '{style}' — available: {}. Compose does not improvise styles.",
            theme::STYLE_PRESETS.join(", ")
        )
    })?;
    let style_name = if style.trim().is_empty() {
        "editorial".to_string()
    } else {
        style.trim().to_ascii_lowercase()
    };
    let brand = match opts.brand_preset.as_deref() {
        Some(name) => {
            let preset = theme::brand_preset(name)
                .with_context(|| format!("unknown brand preset '{name}'"))?;
            preset.brand
        }
        None => theme::Brand::default(),
    };
    let level = resolve_persona(
        embedder,
        opts.persona.as_deref(),
        opts.level.as_deref(),
        &mut provenance,
        &mut warnings,
    )
    .await?;

    // ---- Resolve sections from the knowledge base only. ----
    let mut sections: Vec<ComposedSection> = Vec::new();
    // The features section feeds correlations; resolve it once if either needs it.
    let needs_features = section_kinds
        .iter()
        .any(|k| k == "features" || k == "correlations");
    let collected = if needs_features {
        let inv_opts = FeatureInventoryOptions {
            output_html: None,
            limit: 0,
            layer: None,
            folder: None,
            topic: None,
            curation: None,
        };
        let (collected, _resolved) =
            feature_inventory::collect(storage, embedder, &repo, opts.filter.as_deref(), &inv_opts)
                .await?;
        if collected.cards.is_empty() {
            bail!(
                "cannot compose '{}': no features in the knowledge base for this repo/filter (filter read as {} {:?}). Run chaos_analyze so the L1 hierarchy exists, or widen the filter. Compose will not parse files to invent features.",
                section_kinds.join(", "),
                collected.filter.kind,
                collected.filter.value
            );
        }
        provenance.extend(collected.provenance.iter().cloned());
        warnings.extend(collected.warnings.iter().cloned());
        Some(collected)
    } else {
        None
    };

    // Output naming first: site pages need the directory name for their links.
    let title = opts
        .title
        .clone()
        .unwrap_or_else(|| format!("{} — composed: {}", repo.name, section_kinds.join(" + ")));
    let default_slug = format!("{}-{}", repo.name, section_kinds.join("-"));
    let slug = safe_slug(opts.slug.as_deref().unwrap_or(&default_slug));
    let output = opts
        .output_html
        .clone()
        .unwrap_or_else(|| features_dir.join(format!("{slug}-composed.html")));
    let index_file = output
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("index.html")
        .to_string();
    let site_dir_name = format!("{slug}-composed");

    // ---- Site mode: build one page per feature (manifests + hashes). ----
    if opts.feature_pages && !section_kinds.iter().any(|k| k == "features") {
        bail!(
            "feature_pages needs the 'features' section — the per-feature pages are built from it"
        );
    }
    let site_pages: Vec<SitePage> = if opts.feature_pages {
        let collected = collected.as_ref().expect("features collected above");
        let hierarchy = storage.load_community_hierarchy(&repo, 8).await?;
        provenance.push(Breadcrumb::new(
            source::POSTGRES,
            "load_community_hierarchy",
            format!(
                "loaded {} quotient edge(s) for cross-feature relations",
                hierarchy.edges.len()
            ),
        ));
        build_site_pages(
            collected,
            &hierarchy,
            &features_dir,
            &site_dir_name,
            &index_file,
            &level,
            opts.persona.as_deref(),
            &style_name,
        )?
    } else {
        Vec::new()
    };
    let page_links: HashMap<uuid::Uuid, String> = site_pages
        .iter()
        .map(|p| (p.id, p.rel_path.clone()))
        .collect();

    let mut filter_info_json = json!(null);
    for kind in &section_kinds {
        match kind.as_str() {
            "features" => {
                let collected = collected.as_ref().expect("features collected above");
                filter_info_json = serde_json::to_value(&collected.filter)?;
                sections.push(features_section(
                    collected,
                    opts.limit,
                    &page_links,
                    &mut warnings,
                ));
            }
            "correlations" => {
                let collected = collected.as_ref().expect("features collected above");
                sections.push(correlations_section(
                    collected,
                    &features_dir,
                    &mut provenance,
                    &mut warnings,
                )?);
            }
            "stack" => {
                let manifest = stack::build_manifest(storage, &repo).await?;
                provenance.extend(manifest.provenance.iter().cloned());
                warnings.extend(manifest.warnings.iter().cloned());
                sections.push(ComposedSection {
                    kind: "stack".into(),
                    title: "Tech stack".into(),
                    data: serde_json::to_value(&manifest)?,
                });
            }
            other => unreachable!("normalize_sections admitted unknown section {other}"),
        }
    }

    // ---- Request descriptor + content hash (the agent's dedup key). ----
    let request = json!({
        "sections": section_kinds,
        "persona": opts.persona,
        "level": level,
        "style": style_name,
        "filter": filter_info_json,
        "brand_preset": opts.brand_preset,
        "feature_pages": opts.feature_pages,
    });
    let site_value: Option<Value> = if opts.feature_pages {
        Some(json!({
            "dir": site_dir_name,
            "pages": site_pages.iter().map(|p| json!({
                "label": p.label,
                "page": p.rel_path,
                "content_hash": p.content_hash,
            })).collect::<Vec<_>>(),
        }))
    } else {
        None
    };
    let hash_input = serde_json::to_string(&json!({
        "repo": repo.name,
        "request": request,
        "sections": sections.iter().map(|s| json!({"kind": s.kind, "data": s.data})).collect::<Vec<_>>(),
        "site": site_value,
    }))?;
    let content_hash = hash(&hash_input);

    // ---- Hash gate: same composition already on disk → cached no-op. The
    // index hash covers every per-feature page hash, so "index unchanged"
    // implies "site content unchanged" — but the files must actually exist.
    let pages_on_disk = site_pages.iter().all(|p| {
        existing_content_hash(&features_dir.join(&p.rel_path)).as_deref()
            == Some(p.content_hash.as_str())
    });
    if existing_content_hash(&output).as_deref() == Some(content_hash.as_str()) && pages_on_disk {
        provenance.push(Breadcrumb::new(
            source::MERKLE,
            "content_hash",
            format!(
                "existing page(s) already hold composition {content_hash} — all writes skipped"
            ),
        ));
        let site_summary = site_value.as_ref().map(|_| {
            json!({"dir": site_dir_name, "feature_pages": site_pages.len(), "written": 0, "cached": site_pages.len()})
        });
        return Ok(compact_return(
            &repo.name,
            &title,
            &output,
            &content_hash,
            true,
            &request,
            &sections,
            site_summary,
            &provenance,
            &warnings,
        ));
    }

    provenance.push(Breadcrumb::new(
        source::MERKLE,
        "content_hash",
        format!("composition hashed as {content_hash} (request + section data + site pages)"),
    ));

    // ---- Write per-feature pages, each gated by its own hash. ----
    let mut pages_written = 0usize;
    let mut pages_cached = 0usize;
    for page in &site_pages {
        let path = features_dir.join(&page.rel_path);
        if existing_content_hash(&path).as_deref() == Some(page.content_hash.as_str()) {
            pages_cached += 1;
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            &path,
            render_feature_page_html(&page.manifest, style_css, &brand)?,
        )?;
        pages_written += 1;
    }
    if opts.feature_pages {
        provenance.push(Breadcrumb::new(
            source::MERKLE,
            "site_pages",
            format!(
                "{pages_written} feature page(s) written, {pages_cached} unchanged (per-page hash gate)"
            ),
        ));
    }

    let manifest = ComposedManifest {
        schema_version: "composed-1".into(),
        repo_name: repo.name.clone(),
        title: title.clone(),
        subtitle: subtitle_for(&level, &section_kinds),
        request: request.clone(),
        sections,
        site: site_value.clone(),
        content_hash: content_hash.clone(),
        provenance: provenance.clone(),
        warnings: warnings.clone(),
    };

    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&output, render_composed_html(&manifest, style_css, &brand)?)?;

    let site_summary = site_value.as_ref().map(|_| {
        json!({"dir": site_dir_name, "feature_pages": site_pages.len(), "written": pages_written, "cached": pages_cached})
    });
    Ok(compact_return(
        &repo.name,
        &title,
        &output,
        &content_hash,
        false,
        &request,
        &manifest.sections,
        site_summary,
        &provenance,
        &warnings,
    ))
}

/// Normalize requested sections against the vocabulary; tiny exact aliases
/// only (this is API surface, not semantic matching). Unknown → loud error.
fn normalize_sections(requested: &[String]) -> Result<Vec<String>> {
    if requested.is_empty() {
        bail!("sections is required — pick from: {}", section_vocabulary());
    }
    let mut out: Vec<String> = Vec::new();
    for raw in requested {
        let kind = match raw.trim().to_ascii_lowercase().as_str() {
            "features" | "feature-list" | "feature_list" | "explanations" | "summaries" => {
                "features"
            }
            "correlations" | "file-correlations" | "file_correlations" | "related-pages"
            | "related_pages" => "correlations",
            "stack" | "tech-stack" | "tech_stack" | "dependencies" => "stack",
            other => bail!(
                "unknown section '{other}' — compose only serves knowledge-base-backed sections: {}. A missing section kind is a chaos feature request, not something to improvise.",
                section_vocabulary()
            ),
        };
        if !out.iter().any(|k| k == kind) {
            out.push(kind.to_string());
        }
    }
    Ok(out)
}

fn section_vocabulary() -> String {
    SECTIONS
        .iter()
        .map(|(k, d)| format!("'{k}' ({d})"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Resolve the audience to a detail level. Explicit level wins (embedder-free,
/// validated). Free text needs the embedder — without one this is an ERROR
/// telling the caller to pass `level`, not a silent guess.
async fn resolve_persona(
    embedder: Option<&dyn Embedder>,
    persona: Option<&str>,
    level: Option<&str>,
    provenance: &mut Vec<Breadcrumb>,
    warnings: &mut Vec<String>,
) -> Result<String> {
    if let Some(level) = level {
        let level = level.trim().to_ascii_lowercase();
        if !LEVELS.contains(&level.as_str()) {
            bail!(
                "unknown level '{level}' — pass one of: {}",
                LEVELS.join(", ")
            );
        }
        provenance.push(Breadcrumb::new(
            source::FILE,
            "explicit_level",
            format!("detail level '{level}' passed explicitly (embedder-free)"),
        ));
        return Ok(level);
    }
    let Some(persona) = persona.map(str::trim).filter(|p| !p.is_empty()) else {
        return Ok("practitioner".to_string());
    };
    let Some(emb) = embedder else {
        bail!(
            "persona '{persona}' needs the embedder to resolve by meaning, and no embedder is configured — pass level: {} instead",
            LEVELS.join(" | ")
        );
    };
    let routed: Result<(String, f64, &str)> = async {
        let query = emb.embed(persona).await?;
        let protos = persona_prototype_embeddings(emb).await?;
        // Max-pool per level: a level is as close as its closest phrasing.
        let mut best: Vec<(&str, f64, &str)> = Vec::new();
        for ((lvl, text), vec) in PERSONA_PROTOTYPES.iter().zip(protos.iter()) {
            let score = cosine(&query, vec);
            match best.iter_mut().find(|(l, _, _)| l == lvl) {
                Some(slot) if score > slot.1 => *slot = (lvl, score, text),
                Some(_) => {}
                None => best.push((lvl, score, text)),
            }
        }
        best.sort_by(|a, b| b.1.total_cmp(&a.1));
        let (lvl, top, anchor) = best[0];
        Ok((lvl.to_string(), top, anchor))
    }
    .await;
    match routed {
        Ok((lvl, top, anchor)) if top >= PERSONA_FLOOR => {
            provenance.push(Breadcrumb::new(
                source::EMBEDDING,
                "persona_routing",
                format!(
                    "persona resolved to '{lvl}' by meaning (cosine {top:.2} vs anchor \"{anchor}\")"
                ),
            ));
            Ok(lvl)
        }
        Ok((_, top, _)) => {
            warnings.push(format!(
                "persona '{persona}' did not clearly match a detail level (best cosine {top:.2} < {PERSONA_FLOOR}) — defaulting to 'practitioner'; pass level explicitly to override"
            ));
            Ok("practitioner".to_string())
        }
        Err(err) => bail!(
            "persona routing failed ({err}) — compose does not guess; pass level: {} instead",
            LEVELS.join(" | ")
        ),
    }
}

/// Prototype embeddings per embedder identity, computed once per process
/// (mirrors the feature-inventory layer-prototype cache).
type PrototypeVectors = std::sync::Arc<Vec<Vec<f32>>>;
static PERSONA_PROTOTYPE_CACHE: std::sync::OnceLock<
    std::sync::Mutex<HashMap<String, PrototypeVectors>>,
> = std::sync::OnceLock::new();

async fn persona_prototype_embeddings(emb: &dyn Embedder) -> Result<PrototypeVectors> {
    let key = format!("{}/{}/{}", emb.provider(), emb.model_id(), emb.dimensions());
    let cache = PERSONA_PROTOTYPE_CACHE.get_or_init(Default::default);
    if let Some(hit) = cache.lock().unwrap().get(&key) {
        return Ok(hit.clone());
    }
    let texts: Vec<String> = PERSONA_PROTOTYPES
        .iter()
        .map(|(_, t)| t.to_string())
        .collect();
    let vecs = std::sync::Arc::new(emb.embed_batch(&texts).await?);
    cache.lock().unwrap().insert(key, vecs.clone());
    Ok(vecs)
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum();
    let na: f64 = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// The features section: every selected card with its concise L3 explanation.
/// In site mode each entry carries a `page` link to its per-feature page.
fn features_section(
    collected: &feature_inventory::CollectedFeatures,
    limit: usize,
    page_links: &HashMap<uuid::Uuid, String>,
    warnings: &mut Vec<String>,
) -> ComposedSection {
    let mut cards: Vec<_> = collected.cards.iter().collect();
    cards.sort_by(|a, b| {
        a.0.rank()
            .cmp(&b.0.rank())
            .then_with(|| b.1.member_count.cmp(&a.1.member_count))
    });
    if limit > 0 && cards.len() > limit {
        let dropped = cards.len() - limit;
        cards.truncate(limit);
        warnings.push(format!(
            "features section capped at {limit}; {dropped} more matched (pass limit 0 for all)"
        ));
    }
    let mut layer_tally: BTreeMap<u8, (String, usize)> = BTreeMap::new();
    for (layer, _) in &cards {
        layer_tally
            .entry(layer.rank())
            .or_insert((layer.as_str().to_string(), 0))
            .1 += 1;
    }
    let features: Vec<Value> = cards
        .iter()
        .map(|(layer, card)| {
            json!({
                "id": card.id,
                "label": card.label,
                "layer": layer.as_str(),
                "explanation": card.summary,
                "member_count": card.member_count,
                "languages": card.languages,
                "folders": card.folders,
                "top_symbols": card.top_symbols,
                "key_files": card.key_files,
                "page": page_links.get(&card.id),
            })
        })
        .collect();
    ComposedSection {
        kind: "features".into(),
        title: "Features".into(),
        data: json!({
            "total": features.len(),
            "layer_counts": layer_tally.values().map(|(l, c)| json!({"layer": l, "count": c})).collect::<Vec<_>>(),
            "features": features,
        }),
    }
}

/// The correlations section: files shared between the listed features (from
/// each feature's top files — explicitly labelled as such) and prior generated
/// pages that overlap them by files/symbols.
fn correlations_section(
    collected: &feature_inventory::CollectedFeatures,
    features_dir: &Path,
    provenance: &mut Vec<Breadcrumb>,
    warnings: &mut Vec<String>,
) -> Result<ComposedSection> {
    // Cross-feature shared files, computed from the cards' (capped) key files.
    let mut by_file: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (_, card) in &collected.cards {
        for file in &card.key_files {
            by_file.entry(file).or_default().insert(&card.label);
        }
    }
    let mut shared: Vec<Value> = by_file
        .iter()
        .filter(|(_, feats)| feats.len() >= 2)
        .map(|(file, feats)| json!({"file": file, "features": feats.iter().collect::<Vec<_>>()}))
        .collect();
    shared.sort_by_key(|entry| {
        std::cmp::Reverse(entry["features"].as_array().map(Vec::len).unwrap_or(0))
    });
    shared.truncate(40);

    // Prior generated pages correlated by file/symbol overlap.
    let files: HashSet<String> = collected
        .cards
        .iter()
        .flat_map(|(_, c)| c.key_files.iter().cloned())
        .collect();
    let symbols: HashSet<String> = collected
        .cards
        .iter()
        .flat_map(|(_, c)| c.top_symbols.iter().map(|s| s.name.clone()))
        .collect();
    let pages = correlate_feature_manifests(features_dir, &files, &symbols, "", 10)?;
    provenance.push(Breadcrumb::new(
        source::MANIFEST,
        "correlate_feature_manifests",
        format!(
            "{} prior page(s) correlated by shared files/symbols across {} file(s), {} symbol(s)",
            pages.len(),
            files.len(),
            symbols.len()
        ),
    ));
    if shared.is_empty() && pages.is_empty() {
        warnings.push(
            "correlations: no shared files among the listed features' top files and no prior generated pages overlap them — that is the honest state of the knowledge base, not an error"
                .into(),
        );
    }
    Ok(ComposedSection {
        kind: "correlations".into(),
        title: "File correlations".into(),
        data: json!({
            "note": "shared files are computed from each feature's top files (capped per feature), so counts are a floor, not a census",
            "shared_files": shared,
            "related_pages": pages,
        }),
    })
}

/// Build one page per selected feature: code/files, cross-feature relations
/// (smart-contract neighbours tagged via their Solidity language), prior-page
/// correlations, and a deterministic persona-adapted walkthrough. Everything
/// comes from the persisted graph — the narrative describes real structure and
/// says so on the page.
#[allow(clippy::too_many_arguments)]
fn build_site_pages(
    collected: &feature_inventory::CollectedFeatures,
    hierarchy: &crate::hierarchy_export::CommunityHierarchy,
    features_dir: &Path,
    site_dir_name: &str,
    index_file: &str,
    level: &str,
    persona: Option<&str>,
    style_name: &str,
) -> Result<Vec<SitePage>> {
    let detail_by_id: HashMap<uuid::Uuid, &crate::hierarchy_export::CommunityDetail> =
        hierarchy.communities.iter().map(|c| (c.id, c)).collect();
    let in_scope: HashMap<uuid::Uuid, (&str, &feature_inventory::FeatureCard)> = collected
        .cards
        .iter()
        .map(|(layer, card)| (card.id, (layer.as_str(), card)))
        .collect();

    // Deterministic, readable, collision-free file names.
    let mut taken: HashMap<String, usize> = HashMap::new();
    let mut rel_path_for: HashMap<uuid::Uuid, String> = HashMap::new();
    for (_, card) in &collected.cards {
        let base = safe_slug(&card.label);
        let n = taken.entry(base.clone()).or_insert(0);
        *n += 1;
        let name = if *n == 1 { base } else { format!("{base}-{n}") };
        rel_path_for.insert(card.id, format!("{site_dir_name}/{name}.html"));
    }

    let mut pages = Vec::new();
    for (layer, card) in &collected.cards {
        // Cross-feature relations from the quotient graph.
        let mut relations: Vec<Value> = Vec::new();
        for edge in &hierarchy.edges {
            let (other_id, direction) = if edge.source == card.id {
                (edge.target, "uses")
            } else if edge.target == card.id {
                (edge.source, "used by")
            } else {
                continue;
            };
            let (label, other_layer, languages, page) = match in_scope.get(&other_id) {
                Some((l, other_card)) => (
                    other_card.label.clone(),
                    Some(l.to_string()),
                    other_card.languages.clone(),
                    rel_path_for.get(&other_id).map(|p| relative_within_site(p)),
                ),
                None => {
                    let Some(detail) = detail_by_id.get(&other_id) else {
                        continue;
                    };
                    let files: Vec<String> = detail
                        .top_members
                        .iter()
                        .map(|(_, _, f)| f.clone())
                        .collect();
                    (
                        detail.label.clone(),
                        None,
                        feature_inventory::language_tally(&files),
                        None,
                    )
                }
            };
            let smart_contract = languages.iter().any(|l| l.language == "Solidity");
            relations.push(json!({
                "label": label,
                "direction": direction,
                "kind": edge.kind,
                "weight": edge.weight,
                "edge_count": edge.edge_count,
                "layer": other_layer,
                "languages": languages,
                "page": page,
                "in_scope": other_layer.is_some(),
                "smart_contract": smart_contract,
            }));
        }
        relations.sort_by(|a, b| {
            b["weight"]
                .as_f64()
                .unwrap_or(0.0)
                .total_cmp(&a["weight"].as_f64().unwrap_or(0.0))
        });
        relations.truncate(12);

        // Prior generated pages that overlap THIS feature.
        let files: HashSet<String> = card.key_files.iter().cloned().collect();
        let symbols: HashSet<String> = card.top_symbols.iter().map(|s| s.name.clone()).collect();
        let related_pages = correlate_feature_manifests(features_dir, &files, &symbols, "", 5)?;

        let narrative = narrative_blocks(level, layer.as_str(), card, &relations);

        let mut manifest = json!({
            "schema_version": "composed-feature-1",
            "feature": {
                "id": card.id,
                "label": card.label,
                "layer": layer.as_str(),
                "explanation": card.summary,
                "member_count": card.member_count,
                "languages": card.languages,
                "folders": card.folders,
            },
            "request": {"level": level, "persona": persona, "style": style_name},
            "narrative": narrative,
            "relations": relations,
            "code": {"top_symbols": card.top_symbols, "key_files": card.key_files},
            "related_pages": related_pages,
            "back": format!("../{index_file}"),
            "honesty": "Generated deterministically from the indexed graph — this describes real structure, not invented user journeys. For a UX storyboard with real screens, use chaos_write_storyboard.",
            "content_hash": "",
        });
        let content_hash = hash(&serde_json::to_string(&manifest)?);
        manifest["content_hash"] = json!(content_hash);
        pages.push(SitePage {
            id: card.id,
            label: card.label.clone(),
            rel_path: rel_path_for[&card.id].clone(),
            manifest,
            content_hash,
        });
    }
    Ok(pages)
}

/// `<site-dir>/<file>.html` → `<file>.html` (links between pages in the same
/// site directory).
fn relative_within_site(rel_path: &str) -> String {
    rel_path.rsplit('/').next().unwrap_or(rel_path).to_string()
}

/// Deterministic persona-adapted walkthrough blocks, built ONLY from indexed
/// data: the L3 summary, the journey layer, the quotient-graph relations, and
/// the code location. No invented user journeys.
fn narrative_blocks(
    level: &str,
    layer: &str,
    card: &feature_inventory::FeatureCard,
    relations: &[Value],
) -> Vec<Value> {
    let mut blocks = Vec::new();

    let what = match card.summary.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(summary) => summary.to_string(),
        None => "No knowledge-base summary exists for this feature yet — run chaos_analyze so the L3 community summaries are generated.".to_string(),
    };
    blocks.push(json!({"title": "What this is", "body": what}));

    let mut sits = match layer {
        "entry" => "This is an entry-layer feature — part of what users (or external callers) touch first.",
        "interface" => "This is an interface-layer feature — a surface other code calls into (APIs, routes, resolvers).",
        "core" => "This is a core-layer feature — business logic working behind the callable surface.",
        "foundation" => "This is a foundation-layer feature — contracts, infrastructure, or shared groundwork the other layers rest on.",
        _ => "Its journey layer could not be determined from the graph.",
    }
    .to_string();
    if level == "beginner" {
        sits.push_str(" A useful reading order for the whole site is entry → interface → core → foundation: start from what users see and descend toward what it all rests on.");
    }
    if level == "expert" {
        sits.push_str(&format!(
            " {} member node(s) across {}.",
            card.member_count,
            if card.folders.is_empty() {
                "the repo root".to_string()
            } else {
                card.folders.join(", ")
            }
        ));
    }
    blocks.push(json!({"title": "Where it sits", "body": sits}));

    let uses = relations
        .iter()
        .filter(|r| r["direction"] == "uses")
        .count();
    let used_by = relations.len() - uses;
    let mut connects = if relations.is_empty() {
        "No cross-feature edges in the quotient graph — at the feature level this stands alone."
            .to_string()
    } else {
        let top: Vec<String> = relations
            .iter()
            .take(3)
            .map(|r| {
                format!(
                    "{} ({})",
                    r["label"].as_str().unwrap_or("?"),
                    r["direction"].as_str().unwrap_or("?")
                )
            })
            .collect();
        format!(
            "It connects to {} other feature(s) — {uses} it uses, {used_by} that use it. Strongest: {}.",
            relations.len(),
            top.join("; ")
        )
    };
    let contracts: Vec<&str> = relations
        .iter()
        .filter(|r| r["smart_contract"] == json!(true))
        .filter_map(|r| r["label"].as_str())
        .collect();
    if !contracts.is_empty() {
        connects.push_str(&format!(
            " It touches the smart-contract side of the stack: {} (Solidity).",
            contracts.join(", ")
        ));
    }
    blocks.push(json!({"title": "How it connects", "body": connects}));

    blocks.push(json!({
        "title": "Where the code lives",
        "body": format!(
            "{} top file(s) under {}{}.",
            card.key_files.len(),
            if card.folders.is_empty() { "the repo root".to_string() } else { card.folders.join(", ") },
            if card.languages.is_empty() {
                String::new()
            } else {
                format!(
                    " — {}",
                    card.languages
                        .iter()
                        .map(|l| format!("{} ({})", l.language, l.count))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        ),
    }));
    blocks
}

/// Render a per-feature site page (same theme/style/brand as the index).
fn render_feature_page_html(
    manifest: &Value,
    style_css: &str,
    brand: &theme::Brand,
) -> Result<String> {
    let title = manifest["feature"]["label"].as_str().unwrap_or("Feature");
    let layer = manifest["feature"]["layer"].as_str().unwrap_or("unknown");
    let back = manifest["back"].as_str().unwrap_or("../");
    let manifest_json = serde_json::to_string(manifest)?;
    Ok(COMPOSED_FEATURE_HTML
        .replace("__THEME__", theme::THEME_CSS)
        .replace("__STYLE__", style_css)
        .replace("__BRAND_TOPBAR__", &theme::render_brand(brand, "topbar"))
        .replace("__BRAND_FOOTER__", &theme::render_brand(brand, "footer"))
        .replace("__TITLE__", &html_escape(title))
        .replace("__LAYER__", &html_escape(layer))
        .replace("__BACK__", &html_escape(back))
        .replace("__MANIFEST__", &escape_script_json(&manifest_json)))
}

const COMPOSED_FEATURE_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__TITLE__</title>
<style>
__THEME__
__STYLE__
[hidden]{display:none!important}
.hero .wrap{grid-template-columns:1fr}
main.wrap{padding:8px 0 40px}
section.compose{padding:30px 0;border-bottom:var(--border-hairline)}section.compose:last-of-type{border-bottom:0}
section.compose>h2{margin:0 0 10px;font:var(--type-h4);color:var(--color-ink-700)}
.rolebadge{display:inline-flex;border-radius:var(--radius-pill);padding:4px 12px;font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.06em;color:#fff;background:var(--color-blue-700);margin-bottom:10px}
.block-card{background:var(--color-surface-0);border:var(--border-hairline);border-radius:var(--radius-lg);box-shadow:var(--shadow-sm);padding:16px 18px;margin-top:12px}
.block-card h3{margin:0 0 6px;font:var(--type-h5);color:var(--color-blue-700)}
.block-card p{margin:0;color:var(--color-ink-500);line-height:1.6}
.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(300px,1fr));gap:14px}
.card{background:var(--color-surface-0);border:var(--border-hairline);border-radius:var(--radius-lg);box-shadow:var(--shadow-sm);padding:14px 16px}
.card h3{margin:0;font:var(--type-h5);color:var(--color-ink-700);overflow-wrap:anywhere}
.card p{margin:6px 0 0;color:var(--color-ink-500);font:var(--type-body-sm);line-height:1.5}
.card a{color:var(--color-blue-700)}
.tag{display:inline-flex;border-radius:var(--radius-pill);padding:2px 9px;font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.05em;background:var(--color-surface-2);color:var(--color-ink-400);margin-right:6px}
.tag.sc{background:var(--color-violet-500);color:#fff}
.mono{font-family:var(--font-mono);font-size:12px}
table{width:100%;border-collapse:collapse;margin-top:10px;font:var(--type-body-sm)}
th{text-align:left;color:var(--fg-tertiary);font:var(--type-overline-sm);text-transform:uppercase;padding:8px 10px;border-bottom:var(--border-hairline)}
td{padding:8px 10px;border-bottom:var(--border-soft);color:var(--color-ink-500);overflow-wrap:anywhere}
details.more{margin-top:10px}details.more summary{cursor:pointer;color:var(--color-blue-700);font:var(--type-body-sm);font-weight:500}
.honesty{border-left:3px solid var(--color-blue-400);background:var(--color-surface-1);padding:10px 14px;border-radius:var(--radius-sm);color:var(--fg-tertiary);font:var(--type-body-sm);margin-top:18px}
.backlink{font:var(--type-body-sm);font-weight:500}
</style>
</head>
<body>
<div class="topbar"><div class="wrap">__BRAND_TOPBAR__<span class="crumb"><a class="backlink" href="__BACK__">&larr; All features</a><span class="sep">&rsaquo;</span><b>__TITLE__</b></span><span class="sp"></span><span class="pilltag">Feature</span></div></div>
<div data-chaos-composed-feature>
<header class="hero"><div class="wrap"><div><div class="eyebrow">Composed feature page</div><span class="rolebadge">__LAYER__</span><h1>__TITLE__</h1><div class="subtitle" id="subtitle"></div></div></div></header>
<main class="wrap wide">
<section class="compose" data-chaos-narrative><h2>Walkthrough</h2><div id="narrative"></div></section>
<section class="compose" data-chaos-relations><h2>How it relates to the rest of the stack</h2><div id="relations" class="cards"></div></section>
<section class="compose" data-chaos-code><h2>Code &amp; files</h2><div id="code"></div></section>
<section class="compose" data-chaos-related hidden><h2>Related generated pages</h2><div id="related" class="cards"></div></section>
<div class="honesty" id="honesty"></div>
</main>
</div>
<footer><div class="wrap">__BRAND_FOOTER__<span class="sp"></span><span class="meta">generated by Chaos Substrate</span></div></footer>
<script type="application/json" id="chaos-composed-manifest">
__MANIFEST__
</script>
<script>
(function(){
var M=JSON.parse(document.getElementById("chaos-composed-manifest").textContent);
var LEVEL=(M.request&&M.request.level)||"practitioner";
function esc(v){return String(v==null?"":v).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;").replace(/'/g,"&#039;");}
function el(tag,cls,html){var e=document.createElement(tag);if(cls)e.className=cls;if(html!=null)e.innerHTML=html;return e;}
var f=M.feature||{};
document.getElementById("subtitle").textContent=(f.member_count||0)+" code node(s) · "+((f.languages||[]).map(function(l){return l.language;}).join(", ")||"language unknown");
var nar=document.getElementById("narrative");
(M.narrative||[]).forEach(function(b){var c=el("div","block-card");c.appendChild(el("h3",null,esc(b.title)));c.appendChild(el("p",null,esc(b.body)));nar.appendChild(c);});
var rel=document.getElementById("relations");
if((M.relations||[]).length){(M.relations||[]).forEach(function(r){
var c=el("div","card");
var name=r.page?('<a href="'+esc(r.page)+'">'+esc(r.label)+"</a>"):esc(r.label);
c.appendChild(el("h3",null,name));
var tags='<span class="tag">'+esc(r.direction)+"</span>"+(r.layer?'<span class="tag">'+esc(r.layer)+"</span>":'<span class="tag">outside this filter</span>')+(r.smart_contract?'<span class="tag sc">smart contracts</span>':"");
c.appendChild(el("p",null,tags));
c.appendChild(el("p",null,esc(r.kind)+" · weight "+(Math.round((r.weight||0)*100)/100)+" · "+(r.edge_count||0)+" edge(s)"));
rel.appendChild(c);});}
else rel.appendChild(el("p",null,"No cross-feature edges in the quotient graph."));
var code=document.getElementById("code");
var more=el("details","more");more.innerHTML="<summary>"+((M.code||{}).top_symbols||[]).length+" symbol(s), "+((M.code||{}).key_files||[]).length+" file(s)</summary>";
if(LEVEL==="expert")more.open=true;
var t=el("table");t.innerHTML="<tr><th>Symbol</th><th>Kind</th><th>File</th></tr>"+(((M.code||{}).top_symbols)||[]).map(function(s){return "<tr><td class=mono>"+esc(s.name)+"</td><td>"+esc(s.kind)+"</td><td class=mono>"+esc(s.file)+"</td></tr>";}).join("");
more.appendChild(t);
more.appendChild(el("p","mono",(((M.code||{}).key_files)||[]).map(esc).join("<br>")));
code.appendChild(more);
var related=(M.related_pages||[]);
if(related.length){var sec=document.querySelector("[data-chaos-related]");sec.hidden=false;var grid=document.getElementById("related");
related.forEach(function(p){var c=el("div","card");c.appendChild(el("h3",null,esc(p.title||p.feature_id)));c.appendChild(el("p",null,esc(p.page)+" — "+((p.shared_files||[]).length)+" shared file(s)"));grid.appendChild(c);});}
document.getElementById("honesty").textContent=M.honesty||"";
})();
</script>
</body>
</html>
"##;

fn subtitle_for(level: &str, kinds: &[String]) -> String {
    let audience = match level {
        "beginner" => "written for a newcomer: plain language first, jargon collapsed",
        "expert" => "written for an expert: dense detail, symbols and files up front",
        _ => "written for a working engineer",
    };
    format!(
        "Composed from the chaos knowledge base only ({}) — {audience}.",
        kinds.join(" + ")
    )
}

/// Read the content hash out of an existing composed page, if any.
fn existing_content_hash(path: &Path) -> Option<String> {
    let html = fs::read_to_string(path).ok()?;
    let start = html.find(MANIFEST_START)? + MANIFEST_START.len();
    let end = html[start..].find(MANIFEST_END)? + start;
    let value: Value = serde_json::from_str(html[start..end].trim()).ok()?;
    value
        .get("content_hash")
        .and_then(Value::as_str)
        .map(String::from)
}

#[allow(clippy::too_many_arguments)]
fn compact_return(
    repo: &str,
    title: &str,
    output: &Path,
    content_hash: &str,
    cached: bool,
    request: &Value,
    sections: &[ComposedSection],
    site: Option<Value>,
    provenance: &[Breadcrumb],
    warnings: &[String],
) -> Value {
    let section_lines: Vec<Value> = sections
        .iter()
        .map(|s| {
            let items = match s.kind.as_str() {
                "features" => s.data["features"].as_array().map(Vec::len).unwrap_or(0),
                "correlations" => {
                    s.data["shared_files"].as_array().map(Vec::len).unwrap_or(0)
                        + s.data["related_pages"]
                            .as_array()
                            .map(Vec::len)
                            .unwrap_or(0)
                }
                "stack" => s.data["totals"]["packages"].as_u64().unwrap_or(0) as usize,
                _ => 0,
            };
            json!({"kind": s.kind, "items": items})
        })
        .collect();
    json!({
        "repo": repo,
        "title": title,
        "html": output,
        "content_hash": content_hash,
        "cached": cached,
        "request": request,
        "sections": section_lines,
        "site": site,
        "next": if cached {
            "This exact composition already exists (same content hash) — reuse the existing page/manifest; do not re-ingest it as new memory."
        } else {
            "Composed page written. The embedded chaos-composed-manifest carries every section's full data for agent consumption; the content_hash is the dedup key."
        },
        "provenance": provenance,
        "warnings": warnings,
    })
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
        "composed".to_string()
    } else {
        slug.chars().take(80).collect()
    }
}

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Render the composed page: shared theme + optional style-preset token
/// override + brand chrome; the JS renders each section from the embedded
/// manifest, adapting density to the resolved detail level.
fn render_composed_html(
    manifest: &ComposedManifest,
    style_css: &str,
    brand: &theme::Brand,
) -> Result<String> {
    let manifest_json = serde_json::to_string(manifest)?;
    Ok(COMPOSED_HTML
        .replace("__THEME__", theme::THEME_CSS)
        .replace("__STYLE__", style_css)
        .replace("__BRAND_TOPBAR__", &theme::render_brand(brand, "topbar"))
        .replace("__BRAND_FOOTER__", &theme::render_brand(brand, "footer"))
        .replace("__TITLE__", &html_escape(&manifest.title))
        .replace("__SUBTITLE__", &html_escape(&manifest.subtitle))
        .replace("__MANIFEST__", &escape_script_json(&manifest_json)))
}

const COMPOSED_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__TITLE__</title>
<style>
__THEME__
__STYLE__
/* ===== composed page components ===== */
[hidden]{display:none!important}
.hero .wrap{grid-template-columns:1fr}
.subtitle{max-width:1100px;color:var(--color-ink-400);line-height:1.55;font:var(--type-body-lg)}
main.wrap{padding:8px 0 40px}
section.compose{padding:34px 0;border-bottom:var(--border-hairline)}section.compose:last-of-type{border-bottom:0}
section.compose>h2{margin:0 0 4px;font:var(--type-h4);color:var(--color-ink-700)}
.sec-sub{color:var(--fg-tertiary);font:var(--type-body-sm);margin:0 0 16px}
.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(330px,1fr));gap:14px}
.card{background:var(--color-surface-0);border:var(--border-hairline);border-radius:var(--radius-lg);box-shadow:var(--shadow-sm);padding:16px 18px}
.card h3{margin:0;font:var(--type-h5);color:var(--color-ink-700);overflow-wrap:anywhere}
.card p{margin:8px 0 0;color:var(--color-ink-500);line-height:1.55;font:var(--type-body-sm)}
.rolebadge{display:inline-flex;border-radius:var(--radius-pill);padding:3px 10px;font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.06em;color:#fff;background:var(--color-blue-700);margin-bottom:8px}
.rolebadge.entry{background:var(--color-blue-700)}.rolebadge.interface{background:rgb(176,124,15)}.rolebadge.core{background:var(--color-teal-500)}.rolebadge.foundation{background:var(--color-violet-500)}.rolebadge.unknown{background:var(--color-ink-300)}
.chips{display:flex;flex-wrap:wrap;gap:6px;margin-top:10px}
.chip{border:var(--border-hairline);border-radius:var(--radius-pill);padding:3px 10px;font:var(--type-body-sm);color:var(--color-ink-400);background:var(--color-surface-1)}
.mono{font-family:var(--font-mono);font-size:12px}
details.more{margin-top:10px}details.more summary{cursor:pointer;color:var(--color-blue-700);font:var(--type-body-sm);font-weight:500}
table{width:100%;border-collapse:collapse;margin-top:12px;font:var(--type-body-sm)}
th{text-align:left;color:var(--fg-tertiary);font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.08em;padding:8px 10px;border-bottom:var(--border-hairline)}
td{padding:8px 10px;border-bottom:var(--border-soft);color:var(--color-ink-500);vertical-align:top;overflow-wrap:anywhere}
.stats{display:flex;flex-wrap:wrap;gap:12px;margin:6px 0 4px}
.stat{background:var(--color-surface-0);border:var(--border-hairline);border-radius:var(--radius-md);box-shadow:var(--shadow-xs);padding:12px 16px;min-width:120px}
.stat b{display:block;font:var(--type-h4);color:var(--color-blue-700)}.stat span{color:var(--fg-tertiary);font:var(--type-body-sm)}
.provlist .prov{padding:10px 12px;border:var(--border-hairline);border-radius:var(--radius-md);background:var(--color-surface-1);font:var(--type-body-sm);margin-top:8px;color:var(--color-ink-500)}
.provlist .prov strong{color:var(--color-blue-700)}
.warn{border-left:3px solid rgb(176,124,15);background:var(--color-surface-1);padding:10px 14px;border-radius:var(--radius-sm);color:var(--color-ink-500);font:var(--type-body-sm);margin-top:8px}
</style>
</head>
<body>
<div class="topbar"><div class="wrap">__BRAND_TOPBAR__<span class="crumb">Composed<span class="sep">&rsaquo;</span><b>__TITLE__</b></span><span class="sp"></span><span class="pilltag">Composed</span></div></div>
<div data-chaos-composed>
<header class="hero"><div class="wrap"><div><div class="eyebrow">Composed page</div><h1>__TITLE__</h1><div class="subtitle">__SUBTITLE__</div></div></div></header>
<main class="wrap wide"><div id="sections"></div>
<section class="compose" data-chaos-provenance><h2>How this was generated</h2><p class="sec-sub">Every section was resolved from the chaos knowledge base — no source files were parsed to build this page.</p><div id="provenance" class="provlist"></div><div id="warnings"></div></section>
</main>
</div>
<footer><div class="wrap">__BRAND_FOOTER__<span class="sp"></span><span class="meta">generated by Chaos Substrate</span></div></footer>
<script type="application/json" id="chaos-composed-manifest">
__MANIFEST__
</script>
<script>
(function(){
var M=JSON.parse(document.getElementById("chaos-composed-manifest").textContent);
var LEVEL=(M.request&&M.request.level)||"practitioner";
function esc(v){return String(v==null?"":v).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;").replace(/'/g,"&#039;");}
function el(tag,cls,html){var e=document.createElement(tag);if(cls)e.className=cls;if(html!=null)e.innerHTML=html;return e;}
var root=document.getElementById("sections");
function renderFeatures(s){
var sec=el("section","compose");sec.setAttribute("data-chaos-section","features");
sec.appendChild(el("h2",null,esc(s.title)));
var counts=(s.data.layer_counts||[]).map(function(c){return esc(c.layer)+": "+c.count;}).join(" · ");
sec.appendChild(el("p","sec-sub",s.data.total+" feature(s)"+(counts?" — "+counts:"")+(LEVEL==="beginner"?" · Read top to bottom: entry-layer features are what users touch; foundation features are what everything rests on.":"")));
var grid=el("div","cards");
(s.data.features||[]).forEach(function(f){
var card=el("div","card");
card.appendChild(el("span","rolebadge "+esc(f.layer),esc(f.layer)));
card.appendChild(el("h3",null,f.page?('<a href="'+esc(f.page)+'">'+esc(f.label)+"</a>"):esc(f.label)));
if(f.explanation)card.appendChild(el("p",null,esc(f.explanation)));
var chips=el("div","chips");
(f.languages||[]).forEach(function(l){chips.appendChild(el("span","chip",esc(l.language)+" · "+l.count));});
(f.folders||[]).slice(0,4).forEach(function(d){chips.appendChild(el("span","chip mono",esc(d)));});
card.appendChild(chips);
if(LEVEL!=="beginner"&&((f.top_symbols||[]).length||(f.key_files||[]).length)){
var more=el("details","more");more.innerHTML="<summary>Symbols &amp; files</summary>";
if(LEVEL==="expert")more.open=true;
var t=el("table");t.innerHTML="<tr><th>Symbol</th><th>Kind</th><th>File</th></tr>"+(f.top_symbols||[]).map(function(sym){return "<tr><td class=mono>"+esc(sym.name)+"</td><td>"+esc(sym.kind)+"</td><td class=mono>"+esc(sym.file)+"</td></tr>";}).join("");
more.appendChild(t);
if((f.key_files||[]).length)more.appendChild(el("p","mono",(f.key_files||[]).map(esc).join("<br>")));
card.appendChild(more);}
grid.appendChild(card);});
sec.appendChild(grid);root.appendChild(sec);}
function renderCorrelations(s){
var sec=el("section","compose");sec.setAttribute("data-chaos-section","correlations");
sec.appendChild(el("h2",null,esc(s.title)));
sec.appendChild(el("p","sec-sub",esc(s.data.note||"")));
var shared=s.data.shared_files||[];
if(shared.length){var t=el("table");t.innerHTML="<tr><th>File</th><th>Shared by features</th></tr>"+shared.map(function(r){return "<tr><td class=mono>"+esc(r.file)+"</td><td>"+(r.features||[]).map(esc).join("<br>")+"</td></tr>";}).join("");sec.appendChild(t);}
else sec.appendChild(el("p","sec-sub","No files shared between the listed features' top files."));
var pages=s.data.related_pages||[];
if(pages.length){var grid=el("div","cards");pages.forEach(function(p){var c=el("div","card");c.appendChild(el("h3",null,esc(p.title||p.feature_id)));c.appendChild(el("p",null,esc(p.page)+" — "+((p.shared_files||[]).length)+" shared file(s), "+((p.shared_symbols||[]).length)+" shared symbol(s)"));grid.appendChild(c);});sec.appendChild(grid);}
root.appendChild(sec);}
function renderStack(s){
var d=s.data,sec=el("section","compose");sec.setAttribute("data-chaos-section","stack");
sec.appendChild(el("h2",null,esc(s.title)));
if(d.overview)sec.appendChild(el("p","sec-sub",esc(d.overview)));
var stats=el("div","stats");
[["packages","packages"],["manifests","manifests"],["scripts","scripts"],["deployment_resources","deploy resources"],["stacks","CDK stacks"]].forEach(function(pair){var v=(d.totals||{})[pair[0]];if(v==null)return;var st=el("div","stat");st.innerHTML="<b>"+v+"</b><span>"+esc(pair[1])+"</span>";stats.appendChild(st);});
sec.appendChild(stats);
var chips=el("div","chips");
((d.languages)||[]).forEach(function(l){chips.appendChild(el("span","chip",esc(l.name)+" · "+l.count));});
sec.appendChild(chips);
(d.ecosystems||[]).forEach(function(eco){
var more=el("details","more");more.innerHTML="<summary>"+esc(eco.ecosystem)+" — "+(eco.packages||[]).length+" package(s)</summary>";
if(LEVEL==="expert")more.open=true;
var t=el("table");t.innerHTML="<tr><th>Package</th><th>Versions</th><th>Scope</th></tr>"+(eco.packages||[]).map(function(p){return "<tr><td class=mono>"+esc(p.name)+"</td><td class=mono>"+esc((p.versions||[]).join(", "))+"</td><td>"+esc(p.scope||"")+"</td></tr>";}).join("");
more.appendChild(t);sec.appendChild(more);});
var cov=d.coverage||{};
if((cov.not_indexed||[]).length)sec.appendChild(el("div","warn","Not yet extracted by chaos (honest coverage gap): "+(cov.not_indexed||[]).map(esc).join("; ")));
root.appendChild(sec);}
(M.sections||[]).forEach(function(s){
if(s.kind==="features")renderFeatures(s);
else if(s.kind==="correlations")renderCorrelations(s);
else if(s.kind==="stack")renderStack(s);});
var prov=document.getElementById("provenance");
(M.provenance||[]).forEach(function(c){prov.appendChild(el("div","prov","<strong>"+esc(c.source)+"</strong> "+esc(c.method)+"<br>"+esc(c.detail)+(c.locator?'<br><span class="mono">'+esc(c.locator)+"</span>":"")));});
var warn=document.getElementById("warnings");
(M.warnings||[]).forEach(function(w){warn.appendChild(el("div","warn",esc(w)));});
})();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_section_is_a_loud_error() {
        let err = normalize_sections(&["features".into(), "vibes".into()])
            .expect_err("unknown section must fail");
        assert!(err.to_string().contains("unknown section 'vibes'"));
        assert!(err.to_string().contains("'stack'"));
    }

    #[test]
    fn empty_sections_is_an_error() {
        assert!(normalize_sections(&[]).is_err());
    }

    #[test]
    fn aliases_normalize_and_dedup() {
        let kinds = normalize_sections(&[
            "Feature-List".into(),
            "explanations".into(),
            "tech-stack".into(),
        ])
        .unwrap();
        assert_eq!(kinds, vec!["features".to_string(), "stack".to_string()]);
    }

    #[test]
    fn unknown_style_preset_resolves_to_none() {
        assert!(theme::style_preset("vaporwave").is_none());
        assert!(theme::style_preset("blade-runner").is_some());
        assert_eq!(theme::style_preset("editorial"), Some(""));
    }

    fn sample_manifest() -> ComposedManifest {
        ComposedManifest {
            schema_version: "composed-1".into(),
            repo_name: "demo".into(),
            title: "demo — composed: features".into(),
            subtitle: "subtitle".into(),
            request: json!({"sections": ["features"], "level": "beginner", "style": "blade-runner"}),
            sections: vec![ComposedSection {
                kind: "features".into(),
                title: "Features".into(),
                data: json!({"total": 1, "layer_counts": [{"layer": "core", "count": 1}],
                    "features": [{"id": "00000000-0000-0000-0000-000000000000", "label": "auth",
                        "layer": "core", "explanation": "Authentication and sessions.",
                        "member_count": 12, "languages": [], "folders": ["src"],
                        "top_symbols": [], "key_files": ["src/auth.rs"]}]}),
            }],
            site: None,
            content_hash: "abc123".into(),
            provenance: vec![Breadcrumb::new(source::POSTGRES, "test", "test")],
            warnings: Vec::new(),
        }
    }

    #[test]
    fn feature_page_renders_manifest_walkthrough_and_links() {
        let manifest = json!({
            "schema_version": "composed-feature-1",
            "feature": {"id": "00000000-0000-0000-0000-000000000000", "label": "poi-registry",
                "layer": "foundation", "explanation": "Proof-of-invention registry.",
                "member_count": 9, "languages": [{"language": "Solidity", "count": 3}],
                "folders": ["contracts"]},
            "request": {"level": "beginner", "persona": "newcomer", "style": "editorial"},
            "narrative": [{"title": "What this is", "body": "Proof-of-invention registry."}],
            "relations": [{"label": "labs-api", "direction": "used by", "kind": "calls",
                "weight": 2.5, "edge_count": 4, "layer": "interface", "languages": [],
                "page": "labs-api.html", "in_scope": true, "smart_contract": false}],
            "code": {"top_symbols": [], "key_files": ["contracts/POI.sol"]},
            "related_pages": [],
            "back": "../index-composed.html",
            "honesty": "Generated deterministically from the indexed graph.",
            "content_hash": "feedface",
        });
        let html = render_feature_page_html(
            &manifest,
            theme::style_preset("editorial").unwrap(),
            &theme::Brand::default(),
        )
        .unwrap();
        assert!(html.contains("chaos-composed-manifest"));
        assert!(html.contains("data-chaos-composed-feature"));
        assert!(html.contains("poi-registry"));
        assert!(html.contains("../index-composed.html"));
        // Round-trip: the per-page hash gate reads this back.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("poi-registry.html");
        fs::write(&path, &html).unwrap();
        assert_eq!(existing_content_hash(&path).as_deref(), Some("feedface"));
    }

    #[test]
    fn narrative_blocks_tag_smart_contract_relations() {
        let card = feature_inventory::FeatureCard {
            id: uuid::Uuid::nil(),
            label: "labs-api".into(),
            summary: Some("API surface.".into()),
            member_count: 4,
            role: "interface".into(),
            languages: vec![],
            top_symbols: vec![],
            key_files: vec!["src/api.ts".into()],
            folders: vec!["src".into()],
            matched_by: vec![],
            repo: None,
            cross_links: vec![],
        };
        let relations = vec![json!({
            "label": "access-resolver", "direction": "uses", "kind": "calls",
            "weight": 3.0, "edge_count": 2, "layer": "foundation",
            "languages": [{"language": "Solidity", "count": 2}],
            "page": null, "in_scope": true, "smart_contract": true,
        })];
        let blocks = narrative_blocks("beginner", "interface", &card, &relations);
        let connects = blocks
            .iter()
            .find(|b| b["title"] == "How it connects")
            .unwrap();
        assert!(connects["body"]
            .as_str()
            .unwrap()
            .contains("smart-contract side of the stack: access-resolver"));
        // Beginner gets the read-order hint.
        let sits = blocks
            .iter()
            .find(|b| b["title"] == "Where it sits")
            .unwrap();
        assert!(sits["body"]
            .as_str()
            .unwrap()
            .contains("entry → interface → core → foundation"));
    }

    #[test]
    fn rendered_page_embeds_manifest_and_style() {
        let html = render_composed_html(
            &sample_manifest(),
            theme::style_preset("blade-runner").unwrap(),
            &theme::Brand::default(),
        )
        .unwrap();
        assert!(html.contains("chaos-composed-manifest"));
        assert!(html.contains("blade-runner (dark neon token override)"));
        assert!(html.contains("data-chaos-composed"));
        assert!(html.contains("How this was generated"));
        // The manifest survives round-trip extraction (the agent-consumption path).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x-composed.html");
        fs::write(&path, &html).unwrap();
        assert_eq!(existing_content_hash(&path).as_deref(), Some("abc123"));
    }

    #[test]
    fn editorial_style_adds_no_override() {
        let html = render_composed_html(
            &sample_manifest(),
            theme::style_preset("editorial").unwrap(),
            &theme::Brand::default(),
        )
        .unwrap();
        assert!(!html.contains("blade-runner (dark neon token override)"));
    }

    #[test]
    fn missing_page_has_no_cached_hash() {
        assert_eq!(
            existing_content_hash(Path::new("/nonexistent/x.html")),
            None
        );
    }
}
