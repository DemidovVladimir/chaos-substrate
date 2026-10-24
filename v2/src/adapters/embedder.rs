//! Embeddings over the OpenAI-compatible `POST {base_url}/embeddings` API,
//! which Ollama (`http://localhost:11434/v1`), OpenRouter and OpenAI all
//! serve. Model-specific prompt formats (EmbeddingGemma's `task: … | query:`)
//! are applied here, and are part of the embedder id.

use crate::ports::Embedder;
use anyhow::Result;

pub struct HttpEmbedder {
    id: String,
}

impl HttpEmbedder {
    /// `base_url` like `http://localhost:11434/v1`; `api_key` is sent as a
    /// bearer token when present. Nothing is contacted until the first embed.
    pub fn new(base_url: &str, model: &str, api_key: Option<String>) -> HttpEmbedder {
        let _ = (base_url, model, api_key);
        todo!()
    }
}

impl Embedder for HttpEmbedder {
    fn id(&self) -> &str {
        &self.id
    }

    fn embed_documents(&self, passages: &[(&str, &str)]) -> Result<Vec<Vec<f32>>> {
        let _ = passages;
        todo!()
    }

    fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let _ = query;
        todo!()
    }
}
