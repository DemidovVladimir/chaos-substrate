//! Port: persisting L1 communities and their L3 summaries.

use crate::domain::model::{Community, ModelRef};
use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

#[async_trait]
pub trait CommunityStore: Send + Sync {
    /// Replace a repo's communities. Summaries/embeddings of communities whose
    /// id AND subtree_hash are unchanged MUST be preserved (hash gate).
    async fn replace_communities(&self, repo_id: Uuid, communities: &[Community], members: &[(Uuid, Uuid)]) -> Result<()>;
    async fn load_communities(&self, repo_id: Uuid) -> Result<Vec<Community>>;
    /// `(community_id, node_id)` pairs.
    async fn load_members(&self, repo_id: Uuid) -> Result<Vec<(Uuid, Uuid)>>;
    /// Communities whose stored `summary_hash` differs from their current gate value
    /// (or that have no summary for `model`).
    async fn communities_needing_summary(&self, repo_id: Uuid, model: &ModelRef) -> Result<Vec<Community>>;
    async fn save_summary(&self, community_id: Uuid, summary: &str, summary_hash: &str, model: &ModelRef, embedding: &[f32]) -> Result<()>;
}
