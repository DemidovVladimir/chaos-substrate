use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{env, fs, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub storage: StorageConfig,
    pub embedding: EmbeddingConfig,
    #[serde(default)]
    pub indexing: IndexingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub database_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    pub provider: EmbeddingProvider,
    pub model: String,
    pub dimensions: usize,
    #[serde(default)]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingProvider {
    OpenAi,
    Ollama,
}

impl EmbeddingProvider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Ollama => "ollama",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexingConfig {
    pub skip_dirs: Vec<String>,
}

impl Default for IndexingConfig {
    fn default() -> Self {
        Self {
            skip_dirs: vec![
                ".git".into(),
                "target".into(),
                "node_modules".into(),
                ".venv".into(),
                "dist".into(),
                "build".into(),
            ],
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        dotenvy::dotenv().ok();

        let path = path.unwrap_or_else(|| Path::new("chaos-substrate.toml"));
        let mut cfg = if path.exists() {
            let raw = fs::read_to_string(path)
                .with_context(|| format!("failed to read config {}", path.display()))?;
            toml::from_str::<Config>(&raw)
                .with_context(|| format!("failed to parse config {}", path.display()))?
        } else {
            Self::from_env()?
        };

        if let Ok(url) = env::var("DATABASE_URL") {
            cfg.storage.database_url = url;
        }

        // Environment variables override the embedding section of a config file
        // too, matching the env-wins precedence DATABASE_URL already follows.
        // Without this, CHAOS_EMBED_* was silently ignored whenever a
        // chaos-substrate.toml existed (read only in the no-file `from_env`
        // branch), so dev (no file) and prod (with file) could diverge.
        if let Ok(provider) = env::var("CHAOS_EMBED_PROVIDER") {
            cfg.embedding.provider = match provider.to_ascii_lowercase().as_str() {
                "openai" | "open_ai" => EmbeddingProvider::OpenAi,
                "ollama" => EmbeddingProvider::Ollama,
                other => anyhow::bail!("unsupported CHAOS_EMBED_PROVIDER: {other}"),
            };
        }
        if let Ok(model) = env::var("CHAOS_EMBED_MODEL") {
            cfg.embedding.model = model;
        }
        if let Ok(dimensions) = env::var("CHAOS_EMBED_DIMENSIONS") {
            cfg.embedding.dimensions = dimensions
                .parse()
                .with_context(|| format!("invalid CHAOS_EMBED_DIMENSIONS: {dimensions}"))?;
        }
        if let Ok(base_url) = env::var("CHAOS_EMBED_BASE_URL") {
            cfg.embedding.base_url = Some(base_url);
        }

        Ok(cfg)
    }

    fn from_env() -> Result<Self> {
        let database_url = env::var("DATABASE_URL")
            .context("DATABASE_URL is required when chaos-substrate.toml is absent")?;
        let provider = match env::var("CHAOS_EMBED_PROVIDER")
            .unwrap_or_else(|_| "openai".into())
            .to_ascii_lowercase()
            .as_str()
        {
            "openai" | "open_ai" => EmbeddingProvider::OpenAi,
            "ollama" => EmbeddingProvider::Ollama,
            other => anyhow::bail!("unsupported CHAOS_EMBED_PROVIDER: {other}"),
        };
        let model = env::var("CHAOS_EMBED_MODEL").unwrap_or_else(|_| match provider {
            EmbeddingProvider::OpenAi => "text-embedding-3-small".into(),
            EmbeddingProvider::Ollama => "nomic-embed-text".into(),
        });
        let dimensions = env::var("CHAOS_EMBED_DIMENSIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(match provider {
                EmbeddingProvider::OpenAi => 1536,
                EmbeddingProvider::Ollama => 768,
            });

        Ok(Self {
            storage: StorageConfig { database_url },
            embedding: EmbeddingConfig {
                provider,
                model,
                dimensions,
                base_url: env::var("CHAOS_EMBED_BASE_URL").ok(),
            },
            indexing: IndexingConfig::default(),
        })
    }
}
