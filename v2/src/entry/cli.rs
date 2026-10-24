//! The command line — the single definition of every tool. Each subcommand's
//! doc comment and argument docs are also its MCP description and schema
//! (`entry::tools`), so the two surfaces cannot drift.

use crate::application::remember::INBOX;
use crate::domain::presence::DEFAULT_MIN_SCORE;
use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use serde_json::Value;
use std::path::PathBuf;
use std::process::ExitCode;

/// Personal knowledge base for agents: Markdown notes in, instant "is this
/// known?" checks and similarity search out.
#[derive(Debug, Parser)]
#[command(name = "chaos-kb", version)]
pub struct Cli {
    #[command(flatten)]
    pub settings: Settings,
    #[command(subcommand)]
    pub command: Command,
}

/// Where the knowledge lives and which embedder reads it. The embeddings API
/// key, when one is needed, comes only from `CHAOS_KB_EMBED_API_KEY`.
#[derive(Debug, Clone, Args)]
pub struct Settings {
    /// Folder of Markdown notes (an Obsidian vault).
    #[arg(long, env = "CHAOS_KB_VAULT", global = true)]
    pub vault: Option<PathBuf>,
    /// Index file [default: <vault>/.chaos-kb/index.redb].
    #[arg(long, env = "CHAOS_KB_INDEX", global = true)]
    pub index: Option<PathBuf>,
    /// OpenAI-compatible embeddings base URL (Ollama, OpenRouter, OpenAI).
    #[arg(
        long,
        env = "CHAOS_KB_EMBED_URL",
        default_value = "http://localhost:11434/v1",
        global = true
    )]
    pub embed_url: String,
    /// Embedding model.
    #[arg(
        long,
        env = "CHAOS_KB_EMBED_MODEL",
        default_value = "embeddinggemma",
        global = true
    )]
    pub embed_model: String,
}

#[derive(Debug, Clone, PartialEq, Subcommand)]
pub enum Command {
    /// Bring the index up to date with the vault: rebuild the Merkle tree,
    /// embed only new or edited passages, drop deleted ones. Run after
    /// editing notes.
    Sync {
        /// Report what would change without embedding or writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Store one fact in the vault as Markdown and index it. Skipped when the
    /// same words are already stored anywhere in the vault.
    Remember {
        /// The fact, in plain words or Markdown. One fact per call.
        text: String,
        /// Note to append to, relative to the vault (`.md` optional).
        #[arg(long, default_value = INBOX)]
        note: String,
        /// Section heading inside the note (added when missing).
        #[arg(long)]
        heading: Option<String>,
    },
    /// Is this already in the knowledge base? Exact words are answered from
    /// the Merkle leaves without embedding; otherwise the nearest passages by
    /// cosine similarity decide. Returns where the knowledge sits.
    Check {
        /// A fact, topic or question.
        query: String,
        /// Similarity from which a passage counts as covering the query.
        #[arg(long, default_value_t = DEFAULT_MIN_SCORE)]
        min_score: f32,
    },
    /// Passages most similar to a question, best first, with note, section,
    /// line range and text.
    Search {
        /// A question or topic.
        query: String,
        /// Maximum passages returned.
        #[arg(long, default_value_t = 5)]
        limit: usize,
        /// Drop passages below this similarity.
        #[arg(long, default_value_t = 0.0)]
        min_score: f32,
    },
    /// Serve the tools to agents over MCP (stdio, newline-delimited JSON-RPC 2.0).
    Mcp,
}

/// Run one tool command against the configured knowledge base.
pub fn execute(settings: &Settings, command: &Command) -> Result<Value> {
    let _ = (settings, command);
    todo!()
}

/// `--index`, or `<vault>/.chaos-kb/index.redb`.
fn index_path(settings: &Settings, vault: &std::path::Path) -> PathBuf {
    let _ = (settings, vault);
    todo!()
}

pub fn main() -> ExitCode {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use std::path::Path;

    fn parse(argv: &[&str]) -> Command {
        Cli::try_parse_from(argv).unwrap().command
    }

    fn settings(vault: Option<&str>, index: Option<&str>) -> Settings {
        Settings {
            vault: vault.map(PathBuf::from),
            index: index.map(PathBuf::from),
            embed_url: "http://localhost:11434/v1".into(),
            embed_model: "embeddinggemma".into(),
        }
    }

    #[test]
    fn the_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn defaults() {
        assert_eq!(
            parse(&["chaos-kb", "sync"]),
            Command::Sync { dry_run: false }
        );
        assert_eq!(
            parse(&["chaos-kb", "remember", "tea"]),
            Command::Remember {
                text: "tea".into(),
                note: INBOX.into(),
                heading: None,
            }
        );
        assert_eq!(
            parse(&["chaos-kb", "check", "tea"]),
            Command::Check {
                query: "tea".into(),
                min_score: DEFAULT_MIN_SCORE,
            }
        );
        assert_eq!(
            parse(&["chaos-kb", "search", "tea"]),
            Command::Search {
                query: "tea".into(),
                limit: 5,
                min_score: 0.0,
            }
        );
    }

    #[test]
    fn check_with_min_score() {
        assert_eq!(
            parse(&["chaos-kb", "check", "x", "--min-score", "0.7"]),
            Command::Check {
                query: "x".into(),
                min_score: 0.7,
            }
        );
    }

    #[test]
    fn about_is_the_agent_workflow() {
        let about = Cli::command().get_long_about().map(|a| a.to_string());
        let about = about.expect("a workflow paragraph after the summary");
        for step in ["check", "search", "remember", "sync"] {
            assert!(about.contains(step), "{step}: {about}");
        }
    }

    #[test]
    fn index_defaults_into_the_vault() {
        let vault = Path::new("/notes");
        assert_eq!(
            index_path(&settings(None, None), vault),
            Path::new("/notes/.chaos-kb/index.redb")
        );
        assert_eq!(
            index_path(&settings(None, Some("/tmp/kb.redb")), vault),
            Path::new("/tmp/kb.redb")
        );
    }

    #[test]
    fn execute_needs_a_vault() {
        let e = execute(&settings(None, None), &Command::Sync { dry_run: true }).unwrap_err();
        assert_eq!(e.to_string(), "set --vault or CHAOS_KB_VAULT");
    }

    #[test]
    fn mcp_is_not_a_tool() {
        let e = execute(&settings(Some("/notes"), None), &Command::Mcp).unwrap_err();
        assert!(e.to_string().contains("mcp"), "{e}");
    }
}
