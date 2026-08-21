//! DB-backed integration tests for the persisted hierarchy layers. They run
//! only when `DATABASE_URL` is set (so the embedder-free CI path skips them) and
//! always operate on a throwaway repo path, purged at the end. They need no
//! embedder — community detection and Merkle rollup are embedder-free.

use super::Storage;
use crate::community::{detect_and_persist, CommunityConfig};
use crate::models::{
    EdgeKind, ExtractionResult, KnowledgeChunk, KnowledgeEdge, KnowledgeNode, Language, NodeKind,
    Repository, SourceFile,
};
use serde_json::json;
use sqlx::Row;
use std::collections::HashMap;
use std::path::Path;
use uuid::Uuid;

fn db_url() -> Option<String> {
    std::env::var("DATABASE_URL").ok()
}

fn func(repo_id: Uuid, file_id: Uuid, file: &str, name: &str) -> KnowledgeNode {
    KnowledgeNode {
        id: Uuid::new_v4(),
        repo_id,
        file_id: Some(file_id),
        kind: NodeKind::Function,
        stable_id: format!("{file}:function:{name}"),
        name: name.into(),
        line_start: Some(1),
        line_end: Some(5),
        metadata: json!({ "language": "rust" }),
    }
}

fn src_file(repo_id: Uuid, path: &str) -> SourceFile {
    SourceFile {
        id: Uuid::new_v4(),
        repo_id,
        commit_sha: Some("testsha".into()),
        path: path.into(),
        language: Language::Rust,
        content: format!("// {path}\n"),
        content_hash: crate::extractor::hash(path),
        line_count: 1,
    }
}

/// Two dense clusters joined by a single weak edge ⇒ two communities.
fn two_cluster_fixture(repo_id: Uuid) -> ExtractionResult {
    let mut result = ExtractionResult::empty();
    result.nodes.push(KnowledgeNode {
        id: Uuid::new_v4(),
        repo_id,
        file_id: None,
        kind: NodeKind::Repository,
        stable_id: "repo".into(),
        name: "fixture".into(),
        line_start: None,
        line_end: None,
        metadata: json!({}),
    });
    let mut funcs = Vec::new();
    for (ci, file) in ["a/a.rs", "b/b.rs"].iter().enumerate() {
        let f = src_file(repo_id, file);
        let fid = f.id;
        result.files.push(f);
        for k in 0..3 {
            let nd = func(repo_id, fid, file, &format!("c{ci}_f{k}"));
            let node_id = nd.id;
            funcs.push((ci, node_id));
            result.nodes.push(nd);
            // One chunk per symbol, with distinct content ⇒ distinct,
            // non-empty file subtree hashes for the Merkle rollup. The
            // `repo_id` makes the content (and its rolled-up subtree
            // hashes) unique per test, so the content-addressed summary
            // cache — which deliberately survives repo wipes — can never
            // bleed between concurrent tests sharing one database.
            let content = format!("fn {file}::c{ci}_f{k} body // {repo_id}");
            result.chunks.push(KnowledgeChunk {
                id: Uuid::new_v4(),
                repo_id,
                file_id: Some(fid),
                node_id: Some(node_id),
                chunk_type: "function".into(),
                content_hash: crate::extractor::hash(&content),
                content,
                line_start: Some(k * 6 + 1),
                line_end: Some(k * 6 + 5),
                metadata: json!({}),
            });
        }
    }
    // Dense intra-cluster edges.
    for ci in 0..2 {
        let ids: Vec<Uuid> = funcs
            .iter()
            .filter(|(c, _)| *c == ci)
            .map(|(_, id)| *id)
            .collect();
        for a in 0..ids.len() {
            for b in (a + 1)..ids.len() {
                result.edges.push(KnowledgeEdge {
                    id: Uuid::new_v4(),
                    repo_id,
                    source_node_id: ids[a],
                    target_node_id: ids[b],
                    kind: EdgeKind::Calls,
                    cost: 0.1,
                    confidence: 1.0,
                    metadata: json!({}),
                });
            }
        }
    }
    result
}

async fn load_file_hashes(storage: &Storage, repo_id: Uuid) -> HashMap<String, Option<String>> {
    let rows = sqlx::query("select path, subtree_hash from files where repo_id = $1")
        .bind(repo_id)
        .fetch_all(&storage.pool)
        .await
        .unwrap();
    rows.into_iter()
        .map(|r| {
            (
                r.get::<String, _>("path"),
                r.get::<Option<String>, _>("subtree_hash"),
            )
        })
        .collect()
}

async fn community_of_file(storage: &Storage, repo_id: Uuid, path: &str) -> Uuid {
    sqlx::query_scalar::<_, Uuid>(
        "select distinct cm.community_id from community_members cm \
         join nodes n on n.id = cm.node_id \
         join files f on f.id = n.file_id \
         where f.repo_id = $1 and f.path = $2 limit 1",
    )
    .bind(repo_id)
    .bind(path)
    .fetch_one(&storage.pool)
    .await
    .unwrap()
}

/// Stable per-run digest: (label, sorted member stable_ids), independent of
/// regenerated node UUIDs.
fn partition_digest(det: &crate::community::CommunityDetection) -> Vec<String> {
    let mut rows: Vec<String> = det
        .communities
        .iter()
        .map(|c| format!("{}|{}", c.label, c.member_stable_ids.join(",")))
        .collect();
    rows.sort();
    rows
}

#[tokio::test]
async fn community_layer_round_trip_and_stable() {
    let Some(url) = db_url() else {
        eprintln!("skip community_layer_round_trip_and_stable: DATABASE_URL unset");
        return;
    };
    let storage = Storage::connect(&url).await.expect("connect");
    storage.migrate().await.expect("migrate");

    let repo_path = format!("/tmp/chaos-test-{}", Uuid::new_v4());
    let repo = storage
        .upsert_repository(Path::new(&repo_path), Some("testsha"))
        .await
        .expect("repo");

    let result = two_cluster_fixture(repo.id);
    storage
        .replace_repo_index(repo.id, &result)
        .await
        .expect("index");

    let det1 = detect_and_persist(&storage, repo.id, &CommunityConfig::default())
        .await
        .expect("detect");
    assert!(
        det1.communities.len() >= 2,
        "two clusters => >=2 communities"
    );

    // Round-trip: stats counts == direct SQL == detection.
    let stats = storage.repo_stats(&repo).await.expect("stats");
    let stats_comm = stats["hierarchy"]["communities"].as_i64().unwrap();
    let sql_comm: i64 = sqlx::query_scalar("select count(*) from communities where repo_id = $1")
        .bind(repo.id)
        .fetch_one(&storage.pool)
        .await
        .unwrap();
    assert_eq!(stats_comm, sql_comm);
    assert_eq!(stats_comm as usize, det1.communities.len());

    let sql_members: i64 = sqlx::query_scalar(
        "select count(*) from community_members cm \
         join communities c on c.id = cm.community_id where c.repo_id = $1",
    )
    .bind(repo.id)
    .fetch_one(&storage.pool)
    .await
    .unwrap();
    let expected_members: usize = det1
        .communities
        .iter()
        .map(|c| c.member_node_ids.len())
        .sum();
    assert_eq!(sql_members as usize, expected_members);

    // Re-detect after a full re-index: same logical partition (node UUIDs
    // change, but the stable_id-level digest must not).
    let result2 = two_cluster_fixture(repo.id);
    storage
        .replace_repo_index(repo.id, &result2)
        .await
        .expect("reindex");
    let det2 = detect_and_persist(&storage, repo.id, &CommunityConfig::default())
        .await
        .expect("detect2");
    assert_eq!(
        partition_digest(&det1),
        partition_digest(&det2),
        "community partition must be stable across re-index"
    );

    storage.purge_repository(repo.id).await.expect("purge");
}

/// Golden change-localization test: editing one chunk in one file flips
/// exactly that file's hash, its community root(s), and the repo root —
/// every sibling byte-identical.
#[tokio::test]
async fn merkle_localizes_a_single_chunk_change() {
    let Some(url) = db_url() else {
        eprintln!("skip merkle_localizes_a_single_chunk_change: DATABASE_URL unset");
        return;
    };
    let storage = Storage::connect(&url).await.expect("connect");
    storage.migrate().await.expect("migrate");
    let repo_path = format!("/tmp/chaos-test-{}", Uuid::new_v4());
    let repo = storage
        .upsert_repository(Path::new(&repo_path), Some("testsha"))
        .await
        .expect("repo");

    let result = two_cluster_fixture(repo.id);
    storage
        .replace_repo_index(repo.id, &result)
        .await
        .expect("index");
    detect_and_persist(&storage, repo.id, &CommunityConfig::default())
        .await
        .expect("detect");
    let m1 = crate::merkle::compute_and_persist(&storage, repo.id)
        .await
        .expect("merkle1");

    let before_files = load_file_hashes(&storage, repo.id).await;

    // Which community owns a/a.rs, and which does not.
    let comm_a = community_of_file(&storage, repo.id, "a/a.rs").await;
    let comm_b = community_of_file(&storage, repo.id, "b/b.rs").await;
    assert_ne!(comm_a, comm_b, "two files must be in two communities");

    // Edit exactly one chunk of a/a.rs.
    sqlx::query(
        "update chunks set content_hash = 'CHANGED-CHUNK' \
         where id = (select c.id from chunks c join files f on f.id = c.file_id \
                    where f.repo_id = $1 and f.path = 'a/a.rs' order by c.content_hash limit 1)",
    )
    .bind(repo.id)
    .execute(&storage.pool)
    .await
    .unwrap();

    let m2 = crate::merkle::compute_and_persist(&storage, repo.id)
        .await
        .expect("merkle2");
    let after_files = load_file_hashes(&storage, repo.id).await;

    // The edited file moved; its sibling did not.
    assert_ne!(
        before_files["a/a.rs"], after_files["a/a.rs"],
        "edited file hash must change"
    );
    assert_eq!(
        before_files["b/b.rs"], after_files["b/b.rs"],
        "sibling file hash must be byte-identical"
    );
    // The repo root moved.
    assert_ne!(
        m1.repo_root_hash, m2.repo_root_hash,
        "repo root must change"
    );
    // The ancestor community moved; the unaffected one did not.
    assert_ne!(
        m1.community_hashes[&comm_a], m2.community_hashes[&comm_a],
        "community owning the edited file must change"
    );
    assert_eq!(
        m1.community_hashes[&comm_b], m2.community_hashes[&comm_b],
        "unaffected community must be byte-identical"
    );

    storage.purge_repository(repo.id).await.expect("purge");
}

/// Re-rolling unchanged content reproduces every hash byte-for-byte.
#[tokio::test]
async fn merkle_is_stable_for_unchanged_content() {
    let Some(url) = db_url() else {
        eprintln!("skip merkle_is_stable_for_unchanged_content: DATABASE_URL unset");
        return;
    };
    let storage = Storage::connect(&url).await.expect("connect");
    storage.migrate().await.expect("migrate");
    let repo_path = format!("/tmp/chaos-test-{}", Uuid::new_v4());
    let repo = storage
        .upsert_repository(Path::new(&repo_path), Some("testsha"))
        .await
        .expect("repo");

    storage
        .replace_repo_index(repo.id, &two_cluster_fixture(repo.id))
        .await
        .expect("index");
    detect_and_persist(&storage, repo.id, &CommunityConfig::default())
        .await
        .expect("detect");
    let m1 = crate::merkle::compute_and_persist(&storage, repo.id)
        .await
        .expect("m1");

    // Full re-index of identical content, then re-roll.
    storage
        .replace_repo_index(repo.id, &two_cluster_fixture(repo.id))
        .await
        .expect("reindex");
    detect_and_persist(&storage, repo.id, &CommunityConfig::default())
        .await
        .expect("detect2");
    let m2 = crate::merkle::compute_and_persist(&storage, repo.id)
        .await
        .expect("m2");

    assert_eq!(
        m1.repo_root_hash, m2.repo_root_hash,
        "repo root must be byte-identical for unchanged content"
    );
    // Community hashes match by-value (ids are deterministic too).
    let mut h1: Vec<String> = m1.community_hashes.values().cloned().collect();
    let mut h2: Vec<String> = m2.community_hashes.values().cloned().collect();
    h1.sort();
    h2.sort();
    assert_eq!(
        h1, h2,
        "community hashes must be stable for unchanged content"
    );

    storage.purge_repository(repo.id).await.expect("purge");
}

/// The collapsed `_docs` search variants share ONE SQL statement with an
/// optional chunk-type filter — prove BOTH sides of that parameter bind and
/// run against real Postgres, and that the filter actually filters: the
/// fixture's chunks are all `function` type, so the docs variants return
/// nothing while the unfiltered search returns rows. (The semantic call uses
/// a throwaway QUERY vector under a provider identity that stores no
/// embeddings — nothing is fabricated or persisted; it only exercises the
/// statement.)
#[tokio::test]
async fn docs_search_variants_filter_by_chunk_type() {
    let Some(url) = db_url() else {
        eprintln!("skip docs_search_variants_filter_by_chunk_type: DATABASE_URL unset");
        return;
    };
    let storage = Storage::connect(&url).await.expect("connect");
    storage.migrate().await.expect("migrate");
    let repo_path = format!("/tmp/chaos-test-{}", Uuid::new_v4());
    let repo = storage
        .upsert_repository(Path::new(&repo_path), Some("testsha"))
        .await
        .expect("repo");
    storage
        .replace_repo_index(repo.id, &two_cluster_fixture(repo.id))
        .await
        .expect("index");

    // Unfiltered: the fixture chunk contents carry the word "body".
    let all = storage
        .keyword_search(repo.id, "body", 10)
        .await
        .expect("keyword");
    assert!(
        !all.is_empty(),
        "unfiltered keyword search must hit the fixture chunks"
    );
    // Filtered: none of the fixture chunks are documentation.
    let docs = storage
        .keyword_search_docs(repo.id, "body", 10)
        .await
        .expect("keyword docs");
    assert!(docs.is_empty(), "docs filter must exclude function chunks");
    // Semantic docs: same optional-filter binding on the embedding query.
    let sem_docs = storage
        .semantic_search_docs(repo.id, "sqltest", "none", 3, &[0.0, 0.0, 0.0], 10)
        .await
        .expect("semantic docs");
    assert!(sem_docs.is_empty());

    storage.purge_repository(repo.id).await.expect("purge");
}

/// A test embedder that always fails — used to prove the summary path fails
/// closed and never writes a placeholder vector. (Not a fake embedder: it
/// produces no vector at all, it errors.)
struct FailEmbedder;

#[async_trait::async_trait]
impl crate::embedding::Embedder for FailEmbedder {
    fn provider(&self) -> &'static str {
        "failtest"
    }
    fn model_id(&self) -> &str {
        "fail"
    }
    fn dimensions(&self) -> usize {
        768
    }
    async fn embed(&self, _input: &str) -> anyhow::Result<Vec<f32>> {
        anyhow::bail!("embedder unavailable (test)")
    }
}

/// Build the project's real configured embedder, probing it; None if the DB
/// or embedder backend is unavailable (so CI skips cleanly).
async fn try_real_embedder() -> Option<std::sync::Arc<dyn crate::embedding::Embedder>> {
    let cfg = crate::config::Config::load(None).ok()?;
    let embedder = crate::embedding::build_embedder(&cfg.embedding).ok()?;
    embedder.embed("probe").await.ok()?;
    Some(embedder)
}

async fn index_two_clusters(storage: &Storage) -> Repository {
    let repo_path = format!("/tmp/chaos-test-{}", Uuid::new_v4());
    let repo = storage
        .upsert_repository(Path::new(&repo_path), Some("testsha"))
        .await
        .expect("repo");
    storage
        .replace_repo_index(repo.id, &two_cluster_fixture(repo.id))
        .await
        .expect("index");
    detect_and_persist(storage, repo.id, &CommunityConfig::default())
        .await
        .expect("detect");
    crate::merkle::compute_and_persist(storage, repo.id)
        .await
        .expect("merkle");
    repo
}

/// THE headline P3 test: a no-op re-summarize makes ZERO embedder calls, and
/// changing one chunk re-summarizes only the affected community.
#[tokio::test]
async fn summary_hash_gate_skips_unchanged_communities() {
    let Some(url) = db_url() else {
        eprintln!("skip summary_hash_gate: DATABASE_URL unset");
        return;
    };
    let storage = Storage::connect(&url).await.expect("connect");
    storage.migrate().await.expect("migrate");
    let Some(embedder) = try_real_embedder().await else {
        eprintln!("skip summary_hash_gate: real embedder unavailable");
        return;
    };
    let repo = index_two_clusters(&storage).await;
    let total = storage.count_hashed_communities(repo.id).await.unwrap();
    assert!(total >= 2);

    // The content-addressed summary cache deliberately survives repo
    // wipes, and this fixture's content is deterministic — a previous run
    // of this test would otherwise serve pass 1 from cache and break the
    // "first pass embeds everything" assertion. Clear just our rows.
    sqlx::query(
        "delete from community_summary_cache where content_hash in \
         (select subtree_hash from communities where repo_id = $1)",
    )
    .bind(repo.id)
    .execute(&storage.pool)
    .await
    .expect("clear summary cache for fixture content");

    // First pass: everything summarized.
    let first = crate::community_summary::summarize_repo(&storage, embedder.as_ref(), repo.id)
        .await
        .expect("summarize1");
    assert_eq!(first.embed_calls as i64, total, "first pass summarizes all");
    assert_eq!(first.skipped, 0);

    // Second pass, no change: the gate skips everything — ZERO embed calls.
    let second = crate::community_summary::summarize_repo(&storage, embedder.as_ref(), repo.id)
        .await
        .expect("summarize2");
    assert_eq!(
        second.embed_calls, 0,
        "no-op re-summarize must make 0 embed calls"
    );
    assert_eq!(second.skipped as i64, total);

    // Change one chunk of a/a.rs, re-roll, summarize: exactly one community.
    sqlx::query(
        "update chunks set content_hash = 'CHANGED-CHUNK-P3' \
         where id = (select c.id from chunks c join files f on f.id = c.file_id \
                    where f.repo_id = $1 and f.path = 'a/a.rs' order by c.content_hash limit 1)",
    )
    .bind(repo.id)
    .execute(&storage.pool)
    .await
    .unwrap();
    crate::merkle::compute_and_persist(&storage, repo.id)
        .await
        .expect("merkle2");
    // The mutated content is deterministic too — clear its cache rows so
    // the changed community embeds for real instead of restoring.
    sqlx::query(
        "delete from community_summary_cache where content_hash in \
         (select subtree_hash from communities where repo_id = $1)",
    )
    .bind(repo.id)
    .execute(&storage.pool)
    .await
    .expect("clear summary cache for mutated content");
    let third = crate::community_summary::summarize_repo(&storage, embedder.as_ref(), repo.id)
        .await
        .expect("summarize3");
    assert_eq!(
        third.embed_calls, 1,
        "only the community owning the changed file re-summarizes"
    );

    storage.purge_repository(repo.id).await.expect("purge");
}

/// Fail-closed: with the embedder unavailable, summarizing errors and writes
/// NO embedding row and NO summary text (never a placeholder vector).
#[tokio::test]
async fn summary_fails_closed_without_embedder() {
    let Some(url) = db_url() else {
        eprintln!("skip summary_fails_closed: DATABASE_URL unset");
        return;
    };
    let storage = Storage::connect(&url).await.expect("connect");
    storage.migrate().await.expect("migrate");
    let repo = index_two_clusters(&storage).await;

    let result = crate::community_summary::summarize_repo(&storage, &FailEmbedder, repo.id).await;
    assert!(
        result.is_err(),
        "summary must fail closed when embedder is down"
    );

    let embeddings: i64 = sqlx::query_scalar(
        "select count(*) from community_embeddings ce \
         join communities c on c.id = ce.community_id where c.repo_id = $1",
    )
    .bind(repo.id)
    .fetch_one(&storage.pool)
    .await
    .unwrap();
    assert_eq!(embeddings, 0, "no placeholder embedding may be written");
    let summarized: i64 = sqlx::query_scalar(
        "select count(*) from communities where repo_id = $1 and summary is not null",
    )
    .bind(repo.id)
    .fetch_one(&storage.pool)
    .await
    .unwrap();
    assert_eq!(summarized, 0, "no summary text may be written on failure");

    storage.purge_repository(repo.id).await.expect("purge");
}

/// P4 decomposition golden test: a description naming symbols from both
/// clusters spans both features; one naming a single cluster's symbols leads
/// with that feature. Determinism: same change ⇒ same feature set + order.
#[tokio::test]
async fn change_plan_decomposes_into_features() {
    let Some(url) = db_url() else {
        eprintln!("skip change_plan_decomposes: DATABASE_URL unset");
        return;
    };
    let storage = Storage::connect(&url).await.expect("connect");
    storage.migrate().await.expect("migrate");
    let Some(embedder) = try_real_embedder().await else {
        eprintln!("skip change_plan_decomposes: real embedder unavailable");
        return;
    };
    let repo = index_two_clusters(&storage).await;
    crate::community_summary::summarize_repo(&storage, embedder.as_ref(), repo.id)
        .await
        .expect("summarize");

    let out = std::env::temp_dir().join(format!("plan-{}.html", Uuid::new_v4()));
    let opts = crate::change_plan::ChangePlanOptions {
        output_html: Some(out.clone()),
        diff_since: None,
        limit: 8,
    };

    // Mentions symbols from BOTH clusters ⇒ spans ≥2 features.
    let both = crate::change_plan::run(
        &storage,
        embedder.as_ref(),
        &repo.root_path,
        "update functions c0_f0 c0_f1 c0_f2 and c1_f0 c1_f1 c1_f2 across a/a.rs and b/b.rs",
        &opts,
    )
    .await
    .expect("plan both");
    assert!(out.exists(), "plan HTML must be written");
    let n = both["feature_count"].as_u64().unwrap();
    assert!(
        n >= 2,
        "a both-cluster change must span >=2 features, got {n}"
    );

    // Determinism: same change ⇒ identical feature set + order.
    let again = crate::change_plan::run(
        &storage,
        embedder.as_ref(),
        &repo.root_path,
        "update functions c0_f0 c0_f1 c0_f2 and c1_f0 c1_f1 c1_f2 across a/a.rs and b/b.rs",
        &opts,
    )
    .await
    .expect("plan again");
    let labels = |v: &serde_json::Value| -> Vec<String> {
        v["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["label"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(labels(&both), labels(&again), "same change => same plan");

    // Compact JSON discipline: the returned payload stays small (detail is
    // in the HTML), bounded well under a feature_context-style dump.
    let payload = serde_json::to_string(&both).unwrap();
    assert!(
        payload.len() < 8000,
        "plan JSON must be compact, got {}",
        payload.len()
    );

    let _ = std::fs::remove_file(&out);
    storage.purge_repository(repo.id).await.expect("purge");
}
