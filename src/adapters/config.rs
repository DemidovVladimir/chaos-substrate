//! Configuration: `chaos-substrate.toml` (+ env overrides) for storage/embedder,
//! and `chaos.toml` for a multi-repo PROJECT.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingProvider {
    OpenAi,
    Ollama,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexingConfig {
    pub skip_dirs: Vec<String>,
}

impl Default for IndexingConfig {
    fn default() -> Self {
        Self {
            skip_dirs: ["\u{2e}git", "target", "node_modules", ".venv", "dist", "build", ".next", "coverage"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

pub const DEFAULT_CONFIG_PATH: &str = "chaos-substrate.toml";

impl Config {
    /// `path` (or `./chaos-substrate.toml` if present) then env overrides:
    /// `DATABASE_URL`, `CHAOS_EMBED_PROVIDER|MODEL|DIMENSIONS|BASE_URL`.
    /// Without a file, `DATABASE_URL` is required and the embedder defaults to
    /// Ollama `embeddinggemma` (768d).
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let _ = dotenvy::dotenv();
        let path = path
            .map(Path::to_path_buf)
            .or_else(|| Path::new(DEFAULT_CONFIG_PATH).exists().then(|| PathBuf::from(DEFAULT_CONFIG_PATH)));
        let mut config = match path {
            Some(p) => toml::from_str::<Config>(&fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?)
                .with_context(|| format!("parse {}", p.display()))?,
            None => Config {
                storage: StorageConfig {
                    database_url: env::var("DATABASE_URL").context("DATABASE_URL is required without chaos-substrate.toml")?,
                },
                embedding: EmbeddingConfig {
                    provider: EmbeddingProvider::Ollama,
                    model: "embeddinggemma".into(),
                    dimensions: 768,
                    base_url: None,
                },
                indexing: IndexingConfig::default(),
            },
        };
        if let Ok(v) = env::var("DATABASE_URL") {
            config.storage.database_url = v;
        }
        if let Ok(v) = env::var("CHAOS_EMBED_PROVIDER") {
            config.embedding.provider = match v.as_str() {
                "openai" | "open_ai" => EmbeddingProvider::OpenAi,
                "ollama" => EmbeddingProvider::Ollama,
                other => anyhow::bail!("unknown CHAOS_EMBED_PROVIDER {other}"),
            };
        }
        if let Ok(v) = env::var("CHAOS_EMBED_MODEL") {
            config.embedding.model = v;
        }
        if let Ok(v) = env::var("CHAOS_EMBED_DIMENSIONS") {
            config.embedding.dimensions = v.parse().context("CHAOS_EMBED_DIMENSIONS")?;
        }
        if let Ok(v) = env::var("CHAOS_EMBED_BASE_URL") {
            config.embedding.base_url = Some(v);
        }
        Ok(config)
    }
}

/// A multi-repository project: `chaos.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub name: String,
    #[serde(default)]
    pub repos: Vec<RepoEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RepoEntry {
    pub alias: String,
    /// Absolute, or relative to the chaos.toml directory.
    pub path: String,
}

pub const PROJECT_FILE: &str = "chaos.toml";

impl Project {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let mut project: Project = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));
        for repo in &mut project.repos {
            let p = Path::new(&repo.path);
            if p.is_relative() {
                repo.path = base.join(p).to_string_lossy().into_owned();
            }
            if let Ok(c) = fs::canonicalize(&repo.path) {
                repo.path = c.to_string_lossy().into_owned();
            }
        }
        Ok(project)
    }

    /// Nearest `chaos.toml` in `start` or its ancestors.
    pub fn find(start: &Path) -> Option<PathBuf> {
        start.ancestors().map(|d| d.join(PROJECT_FILE)).find(|p| p.is_file())
    }

    /// Resolve `--repo <alias|path>` (None = cwd) to a canonical root path.
    pub fn resolve_repo(project: Option<&Project>, repo: Option<&str>) -> Result<PathBuf> {
        let raw = match repo {
            None => env::current_dir()?,
            Some(r) => match project.and_then(|p| p.repos.iter().find(|e| e.alias == r)) {
                Some(entry) => PathBuf::from(&entry.path),
                None => PathBuf::from(r),
            },
        };
        fs::canonicalize(&raw).with_context(|| format!("repository path {} does not exist", raw.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_parses_and_resolves_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("api")).unwrap();
        let toml_path = dir.path().join(PROJECT_FILE);
        fs::write(&toml_path, "name = \"demo\"\n[[repos]]\nalias = \"api\"\npath = \"./api\"\n").unwrap();
        let project = Project::load(&toml_path).unwrap();
        assert_eq!(project.name, "demo");
        assert!(Path::new(&project.repos[0].path).is_absolute());
        assert_eq!(Project::find(&dir.path().join("api")), Some(toml_path));
        let resolved = Project::resolve_repo(Some(&project), Some("api")).unwrap();
        assert!(resolved.ends_with("api"));
    }
}
