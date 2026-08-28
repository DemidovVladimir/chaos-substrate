//! Port: retrieval channels the `feature` use-case fuses.

use crate::domain::model::{Chunk, Community, Edge, ModelRef, Node, NodeKind};
use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Hit {
    pub chunk: Chunk,
    pub path: String,
    /// Raw channel score (cosine / ts_rank_cd / fixed literal score) — normalized by the fusion.
    pub score: f32,
}

#[async_trait]
pub trait SearchStore: Send + Sync {
    /// Cosine over chunk embeddings; `file_ids` restricts the candidate set (None = whole repo).
    async fn semantic(&self, repo_id: Uuid, model: &ModelRef, query: &[f32], file_ids: Option<&[Uuid]>, limit: i64) -> Result<Vec<Hit>>;
    /// `websearch_to_tsquery` OR-joined over `search_vector`.
    async fn keyword(&self, repo_id: Uuid, query: &str, file_ids: Option<&[Uuid]>, limit: i64) -> Result<Vec<Hit>>;
    /// Case-insensitive substring over header/body/path; ≤ 2 hits per file.
    async fn literal(&self, repo_id: Uuid, term: &str, file_ids: Option<&[Uuid]>, limit: i64) -> Result<Vec<Hit>>;

    /// Communities ranked by cosine of `summary_embedding`; returns (community, score).
    async fn communities_by_embedding(&self, repo_id: Uuid, model: &ModelRef, query: &[f32], limit: i64) -> Result<Vec<(Community, f32)>>;
    /// File ids of the member nodes of these communities.
    async fn community_file_ids(&self, community_ids: &[Uuid]) -> Result<Vec<Uuid>>;

    async fn edges_for_nodes(&self, repo_id: Uuid, node_ids: &[Uuid]) -> Result<Vec<Edge>>;
    async fn nodes_by_ids(&self, node_ids: &[Uuid]) -> Result<Vec<Node>>;
    async fn chunks_for_nodes(&self, node_ids: &[Uuid]) -> Result<Vec<Chunk>>;
    /// Surface nodes (env vars / routes) of a repo, optionally only from `file_ids`.
    async fn surfaces(&self, repo_id: Uuid, kinds: &[NodeKind], file_ids: Option<&[Uuid]>) -> Result<Vec<Node>>;
    /// Path lookup for file ids.
    async fn file_paths(&self, file_ids: &[Uuid]) -> Result<Vec<(Uuid, String)>>;
}
