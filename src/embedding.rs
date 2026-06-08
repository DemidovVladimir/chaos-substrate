use crate::config::{EmbeddingConfig, EmbeddingProvider};
use anyhow::{Context, Result};
use async_trait::async_trait;
use backon::{ExponentialBuilder, Retryable};
use reqwest::Client;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::env;
use std::time::Duration;

/// Per-request timeout for embedding HTTP calls.
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum number of retries after the initial attempt for transient failures
/// (so up to 1 + this many total attempts).
const MAX_RETRY_TIMES: usize = 3;

/// An error from a single embedding HTTP attempt, tagged with whether it is
/// worth retrying. Only connection/timeout errors, HTTP 429, and 5xx responses
/// are transient; 4xx (other than 429) — e.g. a bad API key — fail fast.
struct EmbedAttemptError {
    error: anyhow::Error,
    transient: bool,
}

impl EmbedAttemptError {
    fn transient(error: anyhow::Error) -> Self {
        Self {
            error,
            transient: true,
        }
    }

    fn permanent(error: anyhow::Error) -> Self {
        Self {
            error,
            transient: false,
        }
    }
}

/// Backoff policy shared by both embedders: exponential with jitter, capped at
/// [`MAX_RETRY_TIMES`] retries.
fn retry_policy() -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_jitter()
        .with_max_times(MAX_RETRY_TIMES)
}

/// Classify a reqwest transport error: connect/timeout/request errors are
/// transient and worth retrying.
fn transport_error_is_transient(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect() || err.is_request()
}

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
        EmbeddingProvider::Ollama => Ok(Box::new(OllamaEmbedder::new(cfg)?)),
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
        let client = Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .context("failed to build OpenAI HTTP client")?;
        Ok(Self {
            client,
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
        let body = (|| async {
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
                .map_err(|e| {
                    let transient = transport_error_is_transient(&e);
                    let err = anyhow::Error::new(e).context("failed to call OpenAI embeddings API");
                    EmbedAttemptError {
                        error: err,
                        transient,
                    }
                })?;
            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|e| EmbedAttemptError::transient(anyhow::Error::new(e)))?;
            ensure_success(status, text, "OpenAI")
        })
        .retry(retry_policy())
        .when(|e: &EmbedAttemptError| e.transient)
        .await
        .map_err(|e| e.error)?;

        let response = serde_json::from_str::<OpenAiEmbeddingResponse>(&body)
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
    fn new(cfg: &EmbeddingConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .context("failed to build Ollama HTTP client")?;
        Ok(Self {
            client,
            base_url: cfg
                .base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:11434".into()),
            model: cfg.model.clone(),
            dimensions: cfg.dimensions,
        })
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
        let body = (|| async {
            let response = self
                .client
                .post(&url)
                .json(&OllamaEmbeddingRequest {
                    model: &self.model,
                    input,
                })
                .send()
                .await
                .map_err(|e| {
                    let transient = transport_error_is_transient(&e);
                    let err = anyhow::Error::new(e).context("failed to call Ollama embeddings API");
                    EmbedAttemptError {
                        error: err,
                        transient,
                    }
                })?;
            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|e| EmbedAttemptError::transient(anyhow::Error::new(e)))?;
            ensure_success(status, text, "Ollama")
        })
        .retry(retry_policy())
        .when(|e: &EmbedAttemptError| e.transient)
        .await
        .map_err(|e| e.error)?;

        let response = serde_json::from_str::<OllamaEmbeddingResponse>(&body)
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
    // Fail closed on non-finite values: `vector_literal` would otherwise coerce
    // NaN/Inf to 0, silently persisting a degraded "placeholder-ish" vector.
    // A real embedder never returns these, so erroring is the correct posture
    // (invariant: no fabricated vectors).
    if let Some(pos) = embedding.iter().position(|v| !v.is_finite()) {
        anyhow::bail!(
            "embedder returned a non-finite value ({}) at index {pos}; refusing to store a degraded vector",
            embedding[pos]
        );
    }
    Ok(())
}

fn ensure_success(
    status: StatusCode,
    body: String,
    provider: &str,
) -> Result<String, EmbedAttemptError> {
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
    let err = anyhow::anyhow!("{provider} embeddings API returned {status}: {detail}");
    // Retry only on 429 (rate limit) and 5xx (server errors); other 4xx — e.g.
    // a bad API key — must fail fast.
    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        Err(EmbedAttemptError::transient(err))
    } else {
        Err(EmbedAttemptError::permanent(err))
    }
}

#[cfg(test)]
mod tests {
    use super::{validate_dimensions, vector_literal};

    #[test]
    fn rejects_non_finite_vectors() {
        assert!(validate_dimensions(&[0.1, 0.2, 0.3], 3).is_ok());
        assert!(validate_dimensions(&[0.1, f32::NAN, 0.3], 3).is_err());
        assert!(validate_dimensions(&[0.1, f32::INFINITY, 0.3], 3).is_err());
        assert!(validate_dimensions(&[1.0, 2.0], 3).is_err()); // wrong length
    }

    #[test]
    fn vector_literal_formats_finite_values() {
        // (Finite values only — non-finite are rejected before this is reached.)
        assert_eq!(vector_literal(&[1.0, 2.5]), "[1,2.5]");
    }
}
