//! `chaos_components` — explain the CORE COMPONENTS of a big area.
//!
//! This is the **orientation step before feature extraction**. An area like
//! "OCL" is bigger than a single feature — it spans several L1 communities. This
//! tool zooms out one level: given an *area* description (or nothing, for a
//! repo-level overview) it surfaces the communities that make up the area as
//! "components", explains each (its L3 summary, key symbols/files, languages),
//! shows how they connect (the quotient graph), and proposes a dependency-first
//! **read order** so an agent can understand the subsystem before drilling into
//! any one feature.
//!
//! Like `chaos_impact` / `chaos_change_plan` it ALWAYS writes an interactive HTML
//! page (with the manifest embedded under `id="chaos-components-manifest"` so an
//! agent can extract it) and returns a COMPACT JSON summary, so an MCP caller's
//! context is not flooded.
//!
//! Everything here reuses the existing community / quotient-graph / provenance /
//! theme machinery — it is purely additive and embedder-light (one embed call,
//! only when an area string is given).

use crate::{
    embedding::Embedder,
    export_util::escape_script_json,
    feature_context::load_feature_matches,
    hierarchy_export::{CommunityDetail, CommunityHierarchy},
    provenance::{source, Breadcrumb},
    storage::Storage,
};
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

/// Semantic candidates below this cosine score are ignored (noise floor; same as
/// `chaos_change_plan`).
const MIN_SEMANTIC_SCORE: f64 = 0.30;
/// Default cap on components surfaced.
const DEFAULT_LIMIT: usize = 8;
/// Default representative members loaded per component.
const DEFAULT_TOP_MEMBERS: usize = 12;
/// Confidence floor for a component surfaced only by a label/summary keyword
/// match (it is real evidence, just weaker than a strong cosine hit).
const LEXICAL_FLOOR: f64 = 0.5;
/// Minimum token length used for the lexical area match (avoids noisy 1–2 char
/// substring hits).
const MIN_LEXICAL_TOKEN: usize = 3;
/// Minimum L3-summary cosine for two shown components to be drawn as "related".
const RELATED_THRESHOLD: f64 = 0.5;
/// Minimum cosine for a non-seed community to be pulled in as the area's "core".
const SEMANTIC_EXPAND_THRESHOLD: f64 = 0.55;
/// Max related-by-topic communities pulled in to surface a scattered area's core.
const SEMANTIC_EXPAND_BUDGET: usize = 3;

#[derive(Debug, Default, Clone)]
pub struct ComponentsOptions {
    pub output_html: Option<PathBuf>,
    /// Max components to surface.
    pub limit: usize,
    /// Representative members (symbols/files) loaded per component.
    pub top_members: usize,
}

/// The embedded + (compacted) returned manifest that "explains the core
/// components to agents".
#[derive(Debug, Clone, Serialize)]
pub struct ComponentsOverviewManifest {
    pub schema_version: String,
    /// The area description, if one was given (None = repo-level overview).
    pub area: Option<String>,
    pub repo_name: String,
    pub title: String,
    pub subtitle: String,
    /// Deterministic extractive overview paragraph.
    pub overview: String,
    pub components: Vec<ComponentCard>,
    pub relationships: Vec<ComponentLink>,
    pub related_features: Vec<RelatedFeaturePage>,
    pub provenance: Vec<Breadcrumb>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComponentCard {
    pub id: Uuid,
    pub label: String,
    pub summary: Option<String>,
    pub member_count: i32,
    /// The component's journey layer: `entry`, `interface`, `core`,
    /// `foundation`, or `unknown`.
    pub role: String,
    /// 1-based read order following the user journey (entry first, foundation last).
    pub read_order: usize,
    pub languages: Vec<LangCount>,
    pub top_symbols: Vec<ComponentSymbol>,
    pub key_files: Vec<String>,
    /// How this component was surfaced (cosine match / keyword match / top by
    /// size).
    pub matched_by: Vec<Breadcrumb>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LangCount {
    pub language: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComponentSymbol {
    pub name: String,
    pub kind: String,
    pub file: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComponentLink {
    pub source: String,
    pub target: String,
    pub kind: String,
    pub weight: f64,
    pub edge_count: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelatedFeaturePage {
    pub page: String,
    pub title: String,
    pub score: usize,
    /// Labels of the components whose files overlap this prior feature page.
    pub components: Vec<String>,
}

/// Run the components overview: select → describe → order → write HTML → compact
/// JSON.
pub async fn run(
    storage: &Storage,
    embedder: &dyn Embedder,
    repo: &str,
    area: Option<&str>,
    opts: &ComponentsOptions,
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
    let top_members = if opts.top_members > 0 {
        opts.top_members
    } else {
        DEFAULT_TOP_MEMBERS
    };
    let area = area.map(str::trim).filter(|a| !a.is_empty());

    let mut warnings: Vec<String> = Vec::new();
    let mut provenance: Vec<Breadcrumb> = Vec::new();

    // 1. Load the feature hierarchy once — it is the source of truth for every
    //    component's details (summary, top members) and the quotient edges among
    //    them. Read-only and embedder-free.
    let hierarchy: CommunityHierarchy =
        storage.load_community_hierarchy(&repo, top_members).await?;
    let detail_by_id: HashMap<Uuid, &CommunityDetail> =
        hierarchy.communities.iter().map(|c| (c.id, c)).collect();
    provenance.push(Breadcrumb::new(
        source::POSTGRES,
        "load_community_hierarchy",
        format!(
            "loaded {} community(ies) + {} quotient edge(s) from the persisted graph",
            hierarchy.communities.len(),
            hierarchy.edges.len()
        ),
    ));

    if hierarchy.communities.is_empty() {
        warnings.push(
            "no communities found — run chaos_analyze/chaos_add so the hierarchy (L1–L3) exists for this repo"
                .into(),
        );
    }

    // 2. Select the component communities.
    //    `ranked` is (id, rank_score, matched_by) in descending rank.
    let mut ranked: Vec<(Uuid, f64, Vec<Breadcrumb>)> = Vec::new();
    if let Some(area) = area {
        // 2a. Semantic match against L3 community summary embeddings.
        let query_embedding = embedder.embed(area).await?;
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
        let mut score_by_id: HashMap<Uuid, f64> = HashMap::new();
        for m in &semantic {
            if m.member_count >= 2 && m.score >= MIN_SEMANTIC_SCORE {
                score_by_id.insert(m.id, m.score);
            }
        }

        // 2b. Lexical match: area tokens vs each community's LABEL (path-derived).
        //     Catches a directory-named area ("OCL") whose summary embedding is
        //     weak. Matched against the label only — every L3 summary starts with
        //     the structural word "Feature:", so matching summaries would hit
        //     everything.
        let tokens = lexical_tokens(area);
        let mut lexical: HashSet<Uuid> = HashSet::new();
        if !tokens.is_empty() {
            for c in &hierarchy.communities {
                let label = c.label.to_lowercase();
                if tokens.iter().any(|t| label.contains(t.as_str())) {
                    lexical.insert(c.id);
                }
            }
        }

        let mut candidates: HashSet<Uuid> = score_by_id.keys().copied().collect();
        candidates.extend(lexical.iter().copied());
        for id in candidates {
            // Only communities that are real features (present in the hierarchy).
            if !detail_by_id.contains_key(&id) {
                continue;
            }
            let semantic_score = score_by_id.get(&id).copied();
            let is_lexical = lexical.contains(&id);
            let rank =
                semantic_score
                    .unwrap_or(0.0)
                    .max(if is_lexical { LEXICAL_FLOOR } else { 0.0 });
            let mut matched_by: Vec<Breadcrumb> = Vec::new();
            if let Some(s) = semantic_score {
                matched_by.push(Breadcrumb::new(
                    source::EMBEDDING,
                    "community_semantic_search",
                    format!("cosine {s:.2} vs the community summary embedding"),
                ));
            }
            if is_lexical {
                matched_by.push(Breadcrumb::new(
                    source::GRAPH,
                    "label_summary_match",
                    format!("the area `{area}` matched this community's label/summary"),
                ));
            }
            ranked.push((id, rank, matched_by));
        }

        provenance.push(Breadcrumb::new(
            source::EMBEDDING,
            "community_semantic_search",
            format!(
                "matched the area against community summary embeddings → {} component(s) (cosine ≥ {MIN_SEMANTIC_SCORE:.2})",
                score_by_id.len()
            ),
        ));
        provenance.push(Breadcrumb::new(
            source::GRAPH,
            "label_summary_match",
            format!(
                "keyword-matched the area against community labels → {} component(s)",
                lexical.len()
            ),
        ));

        if ranked.is_empty() && !hierarchy.communities.is_empty() {
            warnings.push(format!(
                "no components matched the area `{area}` — try a broader term, or omit the area for a repo-level overview"
            ));
        }
    } else {
        // 2c. No area: the repo's core components are the largest communities
        //     (hierarchy rows are already ordered by member_count desc).
        for c in hierarchy.communities.iter().take(limit) {
            ranked.push((
                c.id,
                c.member_count as f64,
                vec![Breadcrumb::new(
                    source::GRAPH,
                    "load_community_hierarchy",
                    format!("top community by size ({} members)", c.member_count),
                )],
            ));
        }
        provenance.push(Breadcrumb::new(
            source::GRAPH,
            "load_community_hierarchy",
            format!(
                "no area given → took the {} largest community(ies) as the repo's core components",
                ranked.len()
            ),
        ));
    }

    // 3. Rank + cap.
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                let am = detail_by_id.get(&a.0).map(|d| d.member_count).unwrap_or(0);
                let bm = detail_by_id.get(&b.0).map(|d| d.member_count).unwrap_or(0);
                bm.cmp(&am)
            })
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked.truncate(limit);

    // 3b. Semantic expansion. The relevance-ranked seeds for an area can be a
    //     scattered set with the actual "core" missing — a name/path match catches
    //     type files and config, not the central contract. Pull in a few non-seed
    //     communities that are strongly related BY MEANING to the seeds (L3 summary
    //     cosine) — relatedness that crosses repo/language lines where code edges
    //     can't — trading out the lowest-relevance seeds to respect `limit`. Needs
    //     L3 embeddings; skipped gracefully when they are absent. Only meaningful
    //     when an area was given: the no-area overview IS the largest communities,
    //     so expanding it would evict exactly the components it promises to show.
    let mut selected_ids: Vec<Uuid> = ranked.iter().map(|r| r.0).collect();
    if area.is_some() && !selected_ids.is_empty() {
        let expand_budget = (limit / 3).clamp(1, SEMANTIC_EXPAND_BUDGET);
        let neighbors: Vec<(Uuid, f64)> = storage
            .community_semantic_neighbors(
                repo.id,
                &selected_ids,
                embedder.provider(),
                embedder.model_id(),
                embedder.dimensions(),
                SEMANTIC_EXPAND_THRESHOLD,
                expand_budget as i64,
            )
            .await?
            .into_iter()
            .filter(|(id, _)| detail_by_id.contains_key(id))
            .collect();
        if !neighbors.is_empty() {
            let keep = limit.saturating_sub(neighbors.len()).max(1);
            selected_ids.truncate(keep);
            let mut have: HashSet<Uuid> = selected_ids.iter().copied().collect();
            for (id, score) in &neighbors {
                if have.insert(*id) {
                    selected_ids.push(*id);
                    ranked.push((
                        *id,
                        *score,
                        vec![Breadcrumb::new(
                            source::EMBEDDING,
                            "community_semantic_neighbors",
                            format!(
                                "pulled in as strongly related by topic to the area (cosine {score:.2})"
                            ),
                        )],
                    ));
                }
            }
            provenance.push(Breadcrumb::new(
                source::EMBEDDING,
                "community_semantic_neighbors",
                format!(
                    "added {} related-by-topic component(s) to surface the area's core (cosine ≥ {SEMANTIC_EXPAND_THRESHOLD:.2})",
                    neighbors.len()
                ),
            ));
        }
    }
    let selected_set: HashSet<Uuid> = selected_ids.iter().copied().collect();
    // Keep only the selected entries for card assembly (ranked may hold dropped
    // seeds; the related-by-topic additions were appended above).
    ranked.retain(|(id, _, _)| selected_set.contains(id));

    // 4. Layer each component (where it sits in the user's journey) and order it
    //    outside-in: entry → interface → core → foundation. Code-level edges
    //    can't give this order — they don't cross repo/language lines — so the
    //    order follows the journey, with community size as a stable within-layer
    //    tiebreak.
    let layer_by_id: HashMap<Uuid, crate::layering::Layer> = selected_ids
        .iter()
        .filter_map(|id| {
            detail_by_id
                .get(id)
                .map(|d| (*id, crate::layering::classify_community(&d.top_members)))
        })
        .collect();
    let mut order_ids: Vec<Uuid> = selected_ids.clone();
    order_ids.sort_by(|a, b| {
        let la = layer_by_id
            .get(a)
            .map(|l| l.rank())
            .unwrap_or(crate::layering::Layer::Unknown.rank());
        let lb = layer_by_id
            .get(b)
            .map(|l| l.rank())
            .unwrap_or(crate::layering::Layer::Unknown.rank());
        la.cmp(&lb)
            .then_with(|| {
                let am = detail_by_id.get(a).map(|d| d.member_count).unwrap_or(0);
                let bm = detail_by_id.get(b).map(|d| d.member_count).unwrap_or(0);
                bm.cmp(&am)
            })
            .then_with(|| a.cmp(b))
    });
    let read_order: HashMap<Uuid, usize> = order_ids
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, i + 1))
        .collect();
    if !selected_ids.is_empty() {
        provenance.push(Breadcrumb::new(
            source::GRAPH,
            "journey_order",
            format!(
                "ordered {} component(s) as a user journey — entry → interface → core → foundation",
                selected_ids.len()
            ),
        ));
    }

    // 5. Assemble component cards (role = the component's journey layer).
    let mut components: Vec<ComponentCard> = Vec::new();
    for (id, _rank, matched_by) in &ranked {
        let Some(detail) = detail_by_id.get(id) else {
            continue;
        };
        let top_symbols: Vec<ComponentSymbol> = detail
            .top_members
            .iter()
            .map(|(name, kind, file)| ComponentSymbol {
                name: name.clone(),
                kind: kind.clone(),
                file: file.clone(),
            })
            .collect();
        let key_files = distinct_files(&detail.top_members);
        let languages = language_tally(&key_files);
        components.push(ComponentCard {
            id: *id,
            label: detail.label.clone(),
            summary: detail.summary.clone(),
            member_count: detail.member_count,
            role: layer_by_id
                .get(id)
                .map(|l| l.as_str().to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            read_order: read_order.get(id).copied().unwrap_or(usize::MAX),
            languages,
            top_symbols,
            key_files,
            matched_by: matched_by.clone(),
        });
    }
    components.sort_by(|a, b| a.read_order.cmp(&b.read_order));

    // 6. Relationships between the shown components, of two kinds:
    //    • a real code dependency (quotient edge), kept directional; and
    //    • "related" — close by meaning (L3 summary cosine), undirected. The
    //      second is what links pieces across repos/languages that share no code
    //      edge — exactly the cross-boundary relatedness code graphs miss.
    let label_of: HashMap<Uuid, String> =
        components.iter().map(|c| (c.id, c.label.clone())).collect();
    let mut relationships: Vec<ComponentLink> = Vec::new();
    let mut linked_pairs: HashSet<(Uuid, Uuid)> = HashSet::new();
    for e in &hierarchy.edges {
        if selected_set.contains(&e.source) && selected_set.contains(&e.target) {
            linked_pairs.insert(unordered(e.source, e.target));
            relationships.push(ComponentLink {
                source: label_of
                    .get(&e.source)
                    .cloned()
                    .unwrap_or_else(|| e.source.to_string()),
                target: label_of
                    .get(&e.target)
                    .cloned()
                    .unwrap_or_else(|| e.target.to_string()),
                kind: e.kind.clone(),
                weight: e.weight,
                edge_count: e.edge_count,
            });
        }
    }
    let dep_count = relationships.len();
    // "Related by topic" links for component pairs with no code dependency.
    let similar = storage
        .community_pairwise_similarity(
            &selected_ids,
            embedder.provider(),
            embedder.model_id(),
            embedder.dimensions(),
        )
        .await?;
    for (a, b, score) in similar {
        if score < RELATED_THRESHOLD || linked_pairs.contains(&unordered(a, b)) {
            continue;
        }
        relationships.push(ComponentLink {
            source: label_of.get(&a).cloned().unwrap_or_else(|| a.to_string()),
            target: label_of.get(&b).cloned().unwrap_or_else(|| b.to_string()),
            kind: "related".to_string(),
            weight: score,
            edge_count: 0,
        });
    }
    let related_count = relationships.len() - dep_count;
    if related_count > 0 {
        provenance.push(Breadcrumb::new(
            source::EMBEDDING,
            "community_pairwise_similarity",
            format!(
                "linked {related_count} component pair(s) by topic (L3 summary cosine ≥ {RELATED_THRESHOLD:.2})"
            ),
        ));
    }

    // 7. Related prior feature pages that fall inside this area (reuse the
    //    existing manifest correlation). Surfaced so an agent sees which
    //    components already have feature docs before extracting more.
    let features_dir = repo_root.join("docs/features_memory");
    let correlate_text = area.unwrap_or(repo.name.as_str());
    let related_matches =
        load_feature_matches(correlate_text, &features_dir, limit.max(3), 24).unwrap_or_default();
    let related_features: Vec<RelatedFeaturePage> = related_matches
        .iter()
        .map(|m| {
            let page_files: HashSet<String> = m
                .matched_nodes
                .iter()
                .filter(|n| !n.file.is_empty())
                .map(|n| n.file.clone())
                .collect();
            let mut comps: Vec<String> = components
                .iter()
                .filter(|c| c.key_files.iter().any(|f| page_files.contains(f)))
                .map(|c| c.label.clone())
                .collect();
            comps.sort();
            comps.dedup();
            RelatedFeaturePage {
                page: m
                    .page
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string(),
                title: m.title.clone(),
                score: m.score,
                components: comps,
            }
        })
        .collect();
    if !related_matches.is_empty() {
        provenance.push(
            Breadcrumb::new(
                source::MANIFEST,
                "load_feature_matches",
                format!(
                    "correlated {} previously generated feature page(s) overlapping this area",
                    related_matches.len()
                ),
            )
            .with_locator(features_dir.display().to_string()),
        );
    }

    // 8. Overview paragraph + page framing.
    let overview = compose_overview(area, &repo.name, &components, dep_count, related_count);
    let title = match area {
        Some(a) => format!("{a} — core components"),
        None => format!("{} — core components", repo.name),
    };
    let subtitle = match area {
        Some(_) => {
            "The components this area is made of, ordered as a user journey — what you meet first (entry/UI) down to the foundation (contracts/infra). Lines show real code dependencies plus pieces related by topic. Use it to understand the subsystem before extracting any single feature.".to_string()
        }
        None => {
            "The repository's core components (its largest features), ordered as a user journey from entry points down to the foundation.".to_string()
        }
    };

    let manifest = ComponentsOverviewManifest {
        schema_version: "components-overview-1".to_string(),
        area: area.map(str::to_string),
        repo_name: repo.name.clone(),
        title,
        subtitle,
        overview,
        components,
        relationships,
        related_features,
        provenance,
        warnings: warnings.clone(),
    };

    // 9. Always write the HTML report.
    let output = opts.output_html.clone().unwrap_or_else(|| {
        let slug = match area {
            Some(a) => safe_slug(a),
            None => format!("{}-overview", safe_slug(&repo.name)),
        };
        repo_root
            .join("docs/features_memory")
            .join(format!("{slug}-components.html"))
    });
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    write_components_html(&output, &manifest)?;

    // 10. Compact JSON return (full detail stays in the HTML).
    let compact_components: Vec<Value> = manifest
        .components
        .iter()
        .map(|c| {
            json!({
                "label": c.label,
                "member_count": c.member_count,
                "role": c.role,
                "read_order": c.read_order,
                "top_symbols": c.top_symbols.iter().take(6).map(|s| s.name.clone()).collect::<Vec<_>>(),
                "matched_by": c.matched_by,
            })
        })
        .collect();
    let compact_relationships: Vec<Value> = manifest
        .relationships
        .iter()
        .map(|r| json!({"source": r.source, "target": r.target, "kind": r.kind}))
        .collect();

    Ok(json!({
        "status": "ok",
        "repo_id": repo.id,
        "area": manifest.area,
        "overview": manifest.overview,
        "component_count": manifest.components.len(),
        "components": compact_components,
        "relationships": compact_relationships,
        "related_features": manifest.related_features,
        "provenance": manifest.provenance,
        "output_html": output,
        "warnings": warnings,
    }))
}

/// Common short words that would over-match path-derived labels (e.g. "and"
/// inside "command.rs"); dropped from the lexical area match.
const STOPWORDS: &[&str] = &[
    "and", "the", "for", "with", "from", "into", "that", "this", "are", "was", "has", "you", "our",
    "its", "all", "any", "how", "via", "per", "out", "use",
];

/// Tokenize an area description for lexical matching (lowercase, length-gated,
/// stopword-filtered).
fn lexical_tokens(area: &str) -> Vec<String> {
    let mut tokens: Vec<String> = area
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|t| t.len() >= MIN_LEXICAL_TOKEN)
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .collect();
    tokens.sort();
    tokens.dedup();
    tokens
}

/// Distinct member file paths (ordered), the component's "key files".
fn distinct_files(members: &[(String, String, String)]) -> Vec<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for (_, _, path) in members {
        if !path.is_empty() && seen.insert(path.as_str()) {
            out.push(path.clone());
        }
    }
    out
}

/// Coarse language tally from file extensions (deterministic).
fn language_tally(files: &[String]) -> Vec<LangCount> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for f in files {
        let lang = language_for(f);
        if !lang.is_empty() {
            *counts.entry(lang).or_insert(0) += 1;
        }
    }
    let mut out: Vec<LangCount> = counts
        .into_iter()
        .map(|(language, count)| LangCount { language, count })
        .collect();
    // Most frequent first, then alphabetical for stability.
    out.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.language.cmp(&b.language))
    });
    out
}

fn language_for(path: &str) -> String {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => "Rust",
        "ts" | "tsx" => "TypeScript",
        "js" | "jsx" | "mjs" | "cjs" => "JavaScript",
        "py" => "Python",
        "sol" => "Solidity",
        "md" | "mdx" => "Markdown",
        "pdf" => "PDF",
        _ => "",
    }
    .to_string()
}

/// Order a community pair canonically (smaller id first) so an undirected link is
/// counted once regardless of which endpoint we saw first.
fn unordered(a: Uuid, b: Uuid) -> (Uuid, Uuid) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Deterministic extractive overview paragraph (pure — same inputs ⇒ same text).
/// `dep_count` = real code dependencies among the components; `related_count` =
/// links found by topic (semantic similarity).
fn compose_overview(
    area: Option<&str>,
    repo_name: &str,
    components: &[ComponentCard],
    dep_count: usize,
    related_count: usize,
) -> String {
    if components.is_empty() {
        return match area {
            Some(a) => format!(
                "No core components were identified for the area `{a}` in {repo_name}. Index the repository (chaos_analyze/chaos_add) so the feature hierarchy exists, or try a broader area term."
            ),
            None => format!(
                "No core components were identified in {repo_name}. Index the repository (chaos_analyze/chaos_add) so the feature hierarchy exists."
            ),
        };
    }
    let labels: Vec<&str> = components.iter().map(|c| c.label.as_str()).collect();
    let n = components.len();
    let mut ordered = components.to_vec();
    ordered.sort_by(|a, b| a.read_order.cmp(&b.read_order));
    let read_order: Vec<&str> = ordered.iter().map(|c| c.label.as_str()).collect();
    let head = match area {
        Some(a) => format!(
            "The \"{a}\" area in {repo_name} is made up of {n} core component(s): {}.",
            join_human(&labels)
        ),
        None => format!(
            "{repo_name} is made up of {n} core component(s): {}.",
            join_human(&labels)
        ),
    };
    let order_line = format!(
        " Read order follows the user journey (entry first, foundation last): {}.",
        read_order.join(" → ")
    );
    let rel_line = match (dep_count, related_count) {
        (0, 0) => " No code dependencies or topic links were found between them.".to_string(),
        (d, 0) => format!(" {d} code dependency link(s) connect them."),
        (0, r) => format!(" {r} link(s) connect them by topic (no direct code dependency)."),
        (d, r) => format!(
            " {} link(s) connect them: {d} by code dependency, {r} by topic.",
            d + r
        ),
    };
    format!("{head}{order_line}{rel_line}")
}

fn join_human(items: &[&str]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].to_string(),
        2 => format!("{} and {}", items[0], items[1]),
        _ => {
            let (last, head) = items.split_last().unwrap();
            format!("{}, and {}", head.join(", "), last)
        }
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
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "components".to_string()
    } else {
        slug.chars().take(80).collect::<String>()
    }
}

fn write_components_html(path: &Path, manifest: &ComponentsOverviewManifest) -> Result<()> {
    let json = serde_json::to_string(manifest)?;
    fs::write(
        path,
        COMPONENTS_HTML
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

const COMPONENTS_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Core components</title>
<style>
__THEME__
/* ===== components overview (light editorial) ===== */
header.ov{background:var(--bg-sky-soft);border-bottom:var(--border-hairline)}
header.ov .wrap{padding:48px 32px 36px}
header.ov .eyebrow{font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.16em;color:var(--color-blue-700);margin-bottom:16px;display:flex;align-items:center;gap:10px}
header.ov .eyebrow::before{content:"";width:22px;height:1px;background:var(--color-blue-500);display:inline-block}
header.ov h1{font:var(--type-display-lg);letter-spacing:-.01em;color:var(--color-ink-700);margin:0 0 10px}
#overview{font:var(--type-body-lg);color:var(--color-ink-500);line-height:1.55;max-width:78ch}
.sub{color:var(--color-ink-400);max-width:74ch;margin-top:14px;font:var(--type-body-sm);line-height:1.6}
main{padding:40px 0 64px;display:grid;gap:24px}
.panel{background:var(--color-surface-0);border:var(--border-hairline);border-radius:var(--radius-lg);box-shadow:var(--shadow-sm);padding:24px}
h2{font:var(--type-h4);color:var(--color-ink-700);margin:0 0 16px}
.muted{color:var(--fg-tertiary);line-height:1.5}
.stats{display:grid;grid-template-columns:repeat(auto-fit,minmax(160px,1fr));gap:16px}
.stat{border:var(--border-hairline);border-radius:var(--radius-md);background:var(--color-surface-2);padding:18px}
.stat b{display:block;font:var(--type-h2);font-family:var(--font-display);color:var(--color-ink-700);line-height:1}
.stat span{display:block;color:var(--fg-tertiary);font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.08em;margin-top:8px}
.comp{border:var(--border-hairline);border-radius:var(--radius-lg);background:var(--color-surface-0);padding:20px 22px;margin-top:14px;position:relative;box-shadow:var(--shadow-xs)}
.comp::before{content:"";position:absolute;left:0;top:0;bottom:0;width:3px;border-radius:var(--radius-lg) 0 0 var(--radius-lg);background:var(--color-blue-400)}
.comp h3{margin:0 0 4px;font:var(--type-h5);color:var(--color-ink-700);display:flex;align-items:center;flex-wrap:wrap;gap:8px}
.order{display:inline-flex;align-items:center;justify-content:center;width:28px;height:28px;border-radius:var(--radius-md);background:var(--color-ink-600);color:#fff;font:var(--type-overline-sm);font-family:var(--font-mono);font-weight:500}
.role{display:inline-flex;align-items:center;border-radius:var(--radius-pill);padding:3px 10px;font:var(--type-overline-sm);font-family:var(--font-mono);text-transform:uppercase;letter-spacing:.06em}
.role.entry{color:var(--color-blue-700);background:var(--color-blue-100)}
.role.interface{color:#9a6700;background:rgba(255,193,7,.16)}
.role.core{color:#007f76;background:rgba(0,200,187,.12)}
.role.foundation{color:var(--color-purple-500);background:var(--color-purple-100)}
.role.unknown,.role.standalone{color:var(--fg-tertiary);background:var(--color-surface-1)}
.summary{color:var(--color-ink-500);font:var(--type-body-sm);line-height:1.6;margin:10px 0;white-space:pre-wrap;max-height:150px;overflow:auto}
.chips{margin-top:8px}
.chip{display:inline-flex;align-items:center;gap:6px;border:var(--border-hairline);border-radius:var(--radius-pill);padding:4px 11px;margin:6px 6px 0 0;color:var(--color-ink-500);font:500 12px/1 var(--font-body);background:var(--color-surface-1)}
.chip .k{color:var(--fg-tertiary);font-family:var(--font-mono);font-size:10px;text-transform:uppercase;letter-spacing:.04em}
.lang{display:inline-flex;align-items:center;gap:6px;border-radius:var(--radius-pill);padding:3px 10px;margin:6px 6px 0 0;font:var(--type-overline-sm);font-family:var(--font-mono);background:var(--color-blue-50);color:var(--color-blue-700)}
.files{margin-top:8px;display:grid;gap:3px}
.files code{color:var(--color-ink-500);font:var(--type-body-xs);font-family:var(--font-mono)}
.matched{margin-top:10px;display:grid;gap:4px}
.matched div{color:var(--color-ink-500);font:var(--type-body-xs);line-height:1.5}
.matched b{color:var(--color-ink-700);font-weight:500;font-family:var(--font-mono);text-transform:uppercase;letter-spacing:.04em;font-size:10px}
.rel{border:var(--border-hairline);border-radius:var(--radius-md);background:var(--color-surface-1);padding:10px 14px;margin-top:8px;font:var(--type-body-sm);color:var(--color-ink-600)}
.rel .k{color:var(--fg-tertiary);font-family:var(--font-mono);font-size:11px}
.rel .arrow{color:var(--color-blue-500);font-family:var(--font-mono)}
.related{border:var(--border-hairline);border-radius:var(--radius-md);background:var(--color-surface-1);padding:12px 14px;margin-top:8px}
.related b{color:var(--color-ink-700);font:var(--type-h6)}
.related .meta{color:var(--fg-tertiary);font:var(--type-body-xs);margin-top:3px}
.item.warn{border:1px solid var(--color-blue-300);border-radius:var(--radius-md);background:var(--color-blue-50);padding:14px 16px;margin-top:12px}
.item.warn strong{color:var(--color-blue-700);font:var(--type-h6);display:block;margin-bottom:4px}
.item.warn div{color:var(--color-ink-500);font:var(--type-body-sm);line-height:1.5}
</style>
</head>
<body data-chaos-components>
<div class="topbar"><div class="wrap">__BRAND_TOPBAR__<span class="crumb">Components<span class="sep">&rsaquo;</span><b>overview</b></span><span class="sp"></span><span class="pilltag">Components</span></div></div>

<header class="ov">
  <div class="wrap">
    <div class="eyebrow" id="eyebrow">Core components</div>
    <h1 id="title">Core components</h1>
    <div id="overview"></div>
    <div class="sub" id="subtitle"></div>
  </div>
</header>

<main>
  <div class="wrap">
    <section class="panel"><div id="stats" class="stats"></div></section>
    <section class="panel" data-comp-list><h2>Components, in journey order</h2><div class="muted" style="margin-bottom:10px">Ordered the way you meet the feature: entry (UI/CLI) &rarr; interface (API) &rarr; core (logic) &rarr; foundation (contracts/infra).</div><div id="components"></div></section>
    <section class="panel" data-comp-rel><h2>How the components connect</h2><div class="muted" style="margin-bottom:10px">Real code dependencies, plus pieces related by topic &mdash; the cross-boundary links a code graph misses.</div><div id="relationships"></div></section>
    <section class="panel" data-comp-related><h2>Related feature pages</h2><div class="muted" style="margin-bottom:10px">Previously generated feature pages whose files overlap this area.</div><div id="related"></div></section>
    <section class="panel" data-comp-provenance><h2>How this was generated</h2><div class="muted" style="margin-bottom:10px">Provenance breadcrumbs &mdash; the steps that produced this overview.</div><div id="provenance"></div></section>
    <section class="panel"><h2>Warnings</h2><div id="warnings"></div></section>
  </div>
</main>

<footer><div class="wrap">__BRAND_FOOTER__<span class="sp"></span><span class="meta">generated by Chaos Substrate</span></div></footer>

<script type="application/json" id="chaos-components-manifest">__DATA__</script>
<script>
(function(){
var D=JSON.parse(document.getElementById("chaos-components-manifest").textContent);
function esc(v){return String(v==null?"":v).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;").replace(/'/g,"&#039;");}
document.getElementById("title").textContent=D.title||"Core components";
document.getElementById("eyebrow").textContent=D.area?("Area · "+D.area):"Repository overview";
document.getElementById("overview").textContent=D.overview||"";
document.getElementById("subtitle").textContent=D.subtitle||"";
var C=D.components||[];
var stat=[[C.length,"components"],[(D.relationships||[]).length,"relationships"],[(D.related_features||[]).length,"related pages"],[(D.warnings||[]).length,"warnings"]];
document.getElementById("stats").innerHTML=stat.map(function(s){return '<div class="stat"><b>'+s[0]+'</b><span>'+s[1]+'</span></div>';}).join("");
var host=document.getElementById("components");
C.forEach(function(c){
  var el=document.createElement("div");el.className="comp";
  var role=(c.role||"core");
  var syms=(c.top_symbols||[]).map(function(s){return '<span class="chip">'+esc(s.name)+' <span class="k">'+esc(s.kind)+'</span></span>';}).join("");
  var langs=(c.languages||[]).map(function(l){return '<span class="lang">'+esc(l.language)+' &middot; '+l.count+'</span>';}).join("");
  var files=(c.key_files||[]).slice(0,8).map(function(f){return '<code>'+esc(f)+'</code>';}).join("");
  var matched=(c.matched_by||[]).map(function(m){return '<div><b>'+esc(m.source)+'</b> '+esc(m.detail)+'</div>';}).join("");
  el.innerHTML='<h3><span class="order">'+(c.read_order||"?")+'</span>'+esc(c.label)+' <span class="role '+esc(role)+'">'+esc(role)+'</span></h3>'+
    '<div class="muted">'+c.member_count+' symbols</div>'+
    (langs?'<div class="chips">'+langs+'</div>':'')+
    (c.summary?'<div class="summary">'+esc(c.summary)+'</div>':'')+
    (syms?'<div class="chips">'+syms+'</div>':'')+
    (files?'<div class="files">'+files+'</div>':'')+
    (matched?'<div class="matched">'+matched+'</div>':'');
  host.appendChild(el);
});
if(!host.children.length)host.innerHTML='<div class="muted">No components identified. Ensure the repo is indexed (chaos_analyze) so communities + summaries exist, and try a broader area or omit it.</div>';
var rel=document.getElementById("relationships");
(D.relationships||[]).forEach(function(r){
  var el=document.createElement("div");el.className="rel";
  if(r.kind==="related"){
    var pct=Math.round((r.weight||0)*100);
    el.innerHTML='<b>'+esc(r.source)+'</b> <span class="arrow">&harr; related &harr;</span> <b>'+esc(r.target)+'</b> <span class="k">(by topic &middot; '+pct+'% similar)</span>';
  }else{
    el.innerHTML='<b>'+esc(r.source)+'</b> <span class="arrow">&mdash;'+esc(r.kind)+'&rarr;</span> <b>'+esc(r.target)+'</b> <span class="k">(code dependency &middot; weight '+(Math.round((r.weight||0)*100)/100)+', '+r.edge_count+' edges)</span>';
  }
  rel.appendChild(el);
});
if(!rel.children.length)rel.innerHTML='<div class="muted">No code dependencies or topic links among these components.</div>';
var related=document.getElementById("related");
(D.related_features||[]).forEach(function(r){var el=document.createElement("div");el.className="related";el.innerHTML='<b>'+esc(r.title)+'</b><div class="meta">'+esc(r.page)+(r.components&&r.components.length?' &middot; touches '+esc(r.components.join(", ")):'')+' &middot; overlap '+r.score+'</div>';related.appendChild(el);});
if(!related.children.length)related.innerHTML='<div class="muted">No previously generated feature pages overlap this area yet.</div>';
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

    fn card(label: &str, read_order: usize, role: &str) -> ComponentCard {
        ComponentCard {
            id: Uuid::from_u128(read_order as u128),
            label: label.to_string(),
            summary: None,
            member_count: 5,
            role: role.to_string(),
            read_order,
            languages: Vec::new(),
            top_symbols: Vec::new(),
            key_files: Vec::new(),
            matched_by: Vec::new(),
        }
    }

    #[test]
    fn overview_is_deterministic_and_grounded() {
        let comps = vec![
            card("access-control", 1, "entry"),
            card("token-issuance", 2, "core"),
        ];
        let a = compose_overview(Some("OCL"), "molecule_core", &comps, 1, 1);
        let b = compose_overview(Some("OCL"), "molecule_core", &comps, 1, 1);
        assert_eq!(a, b, "same inputs => identical overview text");
        assert!(a.contains("OCL"));
        assert!(a.contains("access-control"));
        assert!(a.contains("token-issuance"));
        assert!(a.contains("access-control → token-issuance"));
        // Journey wording, both link kinds reported.
        assert!(a.contains("user journey"));
        assert!(a.contains("by code dependency") && a.contains("by topic"));
    }

    #[test]
    fn overview_handles_empty() {
        let text = compose_overview(None, "repo", &[], 0, 0);
        assert!(text.contains("No core components"));
    }

    #[test]
    fn lexical_tokens_drops_short_and_lowercases() {
        let t = lexical_tokens("OCL on-chain X");
        assert!(t.contains(&"ocl".to_string()));
        assert!(t.contains(&"chain".to_string()));
        assert!(!t.iter().any(|x| x == "x")); // too short
    }

    #[test]
    fn language_tally_counts_by_extension() {
        let files = vec![
            "src/a.rs".to_string(),
            "src/b.rs".to_string(),
            "ui/c.tsx".to_string(),
        ];
        let langs = language_tally(&files);
        assert_eq!(langs[0].language, "Rust");
        assert_eq!(langs[0].count, 2);
    }

    #[test]
    fn unordered_is_canonical() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        assert_eq!(unordered(a, b), unordered(b, a));
        assert_eq!(unordered(b, a), (a, b));
    }
}
