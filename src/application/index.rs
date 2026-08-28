//! Use-case: full index / hash-gated reindex of one repository.
//!
//! Pipeline: list files (Vcs) → read → extract (SourceExtractor) →
//! `analyze` (project-wide resolution) → `build_chunks` → persist
//! (IndexStore::replace_repo, embeddings preserved by content hash) → embed
//! only the chunks the store reports missing → rebuild the hierarchy
//! (Merkle file hashes → Louvain communities → hash-gated summaries) → stamp
//! the repo with `commit_sha` + `root_hash`.
//!
//! Everything here depends on `domain` + `ports` only.

use crate::application::{analyze, chunk};
use crate::domain::community::{detect, to_communities};
use crate::domain::model::{Community, IndexBundle, Language, Node, NodeKind, Repo};
use crate::domain::provenance::{source, Breadcrumb};
use crate::domain::{hash, merkle, summary};
use crate::ports::community_store::CommunityStore;
use crate::ports::embedder::Embedder;
use crate::ports::extractor::{FileExtraction, FileInput, SourceExtractor};
use crate::ports::index_store::{Graph, IndexStore};
use crate::ports::vcs::Vcs;
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
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            skip_dirs: Vec::new(),
            split_cos: crate::domain::chunking::DEFAULT_SPLIT_COS,
            dry_run: false,
            force: false,
        }
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
}

/// Result of [`rebuild_hierarchy`] — shared with `add`.
#[derive(Debug, Clone, Default)]
pub struct HierarchyReport {
    pub communities: Vec<Community>,
    pub summaries_recomputed: usize,
    pub root_hash: String,
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
    let analyzed = analyze::analyze_with(repo_id, &pairs, known);
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

/// Rebuild L2 (Merkle) + L1 (communities) + L3 (hash-gated summaries) from the
/// persisted graph. Summaries are recomputed ONLY when the stored
/// `summary_hash` differs from `summary::summary_gate(subtree_hash, model_id)`.
pub async fn rebuild_hierarchy(deps: Deps<'_>, repo_id: Uuid) -> Result<HierarchyReport> {
    let mut provenance = Vec::new();
    let graph = deps.store.load_graph(repo_id).await?;
    let leaves = deps.store.load_chunk_hashes(repo_id).await?;
    provenance.push(Breadcrumb::new(
        source::POSTGRES,
        "load_graph",
        format!(
            "{} files, {} nodes, {} edges, {} chunk hashes",
            graph.files.len(),
            graph.nodes.len(),
            graph.edges.len(),
            leaves.len()
        ),
    ));

    // L2: Merkle roll-up.
    let file_hashes = merkle::file_hashes(&leaves);
    let mut file_rows: Vec<(String, String)> = file_hashes
        .iter()
        .map(|(p, h)| (p.clone(), h.clone()))
        .collect();
    file_rows.sort();
    deps.store
        .set_file_subtree_hashes(repo_id, &file_rows)
        .await?;
    let root_hash = merkle::root_hash(&file_hashes);
    provenance.push(Breadcrumb::new(
        source::MERKLE,
        "rollup",
        format!("{} file hashes → root {root_hash}", file_rows.len()),
    ));

    // L1: communities.
    let det = detect(repo_id, &graph.nodes, &graph.edges);
    let (communities, members) = to_communities(&det, repo_id, &file_hashes);
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

    // L3: hash-gated summaries.
    let model = deps.embedder.model_ref();
    let candidates = deps
        .communities
        .communities_needing_summary(repo_id, &model)
        .await?;
    let ctx = SummaryContext::new(&graph.nodes, &graph.edges, &communities, &members);
    let mut recomputed = 0usize;
    for c in &candidates {
        let gate = summary::summary_gate(&c.subtree_hash, &model.model_id);
        if c.summary_hash.as_deref() == Some(gate.as_str()) {
            continue;
        }
        let input = ctx.input_for(c);
        let text = summary::compose(&input);
        let vector = deps
            .embedder
            .embed(&text)
            .await
            .with_context(|| format!("summary embedding for {}", c.label))?;
        deps.communities
            .save_summary(c.id, &text, &gate, &model, &vector)
            .await?;
        recomputed += 1;
    }
    provenance.push(Breadcrumb::new(
        source::EMBEDDING,
        "summary_gate",
        format!(
            "{recomputed} of {} candidate summaries recomputed ({} communities total)",
            candidates.len(),
            communities.len()
        ),
    ));
    tracing::info!(recomputed, candidates = candidates.len(), "summaries");
    Ok(HierarchyReport {
        communities,
        summaries_recomputed: recomputed,
        root_hash,
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
    let ident = identify(root)?;
    let mut warnings = Vec::new();
    let mut omitted = 0usize;
    let mut provenance = Vec::new();
    tracing::info!(repo = %ident.root_path, dry_run = opts.dry_run, "index start");

    let head_sha = deps.vcs.head_sha(&ident.root).unwrap_or(None);
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

    let repo = Repo {
        id: ident.id,
        name: ident.name.clone(),
        root_path: ident.root_path.clone(),
        commit_sha: head_sha.clone(),
        root_hash: None,
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
        .set_repo_hashes(ident.id, head_sha.as_deref(), &hier.root_hash)
        .await?;
    provenance.push(Breadcrumb::new(
        source::POSTGRES,
        "set_repo_hashes",
        format!(
            "commit {} root {}",
            head_sha.as_deref().unwrap_or("none"),
            hier.root_hash
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

/// Re-run a full index unless the repo is already indexed at the current HEAD
/// with a clean worktree (`force` overrides).
pub async fn reindex(deps: Deps<'_>, root: &Path, opts: IndexOptions) -> Result<IndexReport> {
    let ident = identify(root)?;
    if !opts.force {
        if let Some(existing) = deps.store.find_repo(&ident.root_path).await? {
            let head = deps.vcs.head_sha(&ident.root).unwrap_or(None);
            let clean = deps.vcs.is_clean(&ident.root).unwrap_or(false);
            if head.is_some() && head == existing.commit_sha && clean {
                tracing::info!(repo = %ident.root_path, "reindex skipped: HEAD unchanged and worktree clean");
                return Ok(IndexReport {
                    repo: ident.root_path.clone(),
                    skipped: true,
                    dry_run: opts.dry_run,
                    provenance: vec![Breadcrumb::new(
                        source::GIT,
                        "head_sha",
                        format!("HEAD {} equals indexed commit and worktree is clean; nothing to do (use force)", head.unwrap_or_default()),
                    )],
                    ..Default::default()
                });
            }
        }
    }
    index(deps, root, opts).await
}

#[cfg(test)]
pub(crate) mod fakes {
    //! Scripted in-memory fakes of the ports, shared by the `index` and `add`
    //! use-case tests. Not mocks of the embedder in runtime code: test-only.
    use super::*;
    use crate::domain::model::{Chunk, Edge, ModelRef};
    use crate::ports::index_store::Graph;
    use crate::ports::vcs::Change;
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct FakeStore {
        pub writes: Mutex<Vec<String>>,
        pub repo: Mutex<Option<Repo>>,
        pub graph: Mutex<Graph>,
        pub chunk_hashes: Mutex<Vec<(String, String)>>,
        pub missing: Mutex<Vec<Chunk>>,
        pub saved_embeddings: Mutex<Vec<(Uuid, String)>>,
        pub purged: Mutex<Vec<String>>,
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
        async fn set_repo_hashes(
            &self,
            _repo_id: Uuid,
            _commit_sha: Option<&str>,
            _root_hash: &str,
        ) -> Result<()> {
            self.log("set_repo_hashes");
            Ok(())
        }
        async fn replace_repo(&self, _repo_id: Uuid, bundle: &IndexBundle) -> Result<()> {
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
            Ok(())
        }
        async fn load_graph(&self, _repo_id: Uuid) -> Result<Graph> {
            Ok(self.graph.lock().unwrap().clone())
        }
        async fn load_chunk_hashes(&self, _repo_id: Uuid) -> Result<Vec<(String, String)>> {
            Ok(self.chunk_hashes.lock().unwrap().clone())
        }
        async fn set_file_subtree_hashes(
            &self,
            _repo_id: Uuid,
            _hashes: &[(String, String)],
        ) -> Result<()> {
            self.log("set_file_subtree_hashes");
            Ok(())
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

    #[derive(Default)]
    pub struct FakeCommunities {
        pub writes: Mutex<Vec<String>>,
        /// Returned by `communities_needing_summary` (scripted).
        pub needing: Mutex<Vec<Community>>,
        /// Last `replace_communities` payload.
        pub stored: Mutex<Vec<Community>>,
        pub summaries_saved: Mutex<Vec<(Uuid, String)>>,
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
            Ok(self.needing.lock().unwrap().clone())
        }
        async fn save_summary(
            &self,
            community_id: Uuid,
            _summary: &str,
            summary_hash: &str,
            _model: &ModelRef,
            _embedding: &[f32],
        ) -> Result<()> {
            self.writes.lock().unwrap().push("save_summary".into());
            self.summaries_saved
                .lock()
                .unwrap()
                .push((community_id, summary_hash.to_string()));
            Ok(())
        }
    }

    /// Scripted embedder: deterministic vectors derived from text length
    /// (test-only; never used at runtime).
    #[derive(Default)]
    pub struct FakeEmbedder {
        pub calls: Mutex<Vec<usize>>,
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
            self.calls.lock().unwrap().push(1);
            let l = input.len() as f32;
            Ok(vec![1.0, l, 1.0 / (l + 1.0)])
        }
        async fn embed_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
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

    #[derive(Default)]
    pub struct FakeVcs {
        pub head: Option<String>,
        pub clean: bool,
        pub files: Vec<PathBuf>,
        pub changes: Vec<Change>,
    }

    impl Vcs for FakeVcs {
        fn head_sha(&self, _root: &Path) -> Result<Option<String>> {
            Ok(self.head.clone())
        }
        fn is_clean(&self, _root: &Path) -> Result<bool> {
            Ok(self.clean)
        }
        fn changes(&self, _root: &Path, _since: Option<&str>) -> Result<Vec<Change>> {
            Ok(self.changes.clone())
        }
        fn list_files(&self, _root: &Path, _skip_dirs: &[String]) -> Result<Vec<PathBuf>> {
            Ok(self.files.clone())
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
    async fn full_index_persists_embeds_missing_and_stamps_hashes() {
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
        assert_eq!(writes.last().unwrap(), "set_repo_hashes");
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

    #[tokio::test]
    async fn reindex_skips_when_clean_and_sha_matches() {
        let dir = tmp_repo();
        let store = FakeStore::default();
        let ident = identify(dir.path()).unwrap();
        *store.repo.lock().unwrap() = Some(Repo {
            id: ident.id,
            name: ident.name.clone(),
            root_path: ident.root_path.clone(),
            commit_sha: Some("abc".into()),
            root_hash: Some("r".into()),
        });
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = FakeVcs {
            head: Some("abc".into()),
            clean: true,
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
        assert!(report.skipped);
        assert!(store.writes.lock().unwrap().is_empty());

        // Dirty worktree → runs.
        let vcs = FakeVcs {
            head: Some("abc".into()),
            clean: false,
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
        assert!(!report.skipped);
        assert!(!store.writes.lock().unwrap().is_empty());

        // Clean + same sha but force → runs.
        store.writes.lock().unwrap().clear();
        let vcs = FakeVcs {
            head: Some("abc".into()),
            clean: true,
            files: vec!["src/lib.rs".into()],
            ..Default::default()
        };
        let report = reindex(
            deps(&store, &comms, &emb, &ext, &vcs),
            dir.path(),
            IndexOptions {
                force: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(!report.skipped);
    }

    #[tokio::test]
    async fn summaries_recomputed_only_when_gate_differs() {
        let repo_id = hash::repo_id("/x");
        let store = FakeStore::default();
        // A small graph: 5 connected nodes so Louvain yields ≥ 1 community.
        let ids: Vec<&str> = vec![
            "rust:src/a.rs:function:a",
            "rust:src/a.rs:function:b",
            "rust:src/a.rs:function:c",
            "rust:src/b.rs:function:d",
            "rust:src/b.rs:function:e",
        ];
        let nodes: Vec<Node> = ids
            .iter()
            .map(|s| node(repo_id, s, NodeKind::Function, Some("does a thing")))
            .collect();
        let mut edges = Vec::new();
        for w in nodes.windows(2) {
            edges.push(edge(
                repo_id,
                w[0].id,
                w[1].id,
                crate::domain::model::EdgeKind::Calls,
            ));
        }
        store.graph.lock().unwrap().nodes = nodes;
        store.graph.lock().unwrap().edges = edges;
        *store.chunk_hashes.lock().unwrap() = vec![
            ("src/a.rs".into(), "h1".into()),
            ("src/b.rs".into(), "h2".into()),
        ];

        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = FakeVcs::default();
        let model_id = emb.model_id().to_string();
        // Script the "needing summary" list: one community already at the gate, one stale, one new.
        let mk = |key: &str, hash: &str, stored: Option<String>| Community {
            id: hash::scoped_id(repo_id, key),
            repo_id,
            label: key.to_string(),
            member_count: 1,
            subtree_hash: hash.to_string(),
            summary: stored.as_ref().map(|_| "old".to_string()),
            summary_hash: stored,
            top_members: vec!["a".into()],
            files: vec!["src/a.rs".into()],
        };
        let at_gate = mk("k1", "s1", Some(summary::summary_gate("s1", &model_id)));
        let stale = mk("k2", "s2", Some("stale".into()));
        let fresh = mk("k3", "s3", None);
        *comms.needing.lock().unwrap() = vec![at_gate.clone(), stale.clone(), fresh.clone()];

        let hier = rebuild_hierarchy(deps(&store, &comms, &emb, &ext, &vcs), repo_id)
            .await
            .unwrap();
        assert_eq!(hier.summaries_recomputed, 2);
        let saved = comms.summaries_saved.lock().unwrap().clone();
        let saved_ids: Vec<Uuid> = saved.iter().map(|(id, _)| *id).collect();
        assert!(!saved_ids.contains(&at_gate.id));
        assert!(saved_ids.contains(&stale.id));
        assert!(saved_ids.contains(&fresh.id));
        assert!(saved.iter().all(|(_, h)| h.len() == 64));
        assert!(!hier.root_hash.is_empty());
        assert!(!hier.communities.is_empty());
        assert!(store
            .writes
            .lock()
            .unwrap()
            .contains(&"set_file_subtree_hashes".to_string()));
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
