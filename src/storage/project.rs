//! The P6 cross-repository project layer: project/membership rows, persisted
//! cross-repo links, and the read-only facets the linkers scan.

use super::Storage;
use crate::models::{CrossRepoLink, Project, ProjectRepo, Repository};
use anyhow::Result;
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;

impl Storage {
    /// Create a project (idempotent on name). Returns the existing row when the
    /// name is already taken so `chaos project create` is safe to repeat.
    pub async fn create_project(&self, name: &str) -> Result<Project> {
        let row = sqlx::query(
            r#"
            insert into projects (id, name, created_at, updated_at)
            values ($1, $2, now(), now())
            on conflict (name) do update set updated_at = now()
            returning id, name, created_at
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(name.trim())
        .fetch_one(&self.pool)
        .await?;
        Ok(row_to_project(&row))
    }

    pub async fn find_project(&self, name: &str) -> Result<Option<Project>> {
        let row = sqlx::query("select id, name, created_at from projects where name = $1")
            .bind(name.trim())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(row_to_project))
    }

    pub async fn list_projects(&self) -> Result<Vec<Project>> {
        let rows = sqlx::query("select id, name, created_at from projects order by name")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_project).collect())
    }

    /// Add an indexed repository to a project under a project-scoped alias
    /// (idempotent; re-adding updates the alias).
    pub async fn add_repo_to_project(
        &self,
        project_id: Uuid,
        repo_id: Uuid,
        alias: &str,
    ) -> Result<()> {
        self.add_member_to_project(project_id, repo_id, alias, false)
            .await
    }

    /// Attach a member to a project, flagging whether it is a project-level DOCS
    /// source (vs a code repo). `add_repo_to_project` is the code-repo shorthand.
    pub async fn add_member_to_project(
        &self,
        project_id: Uuid,
        repo_id: Uuid,
        alias: &str,
        is_project_docs: bool,
    ) -> Result<()> {
        sqlx::query(
            r#"
            insert into project_repos (project_id, repo_id, alias, is_project_docs, added_at)
            values ($1, $2, $3, $4, now())
            on conflict (project_id, repo_id)
              do update set alias = excluded.alias,
                            is_project_docs = excluded.is_project_docs
            "#,
        )
        .bind(project_id)
        .bind(repo_id)
        .bind(alias.trim())
        .bind(is_project_docs)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Member repositories of a project with alias + last-linked hash, in a
    /// stable alias order.
    pub async fn project_member_repos(&self, project_id: Uuid) -> Result<Vec<ProjectRepo>> {
        let rows = sqlx::query(
            r#"
            select r.id, r.name, r.root_path, r.remote_url, r.current_commit_sha,
                   r.created_at, r.updated_at, pr.alias, pr.linked_repo_hash,
                   pr.is_project_docs
            from project_repos pr
            join repositories r on r.id = pr.repo_id
            where pr.project_id = $1
            order by pr.alias
            "#,
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| ProjectRepo {
                repo: Repository {
                    id: row.get("id"),
                    name: row.get("name"),
                    root_path: row.get("root_path"),
                    remote_url: row.get("remote_url"),
                    current_commit_sha: row.get("current_commit_sha"),
                    created_at: row.get("created_at"),
                    updated_at: row.get("updated_at"),
                },
                alias: row.get("alias"),
                linked_repo_hash: row.get("linked_repo_hash"),
                is_project_docs: row.get("is_project_docs"),
            })
            .collect())
    }

    /// Every project that contains the given repository — the hook `analyze`/
    /// `add` use to keep the project layer fresh after re-indexing one member.
    pub async fn projects_containing_repo(&self, repo_id: Uuid) -> Result<Vec<Project>> {
        let rows = sqlx::query(
            r#"
            select p.id, p.name, p.created_at
            from projects p
            join project_repos pr on pr.project_id = p.id
            where pr.repo_id = $1
            order by p.name
            "#,
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_project).collect())
    }

    /// Record the repo root hash a relink run was computed from (the gate).
    pub async fn set_linked_repo_hash(
        &self,
        project_id: Uuid,
        repo_id: Uuid,
        hash: &str,
    ) -> Result<()> {
        sqlx::query(
            "update project_repos set linked_repo_hash = $3 \
             where project_id = $1 and repo_id = $2",
        )
        .bind(project_id)
        .bind(repo_id)
        .bind(hash)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Replace a project's cross-repo links with a fresh detection result, in
    /// one transaction (mirrors `replace_communities`: full replace is correct
    /// because links are cheap to recompute and carry no gated state).
    pub async fn replace_project_links(
        &self,
        project_id: Uuid,
        links: &[CrossRepoLink],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("delete from cross_repo_links where project_id = $1")
            .bind(project_id)
            .execute(&mut *tx)
            .await?;
        for link in links {
            sqlx::query(
                r#"
                insert into cross_repo_links
                    (id, project_id, source_repo_id, source_community_id,
                     target_repo_id, target_community_id, kind, evidence, confidence, created_at)
                values ($1, $2, $3, $4, $5, $6, $7, $8, $9, now())
                on conflict (project_id, source_community_id, target_community_id, kind)
                do update set evidence = excluded.evidence, confidence = excluded.confidence
                "#,
            )
            .bind(link.id)
            .bind(project_id)
            .bind(link.source_repo_id)
            .bind(link.source_community_id)
            .bind(link.target_repo_id)
            .bind(link.target_community_id)
            .bind(&link.kind)
            .bind(&link.evidence)
            .bind(link.confidence)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// All persisted cross-repo links of a project, strongest first.
    pub async fn load_project_links(&self, project_id: Uuid) -> Result<Vec<CrossRepoLink>> {
        let rows = sqlx::query(
            r#"
            select id, source_repo_id, source_community_id,
                   target_repo_id, target_community_id, kind, evidence, confidence
            from cross_repo_links
            where project_id = $1
            order by confidence desc, kind, source_community_id, target_community_id
            "#,
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| CrossRepoLink {
                id: row.get("id"),
                source_repo_id: row.get("source_repo_id"),
                source_community_id: row.get("source_community_id"),
                target_repo_id: row.get("target_repo_id"),
                target_community_id: row.get("target_community_id"),
                kind: row.get("kind"),
                evidence: row.get("evidence"),
                confidence: row.get("confidence"),
            })
            .collect())
    }

    /// Distinct embedder identities used across a project's member repos'
    /// community embeddings. More than one identity means project-wide semantic
    /// routing would compare incompatible vector spaces — surfaced as a warning.
    pub async fn project_embedder_identities(
        &self,
        project_id: Uuid,
    ) -> Result<Vec<(String, String, i32)>> {
        let rows = sqlx::query(
            r#"
            select distinct ce.provider, ce.model_id, ce.dimensions
            from project_repos pr
            join communities c on c.repo_id = pr.repo_id
            join community_embeddings ce on ce.community_id = c.id
            where pr.project_id = $1
            order by ce.provider, ce.model_id, ce.dimensions
            "#,
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("provider"),
                    r.get::<String, _>("model_id"),
                    r.get::<i32, _>("dimensions"),
                )
            })
            .collect())
    }

    // ---- P6 linker facets (read-only, off the persisted index) --------------

    /// Indexed manifest file paths (package.json / Cargo.toml) of a repo — the
    /// places a published package name can be declared.
    pub async fn manifest_file_paths(&self, repo_id: Uuid) -> Result<Vec<String>> {
        let rows = sqlx::query(
            r#"
            select distinct path from files
            where repo_id = $1
              and (path = 'package.json' or path like '%/package.json'
                   or path = 'Cargo.toml' or path like '%/Cargo.toml')
            order by path
            "#,
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| r.get::<String, _>("path"))
            .collect())
    }

    /// Solidity contract / interface / library definitions of a repo as
    /// `(name, solidity_kind, file_path)` — the ABI anchors other repos may
    /// reference.
    pub async fn solidity_contract_nodes(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<(String, String, String)>> {
        let rows = sqlx::query(
            r#"
            select n.name, n.metadata->>'solidity_kind' as skind, coalesce(f.path, '') as path
            from nodes n
            join files f on f.id = n.file_id
            where n.repo_id = $1
              and n.metadata->>'language' = 'solidity'
              and n.metadata->>'solidity_kind' in ('contract', 'interface', 'library')
            order by n.name, f.path
            "#,
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("name"),
                    r.get::<String, _>("skind"),
                    r.get::<String, _>("path"),
                )
            })
            .collect())
    }

    /// All nodes of one `kind` in a repo as `(name, metadata, file_path)`.
    /// Same query shape as [`Storage::solidity_contract_nodes`] but returns
    /// the full metadata jsonb — the linker facets built on persisted surface
    /// nodes (`graphql_field`, `graphql_operation`, `http_route`) need the
    /// per-node `operation_type` / `route_path` / `root_fields` detail.
    pub async fn nodes_by_kind_with_file(
        &self,
        repo_id: Uuid,
        kind: &str,
    ) -> Result<Vec<(String, Value, String)>> {
        let rows = sqlx::query(
            r#"
            select n.name, n.metadata, coalesce(f.path, n.metadata->>'file', '') as path
            from nodes n
            left join files f on f.id = n.file_id
            where n.repo_id = $1 and n.kind = $2
            order by n.name, path, n.stable_id
            "#,
        )
        .bind(repo_id)
        .bind(kind)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("name"),
                    r.get::<Value, _>("metadata"),
                    r.get::<String, _>("path"),
                )
            })
            .collect())
    }

    /// Chunks whose content matches any of the ILIKE `patterns`, as
    /// `(file_path, content)` in a deterministic order. The SQL prefilter for
    /// the linkers' lexical scans (route literals, contract references,
    /// import statements); precise matching happens in Rust.
    pub async fn scan_chunks(
        &self,
        repo_id: Uuid,
        patterns: &[String],
        limit: i64,
    ) -> Result<Vec<(String, String)>> {
        if patterns.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"
            select coalesce(f.path, '') as path, c.content
            from chunks c
            left join files f on f.id = c.file_id
            where c.repo_id = $1 and c.content ilike any($2::text[])
            order by path, c.line_start nulls first, c.content_hash
            limit $3
            "#,
        )
        .bind(repo_id)
        .bind(patterns)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<String, _>("path"), r.get::<String, _>("content")))
            .collect())
    }
}

fn row_to_project(row: &sqlx::postgres::PgRow) -> Project {
    Project {
        id: row.get("id"),
        name: row.get("name"),
        created_at: row.get("created_at"),
    }
}
