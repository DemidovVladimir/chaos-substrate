use crate::{
    embedding::vector_literal,
    graph_export::{GraphExport, GraphExportEdge, GraphExportNode, GraphRepository},
    models::{
        ExtractionResult, KnowledgeChunk, KnowledgeEdge, KnowledgeNode, Repository, SearchHit,
        SourceFile,
    },
};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, PgPool, Row};
use std::{fs, path::Path};
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

    pub async fn replace_repo_index(&self, repo_id: Uuid, result: &ExtractionResult) -> Result<()> {
        let mut tx = self.pool.begin().await?;
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

        tx.commit().await?;
        Ok(())
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
                (select count(*) from embeddings) as embeddings",
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
        }))
    }

    /// Wipe the entire persisted index (every repository). Returns the row
    /// counts that were removed so the caller can report what was cleared.
    pub async fn clear_all(&self) -> Result<Value> {
        let removed = self.table_counts().await?;
        sqlx::query(
            "truncate embeddings, chunks, edges, nodes, files, analysis_runs, repositories restart identity cascade",
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

        Ok(rows
            .into_iter()
            .map(|row| KnowledgeEdge {
                id: row.get("id"),
                repo_id: row.get("repo_id"),
                source_node_id: row.get("source_node_id"),
                target_node_id: row.get("target_node_id"),
                kind: serde_json::from_value(json!(row.get::<String, _>("kind")))
                    .unwrap_or(crate::models::EdgeKind::Mentions),
                cost: row.get("cost"),
                confidence: row.get("confidence"),
                metadata: row.get("metadata"),
            })
            .collect())
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
