//! Use-case: full index / hash-gated reindex of one repository.
//!
//! Pipeline: list files (Vcs) → read → extract (SourceExtractor) →
//! `analyze` (project-wide resolution) → `build_chunks` → persist
//! (IndexStore::replace_repo, embeddings preserved by content hash) → embed
//! only the chunks the store reports missing → rebuild the hierarchy
//! (Louvain communities → summaries, embedded only when their text changed)
//! → stamp the repo with the HEAD read at the start and the paths that were
//! uncommitted then (`commit_sha` + `worktree_paths`), only once it succeeded.
//! `reindex` re-reads the git delta from that stamp (see `delta`).
//!
//! Everything here depends on `domain` + `ports` only.

use crate::application::delta::{self, DeltaSummary, Stored};
use crate::application::{analyze, chunk};
use crate::domain::community::{detect, to_communities};
use crate::domain::model::{listing_rules, Community, IndexBundle, Language, Node, NodeKind, Repo};
use crate::domain::provenance::{source, Breadcrumb};
use crate::domain::solidity::SolidityLayout;
use crate::domain::{hash, summary};
use crate::ports::community_store::CommunityStore;
use crate::ports::embedder::Embedder;
use crate::ports::extractor::{FileExtraction, FileInput, SourceExtractor};
use crate::ports::index_store::{Graph, IndexStore};
use crate::ports::vcs::{Change, Vcs};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Files larger than this are skipped with a warning (generated bundles, dumps).
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Texts per `embed_batch` call — adapter-agnostic upper bound.
pub const EMBED_BATCH: usize = 16;
/// Warnings kept in a report (the rest are counted, not listed).
pub const MAX_WARNINGS: usize = 20;
/// Neighbour labels fed to a summary.
const MAX_SUMMARY_NEIGHBOURS: usize = 6;
/// Member docs fed to a summary.
const MAX_SUMMARY_DOCS: usize = 6;

/// All the ports an index run needs.
#[derive(Clone, Copy)]
pub struct Deps<'a> {
    pub store: &'a dyn IndexStore,
    pub communities: &'a dyn CommunityStore,
    pub embedder: &'a dyn Embedder,
    pub extractor: &'a dyn SourceExtractor,
    pub vcs: &'a dyn Vcs,
}

#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub skip_dirs: Vec<String>,
    pub split_cos: f32,
    pub dry_run: bool,
    pub force: bool,
    /// Foundry projects of the repository (remappings, libs): Solidity import
    /// resolution reads it.
    pub solidity: SolidityLayout,
    /// Repo-relative directories skipped by path (component prefix), e.g. the
    /// Foundry `libs` of every project; unlike `skip_dirs`, never by basename.
    pub skip_paths: Vec<String>,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            skip_dirs: Vec::new(),
            split_cos: crate::domain::chunking::DEFAULT_SPLIT_COS,
            dry_run: false,
            force: false,
            solidity: SolidityLayout::default(),
            skip_paths: Vec::new(),
        }
    }
}

impl IndexOptions {
    /// The listing rules this run stamps: `skip_dirs`, `skip_paths` and the
    /// indexable languages ([`listing_rules`]).
    pub fn listing_rules(&self) -> String {
        listing_rules(&self.skip_dirs, &self.skip_paths)
    }
}

/// `listed` minus every path under one of `skip_paths` (whole components:
/// `lib` drops `lib/dep/A.sol`, never `src/lib/A.sol`).
pub fn drop_skipped_paths(listed: &mut Vec<PathBuf>, skip_paths: &[String]) {
    listed.retain(|p| !skip_paths.iter().any(|s| p.starts_with(s)));
}

/// The VCS as `index`, `add` and `reindex` see it: every listing, the full one
/// (`list_files`) and the one restricted to the delta (`list_paths`), drops
/// `skip_paths`. The full index and the git delta therefore agree on vendored
/// code, including a Foundry `lib/` submodule git reports as one directory.
pub(crate) struct SkipPathsVcs<'a> {
    inner: &'a dyn Vcs,
    skip_paths: &'a [String],
}

impl<'a> SkipPathsVcs<'a> {
    pub(crate) fn new(inner: &'a dyn Vcs, skip_paths: &'a [String]) -> Self {
        Self { inner, skip_paths }
    }

    /// `deps` with this listing in place of its VCS.
    pub(crate) fn deps(&'a self, deps: Deps<'a>) -> Deps<'a> {
        Deps { vcs: self, ..deps }
    }
}

impl Vcs for SkipPathsVcs<'_> {
    fn head_sha(&self, root: &Path) -> Result<Option<String>> {
        self.inner.head_sha(root)
    }
    fn is_clean(&self, root: &Path) -> Result<bool> {
        self.inner.is_clean(root)
    }
    fn worktree_changes(&self, root: &Path) -> Result<Vec<Change>> {
        self.inner.worktree_changes(root)
    }
    fn diff_commits(&self, root: &Path, base: &str, head: &str) -> Result<Vec<Change>> {
        self.inner.diff_commits(root, base, head)
    }
    fn resolve_commit(&self, root: &Path, rev: &str) -> Result<Option<String>> {
        self.inner.resolve_commit(root, rev)
    }
    fn list_files(&self, root: &Path, skip_dirs: &[String]) -> Result<Vec<PathBuf>> {
        let mut listed = self.inner.list_files(root, skip_dirs)?;
        drop_skipped_paths(&mut listed, self.skip_paths);
        Ok(listed)
    }
    fn list_paths(
        &self,
        root: &Path,
        paths: &[String],
        skip_dirs: &[String],
    ) -> Result<Vec<String>> {
        let mut listed = self.inner.list_paths(root, paths, skip_dirs)?;
        listed.retain(|p| !self.skip_paths.iter().any(|s| Path::new(p).starts_with(s)));
        Ok(listed)
    }
}

/// Compact summary of an index run (goes straight into an agent context).
#[derive(Debug, Clone, Default, Serialize)]
pub struct IndexReport {
    pub repo: String,
    pub files: usize,
    pub nodes: usize,
    pub edges: usize,
    pub chunks: usize,
    pub chunks_embedded: usize,
    pub chunks_reused: usize,
    pub communities: usize,
    pub summaries_recomputed: usize,
    /// Resolved call edges / call candidates seen by extraction (0..=1).
    pub call_resolution: f32,
    pub warnings: Vec<String>,
    pub provenance: Vec<Breadcrumb>,
    pub skipped: bool,
    pub dry_run: bool,
    /// `reindex` only: the git delta it re-read, or why it ran in full.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<DeltaSummary>,
}

/// Result of [`rebuild_hierarchy`] — shared with `add`.
#[derive(Debug, Clone, Default)]
pub struct HierarchyReport {
    pub communities: Vec<Community>,
    pub summaries_recomputed: usize,
    pub provenance: Vec<Breadcrumb>,
}

/// Canonical root path string + deterministic repo id + display name.
pub struct RepoIdentity {
    pub root: PathBuf,
    pub root_path: String,
    pub id: Uuid,
    pub name: String,
}

pub fn identify(root: &Path) -> Result<RepoIdentity> {
    let root = root
        .canonicalize()
        .with_context(|| format!("repository root {} does not exist", root.display()))?;
    let root_path = root.to_string_lossy().replace('\\', "/");
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "repo".to_string());
    Ok(RepoIdentity {
        id: hash::repo_id(&root_path),
        root,
        root_path,
        name,
    })
}

/// Push a warning, keeping at most [`MAX_WARNINGS`] listed.
pub fn push_warning(warnings: &mut Vec<String>, omitted: &mut usize, msg: String) {
    if warnings.len() < MAX_WARNINGS {
        warnings.push(msg);
    } else {
        *omitted += 1;
    }
}

pub fn finish_warnings(mut warnings: Vec<String>, omitted: usize) -> Vec<String> {
    if omitted > 0 {
        if warnings.len() >= MAX_WARNINGS {
            warnings.pop();
        }
        warnings.push(format!("… {omitted} more warnings omitted"));
    }
    warnings
}

/// Read one repo-relative file into a [`FileInput`]. `None` (with a warning)
/// for oversize, non-UTF-8 or non-indexable files.
pub fn read_input(
    root: &Path,
    rel: &str,
    warnings: &mut Vec<String>,
    omitted: &mut usize,
) -> Option<FileInput> {
    let language = Language::from_path(rel)?;
    let abs = root.join(rel);
    let meta = match std::fs::metadata(&abs) {
        Ok(m) => m,
        Err(e) => {
            push_warning(warnings, omitted, format!("{rel}: cannot stat ({e})"));
            return None;
        }
    };
    if meta.len() > MAX_FILE_BYTES {
        push_warning(
            warnings,
            omitted,
            format!("{rel}: skipped, {} bytes > {MAX_FILE_BYTES}", meta.len()),
        );
        return None;
    }
    let bytes = match std::fs::read(&abs) {
        Ok(b) => b,
        Err(e) => {
            push_warning(warnings, omitted, format!("{rel}: cannot read ({e})"));
            return None;
        }
    };
    match String::from_utf8(bytes) {
        Ok(content) => Some(FileInput {
            path: rel.to_string(),
            language,
            content,
        }),
        Err(_) => {
            push_warning(
                warnings,
                omitted,
                format!("{rel}: skipped, not valid UTF-8"),
            );
            None
        }
    }
}

/// Extract + analyze + chunk a set of inputs. Returns the bundle (with chunks)
/// and the call-resolution ratio. `known` is the persisted graph the inputs
/// resolve against (incremental `add`); a full `index` passes `None` because
/// the inputs ARE the whole repository.
#[allow(clippy::too_many_arguments)]
pub async fn extract_and_analyze(
    deps: Deps<'_>,
    repo_id: Uuid,
    inputs: Vec<FileInput>,
    known: Option<&Graph>,
    solidity: &SolidityLayout,
    split_cos: f32,
    warnings: &mut Vec<String>,
    omitted: &mut usize,
    provenance: &mut Vec<Breadcrumb>,
) -> Result<(IndexBundle, f32)> {
    let extractions = deps.extractor.extract_all(&inputs);
    let mut call_candidates = 0usize;
    for ex in &extractions {
        call_candidates += ex.calls.len();
        for w in &ex.warnings {
            push_warning(warnings, omitted, format!("{}: {w}", ex.path));
        }
    }
    let symbols: usize = extractions.iter().map(|e| e.symbols.len()).sum();
    provenance.push(Breadcrumb::new(
        source::TREE_SITTER,
        "extract",
        format!(
            "{} files → {symbols} symbols, {call_candidates} call candidates",
            extractions.len()
        ),
    ));
    let pairs: Vec<(FileInput, FileExtraction)> = inputs.into_iter().zip(extractions).collect();
    let analyzed = analyze::analyze_with(repo_id, &pairs, known, solidity);
    let contents: Vec<(String, String)> = pairs
        .iter()
        .map(|(i, _)| (i.path.clone(), i.content.clone()))
        .collect();
    let (chunks, _chunk_stats) = chunk::build_chunks(
        repo_id,
        &analyzed,
        &contents,
        Some(deps.embedder),
        split_cos,
    )
    .await
    .context("chunking failed")?;
    let stats = analyzed.stats;
    let mut bundle = analyzed.bundle;
    let resolved_calls = stats.calls_resolved;
    let call_candidates = stats.calls_total.max(call_candidates);
    let call_resolution = if call_candidates == 0 {
        1.0
    } else {
        (resolved_calls as f32 / call_candidates as f32).min(1.0)
    };
    provenance.push(Breadcrumb::new(
        source::GRAPH,
        "analyze",
        format!(
            "{} nodes, {} edges ({resolved_calls}/{call_candidates} calls resolved), {} chunks",
            bundle.nodes.len(),
            bundle.edges.len(),
            chunks.len()
        ),
    ));
    bundle.chunks = chunks;
    Ok((bundle, call_resolution))
}

/// Embed every chunk the store reports as missing for the embedder's model, in
/// batches of ≤ [`EMBED_BATCH`]. Returns the number embedded.
pub async fn embed_missing(
    deps: Deps<'_>,
    repo_id: Uuid,
    provenance: &mut Vec<Breadcrumb>,
) -> Result<usize> {
    let model = deps.embedder.model_ref();
    let missing = deps
        .store
        .chunks_missing_embeddings(repo_id, &model)
        .await?;
    let total = missing.len();
    tracing::info!(repo = %repo_id, missing = total, "embedding chunks");
    let mut done = 0usize;
    for batch in missing.chunks(EMBED_BATCH) {
        let texts: Vec<String> = batch.iter().map(|c| c.text()).collect();
        let vectors = deps
            .embedder
            .embed_batch(&texts)
            .await
            .context("embedding failed")?;
        anyhow::ensure!(
            vectors.len() == batch.len(),
            "embedder returned {} vectors for {} texts",
            vectors.len(),
            batch.len()
        );
        let rows: Vec<(Uuid, String, Vec<f32>)> = batch
            .iter()
            .zip(vectors)
            .map(|(c, v)| (c.id, c.content_hash.clone(), v))
            .collect();
        deps.store.save_embeddings(&model, &rows).await?;
        done += rows.len();
        tracing::info!(done, total, "embedded batch");
    }
    provenance.push(Breadcrumb::new(
        source::EMBEDDING,
        "embed_batch",
        format!(
            "{done} chunks embedded with {}/{} ({} dims)",
            model.provider, model.model_id, model.dimensions
        ),
    ));
    Ok(done)
}

/// Rebuild L1 (communities) + L2 (summaries) from the persisted graph. Every
/// summary is composed and keyed by `summary::summary_hash(text, model_id)`;
/// the store restores an embedding saved under the same key, so only a
/// summary whose text changed is embedded again.
pub async fn rebuild_hierarchy(deps: Deps<'_>, repo_id: Uuid) -> Result<HierarchyReport> {
    let mut provenance = Vec::new();
    let graph = deps.store.load_graph(repo_id).await?;
    provenance.push(Breadcrumb::new(
        source::POSTGRES,
        "load_graph",
        format!(
            "{} files, {} nodes, {} edges",
            graph.files.len(),
            graph.nodes.len(),
            graph.edges.len()
        ),
    ));

    // L1: communities, each with its composed L2 summary text and key.
    let det = detect(repo_id, &graph.nodes, &graph.edges);
    let (mut communities, members) = to_communities(&det, repo_id);
    let model = deps.embedder.model_ref();
    let texts: Vec<String> = {
        let ctx = SummaryContext::new(&graph.nodes, &graph.edges, &communities, &members);
        communities
            .iter()
            .map(|c| summary::compose(&ctx.input_for(c)))
            .collect()
    };
    for (c, text) in communities.iter_mut().zip(texts) {
        c.summary_hash = Some(summary::summary_hash(&text, &model.model_id));
        c.summary = Some(text);
    }
    deps.communities
        .replace_communities(repo_id, &communities, &members)
        .await?;
    provenance.push(Breadcrumb::new(
        source::GRAPH,
        "louvain",
        format!(
            "{} communities (modularity {:.3}, {} levels, {} fragments merged)",
            communities.len(),
            det.modularity,
            det.levels,
            det.merged_fragments
        ),
    ));
    tracing::info!(repo = %repo_id, communities = communities.len(), "hierarchy rebuilt");

    // L2: embed only the summaries the store has no vector for.
    let missing = deps
        .communities
        .communities_needing_summary(repo_id, &model)
        .await?;
    let mut recomputed = 0usize;
    for c in &missing {
        let Some(text) = c.summary.as_deref() else {
            continue;
        };
        let vector = deps
            .embedder
            .embed(text)
            .await
            .with_context(|| format!("summary embedding for {}", c.label))?;
        deps.communities
            .save_summary_embedding(c.id, &model, &vector)
            .await?;
        recomputed += 1;
    }
    provenance.push(Breadcrumb::new(
        source::EMBEDDING,
        "summary_hash",
        format!(
            "{recomputed} summaries embedded, {} reused (same text)",
            communities.len().saturating_sub(recomputed)
        ),
    ));
    tracing::info!(recomputed, communities = communities.len(), "summaries");
    Ok(HierarchyReport {
        communities,
        summaries_recomputed: recomputed,
        provenance,
    })
}

/// Everything needed to compose a community summary from the loaded graph.
struct SummaryContext<'a> {
    nodes: HashMap<Uuid, &'a Node>,
    membership: HashMap<Uuid, Uuid>,
    members_of: HashMap<Uuid, Vec<Uuid>>,
    labels: HashMap<Uuid, &'a str>,
    /// community → (neighbour community → weight)
    neighbours: HashMap<Uuid, BTreeMap<Uuid, f32>>,
}

impl<'a> SummaryContext<'a> {
    fn new(
        nodes: &'a [Node],
        edges: &'a [crate::domain::model::Edge],
        communities: &'a [Community],
        members: &[(Uuid, Uuid)],
    ) -> Self {
        let node_map: HashMap<Uuid, &Node> = nodes.iter().map(|n| (n.id, n)).collect();
        let mut membership = HashMap::new();
        let mut members_of: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        for (c, n) in members {
            membership.insert(*n, *c);
            members_of.entry(*c).or_default().push(*n);
        }
        let labels = communities
            .iter()
            .map(|c| (c.id, c.label.as_str()))
            .collect();
        let mut neighbours: HashMap<Uuid, BTreeMap<Uuid, f32>> = HashMap::new();
        for e in edges {
            let (Some(a), Some(b)) = (
                membership.get(&e.source_node_id),
                membership.get(&e.target_node_id),
            ) else {
                continue;
            };
            if a == b {
                continue;
            }
            *neighbours.entry(*a).or_default().entry(*b).or_insert(0.0) += e.confidence;
            *neighbours.entry(*b).or_default().entry(*a).or_insert(0.0) += e.confidence;
        }
        Self {
            nodes: node_map,
            membership,
            members_of,
            labels,
            neighbours,
        }
    }

    fn input_for(&self, c: &Community) -> summary::SummaryInput {
        let member_nodes: Vec<&Node> = self
            .members_of
            .get(&c.id)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.nodes.get(id).copied())
                    .collect()
            })
            .unwrap_or_default();
        let by_name: HashMap<&str, &Node> =
            member_nodes.iter().map(|n| (n.name.as_str(), *n)).collect();
        let top_members: Vec<(String, String)> = c
            .top_members
            .iter()
            .map(|name| {
                let kind = by_name
                    .get(name.as_str())
                    .map(|n| n.kind.as_str())
                    .unwrap_or("");
                (name.clone(), kind.to_string())
            })
            .collect();
        // Docs: top members first (in order), then any other documented member.
        let mut docs = Vec::new();
        let mut seen: HashSet<Uuid> = HashSet::new();
        for name in &c.top_members {
            if let Some(n) = by_name.get(name.as_str()) {
                if seen.insert(n.id) {
                    if let Some(d) = n.doc.as_deref().filter(|d| !d.trim().is_empty()) {
                        docs.push(d.to_string());
                    }
                }
            }
        }
        for n in &member_nodes {
            if docs.len() >= MAX_SUMMARY_DOCS {
                break;
            }
            if n.kind == NodeKind::File || !seen.insert(n.id) {
                continue;
            }
            if let Some(d) = n.doc.as_deref().filter(|d| !d.trim().is_empty()) {
                docs.push(d.to_string());
            }
        }
        docs.truncate(MAX_SUMMARY_DOCS);
        let mut neigh: Vec<(Uuid, f32)> = self
            .neighbours
            .get(&c.id)
            .map(|m| m.iter().map(|(k, v)| (*k, *v)).collect())
            .unwrap_or_default();
        neigh.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        let neighbours: Vec<String> = neigh
            .iter()
            .filter_map(|(id, _)| self.labels.get(id).map(|l| l.to_string()))
            .take(MAX_SUMMARY_NEIGHBOURS)
            .collect();
        let _ = &self.membership;
        summary::SummaryInput {
            label: c.label.clone(),
            files: c.files.clone(),
            top_members,
            docs,
            neighbours,
        }
    }
}

/// Full index of `root`.
pub async fn index(deps: Deps<'_>, root: &Path, opts: IndexOptions) -> Result<IndexReport> {
    let skip_paths = opts.skip_paths.clone();
    let vcs = SkipPathsVcs::new(deps.vcs, &skip_paths);
    let deps = vcs.deps(deps);
    let ident = identify(root)?;
    let mut warnings = Vec::new();
    let mut omitted = 0usize;
    let mut provenance = Vec::new();
    tracing::info!(repo = %ident.root_path, dry_run = opts.dry_run, "index start");

    // The stamp is taken before anything is read: a commit or an edit made
    // during the run shows up in the next delta instead of being lost.
    let head_sha = deps.vcs.head_sha(&ident.root).unwrap_or(None);
    let uncommitted = match head_sha {
        Some(_) => deps.vcs.worktree_changes(&ident.root).unwrap_or_default(),
        None => Vec::new(),
    };
    let listed = deps.vcs.list_files(&ident.root, &opts.skip_dirs)?;
    provenance.push(
        Breadcrumb::new(
            source::GIT,
            "list_files",
            format!(
                "{} indexable files (HEAD {})",
                listed.len(),
                head_sha.as_deref().unwrap_or("none")
            ),
        )
        .with_locator(ident.root_path.clone()),
    );
    let mut inputs = Vec::with_capacity(listed.len());
    for p in &listed {
        let rel = p.to_string_lossy().replace('\\', "/");
        if let Some(input) = read_input(&ident.root, &rel, &mut warnings, &mut omitted) {
            inputs.push(input);
        }
    }
    provenance.push(Breadcrumb::new(
        source::FILE,
        "read",
        format!("{} files read", inputs.len()),
    ));
    let files = inputs.len();

    let (bundle, call_resolution) = extract_and_analyze(
        deps,
        ident.id,
        inputs,
        None,
        &opts.solidity,
        opts.split_cos,
        &mut warnings,
        &mut omitted,
        &mut provenance,
    )
    .await?;

    let mut report = IndexReport {
        repo: ident.root_path.clone(),
        files,
        nodes: bundle.nodes.len(),
        edges: bundle.edges.len(),
        chunks: bundle.chunks.len(),
        call_resolution,
        dry_run: opts.dry_run,
        ..Default::default()
    };
    if opts.dry_run {
        report.warnings = finish_warnings(warnings, omitted);
        report.provenance = provenance;
        return Ok(report);
    }

    let indexed: Stored = bundle
        .files
        .iter()
        .map(|f| (f.path.clone(), f.content_hash.clone()))
        .collect();
    let worktree_paths = delta::stamp_paths(&uncommitted, &ident.root, &opts.skip_dirs, &indexed);
    let rules = opts.listing_rules();
    // Unstamped until the run succeeds: an interrupted index leaves no stamp,
    // so the next `add` reconciles every file and finishes embedding and the
    // hierarchy instead of trusting a delta.
    let repo = Repo {
        id: ident.id,
        name: ident.name.clone(),
        root_path: ident.root_path.clone(),
        commit_sha: None,
        worktree_paths: None,
        listing_rules: None,
    };
    deps.store.upsert_repo(&repo).await?;
    deps.store.replace_repo(ident.id, &bundle).await?;
    provenance.push(Breadcrumb::new(
        source::POSTGRES,
        "replace_repo",
        format!(
            "{} files, {} nodes, {} edges, {} chunks persisted",
            bundle.files.len(),
            bundle.nodes.len(),
            bundle.edges.len(),
            bundle.chunks.len()
        ),
    ));

    report.chunks_embedded = embed_missing(deps, ident.id, &mut provenance).await?;
    report.chunks_reused = report.chunks.saturating_sub(report.chunks_embedded);

    let hier = rebuild_hierarchy(deps, ident.id).await?;
    report.communities = hier.communities.len();
    report.summaries_recomputed = hier.summaries_recomputed;
    provenance.extend(hier.provenance);
    deps.store
        .set_repo_stamp(ident.id, head_sha.as_deref(), &worktree_paths, &rules)
        .await?;
    provenance.push(Breadcrumb::new(
        source::POSTGRES,
        "set_repo_stamp",
        format!(
            "commit {}, {} uncommitted paths",
            head_sha.as_deref().unwrap_or("none"),
            worktree_paths.len()
        ),
    ));

    report.warnings = finish_warnings(warnings, omitted);
    report.provenance = provenance;
    tracing::info!(
        files = report.files,
        chunks = report.chunks,
        embedded = report.chunks_embedded,
        communities = report.communities,
        "index done"
    );
    Ok(report)
}

/// Bring the index up to HEAD. A no-op when HEAD is the stamped commit, the
/// worktree is clean, nothing was uncommitted at the stamp and the listing
/// rules are the stamped ones. Otherwise the git delta from the stamp is
/// re-read like `add` does, plus the unchanged files one hop upstream of each
/// change (imports into a changed file, calls / extends / implements /
/// queries into its nodes), so their edges to new symbols resolve. A full
/// index runs with `force`, on a first run, or when the stamp cannot be used
/// (missing or incomplete, other listing rules, commit not in this clone; the
/// report says why).
pub async fn reindex(deps: Deps<'_>, root: &Path, opts: IndexOptions) -> Result<IndexReport> {
    let ident = identify(root)?;
    if opts.force || opts.dry_run {
        return index(deps, root, opts).await;
    }
    let Some(existing) = deps.store.find_repo(&ident.root_path).await? else {
        return index(deps, root, opts).await;
    };
    let head = deps.vcs.head_sha(&ident.root).unwrap_or(None);
    let clean = deps.vcs.is_clean(&ident.root).unwrap_or(false);
    let rules = opts.listing_rules();
    if head.is_some()
        && head == existing.commit_sha
        && clean
        && existing.worktree_paths.as_ref().is_some_and(Vec::is_empty)
        && existing.listing_rules.as_deref() == Some(rules.as_str())
    {
        tracing::info!(repo = %ident.root_path, "reindex skipped: HEAD is the stamp and nothing is uncommitted");
        return Ok(IndexReport {
            repo: ident.root_path.clone(),
            skipped: true,
            provenance: vec![Breadcrumb::new(
                source::GIT,
                "head_sha",
                format!(
                    "HEAD {} equals the stamped commit, the worktree is clean, nothing was uncommitted at the stamp and the indexing rules (skip_dirs, languages) are unchanged; nothing to do (after a Chaos upgrade that changes extraction, use force)",
                    head.unwrap_or_default()
                ),
            )],
            ..Default::default()
        });
    }
    let skip_paths = opts.skip_paths.clone();
    let vcs = SkipPathsVcs::new(deps.vcs, &skip_paths);
    match crate::application::add::reindex_delta(vcs.deps(deps), &ident, &existing, &opts).await? {
        Ok(report) => Ok(report),
        Err(why) => {
            tracing::warn!(repo = %ident.root_path, %why, "reindex: git delta unavailable, full index");
            let base = existing.commit_sha.clone();
            let mut report = index(deps, root, opts).await?;
            report.provenance.insert(
                0,
                Breadcrumb::new(
                    source::GIT,
                    "delta",
                    format!("fallback to a full index: {why}"),
                )
                .with_locator(ident.root_path.clone()),
            );
            report.delta = Some(DeltaSummary {
                base,
                head,
                fallback: Some(why),
                ..Default::default()
            });
            Ok(report)
        }
    }
}

#[cfg(test)]
pub(crate) mod fakes {
    //! Scripted in-memory fakes of the ports, shared by the `index` and `add`
    //! use-case tests. Not mocks of the embedder in runtime code: test-only.
    use super::*;
    use crate::domain::model::{Chunk, Edge, ModelRef, SourceFile};
    use crate::ports::index_store::Graph;
    use crate::ports::vcs::Change;
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct FakeStore {
        pub writes: Mutex<Vec<String>>,
        pub repo: Mutex<Option<Repo>>,
        pub graph: Mutex<Graph>,
        pub missing: Mutex<Vec<Chunk>>,
        pub saved_embeddings: Mutex<Vec<(Uuid, String)>>,
        pub purged: Mutex<Vec<String>>,
        /// Paths of the files every `merge_files` bundle carried (re-read files).
        pub merged: Mutex<Vec<String>>,
        /// Every `set_repo_stamp` payload (commit, uncommitted paths), in order.
        pub stamps: Mutex<Vec<(Option<String>, Vec<String>)>>,
        /// Every repository `upsert_repo` received, in order.
        pub upserts: Mutex<Vec<Repo>>,
        /// `replace_repo` fails.
        pub fail_replace: bool,
    }

    impl FakeStore {
        pub fn log(&self, s: impl Into<String>) {
            self.writes.lock().unwrap().push(s.into());
        }
    }

    #[async_trait]
    impl IndexStore for FakeStore {
        async fn upsert_repo(&self, repo: &Repo) -> Result<()> {
            self.log("upsert_repo");
            self.upserts.lock().unwrap().push(repo.clone());
            *self.repo.lock().unwrap() = Some(repo.clone());
            Ok(())
        }
        async fn find_repo(&self, root_path: &str) -> Result<Option<Repo>> {
            Ok(self
                .repo
                .lock()
                .unwrap()
                .clone()
                .filter(|r| r.root_path == root_path))
        }
        async fn list_repos(&self) -> Result<Vec<Repo>> {
            Ok(self.repo.lock().unwrap().clone().into_iter().collect())
        }
        async fn set_repo_stamp(
            &self,
            _repo_id: Uuid,
            commit_sha: Option<&str>,
            worktree_paths: &[String],
            listing_rules: &str,
        ) -> Result<()> {
            self.log("set_repo_stamp");
            let stamp = (commit_sha.map(str::to_string), worktree_paths.to_vec());
            if let Some(r) = self.repo.lock().unwrap().as_mut() {
                r.commit_sha = stamp.0.clone();
                r.worktree_paths = Some(stamp.1.clone());
                r.listing_rules = Some(listing_rules.to_string());
            }
            self.stamps.lock().unwrap().push(stamp);
            Ok(())
        }
        async fn replace_repo(&self, _repo_id: Uuid, bundle: &IndexBundle) -> Result<()> {
            anyhow::ensure!(!self.fail_replace, "replace_repo: scripted failure");
            self.log(format!("replace_repo:{}", bundle.files.len()));
            Ok(())
        }
        async fn merge_files(
            &self,
            _repo_id: Uuid,
            purge_paths: &[String],
            bundle: &IndexBundle,
        ) -> Result<()> {
            self.log(format!(
                "merge_files:{}:{}",
                purge_paths.len(),
                bundle.files.len()
            ));
            self.purged
                .lock()
                .unwrap()
                .extend(purge_paths.iter().cloned());
            self.merged
                .lock()
                .unwrap()
                .extend(bundle.files.iter().map(|f| f.path.clone()));
            Ok(())
        }
        async fn load_graph(&self, _repo_id: Uuid) -> Result<Graph> {
            Ok(self.graph.lock().unwrap().clone())
        }
        async fn chunks_missing_embeddings(
            &self,
            _repo_id: Uuid,
            _model: &ModelRef,
        ) -> Result<Vec<Chunk>> {
            Ok(self.missing.lock().unwrap().clone())
        }
        async fn save_embeddings(
            &self,
            _model: &ModelRef,
            rows: &[(Uuid, String, Vec<f32>)],
        ) -> Result<()> {
            self.log(format!("save_embeddings:{}", rows.len()));
            self.saved_embeddings
                .lock()
                .unwrap()
                .extend(rows.iter().map(|(id, h, _)| (*id, h.clone())));
            Ok(())
        }
        async fn count_chunks(&self, _repo_id: Uuid) -> Result<i64> {
            Ok(0)
        }
    }

    /// Keeps the store contract: an embedding saved under a `summary_hash`
    /// survives `replace_communities`, and only rows whose hash has none are
    /// reported as needing a summary.
    #[derive(Default)]
    pub struct FakeCommunities {
        pub writes: Mutex<Vec<String>>,
        /// Last `replace_communities` payload.
        pub stored: Mutex<Vec<Community>>,
        /// `summary_hash`es that have an embedding.
        pub embedded: Mutex<HashSet<String>>,
        pub summaries_saved: Mutex<Vec<Uuid>>,
    }

    #[async_trait]
    impl CommunityStore for FakeCommunities {
        async fn replace_communities(
            &self,
            _repo_id: Uuid,
            communities: &[Community],
            _members: &[(Uuid, Uuid)],
        ) -> Result<()> {
            self.writes
                .lock()
                .unwrap()
                .push(format!("replace_communities:{}", communities.len()));
            *self.stored.lock().unwrap() = communities.to_vec();
            Ok(())
        }
        async fn load_communities(&self, _repo_id: Uuid) -> Result<Vec<Community>> {
            Ok(self.stored.lock().unwrap().clone())
        }
        async fn load_members(&self, _repo_id: Uuid) -> Result<Vec<(Uuid, Uuid)>> {
            Ok(Vec::new())
        }
        async fn communities_needing_summary(
            &self,
            _repo_id: Uuid,
            _model: &ModelRef,
        ) -> Result<Vec<Community>> {
            let embedded = self.embedded.lock().unwrap();
            Ok(self
                .stored
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.summary.is_some())
                .filter(|c| {
                    !c.summary_hash
                        .as_ref()
                        .is_some_and(|h| embedded.contains(h))
                })
                .cloned()
                .collect())
        }
        async fn save_summary_embedding(
            &self,
            community_id: Uuid,
            _model: &ModelRef,
            _embedding: &[f32],
        ) -> Result<()> {
            self.writes
                .lock()
                .unwrap()
                .push("save_summary_embedding".into());
            let hash = self
                .stored
                .lock()
                .unwrap()
                .iter()
                .find(|c| c.id == community_id)
                .and_then(|c| c.summary_hash.clone());
            self.embedded.lock().unwrap().extend(hash);
            self.summaries_saved.lock().unwrap().push(community_id);
            Ok(())
        }
    }

    /// Scripted embedder: deterministic vectors derived from text length
    /// (test-only; never used at runtime).
    #[derive(Default)]
    pub struct FakeEmbedder {
        pub calls: Mutex<Vec<usize>>,
        /// Every call fails, like an unreachable embedding server.
        pub fail: bool,
    }

    #[async_trait]
    impl Embedder for FakeEmbedder {
        fn provider(&self) -> &'static str {
            "fake"
        }
        fn model_id(&self) -> &str {
            "fake-model"
        }
        fn dimensions(&self) -> usize {
            3
        }
        async fn embed(&self, input: &str) -> Result<Vec<f32>> {
            anyhow::ensure!(!self.fail, "embedding failed: scripted");
            self.calls.lock().unwrap().push(1);
            let l = input.len() as f32;
            Ok(vec![1.0, l, 1.0 / (l + 1.0)])
        }
        async fn embed_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
            anyhow::ensure!(!self.fail, "embedding failed: scripted");
            self.calls.lock().unwrap().push(inputs.len());
            Ok(inputs
                .iter()
                .map(|s| vec![1.0, s.len() as f32, 0.5])
                .collect())
        }
    }

    /// File-level extractor: one content hash, no symbols.
    #[derive(Default)]
    pub struct FakeExtractor;

    impl SourceExtractor for FakeExtractor {
        fn extract(&self, file: &FileInput) -> FileExtraction {
            FileExtraction {
                path: file.path.clone(),
                language: Some(file.language),
                content_hash: hash::sha256_hex(&file.content),
                line_count: file.content.lines().count() as u32,
                free_ranges: vec![(0, file.content.len())],
                ..Default::default()
            }
        }
    }

    /// Scripted git: every commit resolves to itself unless listed in
    /// `missing`; calls are recorded so tests can assert what ran.
    #[derive(Default)]
    pub struct FakeVcs {
        pub head: Option<String>,
        pub clean: bool,
        /// `list_files` of the repository root.
        pub files: Vec<PathBuf>,
        /// Paths (and directories) `list_paths` treats as ignored, like a
        /// `.gitignore` entry would.
        pub ignored: Vec<String>,
        /// `worktree_changes`.
        pub changes: Vec<Change>,
        /// `diff_commits(base, _)` by base.
        pub diffs: HashMap<String, Vec<Change>>,
        /// Commits this clone does not have.
        pub missing: Vec<String>,
        /// `diff_commits` and `worktree_changes` fail.
        pub broken: bool,
        pub calls: Mutex<Vec<String>>,
    }

    impl FakeVcs {
        fn call(&self, s: String) {
            self.calls.lock().unwrap().push(s);
        }
        pub fn called(&self, prefix: &str) -> bool {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with(prefix))
        }
    }

    impl Vcs for FakeVcs {
        fn head_sha(&self, _root: &Path) -> Result<Option<String>> {
            Ok(self.head.clone())
        }
        fn is_clean(&self, _root: &Path) -> Result<bool> {
            Ok(self.clean)
        }
        fn worktree_changes(&self, _root: &Path) -> Result<Vec<Change>> {
            self.call("worktree_changes".into());
            anyhow::ensure!(!self.broken, "git status: scripted failure");
            Ok(self.changes.clone())
        }
        fn diff_commits(&self, _root: &Path, base: &str, head: &str) -> Result<Vec<Change>> {
            self.call(format!("diff_commits:{base}..{head}"));
            anyhow::ensure!(!self.broken, "git diff: scripted failure");
            Ok(self.diffs.get(base).cloned().unwrap_or_default())
        }
        fn resolve_commit(&self, _root: &Path, rev: &str) -> Result<Option<String>> {
            Ok((!self.missing.iter().any(|m| m == rev)).then(|| rev.to_string()))
        }
        fn list_files(&self, root: &Path, _skip_dirs: &[String]) -> Result<Vec<PathBuf>> {
            self.call(format!("list_files:{}", root.display()));
            Ok(self.files.clone())
        }
        /// The disk below each scope: regular files only (no symlink, also
        /// none on the way), indexable, not in `ignored`.
        fn list_paths(
            &self,
            root: &Path,
            paths: &[String],
            skip_dirs: &[String],
        ) -> Result<Vec<String>> {
            self.call(format!("list_paths:{}", paths.join(",")));
            fn walk(root: &Path, rel: &str, out: &mut std::collections::BTreeSet<String>) {
                let abs = root.join(rel);
                let Ok(meta) = std::fs::symlink_metadata(&abs) else {
                    return;
                };
                if meta.is_file() {
                    out.insert(rel.to_string());
                } else if meta.is_dir() {
                    for e in std::fs::read_dir(&abs).into_iter().flatten().flatten() {
                        let name = e.file_name().to_string_lossy().into_owned();
                        let child = if rel.is_empty() {
                            name
                        } else {
                            format!("{rel}/{name}")
                        };
                        walk(root, &child, out);
                    }
                }
            }
            let through_symlink = |rel: &str| {
                let mut cur = rel;
                while let Some((parent, _)) = cur.rsplit_once('/') {
                    if std::fs::symlink_metadata(root.join(parent)).is_ok_and(|m| !m.is_dir()) {
                        return true;
                    }
                    cur = parent;
                }
                false
            };
            let mut out = std::collections::BTreeSet::new();
            for scope in paths {
                let scope = scope.trim_matches('/');
                if !through_symlink(scope) {
                    walk(root, scope, &mut out);
                }
            }
            let ignored = |p: &str| {
                self.ignored
                    .iter()
                    .any(|i| p == i || p.starts_with(&format!("{i}/")))
            };
            Ok(out
                .into_iter()
                .filter(|p| Language::indexable(p, skip_dirs).is_some() && !ignored(p))
                .collect())
        }
    }

    /// An indexed file holding `content`.
    pub fn stored_file(repo: Uuid, path: &str, content: &str) -> SourceFile {
        SourceFile {
            id: hash::scoped_id(repo, &format!("source_file:{path}")),
            repo_id: repo,
            path: path.to_string(),
            language: Language::from_path(path).unwrap_or(Language::Markdown),
            content_hash: hash::sha256_hex(content),
            line_count: content.lines().count() as i32,
        }
    }

    pub fn node(repo: Uuid, stable_id: &str, kind: NodeKind, doc: Option<&str>) -> Node {
        let name = stable_id.rsplit(':').next().unwrap().to_string();
        Node {
            id: hash::scoped_id(repo, stable_id),
            repo_id: repo,
            file_id: None,
            kind,
            stable_id: stable_id.to_string(),
            name,
            signature: None,
            doc: doc.map(str::to_string),
            line_start: Some(1),
            line_end: Some(2),
            metadata: serde_json::Value::Null,
        }
    }

    pub fn edge(repo: Uuid, a: Uuid, b: Uuid, kind: crate::domain::model::EdgeKind) -> Edge {
        Edge {
            id: hash::edge_id(a, b, kind.as_str()),
            repo_id: repo,
            source_node_id: a,
            target_node_id: b,
            kind,
            confidence: 1.0,
        }
    }

    pub fn chunk(repo: Uuid, key: &str) -> Chunk {
        Chunk {
            id: hash::scoped_id(repo, key),
            repo_id: repo,
            file_id: None,
            node_id: None,
            kind: crate::domain::model::ChunkKind::FileHead,
            header: format!("// file: {key}"),
            body: "x".into(),
            content_hash: hash::sha256_hex(key),
            line_start: None,
            line_end: None,
            metadata: serde_json::Value::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::*;
    use super::*;
    use crate::domain::model::{listing_rules, SourceFile};
    use crate::ports::vcs::{Change, ChangeStatus};

    fn deps<'a>(
        s: &'a FakeStore,
        c: &'a FakeCommunities,
        e: &'a FakeEmbedder,
        x: &'a FakeExtractor,
        v: &'a FakeVcs,
    ) -> Deps<'a> {
        Deps {
            store: s,
            communities: c,
            embedder: e,
            extractor: x,
            vcs: v,
        }
    }

    fn tmp_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn a() {}\npub fn b() { a() }\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("README.md"), "# Hi\n\nintro\n").unwrap();
        std::fs::write(dir.path().join("blob.bin"), [0xffu8, 0xfe, 0x00]).unwrap();
        dir
    }

    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let dir = tmp_repo();
        let store = FakeStore::default();
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = FakeVcs {
            head: Some("abc".into()),
            clean: true,
            files: vec!["src/lib.rs".into(), "README.md".into()],
            ..Default::default()
        };
        let opts = IndexOptions {
            dry_run: true,
            ..Default::default()
        };
        let report = index(deps(&store, &comms, &emb, &ext, &vcs), dir.path(), opts)
            .await
            .unwrap();
        assert!(report.dry_run);
        assert_eq!(report.files, 2);
        assert!(
            store.writes.lock().unwrap().is_empty(),
            "{:?}",
            store.writes.lock().unwrap()
        );
        assert!(comms.writes.lock().unwrap().is_empty());
        assert_eq!(report.chunks_embedded, 0);
        assert!(report.provenance.iter().any(|b| b.source == source::GIT));
    }

    /// onchainlabs foundry.toml `libs = ["dependencies"]`: the vendored
    /// OpenZeppelin copy is skipped by path; `script/lib/` stays (a basename
    /// match would drop it).
    #[tokio::test]
    async fn foundry_libs_are_skipped_by_path() {
        let dir = tempfile::tempdir().unwrap();
        for p in [
            "src/OnChainLab.sol",
            "script/lib/Create2DeployLib.sol",
            "dependencies/@openzeppelin-contracts-5.6.0/token/ERC20/ERC20.sol",
        ] {
            std::fs::create_dir_all(dir.path().join(p).parent().unwrap()).unwrap();
            std::fs::write(dir.path().join(p), "contract A {}\n").unwrap();
        }
        let store = FakeStore::default();
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = FakeVcs {
            files: vec![
                "src/OnChainLab.sol".into(),
                "script/lib/Create2DeployLib.sol".into(),
                "dependencies/@openzeppelin-contracts-5.6.0/token/ERC20/ERC20.sol".into(),
            ],
            ..Default::default()
        };
        let opts = IndexOptions {
            dry_run: true,
            skip_paths: vec!["dependencies".into()],
            ..Default::default()
        };
        let report = index(deps(&store, &comms, &emb, &ext, &vcs), dir.path(), opts)
            .await
            .unwrap();
        assert_eq!(report.files, 2);
        let mut listed: Vec<PathBuf> = vec![
            "lib/forge-std/src/Test.sol".into(),
            "src/lib/X.sol".into(),
            "libs/Y.sol".into(),
        ];
        drop_skipped_paths(&mut listed, &["lib".to_string()]);
        assert_eq!(
            listed,
            vec![PathBuf::from("src/lib/X.sol"), PathBuf::from("libs/Y.sol")]
        );
    }

    #[tokio::test]
    async fn non_utf8_and_oversize_files_are_warned_not_fatal() {
        let dir = tmp_repo();
        std::fs::write(dir.path().join("bad.rs"), [0xffu8, 0xfe]).unwrap();
        let store = FakeStore::default();
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = FakeVcs {
            files: vec!["bad.rs".into(), "src/lib.rs".into()],
            ..Default::default()
        };
        let opts = IndexOptions {
            dry_run: true,
            ..Default::default()
        };
        let report = index(deps(&store, &comms, &emb, &ext, &vcs), dir.path(), opts)
            .await
            .unwrap();
        assert_eq!(report.files, 1);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("bad.rs") && w.contains("UTF-8")),
            "{:?}",
            report.warnings
        );
    }

    #[tokio::test]
    async fn full_index_persists_embeds_missing_and_stamps_commit() {
        let dir = tmp_repo();
        let store = FakeStore::default();
        let repo_id = identify(dir.path()).unwrap().id;
        *store.missing.lock().unwrap() = vec![chunk(repo_id, "c1"), chunk(repo_id, "c2")];
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = FakeVcs {
            head: Some("abc".into()),
            clean: true,
            files: vec!["src/lib.rs".into()],
            ..Default::default()
        };
        let report = index(
            deps(&store, &comms, &emb, &ext, &vcs),
            dir.path(),
            IndexOptions::default(),
        )
        .await
        .unwrap();
        assert!(!report.skipped);
        assert_eq!(report.chunks_embedded, 2);
        let writes = store.writes.lock().unwrap().clone();
        assert!(
            writes.iter().any(|w| w.starts_with("replace_repo")),
            "{writes:?}"
        );
        assert!(
            writes.iter().any(|w| w == "save_embeddings:2"),
            "{writes:?}"
        );
        assert_eq!(writes.last().unwrap(), "set_repo_stamp");
        assert_eq!(
            store
                .repo
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .commit_sha
                .as_deref(),
            Some("abc")
        );
    }

    fn stamped(dir: &Path, store: &FakeStore, commit: &str, prev: &[&str]) -> Uuid {
        let ident = identify(dir).unwrap();
        *store.repo.lock().unwrap() = Some(Repo {
            id: ident.id,
            name: ident.name.clone(),
            root_path: ident.root_path.clone(),
            commit_sha: Some(commit.into()),
            worktree_paths: Some(prev.iter().map(|p| p.to_string()).collect()),
            // The rules of `IndexOptions::default()`.
            listing_rules: Some(listing_rules(&[], &[])),
        });
        ident.id
    }

    #[tokio::test]
    async fn index_stamps_head_and_uncommitted_paths() {
        let dir = tmp_repo();
        let store = FakeStore::default();
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = FakeVcs {
            head: Some("abc".into()),
            files: vec!["src/lib.rs".into(), "README.md".into()],
            changes: vec![
                Change {
                    path: "src/lib.rs".into(),
                    status: ChangeStatus::Modified,
                },
                Change {
                    path: "Cargo.lock".into(),
                    status: ChangeStatus::Modified,
                },
            ],
            ..Default::default()
        };
        index(
            deps(&store, &comms, &emb, &ext, &vcs),
            dir.path(),
            IndexOptions::default(),
        )
        .await
        .unwrap();
        // Unstamped while running, stamped once it succeeded.
        let writes = store.writes.lock().unwrap().clone();
        assert_eq!(writes[0], "upsert_repo");
        assert_eq!(writes.last().unwrap(), "set_repo_stamp");
        let upserted = store.upserts.lock().unwrap().clone();
        assert_eq!(upserted.len(), 1);
        assert_eq!(
            (
                &upserted[0].commit_sha,
                &upserted[0].worktree_paths,
                &upserted[0].listing_rules
            ),
            (&None, &None, &None),
            "the first write carries no stamp"
        );
        assert_eq!(
            store.stamps.lock().unwrap().as_slice(),
            &[(Some("abc".to_string()), vec!["src/lib.rs".to_string()])]
        );
        let repo = store.repo.lock().unwrap().clone().unwrap();
        assert_eq!(repo.listing_rules, Some(listing_rules(&[], &[])));
    }

    #[tokio::test]
    async fn a_failed_index_leaves_no_stamp() {
        let dir = tmp_repo();
        let comms = FakeCommunities::default();
        let ext = FakeExtractor;
        let vcs = FakeVcs {
            head: Some("abc".into()),
            files: vec!["src/lib.rs".into()],
            ..Default::default()
        };
        // replace_repo fails …
        let store = FakeStore {
            fail_replace: true,
            ..Default::default()
        };
        let emb = FakeEmbedder::default();
        let err = index(
            deps(&store, &comms, &emb, &ext, &vcs),
            dir.path(),
            IndexOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("replace_repo"), "{err}");
        assert!(store.stamps.lock().unwrap().is_empty());
        assert_eq!(
            store.repo.lock().unwrap().clone().unwrap().worktree_paths,
            None
        );
        // … or the embedder does, after the index was replaced.
        let store = FakeStore::default();
        *store.missing.lock().unwrap() = vec![chunk(Uuid::nil(), "x")];
        let emb = FakeEmbedder {
            fail: true,
            ..Default::default()
        };
        let err = index(
            deps(&store, &comms, &emb, &ext, &vcs),
            dir.path(),
            IndexOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("embedding failed"), "{err}");
        assert!(store
            .writes
            .lock()
            .unwrap()
            .iter()
            .any(|w| w.starts_with("replace_repo")));
        assert!(store.stamps.lock().unwrap().is_empty());
        let repo = store.repo.lock().unwrap().clone().unwrap();
        assert_eq!((repo.commit_sha, repo.worktree_paths), (None, None));
    }

    #[tokio::test]
    async fn reindex_noop_rule() {
        let dir = tmp_repo();
        let store = FakeStore::default();
        stamped(dir.path(), &store, "abc", &[]);
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let root = dir.path();
        let run = |vcs: FakeVcs, force: bool| {
            let (store, comms, emb, ext) = (&store, &comms, &emb, &ext);
            async move {
                let report = reindex(
                    deps(store, comms, emb, ext, &vcs),
                    root,
                    IndexOptions {
                        force,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                (report, vcs)
            }
        };
        let clean = || FakeVcs {
            head: Some("abc".into()),
            clean: true,
            files: vec!["src/lib.rs".into()],
            ..Default::default()
        };
        // HEAD is the stamp, clean, nothing uncommitted at the stamp: no-op.
        let (report, vcs) = run(clean(), false).await;
        assert!(report.skipped);
        assert!(store.writes.lock().unwrap().is_empty());
        assert!(!vcs.called("diff_commits") && !vcs.called("worktree_changes"));

        // Dirty worktree → the delta runs (not a full index).
        let dirty = FakeVcs {
            clean: false,
            changes: vec![Change {
                path: "src/lib.rs".into(),
                status: ChangeStatus::Modified,
            }],
            ..clean()
        };
        let (report, vcs) = run(dirty, false).await;
        assert!(!report.skipped);
        assert_eq!(report.files, 1);
        assert!(report.delta.as_ref().unwrap().fallback.is_none());
        assert!(!vcs.called("list_files"));
        let writes = store.writes.lock().unwrap().clone();
        assert!(
            !writes.iter().any(|w| w.starts_with("replace_repo")),
            "{writes:?}"
        );
        assert!(
            writes.iter().any(|w| w.starts_with("merge_files:0:1")),
            "{writes:?}"
        );

        // Clean, HEAD is the stamp, but a path was uncommitted at the stamp:
        // not a no-op (the file may have been reverted since).
        store.writes.lock().unwrap().clear();
        let (report, _) = run(clean(), false).await;
        assert!(!report.skipped);
        assert_eq!(
            store.stamps.lock().unwrap().last().unwrap(),
            &(Some("abc".to_string()), vec![])
        );
        // … and once that is stamped away, the next one is a no-op again.
        store.writes.lock().unwrap().clear();
        assert!(run(clean(), false).await.0.skipped);

        // Clean + same sha but force → full run.
        let (report, vcs) = run(clean(), true).await;
        assert!(!report.skipped);
        assert!(report.delta.is_none());
        assert!(vcs.called("list_files"));
    }

    #[tokio::test]
    async fn reindex_rereads_dependents_of_changed_files() {
        let dir = tmp_repo();
        std::fs::write(dir.path().join("src/a.rs"), "use crate::b;\n").unwrap();
        std::fs::write(dir.path().join("src/b.rs"), "pub fn b2() {}\n").unwrap();
        std::fs::write(dir.path().join("src/gen.rs"), "use crate::b;\n").unwrap();
        let store = FakeStore::default();
        let repo_id = stamped(dir.path(), &store, "abc", &[]);
        let files: Vec<_> = [
            ("src/a.rs", "use crate::b;\n"),
            ("src/b.rs", "pub fn b() {}\n"),
            ("src/lib.rs", "pub fn a() {}\npub fn b() { a() }\n"),
            ("src/gen.rs", "use crate::b;\n"),
        ]
        .iter()
        .map(|(p, c)| stored_file(repo_id, p, c))
        .collect();
        let file_node = |f: &SourceFile| Node {
            file_id: Some(f.id),
            ..node(repo_id, &format!("file:{}", f.path), NodeKind::File, None)
        };
        let (fa, fb, flib, fgen) = (
            file_node(&files[0]),
            file_node(&files[1]),
            file_node(&files[2]),
            file_node(&files[3]),
        );
        *store.graph.lock().unwrap() = Graph {
            edges: vec![
                edge(
                    repo_id,
                    fa.id,
                    fb.id,
                    crate::domain::model::EdgeKind::Imports,
                ),
                edge(
                    repo_id,
                    fgen.id,
                    fb.id,
                    crate::domain::model::EdgeKind::Imports,
                ),
            ],
            nodes: vec![fa, fb, flib, fgen],
            files,
        };
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = FakeVcs {
            head: Some("def".into()),
            clean: true,
            diffs: [(
                "abc".to_string(),
                vec![Change {
                    path: "src/b.rs".into(),
                    status: ChangeStatus::Modified,
                }],
            )]
            .into_iter()
            .collect(),
            // Indexed by an older run, gitignored since: the full listing
            // no longer has it.
            ignored: vec!["src/gen.rs".into()],
            ..Default::default()
        };
        let report = reindex(
            deps(&store, &comms, &emb, &ext, &vcs),
            dir.path(),
            IndexOptions::default(),
        )
        .await
        .unwrap();
        let delta = report.delta.unwrap();
        assert_eq!((delta.committed, delta.dependents), (1, 2));
        assert_eq!(report.files, 2);
        // b.rs changed; a.rs imports it and is re-read; gen.rs imports it
        // but is no longer listed, so it is purged; lib.rs is untouched.
        assert_eq!(
            store.merged.lock().unwrap().as_slice(),
            &["src/a.rs", "src/b.rs"]
        );
        assert_eq!(
            store.purged.lock().unwrap().as_slice(),
            &["src/a.rs", "src/b.rs", "src/gen.rs"]
        );
        assert!(vcs.called("list_paths:src/a.rs,src/gen.rs"));
        assert_eq!(
            store.stamps.lock().unwrap().as_slice(),
            &[(Some("def".to_string()), vec![])]
        );
        assert!(report.provenance.iter().any(|b| b.method == "dependents"));
    }

    #[tokio::test]
    async fn reindex_runs_in_full_on_an_incomplete_stamp_or_other_rules() {
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = FakeVcs {
            head: Some("abc".into()),
            clean: true,
            files: vec!["src/lib.rs".into()],
            ..Default::default()
        };
        type Spoil = fn(&mut Repo);
        let cases: [(&str, Spoil); 3] = [
            // A run died after its first write (or the row predates 1.4).
            ("the stamp is incomplete", |r| r.worktree_paths = None),
            // skip_dirs changed, or a language was added.
            ("indexing rules changed", |r| {
                r.listing_rules = Some(listing_rules(&["tests".into()], &[]))
            }),
            ("indexing rules changed", |r| r.listing_rules = None),
        ];
        for (why, spoil) in cases {
            let dir = tmp_repo();
            let store = FakeStore::default();
            stamped(dir.path(), &store, "abc", &[]);
            spoil(store.repo.lock().unwrap().as_mut().unwrap());
            // HEAD is the stamp and the worktree is clean: not a no-op.
            let report = reindex(
                deps(&store, &comms, &emb, &ext, &vcs),
                dir.path(),
                IndexOptions::default(),
            )
            .await
            .unwrap();
            assert!(!report.skipped, "{why}");
            let fallback = report.delta.unwrap().fallback.unwrap();
            assert!(fallback.contains(why), "{fallback}");
            assert!(store
                .writes
                .lock()
                .unwrap()
                .iter()
                .any(|w| w.starts_with("replace_repo")));
            let repo = store.repo.lock().unwrap().clone().unwrap();
            assert_eq!(repo.worktree_paths, Some(Vec::new()));
            assert_eq!(repo.listing_rules, Some(listing_rules(&[], &[])));
        }
    }

    #[tokio::test]
    async fn reindex_falls_back_to_a_full_index_when_the_base_is_gone() {
        let dir = tmp_repo();
        let store = FakeStore::default();
        stamped(dir.path(), &store, "gcd", &[]);
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = FakeVcs {
            head: Some("def".into()),
            clean: true,
            missing: vec!["gcd".into()],
            files: vec!["src/lib.rs".into()],
            ..Default::default()
        };
        let report = reindex(
            deps(&store, &comms, &emb, &ext, &vcs),
            dir.path(),
            IndexOptions::default(),
        )
        .await
        .unwrap();
        let why = report.delta.as_ref().unwrap().fallback.clone().unwrap();
        assert!(
            why.contains("gcd") && why.contains("not in this clone"),
            "{why}"
        );
        assert_eq!(report.provenance[0].method, "delta");
        assert!(store
            .writes
            .lock()
            .unwrap()
            .iter()
            .any(|w| w.starts_with("replace_repo")));
        assert_eq!(
            store.stamps.lock().unwrap().as_slice(),
            &[(Some("def".to_string()), vec![])]
        );
    }

    /// Five connected functions in two files (Louvain yields ≥ 1 community),
    /// every one documented with `doc` and starting at `line`.
    fn small_graph(repo_id: Uuid, doc: &str, line: i32) -> Graph {
        let nodes: Vec<Node> = [
            "rust:src/a.rs:function:a",
            "rust:src/a.rs:function:b",
            "rust:src/a.rs:function:c",
            "rust:src/b.rs:function:d",
            "rust:src/b.rs:function:e",
        ]
        .iter()
        .map(|s| Node {
            line_start: Some(line),
            line_end: Some(line + 1),
            ..node(repo_id, s, NodeKind::Function, Some(doc))
        })
        .collect();
        let edges = nodes
            .windows(2)
            .map(|w| {
                edge(
                    repo_id,
                    w[0].id,
                    w[1].id,
                    crate::domain::model::EdgeKind::Calls,
                )
            })
            .collect();
        Graph {
            nodes,
            edges,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn summaries_are_embedded_only_when_their_text_changes() {
        let repo_id = hash::repo_id("/x");
        let store = FakeStore::default();
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = FakeVcs::default();
        let run = || rebuild_hierarchy(deps(&store, &comms, &emb, &ext, &vcs), repo_id);

        *store.graph.lock().unwrap() = small_graph(repo_id, "does a thing", 1);
        let first = run().await.unwrap();
        assert!(!first.communities.is_empty());
        assert_eq!(first.summaries_recomputed, first.communities.len());
        for c in &first.communities {
            let text = c.summary.as_deref().unwrap();
            assert_eq!(
                c.summary_hash.as_deref(),
                Some(summary::summary_hash(text, "fake-model").as_str())
            );
        }

        // Nothing changed: every summary keeps its vector.
        assert_eq!(run().await.unwrap().summaries_recomputed, 0);

        // The code moved inside its files, but no summary reads line numbers.
        *store.graph.lock().unwrap() = small_graph(repo_id, "does a thing", 40);
        assert_eq!(run().await.unwrap().summaries_recomputed, 0);

        // A doc comment changed: the summaries that quote it are embedded again.
        *store.graph.lock().unwrap() = small_graph(repo_id, "does another thing", 40);
        let changed = run().await.unwrap();
        assert!(changed.summaries_recomputed > 0);
        assert!(changed.summaries_recomputed <= changed.communities.len());
        assert_eq!(
            comms.summaries_saved.lock().unwrap().len(),
            first.summaries_recomputed + changed.summaries_recomputed
        );
    }

    #[test]
    fn warnings_are_capped() {
        let mut w = Vec::new();
        let mut omitted = 0;
        for i in 0..30 {
            push_warning(&mut w, &mut omitted, format!("w{i}"));
        }
        let out = finish_warnings(w, omitted);
        assert_eq!(out.len(), MAX_WARNINGS);
        assert!(out.last().unwrap().contains("10 more"));
    }
}
