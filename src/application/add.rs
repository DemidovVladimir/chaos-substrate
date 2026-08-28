//! Use-case: incremental index of the files git reports as changed.
//!
//! Added/modified/renamed-to files are re-extracted and re-analyzed against
//! the persisted graph (`analyze_with(.., Some(graph))`), so edges INTO
//! unchanged files and the chunk headers match a full `index`; edges FROM
//! unchanged files to new symbols still wait for the next full `index` (a
//! documented limitation). Deleted and renamed-from
//! paths are purged; every changed path is purged too and re-inserted by
//! `merge_files`. Only chunks the store reports missing get embedded. The
//! hierarchy is always rebuilt, but summaries are hash-gated so a no-change
//! add recomputes zero.

use crate::application::index::{
    embed_missing, extract_and_analyze, finish_warnings, identify, push_warning, read_input,
    rebuild_hierarchy, Deps, IndexOptions,
};
use crate::domain::model::Language;
use crate::domain::provenance::{source, Breadcrumb};
use crate::ports::vcs::ChangeStatus;
use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize)]
pub struct AddReport {
    pub repo: String,
    /// Changed paths considered (indexable languages only).
    pub changed: usize,
    /// Paths purged from the index (deleted + renamed-from + re-inserted).
    pub purged: usize,
    pub nodes: usize,
    pub chunks: usize,
    pub chunks_embedded: usize,
    /// Communities whose subtree hash changed, appeared or disappeared.
    pub communities_changed: usize,
    pub summaries_recomputed: usize,
    pub warnings: Vec<String>,
    pub provenance: Vec<Breadcrumb>,
}

pub async fn add(
    deps: Deps<'_>,
    root: &Path,
    since: Option<&str>,
    opts: IndexOptions,
) -> Result<AddReport> {
    let ident = identify(root)?;
    let Some(repo) = deps.store.find_repo(&ident.root_path).await? else {
        bail!(
            "repository {} is not indexed yet — run `index` first",
            ident.root_path
        );
    };
    let repo_id = repo.id;
    let mut warnings = Vec::new();
    let mut omitted = 0usize;
    let mut provenance = Vec::new();

    // Explicit git range (`--since <ref>`): everything git reports in that
    // range. Without it, detection is purely content-hash based (below) — the
    // worktree channel would keep re-reporting uncommitted edits that are
    // already indexed.
    let all_changes = match since {
        Some(_) => deps.vcs.changes(&ident.root, since)?,
        None => Vec::new(),
    };
    let mut purge: BTreeSet<String> = BTreeSet::new();
    let mut to_read: BTreeSet<String> = BTreeSet::new();
    for c in &all_changes {
        match &c.status {
            ChangeStatus::Deleted => {
                if Language::from_path(&c.path).is_some() {
                    purge.insert(c.path.clone());
                }
            }
            ChangeStatus::Renamed { from } => {
                if Language::from_path(from).is_some() {
                    purge.insert(from.clone());
                }
                if Language::from_path(&c.path).is_some() {
                    purge.insert(c.path.clone());
                    to_read.insert(c.path.clone());
                }
            }
            ChangeStatus::Added | ChangeStatus::Modified => {
                if Language::from_path(&c.path).is_some() {
                    purge.insert(c.path.clone());
                    to_read.insert(c.path.clone());
                }
            }
        }
    }
    if let Some(r) = since {
        provenance.push(
            Breadcrumb::new(
                source::GIT,
                "git diff",
                format!("{} raw changes since {r}", all_changes.len()),
            )
            .with_locator(ident.root_path.clone()),
        );
    }
    // Reconcile against what the index actually holds: on-disk files whose
    // content hash differs from the stored one (or that are new) are
    // re-extracted; indexed paths missing from disk are purged. This catches
    // what git cannot see — a reverted worktree, edits between two adds, an
    // index that fell behind.
    let graph = deps.store.load_graph(repo_id).await?;
    let indexed: HashMap<String, String> = graph
        .files
        .iter()
        .map(|f| (f.path.clone(), f.content_hash.clone()))
        .collect();
    let on_disk = deps.vcs.list_files(&ident.root, &opts.skip_dirs)?;
    let mut disk_set: BTreeSet<String> = BTreeSet::new();
    let mut drifted = 0usize;
    for p in &on_disk {
        let rel = p.to_string_lossy().replace('\\', "/");
        disk_set.insert(rel.clone());
        if Language::from_path(&rel).is_none() {
            continue;
        }
        let Ok(bytes) = std::fs::read(ident.root.join(p)) else {
            continue;
        };
        let hash = crate::domain::hash::sha256_hex(&String::from_utf8_lossy(&bytes));
        if indexed.get(&rel) != Some(&hash) {
            if to_read.insert(rel.clone()) {
                drifted += 1;
            }
            purge.insert(rel);
        }
    }
    let mut gone = 0usize;
    for path in indexed.keys() {
        if !disk_set.contains(path) && purge.insert(path.clone()) {
            gone += 1;
        }
    }
    provenance.push(Breadcrumb::new(
        source::FILE,
        "hash reconcile",
        format!(
            "{} indexed files vs {} on disk: {drifted} drifted, {gone} gone",
            indexed.len(),
            on_disk.len()
        ),
    ));
    let changed = purge.len();
    let mut report = AddReport {
        repo: ident.root_path.clone(),
        changed,
        ..Default::default()
    };
    if changed == 0 {
        tracing::info!(repo = %ident.root_path, "add: no indexable changes");
        report.provenance = provenance;
        return Ok(report);
    }

    let mut inputs = Vec::with_capacity(to_read.len());
    for rel in &to_read {
        // A path reported as changed but gone from disk is purged only.
        if !ident.root.join(rel).is_file() {
            push_warning(
                &mut warnings,
                &mut omitted,
                format!("{rel}: listed as changed but missing on disk; purged"),
            );
            continue;
        }
        if let Some(input) = read_input(&ident.root, rel, &mut warnings, &mut omitted) {
            inputs.push(input);
        }
    }
    provenance.push(Breadcrumb::new(
        source::FILE,
        "read",
        format!("{} changed files read", inputs.len()),
    ));

    // Resolve against the persisted graph so imports/calls/relations — and the
    // `uses:`/`calls:` chunk-header lines that hash into content_hash — come
    // out identical to a full `index` of the same file.
    let (bundle, _call_resolution) = extract_and_analyze(
        deps,
        repo_id,
        inputs,
        Some(&graph),
        opts.split_cos,
        &mut warnings,
        &mut omitted,
        &mut provenance,
    )
    .await?;
    report.nodes = bundle.nodes.len();
    report.chunks = bundle.chunks.len();

    let purge_paths: Vec<String> = purge.into_iter().collect();
    let before: HashMap<_, _> = deps
        .communities
        .load_communities(repo_id)
        .await?
        .into_iter()
        .map(|c| (c.id, c.subtree_hash))
        .collect();
    deps.store
        .merge_files(repo_id, &purge_paths, &bundle)
        .await?;
    report.purged = purge_paths.len();
    provenance.push(Breadcrumb::new(
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

    report.chunks_embedded = embed_missing(deps, repo_id, &mut provenance).await?;

    let hier = rebuild_hierarchy(deps, repo_id).await?;
    let after: HashMap<_, _> = hier
        .communities
        .iter()
        .map(|c| (c.id, c.subtree_hash.as_str()))
        .collect();
    let mut changed_comms = after
        .iter()
        .filter(|(id, h)| before.get(*id).map(|b| b != **h).unwrap_or(true))
        .count();
    changed_comms += before.keys().filter(|id| !after.contains_key(*id)).count();
    report.communities_changed = changed_comms;
    report.summaries_recomputed = hier.summaries_recomputed;
    provenance.extend(hier.provenance);
    provenance.push(Breadcrumb::new(
        source::MERKLE,
        "diff",
        format!(
            "{changed_comms} of {} communities changed subtree hash",
            after.len()
        ),
    ));

    let head = deps.vcs.head_sha(&ident.root).unwrap_or(None);
    deps.store
        .set_repo_hashes(repo_id, head.as_deref(), &hier.root_hash)
        .await?;
    report.warnings = finish_warnings(warnings, omitted);
    report.provenance = provenance;
    tracing::info!(
        changed,
        purged = report.purged,
        embedded = report.chunks_embedded,
        communities_changed = changed_comms,
        "add done"
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::index::fakes::*;
    use crate::domain::hash;
    use crate::domain::model::Repo;
    use crate::ports::vcs::Change;

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

    fn indexed_repo(dir: &Path, store: &FakeStore) -> uuid::Uuid {
        let ident = identify(dir).unwrap();
        *store.repo.lock().unwrap() = Some(Repo {
            id: ident.id,
            name: ident.name.clone(),
            root_path: ident.root_path.clone(),
            commit_sha: Some("abc".into()),
            root_hash: None,
        });
        ident.id
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

    #[tokio::test]
    async fn no_changes_returns_early() {
        let dir = tempfile::tempdir().unwrap();
        let store = FakeStore::default();
        indexed_repo(dir.path(), &store);
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = FakeVcs {
            changes: vec![Change {
                path: "Cargo.toml".into(),
                status: ChangeStatus::Modified,
            }],
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
        assert!(store.writes.lock().unwrap().is_empty());
        assert!(comms.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn purges_deleted_and_renamed_from_and_embeds_only_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/new.rs"), "pub fn n() {}\n").unwrap();
        std::fs::write(dir.path().join("src/moved.rs"), "pub fn m() {}\n").unwrap();
        let store = FakeStore::default();
        let repo_id = indexed_repo(dir.path(), &store);
        // The store reports exactly one chunk missing an embedding.
        *store.missing.lock().unwrap() = vec![chunk(repo_id, "only-this")];
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = FakeVcs {
            head: Some("def".into()),
            changes: vec![
                Change {
                    path: "src/gone.rs".into(),
                    status: ChangeStatus::Deleted,
                },
                Change {
                    path: "src/moved.rs".into(),
                    status: ChangeStatus::Renamed {
                        from: "src/old.rs".into(),
                    },
                },
                Change {
                    path: "src/new.rs".into(),
                    status: ChangeStatus::Added,
                },
                Change {
                    path: "assets/logo.png".into(),
                    status: ChangeStatus::Added,
                },
            ],
            ..Default::default()
        };
        let report = add(
            deps(&store, &comms, &emb, &ext, &vcs),
            dir.path(),
            Some("HEAD~1"),
            IndexOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(report.changed, 4, "gone + old + moved + new");
        let purged = store.purged.lock().unwrap().clone();
        assert_eq!(
            purged,
            vec!["src/gone.rs", "src/moved.rs", "src/new.rs", "src/old.rs"]
        );
        assert_eq!(report.purged, 4);
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
        assert_eq!(emb.calls.lock().unwrap().as_slice(), &[1]);
        let writes = store.writes.lock().unwrap().clone();
        assert!(
            writes.iter().any(|w| w.starts_with("merge_files:4:")),
            "{writes:?}"
        );
        assert_eq!(writes.last().unwrap(), "set_repo_hashes");
        assert_eq!(report.summaries_recomputed, 0);
        assert!(report.provenance.iter().any(|b| b.method == "git diff"));
    }

    #[tokio::test]
    async fn changed_path_missing_on_disk_is_purged_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let store = FakeStore::default();
        indexed_repo(dir.path(), &store);
        let comms = FakeCommunities::default();
        let emb = FakeEmbedder::default();
        let ext = FakeExtractor;
        let vcs = FakeVcs {
            changes: vec![Change {
                path: "src/ghost.rs".into(),
                status: ChangeStatus::Modified,
            }],
            ..Default::default()
        };
        let report = add(
            deps(&store, &comms, &emb, &ext, &vcs),
            dir.path(),
            Some("HEAD~1"),
            IndexOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(report.purged, 1);
        assert!(report.warnings.iter().any(|w| w.contains("ghost.rs")));
    }
}
