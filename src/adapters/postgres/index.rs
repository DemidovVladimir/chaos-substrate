//! `IndexStore` on Postgres: the L0 write path (replace / merge), graph loads,
//! Merkle inputs and embedding bookkeeping. Bulk rows go through `unnest`.

use super::{
    row_to_chunk, row_to_edge, row_to_file, row_to_node, row_to_repo, vector_literal, Postgres,
    CHUNK_COLS,
};
use crate::domain::model::{Chunk, Edge, IndexBundle, ModelRef, Node, Repo, SourceFile};
use crate::ports::index_store::{Graph, IndexStore};
use anyhow::{bail, Result};
use async_trait::async_trait;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

/// Batch size for `unnest` inserts (keeps bind payloads bounded).
const BATCH: usize = 2000;

async fn insert_files(conn: &mut PgConnection, files: &[SourceFile]) -> Result<()> {
    for batch in files.chunks(BATCH) {
        sqlx::query(
            r#"
            insert into files (id, repo_id, path, language, content_hash, line_count, subtree_hash)
            select * from unnest($1::uuid[], $2::uuid[], $3::text[], $4::text[], $5::text[], $6::int4[], $7::text[])
            on conflict (repo_id, path) do update set
                language = excluded.language, content_hash = excluded.content_hash,
                line_count = excluded.line_count, subtree_hash = excluded.subtree_hash
            "#,
        )
        .bind(batch.iter().map(|f| f.id).collect::<Vec<_>>())
        .bind(batch.iter().map(|f| f.repo_id).collect::<Vec<_>>())
        .bind(batch.iter().map(|f| f.path.clone()).collect::<Vec<_>>())
        .bind(batch.iter().map(|f| f.language.as_str().to_string()).collect::<Vec<_>>())
        .bind(batch.iter().map(|f| f.content_hash.clone()).collect::<Vec<_>>())
        .bind(batch.iter().map(|f| f.line_count).collect::<Vec<_>>())
        .bind(batch.iter().map(|f| f.subtree_hash.clone()).collect::<Vec<_>>())
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn upsert_nodes(conn: &mut PgConnection, nodes: &[Node]) -> Result<()> {
    for batch in nodes.chunks(BATCH) {
        sqlx::query(
            r#"
            insert into nodes (id, repo_id, file_id, kind, stable_id, name, signature, doc, line_start, line_end, metadata)
            select * from unnest($1::uuid[], $2::uuid[], $3::uuid[], $4::text[], $5::text[], $6::text[],
                                 $7::text[], $8::text[], $9::int4[], $10::int4[], $11::jsonb[])
            on conflict (repo_id, stable_id) do update set
                file_id = excluded.file_id, kind = excluded.kind, name = excluded.name,
                signature = excluded.signature, doc = excluded.doc, line_start = excluded.line_start,
                line_end = excluded.line_end, metadata = excluded.metadata
            "#,
        )
        .bind(batch.iter().map(|n| n.id).collect::<Vec<_>>())
        .bind(batch.iter().map(|n| n.repo_id).collect::<Vec<_>>())
        .bind(batch.iter().map(|n| n.file_id).collect::<Vec<_>>())
        .bind(batch.iter().map(|n| n.kind.as_str().to_string()).collect::<Vec<_>>())
        .bind(batch.iter().map(|n| n.stable_id.clone()).collect::<Vec<_>>())
        .bind(batch.iter().map(|n| n.name.clone()).collect::<Vec<_>>())
        .bind(batch.iter().map(|n| n.signature.clone()).collect::<Vec<_>>())
        .bind(batch.iter().map(|n| n.doc.clone()).collect::<Vec<_>>())
        .bind(batch.iter().map(|n| n.line_start).collect::<Vec<_>>())
        .bind(batch.iter().map(|n| n.line_end).collect::<Vec<_>>())
        .bind(batch.iter().map(|n| n.metadata.clone()).collect::<Vec<_>>())
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Insert edges, skipping duplicates and edges whose endpoints are not in the
/// `nodes` table (the bundle's nodes are upserted first, so "absent from both
/// DB and bundle" reduces to "absent from the table").
async fn insert_edges(conn: &mut PgConnection, edges: &[Edge]) -> Result<()> {
    for batch in edges.chunks(BATCH) {
        sqlx::query(
            r#"
            insert into edges (id, repo_id, source_node_id, target_node_id, kind, confidence)
            select e.id, e.repo_id, e.source_node_id, e.target_node_id, e.kind, e.confidence
            from unnest($1::uuid[], $2::uuid[], $3::uuid[], $4::uuid[], $5::text[], $6::real[])
                 as e(id, repo_id, source_node_id, target_node_id, kind, confidence)
            where e.source_node_id <> e.target_node_id
              and exists (select 1 from nodes s where s.id = e.source_node_id)
              and exists (select 1 from nodes t where t.id = e.target_node_id)
            on conflict do nothing
            "#,
        )
        .bind(batch.iter().map(|e| e.id).collect::<Vec<_>>())
        .bind(batch.iter().map(|e| e.repo_id).collect::<Vec<_>>())
        .bind(batch.iter().map(|e| e.source_node_id).collect::<Vec<_>>())
        .bind(batch.iter().map(|e| e.target_node_id).collect::<Vec<_>>())
        .bind(
            batch
                .iter()
                .map(|e| e.kind.as_str().to_string())
                .collect::<Vec<_>>(),
        )
        .bind(batch.iter().map(|e| e.confidence).collect::<Vec<_>>())
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn insert_chunks(conn: &mut PgConnection, chunks: &[Chunk]) -> Result<()> {
    for batch in chunks.chunks(BATCH) {
        sqlx::query(
            r#"
            insert into chunks (id, repo_id, file_id, node_id, kind, header, body, content_hash, line_start, line_end, metadata)
            select * from unnest($1::uuid[], $2::uuid[], $3::uuid[], $4::uuid[], $5::text[], $6::text[],
                                 $7::text[], $8::text[], $9::int4[], $10::int4[], $11::jsonb[])
            "#,
        )
        .bind(batch.iter().map(|c| c.id).collect::<Vec<_>>())
        .bind(batch.iter().map(|c| c.repo_id).collect::<Vec<_>>())
        .bind(batch.iter().map(|c| c.file_id).collect::<Vec<_>>())
        .bind(batch.iter().map(|c| c.node_id).collect::<Vec<_>>())
        .bind(batch.iter().map(|c| c.kind.as_str().to_string()).collect::<Vec<_>>())
        .bind(batch.iter().map(|c| c.header.clone()).collect::<Vec<_>>())
        .bind(batch.iter().map(|c| c.body.clone()).collect::<Vec<_>>())
        .bind(batch.iter().map(|c| c.content_hash.clone()).collect::<Vec<_>>())
        .bind(batch.iter().map(|c| c.line_start).collect::<Vec<_>>())
        .bind(batch.iter().map(|c| c.line_end).collect::<Vec<_>>())
        .bind(batch.iter().map(|c| c.metadata.clone()).collect::<Vec<_>>())
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

#[async_trait]
impl IndexStore for Postgres {
    async fn upsert_repo(&self, repo: &Repo) -> Result<()> {
        sqlx::query(
            r#"
            insert into repositories (id, name, root_path, commit_sha, root_hash, indexed_at)
            values ($1, $2, $3, $4, $5, now())
            on conflict (root_path) do update set
                name = excluded.name, commit_sha = excluded.commit_sha,
                root_hash = excluded.root_hash, indexed_at = now()
            "#,
        )
        .bind(repo.id)
        .bind(&repo.name)
        .bind(&repo.root_path)
        .bind(&repo.commit_sha)
        .bind(&repo.root_hash)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_repo(&self, root_path: &str) -> Result<Option<Repo>> {
        let row = sqlx::query("select id, name, root_path, commit_sha, root_hash from repositories where root_path = $1")
            .bind(root_path)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(row_to_repo))
    }

    async fn list_repos(&self) -> Result<Vec<Repo>> {
        let rows = sqlx::query("select id, name, root_path, commit_sha, root_hash from repositories order by root_path")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_repo).collect())
    }

    async fn set_repo_hashes(
        &self,
        repo_id: Uuid,
        commit_sha: Option<&str>,
        root_hash: &str,
    ) -> Result<()> {
        sqlx::query("update repositories set commit_sha = $2, root_hash = $3, indexed_at = now() where id = $1")
            .bind(repo_id)
            .bind(commit_sha)
            .bind(root_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Harvest the repo's embeddings by `(content_hash, provider, model_id,
    /// dimensions)` into a transaction-scoped temp table, wipe the repo's L0
    /// rows, insert the bundle, then restore embeddings onto the fresh chunks
    /// that share a `content_hash` — an unchanged chunk is never re-embedded.
    async fn replace_repo(&self, repo_id: Uuid, bundle: &IndexBundle) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            create temp table _chaos_saved_embeddings on commit drop as
            select distinct on (c.content_hash, e.provider, e.model_id, e.dimensions)
                   c.content_hash, e.provider, e.model_id, e.dimensions, e.embedding
            from embeddings e join chunks c on c.id = e.chunk_id
            where c.repo_id = $1
            order by c.content_hash, e.provider, e.model_id, e.dimensions
            "#,
        )
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;
        for table in ["chunks", "edges", "nodes", "files"] {
            sqlx::query(&format!("delete from {table} where repo_id = $1"))
                .bind(repo_id)
                .execute(&mut *tx)
                .await?;
        }
        insert_files(&mut tx, &bundle.files).await?;
        upsert_nodes(&mut tx, &bundle.nodes).await?;
        insert_edges(&mut tx, &bundle.edges).await?;
        insert_chunks(&mut tx, &bundle.chunks).await?;
        sqlx::query(
            r#"
            insert into embeddings (chunk_id, provider, model_id, dimensions, content_hash, embedding)
            select c.id, s.provider, s.model_id, s.dimensions, c.content_hash, s.embedding
            from chunks c join _chaos_saved_embeddings s on s.content_hash = c.content_hash
            where c.repo_id = $1
            on conflict do nothing
            "#,
        )
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Purge `purge_paths` (FK cascade removes their nodes/edges/chunks/
    /// embeddings), upsert the bundle's files by `(repo_id, path)`, drop the
    /// stale chunks and vanished nodes of re-inserted files, upsert nodes by
    /// `(repo_id, stable_id)`, insert edges (anti-joined on endpoints) and
    /// chunks. File ids are remapped onto surviving rows when a re-inserted
    /// path already exists under a different id.
    async fn merge_files(
        &self,
        repo_id: Uuid,
        purge_paths: &[String],
        bundle: &IndexBundle,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        if !purge_paths.is_empty() {
            sqlx::query("delete from files where repo_id = $1 and path = any($2)")
                .bind(repo_id)
                .bind(purge_paths)
                .execute(&mut *tx)
                .await?;
        }
        insert_files(&mut tx, &bundle.files).await?;

        let paths: Vec<String> = bundle.files.iter().map(|f| f.path.clone()).collect();
        let rows = sqlx::query("select id, path from files where repo_id = $1 and path = any($2)")
            .bind(repo_id)
            .bind(&paths)
            .fetch_all(&mut *tx)
            .await?;
        let db_ids: std::collections::HashMap<String, Uuid> = rows
            .iter()
            .map(|r| (r.get::<String, _>("path"), r.get::<Uuid, _>("id")))
            .collect();
        let remap: std::collections::HashMap<Uuid, Uuid> = bundle
            .files
            .iter()
            .filter_map(|f| db_ids.get(&f.path).map(|db| (f.id, *db)))
            .collect();
        let file_ids: Vec<Uuid> = remap.values().copied().collect();
        let map = |id: Option<Uuid>| id.map(|i| remap.get(&i).copied().unwrap_or(i));

        let stable_ids: Vec<String> = bundle.nodes.iter().map(|n| n.stable_id.clone()).collect();
        sqlx::query("delete from nodes where repo_id = $1 and file_id = any($2) and not (stable_id = any($3))")
            .bind(repo_id)
            .bind(&file_ids)
            .bind(&stable_ids)
            .execute(&mut *tx)
            .await?;
        sqlx::query("delete from chunks where repo_id = $1 and file_id = any($2)")
            .bind(repo_id)
            .bind(&file_ids)
            .execute(&mut *tx)
            .await?;

        let nodes: Vec<Node> = bundle
            .nodes
            .iter()
            .map(|n| Node {
                file_id: map(n.file_id),
                ..n.clone()
            })
            .collect();
        upsert_nodes(&mut tx, &nodes).await?;
        insert_edges(&mut tx, &bundle.edges).await?;
        let chunks: Vec<Chunk> = bundle
            .chunks
            .iter()
            .map(|c| Chunk {
                file_id: map(c.file_id),
                ..c.clone()
            })
            .collect();
        insert_chunks(&mut tx, &chunks).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn load_graph(&self, repo_id: Uuid) -> Result<Graph> {
        let files = sqlx::query("select id, repo_id, path, language, content_hash, line_count, subtree_hash from files where repo_id = $1 order by path")
            .bind(repo_id)
            .fetch_all(&self.pool)
            .await?;
        let nodes = sqlx::query("select id, repo_id, file_id, kind, stable_id, name, signature, doc, line_start, line_end, metadata from nodes where repo_id = $1 order by stable_id")
            .bind(repo_id)
            .fetch_all(&self.pool)
            .await?;
        let edges = sqlx::query("select id, repo_id, source_node_id, target_node_id, kind, confidence from edges where repo_id = $1 order by source_node_id, target_node_id, kind")
            .bind(repo_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(Graph {
            files: files.iter().map(row_to_file).collect::<Result<_>>()?,
            nodes: nodes.iter().map(row_to_node).collect::<Result<_>>()?,
            edges: edges.iter().map(row_to_edge).collect::<Result<_>>()?,
        })
    }

    async fn load_chunk_hashes(&self, repo_id: Uuid) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query(
            "select f.path, c.content_hash from chunks c join files f on f.id = c.file_id where c.repo_id = $1 order by f.path, c.line_start nulls last, c.content_hash",
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| (r.get("path"), r.get("content_hash")))
            .collect())
    }

    async fn set_file_subtree_hashes(
        &self,
        repo_id: Uuid,
        hashes: &[(String, String)],
    ) -> Result<()> {
        for batch in hashes.chunks(BATCH) {
            sqlx::query(
                r#"
                update files f set subtree_hash = h.hash
                from unnest($2::text[], $3::text[]) as h(path, hash)
                where f.repo_id = $1 and f.path = h.path
                "#,
            )
            .bind(repo_id)
            .bind(batch.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>())
            .bind(batch.iter().map(|(_, h)| h.clone()).collect::<Vec<_>>())
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    async fn chunks_missing_embeddings(
        &self,
        repo_id: Uuid,
        model: &ModelRef,
    ) -> Result<Vec<Chunk>> {
        let rows = sqlx::query(&format!(
            r#"
            select {CHUNK_COLS} from chunks c
            left join embeddings e on e.chunk_id = c.id and e.provider = $2 and e.model_id = $3
                 and e.dimensions = $4 and e.content_hash = c.content_hash
            where c.repo_id = $1 and e.chunk_id is null
            order by c.id
            "#
        ))
        .bind(repo_id)
        .bind(&model.provider)
        .bind(&model.model_id)
        .bind(model.dimensions as i32)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_chunk).collect()
    }

    async fn save_embeddings(
        &self,
        model: &ModelRef,
        rows: &[(Uuid, String, Vec<f32>)],
    ) -> Result<()> {
        if let Some((id, _, v)) = rows.iter().find(|(_, _, v)| v.len() != model.dimensions) {
            bail!(
                "embedding for chunk {id} has {} dimensions; model {} expects {}",
                v.len(),
                model.model_id,
                model.dimensions
            );
        }
        for batch in rows.chunks(BATCH) {
            sqlx::query(
                r#"
                insert into embeddings (chunk_id, provider, model_id, dimensions, content_hash, embedding)
                select r.chunk_id, $1, $2, $3, r.content_hash, r.embedding::vector
                from unnest($4::uuid[], $5::text[], $6::text[]) as r(chunk_id, content_hash, embedding)
                on conflict (chunk_id, provider, model_id, dimensions) do update set
                    content_hash = excluded.content_hash, embedding = excluded.embedding
                "#,
            )
            .bind(&model.provider)
            .bind(&model.model_id)
            .bind(model.dimensions as i32)
            .bind(batch.iter().map(|(id, _, _)| *id).collect::<Vec<_>>())
            .bind(batch.iter().map(|(_, h, _)| h.clone()).collect::<Vec<_>>())
            .bind(batch.iter().map(|(_, _, v)| vector_literal(v)).collect::<Vec<_>>())
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    async fn count_chunks(&self, repo_id: Uuid) -> Result<i64> {
        Ok(
            sqlx::query_scalar("select count(*) from chunks where repo_id = $1")
                .bind(repo_id)
                .fetch_one(&self.pool)
                .await?,
        )
    }
}
