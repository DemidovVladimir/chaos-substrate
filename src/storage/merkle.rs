//! L2 Merkle rollup support: chunk-hash leaves, subtree-hash persistence, and
//! the repo root hash (see `src/merkle.rs` for the rollup itself).

use super::Storage;
use anyhow::Result;
use sqlx::Row;
use std::collections::HashMap;
use uuid::Uuid;

impl Storage {
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
}
