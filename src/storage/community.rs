//! The L1/L3 community layer: partition persistence, hash-gated summaries,
//! top-down semantic routing, and community-scoped reads.

use super::Storage;
use crate::embedding::vector_literal;
use crate::hierarchy_export::{CommunityDetail, CommunityHierarchy, QuotientEdgeDetail};
use crate::models::Repository;
use anyhow::Result;
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::HashMap;
use uuid::Uuid;

impl Storage {
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

fn row_to_community_match(row: sqlx::postgres::PgRow) -> CommunityMatch {
    CommunityMatch {
        id: row.get("id"),
        label: row.get("label"),
        summary: row.get("summary"),
        member_count: row.get("member_count"),
        score: row.get("score"),
    }
}
