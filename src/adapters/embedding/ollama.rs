//! Ollama embedder: `POST {base_url}/api/embed` with an array `input`.

use super::{http_client, post_json_with_retry, validate_dimensions};
use crate::adapters::config::EmbeddingConfig;
use crate::ports::embedder::Embedder;
use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

const DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// Embedder backed by a local or remote Ollama server.
pub struct OllamaEmbedder {
    client: Client,
    base_url: String,
    model: String,
    dimensions: usize,
}

impl OllamaEmbedder {
    /// Build from config; `base_url` defaults to `http://localhost:11434`.
    pub fn new(cfg: &EmbeddingConfig) -> Result<Self> {
        Ok(Self {
            client: http_client("Ollama")?,
            base_url: cfg
                .base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_BASE_URL.into()),
            model: cfg.model.clone(),
            dimensions: cfg.dimensions,
        })
    }

    fn url(&self) -> String {
        format!("{}/api/embed", self.base_url.trim_end_matches('/'))
    }
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct Response {
    embeddings: Vec<Vec<f32>>,
}

/// Decode an Ollama `/api/embed` body into `expected` validated vectors.
fn parse_response(body: &str, expected: usize, dimensions: usize) -> Result<Vec<Vec<f32>>> {
    let response: Response =
        serde_json::from_str(body).context("failed to decode Ollama embeddings response")?;
    anyhow::ensure!(
        response.embeddings.len() == expected,
        "Ollama returned {} vectors for {expected} inputs",
        response.embeddings.len()
    );
    for v in &response.embeddings {
        validate_dimensions(v, dimensions)?;
    }
    Ok(response.embeddings)
}

#[async_trait]
impl Embedder for OllamaEmbedder {
    fn provider(&self) -> &'static str {
        "ollama"
    }
    fn model_id(&self) -> &str {
        &self.model
    }
    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn embed(&self, input: &str) -> Result<Vec<f32>> {
        Ok(self.embed_batch(&[input.to_string()]).await?.remove(0))
    }

    async fn embed_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let body = post_json_with_retry(
            &self.client,
            &self.url(),
            None,
            &Request {
                model: &self.model,
                input: inputs,
            },
            "Ollama",
        )
        .await?;
        parse_response(&body, inputs.len(), self.dimensions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::config::EmbeddingProvider;

    #[test]
    fn request_body_shape() {
        let inputs = vec!["a".to_string(), "b".to_string()];
        let json = serde_json::to_value(Request {
            model: "m",
            input: &inputs,
        })
        .unwrap();
        assert_eq!(json, serde_json::json!({"model": "m", "input": ["a", "b"]}));
    }

    #[test]
    fn url_trims_trailing_slash() {
        let cfg = EmbeddingConfig {
            provider: EmbeddingProvider::Ollama,
            model: "m".into(),
            dimensions: 3,
            base_url: Some("http://h:1/".into()),
        };
        assert_eq!(
            OllamaEmbedder::new(&cfg).unwrap().url(),
            "http://h:1/api/embed"
        );
    }

    #[test]
    fn response_parsing_validates() {
        let ok = parse_response(r#"{"embeddings":[[1,2,3],[4,5,6]]}"#, 2, 3).unwrap();
        assert_eq!(ok, vec![vec![1.0, 2.0, 3.0], vec![4.0, 5.0, 6.0]]);
        assert!(parse_response(r#"{"embeddings":[[1,2,3]]}"#, 2, 3).is_err());
        assert!(parse_response(r#"{"embeddings":[[1,2]]}"#, 1, 3).is_err());
    }
}
