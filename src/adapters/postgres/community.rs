//! `CommunityStore` on Postgres: L1 communities + members and their L2
//! summaries, whose embeddings survive re-clustering under `summary_hash`.

use super::{row_to_community, vector_literal, Postgres};
use crate::domain::model::{Community, ModelRef};
use crate::ports::community_store::CommunityStore;
use anyhow::{bail, Result};
use async_trait::async_trait;
use sqlx::Row;
use uuid::Uuid;

const BATCH: usize = 2000;

const COMMUNITY_COLS: &str =
    "id, repo_id, label, member_count, summary, summary_hash, top_members, files";

#[async_trait]
impl CommunityStore for Postgres {
    /// Harvest the repo's summary embeddings by `summary_hash` into a temp
    /// table, replace the community rows (with their summary text and hash)
    /// and members, then restore provider / model / embedding for every row
    /// whose `summary_hash` was harvested — whatever its id. Members are
    /// inserted via `unnest` (only ids present in `nodes` are kept, so a member
    /// of a since-purged node is dropped rather than failing the FK).
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
            select distinct on (summary_hash) summary_hash, summary_provider, summary_model, summary_embedding
            from communities
            where repo_id = $1 and summary_hash is not null and summary_embedding is not null
            order by summary_hash, id
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
                insert into communities (id, repo_id, label, member_count, summary, summary_hash, top_members, files)
                select * from unnest($1::uuid[], $2::uuid[], $3::text[], $4::int4[], $5::text[], $6::text[], $7::jsonb[], $8::jsonb[])
                "#,
            )
            .bind(batch.iter().map(|c| c.id).collect::<Vec<_>>())
            .bind(batch.iter().map(|_| repo_id).collect::<Vec<_>>())
            .bind(batch.iter().map(|c| c.label.clone()).collect::<Vec<_>>())
            .bind(batch.iter().map(|c| c.member_count).collect::<Vec<_>>())
            .bind(batch.iter().map(|c| c.summary.clone()).collect::<Vec<_>>())
            .bind(batch.iter().map(|c| c.summary_hash.clone()).collect::<Vec<_>>())
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
                summary_provider = s.summary_provider, summary_model = s.summary_model,
                summary_embedding = s.summary_embedding
            from _chaos_saved_summaries s
            where c.repo_id = $1 and c.summary_hash = s.summary_hash
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

    /// Communities with a summary but no embedding, or an embedding from a
    /// different provider / model / dimension count than `model`.
    async fn communities_needing_summary(
        &self,
        repo_id: Uuid,
        model: &ModelRef,
    ) -> Result<Vec<Community>> {
        let rows = sqlx::query(&format!(
            r#"
            select {COMMUNITY_COLS} from communities
            where repo_id = $1 and summary is not null and (
                summary_embedding is null
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

    async fn save_summary_embedding(
        &self,
        community_id: Uuid,
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
            update communities set summary_provider = $2, summary_model = $3,
                                   summary_embedding = $4::vector, updated_at = now()
            where id = $1
            "#,
        )
        .bind(community_id)
        .bind(&model.provider)
        .bind(&model.model_id)
        .bind(vector_literal(embedding))
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
