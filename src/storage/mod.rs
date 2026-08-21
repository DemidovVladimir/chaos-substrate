//! Postgres + pgvector persistence, split by concern behind ONE public
//! `Storage` type (the split is file-organization only — zero public-API
//! churn, every consumer keeps `crate::storage::{Storage, …}` paths):
//!
//! - [`repo`] — repository rows, analysis runs, stats, purge/clear.
//! - [`index`] — the L0 write path (replace/merge extractions, embeddings).
//! - [`merkle`] — the L2 rollup reads/writes (subtree hashes, repo root).
//! - [`community`] — the L1/L3 layer (communities, summaries, routing).
//! - [`search`] — chunk retrieval (semantic/keyword/literal/subject) and the
//!   [`ChunkSearch`] port the query pipeline consumes.
//! - [`graph`] — whole-graph and node-level reads (exports, consumers).
//! - [`project`] — the P6 cross-repo project layer + linker facets.
//! - [`stack_queries`] — the tech-stack inventory facets.

mod community;
mod graph;
mod index;
mod merkle;
mod project;
mod repo;
mod search;
mod stack_queries;
#[cfg(test)]
mod tests;

pub use community::{CommunityMatch, CommunitySummaryInputs};
pub use graph::{ConsumerRow, NodeRef};
pub use search::{ChunkSearch, SymbolHit};
pub use stack_queries::{StackDependencyRow, StackDeploymentRow, StackScriptRow};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

#[derive(Clone)]
pub struct Storage {
    pool: PgPool,
}

impl Storage {
    pub async fn connect(database_url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(database_url)
            .await
            .context("failed to connect to Postgres")?;
        Ok(Self { pool })
    }

    /// Test-only: a Storage over a LAZY pool that never connects until a
    /// query runs. Lets unit tests prove real dispatch wiring (the first
    /// storage call fails fast with a connection error) without Postgres.
    #[cfg(test)]
    pub(crate) fn connect_lazy_for_tests(database_url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(300))
            .connect_lazy(database_url)
            .context("failed to build the lazy test pool")?;
        Ok(Self { pool })
    }

    /// Connect with a short acquire timeout — used by the `hook` subcommand so
    /// a down database degrades fast rather than blocking the editor.
    pub async fn connect_fast(database_url: &str, timeout: std::time::Duration) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(timeout)
            .connect(database_url)
            .await
            .context("failed to connect to Postgres (fast)")?;
        Ok(Self { pool })
    }

    pub async fn migrate(&self) -> Result<()> {
        // The migrations directory is embedded at compile time and each file is
        // executed whole (no fragile ';' splitting); applied versions are tracked
        // in the `_sqlx_migrations` table.
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .context("failed to run database migrations")?;
        Ok(())
    }

    pub async fn health(&self) -> Result<Value> {
        let version: String = sqlx::query_scalar("select version()")
            .fetch_one(&self.pool)
            .await?;
        let pgvector: Option<String> =
            sqlx::query_scalar("select extversion from pg_extension where extname = 'vector'")
                .fetch_optional(&self.pool)
                .await?;
        Ok(json!({
            "postgres": version,
            "pgvector": pgvector,
        }))
    }
}
