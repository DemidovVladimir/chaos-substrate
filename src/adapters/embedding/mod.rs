//! Embedding adapters: real OpenAI / Ollama providers behind the [`Embedder`]
//! port. No mock, fake, or random embedder exists in runtime code.

pub mod ollama;
pub mod openai;

use crate::adapters::config::{EmbeddingConfig, EmbeddingProvider};
use crate::ports::embedder::Embedder;
use anyhow::{Context, Result};
use backon::{ExponentialBuilder, Retryable};
use reqwest::{Client, StatusCode};
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;

/// Per-request timeout; generous because a local Ollama queues requests.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(300);
/// Retries after the initial attempt for transient failures.
const MAX_RETRY_TIMES: usize = 3;

/// Build the configured provider behind a shared handle.
pub fn build_embedder(cfg: &EmbeddingConfig) -> Result<Arc<dyn Embedder>> {
    match cfg.provider {
        EmbeddingProvider::OpenAi => Ok(Arc::new(openai::OpenAiEmbedder::new(cfg)?)),
        EmbeddingProvider::Ollama => Ok(Arc::new(ollama::OllamaEmbedder::new(cfg)?)),
    }
}

pub(crate) fn http_client(provider: &str) -> Result<Client> {
    Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .with_context(|| format!("failed to build {provider} HTTP client"))
}

/// Reject vectors of the wrong length or with non-finite values — a real
/// embedder never returns these (invariant: no fabricated vectors stored).
pub(crate) fn validate_dimensions(embedding: &[f32], expected: usize) -> Result<()> {
    anyhow::ensure!(
        embedding.len() == expected,
        "embedding dimension mismatch: provider returned {}, config expects {expected}",
        embedding.len()
    );
    if let Some(pos) = embedding.iter().position(|v| !v.is_finite()) {
        anyhow::bail!(
            "embedder returned a non-finite value ({}) at index {pos}; refusing to store it",
            embedding[pos]
        );
    }
    Ok(())
}

/// One HTTP attempt's error, tagged with whether a retry is worthwhile.
#[derive(Debug)]
struct AttemptError {
    error: anyhow::Error,
    transient: bool,
}

/// POST JSON and return the success body, retrying transient failures
/// (connect/timeout, 429, 5xx) with exponential backoff + jitter at most
/// [`MAX_RETRY_TIMES`] times. Other 4xx fail fast.
pub(crate) async fn post_json_with_retry<B: Serialize + ?Sized + Sync>(
    client: &Client,
    url: &str,
    bearer: Option<&str>,
    body: &B,
    provider: &str,
) -> Result<String> {
    (|| async {
        let mut request = client.post(url).json(body);
        if let Some(key) = bearer {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.map_err(|e| AttemptError {
            transient: e.is_timeout() || e.is_connect() || e.is_request(),
            error: anyhow::Error::new(e)
                .context(format!("failed to call {provider} embeddings API")),
        })?;
        let status = response.status();
        let text = response.text().await.map_err(|e| AttemptError {
            error: anyhow::Error::new(e),
            transient: true,
        })?;
        ensure_success(status, text, provider)
    })
    .retry(
        ExponentialBuilder::default()
            .with_jitter()
            .with_max_times(MAX_RETRY_TIMES),
    )
    .when(|e: &AttemptError| e.transient)
    .await
    .map_err(|e| e.error)
}

fn ensure_success(
    status: StatusCode,
    body: String,
    provider: &str,
) -> Result<String, AttemptError> {
    if status.is_success() {
        return Ok(body);
    }
    let detail = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.pointer("/error"))
                .and_then(|m| m.as_str().map(str::to_string))
        })
        .unwrap_or_else(|| body.chars().take(500).collect());
    Err(AttemptError {
        error: anyhow::anyhow!("{provider} embeddings API returned {status}: {detail}"),
        transient: status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_wrong_length_and_non_finite() {
        assert!(validate_dimensions(&[0.1, 0.2, 0.3], 3).is_ok());
        assert!(validate_dimensions(&[1.0, 2.0], 3).is_err());
        assert!(validate_dimensions(&[0.1, f32::NAN, 0.3], 3).is_err());
        assert!(validate_dimensions(&[0.1, f32::INFINITY, 0.3], 3).is_err());
    }

    #[test]
    fn transient_classification_by_status() {
        let check = |st: StatusCode, body: &str| ensure_success(st, body.into(), "X");
        assert_eq!(check(StatusCode::OK, "{}").unwrap(), "{}");
        assert!(
            check(StatusCode::TOO_MANY_REQUESTS, "")
                .unwrap_err()
                .transient
        );
        assert!(check(StatusCode::BAD_GATEWAY, "").unwrap_err().transient);
        let e = check(
            StatusCode::UNAUTHORIZED,
            r#"{"error":{"message":"bad key"}}"#,
        )
        .unwrap_err();
        assert!(!e.transient && e.error.to_string().contains("bad key"));
        assert!(check(StatusCode::BAD_REQUEST, "plain")
            .unwrap_err()
            .error
            .to_string()
            .contains("plain"));
    }
}
