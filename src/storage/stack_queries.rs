//! Tech-stack inventory facets (read-only, off the persisted index): declared
//! dependencies, npm scripts, deployment resources, and indexed configs.

use super::Storage;
use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

impl Storage {
    /// Manifest-DECLARED dependencies of a repo. Import-derived `dependency`
    /// nodes (a file importing `react`) are excluded by the stable_id shape —
    /// only entries that appear in an indexed package.json / Cargo.toml count
    /// as part of the declared stack.
    pub async fn stack_dependencies(&self, repo_id: Uuid) -> Result<Vec<StackDependencyRow>> {
        let rows = sqlx::query(
            r#"
            select
                case when n.stable_id like '%:cargo:dependency:%' then 'cargo' else 'npm' end as ecosystem,
                n.name,
                coalesce(n.metadata->>'version', '') as version,
                coalesce(n.metadata->>'section', '') as section,
                coalesce(f.path, '') as manifest
            from nodes n
            left join files f on f.id = n.file_id
            where n.repo_id = $1
              and n.kind = 'dependency'
              and (n.stable_id like '%:npm:dependency:%'
                   or n.stable_id like '%:cargo:dependency:%')
            order by n.name, manifest
            "#,
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| StackDependencyRow {
                ecosystem: r.get("ecosystem"),
                name: r.get("name"),
                version: r.get("version"),
                section: r.get("section"),
                manifest: r.get("manifest"),
            })
            .collect())
    }

    /// Manifest-declared scripts (npm `scripts` entries) of a repo.
    pub async fn stack_scripts(&self, repo_id: Uuid) -> Result<Vec<StackScriptRow>> {
        let rows = sqlx::query(
            r#"
            select
                coalesce(n.metadata->>'script', n.name) as name,
                coalesce(n.metadata->>'command', '') as command,
                coalesce(f.path, '') as manifest
            from nodes n
            left join files f on f.id = n.file_id
            where n.repo_id = $1 and n.kind = 'script'
            order by name, manifest
            "#,
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| StackScriptRow {
                name: r.get("name"),
                command: r.get("command"),
                manifest: r.get("manifest"),
            })
            .collect())
    }

    /// Deployment resources of a repo (CDK apps from cdk.json, Stack classes,
    /// L2 constructs) with their extraction metadata flattened.
    pub async fn stack_deployment_resources(
        &self,
        repo_id: Uuid,
    ) -> Result<Vec<StackDeploymentRow>> {
        let rows = sqlx::query(
            r#"
            select
                n.name,
                coalesce(n.metadata->>'technology', '') as technology,
                coalesce(n.metadata->>'resource_kind',
                         case when n.stable_id like '%:aws-cdk:app' then 'app' else '' end) as resource_kind,
                coalesce(n.metadata->>'service', '') as service,
                coalesce(n.metadata->>'app', '') as app,
                coalesce(n.metadata->>'file', n.metadata->>'config', f.path, '') as file,
                n.line_start
            from nodes n
            left join files f on f.id = n.file_id
            where n.repo_id = $1 and n.kind = 'deployment_resource'
            order by n.name, file
            "#,
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| StackDeploymentRow {
                name: r.get("name"),
                technology: r.get("technology"),
                resource_kind: r.get("resource_kind"),
                service: r.get("service"),
                app: r.get("app"),
                file: r.get("file"),
                line_start: r.get::<Option<i32>, _>("line_start"),
            })
            .collect())
    }

    /// Indexed JS/TS configuration files (tsconfig/jsconfig `config` chunks).
    pub async fn stack_config_files(&self, repo_id: Uuid) -> Result<Vec<String>> {
        let rows = sqlx::query(
            r#"
            select distinct coalesce(metadata->>'config', '') as path
            from chunks
            where repo_id = $1 and chunk_type = 'config'
            order by path
            "#,
        )
        .bind(repo_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| r.get::<String, _>("path"))
            .filter(|p| !p.is_empty())
            .collect())
    }
}

/// One manifest-declared dependency row (see [`Storage::stack_dependencies`]).
#[derive(Debug, Clone)]
pub struct StackDependencyRow {
    /// `npm` or `cargo`, derived from the stable_id shape.
    pub ecosystem: String,
    pub name: String,
    /// Declared version requirement; empty for Cargo entries (not extracted).
    pub version: String,
    /// Manifest section (`dependencies`, `devDependencies`, `dev-dependencies`, …).
    pub section: String,
    /// Repo-relative path of the declaring manifest.
    pub manifest: String,
}

/// One npm script row (see [`Storage::stack_scripts`]).
#[derive(Debug, Clone)]
pub struct StackScriptRow {
    pub name: String,
    pub command: String,
    pub manifest: String,
}

/// One deployment-resource row (see [`Storage::stack_deployment_resources`]).
#[derive(Debug, Clone)]
pub struct StackDeploymentRow {
    pub name: String,
    /// e.g. `aws_cdk`.
    pub technology: String,
    /// `app` (cdk.json), `stack` (Stack class), or `construct` (L2 resource).
    pub resource_kind: String,
    /// Cloud service bucket derived from the construct type (lambda, dynamodb, …).
    pub service: String,
    /// CDK app command, populated for `app` rows only.
    pub app: String,
    pub file: String,
    pub line_start: Option<i32>,
}
