//! Use-case: incremental index of what changed since the last stamped run.
//!
//! The work comes from the git delta ([`crate::application::delta`]): files
//! changed between the stamped commit and HEAD, files uncommitted now and
//! files uncommitted at the stamp, filtered by the full listing's rules and
//! re-read only when their content hash differs from the indexed one. No
//! full scan: when the stamp is missing or incomplete, the listing rules
//! changed, or its commit is not in this clone, a content-hash reconcile of
//! every file runs instead and the report says why.
//!
//! Re-read files are extracted and analyzed against the persisted graph
//! (`analyze_with(.., Some(graph))`), so edges INTO unchanged files and the
//! chunk headers match a full `index`; edges FROM unchanged files to new
//! symbols wait for `reindex`, which also re-reads the files one hop upstream
//! of a change. Deleted, renamed-from and no-longer-listed paths are purged;
//! every re-read indexed path is purged too and re-inserted by `merge_files`.
//! Only chunks the store reports missing get embedded. The hierarchy is
//! rebuilt only when something changed, and a summary is embedded only when
//! its text changed.
//!
//! The stamp is marked incomplete before the first index write and completed
//! only after embedding and the hierarchy, so a run that fails in between is
//! finished by the next `add` (full reconcile, then embedding and the
//! hierarchy even when no file changed) or the next `reindex` (full index).
//! A run with nothing to do writes nothing but the new stamp, and nothing at
//! all when the stamp is current.

use crate::application::delta::{self, Collected, Delta, DeltaSummary, Stored};
use crate::application::index::{
    embed_missing, extract_and_analyze, finish_warnings, identify, push_warning, read_input,
    rebuild_hierarchy, Deps, IndexOptions, IndexReport, RepoIdentity, SkipPathsVcs,
};
use crate::domain::model::Repo;
use crate::domain::provenance::{source, Breadcrumb};
use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize)]
pub struct AddReport {
    pub repo: String,
    /// Paths whose index rows change (re-read ∪ purged).
    pub changed: usize,
    /// Files re-read and re-extracted.
    pub read: usize,
    /// Paths purged from the index (deleted + renamed-from + re-inserted).
    pub purged: usize,
    pub nodes: usize,
    pub chunks: usize,
    pub chunks_embedded: usize,
    /// Communities whose summary text changed, appeared or disappeared.
    pub communities_changed: usize,
    pub summaries_recomputed: usize,
    /// Base and head commits, the committed / uncommitted / previously
    /// uncommitted path counts, and whether the full reconcile ran instead.
    pub delta: DeltaSummary,
    pub warnings: Vec<String>,
    pub provenance: Vec<Breadcrumb>,
}

/// Warnings and provenance collected along one run.
#[derive(Default)]
struct Log {
    warnings: Vec<String>,
    omitted: usize,
    provenance: Vec<Breadcrumb>,
}

/// Result of [`apply`].
#[derive(Debug, Clone, Default)]
struct Applied {
    read: usize,
    purged: usize,
    nodes: usize,
    edges: usize,
    chunks: usize,
    chunks_embedded: usize,
    communities: usize,
    communities_changed: usize,
    summaries_recomputed: usize,
    call_resolution: f32,
    /// The delta's summary, with `dependents` filled in.
    summary: DeltaSummary,
}

pub async fn add(
    deps: Deps<'_>,
    root: &Path,
    since: Option<&str>,
    opts: IndexOptions,
) -> Result<AddReport> {
    let skip_paths = opts.skip_paths.clone();
    let vcs = SkipPathsVcs::new(deps.vcs, &skip_paths);
    let deps = vcs.deps(deps);
    let ident = identify(root)?;
    let Some(repo) = deps.store.find_repo(&ident.root_path).await? else {
        bail!(
            "repository {} is not indexed yet — run `index` first",
            ident.root_path
        );
    };
    let mut log = Log::default();
    // An incomplete stamp: the last run wrote to the index and did not finish.
    let finish = repo.worktree_paths.is_none();
    let rules = opts.listing_rules();
    let delta = match delta::collect(deps.vcs, &ident.root, &repo, since, &rules)? {
        Collected::Ready(pending) => resolve(deps, &ident, &repo, pending, &opts, &mut log).await?,
        Collected::Fallback(why) => {
            if since.is_some() {
                log.provenance.push(Breadcrumb::new(
                    source::GIT,
                    "since",
                    "--since subsumed: the full reconcile checks every file".to_string(),
                ));
            }
            full_reconcile(deps, &ident, &repo, why, &opts, &mut log).await?
        }
    };
    let changed = delta.to_read.union(&delta.purge).count();
    let applied = apply(deps, &ident, &repo, delta, false, finish, &opts, &mut log).await?;
    tracing::info!(
        repo = %ident.root_path,
        changed,
        read = applied.read,
        purged = applied.purged,
        embedded = applied.chunks_embedded,
        fallback = applied.summary.fallback.is_some(),
        "add done"
    );
    Ok(AddReport {
        repo: ident.root_path.clone(),
        changed,
        read: applied.read,
        purged: applied.purged,
        nodes: applied.nodes,
        chunks: applied.chunks,
        chunks_embedded: applied.chunks_embedded,
        communities_changed: applied.communities_changed,
        summaries_recomputed: applied.summaries_recomputed,
        delta: applied.summary,
        warnings: finish_warnings(log.warnings, log.omitted),
        provenance: log.provenance,
    })
}

/// The incremental half of `reindex`: the same delta as `add` plus the files
/// one hop upstream of every change ([`delta::dependents`]). `Ok(Err(why))`
/// when the delta cannot run and the caller should run the full index.
pub(crate) async fn reindex_delta(
    deps: Deps<'_>,
    ident: &RepoIdentity,
    repo: &Repo,
    opts: &IndexOptions,
) -> Result<std::result::Result<IndexReport, String>> {
    let mut log = Log::default();
    let rules = opts.listing_rules();
    // An incomplete stamp falls back here, so the full index finishes it.
    let pending = match delta::collect(deps.vcs, &ident.root, repo, None, &rules)? {
        Collected::Ready(pending) => pending,
        Collected::Fallback(why) => return Ok(Err(why)),
    };
    let delta = resolve(deps, ident, repo, pending, opts, &mut log).await?;
    let applied = apply(deps, ident, repo, delta, true, false, opts, &mut log).await?;
    Ok(Ok(IndexReport {
        repo: ident.root_path.clone(),
        files: applied.read,
        nodes: applied.nodes,
        edges: applied.edges,
        chunks: applied.chunks,
        chunks_embedded: applied.chunks_embedded,
        chunks_reused: applied.chunks.saturating_sub(applied.chunks_embedded),
        communities: applied.communities,
        summaries_recomputed: applied.summaries_recomputed,
        call_resolution: applied.call_resolution,
        delta: Some(applied.summary),
        warnings: finish_warnings(log.warnings, log.omitted),
        provenance: log.provenance,
        ..Default::default()
    }))
}

/// Indexed paths and content hashes, read only when git reported something.
async fn stored_hashes(deps: Deps<'_>, repo: &Repo) -> Result<Stored> {
    Ok(deps.store.file_hashes(repo.id).await?.into_iter().collect())
}

async fn resolve(
    deps: Deps<'_>,
    ident: &RepoIdentity,
    repo: &Repo,
    pending: delta::Pending,
    opts: &IndexOptions,
    log: &mut Log,
) -> Result<Delta> {
    let stored = if pending.is_empty() {
        Stored::new()
    } else {
        stored_hashes(deps, repo).await?
    };
    let delta = pending.resolve(deps.vcs, &ident.root, &stored, &opts.skip_dirs)?;
    let s = &delta.summary;
    log.provenance.push(
        Breadcrumb::new(
            source::GIT,
            "delta",
            format!(
                "{} → {}{}: {} committed, {} uncommitted, {} uncommitted at the stamp, {} directories, as the full listing has them; {} unchanged by content hash → {} to read, {} to purge",
                s.base.as_deref().unwrap_or("none"),
                s.head.as_deref().unwrap_or("none"),
                s.since
                    .as_deref()
                    .map(|c| format!(" (and {c} → HEAD)"))
                    .unwrap_or_default(),
                s.committed,
                s.uncommitted,
                s.previous_worktree,
                s.directories,
                s.unchanged,
                delta.to_read.len(),
                delta.purge.len()
            ),
        )
        .with_locator(ident.root_path.clone()),
    );
    Ok(delta)
}

/// The fallback of `add`: compare every file on disk with the index.
async fn full_reconcile(
    deps: Deps<'_>,
    ident: &RepoIdentity,
    repo: &Repo,
    why: String,
    opts: &IndexOptions,
    log: &mut Log,
) -> Result<Delta> {
    push_warning(
        &mut log.warnings,
        &mut log.omitted,
        format!("git delta unavailable ({why}); reconciled every file by content hash"),
    );
    let stored = stored_hashes(deps, repo).await?;
    let head = deps.vcs.head_sha(&ident.root).unwrap_or(None);
    let mut delta = Delta {
        head: head.clone(),
        summary: DeltaSummary {
            base: repo.commit_sha.clone(),
            head,
            fallback: Some(why.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    let (listed, gone) = delta::reconcile(
        deps.vcs,
        &ident.root,
        None,
        &stored,
        &opts.skip_dirs,
        &mut delta,
    )?;
    if delta.head.is_some() {
        if let Ok(changes) = deps.vcs.worktree_changes(&ident.root) {
            delta.worktree = delta::stamp_paths(&changes, &ident.root, &opts.skip_dirs, &stored);
        }
    }
    log.provenance.push(
        Breadcrumb::new(source::GIT, "delta", format!("fallback: {why}"))
            .with_locator(ident.root_path.clone()),
    );
    log.provenance.push(Breadcrumb::new(
        source::FILE,
        "hash reconcile",
        format!(
            "{} indexed files vs {listed} on disk: {} new or drifted, {gone} gone, {} unchanged",
            stored.len(),
            delta.to_read.len(),
            delta.summary.unchanged
        ),
    ));
    Ok(delta)
}

/// Read, analyze, merge, embed and rebuild the hierarchy for `delta`, then
/// stamp. With `dependents`, files one hop upstream of the change are read
/// too. With `finish` (the stamp was incomplete), embedding and the hierarchy
/// run even when no file changed. Writes nothing but a changed stamp when
/// there is nothing to do.
#[allow(clippy::too_many_arguments)]
async fn apply(
    deps: Deps<'_>,
    ident: &RepoIdentity,
    repo: &Repo,
    mut delta: Delta,
    dependents: bool,
    finish: bool,
    opts: &IndexOptions,
    log: &mut Log,
) -> Result<Applied> {
    let rules = opts.listing_rules();
    let mut out = Applied {
        summary: delta.summary.clone(),
        ..Default::default()
    };
    if delta.is_empty() && !finish {
        write_stamp(deps, repo, &delta, &rules, false, log).await?;
        return Ok(out);
    }
    let before: HashMap<_, _> = deps
        .communities
        .load_communities(repo.id)
        .await?
        .into_iter()
        .map(|c| (c.id, c.summary_hash))
        .collect();
    let merged = !delta.is_empty()
        && merge(
            deps, ident, repo, &mut delta, dependents, opts, log, &mut out,
        )
        .await?;
    if !merged {
        if !finish {
            // Only unreadable new files (warned): the index is unchanged.
            write_stamp(deps, repo, &delta, &rules, false, log).await?;
            return Ok(out);
        }
        log.provenance.push(Breadcrumb::new(
            source::POSTGRES,
            "finish",
            "the stamp was incomplete (a run that did not finish, or a stamp from before 1.4): embedding missing chunks and rebuilding the hierarchy"
                .to_string(),
        ));
    }

    out.chunks_embedded = embed_missing(deps, repo.id, &mut log.provenance).await?;

    let hier = rebuild_hierarchy(deps, repo.id).await?;
    let after: HashMap<_, _> = hier
        .communities
        .iter()
        .map(|c| (c.id, &c.summary_hash))
        .collect();
    let mut changed_comms = after
        .iter()
        .filter(|(id, h)| before.get(*id) != Some(**h))
        .count();
    changed_comms += before.keys().filter(|id| !after.contains_key(*id)).count();
    out.communities = hier.communities.len();
    out.communities_changed = changed_comms;
    out.summaries_recomputed = hier.summaries_recomputed;
    log.provenance.extend(hier.provenance);
    log.provenance.push(Breadcrumb::new(
        source::GRAPH,
        "diff",
        format!(
            "{changed_comms} of {} communities changed their summary",
            after.len()
        ),
    ));

    write_stamp(deps, repo, &delta, &rules, true, log).await?;
    Ok(out)
}

/// The index writes of [`apply`]: add the dependents, read and analyze, mark
/// the stamp incomplete, merge. `false` when there was nothing to write.
#[allow(clippy::too_many_arguments)]
async fn merge(
    deps: Deps<'_>,
    ident: &RepoIdentity,
    repo: &Repo,
    delta: &mut Delta,
    dependents: bool,
    opts: &IndexOptions,
    log: &mut Log,
    out: &mut Applied,
) -> Result<bool> {
    let mut graph = deps.store.load_graph(repo.id).await?;
    if dependents {
        let changed: BTreeSet<String> = delta.to_read.union(&delta.purge).cloned().collect();
        let upstream: Vec<String> = delta::dependents(&graph, &changed).into_iter().collect();
        // The full listing decides, as for every other path: a dependent it
        // no longer has is purged, not read.
        let listed: BTreeSet<String> = if upstream.is_empty() {
            BTreeSet::new()
        } else {
            deps.vcs
                .list_paths(&ident.root, &upstream, &opts.skip_dirs)?
                .into_iter()
                .collect()
        };
        log.provenance.push(Breadcrumb::new(
            source::GRAPH,
            "dependents",
            format!(
                "{} unchanged files import or reference the {} changed files: {} re-read, {} no longer listed and purged",
                upstream.len(),
                changed.len(),
                listed.len(),
                upstream.len() - listed.len()
            ),
        ));
        out.summary.dependents = upstream.len();
        for p in upstream {
            if listed.contains(&p) {
                delta.to_read.insert(p.clone());
            }
            delta.purge.insert(p);
        }
    }
    // Resolve against the index minus the files leaving it.
    let leaving: HashSet<&String> = delta.purge.difference(&delta.to_read).collect();
    graph.files.retain(|f| !leaving.contains(&f.path));

    let mut inputs = Vec::with_capacity(delta.to_read.len());
    for rel in &delta.to_read {
        // A path reported as changed but gone from disk is purged only.
        if !ident.root.join(rel).is_file() {
            push_warning(
                &mut log.warnings,
                &mut log.omitted,
                format!("{rel}: listed as changed but missing on disk; purged"),
            );
            continue;
        }
        if let Some(input) = read_input(&ident.root, rel, &mut log.warnings, &mut log.omitted) {
            inputs.push(input);
        }
    }
    out.read = inputs.len();
    log.provenance.push(Breadcrumb::new(
        source::FILE,
        "read",
        format!("{} changed files read", inputs.len()),
    ));
    if inputs.is_empty() && delta.purge.is_empty() {
        return Ok(false);
    }

    // Resolve against the persisted graph so imports/calls/relations — and the
    // `uses:`/`calls:` chunk-header lines that hash into content_hash — come
    // out identical to a full `index` of the same file.
    let (bundle, call_resolution) = extract_and_analyze(
        deps,
        repo.id,
        inputs,
        Some(&graph),
        &opts.solidity,
        opts.split_cos,
        &mut log.warnings,
        &mut log.omitted,
        &mut log.provenance,
    )
    .await?;
    drop(graph);
    out.nodes = bundle.nodes.len();
    out.edges = bundle.edges.len();
    out.chunks = bundle.chunks.len();
    out.call_resolution = call_resolution;

    // Incomplete from the first write until `write_stamp`: if anything below
    // fails, the merged hashes match the disk and the delta of the next run
    // would be empty, so that run must know to finish embedding and the
    // hierarchy. The base commit is kept.
    if repo.worktree_paths.is_some() {
        deps.store
            .upsert_repo(&Repo {
                worktree_paths: None,
                ..repo.clone()
            })
            .await?;
    }
    let purge_paths: Vec<String> = delta.purge.iter().cloned().collect();
    deps.store
        .merge_files(repo.id, &purge_paths, &bundle)
        .await?;
    out.purged = purge_paths.len();
    log.provenance.push(Breadcrumb::new(
        source::POSTGRES,
        "merge_files",
        format!(
            "{} paths purged, {} files / {} nodes / {} chunks merged",
            purge_paths.len(),
            bundle.files.len(),
            bundle.nodes.len(),
            bundle.chunks.len()
        ),
    ));
    Ok(true)
}

/// Stamp HEAD, the uncommitted paths and the listing rules. Unless `force`,
/// only when they differ from the stored stamp, so a no-op run writes
/// nothing.
async fn write_stamp(
    deps: Deps<'_>,
    repo: &Repo,
    delta: &Delta,
    rules: &str,
    force: bool,
    log: &mut Log,
) -> Result<()> {
    if !force
        && repo.commit_sha == delta.head
        && repo.worktree_paths.as_ref() == Some(&delta.worktree)
        && repo.listing_rules.as_deref() == Some(rules)
    {
        return Ok(());
    }
    deps.store
        .set_repo_stamp(repo.id, delta.head.as_deref(), &delta.worktree, rules)
        .await?;
    log.provenance.push(Breadcrumb::new(
        source::POSTGRES,
        "set_repo_stamp",
        format!(
            "commit {}, {} uncommitted paths",
            delta.head.as_deref().unwrap_or("none"),
            delta.worktree.len()
        ),
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::index::fakes::*;
    use crate::domain::hash;
    use crate::domain::model::listing_rules;
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

    fn ch(path: &str, status: ChangeStatus) -> Change {
        Change {
            path: path.into(),
            status,
        }
    }

    fn write(root: &Path, path: &str, content: &str) {
        let abs = root.join(path);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(abs, content).unwrap();
    }

    /// The repo is indexed at commit `abc` with `files` (path, content) and
    /// `prev` uncommitted at that stamp.
    fn indexed_repo(
        dir: &Path,
        store: &FakeStore,
        files: &[(&str, &str)],
        prev: &[&str],
    ) -> uuid::Uuid {
        let ident = identify(dir).unwrap();
        *store.repo.lock().unwrap() = Some(Repo {
            id: ident.id,
            name: ident.name.clone(),
            root_path: ident.root_path.clone(),
            commit_sha: Some("abc".into()),
            worktree_paths: Some(prev.iter().map(|p| p.to_string()).collect()),
            // The rules of `IndexOptions::default()`.
            listing_rules: Some(listing_rules(&[], &[])),
        });
        store.graph.lock().unwrap().files = files
            .iter()
            .map(|(p, c)| stored_file(ident.id, p, c))
            .collect();
        ident.id
    }

    /// git at HEAD `def`, `committed` since the stamp `abc`, `uncommitted` now.
    fn git(committed: Vec<Change>, uncommitted: Vec<Change>) -> FakeVcs {
        FakeVcs {
            head: Some("def".into()),
            diffs: [("abc".to_string(), committed)].into_iter().collect(),
            changes: uncommitted,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn requires_prior_index() {
        let dir = tempfile::tempdir().unwrap();
        let store = FakeStore::default();
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = FakeVcs::default();
        let err = add(
            deps(&store, &comms, &emb, &ext, &vcs),
            dir.path(),
            None,
            IndexOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("run `index` first"), "{err}");
    }

    /// Foundry `libs` stay out of the delta as they stay out of the full
    /// listing: a change under one is never read, a file renamed into one
    /// only purges its old path, and a `lib/` submodule git reports as one
    /// directory is not walked into the index.
    #[tokio::test]
    async fn foundry_libs_stay_out_of_the_delta() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/OnChainLab.sol", "contract A { uint x; }\n");
        write(
            r,
            "dependencies/forge-std-1.12.0/src/Test.sol",
            "contract T {}\n",
        );
        write(r, "dependencies/local/Old.sol", "contract O {}\n");
        write(r, "lib/forge-std/src/Vm.sol", "interface Vm {}\n");
        let store = FakeStore::default();
        indexed_repo(
            r,
            &store,
            &[
                ("src/Old.sol", "contract O {}\n"),
                ("src/OnChainLab.sol", "contract A {}\n"),
            ],
            &[],
        );
        let opts = IndexOptions {
            skip_paths: vec!["dependencies".into(), "lib".into()],
            ..Default::default()
        };
        store.repo.lock().unwrap().as_mut().unwrap().listing_rules = Some(opts.listing_rules());
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = git(
            vec![
                ch(
                    "dependencies/forge-std-1.12.0/src/Test.sol",
                    ChangeStatus::Modified,
                ),
                ch(
                    "dependencies/local/Old.sol",
                    ChangeStatus::Renamed {
                        from: "src/Old.sol".into(),
                    },
                ),
                ch("src/OnChainLab.sol", ChangeStatus::Modified),
                // A submodule pointer: git names only the directory.
                ch("lib/forge-std", ChangeStatus::Modified),
            ],
            vec![],
        );
        let report = add(deps(&store, &comms, &emb, &ext, &vcs), r, None, opts)
            .await
            .unwrap();
        assert!(report.delta.fallback.is_none(), "{report:?}");
        assert_eq!(
            store.merged.lock().unwrap().as_slice(),
            &["src/OnChainLab.sol"]
        );
        assert_eq!(
            store.purged.lock().unwrap().as_slice(),
            &["src/Old.sol", "src/OnChainLab.sol"]
        );
        assert_eq!(report.read, 1);
    }

    /// `skip_paths` are part of the listing rules: a stamp written without
    /// them (another Foundry layout, or a pre-1.4 run) makes `add` reconcile
    /// every file once.
    #[tokio::test]
    async fn other_skip_paths_reconcile_every_file() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/A.sol", "contract A {}\n");
        write(r, "lib/dep/B.sol", "contract B {}\n");
        let store = FakeStore::default();
        indexed_repo(
            r,
            &store,
            &[
                ("src/A.sol", "contract A {}\n"),
                ("lib/dep/B.sol", "contract B {}\n"),
            ],
            &[],
        );
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = FakeVcs {
            head: Some("abc".into()),
            files: vec!["src/A.sol".into(), "lib/dep/B.sol".into()],
            ..Default::default()
        };
        let opts = IndexOptions {
            skip_paths: vec!["lib".into()],
            ..Default::default()
        };
        let rules = opts.listing_rules();
        let report = add(deps(&store, &comms, &emb, &ext, &vcs), r, None, opts)
            .await
            .unwrap();
        assert!(report.delta.fallback.is_some(), "{report:?}");
        assert_eq!(store.purged.lock().unwrap().as_slice(), &["lib/dep/B.sol"]);
        assert!(store.merged.lock().unwrap().is_empty());
        let repo = store.repo.lock().unwrap().clone().unwrap();
        assert_eq!(repo.listing_rules, Some(rules));
    }

    #[tokio::test]
    async fn no_change_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/a.rs", "fn a() {}");
        let store = FakeStore::default();
        indexed_repo(dir.path(), &store, &[("src/a.rs", "fn a() {}")], &[]);
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        // HEAD is the stamp; only a non-indexable file is dirty.
        let vcs = FakeVcs {
            head: Some("abc".into()),
            changes: vec![ch("Cargo.toml", ChangeStatus::Modified)],
            ..Default::default()
        };
        let report = add(
            deps(&store, &comms, &emb, &ext, &vcs),
            dir.path(),
            None,
            IndexOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(report.changed, 0);
        assert!(report.delta.fallback.is_none());
        assert!(store.writes.lock().unwrap().is_empty());
        assert!(comms.writes.lock().unwrap().is_empty());
        assert!(!vcs.called("list_files"), "no full scan");
    }

    #[tokio::test]
    async fn reads_the_delta_purges_and_stamps_head_with_worktree_paths() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/new.rs", "pub fn n() {}\n");
        write(r, "src/moved.rs", "pub fn m() {}\n");
        write(r, "src/dirty.rs", "pub fn d2() {}\n");
        write(r, "src/same.rs", "pub fn s() {}\n");
        write(r, "src/untouched.rs", "pub fn u() {}\n");
        let store = FakeStore::default();
        let repo_id = indexed_repo(
            r,
            &store,
            &[
                ("src/gone.rs", "pub fn g() {}\n"),
                ("src/old.rs", "pub fn m() {}\n"),
                ("src/dirty.rs", "pub fn d() {}\n"),
                ("src/same.rs", "pub fn s() {}\n"),
                ("src/untouched.rs", "pub fn u() {}\n"),
            ],
            &[],
        );
        // The store reports exactly one chunk missing an embedding.
        *store.missing.lock().unwrap() = vec![chunk(repo_id, "only-this")];
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = git(
            vec![
                ch("src/gone.rs", ChangeStatus::Deleted),
                ch(
                    "src/moved.rs",
                    ChangeStatus::Renamed {
                        from: "src/old.rs".into(),
                    },
                ),
                ch("src/new.rs", ChangeStatus::Added),
                ch("src/same.rs", ChangeStatus::Modified),
                ch("assets/logo.png", ChangeStatus::Added),
            ],
            vec![ch("src/dirty.rs", ChangeStatus::Modified)],
        );
        let report = add(
            deps(&store, &comms, &emb, &ext, &vcs),
            r,
            None,
            IndexOptions::default(),
        )
        .await
        .unwrap();
        assert!(!vcs.called("list_files"), "no full scan in the normal path");
        assert_eq!(
            store.merged.lock().unwrap().as_slice(),
            &["src/dirty.rs", "src/moved.rs", "src/new.rs"]
        );
        assert_eq!(
            store.purged.lock().unwrap().as_slice(),
            &["src/dirty.rs", "src/gone.rs", "src/old.rs"]
        );
        assert_eq!(report.read, 3);
        assert_eq!(report.purged, 3);
        assert_eq!(report.changed, 5);
        assert_eq!(report.delta.base.as_deref(), Some("abc"));
        assert_eq!(report.delta.head.as_deref(), Some("def"));
        assert_eq!(report.delta.committed, 5, "both rename sides, no png");
        assert_eq!(report.delta.uncommitted, 1);
        assert_eq!(report.delta.unchanged, 1, "src/same.rs");
        // Only the store-reported missing chunk is embedded, regardless of the bundle.
        assert_eq!(report.chunks_embedded, 1);
        let saved = store.saved_embeddings.lock().unwrap().clone();
        assert_eq!(
            saved,
            vec![(
                hash::scoped_id(repo_id, "only-this"),
                hash::sha256_hex("only-this")
            )]
        );
        let writes = store.writes.lock().unwrap().clone();
        let merge_at = writes
            .iter()
            .position(|w| w.starts_with("merge_files:3:"))
            .unwrap_or_else(|| panic!("{writes:?}"));
        // The stamp is marked incomplete (base kept) before the first index
        // write and completed last.
        assert_eq!(writes[merge_at - 1], "upsert_repo", "{writes:?}");
        let marked = store.upserts.lock().unwrap().clone();
        assert_eq!(marked.len(), 1);
        assert_eq!(marked[0].commit_sha.as_deref(), Some("abc"));
        assert_eq!(marked[0].worktree_paths, None);
        assert_eq!(writes.last().unwrap(), "set_repo_stamp");
        assert_eq!(
            store.stamps.lock().unwrap().as_slice(),
            &[(Some("def".to_string()), vec!["src/dirty.rs".to_string()])]
        );
        assert!(report.provenance.iter().any(|b| b.method == "delta"));

        // A second add right after: HEAD is the new stamp, the dirty file is
        // indexed with its current bytes — nothing to do, nothing written.
        store.writes.lock().unwrap().clear();
        store.graph.lock().unwrap().files = [
            ("src/dirty.rs", "pub fn d2() {}\n"),
            ("src/moved.rs", "pub fn m() {}\n"),
            ("src/new.rs", "pub fn n() {}\n"),
            ("src/same.rs", "pub fn s() {}\n"),
            ("src/untouched.rs", "pub fn u() {}\n"),
        ]
        .iter()
        .map(|(p, c)| stored_file(repo_id, p, c))
        .collect();
        let again = FakeVcs {
            head: Some("def".into()),
            changes: vec![ch("src/dirty.rs", ChangeStatus::Modified)],
            ..Default::default()
        };
        let report = add(
            deps(&store, &comms, &emb, &ext, &again),
            r,
            None,
            IndexOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(report.changed, 0);
        assert_eq!(report.delta.unchanged, 1);
        assert!(store.writes.lock().unwrap().is_empty());
        assert!(!again.called("diff_commits"));
    }

    #[tokio::test]
    async fn reverted_dirty_file_is_reread_and_the_stamp_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/a.rs", "pub fn clean() {}\n");
        let store = FakeStore::default();
        indexed_repo(
            r,
            &store,
            &[("src/a.rs", "pub fn dirty() {}\n")],
            &["src/a.rs"],
        );
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = FakeVcs {
            head: Some("abc".into()),
            ..Default::default()
        };
        let report = add(
            deps(&store, &comms, &emb, &ext, &vcs),
            r,
            None,
            IndexOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(report.read, 1);
        assert_eq!(report.delta.previous_worktree, 1);
        assert_eq!(store.merged.lock().unwrap().as_slice(), &["src/a.rs"]);
        assert_eq!(
            store.stamps.lock().unwrap().as_slice(),
            &[(Some("abc".to_string()), vec![])]
        );
    }

    #[tokio::test]
    async fn missing_base_falls_back_to_the_full_reconcile() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/a.rs", "pub fn a2() {}\n");
        write(r, "src/b.rs", "pub fn b() {}\n");
        let store = FakeStore::default();
        indexed_repo(
            r,
            &store,
            &[
                ("src/a.rs", "pub fn a() {}\n"),
                ("src/b.rs", "pub fn b() {}\n"),
                ("src/c.rs", "x"),
            ],
            &[],
        );
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = FakeVcs {
            head: Some("def".into()),
            missing: vec!["abc".into()],
            files: vec!["src/a.rs".into(), "src/b.rs".into()],
            changes: vec![ch("src/b.rs", ChangeStatus::Modified)],
            ..Default::default()
        };
        let report = add(
            deps(&store, &comms, &emb, &ext, &vcs),
            r,
            None,
            IndexOptions::default(),
        )
        .await
        .unwrap();
        let why = report.delta.fallback.as_deref().unwrap();
        assert!(why.contains("not in this clone"), "{why}");
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("git delta unavailable")));
        assert!(vcs.called("list_files"));
        assert!(report
            .provenance
            .iter()
            .any(|b| b.method == "hash reconcile"));
        assert_eq!(store.merged.lock().unwrap().as_slice(), &["src/a.rs"]);
        assert_eq!(
            store.purged.lock().unwrap().as_slice(),
            &["src/a.rs", "src/c.rs"]
        );
        // Stamped, so the next add uses the delta again.
        assert_eq!(
            store.stamps.lock().unwrap().as_slice(),
            &[(Some("def".to_string()), vec!["src/b.rs".to_string()])]
        );
    }

    #[tokio::test]
    async fn since_adds_its_range_and_unknown_refs_fail() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/a.rs", "pub fn a2() {}\n");
        let store = FakeStore::default();
        indexed_repo(r, &store, &[("src/a.rs", "pub fn a() {}\n")], &[]);
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let mut vcs = git(vec![], vec![]);
        vcs.diffs.insert(
            "HEAD~1".into(),
            vec![ch("src/a.rs", ChangeStatus::Modified)],
        );
        vcs.missing.push("nope".into());
        let report = add(
            deps(&store, &comms, &emb, &ext, &vcs),
            r,
            Some("HEAD~1"),
            IndexOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(report.read, 1);
        assert_eq!(report.delta.since.as_deref(), Some("HEAD~1"));
        assert!(vcs.called("diff_commits:abc..def"));
        assert!(vcs.called("diff_commits:HEAD~1..def"));
        let err = add(
            deps(&store, &comms, &emb, &ext, &vcs),
            r,
            Some("nope"),
            IndexOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not a commit"), "{err}");
    }

    #[tokio::test]
    async fn changed_path_missing_on_disk_is_purged() {
        let dir = tempfile::tempdir().unwrap();
        let store = FakeStore::default();
        indexed_repo(dir.path(), &store, &[("src/ghost.rs", "fn g() {}")], &[]);
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = git(vec![ch("src/ghost.rs", ChangeStatus::Modified)], vec![]);
        let report = add(
            deps(&store, &comms, &emb, &ext, &vcs),
            dir.path(),
            None,
            IndexOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(report.purged, 1);
        assert_eq!(report.read, 0);
        assert_eq!(store.purged.lock().unwrap().as_slice(), &["src/ghost.rs"]);
    }

    /// A run that dies after `merge_files` leaves the merged hashes behind,
    /// so the next delta is empty; the incomplete stamp makes that run finish
    /// embedding and the hierarchy anyway.
    #[tokio::test]
    async fn a_failed_add_is_finished_by_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/a.rs", "pub fn a2() {}\n");
        let store = FakeStore::default();
        let repo_id = indexed_repo(r, &store, &[("src/a.rs", "pub fn a() {}\n")], &[]);
        *store.missing.lock().unwrap() = vec![chunk(repo_id, "new-chunk")];
        let comms = FakeCommunities::default();
        let ext = FakeExtractor;
        let mut vcs = git(vec![ch("src/a.rs", ChangeStatus::Modified)], vec![]);
        vcs.files = vec!["src/a.rs".into()];
        let down = FakeEmbedder {
            fail: true,
            ..Default::default()
        };
        let err = add(
            deps(&store, &comms, &down, &ext, &vcs),
            r,
            None,
            IndexOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("embedding failed"), "{err}");
        assert_eq!(store.merged.lock().unwrap().as_slice(), &["src/a.rs"]);
        assert!(store.stamps.lock().unwrap().is_empty());
        let after = store.repo.lock().unwrap().clone().unwrap();
        assert_eq!(after.commit_sha.as_deref(), Some("abc"), "base kept");
        assert_eq!(after.worktree_paths, None, "stamp marked incomplete");

        // The merge landed: the index holds the new bytes.
        store.graph.lock().unwrap().files =
            vec![stored_file(repo_id, "src/a.rs", "pub fn a2() {}\n")];
        store.writes.lock().unwrap().clear();
        let emb = FakeEmbedder::default();
        let report = add(
            deps(&store, &comms, &emb, &ext, &vcs),
            r,
            None,
            IndexOptions::default(),
        )
        .await
        .unwrap();
        let why = report.delta.fallback.as_deref().unwrap();
        assert!(why.contains("the stamp is incomplete"), "{why}");
        assert_eq!((report.changed, report.read), (0, 0));
        assert_eq!(report.chunks_embedded, 1, "finished the failed run");
        assert!(report.provenance.iter().any(|b| b.method == "finish"));
        assert!(comms
            .writes
            .lock()
            .unwrap()
            .iter()
            .any(|w| w.starts_with("replace_communities")));
        assert_eq!(
            store.stamps.lock().unwrap().as_slice(),
            &[(Some("def".to_string()), vec![])]
        );
        assert!(!store
            .writes
            .lock()
            .unwrap()
            .iter()
            .any(|w| w.starts_with("merge_files")));

        // Complete again: the next add has nothing to do and writes nothing.
        store.writes.lock().unwrap().clear();
        *store.missing.lock().unwrap() = Vec::new();
        let report = add(
            deps(&store, &comms, &emb, &ext, &vcs),
            r,
            None,
            IndexOptions::default(),
        )
        .await
        .unwrap();
        assert!(report.delta.fallback.is_none());
        assert_eq!(report.changed, 0);
        assert!(store.writes.lock().unwrap().is_empty());
    }

    /// skip_dirs changed (or a language was added): files git reports no
    /// change for enter or leave the index, so every file is reconciled once.
    #[tokio::test]
    async fn other_listing_rules_reconcile_every_file() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/s.rs", "pub fn s() {}\n");
        write(r, "tests/t.rs", "pub fn t() {}\n");
        let store = FakeStore::default();
        indexed_repo(r, &store, &[("src/s.rs", "pub fn s() {}\n")], &[]);
        store.repo.lock().unwrap().as_mut().unwrap().listing_rules =
            Some(listing_rules(&["tests".into()], &[]));
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        // HEAD is the stamp and nothing is uncommitted.
        let vcs = FakeVcs {
            head: Some("abc".into()),
            files: vec!["src/s.rs".into(), "tests/t.rs".into()],
            ..Default::default()
        };
        let report = add(
            deps(&store, &comms, &emb, &ext, &vcs),
            r,
            None,
            IndexOptions::default(),
        )
        .await
        .unwrap();
        let why = report.delta.fallback.as_deref().unwrap();
        assert!(why.contains("indexing rules changed"), "{why}");
        assert!(vcs.called("list_files"));
        assert_eq!(store.merged.lock().unwrap().as_slice(), &["tests/t.rs"]);
        assert_eq!(report.delta.unchanged, 1);
        let repo = store.repo.lock().unwrap().clone().unwrap();
        assert_eq!(repo.listing_rules, Some(listing_rules(&[], &[])));
    }
}
