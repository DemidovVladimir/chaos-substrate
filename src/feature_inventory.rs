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
//!   * anything else is first tried as a layer **by meaning** (embedding cosine
//!     against per-layer prototype phrasings, so "backend", "client app" or
//!     "devops" resolve to layers without a keyword list — "backend" spans
//!     interface+core), and only then falls to a **topic** match
//!     (summary-embedding cosine + label/summary keywords), exhaustive;
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
use serde::{Deserialize, Serialize};
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
/// Auto-domain split threshold: a path-prefix group bigger than this splits
/// into its child folders, so domain sections stay readable.
const MAX_DOMAIN_GROUP: usize = 12;

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
    /// Agent-supplied curation: human domain groups + one-line notes rendered
    /// onto the SAME inventory page (re-run the identical query with this set).
    pub curation: Option<CurationSpec>,
}

/// Agent-written curation for the human-facing inventory page. Chaos has no
/// LLM, so readable domain titles and "what's in it" notes can only come from
/// the agent — it passes the structure it already composed for its answer and
/// Rust renders it deterministically (the `chaos_write_feature_website`
/// philosophy applied to the feature inventory).
#[derive(Debug, Clone, Deserialize)]
pub struct CurationSpec {
    pub groups: Vec<CurationGroup>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CurationGroup {
    /// Human heading, e.g. "IP-NFT Minting flow — the wizard".
    pub title: String,
    /// Optional emoji/icon shown before the title.
    #[serde(default)]
    pub icon: Option<String>,
    /// Optional one-paragraph description of the domain.
    #[serde(default)]
    pub blurb: Option<String>,
    #[serde(default)]
    pub features: Vec<CurationFeature>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CurationFeature {
    /// Feature label as returned by the inventory (full label or a unique
    /// trailing fragment of it).
    pub label: String,
    /// One-line human "what's in it" note.
    #[serde(default)]
    pub note: Option<String>,
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
    /// Layer set resolved SEMANTICALLY (prototype-embedding match) — `None` for
    /// the exact-word path, which `select_features` resolves itself. A phrase
    /// like "backend" legitimately spans two layers (interface + core).
    layers: Option<Vec<Layer>>,
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
    /// Human-first grouping: features clustered into folder-derived domains
    /// (auto) or agent-curated domains with notes. The page renders these as
    /// its primary sections; `groups` keeps the journey-layer view for agents.
    pub domains: Vec<DomainGroup>,
    pub provenance: Vec<Breadcrumb>,
    pub warnings: Vec<String>,
}

/// One domain section of the inventory page.
#[derive(Debug, Clone, Serialize)]
pub struct DomainGroup {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blurb: Option<String>,
    /// True when an agent supplied this group (vs path-derived).
    pub curated: bool,
    pub features: Vec<DomainFeatureRef>,
}

/// Reference into the feature cards (which live in `groups`), plus the
/// curated one-line note when present.
#[derive(Debug, Clone, Serialize)]
pub struct DomainFeatureRef {
    pub id: Uuid,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FilterInfo {
    pub kind: String,
    pub value: Option<String>,
    pub detected: bool,
    /// Present when the value was resolved to layer(s) by embedding similarity
    /// rather than an exact word — e.g. "backend" → ["interface", "core"].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layers: Option<Vec<String>>,
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

    // 2. Resolve how to read the filter — exact rules first, then by meaning.
    let mut resolved = resolve_filter(filter, opts, std::slice::from_ref(&repo_root));
    push_filter_breadcrumb(&resolved, &mut provenance);

    // 3–5. Select, build cards, assemble (shared with the project listing).
    let topic_embedding = topic_embedding_for(embedder, &resolved).await?;
    maybe_route_layers_by_meaning(
        embedder,
        &mut resolved,
        topic_embedding.as_deref(),
        &mut provenance,
        &mut warnings,
    )
    .await;
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
        opts.curation.as_ref(),
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
    let mut resolved = resolve_filter(filter, opts, &roots);
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
    maybe_route_layers_by_meaning(
        embedder,
        &mut resolved,
        topic_embedding.as_deref(),
        &mut provenance,
        &mut warnings,
    )
    .await;
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
        opts.curation.as_ref(),
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

/// Prototype phrasings per journey layer, the anchors for SEMANTIC layer
/// routing: a filter that isn't a folder or an exact layer word is embedded and
/// max-pooled against these, so "backend", "client app", "devops" or "API
/// endpoints" select layer(s) by MEANING — there is no query keyword list to
/// maintain, and an unseen phrasing ("server side stuff") still lands.
const LAYER_PROTOTYPES: &[(Layer, &str)] = &[
    (Layer::Entry, "user-facing client application"),
    (
        Layer::Entry,
        "frontend web app: UI components, screens, pages",
    ),
    (Layer::Entry, "mobile app user interface"),
    (Layer::Entry, "command line interface a user runs"),
    (Layer::Interface, "HTTP API endpoints and routes"),
    (Layer::Interface, "GraphQL resolvers and REST controllers"),
    (
        Layer::Interface,
        "the API surface that client applications call",
    ),
    (Layer::Interface, "backend API request handlers"),
    (Layer::Core, "backend business logic and services"),
    (Layer::Core, "server-side domain model and use cases"),
    (Layer::Core, "data access layer and repositories"),
    (Layer::Core, "backend services behind the API"),
    (Layer::Foundation, "smart contracts on the blockchain"),
    (
        Layer::Foundation,
        "infrastructure as code, deployment and devops",
    ),
    (
        Layer::Foundation,
        "configuration, environment and project setup",
    ),
    (Layer::Foundation, "low-level shared types and utilities"),
];

/// Floor the best layer's max-pooled cosine must clear for the filter to be
/// read as a layer request, and the margin under the best within which further
/// layers join the set ("backend" sits close to both interface and core, so it
/// selects both). Calibrated against the default local embedder
/// (EmbeddingGemma): layer phrasings score 0.68–0.91 while genuine topics
/// ("access control", "payments", "data pipeline") stay ≤ 0.62. A model with a
/// flatter cosine distribution routes fewer filters — they simply remain topic
/// matches, the pre-routing behavior.
const LAYER_ROUTE_FLOOR: f64 = 0.65;
const LAYER_ROUTE_MARGIN: f64 = 0.06;

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

/// Prototype embeddings per embedder identity, computed once per process — the
/// MCP server is long-lived and must not re-embed 16 constant strings on every
/// features call.
type PrototypeVectors = std::sync::Arc<Vec<Vec<f32>>>;
static PROTOTYPE_CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<String, PrototypeVectors>>> =
    std::sync::OnceLock::new();

async fn prototype_embeddings(emb: &dyn Embedder) -> Result<PrototypeVectors> {
    let key = format!("{}/{}/{}", emb.provider(), emb.model_id(), emb.dimensions());
    let cache = PROTOTYPE_CACHE.get_or_init(Default::default);
    if let Some(hit) = cache.lock().unwrap().get(&key) {
        return Ok(hit.clone());
    }
    let texts: Vec<String> = LAYER_PROTOTYPES
        .iter()
        .map(|(_, t)| t.to_string())
        .collect();
    let vecs = std::sync::Arc::new(emb.embed_batch(&texts).await?);
    cache.lock().unwrap().insert(key, vecs.clone());
    Ok(vecs)
}

/// Try to read the filter as a request for journey layer(s) BY MEANING, the
/// step between exact-word layer detection and the topic fallback. Two cases
/// route: an auto-detected topic (the phrase may *mean* a layer — "client
/// app"), and a layer filter whose value isn't an exact layer word
/// (`--layer backend`). On a hit the resolved filter is switched to a layer
/// set; on a miss (or with no embedder, or an embedder error) nothing changes —
/// the filter stays a topic match. Calibration in `LAYER_ROUTE_FLOOR`.
async fn maybe_route_layers_by_meaning(
    embedder: Option<&dyn Embedder>,
    resolved: &mut ResolvedFilter,
    topic_embedding: Option<&[f32]>,
    provenance: &mut Vec<Breadcrumb>,
    warnings: &mut Vec<String>,
) {
    let Some(emb) = embedder else { return };
    let Some(value) = resolved.value.clone() else {
        return;
    };
    let candidate = match resolved.kind {
        FilterKind::Topic => resolved.detected,
        FilterKind::Layer => layering::layer_from_query(&value).is_none(),
        _ => false,
    };
    if !candidate {
        return;
    }
    let routed: Result<Option<(Vec<Layer>, f64, &str)>> = async {
        let query = match topic_embedding {
            Some(v) => v.to_vec(),
            None => emb.embed(&value).await?,
        };
        let protos = prototype_embeddings(emb).await?;
        // Max-pool the cosine per layer: a layer is as close as its closest phrasing.
        let mut best: Vec<(Layer, f64, &str)> = Vec::new();
        for ((layer, text), vec) in LAYER_PROTOTYPES.iter().zip(protos.iter()) {
            let score = cosine(&query, vec);
            match best.iter_mut().find(|(l, _, _)| l == layer) {
                Some(slot) if score > slot.1 => *slot = (*layer, score, text),
                Some(_) => {}
                None => best.push((*layer, score, text)),
            }
        }
        best.sort_by(|a, b| b.1.total_cmp(&a.1));
        let (_, top, anchor) = best[0];
        if top < LAYER_ROUTE_FLOOR {
            return Ok(None);
        }
        let mut layers: Vec<Layer> = best
            .iter()
            .filter(|(_, s, _)| *s >= top - LAYER_ROUTE_MARGIN)
            .map(|(l, _, _)| *l)
            .collect();
        layers.sort_by_key(|l| l.rank());
        Ok(Some((layers, top, anchor)))
    }
    .await;
    match routed {
        Ok(Some((layers, top, anchor))) => {
            let label = layers
                .iter()
                .map(|l| l.as_str())
                .collect::<Vec<_>>()
                .join("+");
            provenance.push(Breadcrumb::new(
                source::EMBEDDING,
                "layer_prototype_match",
                format!(
                    "read `{value}` as the `{label}` layer(s) by meaning — cosine {top:.2} to the prototype \"{anchor}\" (floor {LAYER_ROUTE_FLOOR}, layers within {LAYER_ROUTE_MARGIN} of the best)"
                ),
            ));
            resolved.kind = FilterKind::Layer;
            resolved.layers = Some(layers);
        }
        Ok(None) => {}
        Err(e) => warnings.push(format!(
            "semantic layer routing skipped (embedder error: {e:#}) — `{value}` was matched as a topic"
        )),
    }
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
            // Semantically routed layer set if present, else the exact-word path.
            let want: Vec<Layer> = match &resolved.layers {
                Some(layers) => layers.clone(),
                None => match layering::layer_from_query(&value) {
                    Some(l) => vec![l],
                    None => {
                        warnings.push(format!(
                            "unrecognized layer `{value}` — recognized layers are entry (client/ui/web/cli), interface (api/resolver/route), core (service/logic/domain), foundation (contract/infra/config); with an embedder configured, layer phrasings like `backend` also resolve by meaning; showing features that couldn't be layered"
                        ));
                        vec![Layer::Unknown]
                    }
                },
            };
            let want_label = want
                .iter()
                .map(|l| l.as_str())
                .collect::<Vec<_>>()
                .join("+");
            for c in &hierarchy.communities {
                let layer = layering::classify_community(&c.top_members);
                if want.contains(&layer) {
                    selected.push((
                        c.id,
                        vec![Breadcrumb::new(
                            source::GRAPH,
                            "classify_community",
                            format!(
                                "classified as the `{}` layer from its members' paths",
                                layer.as_str()
                            ),
                        )],
                    ));
                }
            }
            provenance.push(Breadcrumb::new(
                source::GRAPH,
                "classify_community",
                format!(
                    "kept the {} feature(s) in the `{want_label}` journey layer(s)",
                    selected.len(),
                ),
            ));
            if selected.is_empty() && !hierarchy.communities.is_empty() {
                warnings.push(format!(
                    "no features sit in the `{want_label}` layer(s) — try another layer or a folder/topic filter"
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
        // Distinct by name — a community's top members often repeat one import
        // path across many nodes, which reads as noise on a feature card.
        let mut seen_symbols: HashSet<&str> = HashSet::new();
        let top_symbols: Vec<FeatureSymbol> = detail
            .top_members
            .iter()
            .filter(|(name, _, _)| seen_symbols.insert(name.as_str()))
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
    curation: Option<&CurationSpec>,
    extra: Value,
) -> Result<Value> {
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
            "showing the {limit} largest feature(s); {dropped} more matched but were capped by --limit (pass --limit 0 for all)"
        ));
    }

    // Layer tallies across everything selected.
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

    // Language tally across all selected features.
    let all_files: Vec<String> = cards
        .iter()
        .flat_map(|(_, c)| c.key_files.clone())
        .collect();
    let language_counts = language_tally(&all_files);

    let total = cards.len();
    let overview = compose_overview(resolved, display_name, total, &layer_counts);
    let (title, subtitle) = framing(resolved, display_name);

    // Human-first domain grouping: agent curation when supplied, else
    // deterministic folder-derived domains.
    let domains = match curation {
        Some(spec) => {
            let (domains, unmatched) = curated_domains(&cards, spec);
            if !unmatched.is_empty() {
                warnings.push(format!(
                    "curation: no feature matched label(s): {}",
                    unmatched.join(", ")
                ));
            }
            domains
        }
        None => auto_domains(&cards),
    };

    let manifest = FeatureInventoryManifest {
        schema_version: "feature-inventory-2".to_string(),
        repo_name: display_name.to_string(),
        title,
        subtitle,
        filter: FilterInfo {
            kind: resolved.kind.as_str().to_string(),
            value: resolved.value.clone(),
            detected: resolved.detected,
            layers: resolved
                .layers
                .as_ref()
                .map(|ls| ls.iter().map(|l| l.as_str().to_string()).collect()),
        },
        overview,
        total,
        layer_counts,
        language_counts,
        groups,
        domains,
        provenance,
        warnings: warnings.clone(),
    };

    // Always write the HTML page.
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    write_features_html(output, &manifest)?;

    // A curated call is a re-render of an inventory the agent already has —
    // return only the outcome, not the feature lines again.
    if curation.is_some() {
        let mut result = json!({
            "status": "ok",
            "total": manifest.total,
            "curated_groups": manifest.domains.iter().filter(|d| d.curated).count(),
            "auto_groups": manifest.domains.iter().filter(|d| !d.curated).count(),
            "output_html": output,
            "warnings": warnings,
        });
        if let (Value::Object(target), Value::Object(extra)) = (&mut result, extra) {
            for (k, v) in extra {
                target.insert(k, v);
            }
        }
        return Ok(result);
    }

    // Compact JSON (the full detail lives in the HTML). The HTML inventory is
    // exhaustive; the inline list is bounded so a huge repo/project can't flood
    // the agent's context.
    const MAX_COMPACT_FEATURES: usize = 80;
    // Per-row match evidence is only informative for topic matches (per-feature
    // cosine / keyword detail). Layer and folder selection stamps one identical
    // breadcrumb on every row — the top-level `filter` + provenance already
    // state it once, so repeating it 80× would only burn agent context.
    // Full breadcrumbs always remain in the HTML manifest.
    let per_row_match = matches!(resolved.kind, FilterKind::Topic);
    // One readable line per feature instead of a keyed object: no repeated JSON
    // keys, one line under pretty-printing, and the spilled-to-disk form (when
    // a harness persists a large tool result) stays grep-able plain text.
    let mut compact_features: Vec<Value> = manifest
        .groups
        .iter()
        .flat_map(|g| g.features.iter())
        .map(|f| {
            let mut line = String::new();
            if let Some(repo) = &f.repo {
                line.push_str(&format!("[{repo}] "));
            }
            line.push_str(&format!(
                "{} — {}, {} members",
                f.label, f.role, f.member_count
            ));
            // Folders the label doesn't already spell out.
            let extra_folders: Vec<&String> = f
                .folders
                .iter()
                .filter(|d| !f.label.starts_with(d.as_str()))
                .collect();
            if !extra_folders.is_empty() {
                line.push_str(&format!(
                    " · folders: {}",
                    extra_folders
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            // Path-like symbol names (JS/TS import specifiers) repeat the
            // directory context the label already gives — keep the last two
            // segments inline; the HTML manifest has the full names.
            let mut symbol_names: Vec<String> = Vec::new();
            for s in &f.top_symbols {
                let short = short_symbol_name(&s.name);
                if !symbol_names.contains(&short) {
                    symbol_names.push(short);
                }
                if symbol_names.len() == 6 {
                    break;
                }
            }
            if !symbol_names.is_empty() {
                line.push_str(&format!(" · symbols: {}", symbol_names.join(", ")));
            }
            if !f.cross_links.is_empty() {
                line.push_str(&format!(" · links: {}", f.cross_links.join("; ")));
            }
            if per_row_match && !f.matched_by.is_empty() {
                // Short tags; full breadcrumbs live in the HTML manifest.
                let why: Vec<String> = f
                    .matched_by
                    .iter()
                    .map(|m| match m.method.as_str() {
                        "community_semantic_search" => m
                            .detail
                            .split(" vs ")
                            .next()
                            .unwrap_or(&m.detail)
                            .to_string(),
                        "label_summary_match" => "keyword".to_string(),
                        _ => m.detail.clone(),
                    })
                    .collect();
                line.push_str(&format!(" · matched: {}", why.join(" + ")));
            }
            json!(line)
        })
        .collect();
    if compact_features.len() > MAX_COMPACT_FEATURES {
        let omitted = compact_features.len() - MAX_COMPACT_FEATURES;
        compact_features.truncate(MAX_COMPACT_FEATURES);
        compact_features.push(json!(format!(
            "… {omitted} more feature(s) omitted from this inline list — the HTML inventory at output_html has every one"
        )));
    }

    let mut result = json!({
        "status": "ok",
        "filter": manifest.filter,
        "overview": manifest.overview,
        "total": manifest.total,
        "layer_counts": manifest.layer_counts,
        "language_counts": manifest.language_counts,
        "domains": manifest.domains.iter().map(|d| format!("{} ({})", d.title, d.features.len())).collect::<Vec<_>>(),
        "features": compact_features,
        "provenance": manifest.provenance,
        "output_html": output,
        "warnings": warnings,
    });
    if let (Value::Object(target), Value::Object(extra)) = (&mut result, extra) {
        for (k, v) in extra {
            target.insert(k, v);
        }
    }
    Ok(result)
}

/// Path-derived domain grouping: strip the label segments every feature
/// shares, then split top-down — a prefix group bigger than
/// `MAX_DOMAIN_GROUP` splits into its child folders, single-feature children
/// stay at the parent level. Deterministic; no embedder, no LLM.
fn auto_domains(cards: &[(Layer, FeatureCard)]) -> Vec<DomainGroup> {
    if cards.is_empty() {
        return Vec::new();
    }
    let paths: Vec<Vec<String>> = cards
        .iter()
        .map(|(_, c)| {
            let mut segs: Vec<String> = Vec::new();
            if let Some(r) = &c.repo {
                segs.push(format!("[{r}]"));
            }
            segs.extend(
                c.label
                    .split('/')
                    .filter(|s| !s.is_empty())
                    .map(String::from),
            );
            segs
        })
        .collect();
    // Longest common prefix across all paths; a feature whose label IS the
    // prefix lands in the root group.
    let mut common = 0usize;
    'outer: loop {
        let Some(first) = paths[0].get(common) else {
            break;
        };
        for p in &paths {
            if p.get(common).map(String::as_str) != Some(first.as_str()) {
                break 'outer;
            }
        }
        common += 1;
    }
    let mut out: Vec<DomainGroup> = Vec::new();
    split_domain(
        (0..cards.len()).collect(),
        common,
        Vec::new(),
        &paths,
        cards,
        &mut out,
    );
    if out.len() == 1 && out[0].title == "other" {
        out[0].title = "all features".to_string();
    }
    // Largest domain (by member symbols) first.
    let weight = |d: &DomainGroup| -> i64 {
        d.features
            .iter()
            .filter_map(|r| cards.iter().find(|(_, c)| c.id == r.id))
            .map(|(_, c)| c.member_count as i64)
            .sum()
    };
    out.sort_by(|a, b| {
        weight(b)
            .cmp(&weight(a))
            .then_with(|| a.title.cmp(&b.title))
    });
    out
}

/// Recursive splitter for [`auto_domains`].
fn split_domain(
    indices: Vec<usize>,
    depth: usize,
    prefix: Vec<String>,
    paths: &[Vec<String>],
    cards: &[(Layer, FeatureCard)],
    out: &mut Vec<DomainGroup>,
) {
    let emit = |idxs: Vec<usize>, out: &mut Vec<DomainGroup>| {
        if idxs.is_empty() {
            return;
        }
        let mut feats: Vec<&FeatureCard> = idxs.iter().map(|&i| &cards[i].1).collect();
        feats.sort_by(|a, b| {
            b.member_count
                .cmp(&a.member_count)
                .then_with(|| a.label.cmp(&b.label))
        });
        out.push(DomainGroup {
            title: if prefix.is_empty() {
                "other".to_string()
            } else {
                prefix.join("/")
            },
            icon: None,
            blurb: None,
            curated: false,
            features: feats
                .into_iter()
                .map(|c| DomainFeatureRef {
                    id: c.id,
                    label: c.label.clone(),
                    note: None,
                })
                .collect(),
        });
    };
    if indices.len() <= MAX_DOMAIN_GROUP {
        emit(indices, out);
        return;
    }
    let mut here: Vec<usize> = Vec::new();
    let mut by_seg: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for i in indices {
        match paths[i].get(depth) {
            Some(seg) => by_seg.entry(seg.clone()).or_default().push(i),
            None => here.push(i),
        }
    }
    for (seg, idxs) in by_seg {
        if idxs.len() == 1 {
            here.extend(idxs);
        } else {
            let mut p = prefix.clone();
            p.push(seg);
            split_domain(idxs, depth + 1, p, paths, cards, out);
        }
    }
    emit(here, out);
}

/// Overlay agent-supplied curation onto the cards: curated groups in the
/// agent's order (matched by exact label, then unique trailing fragment),
/// every unplaced feature falling back to auto domains. Returns the domains
/// plus any curation labels that matched nothing.
fn curated_domains(
    cards: &[(Layer, FeatureCard)],
    spec: &CurationSpec,
) -> (Vec<DomainGroup>, Vec<String>) {
    let mut used: HashSet<Uuid> = HashSet::new();
    let mut unmatched: Vec<String> = Vec::new();
    let mut domains: Vec<DomainGroup> = Vec::new();
    for g in &spec.groups {
        let mut feats: Vec<DomainFeatureRef> = Vec::new();
        for cf in &g.features {
            let hit = cards
                .iter()
                .find(|(_, c)| !used.contains(&c.id) && c.label == cf.label)
                .or_else(|| {
                    cards
                        .iter()
                        .find(|(_, c)| !used.contains(&c.id) && c.label.ends_with(&cf.label))
                });
            match hit {
                Some((_, c)) => {
                    used.insert(c.id);
                    feats.push(DomainFeatureRef {
                        id: c.id,
                        label: c.label.clone(),
                        note: cf.note.clone(),
                    });
                }
                None => unmatched.push(cf.label.clone()),
            }
        }
        if !feats.is_empty() {
            domains.push(DomainGroup {
                title: g.title.clone(),
                icon: g.icon.clone(),
                blurb: g.blurb.clone(),
                curated: true,
                features: feats,
            });
        }
    }
    let leftovers: Vec<(Layer, FeatureCard)> = cards
        .iter()
        .filter(|(_, c)| !used.contains(&c.id))
        .cloned()
        .collect();
    domains.extend(auto_domains(&leftovers));
    (domains, unmatched)
}

/// Compact-row display form of a symbol name: path-like names (import
/// specifiers) keep only their last two `/` segments, marked with `…/`.
fn short_symbol_name(name: &str) -> String {
    let segments: Vec<&str> = name.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() <= 2 {
        return name.to_string();
    }
    format!("…/{}", segments[segments.len() - 2..].join("/"))
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
            layers: None,
        };
    }
    if let Some(v) = flag(&opts.folder) {
        return ResolvedFilter {
            kind: FilterKind::Folder,
            value: Some(v),
            detected: false,
            layers: None,
        };
    }
    if let Some(v) = flag(&opts.topic) {
        return ResolvedFilter {
            kind: FilterKind::Topic,
            value: Some(v),
            detected: false,
            layers: None,
        };
    }
    let Some(f) = filter.map(str::trim).filter(|s| !s.is_empty()) else {
        return ResolvedFilter {
            kind: FilterKind::All,
            value: None,
            detected: false,
            layers: None,
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
        layers: None,
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
.note{margin:6px 0 2px;color:var(--color-ink-700);font:var(--type-body-sm);line-height:1.6}
.blurb{color:var(--color-ink-500);font:var(--type-body-sm);line-height:1.6;margin:2px 0 6px;max-width:80ch}
.grp>h3 .ico{font-size:18px;line-height:1}
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
    <section class="panel" data-feat-groups><h2 id="groups-title">Features by domain</h2><div class="muted" style="margin-bottom:10px" id="groups-sub"></div><div id="groups"></div></section>
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
var stat=[[D.total||0,"features"],[(D.layer_counts||[]).length,"layers"],[((D.domains&&D.domains.length)||groups.length),"domains"],[(D.warnings||[]).length,"warnings"]];
document.getElementById("stats").innerHTML=stat.map(function(s){return '<div class="stat"><b>'+s[0]+'</b><span>'+s[1]+'</span></div>';}).join("")+(langs?'<div class="stat"><b style="font-size:14px;line-height:1.4">'+esc(langs)+'</b><span>languages</span></div>':'');
var host=document.getElementById("groups");
function featHtml(f,note){
  var syms=(f.top_symbols||[]).map(function(s){return '<span class="chip">'+esc(s.name)+' <span class="k">'+esc(s.kind)+'</span></span>';}).join("");
  var fl=(f.languages||[]).map(function(l){return '<span class="lang">'+esc(l.language)+' &middot; '+l.count+'</span>';}).join("");
  var folders=(f.folders||[]).map(function(x){return '<span class="folder">'+esc(x)+'</span>';}).join("");
  var matched=(f.matched_by||[]).map(function(m){return '<div><b>'+esc(m.source)+'</b> '+esc(m.detail)+'</div>';}).join("");
  var xlinks=(f.cross_links||[]).map(function(x){return '<div><b>link</b> '+esc(x)+'</div>';}).join("");
  var repoChip=f.repo?' <span class="folder">'+esc(f.repo)+'</span>':'';
  return '<div class="feat"><h4>'+esc(f.label)+repoChip+' <span class="role '+esc(f.role||"unknown")+'">'+esc(f.role||"unknown")+'</span></h4>'+
    (note?'<div class="note">'+esc(note)+'</div>':'')+
    '<div class="muted">'+f.member_count+' symbols</div>'+
    (folders?'<div class="folders">'+folders+'</div>':'')+
    (fl?'<div class="chips">'+fl+'</div>':'')+
    (f.summary?'<div class="summary">'+esc(f.summary)+'</div>':'')+
    (syms?'<div class="chips">'+syms+'</div>':'')+
    (xlinks?'<div class="matched">'+xlinks+'</div>':'')+
    (matched?'<div class="matched">'+matched+'</div>':'')+
    '</div>';
}
var domains=D.domains||[];
if(domains.length){
  var anyCurated=domains.some(function(d){return d.curated;});
  document.getElementById("groups-sub").textContent=anyCurated
    ?"Curated domains first; remaining features grouped by folder (tagged auto). Each card's pill shows its journey layer."
    :"Features clustered by folder domain, largest first. Each card's pill shows its journey layer (entry/interface/core/foundation).";
  var byId={};groups.forEach(function(g){(g.features||[]).forEach(function(f){byId[f.id]=f;});});
  domains.forEach(function(g){
    var sec=document.createElement("div");sec.className="grp";
    var head='<h3>'+(g.icon?'<span class="ico">'+esc(g.icon)+'</span>':'')+esc(g.title)+' <span class="muted" style="font-weight:400">&middot; '+(g.features||[]).length+'</span>'+((anyCurated&&!g.curated)?' <span class="pill unknown">auto</span>':'')+'</h3>';
    var blurb=g.blurb?'<div class="blurb">'+esc(g.blurb)+'</div>':'';
    var body=(g.features||[]).map(function(r){var f=byId[r.id];return f?featHtml(f,r.note):'';}).join("");
    sec.innerHTML=head+blurb+body;
    host.appendChild(sec);
  });
}else{
  document.getElementById("groups-title").textContent="Features, grouped by journey layer";
  document.getElementById("groups-sub").innerHTML="Entry (UI/CLI) &rarr; interface (API) &rarr; core (logic) &rarr; foundation (contracts/infra). Largest feature first within each layer.";
  groups.forEach(function(g){
    var sec=document.createElement("div");sec.className="grp";
    var role=g.layer||"unknown";
    var head='<h3><span class="pill '+esc(role)+'">'+esc(role)+'</span>'+esc(g.label)+' <span class="muted" style="font-weight:400">&middot; '+(g.features||[]).length+'</span></h3>';
    var body=(g.features||[]).map(function(f){return featHtml(f);}).join("");
    sec.innerHTML=head+body;
    host.appendChild(sec);
  });
}
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
            layers: None,
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
            layers: None,
        };
        let text = compose_overview(&resolved, "repo", 0, &[]);
        assert!(text.contains("No features matched"));
    }

    fn card(label: &str, members: i32) -> (Layer, FeatureCard) {
        (
            Layer::Entry,
            FeatureCard {
                id: Uuid::new_v4(),
                label: label.to_string(),
                summary: None,
                member_count: members,
                role: "entry".into(),
                languages: vec![],
                top_symbols: vec![],
                key_files: vec![],
                folders: vec![],
                matched_by: vec![],
                repo: None,
                cross_links: vec![],
            },
        )
    }

    #[test]
    fn auto_domains_strips_common_prefix_and_splits_big_groups() {
        // 13 mint features + 3 settings features under one app root: the root
        // (16 > MAX_DOMAIN_GROUP) splits into its child folders.
        let mut cards: Vec<(Layer, FeatureCard)> = (0..13)
            .map(|i| card(&format!("apps/labs/src/mint/f{i}"), 10))
            .collect();
        cards.extend((0..3).map(|i| card(&format!("apps/labs/src/settings/s{i}"), 5)));
        let domains = auto_domains(&cards);
        let titles: Vec<&str> = domains.iter().map(|d| d.title.as_str()).collect();
        // Common prefix apps/labs/src is stripped from titles.
        assert!(titles.contains(&"mint"), "titles: {titles:?}");
        assert!(titles.contains(&"settings"), "titles: {titles:?}");
        // Largest domain first; nothing is curated.
        assert_eq!(domains[0].title, "mint");
        assert!(domains.iter().all(|d| !d.curated));
        let total: usize = domains.iter().map(|d| d.features.len()).sum();
        assert_eq!(total, 16);
    }

    #[test]
    fn auto_domains_small_set_is_one_group() {
        let cards = vec![card("a/x", 1), card("a/y", 2)];
        let domains = auto_domains(&cards);
        assert_eq!(domains.len(), 1);
        assert_eq!(domains[0].title, "all features");
        assert_eq!(domains[0].features.len(), 2);
    }

    #[test]
    fn curated_domains_match_by_label_fragment_and_report_unmatched() {
        let cards = vec![
            card("apps/labs/src/mint/steps", 10),
            card("apps/labs/src/mint/forms", 8),
            card("apps/labs/src/settings", 3),
        ];
        let spec = CurationSpec {
            groups: vec![CurationGroup {
                title: "Minting".into(),
                icon: Some("🧬".into()),
                blurb: Some("the wizard".into()),
                features: vec![
                    CurationFeature {
                        label: "mint/steps".into(), // trailing fragment
                        note: Some("step definitions".into()),
                    },
                    CurationFeature {
                        label: "no/such/feature".into(),
                        note: None,
                    },
                ],
            }],
        };
        let (domains, unmatched) = curated_domains(&cards, &spec);
        assert_eq!(unmatched, vec!["no/such/feature".to_string()]);
        assert!(domains[0].curated);
        assert_eq!(domains[0].features.len(), 1);
        assert_eq!(domains[0].features[0].label, "apps/labs/src/mint/steps");
        assert_eq!(
            domains[0].features[0].note.as_deref(),
            Some("step definitions")
        );
        // The two unplaced cards fall back to auto domains.
        let auto_total: usize = domains
            .iter()
            .filter(|d| !d.curated)
            .map(|d| d.features.len())
            .sum();
        assert_eq!(auto_total, 2);
    }
}
