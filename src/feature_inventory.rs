//! `chaos features` — the exhaustive god-node **feature inventory**.
//!
//! Where `chaos_components` gives a *curated* orientation overview of one
//! subsystem (capped, journey-ordered, seed-traded), this answers a different
//! question: *"give me ALL the features [in this layer / under this folder / about
//! this topic]."* It lists every L1 community (god-node / feature) that matches a
//! filter, grouped by where it sits in the user journey — no cap, no curation.
//!
//! The single positional filter is **auto-detected** (a caller can also force it
//! with `--layer` / `--folder` / `--topic`):
//!   * contains a path separator, or names a real directory ⇒ **folder** scope
//!     (features whose code lives under it — `communities_under_paths`);
//!   * a single known layer word like `client`/`ui`/`api`/`core`/`contracts`
//!     ⇒ **layer** filter (`layering::layer_from_query`) — so "client features"
//!     means "every entry-layer feature";
//!   * anything else ⇒ **topic** match (summary-embedding cosine + label/summary
//!     keywords), exhaustive (no top-N cap);
//!   * nothing ⇒ the whole repo, grouped by layer.
//!
//! Like the other surfacing tools it ALWAYS writes an interactive HTML page to
//! `docs/features_memory/<slug>-features.html` and returns a COMPACT JSON
//! summary, with provenance breadcrumbs throughout. Everything reuses the
//! existing community / layering / theme / provenance machinery; the only path
//! that needs the embedder is a topic filter, so layer/folder/whole-repo listing
//! works embedder-free (like `chaos_stats`).

use crate::{
    embedding::Embedder,
    export_util::escape_script_json,
    hierarchy_export::{CommunityDetail, CommunityHierarchy},
    layering::{self, Layer},
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

/// Topic-match cosine noise floor (same as `chaos_components` / `chaos_change_plan`).
const MIN_SEMANTIC_SCORE: f64 = 0.30;
/// Representative members loaded per feature — generous so the layer vote and the
/// "folders spanned" display are well-grounded, not decided by a handful of rows.
const TOP_MEMBERS: usize = 48;
/// Minimum token length for a lexical topic match (drops noisy 1–2 char hits).
const MIN_LEXICAL_TOKEN: usize = 3;

#[derive(Debug, Default, Clone)]
pub struct FeatureInventoryOptions {
    pub output_html: Option<PathBuf>,
    /// Cap on features surfaced. `0` = no cap (the default — this tool is meant to
    /// be exhaustive); a positive value keeps the largest features.
    pub limit: usize,
    /// Force a layer filter (overrides auto-detection of the positional filter).
    pub layer: Option<String>,
    /// Force a folder filter.
    pub folder: Option<String>,
    /// Force a topic filter.
    pub topic: Option<String>,
}

/// How the filter was interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterKind {
    All,
    Layer,
    Folder,
    Topic,
}

impl FilterKind {
    fn as_str(self) -> &'static str {
        match self {
            FilterKind::All => "all",
            FilterKind::Layer => "layer",
            FilterKind::Folder => "folder",
            FilterKind::Topic => "topic",
        }
    }
}

struct ResolvedFilter {
    kind: FilterKind,
    value: Option<String>,
    /// True when the kind was inferred from the positional filter (vs forced by a flag).
    detected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeatureInventoryManifest {
    pub schema_version: String,
    pub repo_name: String,
    pub title: String,
    pub subtitle: String,
    pub filter: FilterInfo,
    pub overview: String,
    pub total: usize,
    pub layer_counts: Vec<LayerCount>,
    pub language_counts: Vec<LangCount>,
    pub groups: Vec<LayerGroup>,
    pub provenance: Vec<Breadcrumb>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FilterInfo {
    pub kind: String,
    pub value: Option<String>,
    pub detected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LayerCount {
    pub layer: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct LangCount {
    pub language: String,
    pub count: usize,
}

/// One journey-layer section (entry → interface → core → foundation → unknown).
#[derive(Debug, Clone, Serialize)]
pub struct LayerGroup {
    /// Stable layer key (`entry`/`interface`/`core`/`foundation`/`unknown`).
    pub layer: String,
    /// Human heading for the section.
    pub label: String,
    pub features: Vec<FeatureCard>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeatureCard {
    pub id: Uuid,
    pub label: String,
    pub summary: Option<String>,
    pub member_count: i32,
    /// Journey layer of this feature (its group).
    pub role: String,
    pub languages: Vec<LangCount>,
    pub top_symbols: Vec<FeatureSymbol>,
    pub key_files: Vec<String>,
    /// Distinct top-level folders the feature's code spans.
    pub folders: Vec<String>,
    /// Why this feature is in the result (cosine / keyword / layer / folder).
    pub matched_by: Vec<Breadcrumb>,
    /// Member-repo alias, present only in a project-wide listing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Cross-repo link notes ("→ backend:auth-api (http_route)"), project mode only.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cross_links: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeatureSymbol {
    pub name: String,
    pub kind: String,
    pub file: String,
}

/// Run the single-repo inventory: resolve filter → select features → group by
/// layer → write HTML → return compact JSON.
pub async fn run(
    storage: &Storage,
    embedder: Option<&dyn Embedder>,
    repo: &str,
    filter: Option<&str>,
    opts: &FeatureInventoryOptions,
) -> Result<Value> {
    let repo = storage
        .find_repository(repo)
        .await?
        .with_context(|| format!("repository is not indexed: {repo}"))?;
    let repo_root = PathBuf::from(&repo.root_path);

    let mut warnings: Vec<String> = Vec::new();
    let mut provenance: Vec<Breadcrumb> = Vec::new();

    // 1. Load every feature community once (read-only, embedder-free).
    let hierarchy: CommunityHierarchy =
        storage.load_community_hierarchy(&repo, TOP_MEMBERS).await?;
    let detail_by_id: HashMap<Uuid, &CommunityDetail> =
        hierarchy.communities.iter().map(|c| (c.id, c)).collect();
    provenance.push(Breadcrumb::new(
        source::POSTGRES,
        "load_community_hierarchy",
        format!(
            "loaded {} feature community(ies) from the persisted graph",
            hierarchy.communities.len()
        ),
    ));
    if hierarchy.communities.is_empty() {
        warnings.push(
            "no features found — run chaos_analyze/chaos_add so the L1 community hierarchy exists for this repo"
                .into(),
        );
    }

    // 2. Resolve how to read the filter.
    let resolved = resolve_filter(filter, opts, std::slice::from_ref(&repo_root));
    push_filter_breadcrumb(&resolved, &mut provenance);

    // 3–5. Select, build cards, assemble (shared with the project listing).
    let topic_embedding = topic_embedding_for(embedder, &resolved).await?;
    let selected = select_features(
        storage,
        embedder,
        repo.id,
        &hierarchy,
        &detail_by_id,
        &resolved,
        topic_embedding.as_deref(),
        &mut provenance,
        &mut warnings,
    )
    .await?;
    let cards = build_cards(&selected, &detail_by_id, None, &HashMap::new());

    let output = opts.output_html.clone().unwrap_or_else(|| {
        default_output(
            &repo_root.join("docs/features_memory"),
            &resolved,
            &repo.name,
        )
    });
    assemble_and_write(
        cards,
        &resolved,
        &repo.name,
        provenance,
        warnings,
        &output,
        opts.limit,
        json!({ "repo_id": repo.id }),
    )
}

/// Run the PROJECT-WIDE inventory: every member repo's features in one
/// journey-layered listing, each card tagged with its repo alias and annotated
/// with the project's cross-repo links. Writes the HTML into the project
/// workspace (no single repo's docs/ can own a multi-repo page).
pub async fn run_project(
    storage: &Storage,
    embedder: Option<&dyn Embedder>,
    project_name: &str,
    filter: Option<&str>,
    opts: &FeatureInventoryOptions,
) -> Result<Value> {
    let project = storage
        .find_project(project_name)
        .await?
        .with_context(|| format!("project does not exist: {project_name}"))?;
    let members = storage.project_member_repos(project.id).await?;
    anyhow::ensure!(
        !members.is_empty(),
        "project {} has no repositories — add one with `chaos project add-repo {} <repo-path>`",
        project.name,
        project.name
    );

    let mut warnings: Vec<String> = Vec::new();
    let mut provenance: Vec<Breadcrumb> = Vec::new();

    let roots: Vec<PathBuf> = members
        .iter()
        .map(|m| PathBuf::from(&m.repo.root_path))
        .collect();
    let resolved = resolve_filter(filter, opts, &roots);
    push_filter_breadcrumb(&resolved, &mut provenance);

    // Cross-repo link annotations: community id → human-readable notes.
    let links = storage.load_project_links(project.id).await?;
    let alias_by_repo: HashMap<Uuid, String> = members
        .iter()
        .map(|m| (m.repo.id, m.alias.clone()))
        .collect();
    let mut link_ids: Vec<Uuid> = links
        .iter()
        .flat_map(|l| [l.source_community_id, l.target_community_id])
        .collect();
    link_ids.sort();
    link_ids.dedup();
    let labels = storage.community_labels_for(&link_ids).await?;
    let mut cross: HashMap<Uuid, Vec<String>> = HashMap::new();
    for l in &links {
        let alias = |repo: &Uuid| alias_by_repo.get(repo).map(String::as_str).unwrap_or("?");
        let label = |c: &Uuid| labels.get(c).map(String::as_str).unwrap_or("?");
        cross
            .entry(l.source_community_id)
            .or_default()
            .push(format!(
                "→ {}:{} ({})",
                alias(&l.target_repo_id),
                label(&l.target_community_id),
                l.kind
            ));
        cross
            .entry(l.target_community_id)
            .or_default()
            .push(format!(
                "← {}:{} ({})",
                alias(&l.source_repo_id),
                label(&l.source_community_id),
                l.kind
            ));
    }
    for notes in cross.values_mut() {
        notes.sort();
        notes.dedup();
        notes.truncate(6);
    }
    provenance.push(Breadcrumb::new(
        source::POSTGRES,
        "load_project_links",
        format!(
            "annotated features with {} persisted cross-repo link(s)",
            links.len()
        ),
    ));

    // One embed call for the topic filter, shared by every member repo.
    let topic_embedding = topic_embedding_for(embedder, &resolved).await?;
    let mut all_cards: Vec<(Layer, FeatureCard)> = Vec::new();
    let mut repo_summaries: Vec<Value> = Vec::new();
    for m in &members {
        let hierarchy: CommunityHierarchy = storage
            .load_community_hierarchy(&m.repo, TOP_MEMBERS)
            .await?;
        let detail_by_id: HashMap<Uuid, &CommunityDetail> =
            hierarchy.communities.iter().map(|c| (c.id, c)).collect();
        provenance.push(Breadcrumb::new(
            source::POSTGRES,
            "load_community_hierarchy",
            format!(
                "{}: loaded {} feature community(ies)",
                m.alias,
                hierarchy.communities.len()
            ),
        ));
        if hierarchy.communities.is_empty() {
            warnings.push(format!(
                "{}: no features — run chaos_analyze/chaos_add on it so its hierarchy exists",
                m.alias
            ));
        }
        let selected = select_features(
            storage,
            embedder,
            m.repo.id,
            &hierarchy,
            &detail_by_id,
            &resolved,
            topic_embedding.as_deref(),
            &mut provenance,
            &mut warnings,
        )
        .await?;
        let cards = build_cards(&selected, &detail_by_id, Some(&m.alias), &cross);
        repo_summaries.push(json!({ "alias": m.alias, "features": cards.len() }));
        all_cards.extend(cards);
    }

    let workspace = crate::project::project_workspace_dir(&project.name);
    let output = opts
        .output_html
        .clone()
        .unwrap_or_else(|| default_output(&workspace, &resolved, &project.name));
    assemble_and_write(
        all_cards,
        &resolved,
        &project.name,
        provenance,
        warnings,
        &output,
        opts.limit,
        json!({
            "project": project.name,
            "repos": repo_summaries,
            "cross_repo_links": links.len(),
        }),
    )
}

fn push_filter_breadcrumb(resolved: &ResolvedFilter, provenance: &mut Vec<Breadcrumb>) {
    if let Some(v) = &resolved.value {
        provenance.push(Breadcrumb::new(
            source::GRAPH,
            "resolve_filter",
            format!(
                "read `{v}` as a {} filter{}",
                resolved.kind.as_str(),
                if resolved.detected {
                    " (auto-detected)"
                } else {
                    " (forced)"
                }
            ),
        ));
    }
}

/// Embed a topic filter ONCE for the whole listing — `select_features` runs per
/// repo in project mode and must not re-embed the identical query N times.
async fn topic_embedding_for(
    embedder: Option<&dyn Embedder>,
    resolved: &ResolvedFilter,
) -> Result<Option<Vec<f32>>> {
    if resolved.kind != FilterKind::Topic {
        return Ok(None);
    }
    let (Some(emb), Some(value)) = (embedder, resolved.value.as_deref()) else {
        return Ok(None);
    };
    Ok(Some(emb.embed(value).await?))
}

/// Select the matching feature ids of ONE repo, each with a "why it's here"
/// breadcrumb. Shared by the single-repo and project-wide listings.
#[allow(clippy::too_many_arguments)]
async fn select_features(
    storage: &Storage,
    embedder: Option<&dyn Embedder>,
    repo_id: Uuid,
    hierarchy: &CommunityHierarchy,
    detail_by_id: &HashMap<Uuid, &CommunityDetail>,
    resolved: &ResolvedFilter,
    topic_embedding: Option<&[f32]>,
    provenance: &mut Vec<Breadcrumb>,
    warnings: &mut Vec<String>,
) -> Result<Vec<(Uuid, Vec<Breadcrumb>)>> {
    let mut selected: Vec<(Uuid, Vec<Breadcrumb>)> = Vec::new();
    match resolved.kind {
        FilterKind::All => {
            for c in &hierarchy.communities {
                selected.push((c.id, Vec::new()));
            }
        }
        FilterKind::Layer => {
            let value = resolved.value.clone().unwrap_or_default();
            let want = match layering::layer_from_query(&value) {
                Some(l) => l,
                None => {
                    warnings.push(format!(
                        "unrecognized layer `{value}` — recognized layers are entry (client/ui/web/cli), interface (api/resolver/route), core (service/logic/domain), foundation (contract/infra/config); showing features that couldn't be layered"
                    ));
                    Layer::Unknown
                }
            };
            for c in &hierarchy.communities {
                if layering::classify_community(&c.top_members) == want {
                    selected.push((
                        c.id,
                        vec![Breadcrumb::new(
                            source::GRAPH,
                            "classify_community",
                            format!(
                                "classified as the `{}` layer from its members' paths",
                                want.as_str()
                            ),
                        )],
                    ));
                }
            }
            provenance.push(Breadcrumb::new(
                source::GRAPH,
                "classify_community",
                format!(
                    "kept the {} feature(s) in the `{}` journey layer",
                    selected.len(),
                    want.as_str()
                ),
            ));
            if selected.is_empty() && !hierarchy.communities.is_empty() {
                warnings.push(format!(
                    "no features sit in the `{}` layer — try another layer or a folder/topic filter",
                    want.as_str()
                ));
            }
        }
        FilterKind::Folder => {
            let value = resolved.value.clone().unwrap_or_default();
            let ids: HashSet<Uuid> = storage
                .communities_under_paths(repo_id, std::slice::from_ref(&value))
                .await?
                .into_iter()
                .collect();
            for c in &hierarchy.communities {
                if ids.contains(&c.id) {
                    selected.push((
                        c.id,
                        vec![Breadcrumb::new(
                            source::POSTGRES,
                            "communities_under_paths",
                            format!("has member code under `{value}`"),
                        )],
                    ));
                }
            }
            provenance.push(Breadcrumb::new(
                source::POSTGRES,
                "communities_under_paths",
                format!(
                    "matched {} feature(s) with code under the folder `{value}`",
                    selected.len()
                ),
            ));
            if selected.is_empty() && !hierarchy.communities.is_empty() {
                warnings.push(format!(
                    "no features have code under `{value}` — check the path, or omit it for the whole repo"
                ));
            }
        }
        FilterKind::Topic => {
            let value = resolved.value.clone().unwrap_or_default();
            // 3a. Semantic match against the L3 summary embeddings (if available).
            let mut score_by_id: HashMap<Uuid, f64> = HashMap::new();
            if let (Some(emb), Some(query_embedding)) = (embedder, topic_embedding) {
                let cap = hierarchy.communities.len().max(12) as i64;
                let semantic = storage
                    .community_semantic_search(
                        repo_id,
                        emb.provider(),
                        emb.model_id(),
                        emb.dimensions(),
                        query_embedding,
                        cap,
                    )
                    .await?;
                for m in &semantic {
                    if m.member_count >= 2 && m.score >= MIN_SEMANTIC_SCORE {
                        score_by_id.insert(m.id, m.score);
                    }
                }
                provenance.push(Breadcrumb::new(
                    source::EMBEDDING,
                    "community_semantic_search",
                    format!(
                        "matched `{value}` against feature summary embeddings → {} feature(s) (cosine ≥ {MIN_SEMANTIC_SCORE:.2})",
                        score_by_id.len()
                    ),
                ));
            } else {
                warnings.push(
                    "no embedder configured — topic match used feature labels/summaries only; configure OpenAI/Ollama for semantic matching"
                        .into(),
                );
            }
            // 3b. Lexical match over label + summary text.
            let tokens = lexical_tokens(&value);
            let mut lexical: HashSet<Uuid> = HashSet::new();
            if !tokens.is_empty() {
                for c in &hierarchy.communities {
                    // Match the feature's OWN label/summary only — the v3
                    // summary's trailing "Related features: …" line names
                    // neighbors, and matching it would list every neighbor of
                    // a feature named after the topic as a hit.
                    let own_summary = c
                        .summary
                        .as_deref()
                        .unwrap_or("")
                        .split("Related features:")
                        .next()
                        .unwrap_or("");
                    let hay = format!("{} {}", c.label.to_lowercase(), own_summary.to_lowercase());
                    if tokens.iter().any(|t| hay.contains(t.as_str())) {
                        lexical.insert(c.id);
                    }
                }
            }
            provenance.push(Breadcrumb::new(
                source::GRAPH,
                "label_summary_match",
                format!(
                    "keyword-matched `{value}` against feature labels/summaries → {} feature(s)",
                    lexical.len()
                ),
            ));

            let mut candidates: Vec<Uuid> = score_by_id.keys().copied().collect();
            candidates.extend(lexical.iter().copied());
            candidates.sort();
            candidates.dedup();
            for id in candidates {
                if !detail_by_id.contains_key(&id) {
                    continue;
                }
                let mut matched_by: Vec<Breadcrumb> = Vec::new();
                if let Some(s) = score_by_id.get(&id) {
                    matched_by.push(Breadcrumb::new(
                        source::EMBEDDING,
                        "community_semantic_search",
                        format!("cosine {s:.2} vs the feature summary embedding"),
                    ));
                }
                if lexical.contains(&id) {
                    matched_by.push(Breadcrumb::new(
                        source::GRAPH,
                        "label_summary_match",
                        format!("`{value}` matched this feature's label/summary"),
                    ));
                }
                selected.push((id, matched_by));
            }
            if selected.is_empty() && !hierarchy.communities.is_empty() {
                warnings.push(format!(
                    "nothing matched `{value}` — try a folder (e.g. packages/client), a layer (client/api/core/contracts), or a broader term"
                ));
            }
        }
    }

    Ok(selected)
}

/// Build layer-classified cards from selected feature ids. `repo_alias` tags
/// cards in a project-wide listing; `cross_links` carries the project's
/// link annotations (empty in single-repo mode).
fn build_cards(
    selected: &[(Uuid, Vec<Breadcrumb>)],
    detail_by_id: &HashMap<Uuid, &CommunityDetail>,
    repo_alias: Option<&str>,
    cross_links: &HashMap<Uuid, Vec<String>>,
) -> Vec<(Layer, FeatureCard)> {
    let mut cards: Vec<(Layer, FeatureCard)> = Vec::new();
    for (id, matched_by) in selected {
        let Some(detail) = detail_by_id.get(id) else {
            continue;
        };
        let layer = layering::classify_community(&detail.top_members);
        let key_files = distinct_files(&detail.top_members);
        let languages = language_tally(&key_files);
        let folders = top_folders(&key_files);
        let top_symbols: Vec<FeatureSymbol> = detail
            .top_members
            .iter()
            .take(8)
            .map(|(name, kind, file)| FeatureSymbol {
                name: name.clone(),
                kind: kind.clone(),
                file: file.clone(),
            })
            .collect();
        cards.push((
            layer,
            FeatureCard {
                id: *id,
                label: detail.label.clone(),
                summary: detail.summary.clone(),
                member_count: detail.member_count,
                role: layer.as_str().to_string(),
                languages,
                top_symbols,
                key_files: key_files.into_iter().take(8).collect(),
                folders,
                matched_by: matched_by.clone(),
                repo: repo_alias.map(String::from),
                cross_links: cross_links.get(id).cloned().unwrap_or_default(),
            },
        ));
    }
    cards
}

/// Default `<dir>/<slug>-features.html` output path.
fn default_output(dir: &Path, resolved: &ResolvedFilter, name: &str) -> PathBuf {
    let slug = match &resolved.value {
        Some(v) => format!("{}-{}", safe_slug(v), resolved.kind.as_str()),
        None => safe_slug(name),
    };
    dir.join(format!("{slug}-features.html"))
}

/// Group cards by journey layer, write the HTML inventory, and return the
/// compact JSON. `extra` keys (repo_id / project info) merge into the result.
#[allow(clippy::too_many_arguments)]
fn assemble_and_write(
    mut cards: Vec<(Layer, FeatureCard)>,
    resolved: &ResolvedFilter,
    display_name: &str,
    provenance: Vec<Breadcrumb>,
    mut warnings: Vec<String>,
    output: &Path,
    limit: usize,
    extra: Value,
) -> Result<Value> {
    // Tallies FIRST, over everything that matched — `total`, the overview
    // sentence, and the per-layer counts must describe the MATCH, not the
    // listing. A --limit answers "show me fewer", never "pretend fewer exist";
    // an agent relaying `total` must not under-report the repo.
    let mut layer_tally: BTreeMap<u8, (Layer, usize)> = BTreeMap::new();
    for (layer, _) in &cards {
        layer_tally.entry(layer.rank()).or_insert((*layer, 0)).1 += 1;
    }
    let layer_counts: Vec<LayerCount> = layer_tally
        .values()
        .map(|(l, c)| LayerCount {
            layer: l.as_str().to_string(),
            count: *c,
        })
        .collect();
    let total = cards.len();
    let all_files: Vec<String> = cards
        .iter()
        .flat_map(|(_, c)| c.key_files.clone())
        .collect();
    let language_counts = language_tally(&all_files);

    // Optional cap: keep the largest features when a positive limit is set.
    if limit > 0 && cards.len() > limit {
        cards.sort_by(|a, b| {
            b.1.member_count
                .cmp(&a.1.member_count)
                .then_with(|| a.1.id.cmp(&b.1.id))
        });
        let dropped = cards.len() - limit;
        cards.truncate(limit);
        warnings.push(format!(
            "listing the {limit} largest of {total} matching feature(s); {dropped} more matched (counts describe the full match; pass --limit 0 to list all)"
        ));
    }

    // Group, journey order (entry first), largest feature first within a group.
    let group_order = [
        Layer::Entry,
        Layer::Interface,
        Layer::Core,
        Layer::Foundation,
        Layer::Unknown,
    ];
    let mut groups: Vec<LayerGroup> = Vec::new();
    for layer in group_order {
        let mut feats: Vec<FeatureCard> = cards
            .iter()
            .filter(|(l, _)| *l == layer)
            .map(|(_, c)| c.clone())
            .collect();
        if feats.is_empty() {
            continue;
        }
        feats.sort_by(|a, b| {
            b.member_count
                .cmp(&a.member_count)
                .then_with(|| a.label.cmp(&b.label))
                .then_with(|| a.id.cmp(&b.id))
        });
        groups.push(LayerGroup {
            layer: layer.as_str().to_string(),
            label: layer_heading(layer),
            features: feats,
        });
    }

    let overview = compose_overview(resolved, display_name, total, &layer_counts);
    let (title, subtitle) = framing(resolved, display_name);

    let manifest = FeatureInventoryManifest {
        schema_version: "feature-inventory-1".to_string(),
        repo_name: display_name.to_string(),
        title,
        subtitle,
        filter: FilterInfo {
            kind: resolved.kind.as_str().to_string(),
            value: resolved.value.clone(),
            detected: resolved.detected,
        },
        overview,
        total,
        layer_counts,
        language_counts,
        groups,
        provenance,
        warnings: warnings.clone(),
    };

    // Always write the HTML page.
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    write_features_html(output, &manifest)?;

    // Compact JSON (the full detail lives in the HTML). The HTML inventory is
    // exhaustive; the inline list is bounded so a huge repo/project can't flood
    // the agent's context. Every entry has the SAME shape — agents pipe this
    // through jq/group_by, so no mixed-shape sentinel objects in the array.
    const MAX_COMPACT_FEATURES: usize = 80;
    // Layer/folder filters give every feature the IDENTICAL matched_by
    // breadcrumb; repeating it per row is pure payload waste. When all rows
    // would match for the same reason, lift it out as one top-level rule.
    let all_matched: Vec<&Vec<Breadcrumb>> = manifest
        .groups
        .iter()
        .flat_map(|g| g.features.iter())
        .map(|f| &f.matched_by)
        .collect();
    let shared_match_rule = match all_matched.split_first() {
        Some((first, rest))
            if !first.is_empty() && rest.iter().all(|m| m == first) && !rest.is_empty() =>
        {
            Some((*first).clone())
        }
        _ => None,
    };
    let mut compact_features: Vec<Value> = manifest
        .groups
        .iter()
        .flat_map(|g| g.features.iter())
        .map(|f| {
            // The label IS the feature's directory under the structural
            // partition, so `folders` would repeat it; three symbols identify
            // a feature — the full roster lives in the HTML inventory. Goal:
            // the whole response stays small enough to be read INLINE by an
            // agent (oversized tool results get offloaded to files, and then
            // agents mine them with jq instead of reading the answer).
            let mut symbol_names: Vec<String> = Vec::new();
            for sym in &f.top_symbols {
                if !symbol_names.contains(&sym.name) {
                    symbol_names.push(sym.name.clone());
                }
                if symbol_names.len() == 3 {
                    break;
                }
            }
            let mut row = json!({
                "label": f.label,
                "role": f.role,
                "member_count": f.member_count,
                "top_symbols": symbol_names,
            });
            if shared_match_rule.is_none() && !f.matched_by.is_empty() {
                row["matched_by"] = json!(f.matched_by);
            }
            if let Some(repo) = &f.repo {
                row["repo"] = json!(repo);
            }
            if !f.cross_links.is_empty() {
                row["cross_links"] = json!(f.cross_links);
            }
            row
        })
        .collect();
    let mut features_omitted = 0usize;
    if compact_features.len() > MAX_COMPACT_FEATURES {
        features_omitted = compact_features.len() - MAX_COMPACT_FEATURES;
        compact_features.truncate(MAX_COMPACT_FEATURES);
        warnings.push(format!(
            "inline list capped at {MAX_COMPACT_FEATURES} feature(s); {features_omitted} more are in the HTML inventory at output_html"
        ));
    }

    let mut result = json!({
        "status": "ok",
        "filter": manifest.filter,
        "overview": manifest.overview,
        "total": manifest.total,
        "features_omitted": features_omitted,
        "layer_counts": manifest.layer_counts,
        "language_counts": manifest.language_counts,
        "features": compact_features,
        "provenance": manifest.provenance,
        "output_html": output,
        "warnings": warnings,
    });
    if let Some(rule) = shared_match_rule {
        result["matched_by_rule"] = json!(rule);
    }
    if let (Value::Object(target), Value::Object(extra)) = (&mut result, extra) {
        for (k, v) in extra {
            target.insert(k, v);
        }
    }
    Ok(result)
}

/// Decide how the positional filter (or a forcing flag) should be read.
/// Precedence in auto mode: a path separator or an existing directory ⇒ folder;
/// a single known layer word ⇒ layer; everything else ⇒ topic. A bare layer word
/// wins over a same-named directory, so `client` means the entry layer — force a
/// folder with a trailing slash or `--folder`. `roots` are the repo roots the
/// directory check probes (one for a repo, all members for a project).
fn resolve_filter(
    filter: Option<&str>,
    opts: &FeatureInventoryOptions,
    roots: &[PathBuf],
) -> ResolvedFilter {
    let flag = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    if let Some(v) = flag(&opts.layer) {
        return ResolvedFilter {
            kind: FilterKind::Layer,
            value: Some(v),
            detected: false,
        };
    }
    if let Some(v) = flag(&opts.folder) {
        return ResolvedFilter {
            kind: FilterKind::Folder,
            value: Some(v),
            detected: false,
        };
    }
    if let Some(v) = flag(&opts.topic) {
        return ResolvedFilter {
            kind: FilterKind::Topic,
            value: Some(v),
            detected: false,
        };
    }
    let Some(f) = filter.map(str::trim).filter(|s| !s.is_empty()) else {
        return ResolvedFilter {
            kind: FilterKind::All,
            value: None,
            detected: false,
        };
    };
    let kind = if f.contains('/') || f.contains('\\') {
        FilterKind::Folder
    } else if layering::layer_from_query(f).is_some() {
        FilterKind::Layer
    } else if roots.iter().any(|root| root.join(f).is_dir()) {
        FilterKind::Folder
    } else {
        FilterKind::Topic
    };
    ResolvedFilter {
        kind,
        value: Some(f.to_string()),
        detected: true,
    }
}

/// Human heading for a layer section.
fn layer_heading(layer: Layer) -> String {
    match layer {
        Layer::Entry => "Entry — what users touch (UI, client, CLI, pages)",
        Layer::Interface => "Interface — the API surface (resolvers, controllers, routes)",
        Layer::Core => "Core — business logic & data access (services, domain, repositories)",
        Layer::Foundation => "Foundation — contracts, infra, config, low-level types",
        Layer::Unknown => "Unlayered — couldn't be placed from paths",
    }
    .to_string()
}

/// Common short words that would over-match path-derived labels; dropped from the
/// lexical topic match. `feature`/`features` are added because every L3 summary
/// is structurally prefixed "Feature:".
const STOPWORDS: &[&str] = &[
    "and", "the", "for", "with", "from", "into", "that", "this", "are", "was", "has", "you", "our",
    "its", "all", "any", "how", "via", "per", "out", "use", "feature", "features",
];

fn lexical_tokens(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = text
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|t| t.len() >= MIN_LEXICAL_TOKEN)
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .collect();
    tokens.sort();
    tokens.dedup();
    tokens
}

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

/// Distinct top-level folders the files live in (first two path segments, or the
/// first if that is all there is), most-frequent first. The "where does this
/// feature live" summary line.
fn top_folders(files: &[String]) -> Vec<String> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for f in files {
        let segs: Vec<&str> = f.split('/').filter(|s| !s.is_empty()).collect();
        let folder = match segs.len() {
            0 => continue,
            1 => continue, // a bare top-level file has no folder to report
            _ => segs[..2.min(segs.len() - 1)].join("/"),
        };
        if !folder.is_empty() {
            *counts.entry(folder).or_insert(0) += 1;
        }
    }
    let mut out: Vec<(String, usize)> = counts.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.into_iter().take(4).map(|(f, _)| f).collect()
}

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

/// Deterministic extractive overview (pure — same inputs ⇒ same text).
fn compose_overview(
    resolved: &ResolvedFilter,
    repo_name: &str,
    total: usize,
    layer_counts: &[LayerCount],
) -> String {
    if total == 0 {
        return match &resolved.value {
            Some(v) => format!(
                "No features matched the {} filter `{v}` in {repo_name}. Index the repository (chaos_analyze/chaos_add) so the feature hierarchy exists, or broaden the filter.",
                resolved.kind.as_str()
            ),
            None => format!(
                "No features were found in {repo_name}. Index the repository (chaos_analyze/chaos_add) so the L1 community hierarchy exists."
            ),
        };
    }
    let scope = match (&resolved.kind, &resolved.value) {
        (FilterKind::All, _) => format!("{repo_name} has {total} feature(s)"),
        (FilterKind::Layer, Some(v)) => {
            format!("{repo_name} has {total} feature(s) in the `{v}` layer")
        }
        (FilterKind::Folder, Some(v)) => {
            format!("{repo_name} has {total} feature(s) with code under `{v}`")
        }
        (FilterKind::Topic, Some(v)) => {
            format!("{repo_name} has {total} feature(s) matching `{v}`")
        }
        (_, None) => format!("{repo_name} has {total} feature(s)"),
    };
    let breakdown = if layer_counts.is_empty() {
        String::new()
    } else {
        let parts: Vec<String> = layer_counts
            .iter()
            .map(|l| format!("{} {}", l.count, l.layer))
            .collect();
        format!(" — {} by journey layer", join_human_strings(&parts))
    };
    format!("{scope}{breakdown}. Grouped entry → interface → core → foundation.")
}

fn framing(resolved: &ResolvedFilter, repo_name: &str) -> (String, String) {
    let title = match (&resolved.kind, &resolved.value) {
        (FilterKind::Layer, Some(v)) => format!("{repo_name} — `{v}` features"),
        (FilterKind::Folder, Some(v)) => format!("{repo_name} — features under {v}"),
        (FilterKind::Topic, Some(v)) => format!("{repo_name} — features matching “{v}”"),
        _ => format!("{repo_name} — all features"),
    };
    let subtitle =
        "Every god-node (L1 community / feature) that matches, grouped by where it sits in the user journey — entry (UI/CLI) → interface (API) → core (logic) → foundation (contracts/infra). Exhaustive, not a curated overview; use chaos_components for an ordered read-through of one area."
            .to_string();
    (title, subtitle)
}

fn join_human_strings(items: &[String]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].clone(),
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
        "features".to_string()
    } else {
        slug.chars().take(80).collect::<String>()
    }
}

fn write_features_html(path: &Path, manifest: &FeatureInventoryManifest) -> Result<()> {
    let json = serde_json::to_string(manifest)?;
    fs::write(
        path,
        FEATURES_HTML
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

const FEATURES_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Feature inventory</title>
<style>
__THEME__
/* ===== feature inventory (light editorial) ===== */
header.ov{background:var(--bg-sky-soft);border-bottom:var(--border-hairline)}
header.ov .wrap{padding:48px 32px 36px}
header.ov .eyebrow{font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.16em;color:var(--color-blue-700);margin-bottom:16px;display:flex;align-items:center;gap:10px}
header.ov .eyebrow::before{content:"";width:22px;height:1px;background:var(--color-blue-500);display:inline-block}
header.ov h1{font:var(--type-display-lg);letter-spacing:-.01em;color:var(--color-ink-700);margin:0 0 10px}
#overview{font:var(--type-body-lg);color:var(--color-ink-500);line-height:1.55;max-width:80ch}
.sub{color:var(--color-ink-400);max-width:76ch;margin-top:14px;font:var(--type-body-sm);line-height:1.6}
main{padding:40px 0 64px;display:grid;gap:24px}
.panel{background:var(--color-surface-0);border:var(--border-hairline);border-radius:var(--radius-lg);box-shadow:var(--shadow-sm);padding:24px}
h2{font:var(--type-h4);color:var(--color-ink-700);margin:0 0 16px}
.muted{color:var(--fg-tertiary);line-height:1.5}
.stats{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:16px}
.stat{border:var(--border-hairline);border-radius:var(--radius-md);background:var(--color-surface-2);padding:18px}
.stat b{display:block;font:var(--type-h2);font-family:var(--font-display);color:var(--color-ink-700);line-height:1}
.stat span{display:block;color:var(--fg-tertiary);font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.08em;margin-top:8px}
.grp{margin-top:8px}
.grp>h3{font:var(--type-h5);color:var(--color-ink-700);margin:18px 0 6px;display:flex;align-items:center;gap:10px}
.grp>h3 .pill{display:inline-flex;align-items:center;border-radius:var(--radius-pill);padding:2px 10px;font:var(--type-overline-sm);font-family:var(--font-mono);text-transform:uppercase;letter-spacing:.06em}
.role.entry,.pill.entry{color:var(--color-blue-700);background:var(--color-blue-100)}
.role.interface,.pill.interface{color:#9a6700;background:rgba(255,193,7,.16)}
.role.core,.pill.core{color:#007f76;background:rgba(0,200,187,.12)}
.role.foundation,.pill.foundation{color:var(--color-purple-500);background:var(--color-purple-100)}
.role.unknown,.pill.unknown{color:var(--fg-tertiary);background:var(--color-surface-1)}
.feat{border:var(--border-hairline);border-radius:var(--radius-lg);background:var(--color-surface-0);padding:18px 20px;margin-top:12px;position:relative;box-shadow:var(--shadow-xs)}
.feat::before{content:"";position:absolute;left:0;top:0;bottom:0;width:3px;border-radius:var(--radius-lg) 0 0 var(--radius-lg);background:var(--color-blue-400)}
.feat h4{margin:0 0 4px;font:var(--type-h6);color:var(--color-ink-700);display:flex;align-items:center;flex-wrap:wrap;gap:8px}
.role{display:inline-flex;align-items:center;border-radius:var(--radius-pill);padding:3px 10px;font:var(--type-overline-sm);font-family:var(--font-mono);text-transform:uppercase;letter-spacing:.06em}
.folders{margin-top:6px;display:flex;flex-wrap:wrap;gap:6px}
.folder{display:inline-flex;align-items:center;border-radius:var(--radius-pill);padding:3px 10px;font:var(--type-body-xs);font-family:var(--font-mono);background:var(--color-surface-1);color:var(--color-ink-500)}
.summary{color:var(--color-ink-500);font:var(--type-body-sm);line-height:1.6;margin:10px 0;white-space:pre-wrap;max-height:130px;overflow:auto}
.chips{margin-top:8px}
.chip{display:inline-flex;align-items:center;gap:6px;border:var(--border-hairline);border-radius:var(--radius-pill);padding:4px 11px;margin:6px 6px 0 0;color:var(--color-ink-500);font:500 12px/1 var(--font-body);background:var(--color-surface-1)}
.chip .k{color:var(--fg-tertiary);font-family:var(--font-mono);font-size:10px;text-transform:uppercase;letter-spacing:.04em}
.lang{display:inline-flex;align-items:center;gap:6px;border-radius:var(--radius-pill);padding:3px 10px;margin:6px 6px 0 0;font:var(--type-overline-sm);font-family:var(--font-mono);background:var(--color-blue-50);color:var(--color-blue-700)}
.matched{margin-top:10px;display:grid;gap:4px}
.matched div{color:var(--color-ink-500);font:var(--type-body-xs);line-height:1.5}
.matched b{color:var(--color-ink-700);font-weight:500;font-family:var(--font-mono);text-transform:uppercase;letter-spacing:.04em;font-size:10px}
.item.warn{border:1px solid var(--color-blue-300);border-radius:var(--radius-md);background:var(--color-blue-50);padding:14px 16px;margin-top:12px}
.item.warn strong{color:var(--color-blue-700);font:var(--type-h6);display:block;margin-bottom:4px}
.item.warn div{color:var(--color-ink-500);font:var(--type-body-sm);line-height:1.5}
</style>
</head>
<body data-chaos-features>
<div class="topbar"><div class="wrap">__BRAND_TOPBAR__<span class="crumb">Features<span class="sep">&rsaquo;</span><b>inventory</b></span><span class="sp"></span><span class="pilltag">Features</span></div></div>

<header class="ov">
  <div class="wrap">
    <div class="eyebrow" id="eyebrow">Feature inventory</div>
    <h1 id="title">Feature inventory</h1>
    <div id="overview"></div>
    <div class="sub" id="subtitle"></div>
  </div>
</header>

<main>
  <div class="wrap">
    <section class="panel"><div id="stats" class="stats"></div></section>
    <section class="panel" data-feat-groups><h2>Features, grouped by journey layer</h2><div class="muted" style="margin-bottom:10px">Entry (UI/CLI) &rarr; interface (API) &rarr; core (logic) &rarr; foundation (contracts/infra). Largest feature first within each layer.</div><div id="groups"></div></section>
    <section class="panel" data-feat-provenance><h2>How this was generated</h2><div class="muted" style="margin-bottom:10px">Provenance breadcrumbs &mdash; the steps that produced this inventory.</div><div id="provenance"></div></section>
    <section class="panel"><h2>Warnings</h2><div id="warnings"></div></section>
  </div>
</main>

<footer><div class="wrap">__BRAND_FOOTER__<span class="sp"></span><span class="meta">generated by Chaos Substrate</span></div></footer>

<script type="application/json" id="chaos-features-manifest">__DATA__</script>
<script>
(function(){
var D=JSON.parse(document.getElementById("chaos-features-manifest").textContent);
function esc(v){return String(v==null?"":v).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;").replace(/'/g,"&#039;");}
document.getElementById("title").textContent=D.title||"Feature inventory";
var fk=D.filter||{};
document.getElementById("eyebrow").textContent=fk.value?(fk.kind+" · "+fk.value+(fk.detected?" (auto)":"")):"Whole repository";
document.getElementById("overview").textContent=D.overview||"";
document.getElementById("subtitle").textContent=D.subtitle||"";
var groups=D.groups||[];
var langs=(D.language_counts||[]).map(function(l){return l.language+" "+l.count;}).join(" · ");
var stat=[[D.total||0,"features"],[(D.layer_counts||[]).length,"layers"],[groups.length,"groups"],[(D.warnings||[]).length,"warnings"]];
document.getElementById("stats").innerHTML=stat.map(function(s){return '<div class="stat"><b>'+s[0]+'</b><span>'+s[1]+'</span></div>';}).join("")+(langs?'<div class="stat"><b style="font-size:14px;line-height:1.4">'+esc(langs)+'</b><span>languages</span></div>':'');
var host=document.getElementById("groups");
groups.forEach(function(g){
  var sec=document.createElement("div");sec.className="grp";
  var role=g.layer||"unknown";
  var head='<h3><span class="pill '+esc(role)+'">'+esc(role)+'</span>'+esc(g.label)+' <span class="muted" style="font-weight:400">&middot; '+(g.features||[]).length+'</span></h3>';
  var body=(g.features||[]).map(function(f){
    var syms=(f.top_symbols||[]).map(function(s){return '<span class="chip">'+esc(s.name)+' <span class="k">'+esc(s.kind)+'</span></span>';}).join("");
    var fl=(f.languages||[]).map(function(l){return '<span class="lang">'+esc(l.language)+' &middot; '+l.count+'</span>';}).join("");
    var folders=(f.folders||[]).map(function(x){return '<span class="folder">'+esc(x)+'</span>';}).join("");
    var matched=(f.matched_by||[]).map(function(m){return '<div><b>'+esc(m.source)+'</b> '+esc(m.detail)+'</div>';}).join("");
    var xlinks=(f.cross_links||[]).map(function(x){return '<div><b>link</b> '+esc(x)+'</div>';}).join("");
    var repoChip=f.repo?' <span class="folder">'+esc(f.repo)+'</span>':'';
    return '<div class="feat"><h4>'+esc(f.label)+repoChip+' <span class="role '+esc(f.role||"unknown")+'">'+esc(f.role||"unknown")+'</span></h4>'+
      '<div class="muted">'+f.member_count+' symbols</div>'+
      (folders?'<div class="folders">'+folders+'</div>':'')+
      (fl?'<div class="chips">'+fl+'</div>':'')+
      (f.summary?'<div class="summary">'+esc(f.summary)+'</div>':'')+
      (syms?'<div class="chips">'+syms+'</div>':'')+
      (xlinks?'<div class="matched">'+xlinks+'</div>':'')+
      (matched?'<div class="matched">'+matched+'</div>':'')+
      '</div>';
  }).join("");
  sec.innerHTML=head+body;
  host.appendChild(sec);
});
if(!host.children.length)host.innerHTML='<div class="muted">No features matched. Ensure the repo is indexed (chaos_analyze) so communities exist, then try a broader filter or omit it.</div>';
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

    fn opts() -> FeatureInventoryOptions {
        FeatureInventoryOptions::default()
    }

    #[test]
    fn resolve_prefers_layer_word_over_same_named_dir() {
        // "client" is a layer word → layer, even though many repos have a client/ dir.
        let r = resolve_filter(Some("client"), &opts(), &[PathBuf::from("/nonexistent")]);
        assert_eq!(r.kind, FilterKind::Layer);
        assert!(r.detected);
    }

    #[test]
    fn resolve_path_is_folder() {
        let r = resolve_filter(
            Some("packages/ui"),
            &opts(),
            &[PathBuf::from("/nonexistent")],
        );
        assert_eq!(r.kind, FilterKind::Folder);
    }

    #[test]
    fn resolve_unknown_word_is_topic() {
        let r = resolve_filter(Some("payments"), &opts(), &[PathBuf::from("/nonexistent")]);
        assert_eq!(r.kind, FilterKind::Topic);
    }

    #[test]
    fn resolve_empty_is_all() {
        let r = resolve_filter(None, &opts(), &[PathBuf::from("/nonexistent")]);
        assert_eq!(r.kind, FilterKind::All);
        assert!(!r.detected);
    }

    #[test]
    fn resolve_flag_forces_kind_and_is_not_detected() {
        let o = FeatureInventoryOptions {
            folder: Some("client".into()),
            ..Default::default()
        };
        let r = resolve_filter(Some("ignored"), &o, &[PathBuf::from("/nonexistent")]);
        assert_eq!(r.kind, FilterKind::Folder);
        assert_eq!(r.value.as_deref(), Some("client"));
        assert!(!r.detected);
    }

    #[test]
    fn top_folders_groups_by_two_segments() {
        let files = vec![
            "packages/client/src/a.tsx".to_string(),
            "packages/client/src/b.tsx".to_string(),
            "packages/api/src/c.ts".to_string(),
        ];
        let f = top_folders(&files);
        assert_eq!(f[0], "packages/client");
        assert!(f.contains(&"packages/api".to_string()));
    }

    #[test]
    fn overview_is_deterministic_and_grounded() {
        let resolved = ResolvedFilter {
            kind: FilterKind::Layer,
            value: Some("client".into()),
            detected: true,
        };
        let lc = vec![LayerCount {
            layer: "entry".into(),
            count: 3,
        }];
        let a = compose_overview(&resolved, "molecule_core", 3, &lc);
        let b = compose_overview(&resolved, "molecule_core", 3, &lc);
        assert_eq!(a, b);
        assert!(a.contains("client"));
        assert!(a.contains("3 feature"));
    }

    #[test]
    fn overview_handles_empty() {
        let resolved = ResolvedFilter {
            kind: FilterKind::Topic,
            value: Some("xyz".into()),
            detected: true,
        };
        let text = compose_overview(&resolved, "repo", 0, &[]);
        assert!(text.contains("No features matched"));
    }
}
