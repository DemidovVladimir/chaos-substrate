//! OpenAI embedder: `POST /v1/embeddings`, bearer auth from `OPENAI_API_KEY`.

use super::{http_client, post_json_with_retry, validate_dimensions};
use crate::adapters::config::EmbeddingConfig;
use crate::ports::embedder::Embedder;
use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

const URL: &str = "https://api.openai.com/v1/embeddings";

/// Embedder backed by the OpenAI embeddings API.
pub struct OpenAiEmbedder {
    client: Client,
    api_key: String,
    model: String,
    dimensions: usize,
}

impl OpenAiEmbedder {
    /// Build from config; requires `OPENAI_API_KEY` in the environment.
    pub fn new(cfg: &EmbeddingConfig) -> Result<Self> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .context("OPENAI_API_KEY is required for the OpenAI embedder")?;
        Ok(Self {
            client: http_client("OpenAI")?,
            api_key,
            model: cfg.model.clone(),
            dimensions: cfg.dimensions,
        })
    }
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    input: &'a [String],
    dimensions: usize,
}

#[derive(Deserialize)]
struct Response {
    data: Vec<Datum>,
}

#[derive(Deserialize)]
struct Datum {
    embedding: Vec<f32>,
    #[serde(default)]
    index: usize,
}

/// Decode an OpenAI body into `expected` validated vectors in `index` order.
fn parse_response(body: &str, expected: usize, dimensions: usize) -> Result<Vec<Vec<f32>>> {
    let mut data = serde_json::from_str::<Response>(body)
        .context("failed to decode OpenAI embeddings response")?
        .data;
    anyhow::ensure!(
        data.len() == expected,
        "OpenAI returned {} vectors for {expected} inputs",
        data.len()
    );
    data.sort_by_key(|d| d.index);
    let mut out = Vec::with_capacity(data.len());
    for d in data {
        validate_dimensions(&d.embedding, dimensions)?;
        out.push(d.embedding);
    }
    Ok(out)
}

#[async_trait]
impl Embedder for OpenAiEmbedder {
    fn provider(&self) -> &'static str {
        "openai"
    }
    fn model_id(&self) -> &str {
        &self.model
    }
    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn embed(&self, input: &str) -> Result<Vec<f32>> {
        let mut out = self.embed_batch(&[input.to_string()]).await?;
        out.pop()
            .context("OpenAI embeddings response had no vectors")
    }

    async fn embed_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let body = post_json_with_retry(
            &self.client,
            URL,
            Some(&self.api_key),
            &Request {
                model: &self.model,
                input: inputs,
                dimensions: self.dimensions,
            },
            "OpenAI",
        )
        .await?;
        parse_response(&body, inputs.len(), self.dimensions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_shape() {
        let inputs = vec!["x".to_string()];
        let json = serde_json::to_value(Request {
            model: "text-embedding-3-small",
            input: &inputs,
            dimensions: 256,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"model": "text-embedding-3-small", "input": ["x"], "dimensions": 256})
        );
    }

    #[test]
    fn response_parsing_sorts_by_index_and_validates() {
        let body = r#"{"data":[{"index":1,"embedding":[4,5,6]},{"index":0,"embedding":[1,2,3]}]}"#;
        let out = parse_response(body, 2, 3).unwrap();
        assert_eq!(out, vec![vec![1.0, 2.0, 3.0], vec![4.0, 5.0, 6.0]]);
        assert!(parse_response(body, 1, 3).is_err());
        assert!(parse_response(body, 2, 4).is_err());
        assert!(parse_response(r#"{"error":{"message":"x"}}"#, 1, 3).is_err());
    }
}
