//! `CommunityStore` on Postgres: L1 communities + members and their L3
//! summaries, preserved across re-clustering by the `subtree_hash` gate.

use super::{row_to_community, vector_literal, Postgres};
use crate::domain::model::{Community, ModelRef};
use crate::ports::community_store::CommunityStore;
use anyhow::{bail, Result};
use async_trait::async_trait;
use sqlx::Row;
use uuid::Uuid;

const BATCH: usize = 2000;

const COMMUNITY_COLS: &str =
    "id, repo_id, label, member_count, subtree_hash, summary, summary_hash, top_members, files";

#[async_trait]
impl CommunityStore for Postgres {
    /// Harvest the repo's summaries into a temp table, replace the community
    /// rows and members, then restore summary / summary_hash / provider /
    /// model / embedding for every community whose id AND `subtree_hash` are
    /// unchanged. Members are inserted via `unnest` (only ids present in
    /// `nodes` are kept, so a member of a since-purged node is dropped rather
    /// than failing the FK).
    async fn replace_communities(
        &self,
        repo_id: Uuid,
        communities: &[Community],
        members: &[(Uuid, Uuid)],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            create temp table _chaos_saved_summaries on commit drop as
            select id, subtree_hash, summary, summary_hash, summary_provider, summary_model, summary_embedding
            from communities where repo_id = $1 and summary is not null
            "#,
        )
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("delete from communities where repo_id = $1")
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
        for batch in communities.chunks(BATCH) {
            let json = |v: &Vec<String>| serde_json::to_value(v).unwrap_or_default();
            sqlx::query(
                r#"
                insert into communities (id, repo_id, label, member_count, subtree_hash, top_members, files)
                select * from unnest($1::uuid[], $2::uuid[], $3::text[], $4::int4[], $5::text[], $6::jsonb[], $7::jsonb[])
                "#,
            )
            .bind(batch.iter().map(|c| c.id).collect::<Vec<_>>())
            .bind(batch.iter().map(|_| repo_id).collect::<Vec<_>>())
            .bind(batch.iter().map(|c| c.label.clone()).collect::<Vec<_>>())
            .bind(batch.iter().map(|c| c.member_count).collect::<Vec<_>>())
            .bind(batch.iter().map(|c| c.subtree_hash.clone()).collect::<Vec<_>>())
            .bind(batch.iter().map(|c| json(&c.top_members)).collect::<Vec<_>>())
            .bind(batch.iter().map(|c| json(&c.files)).collect::<Vec<_>>())
            .execute(&mut *tx)
            .await?;
        }
        for batch in members.chunks(BATCH) {
            sqlx::query(
                r#"
                insert into community_members (community_id, node_id)
                select m.community_id, m.node_id
                from unnest($1::uuid[], $2::uuid[]) as m(community_id, node_id)
                where exists (select 1 from communities c where c.id = m.community_id)
                  and exists (select 1 from nodes n where n.id = m.node_id)
                on conflict do nothing
                "#,
            )
            .bind(batch.iter().map(|(c, _)| *c).collect::<Vec<_>>())
            .bind(batch.iter().map(|(_, n)| *n).collect::<Vec<_>>())
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            r#"
            update communities c set
                summary = s.summary, summary_hash = s.summary_hash, summary_provider = s.summary_provider,
                summary_model = s.summary_model, summary_embedding = s.summary_embedding
            from _chaos_saved_summaries s
            where c.id = s.id and c.subtree_hash = s.subtree_hash and c.repo_id = $1
            "#,
        )
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn load_communities(&self, repo_id: Uuid) -> Result<Vec<Community>> {
        let rows = sqlx::query(&format!("select {COMMUNITY_COLS} from communities where repo_id = $1 order by member_count desc, label, id"))
            .bind(repo_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_community).collect())
    }

    async fn load_members(&self, repo_id: Uuid) -> Result<Vec<(Uuid, Uuid)>> {
        let rows = sqlx::query(
            r#"
            select m.community_id, m.node_id from community_members m
            join communities c on c.id = m.community_id
            where c.repo_id = $1 order by m.community_id, m.node_id
            "#,
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| (r.get("community_id"), r.get("node_id")))
            .collect())
    }

    /// Communities with no summary, no `summary_hash`, no embedding, or a
    /// summary produced by a different provider/model than `model`. The store
    /// does not know the domain's gate formula: the use-case additionally
    /// compares each returned-or-not community's `summary_hash` against the
    /// domain gate value (derived from `subtree_hash`) and re-summarises on
    /// mismatch — `replace_communities` already wipes the summary of any
    /// community whose `subtree_hash` changed, so that check is a safety net.
    async fn communities_needing_summary(
        &self,
        repo_id: Uuid,
        model: &ModelRef,
    ) -> Result<Vec<Community>> {
        let rows = sqlx::query(&format!(
            r#"
            select {COMMUNITY_COLS} from communities
            where repo_id = $1 and (
                summary is null or summary_hash is null or summary_embedding is null
                or summary_provider is distinct from $2 or summary_model is distinct from $3
                or vector_dims(summary_embedding) <> $4)
            order by member_count desc, label, id
            "#
        ))
        .bind(repo_id)
        .bind(&model.provider)
        .bind(&model.model_id)
        .bind(model.dimensions as i32)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_community).collect())
    }

    async fn save_summary(
        &self,
        community_id: Uuid,
        summary: &str,
        summary_hash: &str,
        model: &ModelRef,
        embedding: &[f32],
    ) -> Result<()> {
        if embedding.len() != model.dimensions {
            bail!(
                "summary embedding has {} dimensions; model {} expects {}",
                embedding.len(),
                model.model_id,
                model.dimensions
            );
        }
        sqlx::query(
            r#"
            update communities set summary = $2, summary_hash = $3, summary_provider = $4, summary_model = $5,
                                   summary_embedding = $6::vector, updated_at = now()
            where id = $1
            "#,
        )
        .bind(community_id)
        .bind(summary)
        .bind(summary_hash)
        .bind(&model.provider)
        .bind(&model.model_id)
        .bind(vector_literal(embedding))
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
