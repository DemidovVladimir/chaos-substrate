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

pub fn main() -> ExitCode {
    todo!()
}
