//! `SearchStore` on Postgres: the three retrieval channels (semantic via
//! pgvector cosine, keyword via `websearch_to_tsquery`, literal via `ilike`),
//! community routing and the graph lookups the feature use-case expands with.

use super::{
    row_to_chunk, row_to_community, row_to_edge, row_to_node, vector_literal, Postgres, CHUNK_COLS,
};
use crate::domain::model::{Chunk, Community, Edge, ModelRef, Node, NodeKind};
use crate::ports::search_store::{Hit, SearchStore};
use anyhow::Result;
use async_trait::async_trait;
use sqlx::postgres::PgRow;
use sqlx::Row;
use uuid::Uuid;

/// Fixed relevance of a literal (substring) match.
const LITERAL_SCORE: f32 = 0.75;
/// Literal hits per file, so one noisy file cannot fill the result list.
const LITERAL_PER_FILE: i64 = 2;

fn row_to_hit(row: &PgRow) -> Result<Hit> {
    Ok(Hit {
        chunk: row_to_chunk(row)?,
        path: row.get::<Option<String>, _>("path").unwrap_or_default(),
        score: row.get::<f64, _>("score") as f32,
    })
}

/// Escape `like` metacharacters and wrap in `%…%`.
fn like_pattern(term: &str) -> String {
    let escaped = term
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// `websearch_to_tsquery` input: whitespace terms OR-joined (any term matches).
/// The SQL side applies `chaos_identifier_text` so `zebraMango` matches the
/// identifier-split `search_vector` of code naming `zebra_mango`.
fn or_query(query: &str) -> String {
    query
        .split_whitespace()
        .filter(|t| !t.eq_ignore_ascii_case("or"))
        .collect::<Vec<_>>()
        .join(" or ")
}

#[async_trait]
impl SearchStore for Postgres {
    async fn semantic(
        &self,
        repo_id: Uuid,
        model: &ModelRef,
        query: &[f32],
        file_ids: Option<&[Uuid]>,
        limit: i64,
    ) -> Result<Vec<Hit>> {
        let rows = sqlx::query(&format!(
            r#"
            select {CHUNK_COLS}, f.path, (1 - (e.embedding <=> $2::vector))::float8 as score
            from embeddings e
            join chunks c on c.id = e.chunk_id
            left join files f on f.id = c.file_id
            where c.repo_id = $1 and e.provider = $3 and e.model_id = $4 and e.dimensions = $5
              and ($6::uuid[] is null or c.file_id = any($6))
            order by e.embedding <=> $2::vector, c.id
            limit $7
            "#
        ))
        .bind(repo_id)
        .bind(vector_literal(query))
        .bind(&model.provider)
        .bind(&model.model_id)
        .bind(model.dimensions as i32)
        .bind(file_ids)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_hit).collect()
    }

    async fn keyword(
        &self,
        repo_id: Uuid,
        query: &str,
        file_ids: Option<&[Uuid]>,
        limit: i64,
    ) -> Result<Vec<Hit>> {
        let tsquery = or_query(query);
        if tsquery.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(&format!(
            r#"
            select {CHUNK_COLS}, f.path,
                   ts_rank_cd(c.search_vector, websearch_to_tsquery('english', chaos_identifier_text($2)))::float8 as score
            from chunks c
            left join files f on f.id = c.file_id
            where c.repo_id = $1 and c.search_vector @@ websearch_to_tsquery('english', chaos_identifier_text($2))
              and ($3::uuid[] is null or c.file_id = any($3))
            order by score desc, c.id
            limit $4
            "#
        ))
        .bind(repo_id)
        .bind(tsquery)
        .bind(file_ids)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_hit).collect()
    }

    async fn literal(
        &self,
        repo_id: Uuid,
        term: &str,
        file_ids: Option<&[Uuid]>,
        limit: i64,
    ) -> Result<Vec<Hit>> {
        if term.trim().is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(&format!(
            r#"
            with matches as (
                select {CHUNK_COLS}, f.path, $5::float8 as score,
                       row_number() over (partition by coalesce(f.path, c.id::text) order by c.line_start nulls last, c.id) as file_rank
                from chunks c
                left join files f on f.id = c.file_id
                where c.repo_id = $1
                  and (c.header ilike $2 escape '\' or c.body ilike $2 escape '\' or coalesce(f.path, '') ilike $2 escape '\')
                  and ($3::uuid[] is null or c.file_id = any($3))
            )
            select * from matches where file_rank <= $6
            order by path nulls last, line_start nulls last, id
            limit $4
            "#
        ))
        .bind(repo_id)
        .bind(like_pattern(term))
        .bind(file_ids)
        .bind(limit)
        .bind(LITERAL_SCORE as f64)
        .bind(LITERAL_PER_FILE)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_hit).collect()
    }

    async fn communities_by_embedding(
        &self,
        repo_id: Uuid,
        model: &ModelRef,
        query: &[f32],
        limit: i64,
    ) -> Result<Vec<(Community, f32)>> {
        let rows = sqlx::query(
            r#"
            select id, repo_id, label, member_count, summary, summary_hash, top_members, files,
                   (1 - (summary_embedding <=> $2::vector))::float8 as score
            from communities
            where repo_id = $1 and summary_embedding is not null
              and summary_provider = $3 and summary_model = $4 and vector_dims(summary_embedding) = $5
            order by summary_embedding <=> $2::vector, id
            limit $6
            "#,
        )
        .bind(repo_id)
        .bind(vector_literal(query))
        .bind(&model.provider)
        .bind(&model.model_id)
        .bind(model.dimensions as i32)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| (row_to_community(r), r.get::<f64, _>("score") as f32))
            .collect())
    }

    async fn community_file_ids(&self, community_ids: &[Uuid]) -> Result<Vec<Uuid>> {
        Ok(sqlx::query_scalar(
            r#"
            select distinct n.file_id from community_members m
            join nodes n on n.id = m.node_id
            where m.community_id = any($1) and n.file_id is not null
            order by n.file_id
            "#,
        )
        .bind(community_ids)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn edges_for_nodes(&self, repo_id: Uuid, node_ids: &[Uuid]) -> Result<Vec<Edge>> {
        let rows = sqlx::query(
            r#"
            select id, repo_id, source_node_id, target_node_id, kind, confidence from edges
            where repo_id = $1 and (source_node_id = any($2) or target_node_id = any($2))
            order by source_node_id, target_node_id, kind
            "#,
        )
        .bind(repo_id)
        .bind(node_ids)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_edge).collect()
    }

    async fn nodes_by_ids(&self, node_ids: &[Uuid]) -> Result<Vec<Node>> {
        let rows = sqlx::query(
            "select id, repo_id, file_id, kind, stable_id, name, signature, doc, line_start, line_end, metadata from nodes where id = any($1) order by stable_id",
        )
        .bind(node_ids)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_node).collect()
    }

    async fn chunks_for_nodes(&self, node_ids: &[Uuid]) -> Result<Vec<Chunk>> {
        let rows = sqlx::query(&format!("select {CHUNK_COLS} from chunks c where c.node_id = any($1) order by c.node_id, c.line_start nulls last, c.id"))
            .bind(node_ids)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(row_to_chunk).collect()
    }

    async fn surfaces(
        &self,
        repo_id: Uuid,
        kinds: &[NodeKind],
        file_ids: Option<&[Uuid]>,
    ) -> Result<Vec<Node>> {
        let kinds: Vec<String> = kinds.iter().map(|k| k.as_str().to_string()).collect();
        let rows = sqlx::query(
            r#"
            select id, repo_id, file_id, kind, stable_id, name, signature, doc, line_start, line_end, metadata from nodes
            where repo_id = $1 and kind = any($2) and ($3::uuid[] is null or file_id = any($3))
            order by kind, name, stable_id
            "#,
        )
        .bind(repo_id)
        .bind(&kinds)
        .bind(file_ids)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_node).collect()
    }

    async fn file_paths(&self, file_ids: &[Uuid]) -> Result<Vec<(Uuid, String)>> {
        let rows = sqlx::query("select id, path from files where id = any($1) order by path")
            .bind(file_ids)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| (r.get("id"), r.get("path"))).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_pattern_escapes() {
        assert_eq!(like_pattern("a_b%c\\"), "%a\\_b\\%c\\\\%");
    }

    #[test]
    fn or_query_joins_terms() {
        assert_eq!(or_query("  env  var or route "), "env or var or route");
        assert_eq!(or_query(""), "");
    }
}
