//! Port: persisting L1 communities and their L2 summaries.

use crate::domain::model::{Community, ModelRef};
use anyhow::Result;
use async_trait::async_trait;
use uuid::Uuid;

#[async_trait]
pub trait CommunityStore: Send + Sync {
    /// Replace a repo's communities, each row carrying its summary text and
    /// `summary_hash`. A summary embedding stored before under the same
    /// `summary_hash` MUST be restored, so an unchanged summary is never
    /// embedded again.
    async fn replace_communities(
        &self,
        repo_id: Uuid,
        communities: &[Community],
        members: &[(Uuid, Uuid)],
    ) -> Result<()>;
    async fn load_communities(&self, repo_id: Uuid) -> Result<Vec<Community>>;
    /// `(community_id, node_id)` pairs.
    async fn load_members(&self, repo_id: Uuid) -> Result<Vec<(Uuid, Uuid)>>;
    /// Communities with a summary but no summary embedding for `model`.
    async fn communities_needing_summary(
        &self,
        repo_id: Uuid,
        model: &ModelRef,
    ) -> Result<Vec<Community>>;
    async fn save_summary_embedding(
        &self,
        community_id: Uuid,
        model: &ModelRef,
        embedding: &[f32],
    ) -> Result<()>;
}
