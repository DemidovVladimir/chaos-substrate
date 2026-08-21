//! `chaos feature-story` — the cross-repository **feature story**.
//!
//! Where `chaos_features --project` lists *every* feature across a project's
//! repos (an inventory), this answers a focused question: *"how does feature X
//! work across the whole stack?"* Given a project (a named set of indexed repos)
//! and a feature query, it:
//!
//! 1. matches the feature in **each** member repo (L1 community semantic search plus a lexical label fallback), recording which repos are involved and which are not;
//! 2. loads the persisted **cross-repo links** and traverses them, pulling in a link's other endpoint (e.g. the Solidity contract a client calls) even when the query didn't match it directly;
//! 3. orders the involved features into a journey-layer **spine** (entry → interface → core → foundation) — the client → backend → contracts narrative;
//! 4. renders a clickable **multi-page site**: an index page (the spine + the cross-repo link chain) and one drill-down page per involved feature, each hash-gated so an unchanged page is never rewritten.
//!
//! Like the rest of Chaos this is deterministic and embedder-light (ONE embed
//! for the whole query, reused across repos): the page describes real indexed
//! structure and says so. Flowing prose is the agent's half — narrate the
//! returned spine, or persist an enriched engineer page with
//! `chaos_write_feature_website`.

use crate::{
    embedding::Embedder,
    export_util::{escape_script_json, existing_content_hash, html_escape, safe_slug},
    extractor::hash,
    feature_context::correlate_feature_manifests,
    feature_inventory::{self, FeatureSymbol, LangCount},
    hierarchy_export::CommunityHierarchy,
    layering::{self, Layer},
    provenance::{source, Breadcrumb},
    storage::Storage,
    theme,
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

/// Element id of the embedded manifest block (index and per-feature pages).
const MANIFEST_ID: &str = "chaos-feature-story-manifest";

/// Minimum cosine for a community to count as a matched feature (the same floor
/// `query_repo_hierarchical` uses).
const MIN_SEMANTIC_SCORE: f64 = 0.30;
/// Score recorded for a feature matched only by a lexical label hit.
const LABEL_ROUTE_SCORE: f64 = 0.5;
/// Representative members loaded per feature (symbols/files/folders/languages).
const TOP_MEMBERS: usize = 24;
/// Default cap on matched features surfaced per repo (semantic + lexical).
const DEFAULT_PER_REPO: usize = 3;
/// Max one-line rows in the compact return (the HTML site holds everything).
const MAX_COMPACT_ROWS: usize = 40;

#[derive(Debug, Default, Clone)]
pub struct FeatureStoryOptions {
    /// Style preset (`editorial` default light, `blade-runner` dark neon).
    pub style: Option<String>,
    /// Brand preset shipped inside Chaos (e.g. `molecule`).
    pub brand_preset: Option<String>,
    /// Override the default `<workspace>/<slug>-story.html` index path.
    pub output_html: Option<PathBuf>,
    /// Cap on matched features per repo. `0` = the default (`DEFAULT_PER_REPO`).
    pub limit: usize,
}

/// One involved feature in the story (a matched L1 community, or one pulled in
/// by a cross-repo link).
#[derive(Debug, Clone, Serialize)]
struct StoryNode {
    id: Uuid,
    repo_id: Uuid,
    alias: String,
    label: String,
    layer: String,
    /// `matched` (the query found it) or `linked-in` (reached via a link).
    role: String,
    score: f64,
    member_count: i32,
    summary: Option<String>,
    languages: Vec<LangCount>,
    top_symbols: Vec<FeatureSymbol>,
    key_files: Vec<String>,
    folders: Vec<String>,
    matched_by: Vec<Breadcrumb>,
    /// Relative path (from the index) to this feature's drill-down page.
    page: String,
    /// Lifecycle: `active` (default), `legacy` (superseded by another feature),
    /// or `variant` (near-duplicate of a sibling, no supersession direction).
    /// Set by `detect_supersession`. See [`crate::feature_story`] module docs.
    #[serde(default)]
    status: String,
    /// For `legacy` nodes: the community id of the current replacement.
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by_alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by_page: Option<String>,
    /// For `variant` nodes: sibling near-duplicate community ids.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    variant_of: Vec<Uuid>,
    /// The decisive lifecycle evidence (doc snippet or similarity score).
    #[serde(skip_serializing_if = "Option::is_none")]
    lifecycle_evidence: Option<Breadcrumb>,
}

/// One cross-repo link in the story (consumer → provider).
#[derive(Debug, Clone, Serialize)]
struct StoryEdge {
    source_id: Uuid,
    target_id: Uuid,
    source: String,
    target: String,
    source_alias: String,
    target_alias: String,
    source_page: String,
    target_page: String,
    kind: String,
    matched: Vec<String>,
    confidence: f64,
}

#[derive(Debug, Serialize)]
struct FeatureStoryManifest {
    schema_version: &'static str,
    project: String,
    feature: String,
    title: String,
    subtitle: String,
    site_dir: String,
    /// Involved features, journey-layer ordered (entry → … → foundation).
    spine: Vec<StoryNode>,
    edges: Vec<StoryEdge>,
    links_by_kind: BTreeMap<String, usize>,
    /// `alias — reason` for every member repo not in the story.
    not_involved: Vec<String>,
    content_hash: String,
    provenance: Vec<Breadcrumb>,
    warnings: Vec<String>,
}

/// One per-feature site page, ready to write (mirrors `compose::SitePage`).
struct SitePage {
    rel_path: String,
    manifest: Value,
    content_hash: String,
}

/// A member repo's loaded community hierarchy, kept so linked-in endpoints can
/// be materialized from the same data the matching used.
struct Loaded {
    repo_id: Uuid,
    alias: String,
    root: PathBuf,
    hierarchy: CommunityHierarchy,
}

/// Build the cross-repo feature story site for `feature` across `project`.
pub async fn run(
    storage: &Storage,
    embedder: &dyn Embedder,
    project: &str,
    feature: &str,
    opts: &FeatureStoryOptions,
) -> Result<Value> {
    let feature = feature.trim();
    anyhow::ensure!(!feature.is_empty(), "feature query must not be empty");
    let project = storage.find_project(project).await?.with_context(|| {
        format!("project does not exist: {project} (create it with `chaos project create`)")
    })?;
    let members = storage.project_member_repos(project.id).await?;
    anyhow::ensure!(
        !members.is_empty(),
        "project {} has no repositories — add one with `chaos project add-repo {} <repo-path>`",
        project.name,
        project.name
    );

    // Style/brand resolve up front so an unknown preset is a loud error.
    let style = opts.style.clone().unwrap_or_default();
    let style_css = theme::style_preset(&style).with_context(|| {
        format!(
            "unknown style preset '{style}' — available: {}",
            theme::STYLE_PRESETS.join(", ")
        )
    })?;
    let brand = match opts.brand_preset.as_deref() {
        Some(name) => {
            theme::brand_preset(name)
                .with_context(|| format!("unknown brand preset '{name}'"))?
                .brand
        }
        None => theme::Brand::default(),
    };

    let mut provenance: Vec<Breadcrumb> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    // Docs members (registered via `project add-docs`) are a documentation
    // SOURCE for the supersession scan, not code repos: they don't contribute
    // spine steps and aren't counted as repos.
    let code_member_count = members.iter().filter(|m| !m.is_project_docs).count();
    let docs_member_count = members.len() - code_member_count;
    provenance.push(Breadcrumb::new(
        source::POSTGRES,
        "project_member_repos",
        format!(
            "resolved project `{}` → {} code repo(s){}",
            project.name,
            code_member_count,
            if docs_member_count > 0 {
                format!(" + {docs_member_count} docs source(s)")
            } else {
                String::new()
            }
        ),
    ));

    // ONE embed for the whole query, reused across every repo.
    let query_emb = embedder.embed(feature).await?;

    // Load every CODE member's L1 hierarchy and index its communities for
    // lookup (docs members are searched later, in the supersession pass).
    let mut loaded: Vec<Loaded> = Vec::new();
    let mut cidx: HashMap<Uuid, (usize, usize)> = HashMap::new();
    for m in &members {
        if m.is_project_docs {
            continue;
        }
        let hierarchy = storage
            .load_community_hierarchy(&m.repo, TOP_MEMBERS)
            .await?;
        for (ci, c) in hierarchy.communities.iter().enumerate() {
            cidx.insert(c.id, (loaded.len(), ci));
        }
        loaded.push(Loaded {
            repo_id: m.repo.id,
            alias: m.alias.clone(),
            root: PathBuf::from(&m.repo.root_path),
            hierarchy,
        });
    }

    // --- 1. Match the feature in each repo. ---
    let per_repo = if opts.limit > 0 {
        opts.limit
    } else {
        DEFAULT_PER_REPO
    };
    let tokens = lexical_tokens(feature);
    // community id → (role, score, matched_by) for matched features.
    let mut roles: HashMap<Uuid, (&'static str, f64, Vec<Breadcrumb>)> = HashMap::new();
    let mut matched_repo_count = 0usize;
    for lo in &loaded {
        let cap = (lo.hierarchy.communities.len().max(12)) as i64;
        let sem = storage
            .community_semantic_search(
                lo.repo_id,
                embedder.provider(),
                embedder.model_id(),
                embedder.dimensions(),
                &query_emb,
                cap,
            )
            .await?;
        let mut hits: Vec<(Uuid, f64)> = sem
            .iter()
            .filter(|m| m.score >= MIN_SEMANTIC_SCORE && m.member_count >= 2)
            .map(|m| (m.id, m.score))
            .collect();
        hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        hits.truncate(per_repo);
        let mut added = 0usize;
        for (id, score) in &hits {
            roles.entry(*id).or_insert((
                "matched",
                *score,
                vec![Breadcrumb::new(
                    source::EMBEDDING,
                    "community_semantic_search",
                    format!(
                        "cosine {score:.2} vs the feature summary embedding in {}",
                        lo.alias
                    ),
                )],
            ));
            added += 1;
        }
        // Lexical label fallback for abbreviation-named features semantic missed.
        if !tokens.is_empty() {
            for c in &lo.hierarchy.communities {
                if added >= per_repo {
                    break;
                }
                if roles.contains_key(&c.id) {
                    continue;
                }
                let label = c.label.to_ascii_lowercase();
                if tokens.iter().any(|t| label.contains(t.as_str())) {
                    roles.insert(
                        c.id,
                        (
                            "matched",
                            LABEL_ROUTE_SCORE,
                            vec![Breadcrumb::new(
                                source::GRAPH,
                                "label_match",
                                format!("`{feature}` matched this feature's label in {}", lo.alias),
                            )],
                        ),
                    );
                    added += 1;
                }
            }
        }
        if added > 0 {
            matched_repo_count += 1;
        }
    }
    provenance.push(Breadcrumb::new(
        source::EMBEDDING,
        "community_semantic_search",
        format!(
            "matched the feature in {matched_repo_count}/{} repo(s) → {} feature(s) (cosine ≥ {MIN_SEMANTIC_SCORE:.2}, + lexical label fallback)",
            code_member_count,
            roles.len()
        ),
    ));

    // --- 2. Load cross-repo links and traverse from the matched features. ---
    let links = storage.load_project_links(project.id).await?;
    let matched_ids: HashSet<Uuid> = roles.keys().copied().collect();
    let mut kept_edges: Vec<&crate::models::CrossRepoLink> = Vec::new();
    let mut dropped_endpoints = 0usize;
    for l in &links {
        if !matched_ids.contains(&l.source_community_id)
            && !matched_ids.contains(&l.target_community_id)
        {
            continue;
        }
        // Both endpoints must be materializable (present in a loaded hierarchy).
        if !cidx.contains_key(&l.source_community_id) || !cidx.contains_key(&l.target_community_id)
        {
            dropped_endpoints += 1;
            continue;
        }
        kept_edges.push(l);
        for (cid, other) in [
            (l.source_community_id, l.target_community_id),
            (l.target_community_id, l.source_community_id),
        ] {
            roles.entry(cid).or_insert_with(|| {
                let other_alias = cidx
                    .get(&other)
                    .map(|&(li, _)| loaded[li].alias.as_str())
                    .unwrap_or("?");
                (
                    "linked-in",
                    0.0,
                    vec![Breadcrumb::new(
                        source::GRAPH,
                        "cross_repo_link",
                        format!("pulled in via a `{}` link with {other_alias}", l.kind),
                    )],
                )
            });
        }
    }
    if dropped_endpoints > 0 {
        warnings.push(format!(
            "{dropped_endpoints} cross-repo link(s) touched a community with no L1 detail (sub-threshold) and were skipped"
        ));
    }
    provenance.push(Breadcrumb::new(
        source::POSTGRES,
        "load_project_links",
        format!(
            "loaded {} cross-repo link(s); kept {} touching the matched features → {} involved feature(s)",
            links.len(),
            kept_edges.len(),
            roles.len()
        ),
    ));

    // --- 3. Assign page paths + materialize involved nodes. ---
    let slug = safe_slug(feature, "feature");
    let index_file = format!("{slug}-story.html");
    let site_dir_name = format!("{slug}-story");
    let mut involved_ids: Vec<Uuid> = roles.keys().copied().collect();
    // Stable naming order: alias, then label.
    involved_ids.sort_by(|a, b| {
        let ka = node_key(&loaded, &cidx, *a);
        let kb = node_key(&loaded, &cidx, *b);
        ka.cmp(&kb)
    });
    let mut taken: HashMap<String, usize> = HashMap::new();
    let mut rel_path_for: HashMap<Uuid, String> = HashMap::new();
    for id in &involved_ids {
        let (li, ci) = cidx[id];
        let lo = &loaded[li];
        let base = safe_slug(
            &format!("{}-{}", lo.alias, lo.hierarchy.communities[ci].label),
            "feature",
        );
        let n = taken.entry(base.clone()).or_insert(0);
        *n += 1;
        let name = if *n == 1 { base } else { format!("{base}-{n}") };
        rel_path_for.insert(*id, format!("{site_dir_name}/{name}.html"));
    }

    let mut spine: Vec<(Layer, StoryNode)> = Vec::new();
    for id in &involved_ids {
        let (li, ci) = cidx[id];
        let lo = &loaded[li];
        let detail = &lo.hierarchy.communities[ci];
        let (role, score, matched_by) = roles[id].clone();
        let (layer, node) = materialize(
            detail,
            lo.repo_id,
            &lo.alias,
            role,
            score,
            matched_by,
            rel_path_for[id].clone(),
        );
        spine.push((layer, node));
    }
    // Journey-layer spine: entry → interface → core → foundation, largest first.
    spine.sort_by(|a, b| {
        a.0.rank()
            .cmp(&b.0.rank())
            .then_with(|| b.1.member_count.cmp(&a.1.member_count))
            .then_with(|| a.1.alias.cmp(&b.1.alias))
            .then_with(|| a.1.label.cmp(&b.1.label))
    });
    let mut spine: Vec<StoryNode> = spine.into_iter().map(|(_, n)| n).collect();
    // Lifecycle pass: detect superseded/legacy features (e.g. an IPNFT stack
    // replaced by OCL) so a replaced stack is not interleaved with its
    // replacement as co-equal steps. Runs while `spine` is still owned, before
    // the immutable `node_by_id` borrow below.
    detect_supersession(
        storage,
        embedder,
        &mut spine,
        &kept_edges,
        &members,
        &mut provenance,
    )
    .await?;
    let node_by_id: HashMap<Uuid, &StoryNode> = spine.iter().map(|n| (n.id, n)).collect();
    provenance.push(Breadcrumb::new(
        source::GRAPH,
        "order_spine",
        "ordered involved features by journey layer (entry → interface → core → foundation)"
            .to_string(),
    ));

    // Edges (with endpoint page links), and links-by-kind tally.
    let mut edges: Vec<StoryEdge> = Vec::new();
    let mut links_by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for l in &kept_edges {
        let s = node_by_id.get(&l.source_community_id);
        let t = node_by_id.get(&l.target_community_id);
        let (Some(s), Some(t)) = (s, t) else { continue };
        *links_by_kind.entry(l.kind.clone()).or_insert(0) += 1;
        edges.push(StoryEdge {
            source_id: l.source_community_id,
            target_id: l.target_community_id,
            source: format!("{}:{}", s.alias, s.label),
            target: format!("{}:{}", t.alias, t.label),
            source_alias: s.alias.clone(),
            target_alias: t.alias.clone(),
            source_page: s.page.clone(),
            target_page: t.page.clone(),
            kind: l.kind.clone(),
            matched: matched_symbols(&l.evidence),
            confidence: l.confidence,
        });
    }

    // Repos with no involved feature at all.
    let involved_repos: HashSet<Uuid> = spine.iter().map(|n| n.repo_id).collect();
    let not_involved: Vec<String> = members
        .iter()
        .filter(|m| !m.is_project_docs && !involved_repos.contains(&m.repo.id))
        .map(|m| {
            format!(
                "{}: no feature matched `{feature}` and no cross-repo link reaches it",
                m.alias
            )
        })
        .collect();

    // --- 4. Build the site (per-feature pages, then the index). ---
    let workspace = crate::project::project_workspace_dir(&project.name);
    let output = opts
        .output_html
        .clone()
        .unwrap_or_else(|| workspace.join(&index_file));

    let site_pages = build_site_pages(
        &spine,
        &edges,
        &node_by_id,
        &loaded,
        &cidx,
        &index_file,
        feature,
        &project.name,
    )?;

    // Index content hash: stable essentials + every per-feature page hash. The
    // index hash covers the page hashes, so "index unchanged" ⇒ "site unchanged"
    // (but the files must actually be on disk).
    let hash_input = serde_json::to_string(&json!({
        "project": project.name,
        "feature": feature,
        "spine": spine.iter().map(|n| json!({
            "id": n.id, "label": n.label, "alias": n.alias, "role": n.role,
            "layer": n.layer, "member_count": n.member_count, "page": n.page,
            "status": n.status, "superseded_by": n.superseded_by,
        })).collect::<Vec<_>>(),
        "edges": edges.iter().map(|e| json!({
            "s": e.source_id, "t": e.target_id, "kind": e.kind,
            "matched": e.matched, "confidence": e.confidence,
        })).collect::<Vec<_>>(),
        "pages": site_pages.iter().map(|p| json!({"page": p.rel_path, "hash": p.content_hash})).collect::<Vec<_>>(),
    }))?;
    let content_hash = hash(&hash_input);

    let pages_on_disk = site_pages.iter().all(|p| {
        existing_content_hash(&workspace.join(&p.rel_path), MANIFEST_ID).as_deref()
            == Some(p.content_hash.as_str())
    });
    let cached = existing_content_hash(&output, MANIFEST_ID).as_deref()
        == Some(content_hash.as_str())
        && pages_on_disk;

    let mut pages_written = 0usize;
    let mut pages_cached = 0usize;
    if cached {
        pages_cached = site_pages.len();
        provenance.push(Breadcrumb::new(
            source::MERKLE,
            "content_hash",
            format!("existing site already holds story {content_hash} — all writes skipped"),
        ));
    } else {
        for page in &site_pages {
            let path = workspace.join(&page.rel_path);
            if existing_content_hash(&path, MANIFEST_ID).as_deref()
                == Some(page.content_hash.as_str())
            {
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
        provenance.push(Breadcrumb::new(
            source::MERKLE,
            "content_hash",
            format!(
                "story hashed as {content_hash}; {pages_written} feature page(s) written, {pages_cached} unchanged (per-page hash gate)"
            ),
        ));
    }

    let (title, subtitle) = framing(&project.name, feature, &spine, code_member_count);
    let manifest = FeatureStoryManifest {
        schema_version: "feature-story-2",
        project: project.name.clone(),
        feature: feature.to_string(),
        title: title.clone(),
        subtitle,
        site_dir: site_dir_name.clone(),
        spine,
        edges,
        links_by_kind: links_by_kind.clone(),
        not_involved: not_involved.clone(),
        content_hash: content_hash.clone(),
        provenance: provenance.clone(),
        warnings: warnings.clone(),
    };

    if !cached {
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&output, render_index_html(&manifest, style_css, &brand)?)?;
    }

    Ok(compact_return(
        &manifest,
        &output,
        &site_dir_name,
        site_pages.len(),
        pages_written,
        pages_cached,
        cached,
    ))
}

/// Build a `StoryNode` from a community detail (its journey layer, top
/// symbols/files/folders/languages) — feature-scoped code surface.
#[allow(clippy::too_many_arguments)]
fn materialize(
    detail: &crate::hierarchy_export::CommunityDetail,
    repo_id: Uuid,
    alias: &str,
    role: &str,
    score: f64,
    matched_by: Vec<Breadcrumb>,
    page: String,
) -> (Layer, StoryNode) {
    let layer = layering::classify_community(&detail.top_members);
    let key_files = feature_inventory::distinct_files(&detail.top_members);
    let languages = feature_inventory::language_tally(&key_files);
    let folders = feature_inventory::top_folders(&key_files);
    let mut seen: HashSet<&str> = HashSet::new();
    let top_symbols: Vec<FeatureSymbol> = detail
        .top_members
        .iter()
        .filter(|(name, _, _)| seen.insert(name.as_str()))
        .take(8)
        .map(|(name, kind, file)| FeatureSymbol {
            name: name.clone(),
            kind: kind.clone(),
            file: file.clone(),
        })
        .collect();
    let node = StoryNode {
        id: detail.id,
        repo_id,
        alias: alias.to_string(),
        label: detail.label.clone(),
        layer: layer.as_str().to_string(),
        role: role.to_string(),
        score,
        member_count: detail.member_count,
        summary: detail.summary.clone(),
        languages,
        top_symbols,
        key_files: key_files.into_iter().take(8).collect(),
        folders,
        matched_by,
        page,
        status: "active".to_string(),
        superseded_by: None,
        superseded_by_label: None,
        superseded_by_alias: None,
        superseded_by_page: None,
        variant_of: Vec::new(),
        lifecycle_evidence: None,
    };
    (layer, node)
}

/// `(alias, label)` sort key for a community id, for stable page naming.
fn node_key(loaded: &[Loaded], cidx: &HashMap<Uuid, (usize, usize)>, id: Uuid) -> (String, String) {
    match cidx.get(&id) {
        Some(&(li, ci)) => (
            loaded[li].alias.clone(),
            loaded[li].hierarchy.communities[ci].label.clone(),
        ),
        None => (String::new(), String::new()),
    }
}

/// Minimum L3-summary cosine for two cross-repo features to count as
/// near-duplicates (a candidate supersession/variant pair).
const SUPERSEDE_SIM: f64 = 0.85;
/// Minimum shared top-symbol names for the structural (D2) supersession signal —
/// confirms a near-verbatim port rather than mere topical similarity.
const D2_MIN_SYMBOL_OVERLAP: usize = 2;
/// Minimum doc chunks that must mark a feature legacy before it can be demoted
/// (a margin so one stray "no X" can't outweigh repeated "legacy X" evidence).
const MIN_LEGACY_DOC_HITS: usize = 2;

/// Tokens too generic to identify a specific feature in prose (so they can't
/// drive a co-mention match or a symbol overlap).
const GENERIC_TOKENS: [&str; 26] = [
    "service",
    "services",
    "contract",
    "contracts",
    "token",
    "tokens",
    "core",
    "client",
    "api",
    "store",
    "main",
    "index",
    "feature",
    "features",
    "lib",
    "libs",
    "src",
    "app",
    "apps",
    "common",
    "utils",
    "util",
    "types",
    "config",
    "packages",
    "package",
];

fn is_generic_token(t: &str) -> bool {
    GENERIC_TOKENS.contains(&t)
}

/// Distinctive identity tokens for a feature: its label words, alias, and
/// top-symbol names (lowercased, ≥3 chars, generics dropped). Returns
/// `(distinctive, symbols)` where `symbols` is the symbol-name subset used for
/// the D2 overlap gate.
fn node_tokens(node: &StoryNode) -> (HashSet<String>, HashSet<String>) {
    let mut distinctive: HashSet<String> = HashSet::new();
    let mut symbols: HashSet<String> = HashSet::new();
    for t in lexical_tokens(&node.label) {
        if !is_generic_token(&t) {
            distinctive.insert(t);
        }
    }
    let a = node.alias.to_ascii_lowercase();
    if a.len() >= 3 && !is_generic_token(&a) {
        distinctive.insert(a);
    }
    for s in &node.top_symbols {
        let name = s.name.to_ascii_lowercase();
        if name.len() >= 3 && !is_generic_token(&name) {
            symbols.insert(name.clone());
            distinctive.insert(name);
        }
    }
    (distinctive, symbols)
}

/// Supersession cue substrings (presence only; direction is parsed separately).
const CUES: [&str; 9] = [
    "replace",
    "supersede",
    "superseded",
    "legacy",
    "deprecat",
    "no longer",
    "replaced by",
    "in favor of",
    "in favour of",
];

fn has_cue(content_lc: &str) -> bool {
    CUES.iter().any(|c| content_lc.contains(c))
}

fn legacy_markers(content_lc: &str, tokens: &HashSet<String>) -> bool {
    tokens.iter().any(|t| {
        content_lc.contains(&format!("no {t}"))
            || content_lc.contains(&format!("legacy {t}"))
            || content_lc.contains(&format!("{t} is legacy"))
            || content_lc.contains(&format!("{t} legacy"))
            || content_lc.contains(&format!("deprecated {t}"))
            || content_lc.contains(&format!("{t} is deprecated"))
            || content_lc.contains(&format!("{t} deprecated"))
            || content_lc.contains(&format!("{t} replaced"))
            || content_lc.contains(&format!("{t} superseded"))
            || content_lc.contains(&format!("{t} is no longer"))
            || content_lc.contains(&format!("{t} no longer"))
            || content_lc.contains(&format!("remove {t}"))
            || content_lc.contains(&format!("removing {t}"))
    })
}

fn current_markers(content_lc: &str, tokens: &HashSet<String>) -> bool {
    tokens.iter().any(|t| {
        content_lc.contains(&format!("{t} replaces"))
            || content_lc.contains(&format!("{t} supersedes"))
            || content_lc.contains(&format!("{t} supersede"))
            || content_lc.contains(&format!("replaced by {t}"))
            || content_lc.contains(&format!("superseded by {t}"))
            || content_lc.contains(&format!("in favor of {t}"))
            || content_lc.contains(&format!("in favour of {t}"))
            || content_lc.contains(&format!("use {t} instead"))
            || content_lc.contains(&format!("migrate to {t}"))
            || content_lc.contains(&format!("migrated to {t}"))
    })
}

/// Detect supersession/variant relationships between the involved features and
/// stamp `status` / `superseded_by` / `variant_of` on the spine. Conservative:
/// a feature is only marked `legacy` with explicit doc evidence (D1) or a
/// tightly-gated structural asymmetry (D2); otherwise near-duplicates are at
/// most `variant`. Never reclassifies when there is no near-duplicate twin.
#[allow(clippy::too_many_arguments)]
async fn detect_supersession(
    storage: &Storage,
    embedder: &dyn Embedder,
    spine: &mut [StoryNode],
    kept_edges: &[&crate::models::CrossRepoLink],
    members: &[crate::models::ProjectRepo],
    provenance: &mut Vec<Breadcrumb>,
) -> Result<()> {
    if spine.len() < 2 {
        return Ok(());
    }

    struct Snap {
        id: Uuid,
        repo_id: Uuid,
        alias: String,
        label: String,
        page: String,
        role: String,
        score: f64,
        /// IDENTITY tokens (alias + label words, no symbols) — used for the
        /// legacy/current marker test. Symbols are deliberately excluded: a port
        /// SHARES symbols with its replacement, so symbol tokens sit next to both
        /// legacy and current cue words and would flag both sides.
        identity: HashSet<String>,
        /// Symbol-name tokens — the port-overlap gate only.
        symbols: HashSet<String>,
    }
    let snaps: Vec<Snap> = spine
        .iter()
        .map(|n| {
            let (_distinctive, symbols) = node_tokens(n);
            let mut identity: HashSet<String> = lexical_tokens(&n.label)
                .into_iter()
                .filter(|t| !is_generic_token(t))
                .collect();
            let a = n.alias.to_ascii_lowercase();
            if a.len() >= 3 && !is_generic_token(&a) {
                identity.insert(a);
            }
            Snap {
                id: n.id,
                repo_id: n.repo_id,
                alias: n.alias.clone(),
                label: n.label.clone(),
                page: n.page.clone(),
                role: n.role.clone(),
                score: n.score,
                identity,
                symbols,
            }
        })
        .collect();
    let idx_of: HashMap<Uuid, usize> = snaps.iter().enumerate().map(|(i, s)| (s.id, i)).collect();

    // abi-linked node ids — the D2 structural-asymmetry signal.
    let mut abi_linked: HashSet<Uuid> = HashSet::new();
    for e in kept_edges {
        if e.kind == crate::linker::kind::ABI {
            abi_linked.insert(e.source_community_id);
            abi_linked.insert(e.target_community_id);
        }
    }

    // Signal (b): near-duplicate cross-repo community pairs (deterministic order).
    let ids: Vec<Uuid> = snaps.iter().map(|s| s.id).collect();
    let sims = storage
        .community_pairwise_similarity(
            &ids,
            embedder.provider(),
            embedder.model_id(),
            embedder.dimensions(),
        )
        .await?;
    let mut twins: Vec<(usize, usize, f64)> = Vec::new();
    for (a, b, score) in &sims {
        if *score < SUPERSEDE_SIM {
            continue;
        }
        let (Some(&ia), Some(&ib)) = (idx_of.get(a), idx_of.get(b)) else {
            continue;
        };
        if snaps[ia].repo_id == snaps[ib].repo_id {
            continue;
        }
        twins.push((ia, ib, *score));
    }
    // Signal (a): documentation evidence across all member repos (incl. docs).
    let cue_query =
        "replaces OR supersedes OR superseded OR legacy OR deprecated OR \"no longer\" OR \"replaced by\"";
    let probe_emb = embedder
        .embed("this feature replaces supersedes deprecates a legacy system; X is replaced by Y and is no longer used")
        .await?;
    let mut doc_chunks: HashMap<Uuid, String> = HashMap::new();
    let mut doc_loc: HashMap<Uuid, (String, Option<i32>)> = HashMap::new();
    for m in members {
        let kw = storage
            .keyword_search_docs(m.repo.id, cue_query, 40)
            .await?;
        let sem = storage
            .semantic_search_docs(
                m.repo.id,
                embedder.provider(),
                embedder.model_id(),
                embedder.dimensions(),
                &probe_emb,
                20,
            )
            .await?;
        for hit in kw.into_iter().chain(sem.into_iter()) {
            let lc = hit.content.to_ascii_lowercase();
            if !has_cue(&lc) {
                continue;
            }
            doc_loc
                .entry(hit.chunk_id)
                .or_insert((hit.file_path.clone().unwrap_or_default(), hit.line_start));
            doc_chunks.entry(hit.chunk_id).or_insert(lc);
        }
    }
    let mut doc_ids: Vec<Uuid> = doc_chunks.keys().copied().collect();
    doc_ids.sort();

    if twins.is_empty() && doc_ids.is_empty() {
        return Ok(()); // nothing to compare → never reclassify
    }

    let mut legacy_targets: HashMap<usize, Vec<(usize, Breadcrumb)>> = HashMap::new();
    let mut variant_links: HashMap<usize, Vec<(usize, f64)>> = HashMap::new();
    let mut current_side: HashSet<usize> = HashSet::new();
    let (mut d1_count, mut d2_count, mut variant_count, mut used_docs) =
        (0usize, 0usize, 0usize, false);

    // --- D1: documentation-evidence supersession. ---
    // Two gates, both required (conservative — false grouping is fine, a false
    // "legacy" is not):
    //   1. STRUCTURAL port: the two features are cross-repo and share ≥2 concrete
    //      symbol names (a near-verbatim port, e.g. Tokenizer→OclTokenizer share
    //      `onlyController`/`PermissionerUpdated`). This is robust and needs no
    //      embedding threshold.
    //   2. DOC direction: across the indexed docs, one side's identity carries a
    //      legacy marker ("no IPNFT", "legacy IPNFT", "IPNFT … replaced") while
    //      the other carries a current/replacement marker ("OCL replaces").
    // Doc signals are aggregated per feature across ALL chunks (the doc names the
    // stacks as concepts in different sections, not the two symbols together).
    // Count how many doc chunks carry a legacy vs current marker for each
    // feature's identity. Counts (not booleans) so a lone stray "no ocl" in one
    // unrelated file can't outweigh nine "legacy ipnft" hits.
    let n = snaps.len();
    let mut legacy_score = vec![0usize; n];
    let mut current_score = vec![0usize; n];
    let mut legacy_evid: Vec<Option<Uuid>> = vec![None; n];
    for cid in &doc_ids {
        let content_lc = &doc_chunks[cid];
        for i in 0..n {
            if legacy_markers(content_lc, &snaps[i].identity) {
                legacy_score[i] += 1;
                legacy_evid[i].get_or_insert(*cid);
            }
            if current_markers(content_lc, &snaps[i].identity) {
                current_score[i] += 1;
            }
        }
    }
    // Candidate symbol-port pairs (cross-repo, ≥2 shared symbols), deterministic.
    let mut port_pairs: Vec<(usize, usize)> = Vec::new();
    for i in 0..n {
        for j in (i + 1)..n {
            if snaps[i].repo_id == snaps[j].repo_id {
                continue;
            }
            if snaps[i].symbols.intersection(&snaps[j].symbols).count() >= D2_MIN_SYMBOL_OVERLAP {
                port_pairs.push((i, j));
            }
        }
    }
    for (i, j) in port_pairs {
        // The legacy side must (1) have ≥MIN_LEGACY_DOC_HITS legacy markers,
        // (2) be net-legacy itself, and (3) DOMINATE its twin — at least double
        // the twin's legacy evidence. Both systems are discussed in the same
        // migration docs, so the current side picks up stray "no X" mentions;
        // dominance is what cleanly separates the replaced stack from its
        // replacement. Conservative: anything short of dominance stays active.
        let dominates = |a: usize, b: usize| {
            legacy_score[a] >= MIN_LEGACY_DOC_HITS
                && legacy_score[a] > current_score[a]
                && legacy_score[a] > legacy_score[b]
                && legacy_score[a] >= 2 * legacy_score[b]
        };
        let dir = if dominates(i, j) {
            Some((i, j))
        } else if dominates(j, i) {
            Some((j, i))
        } else {
            None
        };
        if let Some((leg, cur)) = dir {
            used_docs = true;
            d1_count += 1;
            let (file, line) = legacy_evid[leg]
                .and_then(|c| doc_loc.get(&c).cloned())
                .unwrap_or_default();
            let mut crumb = Breadcrumb::new(
                source::DOCS,
                "supersession_doc",
                format!(
                    "“{}:{}” is a legacy port superseded by “{}:{}” — project docs mark it legacy/replaced (shares {} symbol(s))",
                    snaps[leg].alias,
                    snaps[leg].label,
                    snaps[cur].alias,
                    snaps[cur].label,
                    snaps[leg].symbols.intersection(&snaps[cur].symbols).count()
                ),
            );
            if !file.is_empty() {
                crumb = crumb.with_locator(match line {
                    Some(l) => format!("{file}:{l}"),
                    None => file,
                });
            }
            legacy_targets.entry(leg).or_default().push((cur, crumb));
            current_side.insert(cur);
        }
    }
    let legacy_nodes: HashSet<usize> = legacy_targets.keys().copied().collect();

    // --- D2 (structural) + variant over near-duplicate twins not already decided. ---
    for &(ia, ib, sim) in &twins {
        if legacy_nodes.contains(&ia) || legacy_nodes.contains(&ib) {
            continue; // already classified by doc evidence
        }
        let a_linked = snaps[ia].role == "linked-in"
            && snaps[ia].score == 0.0
            && abi_linked.contains(&snaps[ia].id);
        let b_linked = snaps[ib].role == "linked-in"
            && snaps[ib].score == 0.0
            && abi_linked.contains(&snaps[ib].id);
        let a_matched = snaps[ia].role == "matched" && snaps[ia].score >= MIN_SEMANTIC_SCORE;
        let b_matched = snaps[ib].role == "matched" && snaps[ib].score >= MIN_SEMANTIC_SCORE;
        let overlap = snaps[ia].symbols.intersection(&snaps[ib].symbols).count();
        if overlap >= D2_MIN_SYMBOL_OVERLAP {
            let asym = if a_linked && b_matched {
                Some((ia, ib))
            } else if b_linked && a_matched {
                Some((ib, ia))
            } else {
                None
            };
            if let Some((leg, cur)) = asym {
                d2_count += 1;
                let crumb = Breadcrumb::new(
                    source::EMBEDDING,
                    "supersession_structural",
                    format!(
                        "“{}:{}” appears superseded by “{}:{}”: near-duplicate (cosine {sim:.2}, {overlap} shared symbols), reached only via an ABI link while the other matches the feature directly",
                        snaps[leg].alias, snaps[leg].label, snaps[cur].alias, snaps[cur].label
                    ),
                );
                legacy_targets.entry(leg).or_default().push((cur, crumb));
                current_side.insert(cur);
                continue;
            }
        }
        variant_count += 1;
        variant_links.entry(ia).or_default().push((ib, sim));
        variant_links.entry(ib).or_default().push((ia, sim));
    }

    // Stamp nodes (legacy > active-as-current > variant > active).
    let (mut legacy_marked, mut variant_marked) = (0usize, 0usize);
    for (i, node) in spine.iter_mut().enumerate() {
        if let Some(targets) = legacy_targets.get(&i) {
            // Prefer the current twin that shares the most symbols (the true
            // structural counterpart), then highest score, then stable by id.
            let best = targets
                .iter()
                .max_by(|(ca, _), (cb, _)| {
                    let oa = snaps[i].symbols.intersection(&snaps[*ca].symbols).count();
                    let ob = snaps[i].symbols.intersection(&snaps[*cb].symbols).count();
                    oa.cmp(&ob)
                        .then_with(|| snaps[*ca].score.total_cmp(&snaps[*cb].score))
                        .then_with(|| snaps[*cb].id.cmp(&snaps[*ca].id))
                })
                .unwrap();
            let cur = best.0;
            let crumb = best.1.clone();
            node.status = "legacy".to_string();
            node.superseded_by = Some(snaps[cur].id);
            node.superseded_by_label = Some(snaps[cur].label.clone());
            node.superseded_by_alias = Some(snaps[cur].alias.clone());
            node.superseded_by_page = Some(snaps[cur].page.clone());
            node.matched_by.push(crumb.clone());
            node.lifecycle_evidence = Some(crumb);
            legacy_marked += 1;
            continue;
        }
        if current_side.contains(&i) {
            continue; // a determined replacement → stays active
        }
        if let Some(sibs) = variant_links.get(&i) {
            node.status = "variant".to_string();
            let mut of: Vec<Uuid> = sibs.iter().map(|(j, _)| snaps[*j].id).collect();
            of.sort();
            node.variant_of = of;
            let best_sim = sibs.iter().map(|(_, s)| *s).fold(0.0_f64, f64::max);
            node.lifecycle_evidence = Some(Breadcrumb::new(
                source::EMBEDDING,
                "near_duplicate_variant",
                format!(
                    "near-duplicate of {} sibling feature(s) (cosine up to {best_sim:.2}) — no supersession evidence; shown as variants",
                    sibs.len()
                ),
            ));
            variant_marked += 1;
        }
    }

    provenance.push(Breadcrumb::new(
        source::EMBEDDING,
        "supersession_scan",
        format!(
            "compared {} involved feature(s) pairwise (cosine ≥ {SUPERSEDE_SIM:.2}, cross-repo): {} near-duplicate pair(s) → {legacy_marked} legacy ({d1_count} via docs, {d2_count} via structural asymmetry), {variant_marked} variant ({variant_count} undirected pair(s))",
            snaps.len(),
            twins.len()
        ),
    ));
    if used_docs {
        provenance.push(Breadcrumb::new(
            source::DOCS,
            "supersession_doc_scan",
            format!(
                "scanned documentation chunks across {} member repo(s) for supersession language",
                members.len()
            ),
        ));
    }
    Ok(())
}

/// Build one per-feature page per involved node, mirroring the compose site
/// pattern: a deterministic walkthrough, this feature's cross-repo links, its
/// code, and prior overlapping pages — each with its own content hash.
#[allow(clippy::too_many_arguments)]
fn build_site_pages(
    spine: &[StoryNode],
    edges: &[StoryEdge],
    node_by_id: &HashMap<Uuid, &StoryNode>,
    loaded: &[Loaded],
    cidx: &HashMap<Uuid, (usize, usize)>,
    index_file: &str,
    feature: &str,
    project: &str,
) -> Result<Vec<SitePage>> {
    let mut pages = Vec::new();
    for node in spine {
        // This feature's cross-repo links, with the other endpoint cross-linked
        // (within the same site dir) and smart-contract tagged.
        let mut link_cards: Vec<Value> = Vec::new();
        for e in edges {
            let (direction, other_id) = if e.source_id == node.id {
                ("uses", e.target_id)
            } else if e.target_id == node.id {
                ("used by", e.source_id)
            } else {
                continue;
            };
            let Some(other) = node_by_id.get(&other_id) else {
                continue;
            };
            let smart_contract = other.languages.iter().any(|l| l.language == "Solidity");
            link_cards.push(json!({
                "direction": direction,
                "label": other.label,
                "alias": other.alias,
                "kind": e.kind,
                "matched": e.matched,
                "page": within_site(&other.page),
                "smart_contract": smart_contract,
            }));
        }

        // Prior generated pages overlapping THIS feature (embedder-free).
        let related_pages = match cidx.get(&node.id) {
            Some(&(li, _)) => {
                let files: HashSet<String> = node.key_files.iter().cloned().collect();
                let symbols: HashSet<String> =
                    node.top_symbols.iter().map(|s| s.name.clone()).collect();
                correlate_feature_manifests(
                    &crate::export_util::features_memory_dir(&loaded[li].root),
                    &files,
                    &symbols,
                    "",
                    5,
                )
                .unwrap_or_default()
            }
            None => Vec::new(),
        };

        let walkthrough = walkthrough_blocks(node, &link_cards);
        let mut manifest = json!({
            "schema_version": "feature-story-page-1",
            "project": project,
            "feature_query": feature,
            "feature": {
                "id": node.id,
                "label": node.label,
                "alias": node.alias,
                "layer": node.layer,
                "role": node.role,
                "summary": node.summary,
                "member_count": node.member_count,
                "languages": node.languages,
                "folders": node.folders,
                "status": node.status,
                "superseded_by_label": node.superseded_by_label,
                "superseded_by_alias": node.superseded_by_alias,
                "superseded_by_page": node.superseded_by_page,
                "lifecycle_evidence": node.lifecycle_evidence,
            },
            "walkthrough": walkthrough,
            "links": link_cards,
            "code": {"top_symbols": node.top_symbols, "key_files": node.key_files},
            "related_pages": related_pages,
            "back": format!("../{index_file}"),
            "honesty": "Generated deterministically from the indexed graph and the persisted cross-repo links — this describes real structure, not invented user journeys.",
            "content_hash": "",
        });
        let content_hash = hash(&serde_json::to_string(&manifest)?);
        manifest["content_hash"] = json!(content_hash);
        pages.push(SitePage {
            rel_path: node.page.clone(),
            manifest,
            content_hash,
        });
    }
    Ok(pages)
}

/// Deterministic walkthrough blocks for a per-feature page — built ONLY from the
/// indexed summary, journey layer, cross-repo links, and code location.
fn walkthrough_blocks(node: &StoryNode, links: &[Value]) -> Vec<Value> {
    let mut blocks = Vec::new();
    let what = node
        .summary
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            "No knowledge-base summary exists for this feature yet — run chaos_analyze so the L3 community summaries are generated.".to_string()
        });
    blocks.push(json!({"title": "What this is", "body": what}));

    let sits = match node.layer.as_str() {
        "entry" => "An entry-layer feature — part of what users or external callers touch first (this is where the story starts).",
        "interface" => "An interface-layer feature — a surface other code calls into (APIs, routes, resolvers).",
        "core" => "A core-layer feature — business logic behind the callable surface.",
        "foundation" => "A foundation-layer feature — contracts, infrastructure, or shared groundwork the other layers rest on (this is where the story ends).",
        _ => "Its journey layer could not be determined from the graph.",
    };
    blocks.push(json!({"title": format!("Where it sits — in {}", node.alias), "body": sits}));

    let uses = links.iter().filter(|l| l["direction"] == "uses").count();
    let used_by = links.len() - uses;
    let contracts: Vec<&str> = links
        .iter()
        .filter(|l| l["smart_contract"] == json!(true))
        .filter_map(|l| l["label"].as_str())
        .collect();
    let mut connects = if links.is_empty() {
        "No cross-repo links touch this feature — within this story it stands alone.".to_string()
    } else {
        format!(
            "It connects to {} feature(s) in other repos — {uses} it uses, {used_by} that use it.",
            links.len()
        )
    };
    if !contracts.is_empty() {
        connects.push_str(&format!(
            " It reaches the smart-contract layer: {} (Solidity).",
            contracts.join(", ")
        ));
    }
    blocks.push(json!({"title": "How it connects across repos", "body": connects}));

    blocks.push(json!({
        "title": "Where the code lives",
        "body": format!(
            "{} top file(s) under {}{}.",
            node.key_files.len(),
            if node.folders.is_empty() { "the repo root".to_string() } else { node.folders.join(", ") },
            if node.languages.is_empty() {
                String::new()
            } else {
                format!(
                    " — {}",
                    node.languages.iter().map(|l| format!("{} ({})", l.language, l.count)).collect::<Vec<_>>().join(", ")
                )
            }
        ),
    }));
    blocks
}

/// `<site-dir>/<file>.html` → `<file>.html` (sibling link within the site dir).
fn within_site(rel_path: &str) -> String {
    rel_path.rsplit('/').next().unwrap_or(rel_path).to_string()
}

/// Matched symbol names from a cross-repo link's evidence JSON.
fn matched_symbols(evidence: &Value) -> Vec<String> {
    evidence
        .get("matched")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Lexical tokens for the label fallback (≥3 chars, alphanumeric split).
fn lexical_tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_ascii_lowercase())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

fn framing(
    project: &str,
    feature: &str,
    spine: &[StoryNode],
    total_repos: usize,
) -> (String, String) {
    let involved: HashSet<&str> = spine.iter().map(|n| n.alias.as_str()).collect();
    let legacy = spine.iter().filter(|n| n.status == "legacy").count();
    let title = format!("How “{feature}” works across {project}");
    let mut subtitle = format!(
        "A cross-repo story over {} feature(s) in {}/{} repo(s), traced through the persisted cross-repo links — ordered client → backend → contracts. Built only from the chaos knowledge base.",
        spine.len(),
        involved.len(),
        total_repos
    );
    if legacy > 0 {
        subtitle.push_str(&format!(
            " {legacy} legacy/superseded feature(s) are shown separately."
        ));
    }
    (title, subtitle)
}

#[allow(clippy::too_many_arguments)]
fn compact_return(
    manifest: &FeatureStoryManifest,
    output: &Path,
    site_dir: &str,
    pages: usize,
    written: usize,
    cached_pages: usize,
    cached: bool,
) -> Value {
    let involved: Vec<Value> = manifest
        .spine
        .iter()
        .take(MAX_COMPACT_ROWS)
        .map(|n| {
            let mut line = format!(
                "[{}] {} — {}, {} members",
                n.alias, n.label, n.layer, n.member_count
            );
            if n.role == "matched" {
                line.push_str(&format!(" · matched ({:.2})", n.score));
            } else {
                line.push_str(" · linked-in");
            }
            if n.status == "legacy" {
                match (&n.superseded_by_alias, &n.superseded_by_label) {
                    (Some(a), Some(l)) => {
                        line.push_str(&format!(" · LEGACY (superseded by {a}:{l})"))
                    }
                    _ => line.push_str(" · LEGACY"),
                }
            } else if n.status == "variant" {
                line.push_str(" · variant");
            }
            let syms: Vec<&str> = n
                .top_symbols
                .iter()
                .take(4)
                .map(|s| s.name.as_str())
                .collect();
            if !syms.is_empty() {
                line.push_str(&format!(" · symbols: {}", syms.join(", ")));
            }
            json!(line)
        })
        .collect();
    let chain: Vec<Value> = manifest
        .edges
        .iter()
        .take(MAX_COMPACT_ROWS)
        .map(|e| {
            let m = if e.matched.is_empty() {
                String::new()
            } else {
                format!(": {}", e.matched.join(", "))
            };
            json!(format!("{} → {} ({}{m})", e.source, e.target, e.kind))
        })
        .collect();

    json!({
        "status": "ok",
        "project": manifest.project,
        "feature": manifest.feature,
        "title": manifest.title,
        "involved_repos": involved,
        "link_chain": chain,
        "links_by_kind": manifest.links_by_kind,
        "not_involved": manifest.not_involved,
        "site": {
            "dir": site_dir,
            "feature_pages": pages,
            "written": written,
            "cached": cached_pages,
        },
        "output_html": output,
        "content_hash": manifest.content_hash,
        "cached": cached,
        "provenance": manifest.provenance,
        "warnings": manifest.warnings,
        "next": if cached {
            "This exact story already exists (same content hash) — reuse the existing pages; do not re-ingest them. Narrate the spine for the user, or enrich a per-feature engineer page with chaos_write_feature_website."
        } else {
            "Cross-repo story site written. The index manifest (chaos-feature-story-manifest) holds the full spine + link chain; per-feature pages carry the code + links. Narrate the spine for the user; the content_hash is the dedup key."
        },
    })
}

fn render_index_html(
    manifest: &FeatureStoryManifest,
    style_css: &str,
    brand: &theme::Brand,
) -> Result<String> {
    let manifest_json = serde_json::to_string(manifest)?;
    Ok(INDEX_HTML
        .replace("__THEME__", theme::THEME_CSS)
        .replace("__STYLE__", style_css)
        .replace("__STORY_CSS__", STORY_CSS)
        .replace("__BRAND_TOPBAR__", &theme::render_brand(brand, "topbar"))
        .replace("__BRAND_FOOTER__", &theme::render_brand(brand, "footer"))
        .replace("__TITLE__", &html_escape(&manifest.title))
        .replace("__SUBTITLE__", &html_escape(&manifest.subtitle))
        .replace("__MANIFEST__", &escape_script_json(&manifest_json)))
}

fn render_feature_page_html(
    manifest: &Value,
    style_css: &str,
    brand: &theme::Brand,
) -> Result<String> {
    let title = manifest["feature"]["label"].as_str().unwrap_or("Feature");
    let alias = manifest["feature"]["alias"].as_str().unwrap_or("");
    let layer = manifest["feature"]["layer"].as_str().unwrap_or("unknown");
    let back = manifest["back"].as_str().unwrap_or("../");
    Ok(PAGE_HTML
        .replace("__THEME__", theme::THEME_CSS)
        .replace("__STYLE__", style_css)
        .replace("__STORY_CSS__", STORY_CSS)
        .replace("__BRAND_TOPBAR__", &theme::render_brand(brand, "topbar"))
        .replace("__BRAND_FOOTER__", &theme::render_brand(brand, "footer"))
        .replace("__TITLE__", &html_escape(title))
        .replace("__ALIAS__", &html_escape(alias))
        .replace("__LAYER__", &html_escape(layer))
        .replace("__BACK__", &html_escape(back))
        .replace(
            "__MANIFEST__",
            &escape_script_json(&serde_json::to_string(manifest)?),
        ))
}

const STORY_CSS: &str = r#"
[hidden]{display:none!important}
.hero .wrap{grid-template-columns:1fr}
.subtitle{max-width:1100px;color:var(--color-ink-400);line-height:1.55;font:var(--type-body-lg)}
main.wrap{padding:8px 0 40px}
section.story{padding:30px 0;border-bottom:var(--border-hairline)}section.story:last-of-type{border-bottom:0}
section.story>h2{margin:0 0 4px;font:var(--type-h4);color:var(--color-ink-700)}
.sec-sub{color:var(--fg-tertiary);font:var(--type-body-sm);margin:0 0 16px}
.spine{display:flex;flex-direction:column;gap:0}
.step{position:relative;padding:0 0 22px 26px;border-left:2px solid var(--color-surface-3)}
.step:last-child{border-left-color:transparent}
.step::before{content:"";position:absolute;left:-7px;top:4px;width:12px;height:12px;border-radius:50%;background:var(--color-blue-600);box-shadow:0 0 0 3px var(--color-surface-0)}
.step .card{background:var(--color-surface-0);border:var(--border-hairline);border-radius:var(--radius-lg);box-shadow:var(--shadow-sm);padding:14px 16px}
.card h3{margin:0;font:var(--type-h5);color:var(--color-ink-700);overflow-wrap:anywhere}
.card h3 a{color:var(--color-blue-700);text-decoration:none}.card h3 a:hover{text-decoration:underline}
.card p{margin:8px 0 0;color:var(--color-ink-500);line-height:1.55;font:var(--type-body-sm)}
.badge{display:inline-flex;border-radius:var(--radius-pill);padding:3px 10px;font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.06em;color:#fff;margin-right:6px}
.badge.entry{background:var(--color-blue-700)}.badge.interface{background:rgb(176,124,15)}.badge.core{background:var(--color-teal-500)}.badge.foundation{background:var(--color-violet-500)}.badge.unknown{background:var(--color-ink-300)}
.chip{display:inline-flex;border:var(--border-hairline);border-radius:var(--radius-pill);padding:2px 9px;margin:6px 6px 0 0;font:var(--type-body-sm);color:var(--color-ink-400);background:var(--color-surface-1)}
.chip.sc{background:var(--color-violet-500);color:#fff;border-color:transparent}
.chip.role{background:var(--color-surface-2)}
.mono{font-family:var(--font-mono);font-size:12px}
.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(300px,1fr));gap:12px}
.linkrow{display:flex;align-items:center;gap:8px;flex-wrap:wrap;padding:9px 0;border-bottom:var(--border-soft)}
.linkrow:last-child{border-bottom:0}
.linkrow .arrow{color:var(--fg-tertiary);font-family:var(--font-mono)}
.block-card{background:var(--color-surface-0);border:var(--border-hairline);border-radius:var(--radius-lg);box-shadow:var(--shadow-sm);padding:14px 16px;margin-top:12px}
.block-card h3{margin:0 0 6px;font:var(--type-h5);color:var(--color-blue-700)}
.block-card p{margin:0;color:var(--color-ink-500);line-height:1.6}
table{width:100%;border-collapse:collapse;margin-top:10px;font:var(--type-body-sm)}
th{text-align:left;color:var(--fg-tertiary);font:var(--type-overline-sm);text-transform:uppercase;padding:8px 10px;border-bottom:var(--border-hairline)}
td{padding:8px 10px;border-bottom:var(--border-soft);color:var(--color-ink-500);overflow-wrap:anywhere}
.honesty{border-left:3px solid var(--color-blue-400);background:var(--color-surface-1);padding:10px 14px;border-radius:var(--radius-sm);color:var(--fg-tertiary);font:var(--type-body-sm);margin-top:18px}
.backlink{font:var(--type-body-sm);font-weight:500}
.crumblist .item{border:var(--border-hairline);border-radius:var(--radius-md);background:var(--color-surface-1);padding:10px 12px;margin-top:8px}
.crumblist .item strong{color:var(--color-ink-700)}
.tag{display:inline-flex;border:var(--border-hairline);border-radius:var(--radius-pill);padding:2px 8px;margin-right:6px;font:var(--type-overline-sm);font-family:var(--font-mono);color:var(--color-ink-500);background:var(--color-surface-2)}
.badge.legacy{background:var(--color-ink-300)}
.chip.legacy{background:var(--color-ink-400);color:#fff;border-color:transparent}
.chip.variant{background:var(--color-violet-500);color:#fff;border-color:transparent}
.step.legacy .card{opacity:.66;border-style:dashed}
.supersede{margin-top:8px;font:var(--type-body-sm)}
.supersede a{color:var(--color-blue-700);font-weight:500}
.variant-group{border-left:2px dashed var(--color-violet-500);padding-left:10px}
.lc-note{margin-top:6px;color:var(--fg-tertiary);font:var(--type-body-sm)}
.legacy-banner{border-left:3px solid var(--color-ink-400);background:var(--color-surface-1);padding:10px 14px;border-radius:var(--radius-sm);color:var(--color-ink-600);font:var(--type-body-sm);margin:14px 0}
.legacy-banner.variant{border-left-color:var(--color-violet-500)}
"#;

const INDEX_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__TITLE__</title>
<style>
__THEME__
__STYLE__
__STORY_CSS__
</style>
</head>
<body>
<div class="topbar"><div class="wrap">__BRAND_TOPBAR__<span class="crumb">Feature story<span class="sep">&rsaquo;</span><b id="crumb"></b></span><span class="sp"></span><span class="pilltag">Cross-repo story</span></div></div>
<header class="hero"><div class="wrap"><div><div class="eyebrow">Cross-repo feature story</div><h1>__TITLE__</h1><div class="subtitle">__SUBTITLE__</div></div></div></header>
<main class="wrap wide">
<section class="story"><h2>The story across the stack</h2><div class="sec-sub">Each step is the matching feature in one repo, ordered client &rarr; backend &rarr; contracts. Click a feature to drill in.</div><div class="spine" id="spine"></div></section>
<section class="story" id="legacy-sec" hidden><h2>Legacy / superseded</h2><div class="sec-sub">Features a project doc or near-duplicate analysis marks as replaced. Shown apart from the active story so they're not read as current.</div><div class="spine" id="legacy"></div></section>
<section class="story"><h2>Cross-repo links</h2><div class="sec-sub">The persisted feature&rarr;feature links that connect the steps (consumer &rarr; provider).</div><div id="links"></div></section>
<section class="story" id="ni-sec" hidden><h2>Repos not in this story</h2><div id="notinvolved"></div></section>
<section class="story"><h2>How this was generated</h2><div class="sec-sub">Provenance breadcrumbs &mdash; where each piece came from.</div><div class="crumblist" id="prov"></div></section>
</main>
<footer><div class="wrap">__BRAND_FOOTER__<span class="sp"></span><span class="meta">generated by Chaos Substrate</span></div></footer>
<script type="application/json" id="chaos-feature-story-manifest">__MANIFEST__</script>
<script>
(function(){
var M=JSON.parse(document.getElementById("chaos-feature-story-manifest").textContent);
function esc(v){return String(v==null?"":v).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;").replace(/'/g,"&#039;");}
function el(t,c,h){var e=document.createElement(t);if(c)e.className=c;if(h!=null)e.innerHTML=h;return e;}
document.getElementById("crumb").textContent=M.feature;
var spine=document.getElementById("spine");
var legacyBox=document.getElementById("legacy");
function lcDetail(n){return (n.lifecycle_evidence&&n.lifecycle_evidence.detail)?n.lifecycle_evidence.detail:"";}
function renderStep(n){
  var step=el("div","step"+(n.status==="legacy"?" legacy":""));var card=el("div","card");
  var name=n.page?('<a href="'+esc(n.page)+'">'+esc(n.label)+"</a>"):esc(n.label);
  var role='<span class="chip role">'+esc(n.role)+(n.role==="matched"?" "+(Math.round((n.score||0)*100)/100):"")+"</span>";
  var lc=n.status==="legacy"?'<span class="chip legacy">legacy</span>':(n.status==="variant"?'<span class="chip variant">variant</span>':"");
  card.appendChild(el("h3",null,'<span class="badge '+esc(n.status==="legacy"?"legacy":n.layer)+'">'+esc(n.layer)+"</span>"+name+lc));
  card.appendChild(el("p",null,'<span class="chip">'+esc(n.alias)+"</span><span class=chip>"+(n.member_count||0)+" members</span>"+role));
  if(n.status==="legacy"&&n.superseded_by_page){card.appendChild(el("div","supersede",'Superseded by &rarr; <a href="'+esc(n.superseded_by_page)+'">'+esc(n.superseded_by_alias)+":"+esc(n.superseded_by_label)+"</a>"));}
  if(lcDetail(n))card.appendChild(el("div","lc-note",esc(lcDetail(n))));
  if(n.summary)card.appendChild(el("p",null,esc(n.summary)));
  var syms=(n.top_symbols||[]).slice(0,6).map(function(s){return '<span class="chip mono">'+esc(s.name)+"</span>";}).join("");
  if(syms)card.appendChild(el("p",null,syms));
  step.appendChild(card);return step;
}
var anyLegacy=false;
(M.spine||[]).forEach(function(n){
  if(n.status==="legacy"){legacyBox.appendChild(renderStep(n));anyLegacy=true;}
  else{spine.appendChild(renderStep(n));}
});
if(anyLegacy)document.getElementById("legacy-sec").hidden=false;
if(!(M.spine||[]).filter(function(n){return n.status!=="legacy";}).length)spine.appendChild(el("p","sec-sub","No active feature matched the query in any repo. Try a broader phrasing or check the project is indexed."));
var links=document.getElementById("links");
(M.edges||[]).forEach(function(e){
  var row=el("div","linkrow");
  var s=e.source_page?('<a href="'+esc(e.source_page)+'">'+esc(e.source)+"</a>"):esc(e.source);
  var t=e.target_page?('<a href="'+esc(e.target_page)+'">'+esc(e.target)+"</a>"):esc(e.target);
  var m=(e.matched||[]).length?(" &middot; "+(e.matched||[]).map(esc).join(", ")):"";
  row.innerHTML=s+' <span class="arrow">&rarr;</span> '+t+' <span class="tag">'+esc(e.kind)+"</span><span class=sec-sub style='margin:0'>"+m+"</span>";
  links.appendChild(row);
});
if(!(M.edges||[]).length)links.appendChild(el("p","sec-sub","No cross-repo links connect the matched features — at the project level these stand alone."));
var ni=(M.not_involved||[]);
if(ni.length){document.getElementById("ni-sec").hidden=false;var nd=document.getElementById("notinvolved");ni.forEach(function(x){nd.appendChild(el("p","sec-sub",esc(x)));});}
var prov=document.getElementById("prov");
(M.provenance||[]).forEach(function(c){prov.appendChild(el("div","item",'<strong>'+esc(c.source)+'</strong> <span class="tag">'+esc(c.method)+"</span><div class=sec-sub style='margin:4px 0 0'>"+esc(c.detail)+(c.locator?" &middot; <code>"+esc(c.locator)+"</code>":"")+"</div>"));});
})();
</script>
</body>
</html>"##;

const PAGE_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__TITLE__ — __ALIAS__</title>
<style>
__THEME__
__STYLE__
__STORY_CSS__
</style>
</head>
<body>
<div class="topbar"><div class="wrap">__BRAND_TOPBAR__<span class="crumb"><a class="backlink" href="__BACK__">&larr; Story</a><span class="sep">&rsaquo;</span><b>__TITLE__</b></span><span class="sp"></span><span class="pilltag">__ALIAS__</span></div></div>
<header class="hero"><div class="wrap"><div><div class="eyebrow">Feature in __ALIAS__</div><span class="badge __LAYER__">__LAYER__</span><h1>__TITLE__</h1><div class="subtitle" id="subtitle"></div></div></div></header>
<main class="wrap wide">
<div id="lcbanner"></div>
<section class="story"><h2>Walkthrough</h2><div id="walk"></div></section>
<section class="story"><h2>How it connects across repos</h2><div id="links" class="cards"></div></section>
<section class="story"><h2>Code &amp; files</h2><div id="code"></div></section>
<section class="story" id="rel-sec" hidden><h2>Related generated pages</h2><div id="related" class="cards"></div></section>
<div class="honesty" id="honesty"></div>
</main>
<footer><div class="wrap">__BRAND_FOOTER__<span class="sp"></span><span class="meta">generated by Chaos Substrate</span></div></footer>
<script type="application/json" id="chaos-feature-story-manifest">__MANIFEST__</script>
<script>
(function(){
var M=JSON.parse(document.getElementById("chaos-feature-story-manifest").textContent);
function esc(v){return String(v==null?"":v).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;").replace(/'/g,"&#039;");}
function el(t,c,h){var e=document.createElement(t);if(c)e.className=c;if(h!=null)e.innerHTML=h;return e;}
var f=M.feature||{};
document.getElementById("subtitle").textContent=(f.member_count||0)+" code node(s) · "+((f.languages||[]).map(function(l){return l.language;}).join(", ")||"language unknown");
var lb=document.getElementById("lcbanner");
var lcd=(f.lifecycle_evidence&&f.lifecycle_evidence.detail)?f.lifecycle_evidence.detail:"";
if(f.status==="legacy"){var sb=f.superseded_by_page?(' — superseded by <a href="'+esc(f.superseded_by_page)+'">'+esc(f.superseded_by_alias)+":"+esc(f.superseded_by_label)+"</a>"):"";lb.innerHTML='<div class="legacy-banner"><strong>Legacy / superseded.</strong> This feature is replaced'+sb+"."+(lcd?" "+esc(lcd):"")+"</div>";}
else if(f.status==="variant"){lb.innerHTML='<div class="legacy-banner variant"><strong>Variant.</strong> Near-duplicate of a sibling feature'+(lcd?" — "+esc(lcd):"")+".</div>";}
var walk=document.getElementById("walk");
(M.walkthrough||[]).forEach(function(b){var c=el("div","block-card");c.appendChild(el("h3",null,esc(b.title)));c.appendChild(el("p",null,esc(b.body)));walk.appendChild(c);});
var links=document.getElementById("links");
if((M.links||[]).length){(M.links||[]).forEach(function(l){
  var c=el("div","card");
  var name=l.page?('<a href="'+esc(l.page)+'">'+esc(l.label)+"</a>"):esc(l.label);
  c.appendChild(el("h3",null,name));
  var tags='<span class="chip">'+esc(l.direction)+'</span><span class="chip">'+esc(l.alias)+'</span>'+(l.smart_contract?'<span class="chip sc">smart contract</span>':"");
  c.appendChild(el("p",null,tags));
  c.appendChild(el("p",null,esc(l.kind)+((l.matched||[]).length?(" &middot; "+(l.matched||[]).map(esc).join(", ")):"")));
  links.appendChild(c);
});}else links.appendChild(el("p","sec-sub","No cross-repo links touch this feature."));
var code=document.getElementById("code");
var t=el("table");t.innerHTML="<tr><th>Symbol</th><th>Kind</th><th>File</th></tr>"+(((M.code||{}).top_symbols)||[]).map(function(s){return "<tr><td class=mono>"+esc(s.name)+"</td><td>"+esc(s.kind)+"</td><td class=mono>"+esc(s.file)+"</td></tr>";}).join("");
code.appendChild(t);
code.appendChild(el("p","mono",(((M.code||{}).key_files)||[]).map(esc).join("<br>")));
var related=(M.related_pages||[]);
if(related.length){document.getElementById("rel-sec").hidden=false;var grid=document.getElementById("related");related.forEach(function(p){var c=el("div","card");c.appendChild(el("h3",null,esc(p.title||p.feature_id)));c.appendChild(el("p",null,esc(p.page)+" — "+((p.shared_files||[]).length)+" shared file(s)"));grid.appendChild(c);});}
document.getElementById("honesty").textContent=M.honesty||"";
})();
</script>
</body>
</html>"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_tokens_drops_short_and_dedups() {
        let t = lexical_tokens("Lab Tokenization & OCL access");
        assert!(t.contains(&"tokenization".to_string()));
        assert!(t.contains(&"access".to_string()));
        assert!(t.contains(&"ocl".to_string()));
        assert!(!t.iter().any(|x| x.len() < 3)); // "&" dropped, no 1-2 char tokens
    }

    #[test]
    fn within_site_strips_dir() {
        assert_eq!(
            within_site("lab-story/ipnft-ipnft.html"),
            "ipnft-ipnft.html"
        );
        assert_eq!(within_site("bare.html"), "bare.html");
    }

    #[test]
    fn matched_symbols_reads_evidence() {
        let ev = json!({"matched": ["IPNFT", "Tokenizer"], "other": 1});
        assert_eq!(matched_symbols(&ev), vec!["IPNFT", "Tokenizer"]);
        assert!(matched_symbols(&json!({})).is_empty());
    }

    #[test]
    fn existing_content_hash_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x-story.html");
        let html = format!(
            "<html><script type=\"application/json\" id=\"{MANIFEST_ID}\">{{\"content_hash\":\"abc123\"}}</script></html>"
        );
        std::fs::write(&path, html).unwrap();
        assert_eq!(
            existing_content_hash(&path, MANIFEST_ID).as_deref(),
            Some("abc123")
        );
        assert_eq!(
            existing_content_hash(&dir.path().join("missing.html"), MANIFEST_ID),
            None
        );
    }

    #[test]
    fn safe_slug_is_kebab_and_bounded() {
        assert_eq!(
            safe_slug("Lab Tokenization & Access!", "feature"),
            "lab-tokenization-access"
        );
        assert_eq!(safe_slug("   ", "feature"), "feature");
    }

    fn node(alias: &str, layer: &str, members: i32) -> StoryNode {
        StoryNode {
            id: Uuid::new_v4(),
            repo_id: Uuid::new_v4(),
            alias: alias.into(),
            label: format!("{alias}-feat"),
            layer: layer.into(),
            role: "matched".into(),
            score: 0.6,
            member_count: members,
            summary: None,
            languages: Vec::new(),
            top_symbols: Vec::new(),
            key_files: Vec::new(),
            folders: Vec::new(),
            matched_by: Vec::new(),
            page: format!("d/{alias}.html"),
            status: "active".into(),
            superseded_by: None,
            superseded_by_label: None,
            superseded_by_alias: None,
            superseded_by_page: None,
            variant_of: Vec::new(),
            lifecycle_evidence: None,
        }
    }

    fn tok(words: &[&str]) -> HashSet<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn markers_separate_legacy_from_current() {
        let ipnft = tok(&["ipnft", "tokenizer"]);
        let ocl = tok(&["ocl", "ocltokenizer"]);
        // The decisive sentences from the migration spike.
        let legacy_sent = "on mainnet there is no ipnft. the legacy ipnft stack is replaced.";
        let current_sent = "ocl replaces that ownership/identity model.";
        assert!(legacy_markers(legacy_sent, &ipnft));
        assert!(!current_markers(legacy_sent, &ipnft));
        assert!(current_markers(current_sent, &ocl));
        assert!(!legacy_markers(current_sent, &ocl));
        // An unrelated feature's tokens must not match.
        let other = tok(&["accessresolver"]);
        assert!(!legacy_markers(legacy_sent, &other));
        assert!(!current_markers(current_sent, &other));
    }

    #[test]
    fn has_cue_detects_supersession_language() {
        assert!(has_cue("ocl replaces ipnft"));
        assert!(has_cue("the legacy stack"));
        assert!(has_cue("this is deprecated"));
        assert!(!has_cue("two systems coexist happily"));
    }

    #[test]
    fn node_tokens_keeps_symbols_drops_generics() {
        let mut n = node("ipnft", "foundation", 3);
        n.label = "Lab Tokenizer".into();
        n.top_symbols = ["tokenizeIpnft", "controllerOf", "token"]
            .iter()
            .map(|s| FeatureSymbol {
                name: s.to_string(),
                kind: "function".into(),
                file: "x.sol".into(),
            })
            .collect();
        let (distinctive, symbols) = node_tokens(&n);
        assert!(distinctive.contains("tokenizeipnft"));
        assert!(distinctive.contains("controllerof"));
        assert!(distinctive.contains("ipnft")); // alias
        assert!(!distinctive.contains("token")); // generic dropped
        assert!(symbols.contains("controllerof"));
        assert!(!symbols.contains("token"));
    }

    #[test]
    fn spine_orders_entry_before_foundation() {
        // Mirror run()'s sort: layer rank, then member_count desc.
        let mut spine: Vec<(Layer, StoryNode)> = vec![
            (Layer::Foundation, node("ipnft", "foundation", 50)),
            (Layer::Entry, node("ecosystem", "entry", 20)),
            (Layer::Core, node("infra", "core", 30)),
        ];
        spine.sort_by(|a, b| {
            a.0.rank()
                .cmp(&b.0.rank())
                .then_with(|| b.1.member_count.cmp(&a.1.member_count))
        });
        let order: Vec<&str> = spine.iter().map(|(_, n)| n.alias.as_str()).collect();
        assert_eq!(order, vec!["ecosystem", "infra", "ipnft"]);
    }

    #[test]
    fn walkthrough_flags_smart_contract_links() {
        let n = node("ecosystem", "entry", 10);
        let links = vec![json!({
            "direction": "uses", "label": "IPNFT", "alias": "ipnft",
            "kind": "abi", "matched": ["IPNFT"], "smart_contract": true,
        })];
        let blocks = walkthrough_blocks(&n, &links);
        let connects = blocks
            .iter()
            .find(|b| b["title"] == json!("How it connects across repos"))
            .unwrap();
        assert!(connects["body"]
            .as_str()
            .unwrap()
            .contains("smart-contract"));
    }
}
