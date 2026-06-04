//! `chaos add` — incremental "index what I just changed" command.
//!
//! Detects changed files from git (or an explicit path list), merges only
//! those files into the existing Postgres/pgvector index, refreshes the
//! Obsidian vault, and writes a deterministic interactive feature/bug page into
//! `docs/features_memory`. It is the one-shot companion to the analyze →
//! refresh → feature-website flow: no file lists to type, no LLM round-trip.
//!
//! Both the `chaos add` CLI subcommand and the `chaos_add` MCP tool call
//! [`run`].

use crate::{
    embedding::Embedder,
    extractor::{current_commit, RustRepositoryExtractor},
    feature_context::{
        FeatureClaim, FeatureContextEdge, FeatureContextNode, FeatureDefinition, FeatureEvidence,
        FeatureManifest, FeatureMode, FeatureStoryStep,
    },
    feature_export::render_feature_website,
    graph_export::{GraphExport, GraphExportNode},
    obsidian_export::write_obsidian_vault,
    storage::Storage,
    Config,
};
use anyhow::{bail, Result};
use futures::{StreamExt, TryStreamExt};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use uuid::Uuid;

/// Maximum nodes rendered on a generated feature/bug page (changed symbols plus
/// their graph neighbors). Keeps the interactive surface legible.
const MAX_PAGE_NODES: usize = 16;
/// Maximum source lines embedded per node snippet.
const MAX_SNIPPET_LINES: usize = 120;

/// Options for [`run`], shared by the CLI and MCP surfaces.
#[derive(Debug, Default, Clone)]
pub struct AddOptions {
    /// Explicit files to add; when non-empty this overrides git-diff detection.
    pub paths: Vec<PathBuf>,
    /// Diff against this git ref instead of the working tree (e.g. `HEAD~1`).
    pub since: Option<String>,
    /// Force the page classification: `feature` or `bug`. Auto-detected if None.
    pub kind: Option<String>,
    /// Short title/summary of the addition; drives the page title and slug.
    pub message: Option<String>,
    /// Where to write the Obsidian vault (default `<repo>/chaos-obsidian-vault`).
    pub obsidian_output: Option<PathBuf>,
    /// Skip the Obsidian vault refresh.
    pub no_obsidian: bool,
    /// Skip writing the feature/bug page.
    pub no_page: bool,
}

/// Run `chaos add`: detect changes, index them, and regenerate artifacts.
pub async fn run(
    config: &Config,
    storage: &Storage,
    embedder: &dyn Embedder,
    repo_path: &Path,
    opts: &AddOptions,
) -> Result<Value> {
    let repo_root = fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.to_path_buf());
    let commit = current_commit(&repo_root);
    let repo = storage
        .upsert_repository(&repo_root, commit.as_deref())
        .await?;

    // 1. Resolve the set of files to index.
    let candidates = if opts.paths.is_empty() {
        git_changed_files(&repo_root, opts.since.as_deref())?
    } else {
        opts.paths
            .iter()
            .map(|p| {
                if p.is_absolute() {
                    p.clone()
                } else {
                    repo_root.join(p)
                }
            })
            .collect()
    };
    // Honor the same directory exclusions as the full-repo walk so `chaos add`
    // never re-indexes its own generated artifacts (the Obsidian vault,
    // features_memory pages, target/, node_modules/, …) when they later appear
    // as untracked files.
    let skip_dirs: HashSet<&str> = config
        .indexing
        .skip_dirs
        .iter()
        .map(String::as_str)
        .collect();
    let candidates: Vec<PathBuf> = candidates
        .into_iter()
        .filter(|p| p.is_file())
        .filter(|p| {
            !p.components().any(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .is_some_and(|name| skip_dirs.contains(name))
            })
        })
        .collect();

    // 2. Extract just those files.
    let extractor = RustRepositoryExtractor::new(config.indexing.clone());
    let result = extractor.extract_paths(&repo_root, repo.id, commit.clone(), &candidates)?;
    if result.files.is_empty() {
        return Ok(json!({
            "status": "no_changes",
            "repo_id": repo.id,
            "message": "no indexable changed files detected (nothing was indexed)",
            "scanned_paths": candidates.len(),
            "hint": "stage/edit source or docs, pass --path <file>, or use --since <ref> for a committed range",
        }));
    }
    let changed_rel: Vec<String> = result.files.iter().map(|f| f.path.clone()).collect();
    let counts = json!({
        "files": result.files.len(),
        "nodes": result.nodes.len(),
        "edges": result.edges.len(),
        "chunks": result.chunks.len(),
    });

    // 3. Merge into the index, embed only the new/changed chunks, and recompute
    //    the L1 community layer from the updated graph (deterministic).
    let run_id = storage.begin_analysis(repo.id, commit.as_deref()).await?;
    let indexed = async {
        let embedded = merge_and_embed(storage, embedder, repo.id, &changed_rel, &result).await?;
        let detection = crate::community::detect_and_persist(
            storage,
            repo.id,
            &crate::community::CommunityConfig::default(),
        )
        .await?;
        Result::<_, anyhow::Error>::Ok((embedded, detection))
    }
    .await;
    let (embedded, detection) = match indexed {
        Ok(value) => {
            storage.finish_analysis(run_id, "completed", None).await?;
            value
        }
        Err(err) => {
            storage
                .finish_analysis(run_id, "failed", Some(&err.to_string()))
                .await?;
            return Err(err);
        }
    };
    let feature_communities = detection.communities.iter().filter(|c| c.size >= 2).count();

    // 4. Classify + title the addition.
    let kind = resolve_kind(&repo_root, opts)?;
    let title = resolve_title(&repo_root, opts, kind);

    // 5. Refresh derived artifacts from the now-updated index.
    let graph = storage.load_graph_export(&repo).await?;

    let obsidian = if opts.no_obsidian {
        Value::Null
    } else {
        let output = opts
            .obsidian_output
            .clone()
            .unwrap_or_else(|| repo_root.join("chaos-obsidian-vault"));
        let summary = write_obsidian_vault(&output, &graph)?;
        json!({
            "output": summary.output,
            "topics": summary.topics,
            "node_notes": summary.node_notes,
            "edges": summary.edges,
        })
    };

    let page = if opts.no_page {
        Value::Null
    } else {
        let manifest = build_manifest(&repo_root, kind, &title, &graph, &changed_rel);
        let slug = safe_slug(&format!("{kind}-{title}"));
        let output = repo_root
            .join("docs/features_memory")
            .join(format!("{slug}.html"));
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&output, render_feature_website(&manifest)?)?;
        json!({
            "output": output,
            "slug": slug,
            "nodes": manifest.nodes.len(),
            "edges": manifest.edges.len(),
        })
    };

    Ok(json!({
        "status": "indexed",
        "repo_id": repo.id,
        "kind": kind,
        "title": title,
        "changed_files": changed_rel,
        "indexed": counts,
        "embedded_chunks": embedded,
        "communities": {
            "total": detection.communities.len(),
            "feature_communities": feature_communities,
            "quotient_edges": detection.quotient_edges.len(),
        },
        "obsidian": obsidian,
        "page": page,
    }))
}

/// Merge the partial extraction and embed every chunk that still lacks an
/// embedding for the active provider/model. Returns the number embedded.
async fn merge_and_embed(
    storage: &Storage,
    embedder: &dyn Embedder,
    repo_id: Uuid,
    changed_rel: &[String],
    result: &crate::models::ExtractionResult,
) -> Result<usize> {
    storage
        .merge_files_index(repo_id, changed_rel, result)
        .await?;
    let missing = storage
        .chunks_missing_embeddings(
            repo_id,
            embedder.provider(),
            embedder.model_id(),
            embedder.dimensions(),
        )
        .await?;
    futures::stream::iter(missing.iter().map(|chunk| async move {
        let embedding = embedder.embed(&chunk.content).await?;
        storage
            .insert_embedding(
                chunk,
                embedder.provider(),
                embedder.model_id(),
                embedder.dimensions(),
                &embedding,
            )
            .await?;
        Result::<_, anyhow::Error>::Ok(())
    }))
    .buffer_unordered(crate::EMBED_CONCURRENCY)
    .try_collect::<()>()
    .await?;
    Ok(missing.len())
}

// ---------------------------------------------------------------------------
// git detection
// ---------------------------------------------------------------------------

/// Absolute paths of files to index, derived from git. With `since`, diffs the
/// working tree against that ref; otherwise unions staged + unstaged + untracked
/// changes. A missing or non-git directory yields an empty list rather than an
/// error, so `chaos add` degrades to "no changes".
fn git_changed_files(root: &Path, since: Option<&str>) -> Result<Vec<PathBuf>> {
    let top = git_toplevel(root).unwrap_or_else(|| root.to_path_buf());
    let mut rels: Vec<String> = Vec::new();
    if let Some(since) = since {
        rels.extend(git_lines(root, &["diff", "--name-only", since]));
    } else {
        rels.extend(git_lines(root, &["diff", "--cached", "--name-only"]));
        rels.extend(git_lines(root, &["diff", "--name-only"]));
        rels.extend(git_lines(
            root,
            &["ls-files", "--others", "--exclude-standard"],
        ));
    }

    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut out = Vec::new();
    for rel in rels {
        let abs = top.join(rel);
        if seen.insert(abs.clone()) {
            out.push(abs);
        }
    }
    Ok(out)
}

/// Run `git -C root <args>` and return non-empty stdout lines; empty on any
/// failure (treated as "no changes").
fn git_lines(root: &Path, args: &[&str]) -> Vec<String> {
    let Ok(output) = Command::new("git").arg("-C").arg(root).args(args).output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

/// First non-empty stdout line of `git -C root <args>`, or None on failure.
fn git_first_line(root: &Path, args: &[&str]) -> Option<String> {
    git_lines(root, args).into_iter().next()
}

fn git_toplevel(root: &Path) -> Option<PathBuf> {
    git_first_line(root, &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

// ---------------------------------------------------------------------------
// classification + titling
// ---------------------------------------------------------------------------

/// Decide whether the page is a `feature` or `bug`, honoring an explicit
/// `--kind` and otherwise inferring from the branch name + latest commit
/// subject + message.
fn resolve_kind(root: &Path, opts: &AddOptions) -> Result<&'static str> {
    if let Some(kind) = &opts.kind {
        return match kind.trim().to_ascii_lowercase().as_str() {
            "feature" | "feat" => Ok("feature"),
            "bug" | "fix" => Ok("bug"),
            other => bail!("--kind must be 'feature' or 'bug', got '{other}'"),
        };
    }
    let mut signal = String::new();
    if let Some(branch) = git_first_line(root, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        signal.push_str(&branch);
        signal.push(' ');
    }
    if let Some(subject) = git_first_line(root, &["log", "-1", "--pretty=%s"]) {
        signal.push_str(&subject);
        signal.push(' ');
    }
    if let Some(message) = &opts.message {
        signal.push_str(message);
    }
    Ok(classify_kind(&signal))
}

/// Heuristic feature/bug classifier over a free-text signal (branch + commit +
/// message). Token-level so `prefix` does not match `fix`.
fn classify_kind(signal: &str) -> &'static str {
    const BUG_HINTS: [&str; 8] = [
        "fix",
        "bug",
        "hotfix",
        "patch",
        "regression",
        "revert",
        "issue",
        "defect",
    ];
    let is_bug = signal
        .to_ascii_lowercase()
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|token| {
            !token.is_empty()
                && BUG_HINTS
                    .iter()
                    .any(|hint| token == *hint || token.starts_with(hint))
        });
    if is_bug {
        "bug"
    } else {
        "feature"
    }
}

fn resolve_title(root: &Path, opts: &AddOptions, kind: &str) -> String {
    if let Some(message) = &opts.message {
        if !message.trim().is_empty() {
            return message.trim().to_string();
        }
    }
    if let Some(branch) = git_first_line(root, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        let humanized = humanize_branch(&branch);
        if branch != "HEAD" && !humanized.is_empty() {
            return humanized;
        }
    }
    if let Some(subject) = git_first_line(root, &["log", "-1", "--pretty=%s"]) {
        if !subject.trim().is_empty() {
            return subject.trim().to_string();
        }
    }
    format!("Recent {kind} changes")
}

/// `feat/add-export` → `add export`; `bugfix/null_deref` → `null deref`.
fn humanize_branch(branch: &str) -> String {
    branch
        .rsplit('/')
        .next()
        .unwrap_or(branch)
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

/// Lowercase, hyphen-joined, alphanumeric slug; never empty.
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
        "chaos-add".to_string()
    } else {
        slug.chars()
            .take(80)
            .collect::<String>()
            .trim_matches('-')
            .to_string()
    }
}

// ---------------------------------------------------------------------------
// feature/bug page manifest
// ---------------------------------------------------------------------------

/// Build a [`FeatureManifest`] for the change from the post-merge graph: the
/// changed symbols plus their graph neighbors, with synthesized claims, modes,
/// and a story. The result feeds [`render_feature_website`] and embeds itself,
/// so `chaos refresh --all-features` can re-render it later.
fn build_manifest(
    repo_root: &Path,
    kind: &str,
    title: &str,
    graph: &GraphExport,
    changed_rel: &[String],
) -> FeatureManifest {
    let by_id: HashMap<Uuid, &GraphExportNode> =
        graph.nodes.iter().map(|node| (node.id, node)).collect();
    let changed_files: HashSet<&str> = changed_rel.iter().map(String::as_str).collect();

    // Changed nodes: symbols (and doc/file nodes for files with no symbols) in
    // the changed files.
    let mut changed: Vec<&GraphExportNode> = graph
        .nodes
        .iter()
        .filter(|node| node.kind != "repository")
        .filter(|node| {
            node.file_path
                .as_deref()
                .is_some_and(|path| changed_files.contains(path))
        })
        .collect();
    let files_with_symbols: HashSet<&str> = changed
        .iter()
        .filter(|node| node.kind != "file")
        .filter_map(|node| node.file_path.as_deref())
        .collect();
    changed.retain(|node| {
        node.kind != "file" || !files_with_symbols.contains(node.file_path.as_deref().unwrap_or(""))
    });
    changed.sort_by(|a, b| {
        b.chunk_count
            .cmp(&a.chunk_count)
            .then_with(|| a.name.cmp(&b.name))
    });
    changed.truncate(MAX_PAGE_NODES.saturating_sub(2));
    let changed_ids: HashSet<Uuid> = changed.iter().map(|node| node.id).collect();

    // Neighbors: graph nodes one edge away from a changed node.
    let mut neighbor_ids: Vec<Uuid> = Vec::new();
    let mut seen_neighbor: HashSet<Uuid> = HashSet::new();
    for edge in &graph.edges {
        let neighbor = if changed_ids.contains(&edge.source) && !changed_ids.contains(&edge.target)
        {
            Some(edge.target)
        } else if changed_ids.contains(&edge.target) && !changed_ids.contains(&edge.source) {
            Some(edge.source)
        } else {
            None
        };
        if let Some(id) = neighbor {
            if by_id.get(&id).is_some_and(|n| n.kind != "repository") && seen_neighbor.insert(id) {
                neighbor_ids.push(id);
            }
        }
    }
    let neighbor_budget = MAX_PAGE_NODES.saturating_sub(changed.len());
    neighbor_ids.truncate(neighbor_budget);
    let neighbors: Vec<&GraphExportNode> = neighbor_ids
        .iter()
        .filter_map(|id| by_id.get(id).copied())
        .collect();

    // Assemble selected nodes (changed first), assigning manifest ids.
    let mut nodes: Vec<FeatureContextNode> = Vec::new();
    for node in &changed {
        nodes.push(graph_node_to_feature(repo_root, node, kind, true));
    }
    for node in &neighbors {
        nodes.push(graph_node_to_feature(repo_root, node, kind, false));
    }
    let selected_ids: HashSet<Uuid> = changed
        .iter()
        .chain(neighbors.iter())
        .map(|node| node.id)
        .collect();
    let manifest_id = |id: Uuid| id.to_string();
    let changed_mids: Vec<String> = changed.iter().map(|n| manifest_id(n.id)).collect();
    let neighbor_mids: Vec<String> = neighbors.iter().map(|n| manifest_id(n.id)).collect();

    // Real graph edges among the selected set.
    let mut edges: Vec<FeatureContextEdge> = Vec::new();
    let mut seen_edge: HashSet<(Uuid, Uuid)> = HashSet::new();
    for edge in &graph.edges {
        if edge.source != edge.target
            && selected_ids.contains(&edge.source)
            && selected_ids.contains(&edge.target)
            && seen_edge.insert((edge.source, edge.target))
        {
            edges.push(FeatureContextEdge {
                source: manifest_id(edge.source),
                target: manifest_id(edge.target),
                label: humanize(&edge.kind),
                kind: edge.kind.clone(),
                evidence: FeatureEvidence {
                    source: "knowledge-graph".into(),
                    method: "persisted-edge".into(),
                    notes: format!("confidence {:.0}%", edge.confidence * 100.0),
                },
                confidence: edge.confidence as f32,
            });
        }
    }
    // Fallback: connect co-changed symbols so a tiny diff still renders a graph.
    if edges.len() < 3 {
        let anchor = changed_mids.first().or(neighbor_mids.first()).cloned();
        if let Some(anchor) = anchor {
            for node in nodes.iter() {
                if edges.len() >= 3 {
                    break;
                }
                if node.id == anchor {
                    continue;
                }
                let pair = (anchor.clone(), node.id.clone());
                if edges
                    .iter()
                    .any(|e| e.source == pair.0 && e.target == pair.1)
                {
                    continue;
                }
                edges.push(FeatureContextEdge {
                    source: pair.0,
                    target: pair.1,
                    label: "part of this change".into(),
                    kind: "mentions".into(),
                    evidence: FeatureEvidence {
                        source: "git-diff".into(),
                        method: "co-changed".into(),
                        notes: "indexed together by chaos add".into(),
                    },
                    confidence: 0.5,
                });
            }
        }
    }

    let file_list = changed_rel.join(", ");
    let neighbor_names: Vec<String> = neighbors
        .iter()
        .take(5)
        .map(|node| node.name.clone())
        .collect();

    let claims = vec![
        FeatureClaim {
            id: "claim-changed".into(),
            title: format!(
                "{} symbol(s) across {} file(s) indexed",
                changed.len(),
                changed_rel.len()
            ),
            body: format!("chaos add indexed: {file_list}."),
            confidence: 0.95,
            node_ids: changed_mids.clone(),
        },
        FeatureClaim {
            id: "claim-kind".into(),
            title: format!("Classified as a {kind}"),
            body: "Inferred from branch and commit context (override with --kind). This page is regenerated deterministically by chaos refresh --all-features.".to_string(),
            confidence: 0.7,
            node_ids: changed_mids.iter().take(1).cloned().collect(),
        },
        FeatureClaim {
            id: "claim-context".into(),
            title: if neighbor_names.is_empty() {
                "Self-contained change".into()
            } else {
                format!("Connects to {} existing component(s)", neighbors.len())
            },
            body: if neighbor_names.is_empty() {
                "No existing graph neighbors were found for the changed symbols (yet).".into()
            } else {
                format!("Graph neighbors: {}.", neighbor_names.join(", "))
            },
            confidence: 0.7,
            node_ids: if neighbor_mids.is_empty() {
                changed_mids.clone()
            } else {
                neighbor_mids.clone()
            },
        },
    ];

    let modes = vec![
        FeatureMode {
            id: "mode-changed".into(),
            title: "Changed surface".into(),
            node_ids: changed_mids.clone(),
        },
        FeatureMode {
            id: "mode-context".into(),
            title: "Surrounding context".into(),
            node_ids: if neighbor_mids.is_empty() {
                changed_mids.clone()
            } else {
                neighbor_mids.clone()
            },
        },
    ];

    let verify_ids: Vec<String> = changed
        .iter()
        .filter(|n| n.kind == "test")
        .map(|n| manifest_id(n.id))
        .collect();
    let story = vec![
        FeatureStoryStep {
            id: "story-what".into(),
            title: "What changed".into(),
            body: format!("{file_list} were indexed into code memory as this {kind}."),
            node_ids: changed_mids.clone(),
            edge_ids: Vec::new(),
        },
        FeatureStoryStep {
            id: "story-connects".into(),
            title: "How it connects".into(),
            body: if neighbor_names.is_empty() {
                "Explore the changed symbols and their relations in the graph.".into()
            } else {
                format!(
                    "The change links to {} via graph edges.",
                    neighbor_names.join(", ")
                )
            },
            node_ids: changed_mids
                .iter()
                .chain(neighbor_mids.iter())
                .take(8)
                .cloned()
                .collect(),
            edge_ids: Vec::new(),
        },
        FeatureStoryStep {
            id: "story-verify".into(),
            title: "Where to verify".into(),
            body: "Open the source snippets in the inspector; run the touched tests.".into(),
            node_ids: if verify_ids.is_empty() {
                changed_mids.clone()
            } else {
                verify_ids
            },
            edge_ids: Vec::new(),
        },
    ];

    let slug = safe_slug(&format!("{kind}-{title}"));
    FeatureManifest {
        schema_version: "chaos-add-1".into(),
        feature: FeatureDefinition {
            id: slug,
            title: title.to_string(),
            domain: kind.to_string(),
            summary: format!(
                "{kind} indexed by chaos add: {} symbol(s) across {} file(s).",
                changed.len(),
                changed_rel.len()
            ),
        },
        title: title.to_string(),
        subtitle: format!(
            "{kind} · {} changed · {} context node(s){}",
            changed.len(),
            neighbors.len(),
            graph
                .repository
                .current_commit_sha
                .as_deref()
                .map(|sha| format!(" · {}", &sha[..sha.len().min(8)]))
                .unwrap_or_default()
        ),
        claims,
        modes,
        nodes,
        edges,
        story,
    }
}

fn graph_node_to_feature(
    repo_root: &Path,
    node: &GraphExportNode,
    kind: &str,
    changed: bool,
) -> FeatureContextNode {
    let file = node.file_path.clone().unwrap_or_default();
    let lines = line_range(node.line_start, node.line_end);
    let code = if file.is_empty() {
        String::new()
    } else {
        read_snippet(repo_root, &file, node.line_start, node.line_end)
    };
    FeatureContextNode {
        id: node.id.to_string(),
        label: node.name.chars().take(48).collect(),
        subtitle: node.kind.clone(),
        group: group_for(node),
        file,
        lines,
        role: if changed {
            format!("Added or updated in this {kind}")
        } else {
            format!("Existing {} connected to the change", node.kind)
        },
        code,
        evidence: FeatureEvidence {
            source: if changed {
                "git-diff".into()
            } else {
                "knowledge-graph".into()
            },
            method: "chaos add".into(),
            notes: format!("{} · {} chunk(s)", node.kind, node.chunk_count),
        },
        confidence: if changed { 0.95 } else { 0.7 },
    }
}

/// Map a node onto one of the feature-website palette buckets
/// (ui/api/backend/infra/data) used by the rendered graph colors.
fn group_for(node: &GraphExportNode) -> String {
    let lang = node
        .metadata
        .get("language")
        .and_then(Value::as_str)
        .unwrap_or("");
    match node.kind.as_str() {
        "dependency" | "script" | "deployment_resource" => "infra",
        "test" => "api",
        "file" => match lang {
            "markdown" | "pdf" => "data",
            "json" => "infra",
            _ => "backend",
        },
        "concept" => "data",
        _ if lang == "markdown" || lang == "pdf" => "data",
        _ => "backend",
    }
    .to_string()
}

/// Read `start..=end` (1-based, inclusive) from `repo_root/file` with a
/// line-number gutter, capped at [`MAX_SNIPPET_LINES`]. Empty on any read error.
fn read_snippet(repo_root: &Path, file: &str, start: Option<i32>, end: Option<i32>) -> String {
    let (start, end) = match (start, end) {
        (Some(s), Some(e)) if s >= 1 && e >= s => (s as usize, e as usize),
        (Some(s), _) if s >= 1 => (s as usize, s as usize),
        _ => (1, MAX_SNIPPET_LINES),
    };
    let Ok(content) = fs::read_to_string(repo_root.join(file)) else {
        return String::new();
    };
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() || start > lines.len() {
        return String::new();
    }
    let end = end.min(lines.len()).min(start + MAX_SNIPPET_LINES - 1);
    let mut out = String::new();
    for (offset, line) in lines[start - 1..end].iter().enumerate() {
        out.push_str(&format!("{:>4}  {}\n", start + offset, line));
    }
    out
}

fn line_range(start: Option<i32>, end: Option<i32>) -> String {
    match (start, end) {
        (Some(start), Some(end)) if start != end => format!("{start}-{end}"),
        (Some(start), _) => start.to_string(),
        _ => "n/a".into(),
    }
}

fn humanize(value: &str) -> String {
    value.replace(['_', '-'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn graph_node(kind: &str, name: &str, file: &str) -> GraphExportNode {
        GraphExportNode {
            id: Uuid::new_v4(),
            kind: kind.into(),
            stable_id: format!("{file}:{kind}:{name}"),
            name: name.into(),
            file_path: Some(file.into()),
            line_start: Some(1),
            line_end: Some(1),
            chunk_count: 1,
            metadata: json!({"language": "rust"}),
        }
    }

    #[test]
    fn classify_kind_detects_bugs_token_wise() {
        assert_eq!(classify_kind("fix/null-deref reword"), "bug");
        assert_eq!(classify_kind("hotfix urgent crash"), "bug");
        assert_eq!(classify_kind("feat/add export pipeline"), "feature");
        // `prefix` must not be read as `fix`.
        assert_eq!(classify_kind("prefix routing rework"), "feature");
    }

    #[test]
    fn humanize_branch_strips_prefix_and_separators() {
        assert_eq!(humanize_branch("feat/add-export"), "add export");
        assert_eq!(
            humanize_branch("bugfix/null_deref_guard"),
            "null deref guard"
        );
        assert_eq!(humanize_branch("main"), "main");
    }

    #[test]
    fn safe_slug_is_lowercase_hyphenated_and_nonempty() {
        assert_eq!(safe_slug("Feature: Add Export!"), "feature-add-export");
        assert_eq!(safe_slug("***"), "chaos-add");
    }

    fn edge(source: Uuid, target: Uuid, kind: &str) -> crate::graph_export::GraphExportEdge {
        crate::graph_export::GraphExportEdge {
            id: Uuid::new_v4(),
            source,
            target,
            kind: kind.into(),
            cost: 1.0,
            confidence: 0.8,
            metadata: json!({}),
        }
    }

    #[test]
    fn build_manifest_from_change_plus_neighbors_satisfies_contract() {
        // One changed symbol pulls in graph neighbors; the page must satisfy
        // the same interactive contract the LLM write path enforces.
        let changed = graph_node("function", "do_thing", "src/changed.rs");
        let n1 = graph_node("function", "helper_a", "src/other.rs");
        let n2 = graph_node("struct", "Config", "src/other.rs");
        let n3 = graph_node("function", "helper_b", "src/util.rs");
        let n4 = graph_node("trait", "Sink", "src/util.rs");
        let edges = vec![
            edge(changed.id, n1.id, "calls"),
            edge(changed.id, n2.id, "uses_type"),
            edge(changed.id, n3.id, "calls"),
            edge(changed.id, n4.id, "uses_type"),
        ];
        let graph = GraphExport {
            repository: crate::graph_export::GraphRepository {
                id: Uuid::new_v4(),
                name: "demo".into(),
                root_path: "/tmp/demo".into(),
                current_commit_sha: Some("abcdef1234".into()),
            },
            nodes: vec![changed, n1, n2, n3, n4],
            edges,
        };

        let manifest = build_manifest(
            Path::new("/tmp/demo"),
            "feature",
            "Add do_thing",
            &graph,
            &["src/changed.rs".to_string()],
        );

        assert!(manifest.nodes.len() >= 5);
        assert!(manifest.edges.len() >= 3);
        assert!(manifest.claims.len() >= 3);
        assert!(manifest.modes.len() >= 2);
        assert!(manifest.story.len() >= 3);
        assert_eq!(manifest.feature.domain, "feature");
        let html = render_feature_website(&manifest).unwrap();
        let value = serde_json::to_value(&manifest).unwrap();
        crate::mcp::validate_feature_website_contract(&html, &value).unwrap();
    }

    #[test]
    fn build_manifest_tolerates_a_single_node_change() {
        // A truly tiny diff (one symbol, no neighbors) must not panic and still
        // yields synthesized claims/modes/story; it simply renders fewer nodes.
        let changed = graph_node("function", "lonely", "src/solo.rs");
        let graph = GraphExport {
            repository: crate::graph_export::GraphRepository {
                id: Uuid::new_v4(),
                name: "demo".into(),
                root_path: "/tmp/demo".into(),
                current_commit_sha: None,
            },
            nodes: vec![changed],
            edges: vec![],
        };
        let manifest = build_manifest(
            Path::new("/tmp/demo"),
            "bug",
            "fix lonely",
            &graph,
            &["src/solo.rs".to_string()],
        );
        assert_eq!(manifest.nodes.len(), 1);
        assert!(manifest.claims.len() >= 3);
        assert!(manifest.modes.len() >= 2);
        assert!(manifest.story.len() >= 3);
        // Renders without error even though it can't meet the >=5-node contract.
        render_feature_website(&manifest).unwrap();
    }
}
