//! Port: persisting and loading the L0 index (repos, files, nodes, edges,
//! chunks, embeddings). Postgres is the production adapter.

use crate::domain::model::{Chunk, Edge, IndexBundle, ModelRef, Node, Repo, SourceFile};
use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

/// Nodes + edges + files of a repo (no chunk bodies) — the community/merkle input.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    pub files: Vec<SourceFile>,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

#[async_trait]
pub trait IndexStore: Send + Sync {
    async fn upsert_repo(&self, repo: &Repo) -> Result<()>;
    async fn find_repo(&self, root_path: &str) -> Result<Option<Repo>>;
    async fn list_repos(&self) -> Result<Vec<Repo>>;
    async fn set_repo_hashes(&self, repo_id: Uuid, commit_sha: Option<&str>, root_hash: &str) -> Result<()>;

    /// Replace the WHOLE index of a repo. Embeddings whose `(content_hash,
    /// model)` still exist in the new bundle MUST be preserved (harvested into
    /// a temp table and restored) so an unchanged chunk is never re-embedded.
    async fn replace_repo(&self, repo_id: Uuid, bundle: &IndexBundle) -> Result<()>;

    /// Incremental merge: delete every row of `purge_paths` (cascades nodes/
    /// edges/chunks/embeddings), then insert `bundle`. Nodes are upserted by
    /// `(repo_id, stable_id)`; because ids are uuid5(repo_id, stable_id) no
    /// remap is needed. Edges referencing nodes absent from both DB and bundle
    /// are skipped, not errors.
    async fn merge_files(&self, repo_id: Uuid, purge_paths: &[String], bundle: &IndexBundle) -> Result<()>;

    async fn load_graph(&self, repo_id: Uuid) -> Result<Graph>;
    /// `(file path, chunk content_hash)` pairs — Merkle leaves.
    async fn load_chunk_hashes(&self, repo_id: Uuid) -> Result<Vec<(String, String)>>;
    async fn set_file_subtree_hashes(&self, repo_id: Uuid, hashes: &[(String, String)]) -> Result<()>;

    async fn chunks_missing_embeddings(&self, repo_id: Uuid, model: &ModelRef) -> Result<Vec<Chunk>>;
    /// `(chunk_id, content_hash, vector)`.
    async fn save_embeddings(&self, model: &ModelRef, rows: &[(Uuid, String, Vec<f32>)]) -> Result<()>;

    async fn count_chunks(&self, repo_id: Uuid) -> Result<i64>;
}
