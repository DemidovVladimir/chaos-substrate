//! Port: real vector embeddings. No mock/fake/random implementation may exist
//! in runtime code; tests use scripted vectors behind this trait only.

use crate::domain::model::ModelRef;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

#[async_trait]
pub trait Embedder: Send + Sync {
    fn provider(&self) -> &'static str;
    fn model_id(&self) -> &str;
    fn dimensions(&self) -> usize;

    fn model_ref(&self) -> ModelRef {
        ModelRef {
            provider: self.provider().to_string(),
            model_id: self.model_id().to_string(),
            dimensions: self.dimensions(),
        }
    }

    async fn embed(&self, input: &str) -> Result<Vec<f32>>;

    /// Embed several texts in one provider request where supported. Output
    /// order = input order. Default: sequential single calls.
    async fn embed_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(inputs.len());
        for input in inputs {
            out.push(self.embed(input).await?);
        }
        Ok(out)
    }
}

#[async_trait]
impl<E: Embedder + ?Sized> Embedder for Arc<E> {
    fn provider(&self) -> &'static str {
        (**self).provider()
    }
    fn model_id(&self) -> &str {
        (**self).model_id()
    }
    fn dimensions(&self) -> usize {
        (**self).dimensions()
    }
    async fn embed(&self, input: &str) -> Result<Vec<f32>> {
        (**self).embed(input).await
    }
    async fn embed_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        (**self).embed_batch(inputs).await
    }
}

/// Cosine similarity of two equal-length vectors (0 when either is zero).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}
