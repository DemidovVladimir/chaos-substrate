//! The command line — the single definition of every tool. Each subcommand's
//! doc comment and argument docs are also its MCP description and schema
//! (`entry::tools`), so the two surfaces cannot drift.

use super::{dashboard, mcp};
use crate::adapters::embedder::HttpEmbedder;
use crate::adapters::obsidian::ObsidianVault;
use crate::adapters::redb_index::RedbIndex;
use crate::application::Kb;
use crate::application::remember::INBOX;
use crate::application::topics::DEFAULT_TOPICS;
use crate::domain::path::NotePath;
use crate::domain::presence::DEFAULT_MIN_SCORE;
use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Personal knowledge base for agents: Markdown notes in, instant "is this
/// known?" checks and similarity search out.
///
/// Before a task, list the topics the knowledge base holds near it, then read
/// only the passages that would change how you do it (search returns the
/// nearest passages' text in one step instead). Before asking the user
/// something, check whether the knowledge base already knows it. When the
/// user tells you a new fact, remember it, one fact per call. When a stored
/// fact is wrong or no longer true, forget it in its stored words, then
/// remember the correction. After editing notes in the vault, sync.
#[derive(Debug, Parser)]
#[command(name = "chaos-kb", version, after_help = api_key_note())]
pub struct Cli {
    #[command(flatten)]
    pub settings: Settings,
    #[command(subcommand)]
    pub command: Command,
}

/// Where the knowledge lives and which embedder reads it.
#[derive(Debug, Clone, Args)]
pub struct Settings {
    /// Folder of Markdown notes (an Obsidian vault).
    #[arg(long, env = "CHAOS_KB_VAULT", global = true)]
    pub vault: Option<PathBuf>,
    /// Index file [default: <vault>/.chaos-kb/index.redb].
    #[arg(long, env = "CHAOS_KB_INDEX", global = true)]
    pub index: Option<PathBuf>,
    /// OpenAI-compatible embeddings base URL (Ollama, OpenRouter, OpenAI).
    // The URL may carry credentials, so `--help` never prints its env value.
    #[arg(
        long,
        env = "CHAOS_KB_EMBED_URL",
        hide_env_values = true,
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

/// The only source of the embeddings API key: never a flag, so it stays out of
/// shell history and process lists.
pub const API_KEY_ENV: &str = "CHAOS_KB_EMBED_API_KEY";

/// Shown after `--help` and under the README settings.
pub fn api_key_note() -> String {
    format!(
        "The embeddings API key, when the endpoint needs one, is read only from the \
         `{API_KEY_ENV}` environment variable, never from a flag. It is sent only over \
         `https://`, or over plain `http://` to this machine (`localhost`, `127.0.0.0/8`, \
         `::1`)."
    )
}

#[derive(Debug, Clone, PartialEq, Subcommand)]
pub enum Command {
    #[command(flatten)]
    Tool(Tool),
    /// Serve the tools to agents over MCP (stdio, newline-delimited JSON-RPC 2.0).
    Mcp,
    /// Serve the dashboard, a web page on this machine only
    /// (http://127.0.0.1:<port>): every note and block the knowledge base
    /// holds and whether the index is up to date, with controls to add,
    /// replace and remove facts and to try a question. Every change re-syncs
    /// the index.
    Dashboard {
        /// Port on 127.0.0.1.
        #[arg(long, default_value_t = dashboard::DEFAULT_PORT)]
        port: u16,
    },
}

/// The tools: every subcommand except `mcp`, which serves them.
#[derive(Debug, Clone, PartialEq, Subcommand)]
pub enum Tool {
    /// Bring the index up to date with the vault: compare each note's hash
    /// with the stored one, embed only new or edited passages, drop deleted
    /// ones. Run after editing notes.
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
    /// Take a fact out of the vault and the index: every block with the same
    /// words, compared as fingerprints (case, spacing and punctuation aside),
    /// never a passage that only means the same. Reports where the removed
    /// blocks sat; nothing is removed when no block has these words.
    Forget {
        /// The fact in its stored words, as `read` or `search` return them.
        /// Each block of it is removed wherever it is stored.
        text: String,
        /// Only remove it from this note (relative to the vault, `.md`
        /// optional).
        #[arg(long)]
        note: Option<String>,
    },
    /// Is this already in the knowledge base? Exact words are answered from
    /// the stored block fingerprints without embedding; otherwise the nearest
    /// passages by cosine similarity decide. Returns where the knowledge sits.
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
    /// What the knowledge base holds near a task, as a short list: one line
    /// per passage (id, section, similarity, first words), best first,
    /// without the text. Keep the ids that would change how the task is done
    /// and fetch them with `read`; when none would, the knowledge base has
    /// nothing for this task.
    Topics {
        /// The task or request, in the user's words.
        task: String,
        /// Maximum passages listed.
        #[arg(long, default_value_t = DEFAULT_TOPICS)]
        limit: usize,
    },
    /// The passages behind ids from `topics`, with their text, in the order
    /// given. Embeds nothing. Ids the index no longer holds come back as
    /// `missing` (the note changed: list the topics again).
    Read {
        /// Passage ids from `topics` (`note:first-last`, like `Car.md:8-8`).
        #[arg(required = true)]
        ids: Vec<String>,
    },
}

/// Open the configured knowledge base and hand it to `use_kb`. Fails when
/// no vault is set or its folder is missing; nothing else is opened or
/// contacted before `use_kb` asks for it.
pub fn with_kb<T>(settings: &Settings, use_kb: impl FnOnce(Kb<'_>) -> Result<T>) -> Result<T> {
    let root = settings
        .vault
        .as_deref()
        .context("set --vault or CHAOS_KB_VAULT")?;
    let vault = ObsidianVault::new(root)?;
    let index = RedbIndex::new(&index_path(settings, root));
    let api_key = std::env::var(API_KEY_ENV).ok();
    let embedder = HttpEmbedder::new(&settings.embed_url, &settings.embed_model, api_key);
    use_kb(Kb {
        vault: &vault,
        index: &index,
        embedder: &embedder,
    })
}

/// Run one tool against the configured knowledge base.
pub fn execute(settings: &Settings, tool: &Tool) -> Result<Value> {
    with_kb(settings, |kb| run(kb, tool))
}

/// Run one tool on `kb`.
fn run(kb: Kb<'_>, tool: &Tool) -> Result<Value> {
    match tool {
        Tool::Sync { dry_run } => json(kb.sync(*dry_run)),
        Tool::Remember {
            text,
            note,
            heading,
        } => json(kb.remember(text, &NotePath::parse(note)?, heading.as_deref())),
        Tool::Forget { text, note } => {
            let note = note.as_deref().map(NotePath::parse).transpose()?;
            json(kb.forget(text, note.as_ref()))
        }
        Tool::Check { query, min_score } => json(kb.check(query, *min_score)),
        Tool::Search {
            query,
            limit,
            min_score,
        } => json(kb.search(query, *limit, *min_score)),
        Tool::Topics { task, limit } => json(kb.topics(task, *limit)),
        Tool::Read { ids } => json(kb.read(ids)),
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
        Command::Dashboard { port } => dashboard::serve(&settings, port),
        Command::Tool(tool) => execute(&settings, &tool).and_then(|report| {
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
    fn about_is_the_agent_workflow() {
        let about = Cli::command().get_long_about().map(|a| a.to_string());
        let about = about.expect("a workflow paragraph after the summary");
        for step in [
            "topics", "read", "check", "search", "remember", "forget", "sync",
        ] {
            assert!(about.contains(step), "{step}: {about}");
        }
    }

    #[test]
    fn help_never_prints_the_embed_url_from_the_environment() {
        // Tests cannot set variables (no `unsafe`), so `PATH`, which is
        // always set, stands in for `CHAOS_KB_EMBED_URL`.
        let path = std::env::var("PATH").unwrap();
        let mut cli = Cli::command().mut_arg("embed_url", |a| a.env("PATH"));
        let help = cli.render_long_help().to_string();
        assert!(help.contains("[env: PATH]"), "{help}");
        assert!(!help.contains(&path), "{help}");
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
        let e = execute(&settings(None, None), &Tool::Sync { dry_run: true }).unwrap_err();
        assert_eq!(e.to_string(), "set --vault or CHAOS_KB_VAULT");
    }
}
