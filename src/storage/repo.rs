//! Repository rows, analysis runs, per-repo statistics, purge/clear.

use super::Storage;
use crate::models::Repository;
use anyhow::Result;
use serde_json::{json, Value};
use sqlx::Row;
use std::{fs, path::Path};
use uuid::Uuid;

impl Storage {
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

        Ok(row_to_repository(&row))
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

        Ok(row.as_ref().map(row_to_repository))
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
        Ok(rows.iter().map(row_to_repository).collect())
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
    pub(crate) async fn group_counts(
        &self,
        repo_id: Uuid,
        table: &str,
        column: &str,
    ) -> Result<Value> {
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
}

/// Map a `repositories` row (full column list) to a [`Repository`].
pub(super) fn row_to_repository(row: &sqlx::postgres::PgRow) -> Repository {
    Repository {
        id: row.get("id"),
        name: row.get("name"),
        root_path: row.get("root_path"),
        remote_url: row.get("remote_url"),
        current_commit_sha: row.get("current_commit_sha"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}
