use crate::config::{EmbeddingConfig, EmbeddingProvider};
use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::env;

#[async_trait]
pub trait Embedder: Send + Sync {
    fn provider(&self) -> &'static str;
    fn model_id(&self) -> &str;
    fn dimensions(&self) -> usize;
    async fn embed(&self, input: &str) -> Result<Vec<f32>>;
}

pub fn build_embedder(cfg: &EmbeddingConfig) -> Result<Box<dyn Embedder>> {
    match cfg.provider {
        EmbeddingProvider::OpenAi => Ok(Box::new(OpenAiEmbedder::new(cfg)?)),
        EmbeddingProvider::Ollama => Ok(Box::new(OllamaEmbedder::new(cfg))),
    }
}

pub fn vector_literal(vector: &[f32]) -> String {
    let body = vector
        .iter()
        .map(|v| {
            if v.is_finite() {
                v.to_string()
            } else {
                "0".to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("[{body}]")
}

struct OpenAiEmbedder {
    client: Client,
    api_key: String,
    model: String,
    dimensions: usize,
}

impl OpenAiEmbedder {
    fn new(cfg: &EmbeddingConfig) -> Result<Self> {
        let api_key = env::var("OPENAI_API_KEY")
            .context("OPENAI_API_KEY is required for the OpenAI embedder")?;
        Ok(Self {
            client: Client::new(),
            api_key,
            model: cfg.model.clone(),
            dimensions: cfg.dimensions,
        })
    }
}

#[derive(Serialize)]
struct OpenAiEmbeddingRequest<'a> {
    model: &'a str,
    input: &'a str,
    dimensions: usize,
}

#[derive(Deserialize)]
struct OpenAiEmbeddingResponse {
    data: Vec<OpenAiEmbeddingData>,
}

#[derive(Deserialize)]
struct OpenAiEmbeddingData {
    embedding: Vec<f32>,
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
        let response = self
            .client
            .post("https://api.openai.com/v1/embeddings")
            .bearer_auth(&self.api_key)
            .json(&OpenAiEmbeddingRequest {
                model: &self.model,
                input,
                dimensions: self.dimensions,
            })
            .send()
            .await
            .context("failed to call OpenAI embeddings API")?;
        let response = ensure_success(response.status(), response.text().await?, "OpenAI").await?;
        let response = serde_json::from_str::<OpenAiEmbeddingResponse>(&response)
            .context("failed to decode OpenAI embeddings response")?;

        let embedding = response
            .data
            .into_iter()
            .next()
            .context("OpenAI embeddings response had no vectors")?
            .embedding;
        validate_dimensions(&embedding, self.dimensions)?;
        Ok(embedding)
    }
}

struct OllamaEmbedder {
    client: Client,
    base_url: String,
    model: String,
    dimensions: usize,
}

impl OllamaEmbedder {
    fn new(cfg: &EmbeddingConfig) -> Self {
        Self {
            client: Client::new(),
            base_url: cfg
                .base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:11434".into()),
            model: cfg.model.clone(),
            dimensions: cfg.dimensions,
        }
    }
}

#[derive(Serialize)]
struct OllamaEmbeddingRequest<'a> {
    model: &'a str,
    input: &'a str,
}

#[derive(Deserialize)]
struct OllamaEmbeddingResponse {
    embeddings: Vec<Vec<f32>>,
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
        let url = format!("{}/api/embed", self.base_url.trim_end_matches('/'));
        let response = self
            .client
            .post(url)
            .json(&OllamaEmbeddingRequest {
                model: &self.model,
                input,
            })
            .send()
            .await
            .context("failed to call Ollama embeddings API")?;
        let response = ensure_success(response.status(), response.text().await?, "Ollama").await?;
        let response = serde_json::from_str::<OllamaEmbeddingResponse>(&response)
            .context("failed to decode Ollama embeddings response")?;
        let embedding = response
            .embeddings
            .into_iter()
            .next()
            .context("Ollama embed response had no vectors")?;
        validate_dimensions(&embedding, self.dimensions)?;
        Ok(embedding)
    }
}

fn validate_dimensions(embedding: &[f32], expected: usize) -> Result<()> {
    if embedding.len() != expected {
        anyhow::bail!(
            "embedding dimension mismatch: provider returned {}, config expects {}",
            embedding.len(),
            expected
        );
    }
    Ok(())
}

async fn ensure_success(status: StatusCode, body: String, provider: &str) -> Result<String> {
    if status.is_success() {
        return Ok(body);
    }

    let detail = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.pointer("/error"))
                .and_then(|v| v.as_str().map(str::to_string))
        })
        .unwrap_or_else(|| body.chars().take(500).collect());
    anyhow::bail!("{provider} embeddings API returned {status}: {detail}");
}
