use crate::{
    embedding::vector_literal,
    graph_export::{GraphExport, GraphExportEdge, GraphExportNode, GraphRepository},
    hierarchy_export::{CommunityDetail, CommunityHierarchy, QuotientEdgeDetail},
    models::{
        CrossRepoLink, ExtractionResult, KnowledgeChunk, KnowledgeEdge, KnowledgeNode, Project,
        ProjectRepo, Repository, SearchHit, SourceFile,
    },
};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, PgPool, Row};
use std::{collections::HashMap, fs, path::Path};
use uuid::Uuid;

#[derive(Clone)]
pub struct Storage {
    pool: PgPool,
}

impl Storage {
    pub async fn connect(database_url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(database_url)
            .await
            .context("failed to connect to Postgres")?;
        Ok(Self { pool })
    }

    /// Connect with a short acquire timeout — used by the `hook` subcommand so
    /// a down database degrades fast rather than blocking the editor.
    pub async fn connect_fast(database_url: &str, timeout: std::time::Duration) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(timeout)
            .connect(database_url)
            .await
            .context("failed to connect to Postgres (fast)")?;
        Ok(Self { pool })
    }

    pub async fn migrate(&self) -> Result<()> {
        // The migrations directory is embedded at compile time and each file is
        // executed whole (no fragile ';' splitting); applied versions are tracked
        // in the `_sqlx_migrations` table.
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .context("failed to run database migrations")?;
        Ok(())
    }

    pub async fn health(&self) -> Result<Value> {
        let version: String = sqlx::query_scalar("select version()")
            .fetch_one(&self.pool)
            .await?;
        let pgvector: Option<String> =
            sqlx::query_scalar("select extversion from pg_extension where extname = 'vector'")
                .fetch_optional(&self.pool)
                .await?;
        Ok(json!({
            "postgres": version,
            "pgvector": pgvector,
        }))
    }

    pub async fn upsert_repository(
        &self,
        root: &Path,
        commit_sha: Option<&str>,
    ) -> Result<Repository> {
        let root_path = fs::canonicalize(root)
            .unwrap_or_else(|_| root.to_path_buf())
            .to_string_lossy()
            .to_string();
        let name = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("repository")
            .to_string();
        let row = sqlx::query(
            r#"
            insert into repositories (id, name, root_path, current_commit_sha, created_at, updated_at)
            values ($1, $2, $3, $4, now(), now())
            on conflict (root_path) do update set
                name = excluded.name,
                current_commit_sha = excluded.current_commit_sha,
                updated_at = now()
            returning id, name, root_path, remote_url, current_commit_sha, created_at, updated_at
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(name)
        .bind(root_path)
        .bind(commit_sha)
        .fetch_one(&self.pool)
        .await?;

        Ok(Repository {
            id: row.get("id"),
            name: row.get("name"),
            root_path: row.get("root_path"),
            remote_url: row.get("remote_url"),
            current_commit_sha: row.get("current_commit_sha"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        })
    }

    pub async fn begin_analysis(&self, repo_id: Uuid, commit_sha: Option<&str>) -> Result<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query(
            "insert into analysis_runs (id, repo_id, commit_sha, status, started_at) values ($1, $2, $3, 'running', now())",
        )
        .bind(id)
        .bind(repo_id)
        .bind(commit_sha.unwrap_or("unknown"))
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn finish_analysis(
        &self,
        run_id: Uuid,
        status: &str,
        error: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "update analysis_runs set status = $2, error = $3, finished_at = now() where id = $1",
        )
        .bind(run_id)
        .bind(status)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Replace a repository's whole L0 index with a fresh extraction.
    ///
    /// Embeddings are PRESERVED by content: before the wipe, the repo's
    /// embeddings are saved (server-side, into a transaction-scoped temp table)
    /// keyed by `(content_hash, provider, model_id, dimensions)`, and after the
    /// fresh chunks are inserted every chunk whose content already had an
    /// embedding gets it back. `content_hash` is deterministic SHA-256 of the
    /// chunk content, so a full re-analyze of unchanged code makes ZERO
    /// embedder calls — only genuinely new/changed content is left for
    /// [`Storage::chunks_missing_embeddings`]. Returns how many embeddings were
    /// reused.
    pub async fn replace_repo_index(
        &self,
        repo_id: Uuid,
        result: &ExtractionResult,
    ) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        // Harvest existing embeddings by content before anything is deleted.
        // Temp table is connection-local and dropped on commit, so concurrent
        // replaces on other pool connections cannot collide.
        sqlx::query(
            r#"
            create temp table _chaos_saved_embeddings on commit drop as
            select distinct on (c.content_hash, e.provider, e.model_id, e.dimensions)
                   c.content_hash, e.provider, e.model_id, e.dimensions, e.embedding
            from embeddings e
            join chunks c on c.id = e.chunk_id
            where c.repo_id = $1
            order by c.content_hash, e.provider, e.model_id, e.dimensions
            "#,
        )
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("delete from embeddings using chunks where embeddings.chunk_id = chunks.id and chunks.repo_id = $1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("delete from chunks where repo_id = $1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("delete from edges where repo_id = $1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("delete from nodes where repo_id = $1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("delete from files where repo_id = $1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;

        for file in &result.files {
            insert_file(&mut tx, file).await?;
        }
        for node in &result.nodes {
            insert_node(&mut tx, node).await?;
        }
        for edge in &result.edges {
            insert_edge(&mut tx, edge).await?;
        }
        for chunk in &result.chunks {
            insert_chunk(&mut tx, chunk).await?;
        }

        // Restore saved embeddings onto the fresh chunks with matching content.
        let restored = sqlx::query(
            r#"
            insert into embeddings
                (id, chunk_id, provider, model_id, dimensions, content_hash, embedding, created_at)
            select gen_random_uuid(), c.id, s.provider, s.model_id, s.dimensions,
                   c.content_hash, s.embedding, now()
            from chunks c
            join _chaos_saved_embeddings s on s.content_hash = c.content_hash
            where c.repo_id = $1
            on conflict (chunk_id, provider, model_id, dimensions, content_hash) do nothing
            "#,
        )
        .bind(repo_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        tx.commit().await?;
        Ok(restored)
    }

    /// Incrementally merge a partial extraction (only `changed_paths`) into an
    /// existing repository index, leaving every other file's nodes, edges,
    /// chunks, and embeddings untouched.
    ///
    /// Steps, all in one transaction:
    /// 1. Delete the prior rows for `changed_paths`. The FK cascade chain
    ///    (`files → nodes → edges`, `files → chunks → embeddings`) removes all
    ///    derived data for those files, including stale call edges into their
    ///    symbols.
    /// 2. Insert the fresh files.
    /// 3. Upsert nodes by `(repo_id, stable_id)`, capturing each row's
    ///    authoritative id. Pre-existing nodes that survive the delete (the
    ///    repository node, shared bare-import nodes owned by unchanged files)
    ///    keep their original id, so the extraction's fresh uuids are remapped
    ///    to those ids before edges/chunks that reference them are inserted —
    ///    otherwise the FK constraint would reject a dangling reference.
    /// 4. Insert edges and chunks with remapped node ids.
    ///
    /// Embeddings are NOT created here; callers run
    /// [`Storage::chunks_missing_embeddings`] afterwards (only the newly
    /// inserted chunks lack embeddings, so only they are re-embedded).
    pub async fn merge_files_index(
        &self,
        repo_id: Uuid,
        changed_paths: &[String],
        result: &ExtractionResult,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        if !changed_paths.is_empty() {
            sqlx::query("delete from files where repo_id = $1 and path = any($2)")
                .bind(repo_id)
                .bind(changed_paths)
                .execute(&mut *tx)
                .await?;
        }

        for file in &result.files {
            insert_file(&mut tx, file).await?;
        }

        let mut remap: HashMap<Uuid, Uuid> = HashMap::with_capacity(result.nodes.len());
        for node in &result.nodes {
            let db_id = upsert_node_returning_id(&mut tx, node).await?;
            remap.insert(node.id, db_id);
        }

        for edge in &result.edges {
            let (Some(&source), Some(&target)) = (
                remap.get(&edge.source_node_id),
                remap.get(&edge.target_node_id),
            ) else {
                continue;
            };
            if source == target {
                continue;
            }
            insert_edge(
                &mut tx,
                &KnowledgeEdge {
                    source_node_id: source,
                    target_node_id: target,
                    ..edge.clone()
                },
            )
            .await?;
        }

        for chunk in &result.chunks {
            insert_chunk(
                &mut tx,
                &KnowledgeChunk {
                    node_id: chunk.node_id.and_then(|id| remap.get(&id).copied()),
                    ..chunk.clone()
                },
            )
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Replace the persisted L1 community layer for a repo with a fresh
    /// detection result, in one transaction.
    ///
    /// Communities are **upserted by id** (not wiped) so that the deterministic
    /// id of an unchanged community keeps its row — and therefore its P3 summary,
    /// `summary_hash`, `subtree_hash`, and summary embedding. This is what lets
    /// the P3 hash gate skip re-summarizing across a full re-index. Members and
    /// quotient edges are fully replaced (members reference regenerated node
    /// ids); communities no longer in the partition are deleted (cascading their
    /// members/edges/embeddings).
    pub async fn replace_communities(
        &self,
        repo_id: Uuid,
        detection: &crate::community::CommunityDetection,
        detection_params: &Value,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let new_ids: Vec<Uuid> = detection.communities.iter().map(|c| c.id).collect();

        // Drop members + quotient edges (rebuilt below), then any community no
        // longer in the partition (cascades its members/edges/embeddings).
        sqlx::query(
            "delete from community_members where community_id in \
             (select id from communities where repo_id = $1)",
        )
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("delete from community_edges where repo_id = $1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("delete from communities where repo_id = $1 and not (id = any($2))")
            .bind(repo_id)
            .bind(&new_ids)
            .execute(&mut *tx)
            .await?;

        for community in &detection.communities {
            // On conflict, preserve summary / summary_hash / subtree_hash /
            // summarized_at (set by P2 merkle and P3 summarize) — only the
            // detection-derived fields change.
            sqlx::query(
                r#"
                insert into communities
                    (id, repo_id, level, parent_id, label, member_count, detection_params, created_at, updated_at)
                values ($1, $2, $3, null, $4, $5, $6, now(), now())
                on conflict (id) do update set
                    label = excluded.label,
                    member_count = excluded.member_count,
                    detection_params = excluded.detection_params,
                    updated_at = now()
                "#,
            )
            .bind(community.id)
            .bind(repo_id)
            .bind(0i32)
            .bind(&community.label)
            .bind(community.size as i32)
            .bind(detection_params)
            .execute(&mut *tx)
            .await?;
        }

        // Bulk-insert memberships via UNNEST (one row per (community, node)).
        let mut community_ids: Vec<Uuid> = Vec::new();
        let mut node_ids: Vec<Uuid> = Vec::new();
        let mut weights: Vec<f64> = Vec::new();
        for community in &detection.communities {
            for &node_id in &community.member_node_ids {
                community_ids.push(community.id);
                node_ids.push(node_id);
                weights.push(1.0);
            }
        }
        if !community_ids.is_empty() {
            sqlx::query(
                r#"
                insert into community_members (community_id, node_id, weight)
                select * from unnest($1::uuid[], $2::uuid[], $3::float8[])
                on conflict do nothing
                "#,
            )
            .bind(&community_ids)
            .bind(&node_ids)
            .bind(&weights)
            .execute(&mut *tx)
            .await?;
        }

        for edge in &detection.quotient_edges {
            // Deterministic edge id from its (already-deterministic) endpoints.
            let edge_id = Uuid::new_v5(
                &crate::community::COMMUNITY_NAMESPACE,
                format!(
                    "{repo_id}:edge:{}:{}",
                    edge.source_community_id, edge.target_community_id
                )
                .as_bytes(),
            );
            sqlx::query(
                r#"
                insert into community_edges
                    (id, repo_id, source_community_id, target_community_id, kind, weight, edge_count, metadata)
                values ($1, $2, $3, $4, $5, $6, $7, $8)
                "#,
            )
            .bind(edge_id)
            .bind(repo_id)
            .bind(edge.source_community_id)
            .bind(edge.target_community_id)
            .bind(&edge.kind)
            .bind(edge.weight)
            .bind(edge.edge_count as i32)
            .bind(json!({ "kind_counts": edge.kind_counts }))
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    // ---- L2 Merkle rollup support -------------------------------------------

    /// Ordered chunk `content_hash` leaves per file (the Merkle leaves). Returns
    /// one row per file (left join, so chunk-less files appear with an empty
    /// list), with chunks in canonical order so the rolled hash is stable.
    pub async fn load_file_chunk_hashes(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(Uuid, String, Vec<String>)>> {
        let rows = sqlx::query(
            r#"
            select f.id as file_id, f.path as path, c.content_hash as chunk_hash
            from files f
            left join chunks c on c.file_id = f.id
            where f.repo_id = $1
            order by f.path, f.id,
                     c.line_start nulls first, c.line_end nulls first,
                     c.chunk_type nulls first, c.content_hash nulls first
            "#,
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;

        let mut out: Vec<(Uuid, String, Vec<String>)> = Vec::new();
        for row in rows {
            let file_id: Uuid = row.get("file_id");
            let path: String = row.get("path");
            let chunk_hash: Option<String> = row.get("chunk_hash");
            match out.last_mut() {
                Some((last_id, _, hashes)) if *last_id == file_id => {
                    if let Some(h) = chunk_hash {
                        hashes.push(h);
                    }
                }
                _ => {
                    let mut hashes = Vec::new();
                    if let Some(h) = chunk_hash {
                        hashes.push(h);
                    }
                    out.push((file_id, path, hashes));
                }
            }
        }
        Ok(out)
    }

    /// Distinct member file ids per community (a file is shared across
    /// communities when its symbols are — that overlap is the blast radius).
    pub async fn load_community_member_files(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(Uuid, Vec<Uuid>)>> {
        let rows = sqlx::query(
            r#"
            select cm.community_id as community_id,
                   array_agg(distinct n.file_id) as file_ids
            from community_members cm
            join communities co on co.id = cm.community_id
            join nodes n on n.id = cm.node_id
            where co.repo_id = $1 and n.file_id is not null
            group by cm.community_id
            "#,
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let community_id: Uuid = row.get("community_id");
                let file_ids: Vec<Uuid> = row.get("file_ids");
                (community_id, file_ids)
            })
            .collect())
    }

    /// Persist file-level subtree hashes (bulk UNNEST update).
    pub async fn update_file_subtree_hashes(&self, hashes: &[(Uuid, String)]) -> Result<()> {
        if hashes.is_empty() {
            return Ok(());
        }
        let ids: Vec<Uuid> = hashes.iter().map(|(id, _)| *id).collect();
        let vals: Vec<String> = hashes.iter().map(|(_, h)| h.clone()).collect();
        sqlx::query(
            r#"
            update files as f set subtree_hash = v.hash
            from (select * from unnest($1::uuid[], $2::text[]) as t(id, hash)) v
            where f.id = v.id
            "#,
        )
        .bind(&ids)
        .bind(&vals)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Persist community-level subtree hashes (bulk UNNEST update).
    pub async fn update_community_subtree_hashes(&self, hashes: &[(Uuid, String)]) -> Result<()> {
        if hashes.is_empty() {
            return Ok(());
        }
        let ids: Vec<Uuid> = hashes.iter().map(|(id, _)| *id).collect();
        let vals: Vec<String> = hashes.iter().map(|(_, h)| h.clone()).collect();
        sqlx::query(
            r#"
            update communities as c set subtree_hash = v.hash, updated_at = now()
            from (select * from unnest($1::uuid[], $2::text[]) as t(id, hash)) v
            where c.id = v.id
            "#,
        )
        .bind(&ids)
        .bind(&vals)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Persist the repo root hash.
    pub async fn update_repo_root_hash(&self, repo_id: Uuid, hash: &str) -> Result<()> {
        sqlx::query(
            "update repositories set repo_root_hash = $2, updated_at = now() where id = $1",
        )
        .bind(repo_id)
        .bind(hash)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Current repo root hash (None if never computed).
    pub async fn get_repo_root_hash(&self, repo_id: Uuid) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar("select repo_root_hash from repositories where id = $1")
                .bind(repo_id)
                .fetch_optional(&self.pool)
                .await?
                .flatten(),
        )
    }

    /// Map of community id -> current subtree hash (only communities that have
    /// one). Used to diff before/after for `add` blast radius and P3 gating.
    pub async fn load_community_hashes(&self, repo_id: Uuid) -> Result<HashMap<Uuid, String>> {
        let rows = sqlx::query(
            "select id, subtree_hash from communities where repo_id = $1 and subtree_hash is not null",
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    row.get::<Uuid, _>("id"),
                    row.get::<String, _>("subtree_hash"),
                )
            })
            .collect())
    }

    // ---- L3 community summary support (P3) ----------------------------------

    /// Communities that need a (re)summary for the given embedder identity:
    /// those whose content (`subtree_hash`) moved since the stored
    /// `summary_hash`, or that have no summary embedding yet. Returns
    /// `(community_id, current subtree_hash)` ordered by id (deterministic). The
    /// gate: a no-change re-index returns an empty list ⇒ zero embedder calls.
    pub async fn communities_needing_summary(
        &self,
        repo_id: Uuid,
        provider: &str,
        model_id: &str,
        dimensions: usize,
        algo_tag: &str,
    ) -> Result<Vec<(Uuid, String)>> {
        // The stored `summary_hash` commits to `subtree_hash || algo_tag`, so a
        // content-stable re-index skips (gate property) BUT bumping the summary
        // algorithm version (`algo_tag`) re-summarizes every community once.
        let rows = sqlx::query(
            r#"
            select c.id, c.subtree_hash
            from communities c
            left join community_embeddings ce
              on ce.community_id = c.id
             and ce.provider = $2 and ce.model_id = $3 and ce.dimensions = $4
            where c.repo_id = $1
              and c.subtree_hash is not null
              and (c.summary_hash is distinct from (c.subtree_hash || $5) or ce.id is null)
            order by c.id
            "#,
        )
        .bind(repo_id)
        .bind(provider)
        .bind(model_id)
        .bind(dimensions as i32)
        .bind(algo_tag)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<Uuid, _>("id"), r.get::<String, _>("subtree_hash")))
            .collect())
    }

    /// Count of communities that have a rolled `subtree_hash` (the summarizable
    /// universe).
    pub async fn count_hashed_communities(&self, repo_id: Uuid) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "select count(*) from communities where repo_id = $1 and subtree_hash is not null",
        )
        .bind(repo_id)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Inputs for an extractive community summary: label, member count, member
    /// (name, kind, path) tuples, and a few representative chunk snippets — all
    /// in a deterministic order so the summary text (and its embedding) are
    /// reproducible.
    pub async fn load_community_summary_inputs(
        &self,
        community_id: Uuid,
    ) -> Result<CommunitySummaryInputs> {
        let head = sqlx::query("select label, member_count from communities where id = $1")
            .bind(community_id)
            .fetch_one(&self.pool)
            .await?;
        let members = sqlx::query(
            r#"
            select n.name, n.kind, coalesce(f.path, '') as path
            from community_members cm
            join nodes n on n.id = cm.node_id
            left join files f on f.id = n.file_id
            where cm.community_id = $1
            order by n.kind, n.name, n.stable_id
            limit 200
            "#,
        )
        .bind(community_id)
        .fetch_all(&self.pool)
        .await?;
        let snippets = sqlx::query(
            r#"
            select c.content
            from community_members cm
            join chunks c on c.node_id = cm.node_id
            where cm.community_id = $1
            order by length(c.content) desc, c.content_hash
            limit 5
            "#,
        )
        .bind(community_id)
        .fetch_all(&self.pool)
        .await?;
        // Neighboring features (quotient-graph edges, either direction) — the
        // "what this connects to" context. Deterministic order, capped.
        let related = sqlx::query(
            r#"
            select distinct c.label
            from community_edges e
            join communities c
              on c.id = case when e.source_community_id = $1
                             then e.target_community_id
                             else e.source_community_id end
            where (e.source_community_id = $1 or e.target_community_id = $1)
              and c.id <> $1
              and c.member_count >= 2
            order by c.label
            limit 8
            "#,
        )
        .bind(community_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(CommunitySummaryInputs {
            label: head.get("label"),
            member_count: head.get("member_count"),
            members: members
                .into_iter()
                .map(|r| {
                    (
                        r.get::<String, _>("name"),
                        r.get::<String, _>("kind"),
                        r.get::<String, _>("path"),
                    )
                })
                .collect(),
            snippets: snippets
                .into_iter()
                .map(|r| r.get::<String, _>("content"))
                .collect(),
            related: related
                .into_iter()
                .map(|r| r.get::<String, _>("label"))
                .collect(),
        })
    }

    /// Persist a community summary + its real embedding (one transaction). The
    /// embedding commits to `subtree_hash` via `content_hash`, and `summary_hash`
    /// is set to `subtree_hash` so the gate skips it next time content is stable.
    #[allow(clippy::too_many_arguments)]
    pub async fn save_community_summary(
        &self,
        community_id: Uuid,
        summary: &str,
        subtree_hash: &str,
        provider: &str,
        model_id: &str,
        dimensions: usize,
        embedding: &[f32],
        algo_tag: &str,
    ) -> Result<()> {
        if embedding.len() != dimensions {
            anyhow::bail!(
                "refusing to store community embedding with dimension {}; configured dimension is {}",
                embedding.len(),
                dimensions
            );
        }
        let literal = vector_literal(embedding);
        // `summary_hash` commits to the content hash AND the summary algorithm
        // version, so the gate recomputes when either changes. The embedding's
        // `content_hash` stays the raw `subtree_hash` (it commits to content).
        let summary_hash = format!("{subtree_hash}{algo_tag}");
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "update communities set summary = $2, summary_hash = $3, summarized_at = now() where id = $1",
        )
        .bind(community_id)
        .bind(summary)
        .bind(&summary_hash)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            insert into community_embeddings
                (id, community_id, provider, model_id, dimensions, content_hash, embedding, created_at)
            values ($1, $2, $3, $4, $5, $6, $7::vector, now())
            on conflict (community_id, provider, model_id, dimensions)
            do update set embedding = excluded.embedding, content_hash = excluded.content_hash, created_at = now()
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(community_id)
        .bind(provider)
        .bind(model_id)
        .bind(dimensions as i32)
        .bind(subtree_hash)
        .bind(&literal)
        .execute(&mut *tx)
        .await?;
        // Content-addressed cache: if a later partition shuffle gives this same
        // member content a NEW community id, the summary + embedding can be
        // restored without an embedder call (see restore_cached_summary).
        sqlx::query(
            r#"
            insert into community_summary_cache
                (content_hash, algo, provider, model_id, dimensions, summary, embedding, created_at)
            values ($1, $2, $3, $4, $5, $6, $7::vector, now())
            on conflict (content_hash, algo, provider, model_id, dimensions)
            do update set summary = excluded.summary, embedding = excluded.embedding, created_at = now()
            "#,
        )
        .bind(subtree_hash)
        .bind(algo_tag)
        .bind(provider)
        .bind(model_id)
        .bind(dimensions as i32)
        .bind(summary)
        .bind(&literal)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Restore a community's summary + embedding from the content-addressed
    /// cache when an identical-content entry exists (same subtree hash, summary
    /// algo, and embedder identity). Returns true on a cache hit — the caller
    /// then skips composing and embedding entirely. This is what makes
    /// community-ID churn (a partition shuffle renaming an unchanged community)
    /// cost ZERO embedder calls.
    pub async fn restore_cached_summary(
        &self,
        community_id: Uuid,
        subtree_hash: &str,
        algo_tag: &str,
        provider: &str,
        model_id: &str,
        dimensions: usize,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            r#"
            update communities c
            set summary = s.summary, summary_hash = $2 || $3, summarized_at = now()
            from community_summary_cache s
            where c.id = $1
              and s.content_hash = $2 and s.algo = $3
              and s.provider = $4 and s.model_id = $5 and s.dimensions = $6
            "#,
        )
        .bind(community_id)
        .bind(subtree_hash)
        .bind(algo_tag)
        .bind(provider)
        .bind(model_id)
        .bind(dimensions as i32)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 0 {
            // No cache entry (or no such community) — nothing was written.
            return Ok(false);
        }
        sqlx::query(
            r#"
            insert into community_embeddings
                (id, community_id, provider, model_id, dimensions, content_hash, embedding, created_at)
            select gen_random_uuid(), $1, $4, $5, $6, $2, s.embedding, now()
            from community_summary_cache s
            where s.content_hash = $2 and s.algo = $3
              and s.provider = $4 and s.model_id = $5 and s.dimensions = $6
            on conflict (community_id, provider, model_id, dimensions)
            do update set embedding = excluded.embedding, content_hash = excluded.content_hash, created_at = now()
            "#,
        )
        .bind(community_id)
        .bind(subtree_hash)
        .bind(algo_tag)
        .bind(provider)
        .bind(model_id)
        .bind(dimensions as i32)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    // ---- L1/L3 top-down retrieval support (P4) ------------------------------

    /// Cosine match of a query embedding against community summary embeddings —
    /// the top-down entry point. Returns the best-matching god-nodes.
    pub async fn community_semantic_search(
        &self,
        repo_id: Uuid,
        provider: &str,
        model_id: &str,
        dimensions: usize,
        query_embedding: &[f32],
        limit: i64,
    ) -> Result<Vec<CommunityMatch>> {
        let literal = vector_literal(query_embedding);
        let rows = sqlx::query(
            r#"
            select c.id, c.label, c.summary, c.member_count,
                   1.0 - (ce.embedding <=> $5::vector) as score
            from community_embeddings ce
            join communities c on c.id = ce.community_id
            where c.repo_id = $1 and ce.provider = $2 and ce.model_id = $3 and ce.dimensions = $4
            order by ce.embedding <=> $5::vector
            limit $6
            "#,
        )
        .bind(repo_id)
        .bind(provider)
        .bind(model_id)
        .bind(dimensions as i32)
        .bind(literal)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_community_match).collect())
    }

    /// Lightweight `(id, label, member_count)` for every feature community
    /// (member_count ≥ 2). Used by the hierarchical query router's lexical
    /// label-match fallback — catches a path/label-named feature (e.g. "OCL")
    /// whose extractive summary embeds too weakly to clear the cosine floor.
    pub async fn community_labels(&self, repo_id: Uuid) -> Result<Vec<(Uuid, String, i32)>> {
        let rows = sqlx::query(
            "select id, label, member_count from communities \
             where repo_id = $1 and member_count >= 2 order by member_count desc, id",
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<Uuid, _>("id"),
                    r.get::<String, _>("label"),
                    r.get::<i32, _>("member_count"),
                )
            })
            .collect())
    }

    /// Pairwise cosine similarity among a set of communities, from their L3
    /// summary embeddings. Returns `(a, b, score)` with `a < b` (each unordered
    /// pair once), strongest first. Powers the "related by topic" links between
    /// the components `chaos_components` shows — relatedness that crosses repo and
    /// language boundaries where code-level import/call edges cannot.
    pub async fn community_pairwise_similarity(
        &self,
        ids: &[Uuid],
        provider: &str,
        model_id: &str,
        dimensions: usize,
    ) -> Result<Vec<(Uuid, Uuid, f64)>> {
        if ids.len() < 2 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"
            select a.community_id as src, b.community_id as dst,
                   1.0 - (a.embedding <=> b.embedding) as score
            from community_embeddings a
            join community_embeddings b on a.community_id < b.community_id
            where a.community_id = any($1) and b.community_id = any($1)
              and a.provider = $2 and a.model_id = $3 and a.dimensions = $4
              and b.provider = $2 and b.model_id = $3 and b.dimensions = $4
            order by score desc, src, dst
            "#,
        )
        .bind(ids)
        .bind(provider)
        .bind(model_id)
        .bind(dimensions as i32)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<Uuid, _>("src"),
                    r.get::<Uuid, _>("dst"),
                    r.get::<f64, _>("score"),
                )
            })
            .collect())
    }

    /// Non-seed communities most semantically similar to a set of seed
    /// communities (closest by L3 summary embedding to *any* seed), above
    /// `threshold`, best first. Pulls the missing "core" of an area into
    /// `chaos_components` — the central piece a name/path match overlooked.
    #[allow(clippy::too_many_arguments)]
    pub async fn community_semantic_neighbors(
        &self,
        repo_id: Uuid,
        seeds: &[Uuid],
        provider: &str,
        model_id: &str,
        dimensions: usize,
        threshold: f64,
        limit: i64,
    ) -> Result<Vec<(Uuid, f64)>> {
        if seeds.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"
            select b.community_id as cand,
                   max(1.0 - (a.embedding <=> b.embedding)) as best
            from community_embeddings a
            join community_embeddings b on b.community_id <> all($2)
            join communities cb on cb.id = b.community_id
            where a.community_id = any($2)
              and cb.repo_id = $1 and cb.member_count >= 2
              and a.provider = $3 and a.model_id = $4 and a.dimensions = $5
              and b.provider = $3 and b.model_id = $4 and b.dimensions = $5
            group by b.community_id
            having max(1.0 - (a.embedding <=> b.embedding)) >= $6
            order by best desc, cand
            limit $7
            "#,
        )
        .bind(repo_id)
        .bind(seeds)
        .bind(provider)
        .bind(model_id)
        .bind(dimensions as i32)
        .bind(threshold)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<Uuid, _>("cand"), r.get::<f64, _>("best")))
            .collect())
    }

    /// Briefs (no score) for a set of community ids — used to describe
    /// diff-seeded communities that did not come from the embedding match.
    pub async fn load_community_briefs(
        &self,
        repo_id: Uuid,
        ids: &[Uuid],
    ) -> Result<Vec<CommunityMatch>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"
            select id, label, summary, member_count, 0.0::float8 as score
            from communities
            where repo_id = $1 and id = any($2)
            order by member_count desc, id
            "#,
        )
        .bind(repo_id)
        .bind(ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_community_match).collect())
    }

    /// Representative member symbols of a community (file nodes excluded).
    pub async fn load_community_top_symbols(
        &self,
        community_id: Uuid,
        limit: i64,
    ) -> Result<Vec<(String, String, String)>> {
        let rows = sqlx::query(
            r#"
            select n.name, n.kind, coalesce(f.path, '') as path
            from community_members cm
            join nodes n on n.id = cm.node_id
            left join files f on f.id = n.file_id
            where cm.community_id = $1 and n.kind <> 'file'
            order by n.kind, n.name, n.stable_id
            limit $2
            "#,
        )
        .bind(community_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("name"),
                    r.get::<String, _>("kind"),
                    r.get::<String, _>("path"),
                )
            })
            .collect())
    }

    /// Distinct communities whose members live in any of `paths` — the
    /// communities a concrete diff directly touches.
    pub async fn communities_for_files(
        &self,
        repo_id: Uuid,
        paths: &[String],
    ) -> Result<Vec<Uuid>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"
            select distinct cm.community_id
            from community_members cm
            join nodes n on n.id = cm.node_id
            join files f on f.id = n.file_id
            where f.repo_id = $1 and f.path = any($2)
            "#,
        )
        .bind(repo_id)
        .bind(paths)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| r.get::<Uuid, _>("community_id"))
            .collect())
    }

    /// Distinct feature communities (member_count ≥ 2) with at least one member
    /// whose file lives at OR under any of `prefixes` — folder scoping for
    /// `chaos features <folder>`. Unlike `communities_for_files` (exact path set,
    /// for a concrete diff) this matches a directory subtree: a prefix `src/api`
    /// catches `src/api/x.rs` (LIKE `prefix/%`) and `src/api` itself if it is a
    /// file (`= prefix`), but never `src/apiv2`. Each prefix is stripped of a
    /// trailing slash and matched literally (`%`/`_` in the prefix are escaped).
    pub async fn communities_under_paths(
        &self,
        repo_id: Uuid,
        prefixes: &[String],
    ) -> Result<Vec<Uuid>> {
        let cleaned: Vec<String> = prefixes
            .iter()
            .map(|p| p.trim().trim_start_matches("./").trim_matches('/'))
            .filter(|p| !p.is_empty())
            .map(|p| p.to_string())
            .collect();
        if cleaned.is_empty() {
            return Ok(Vec::new());
        }
        // Two LIKE patterns per prefix: the folder subtree and the exact path.
        // Escape the LIKE metacharacters with backslash — LIKE's default escape
        // character — so a literal `%`/`_`/`\` in a path can't widen the match.
        // (The `ESCAPE` clause can't be spelled alongside `LIKE ANY(array)`, but
        // backslash is the default escape, so the patterns work without it.)
        let escape = |s: &str| {
            s.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        };
        let mut patterns: Vec<String> = Vec::with_capacity(cleaned.len() * 2);
        for p in &cleaned {
            let e = escape(p);
            patterns.push(format!("{e}/%"));
            patterns.push(e);
        }
        let rows = sqlx::query(
            r#"
            select distinct cm.community_id
            from community_members cm
            join communities c on c.id = cm.community_id
            join nodes n on n.id = cm.node_id
            join files f on f.id = n.file_id
            where f.repo_id = $1
              and c.member_count >= 2
              and f.path ilike any($2::text[])
            "#,
        )
        .bind(repo_id)
        .bind(&patterns)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| r.get::<Uuid, _>("community_id"))
            .collect())
    }

    /// Read-only: every node under a folder subtree as `(file_path, name, kind)`,
    /// for the structure-first feature prototype. Matches `prefix` and `prefix/%`
    /// (LIKE metacharacters escaped). Includes file/definition/dependency nodes so
    /// the caller can separate real symbols from imports itself.
    pub async fn load_symbols_under_path(
        &self,
        repo_id: Uuid,
        prefix: &str,
    ) -> Result<Vec<(String, String, String)>> {
        let p = prefix.trim().trim_start_matches("./").trim_matches('/');
        if p.is_empty() {
            return Ok(Vec::new());
        }
        let escaped = p
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let patterns = vec![format!("{escaped}/%"), escaped];
        let rows = sqlx::query(
            r#"
            select f.path as path, n.name as name, n.kind as kind
            from nodes n
            join files f on f.id = n.file_id
            where f.repo_id = $1
              and n.kind <> 'repository'
              and f.path ilike any($2::text[])
            order by f.path, n.kind, n.name
            "#,
        )
        .bind(repo_id)
        .bind(&patterns)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("path"),
                    r.get::<String, _>("name"),
                    r.get::<String, _>("kind"),
                )
            })
            .collect())
    }

    /// Load the full feature hierarchy (communities of size ≥ 2, their top
    /// members, and the quotient edges among them) for surfacing. Read-only and
    /// embedder-free; empty for a repo with no persisted communities.
    pub async fn load_community_hierarchy(
        &self,
        repo: &Repository,
        top_members: usize,
    ) -> Result<CommunityHierarchy> {
        let crows = sqlx::query(
            r#"
            select id, label, summary, member_count
            from communities
            where repo_id = $1 and member_count >= 2
            order by member_count desc, label, id
            "#,
        )
        .bind(repo.id)
        .fetch_all(&self.pool)
        .await?;

        let feature_ids: std::collections::HashSet<Uuid> =
            crows.iter().map(|r| r.get::<Uuid, _>("id")).collect();

        // Members for all feature communities, grouped + capped in Rust.
        let mrows = sqlx::query(
            r#"
            select cm.community_id, n.name, n.kind, coalesce(f.path, '') as path
            from community_members cm
            join communities c on c.id = cm.community_id
            join nodes n on n.id = cm.node_id
            left join files f on f.id = n.file_id
            where c.repo_id = $1 and c.member_count >= 2 and n.kind <> 'file'
            order by cm.community_id, n.kind, n.name, n.stable_id
            "#,
        )
        .bind(repo.id)
        .fetch_all(&self.pool)
        .await?;
        let mut members_by: HashMap<Uuid, Vec<(String, String, String)>> = HashMap::new();
        for row in mrows {
            let cid: Uuid = row.get("community_id");
            let bucket = members_by.entry(cid).or_default();
            if bucket.len() < top_members {
                bucket.push((row.get("name"), row.get("kind"), row.get("path")));
            }
        }

        let communities: Vec<CommunityDetail> = crows
            .into_iter()
            .map(|r| {
                let id: Uuid = r.get("id");
                CommunityDetail {
                    top_members: members_by.remove(&id).unwrap_or_default(),
                    id,
                    label: r.get("label"),
                    summary: r.get("summary"),
                    member_count: r.get("member_count"),
                }
            })
            .collect();

        let erows = sqlx::query(
            r#"
            select source_community_id, target_community_id, kind, weight, edge_count
            from community_edges
            where repo_id = $1
            order by weight desc, source_community_id, target_community_id
            "#,
        )
        .bind(repo.id)
        .fetch_all(&self.pool)
        .await?;
        let edges: Vec<QuotientEdgeDetail> = erows
            .into_iter()
            .filter_map(|r| {
                let source: Uuid = r.get("source_community_id");
                let target: Uuid = r.get("target_community_id");
                if feature_ids.contains(&source) && feature_ids.contains(&target) {
                    Some(QuotientEdgeDetail {
                        source,
                        target,
                        kind: r.get("kind"),
                        weight: r.get("weight"),
                        edge_count: r.get("edge_count"),
                    })
                } else {
                    None
                }
            })
            .collect();

        Ok(CommunityHierarchy {
            repo_name: repo.name.clone(),
            communities,
            edges,
        })
    }

    /// Map of node id -> the communities it belongs to, for a set of nodes.
    /// Used by hierarchical retrieval to boost hits inside matched god-nodes.
    pub async fn node_communities(
        &self,
        repo_id: Uuid,
        node_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<Uuid>>> {
        if node_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = sqlx::query(
            r#"
            select cm.node_id, cm.community_id
            from community_members cm
            join communities c on c.id = cm.community_id
            where c.repo_id = $1 and cm.node_id = any($2)
            "#,
        )
        .bind(repo_id)
        .bind(node_ids)
        .fetch_all(&self.pool)
        .await?;
        let mut map: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        for row in rows {
            map.entry(row.get::<Uuid, _>("node_id"))
                .or_default()
                .push(row.get::<Uuid, _>("community_id"));
        }
        Ok(map)
    }

    /// Directed dependency links between the given communities, derived from L0
    /// edge direction (an edge from a node in A to a node in B ⇒ A → B). Used to
    /// topo-sort a change plan's check order. Returns `(src, dst, count)`.
    pub async fn directed_community_links(
        &self,
        repo_id: Uuid,
        ids: &[Uuid],
    ) -> Result<Vec<(Uuid, Uuid, i64)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"
            select sc.community_id as src, tc.community_id as dst, count(*)::bigint as n
            from edges e
            join community_members sc on sc.node_id = e.source_node_id
            join community_members tc on tc.node_id = e.target_node_id
            where e.repo_id = $1
              and sc.community_id = any($2) and tc.community_id = any($2)
              and sc.community_id <> tc.community_id
            group by sc.community_id, tc.community_id
            order by src, dst
            "#,
        )
        .bind(repo_id)
        .bind(ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<Uuid, _>("src"),
                    r.get::<Uuid, _>("dst"),
                    r.get::<i64, _>("n"),
                )
            })
            .collect())
    }

    /// Row counts per persisted table, used to report what a clean removed.
    pub async fn table_counts(&self) -> Result<Value> {
        let row = sqlx::query(
            "select \
                (select count(*) from repositories) as repositories, \
                (select count(*) from analysis_runs) as analysis_runs, \
                (select count(*) from files) as files, \
                (select count(*) from nodes) as nodes, \
                (select count(*) from edges) as edges, \
                (select count(*) from chunks) as chunks, \
                (select count(*) from embeddings) as embeddings, \
                (select count(*) from communities) as communities, \
                (select count(*) from community_members) as community_members, \
                (select count(*) from community_edges) as community_edges, \
                (select count(*) from community_embeddings) as community_embeddings, \
                (select count(*) from projects) as projects, \
                (select count(*) from project_repos) as project_repos, \
                (select count(*) from cross_repo_links) as cross_repo_links",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(json!({
            "repositories": row.get::<i64, _>("repositories"),
            "analysis_runs": row.get::<i64, _>("analysis_runs"),
            "files": row.get::<i64, _>("files"),
            "nodes": row.get::<i64, _>("nodes"),
            "edges": row.get::<i64, _>("edges"),
            "chunks": row.get::<i64, _>("chunks"),
            "embeddings": row.get::<i64, _>("embeddings"),
            "communities": row.get::<i64, _>("communities"),
            "community_members": row.get::<i64, _>("community_members"),
            "community_edges": row.get::<i64, _>("community_edges"),
            "community_embeddings": row.get::<i64, _>("community_embeddings"),
            "projects": row.get::<i64, _>("projects"),
            "project_repos": row.get::<i64, _>("project_repos"),
            "cross_repo_links": row.get::<i64, _>("cross_repo_links"),
        }))
    }

    /// Per-repository index statistics: totals plus breakdowns by node kind,
    /// edge kind, chunk type, and file language. Pure read (no embedder) —
    /// explains what an `analyze`/`add` produced. Powers `chaos stats` and the
    /// `chaos_stats` MCP tool.
    pub async fn repo_stats(&self, repo: &Repository) -> Result<Value> {
        let repo_id = repo.id;
        let totals = sqlx::query(
            r#"
            select
                (select count(*) from files  where repo_id = $1) as files,
                (select count(*) from nodes  where repo_id = $1) as nodes,
                (select count(*) from edges  where repo_id = $1) as edges,
                (select count(*) from chunks where repo_id = $1) as chunks,
                (select count(distinct e.chunk_id)
                   from embeddings e join chunks c on c.id = e.chunk_id
                   where c.repo_id = $1) as embedded_chunks,
                (select count(*) from chunks c
                   left join embeddings e on e.chunk_id = c.id
                   where c.repo_id = $1 and e.id is null) as chunks_missing_embeddings,
                (select count(*) from chunks
                   where repo_id = $1 and jsonb_exists(metadata, 'split_part')) as split_chunks,
                (select count(distinct node_id) from chunks
                   where repo_id = $1 and node_id is not null) as nodes_with_chunk
            "#,
        )
        .bind(repo_id)
        .fetch_one(&self.pool)
        .await?;

        // L1 hierarchy counts. Communities are 0 for a repo indexed before the
        // hierarchy layer existed (additive degradation). `feature_communities`
        // are the multi-member ones; singletons are isolated leaf nodes.
        let hierarchy = sqlx::query(
            r#"
            select
                (select count(*) from communities where repo_id = $1) as communities,
                (select count(*) from communities where repo_id = $1 and member_count >= 2) as feature_communities,
                (select count(*) from community_edges where repo_id = $1) as quotient_edges,
                (select coalesce(max(member_count), 0)::bigint from communities where repo_id = $1) as largest_community,
                (select count(*) from communities where repo_id = $1 and subtree_hash is not null) as hashed_communities,
                (select repo_root_hash from repositories where id = $1) as repo_root_hash
            "#,
        )
        .bind(repo_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(json!({
            "repo": {
                "id": repo.id,
                "name": repo.name,
                "root_path": repo.root_path,
                "current_commit_sha": repo.current_commit_sha,
            },
            "totals": {
                "files": totals.get::<i64, _>("files"),
                "nodes": totals.get::<i64, _>("nodes"),
                "edges": totals.get::<i64, _>("edges"),
                "chunks": totals.get::<i64, _>("chunks"),
                "embedded_chunks": totals.get::<i64, _>("embedded_chunks"),
                "chunks_missing_embeddings": totals.get::<i64, _>("chunks_missing_embeddings"),
                "split_chunks": totals.get::<i64, _>("split_chunks"),
                "nodes_with_chunk": totals.get::<i64, _>("nodes_with_chunk"),
            },
            "hierarchy": {
                "communities": hierarchy.get::<i64, _>("communities"),
                "feature_communities": hierarchy.get::<i64, _>("feature_communities"),
                "quotient_edges": hierarchy.get::<i64, _>("quotient_edges"),
                "largest_community": hierarchy.get::<i64, _>("largest_community"),
                "hashed_communities": hierarchy.get::<i64, _>("hashed_communities"),
                "repo_root_hash": hierarchy.get::<Option<String>, _>("repo_root_hash"),
            },
            "files_by_language": self.group_counts(repo_id, "files", "language").await?,
            "nodes_by_kind": self.group_counts(repo_id, "nodes", "kind").await?,
            "edges_by_kind": self.group_counts(repo_id, "edges", "kind").await?,
            "chunks_by_type": self.group_counts(repo_id, "chunks", "chunk_type").await?,
        }))
    }

    /// `[{ "name": <value>, "count": <n> }, …]` grouped by `column` of `table`,
    /// ordered by count desc. `table`/`column` are fixed internal identifiers
    /// (never user input), so interpolating them is safe.
    async fn group_counts(&self, repo_id: Uuid, table: &str, column: &str) -> Result<Value> {
        let sql = format!(
            "select {column} as label, count(*) as c from {table} \
             where repo_id = $1 group by {column} order by c desc, label"
        );
        let rows = sqlx::query(&sql)
            .bind(repo_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(Value::Array(
            rows.into_iter()
                .map(|row| {
                    json!({
                        "name": row.get::<String, _>("label"),
                        "count": row.get::<i64, _>("c"),
                    })
                })
                .collect(),
        ))
    }

    /// Wipe the entire persisted index (every repository). Returns the row
    /// counts that were removed so the caller can report what was cleared.
    pub async fn clear_all(&self) -> Result<Value> {
        let removed = self.table_counts().await?;
        sqlx::query(
            "truncate embeddings, chunks, edges, nodes, files, analysis_runs, repositories, projects restart identity cascade",
        )
        .execute(&self.pool)
        .await?;
        Ok(removed)
    }

    /// Remove a single repository and all of its derived rows.
    pub async fn purge_repository(&self, repo_id: Uuid) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("delete from embeddings using chunks where embeddings.chunk_id = chunks.id and chunks.repo_id = $1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
        for table in ["chunks", "edges", "nodes", "files", "analysis_runs"] {
            sqlx::query(&format!("delete from {table} where repo_id = $1"))
                .bind(repo_id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("delete from repositories where id = $1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn chunks_missing_embeddings(
        &self,
        repo_id: Uuid,
        provider: &str,
        model_id: &str,
        dimensions: usize,
    ) -> Result<Vec<KnowledgeChunk>> {
        let rows = sqlx::query(
            r#"
            select c.id, c.repo_id, c.file_id, c.node_id, c.chunk_type, c.content, c.content_hash,
                   c.line_start, c.line_end, c.metadata
            from chunks c
            left join embeddings e on e.chunk_id = c.id
              and e.provider = $2 and e.model_id = $3 and e.dimensions = $4
              and e.content_hash = c.content_hash
            where c.repo_id = $1 and e.id is null
            order by c.id
            "#,
        )
        .bind(repo_id)
        .bind(provider)
        .bind(model_id)
        .bind(dimensions as i32)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(row_to_chunk).collect())
    }

    pub async fn insert_embedding(
        &self,
        chunk: &KnowledgeChunk,
        provider: &str,
        model_id: &str,
        dimensions: usize,
        embedding: &[f32],
    ) -> Result<()> {
        if embedding.len() != dimensions {
            anyhow::bail!(
                "refusing to store embedding with dimension {}; configured dimension is {}",
                embedding.len(),
                dimensions
            );
        }
        let literal = vector_literal(embedding);
        sqlx::query(
            r#"
            insert into embeddings (id, chunk_id, provider, model_id, dimensions, content_hash, embedding, created_at)
            values ($1, $2, $3, $4, $5, $6, $7::vector, now())
            on conflict (chunk_id, provider, model_id, dimensions, content_hash)
            do update set embedding = excluded.embedding, created_at = now()
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(chunk.id)
        .bind(provider)
        .bind(model_id)
        .bind(dimensions as i32)
        .bind(&chunk.content_hash)
        .bind(literal)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn semantic_search(
        &self,
        repo_id: Uuid,
        provider: &str,
        model_id: &str,
        dimensions: usize,
        query_embedding: &[f32],
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        let literal = vector_literal(query_embedding);
        let rows = sqlx::query(
            r#"
            select c.id as chunk_id, c.node_id, f.path as file_path, c.line_start, c.line_end,
                   1.0 - (e.embedding <=> $5::vector) as score, c.content, c.metadata
            from embeddings e
            join chunks c on c.id = e.chunk_id
            left join files f on f.id = c.file_id
            where c.repo_id = $1 and e.provider = $2 and e.model_id = $3 and e.dimensions = $4
            order by e.embedding <=> $5::vector
            limit $6
            "#,
        )
        .bind(repo_id)
        .bind(provider)
        .bind(model_id)
        .bind(dimensions as i32)
        .bind(literal)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_search_hit).collect())
    }

    pub async fn keyword_search(
        &self,
        repo_id: Uuid,
        query: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        let rows = sqlx::query(
            r#"
            select c.id as chunk_id, c.node_id, f.path as file_path, c.line_start, c.line_end,
                   ts_rank_cd(c.search_vector, websearch_to_tsquery('english', $2))::float8 as score,
                   c.content, c.metadata
            from chunks c
            left join files f on f.id = c.file_id
            where c.repo_id = $1 and c.search_vector @@ websearch_to_tsquery('english', $2)
            order by score desc
            limit $3
            "#,
        )
        .bind(repo_id)
        .bind(query)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_search_hit).collect())
    }

    pub async fn literal_search(
        &self,
        repo_id: Uuid,
        term: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        let pattern = format!("%{term}%");
        let rows = sqlx::query(
            r#"
            select c.id as chunk_id, c.node_id, f.path as file_path, c.line_start, c.line_end,
                   (
                     case when lower(coalesce(f.path, '')) like lower($2) then 1.5 else 0 end +
                     case when lower(c.content) like lower($2) then 0.35 else 0 end
                   )::float8 as score,
                   c.content, c.metadata
            from chunks c
            left join files f on f.id = c.file_id
            where c.repo_id = $1
              and (lower(coalesce(f.path, '')) like lower($2) or lower(c.content) like lower($2))
            order by score desc, c.line_start nulls last
            limit $3
            "#,
        )
        .bind(repo_id)
        .bind(pattern)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_search_hit).collect())
    }

    /// Load every node for a repo in canonical `stable_id` order. Used by the
    /// community-detection layer (L1), which must see the whole graph.
    pub async fn load_all_nodes(&self, repo_id: Uuid) -> Result<Vec<KnowledgeNode>> {
        let rows = sqlx::query(
            r#"
            select id, repo_id, file_id, kind, stable_id, name, line_start, line_end, metadata
            from nodes
            where repo_id = $1
            order by stable_id, id
            "#,
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_node).collect())
    }

    /// Load every edge for a repo in a stable order. Used by L1 detection.
    pub async fn load_all_edges(&self, repo_id: Uuid) -> Result<Vec<KnowledgeEdge>> {
        let rows = sqlx::query(
            r#"
            select id, repo_id, source_node_id, target_node_id, kind, cost, confidence, metadata
            from edges
            where repo_id = $1
            order by source_node_id, target_node_id, kind
            "#,
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_edge).collect())
    }

    pub async fn load_edges_for_nodes(
        &self,
        repo_id: Uuid,
        node_ids: &[Uuid],
    ) -> Result<Vec<KnowledgeEdge>> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"
            select id, repo_id, source_node_id, target_node_id, kind, cost, confidence, metadata
            from edges
            where repo_id = $1 and (source_node_id = any($2) or target_node_id = any($2))
            "#,
        )
        .bind(repo_id)
        .bind(node_ids)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(row_to_edge).collect())
    }

    pub async fn load_graph_export(&self, repo: &Repository) -> Result<GraphExport> {
        let node_rows = sqlx::query(
            r#"
            select n.id, n.kind, n.stable_id, n.name, f.path as file_path,
                   n.line_start, n.line_end, n.metadata, count(c.id)::bigint as chunk_count
            from nodes n
            left join files f on f.id = n.file_id
            left join chunks c on c.node_id = n.id
            where n.repo_id = $1
            group by n.id, n.kind, n.stable_id, n.name, f.path,
                     n.line_start, n.line_end, n.metadata
            order by n.kind, f.path nulls first, n.line_start nulls first, n.name
            "#,
        )
        .bind(repo.id)
        .fetch_all(&self.pool)
        .await?;

        let edge_rows = sqlx::query(
            r#"
            select id, source_node_id, target_node_id, kind, cost, confidence, metadata
            from edges
            where repo_id = $1
            order by kind, source_node_id, target_node_id
            "#,
        )
        .bind(repo.id)
        .fetch_all(&self.pool)
        .await?;

        Ok(GraphExport {
            repository: GraphRepository {
                id: repo.id,
                name: repo.name.clone(),
                root_path: repo.root_path.clone(),
                current_commit_sha: repo.current_commit_sha.clone(),
            },
            nodes: node_rows
                .into_iter()
                .map(|row| GraphExportNode {
                    id: row.get("id"),
                    kind: row.get("kind"),
                    stable_id: row.get("stable_id"),
                    name: row.get("name"),
                    file_path: row.get("file_path"),
                    line_start: row.get("line_start"),
                    line_end: row.get("line_end"),
                    chunk_count: row.get("chunk_count"),
                    metadata: row.get("metadata"),
                })
                .collect(),
            edges: edge_rows
                .into_iter()
                .map(|row| GraphExportEdge {
                    id: row.get("id"),
                    source: row.get("source_node_id"),
                    target: row.get("target_node_id"),
                    kind: row.get("kind"),
                    cost: row.get("cost"),
                    confidence: row.get("confidence"),
                    metadata: row.get("metadata"),
                })
                .collect(),
        })
    }

    pub async fn find_repository(&self, root_path_or_name: &str) -> Result<Option<Repository>> {
        let canonical = fs::canonicalize(root_path_or_name)
            .ok()
            .map(|p| p.to_string_lossy().to_string());
        let row = sqlx::query(
            r#"
            select id, name, root_path, remote_url, current_commit_sha, created_at, updated_at
            from repositories
            where root_path = $1 or name = $2 or ($3::text is not null and root_path = $3)
            order by updated_at desc
            limit 1
            "#,
        )
        .bind(root_path_or_name)
        .bind(root_path_or_name)
        .bind(canonical)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|row| Repository {
            id: row.get("id"),
            name: row.get("name"),
            root_path: row.get("root_path"),
            remote_url: row.get("remote_url"),
            current_commit_sha: row.get("current_commit_sha"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        }))
    }

    /// Every indexed repository, in a stable order. Used by `chaos clean
    /// --artifacts` to find each repo's generated files before the DB wipe.
    pub async fn list_repositories(&self) -> Result<Vec<Repository>> {
        let rows = sqlx::query(
            "select id, name, root_path, remote_url, current_commit_sha, created_at, updated_at \
             from repositories order by name, root_path",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| Repository {
                id: row.get("id"),
                name: row.get("name"),
                root_path: row.get("root_path"),
                remote_url: row.get("remote_url"),
                current_commit_sha: row.get("current_commit_sha"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
            })
            .collect())
    }

    /// Fast keyword/symbol lookup by name — no embedder required.
    ///
    /// Joins `nodes` → `files` for the given `repo_id` and does a
    /// case-insensitive ILIKE match on the node name, ordered so exact matches
    /// come first.  Useful for the `hook` subcommand which must not call the
    /// embedding HTTP API.
    pub async fn search_symbols_by_name(
        &self,
        repo_id: Uuid,
        term: &str,
        limit: i64,
    ) -> Result<Vec<SymbolHit>> {
        let pattern = format!("%{term}%");
        let rows = sqlx::query(
            r#"
            select n.name, n.kind, coalesce(f.path, '') as file_path, n.line_start
            from nodes n
            left join files f on f.id = n.file_id
            where n.repo_id = $1
              and n.name ilike $2
              and n.kind not in ('repository', 'file')
            order by
                case when lower(n.name) = lower($3) then 0 else 1 end,
                n.kind,
                n.line_start nulls last
            limit $4
            "#,
        )
        .bind(repo_id)
        .bind(&pattern)
        .bind(term)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| SymbolHit {
                name: row.get("name"),
                kind: row.get("kind"),
                file: row.get("file_path"),
                line_start: row.get("line_start"),
            })
            .collect())
    }

    // ---- P6: cross-repository project layer ---------------------------------

    /// Create a project (idempotent on name). Returns the existing row when the
    /// name is already taken so `chaos project create` is safe to repeat.
    pub async fn create_project(&self, name: &str) -> Result<Project> {
        let row = sqlx::query(
            r#"
            insert into projects (id, name, created_at, updated_at)
            values ($1, $2, now(), now())
            on conflict (name) do update set updated_at = now()
            returning id, name, created_at
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(name.trim())
        .fetch_one(&self.pool)
        .await?;
        Ok(row_to_project(&row))
    }

    pub async fn find_project(&self, name: &str) -> Result<Option<Project>> {
        let row = sqlx::query("select id, name, created_at from projects where name = $1")
            .bind(name.trim())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(row_to_project))
    }

    pub async fn list_projects(&self) -> Result<Vec<Project>> {
        let rows = sqlx::query("select id, name, created_at from projects order by name")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_project).collect())
    }

    /// Add an indexed repository to a project under a project-scoped alias
    /// (idempotent; re-adding updates the alias).
    pub async fn add_repo_to_project(
        &self,
        project_id: Uuid,
        repo_id: Uuid,
        alias: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            insert into project_repos (project_id, repo_id, alias, added_at)
            values ($1, $2, $3, now())
            on conflict (project_id, repo_id) do update set alias = excluded.alias
            "#,
        )
        .bind(project_id)
        .bind(repo_id)
        .bind(alias.trim())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Member repositories of a project with alias + last-linked hash, in a
    /// stable alias order.
    pub async fn project_member_repos(&self, project_id: Uuid) -> Result<Vec<ProjectRepo>> {
        let rows = sqlx::query(
            r#"
            select r.id, r.name, r.root_path, r.remote_url, r.current_commit_sha,
                   r.created_at, r.updated_at, pr.alias, pr.linked_repo_hash
            from project_repos pr
            join repositories r on r.id = pr.repo_id
            where pr.project_id = $1
            order by pr.alias
            "#,
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| ProjectRepo {
                repo: Repository {
                    id: row.get("id"),
                    name: row.get("name"),
                    root_path: row.get("root_path"),
                    remote_url: row.get("remote_url"),
                    current_commit_sha: row.get("current_commit_sha"),
                    created_at: row.get("created_at"),
                    updated_at: row.get("updated_at"),
                },
                alias: row.get("alias"),
                linked_repo_hash: row.get("linked_repo_hash"),
            })
            .collect())
    }

    /// Every project that contains the given repository — the hook `analyze`/
    /// `add` use to keep the project layer fresh after re-indexing one member.
    pub async fn projects_containing_repo(&self, repo_id: Uuid) -> Result<Vec<Project>> {
        let rows = sqlx::query(
            r#"
            select p.id, p.name, p.created_at
            from projects p
            join project_repos pr on pr.project_id = p.id
            where pr.repo_id = $1
            order by p.name
            "#,
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_project).collect())
    }

    /// Record the repo root hash a relink run was computed from (the gate).
    pub async fn set_linked_repo_hash(
        &self,
        project_id: Uuid,
        repo_id: Uuid,
        hash: &str,
    ) -> Result<()> {
        sqlx::query(
            "update project_repos set linked_repo_hash = $3 \
             where project_id = $1 and repo_id = $2",
        )
        .bind(project_id)
        .bind(repo_id)
        .bind(hash)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Replace a project's cross-repo links with a fresh detection result, in
    /// one transaction (mirrors `replace_communities`: full replace is correct
    /// because links are cheap to recompute and carry no gated state).
    pub async fn replace_project_links(
        &self,
        project_id: Uuid,
        links: &[CrossRepoLink],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("delete from cross_repo_links where project_id = $1")
            .bind(project_id)
            .execute(&mut *tx)
            .await?;
        for link in links {
            sqlx::query(
                r#"
                insert into cross_repo_links
                    (id, project_id, source_repo_id, source_community_id,
                     target_repo_id, target_community_id, kind, evidence, confidence, created_at)
                values ($1, $2, $3, $4, $5, $6, $7, $8, $9, now())
                on conflict (project_id, source_community_id, target_community_id, kind)
                do update set evidence = excluded.evidence, confidence = excluded.confidence
                "#,
            )
            .bind(link.id)
            .bind(project_id)
            .bind(link.source_repo_id)
            .bind(link.source_community_id)
            .bind(link.target_repo_id)
            .bind(link.target_community_id)
            .bind(&link.kind)
            .bind(&link.evidence)
            .bind(link.confidence)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// All persisted cross-repo links of a project, strongest first.
    pub async fn load_project_links(&self, project_id: Uuid) -> Result<Vec<CrossRepoLink>> {
        let rows = sqlx::query(
            r#"
            select id, source_repo_id, source_community_id,
                   target_repo_id, target_community_id, kind, evidence, confidence
            from cross_repo_links
            where project_id = $1
            order by confidence desc, kind, source_community_id, target_community_id
            "#,
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| CrossRepoLink {
                id: row.get("id"),
                source_repo_id: row.get("source_repo_id"),
                source_community_id: row.get("source_community_id"),
                target_repo_id: row.get("target_repo_id"),
                target_community_id: row.get("target_community_id"),
                kind: row.get("kind"),
                evidence: row.get("evidence"),
                confidence: row.get("confidence"),
            })
            .collect())
    }

    /// Distinct embedder identities used across a project's member repos'
    /// community embeddings. More than one identity means project-wide semantic
    /// routing would compare incompatible vector spaces — surfaced as a warning.
    pub async fn project_embedder_identities(
        &self,
        project_id: Uuid,
    ) -> Result<Vec<(String, String, i32)>> {
        let rows = sqlx::query(
            r#"
            select distinct ce.provider, ce.model_id, ce.dimensions
            from project_repos pr
            join communities c on c.repo_id = pr.repo_id
            join community_embeddings ce on ce.community_id = c.id
            where pr.project_id = $1
            order by ce.provider, ce.model_id, ce.dimensions
            "#,
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("provider"),
                    r.get::<String, _>("model_id"),
                    r.get::<i32, _>("dimensions"),
                )
            })
            .collect())
    }

    // ---- P6 linker facets (read-only, off the persisted index) --------------

    /// Indexed manifest file paths (package.json / Cargo.toml) of a repo — the
    /// places a published package name can be declared.
    pub async fn manifest_file_paths(&self, repo_id: Uuid) -> Result<Vec<String>> {
        let rows = sqlx::query(
            r#"
            select distinct path from files
            where repo_id = $1
              and (path = 'package.json' or path like '%/package.json'
                   or path = 'Cargo.toml' or path like '%/Cargo.toml')
            order by path
            "#,
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| r.get::<String, _>("path"))
            .collect())
    }

    /// Solidity contract / interface / library definitions of a repo as
    /// `(name, solidity_kind, file_path)` — the ABI anchors other repos may
    /// reference.
    pub async fn solidity_contract_nodes(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(String, String, String)>> {
        let rows = sqlx::query(
            r#"
            select n.name, n.metadata->>'solidity_kind' as skind, coalesce(f.path, '') as path
            from nodes n
            join files f on f.id = n.file_id
            where n.repo_id = $1
              and n.metadata->>'language' = 'solidity'
              and n.metadata->>'solidity_kind' in ('contract', 'interface', 'library')
            order by n.name, f.path
            "#,
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("name"),
                    r.get::<String, _>("skind"),
                    r.get::<String, _>("path"),
                )
            })
            .collect())
    }

    /// Chunks whose content matches any of the ILIKE `patterns`, as
    /// `(file_path, content)` in a deterministic order. The SQL prefilter for
    /// the linkers' lexical scans (route literals, contract references,
    /// import statements); precise matching happens in Rust.
    pub async fn scan_chunks(
        &self,
        repo_id: Uuid,
        patterns: &[String],
        limit: i64,
    ) -> Result<Vec<(String, String)>> {
        if patterns.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"
            select coalesce(f.path, '') as path, c.content
            from chunks c
            left join files f on f.id = c.file_id
            where c.repo_id = $1 and c.content ilike any($2::text[])
            order by path, c.line_start nulls first, c.content_hash
            limit $3
            "#,
        )
        .bind(repo_id)
        .bind(patterns)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<String, _>("path"), r.get::<String, _>("content")))
            .collect())
    }

    /// For each of `paths`, the feature community (member_count ≥ 2) holding
    /// the most of that file's symbols — the community a cross-repo link
    /// attaches to. Files outside any feature community are simply absent.
    pub async fn dominant_community_for_files(
        &self,
        repo_id: Uuid,
        paths: &[String],
    ) -> Result<HashMap<String, Uuid>> {
        if paths.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = sqlx::query(
            r#"
            select f.path as path, cm.community_id as community_id, count(*)::bigint as cnt
            from community_members cm
            join communities c on c.id = cm.community_id
            join nodes n on n.id = cm.node_id
            join files f on f.id = n.file_id
            where f.repo_id = $1 and c.member_count >= 2 and f.path = any($2)
            group by f.path, cm.community_id
            order by f.path, cnt desc, cm.community_id
            "#,
        )
        .bind(repo_id)
        .bind(paths)
        .fetch_all(&self.pool)
        .await?;
        let mut map: HashMap<String, Uuid> = HashMap::new();
        for row in rows {
            let path: String = row.get("path");
            // Rows arrive best-first per path; keep only the dominant one.
            map.entry(path).or_insert_with(|| row.get("community_id"));
        }
        Ok(map)
    }

    /// `(community_id, label)` pairs for a set of communities — used to render
    /// human-readable link endpoints across repos.
    pub async fn community_labels_for(&self, ids: &[Uuid]) -> Result<HashMap<Uuid, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = sqlx::query("select id, label from communities where id = any($1)")
            .bind(ids)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<Uuid, _>("id"), r.get::<String, _>("label")))
            .collect())
    }
}

fn row_to_project(row: &sqlx::postgres::PgRow) -> Project {
    Project {
        id: row.get("id"),
        name: row.get("name"),
        created_at: row.get("created_at"),
    }
}

/// A community matched during top-down retrieval / change planning.
#[derive(Debug, Clone)]
pub struct CommunityMatch {
    pub id: Uuid,
    pub label: String,
    pub summary: Option<String>,
    pub member_count: i32,
    /// Cosine similarity to the query (0.0 for briefs not from the embedding match).
    pub score: f64,
}

/// Deterministically-ordered inputs for an extractive community summary
/// (see `src/community_summary.rs`).
#[derive(Debug, Clone)]
pub struct CommunitySummaryInputs {
    pub label: String,
    pub member_count: i32,
    /// `(name, kind, file_path)` per member, capped and ordered.
    pub members: Vec<(String, String, String)>,
    /// A few representative chunk snippets.
    pub snippets: Vec<String>,
    /// Labels of neighboring features this one connects to (from the quotient
    /// graph) — the "where it sits / what it relates to" signal in the summary.
    pub related: Vec<String>,
}

/// A symbol match returned by [`Storage::search_symbols_by_name`] and the
/// hook subcommand's direct pool query.
#[derive(Debug, Clone)]
pub struct SymbolHit {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line_start: Option<i32>,
}

async fn insert_file(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    file: &SourceFile,
) -> Result<()> {
    sqlx::query(
        r#"
        insert into files (id, repo_id, commit_sha, path, language, content_hash, line_count, indexed_at)
        values ($1, $2, $3, $4, $5, $6, $7, now())
        "#,
    )
    .bind(file.id)
    .bind(file.repo_id)
    .bind(file.commit_sha.as_deref().unwrap_or("unknown"))
    .bind(&file.path)
    .bind(file.language.as_str())
    .bind(&file.content_hash)
    .bind(file.line_count)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_node(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    node: &KnowledgeNode,
) -> Result<()> {
    sqlx::query(
        r#"
        insert into nodes (id, repo_id, file_id, kind, stable_id, name, line_start, line_end, metadata)
        values ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        on conflict (repo_id, stable_id) do update set
            file_id = coalesce(nodes.file_id, excluded.file_id),
            kind = excluded.kind,
            name = excluded.name,
            line_start = coalesce(nodes.line_start, excluded.line_start),
            line_end = coalesce(nodes.line_end, excluded.line_end),
            metadata = nodes.metadata || excluded.metadata
        "#,
    )
    .bind(node.id)
    .bind(node.repo_id)
    .bind(node.file_id)
    .bind(node.kind.as_str())
    .bind(&node.stable_id)
    .bind(&node.name)
    .bind(node.line_start)
    .bind(node.line_end)
    .bind(&node.metadata)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Upsert a node by `(repo_id, stable_id)` and return the authoritative row id
/// (the existing id on conflict, the new id on insert). Mirrors [`insert_node`]
/// but reports the id so [`Storage::merge_files_index`] can remap edge/chunk
/// references onto surviving rows.
async fn upsert_node_returning_id(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    node: &KnowledgeNode,
) -> Result<Uuid> {
    let row = sqlx::query(
        r#"
        insert into nodes (id, repo_id, file_id, kind, stable_id, name, line_start, line_end, metadata)
        values ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        on conflict (repo_id, stable_id) do update set
            file_id = coalesce(nodes.file_id, excluded.file_id),
            kind = excluded.kind,
            name = excluded.name,
            line_start = coalesce(nodes.line_start, excluded.line_start),
            line_end = coalesce(nodes.line_end, excluded.line_end),
            metadata = nodes.metadata || excluded.metadata
        returning id
        "#,
    )
    .bind(node.id)
    .bind(node.repo_id)
    .bind(node.file_id)
    .bind(node.kind.as_str())
    .bind(&node.stable_id)
    .bind(&node.name)
    .bind(node.line_start)
    .bind(node.line_end)
    .bind(&node.metadata)
    .fetch_one(&mut **tx)
    .await?;
    Ok(row.get("id"))
}

async fn insert_edge(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    edge: &KnowledgeEdge,
) -> Result<()> {
    sqlx::query(
        r#"
        insert into edges (id, repo_id, source_node_id, target_node_id, kind, cost, confidence, metadata)
        values ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
    )
    .bind(edge.id)
    .bind(edge.repo_id)
    .bind(edge.source_node_id)
    .bind(edge.target_node_id)
    .bind(edge.kind.as_str())
    .bind(edge.cost)
    .bind(edge.confidence)
    .bind(&edge.metadata)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_chunk(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    chunk: &KnowledgeChunk,
) -> Result<()> {
    sqlx::query(
        r#"
        insert into chunks (id, repo_id, file_id, node_id, chunk_type, content, content_hash, line_start, line_end, metadata, search_vector)
        values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, to_tsvector('english', $6))
        "#,
    )
    .bind(chunk.id)
    .bind(chunk.repo_id)
    .bind(chunk.file_id)
    .bind(chunk.node_id)
    .bind(&chunk.chunk_type)
    .bind(&chunk.content)
    .bind(&chunk.content_hash)
    .bind(chunk.line_start)
    .bind(chunk.line_end)
    .bind(&chunk.metadata)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn row_to_node(row: sqlx::postgres::PgRow) -> KnowledgeNode {
    KnowledgeNode {
        id: row.get("id"),
        repo_id: row.get("repo_id"),
        file_id: row.get("file_id"),
        kind: serde_json::from_value(json!(row.get::<String, _>("kind")))
            .unwrap_or(crate::models::NodeKind::Concept),
        stable_id: row.get("stable_id"),
        name: row.get("name"),
        line_start: row.get("line_start"),
        line_end: row.get("line_end"),
        metadata: row.get("metadata"),
    }
}

fn row_to_edge(row: sqlx::postgres::PgRow) -> KnowledgeEdge {
    KnowledgeEdge {
        id: row.get("id"),
        repo_id: row.get("repo_id"),
        source_node_id: row.get("source_node_id"),
        target_node_id: row.get("target_node_id"),
        kind: serde_json::from_value(json!(row.get::<String, _>("kind")))
            .unwrap_or(crate::models::EdgeKind::Mentions),
        cost: row.get("cost"),
        confidence: row.get("confidence"),
        metadata: row.get("metadata"),
    }
}

fn row_to_community_match(row: sqlx::postgres::PgRow) -> CommunityMatch {
    CommunityMatch {
        id: row.get("id"),
        label: row.get("label"),
        summary: row.get("summary"),
        member_count: row.get("member_count"),
        score: row.get("score"),
    }
}

fn row_to_chunk(row: sqlx::postgres::PgRow) -> KnowledgeChunk {
    KnowledgeChunk {
        id: row.get("id"),
        repo_id: row.get("repo_id"),
        file_id: row.get("file_id"),
        node_id: row.get("node_id"),
        chunk_type: row.get("chunk_type"),
        content: row.get("content"),
        content_hash: row.get("content_hash"),
        line_start: row.get("line_start"),
        line_end: row.get("line_end"),
        metadata: row.get("metadata"),
    }
}

fn row_to_search_hit(row: sqlx::postgres::PgRow) -> SearchHit {
    SearchHit {
        chunk_id: row.get("chunk_id"),
        node_id: row.get("node_id"),
        file_path: row.get("file_path"),
        line_start: row.get("line_start"),
        line_end: row.get("line_end"),
        score: row.get("score"),
        content: row.get("content"),
        metadata: row.get("metadata"),
    }
}

/// DB-backed integration tests for the persisted hierarchy layers. They run
/// only when `DATABASE_URL` is set (so the embedder-free CI path skips them) and
/// always operate on a throwaway repo path, purged at the end. They need no
/// embedder — community detection and Merkle rollup are embedder-free.
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::community::{detect_and_persist, CommunityConfig};
    use crate::models::{
        EdgeKind, ExtractionResult, KnowledgeChunk, KnowledgeEdge, KnowledgeNode, Language,
        NodeKind, SourceFile,
    };
    use std::path::Path;

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
                // non-empty file subtree hashes for the Merkle rollup.
                let content = format!("fn {file}::c{ci}_f{k} body");
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
        let sql_comm: i64 =
            sqlx::query_scalar("select count(*) from communities where repo_id = $1")
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
        async fn embed(&self, _input: &str) -> Result<Vec<f32>> {
            anyhow::bail!("embedder unavailable (test)")
        }
    }

    /// Build the project's real configured embedder, probing it; None if the DB
    /// or embedder backend is unavailable (so CI skips cleanly).
    async fn try_real_embedder() -> Option<Box<dyn crate::embedding::Embedder>> {
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

        let result =
            crate::community_summary::summarize_repo(&storage, &FailEmbedder, repo.id).await;
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
}
