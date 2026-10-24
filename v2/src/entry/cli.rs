//! The command line — the single definition of every tool. Each subcommand's
//! doc comment and argument docs are also its MCP description and schema
//! (`entry::tools`), so the two surfaces cannot drift.

use super::mcp;
use crate::adapters::embedder::HttpEmbedder;
use crate::adapters::obsidian::ObsidianVault;
use crate::adapters::redb_index::RedbIndex;
use crate::application::Kb;
use crate::application::remember::INBOX;
use crate::domain::path::NotePath;
use crate::domain::presence::DEFAULT_MIN_SCORE;
use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Personal knowledge base for agents: Markdown notes in, instant "is this
/// known?" checks and similarity search out.
///
/// Before asking the user something, check whether the knowledge base
/// already knows it, then search to read the passages. When the user tells
/// you a new fact, remember it, one fact per call. After editing notes in the
/// vault, sync.
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
    ensure!(
        *command != Command::Mcp,
        "`mcp` serves the tools; it is not one of them"
    );
    let root = settings
        .vault
        .as_deref()
        .context("set --vault or CHAOS_KB_VAULT")?;
    let vault = ObsidianVault::new(root)?;
    let index = RedbIndex::new(&index_path(settings, root));
    let api_key = std::env::var("CHAOS_KB_EMBED_API_KEY").ok();
    let embedder = HttpEmbedder::new(&settings.embed_url, &settings.embed_model, api_key);
    let kb = Kb {
        vault: &vault,
        index: &index,
        embedder: &embedder,
    };
    match command {
        Command::Sync { dry_run } => json(kb.sync(*dry_run)),
        Command::Remember {
            text,
            note,
            heading,
        } => json(kb.remember(text, &NotePath::parse(note)?, heading.as_deref())),
        Command::Check { query, min_score } => json(kb.check(query, *min_score)),
        Command::Search {
            query,
            limit,
            min_score,
        } => json(kb.search(query, *limit, *min_score)),
        Command::Mcp => unreachable!("rejected above"),
    }
}

/// A use-case report as JSON.
fn json(report: Result<impl Serialize>) -> Result<Value> {
    Ok(serde_json::to_value(report?)?)
}

/// `--index`, or `<vault>/.chaos-kb/index.redb`.
fn index_path(settings: &Settings, vault: &Path) -> PathBuf {
    settings
        .index
        .clone()
        .unwrap_or_else(|| vault.join(".chaos-kb").join("index.redb"))
}

/// Parse the command line; `mcp` serves, every other command prints its
/// report as pretty JSON on stdout.
pub fn main() -> ExitCode {
    let Cli { settings, command } = Cli::parse();
    let outcome = match command {
        Command::Mcp => mcp::serve(&settings),
        tool => execute(&settings, &tool).and_then(|report| {
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
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
