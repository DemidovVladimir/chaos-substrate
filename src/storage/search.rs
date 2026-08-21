//! Chunk retrieval: the semantic / keyword / literal / subject channels the
//! query pipeline fuses, plus the [`ChunkSearch`] port that pipeline consumes.

use super::Storage;
use crate::embedding::vector_literal;
use crate::models::{KnowledgeEdge, SearchHit};
use anyhow::Result;
use async_trait::async_trait;
use sqlx::Row;
use uuid::Uuid;

/// Cap on literal-search hits contributed by ONE file, so a folder whose
/// path matches the term (e.g. `onchainlabs/` for "onchainlabs") adds breadth
/// across files instead of flooding the limit with its first lines.
const LITERAL_HITS_PER_FILE: i64 = 2;

/// Cap on subject-search hits contributed by ONE file (see [`Storage::subject_search`]):
/// the recall floor wants BREADTH across the files named after the query, not a
/// deep dive into any single one.
const SUBJECT_HITS_PER_FILE: i64 = 2;

/// Chunk types counted as DOCUMENTATION by the `_docs` search variants.
const DOC_CHUNK_TYPES: [&str; 2] = ["documentation", "pdf_documentation"];

/// The retrieval PORT the query pipeline (`src/query.rs`) depends on — the
/// four hit channels it fuses plus the edge lookup its context paths need.
/// [`Storage`] is the production adapter; tests drive the composed pipeline
/// with a scripted fake instead of a database. Kept deliberately narrow: a
/// full `StoragePort` (~75 methods) would have no legitimate second adapter —
/// an in-memory store is forbidden by the hard rules — so only the seam that
/// PAYS (retrieval-fusion testability) is a trait (see `ARCHITECTURE.md`,
/// Design decisions).
#[async_trait]
pub trait ChunkSearch: Send + Sync {
    /// Cosine match of a query embedding against chunk embeddings.
    async fn semantic_search(
        &self,
        repo_id: Uuid,
        provider: &str,
        model_id: &str,
        dimensions: usize,
        query_embedding: &[f32],
        limit: i64,
    ) -> Result<Vec<SearchHit>>;

    /// Full-text search over the chunk `search_vector`s.
    async fn keyword_search(
        &self,
        repo_id: Uuid,
        query: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>>;

    /// Exact-substring retrieval over chunk content and file paths.
    async fn literal_search(&self, repo_id: Uuid, term: &str, limit: i64)
        -> Result<Vec<SearchHit>>;

    /// Chunks of the files NAMED AFTER the query (basename-scoped).
    async fn subject_search(
        &self,
        repo_id: Uuid,
        terms: &[String],
        limit: i64,
    ) -> Result<Vec<SearchHit>>;

    /// Edges touching any of `node_ids` — the context-path input.
    async fn load_edges_for_nodes(
        &self,
        repo_id: Uuid,
        node_ids: &[Uuid],
    ) -> Result<Vec<KnowledgeEdge>>;
}

/// The production adapter: every port method delegates to the inherent
/// `Storage` query of the same name.
#[async_trait]
impl ChunkSearch for Storage {
    async fn semantic_search(
        &self,
        repo_id: Uuid,
        provider: &str,
        model_id: &str,
        dimensions: usize,
        query_embedding: &[f32],
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        Storage::semantic_search(
            self,
            repo_id,
            provider,
            model_id,
            dimensions,
            query_embedding,
            limit,
        )
        .await
    }

    async fn keyword_search(
        &self,
        repo_id: Uuid,
        query: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        Storage::keyword_search(self, repo_id, query, limit).await
    }

    async fn literal_search(
        &self,
        repo_id: Uuid,
        term: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        Storage::literal_search(self, repo_id, term, limit).await
    }

    async fn subject_search(
        &self,
        repo_id: Uuid,
        terms: &[String],
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        Storage::subject_search(self, repo_id, terms, limit).await
    }

    async fn load_edges_for_nodes(
        &self,
        repo_id: Uuid,
        node_ids: &[Uuid],
    ) -> Result<Vec<KnowledgeEdge>> {
        Storage::load_edges_for_nodes(self, repo_id, node_ids).await
    }
}

/// Blanket: a borrow of any implementor is itself an implementor, so call
/// sites can hand `&storage` (or `&fake`) around without re-borrow ceremony.
#[async_trait]
impl<T: ChunkSearch + ?Sized> ChunkSearch for &T {
    async fn semantic_search(
        &self,
        repo_id: Uuid,
        provider: &str,
        model_id: &str,
        dimensions: usize,
        query_embedding: &[f32],
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        (**self)
            .semantic_search(
                repo_id,
                provider,
                model_id,
                dimensions,
                query_embedding,
                limit,
            )
            .await
    }

    async fn keyword_search(
        &self,
        repo_id: Uuid,
        query: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        (**self).keyword_search(repo_id, query, limit).await
    }

    async fn literal_search(
        &self,
        repo_id: Uuid,
        term: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        (**self).literal_search(repo_id, term, limit).await
    }

    async fn subject_search(
        &self,
        repo_id: Uuid,
        terms: &[String],
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        (**self).subject_search(repo_id, terms, limit).await
    }

    async fn load_edges_for_nodes(
        &self,
        repo_id: Uuid,
        node_ids: &[Uuid],
    ) -> Result<Vec<KnowledgeEdge>> {
        (**self).load_edges_for_nodes(repo_id, node_ids).await
    }
}

impl Storage {
    pub async fn semantic_search(
        &self,
        repo_id: Uuid,
        provider: &str,
        model_id: &str,
        dimensions: usize,
        query_embedding: &[f32],
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        self.semantic_search_filtered(
            repo_id,
            provider,
            model_id,
            dimensions,
            query_embedding,
            limit,
            None,
        )
        .await
    }

    /// Like [`Storage::semantic_search`] but restricted to DOCUMENTATION chunks
    /// (`documentation` / `pdf_documentation`). Used by the feature-story
    /// supersession pass so prose evidence ("X replaces Y") is not drowned out
    /// by code chunks.
    pub async fn semantic_search_docs(
        &self,
        repo_id: Uuid,
        provider: &str,
        model_id: &str,
        dimensions: usize,
        query_embedding: &[f32],
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        self.semantic_search_filtered(
            repo_id,
            provider,
            model_id,
            dimensions,
            query_embedding,
            limit,
            Some(&DOC_CHUNK_TYPES),
        )
        .await
    }

    /// The one semantic-search query; `chunk_types` = None searches every
    /// chunk, `Some(types)` restricts to those chunk types (the `_docs` filter).
    #[allow(clippy::too_many_arguments)]
    async fn semantic_search_filtered(
        &self,
        repo_id: Uuid,
        provider: &str,
        model_id: &str,
        dimensions: usize,
        query_embedding: &[f32],
        limit: i64,
        chunk_types: Option<&[&str]>,
    ) -> Result<Vec<SearchHit>> {
        let filter: Option<Vec<String>> =
            chunk_types.map(|types| types.iter().map(|t| t.to_string()).collect());
        let literal = vector_literal(query_embedding);
        let rows = sqlx::query(
            r#"
            select c.id as chunk_id, c.node_id, f.path as file_path, c.line_start, c.line_end,
                   1.0 - (e.embedding <=> $5::vector) as score, c.content, c.metadata
            from embeddings e
            join chunks c on c.id = e.chunk_id
            left join files f on f.id = c.file_id
            where c.repo_id = $1 and e.provider = $2 and e.model_id = $3 and e.dimensions = $4
              and ($7::text[] is null or c.chunk_type = any($7))
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
        .bind(filter)
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
        self.keyword_search_filtered(repo_id, query, limit, None)
            .await
    }

    /// Like [`Storage::keyword_search`] but restricted to DOCUMENTATION chunks.
    pub async fn keyword_search_docs(
        &self,
        repo_id: Uuid,
        query: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        self.keyword_search_filtered(repo_id, query, limit, Some(&DOC_CHUNK_TYPES))
            .await
    }

    /// The one keyword-search query; `chunk_types` as in
    /// [`Storage::semantic_search_filtered`].
    async fn keyword_search_filtered(
        &self,
        repo_id: Uuid,
        query: &str,
        limit: i64,
        chunk_types: Option<&[&str]>,
    ) -> Result<Vec<SearchHit>> {
        let filter: Option<Vec<String>> =
            chunk_types.map(|types| types.iter().map(|t| t.to_string()).collect());
        let rows = sqlx::query(
            r#"
            select c.id as chunk_id, c.node_id, f.path as file_path, c.line_start, c.line_end,
                   ts_rank_cd(c.search_vector, websearch_to_tsquery('english', $2))::float8 as score,
                   c.content, c.metadata
            from chunks c
            left join files f on f.id = c.file_id
            where c.repo_id = $1 and c.search_vector @@ websearch_to_tsquery('english', $2)
              and ($4::text[] is null or c.chunk_type = any($4))
            order by score desc
            limit $3
            "#,
        )
        .bind(repo_id)
        .bind(query)
        .bind(limit)
        .bind(filter)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_search_hit).collect())
    }

    /// Exact-substring retrieval over chunk content and file paths.
    ///
    /// Ranking constraints learned from real validation (a term like
    /// "onchainlabs" path-matches an entire repo folder):
    /// - CONTENT matches weigh the same as path matches — a chunk that
    ///   mentions the term is at least as related as a chunk that merely
    ///   lives under a matching path. The old 1.5-path / 0.35-content split
    ///   let one folder's line-1 chunks flood every slot and shadow
    ///   content-matching files elsewhere in the tree.
    /// - At most [`LITERAL_HITS_PER_FILE`] hits per file (content matches
    ///   preferred within a file), so a path-matching folder contributes
    ///   breadth, not a wall of its first lines.
    pub async fn literal_search(
        &self,
        repo_id: Uuid,
        term: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        // Escape LIKE metacharacters so `_`/`%`/`\` in the term match LITERALLY
        // (an env var like API_KEY must not let `_` act as a single-char
        // wildcard). The SQL pairs this with `escape '\'` on each LIKE.
        let escaped = term
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let rows = sqlx::query(
            r#"
            with matches as (
                select c.id as chunk_id, c.node_id, f.path as file_path, c.line_start, c.line_end,
                       (
                         case when lower(coalesce(f.path, '')) like lower($2) escape '\' then 0.75 else 0 end +
                         case when lower(c.content) like lower($2) escape '\' then 0.75 else 0 end
                       )::float8 as score,
                       c.content, c.metadata,
                       row_number() over (
                         partition by coalesce(f.path, c.id::text)
                         order by case when lower(c.content) like lower($2) escape '\' then 0 else 1 end,
                                  c.line_start nulls last
                       ) as file_rank
                from chunks c
                left join files f on f.id = c.file_id
                where c.repo_id = $1
                  and (lower(coalesce(f.path, '')) like lower($2) escape '\' or lower(c.content) like lower($2) escape '\')
            )
            select chunk_id, node_id, file_path, line_start, line_end, score, content, metadata
            from matches
            where file_rank <= $4
            order by score desc, file_path nulls last, line_start nulls last
            limit $3
            "#,
        )
        .bind(repo_id)
        .bind(pattern)
        .bind(limit)
        .bind(LITERAL_HITS_PER_FILE)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_search_hit).collect())
    }

    /// Chunks whose FILE BASENAME carries one of the subject `terms` (the query's
    /// acronym / distinctive tokens) — the files NAMED AFTER the query. Returned
    /// so the shared retrieval can GUARANTEE these into the evidence (see
    /// `query::guarantee_subject_recall`): a named-feature query must surface the
    /// files that ARE the feature, not just whatever the common words rank.
    ///
    /// Basename-scoped, not full path, so `ocl` matches `ocl-service.ts` but NOT
    /// every file under an `onchainlabs/` folder — the recall floor stays precise.
    /// Coarse `ILIKE` here; the caller refines with the segment-exact
    /// `query::node_is_subject`. A modest baseline score lets a subject that ALSO
    /// matched content outrank a name-only one when slots are reserved. Capped per
    /// file ([`SUBJECT_HITS_PER_FILE`]) so one file can't claim the budget.
    pub async fn subject_search(
        &self,
        repo_id: Uuid,
        terms: &[String],
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let patterns: Vec<String> = terms.iter().map(|term| format!("%{term}%")).collect();
        let rows = sqlx::query(
            r#"
            with matches as (
                select c.id as chunk_id, c.node_id, f.path as file_path, c.line_start, c.line_end,
                       0.7::float8 as score, c.content, c.metadata,
                       row_number() over (
                         partition by f.path order by c.line_start nulls last
                       ) as file_rank
                from chunks c
                join files f on f.id = c.file_id
                where c.repo_id = $1
                  and lower(reverse(split_part(reverse(f.path), '/', 1))) ilike any($2)
            )
            select chunk_id, node_id, file_path, line_start, line_end, score, content, metadata
            from matches
            where file_rank <= $4
            order by file_path, line_start nulls last
            limit $3
            "#,
        )
        .bind(repo_id)
        .bind(&patterns)
        .bind(limit)
        .bind(SUBJECT_HITS_PER_FILE)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_search_hit).collect())
    }

    /// Latest-indexed text of every file, aggregated from persisted chunks.
    /// Read-only; feeds the knowledge-gaps detector (`src/gaps.rs`).
    pub async fn load_file_texts(&self, repo_id: Uuid) -> Result<Vec<crate::gaps::FileText>> {
        let rows = sqlx::query(
            r#"
            with latest as (
                select distinct on (path) id, path, language, line_count
                from files
                where repo_id = $1
                order by path, indexed_at desc
            )
            select l.path, l.language, l.line_count,
                   coalesce(string_agg(c.content, ' '), '') as text,
                   count(c.id) as chunk_count
            from latest l
            left join chunks c on c.file_id = l.id
            group by l.path, l.language, l.line_count
            order by l.path
            "#,
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| crate::gaps::FileText {
                path: row.get("path"),
                language: row.get("language"),
                line_count: row.get("line_count"),
                text: row.get("text"),
                chunk_count: row.get("chunk_count"),
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
