//! The L0 write path: replace or merge a repository's extraction result and
//! persist embeddings for its chunks.

use super::Storage;
use crate::embedding::vector_literal;
use crate::models::{ExtractionResult, KnowledgeChunk, KnowledgeEdge, KnowledgeNode, SourceFile};
use anyhow::Result;
use sqlx::Row;
use std::collections::HashMap;
use uuid::Uuid;

impl Storage {
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
        -- Index original content PLUS its identifier-split rendering (008) so
        -- "on chain labs" keyword-matches code naming listAllOnChainLabs.
        values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, to_tsvector('english', $6 || ' ' || chaos_identifier_text($6)))
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
