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
use anyhow::{Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use serde_json::{Value, json};
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
///
/// Several knowledge bases may be configured, and instances lists them by
/// name. Pass instance to any other tool to use one other than the default.
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
    /// Folder of Markdown notes (an Obsidian vault), as PATH or NAME=PATH.
    /// Repeat it to serve several knowledge bases. A bare PATH gets the name
    /// `default`. Calls use the first vault unless `--instance` names another.
    #[arg(
        long,
        env = "CHAOS_KB_VAULT",
        global = true,
        value_name = "[NAME=]PATH"
    )]
    pub vault: Vec<PathBuf>,
    /// Knowledge base to use, by name; without it, the default. `instances`
    /// lists the names and marks the default.
    #[arg(long, env = "CHAOS_KB_INSTANCE", global = true, value_name = "NAME")]
    pub instance: Option<String>,
    /// Index file, with a single `--vault` only [default:
    /// <vault>/.chaos-kb/index.redb].
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
    /// same words are already stored anywhere in the vault. When the index cannot
    /// take it (the embedder is down, say), the note is put back and the error
    /// starts with `nothing was changed`.
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
    /// blocks sat; nothing is removed when no block has these words. When the
    /// index cannot follow (the embedder is down, say), the notes are put back
    /// and the error starts with `nothing was changed`.
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
        // `id` is the field name in the topics rows, so an MCP call may use it.
        #[arg(required = true, alias = "id")]
        ids: Vec<String>,
    },
    /// The configured knowledge bases: each one's name, vault folder and
    /// index file, with `"default": true` on the one a call uses when it names
    /// no `instance`. Opens nothing, so it works while the embedder is down.
    Instances,
}

/// The name of a vault given as a bare PATH.
pub const DEFAULT_INSTANCE: &str = "default";

/// The id of the `--instance` setting, which every tool but `instances` also
/// takes as an argument.
pub const INSTANCE: &str = "instance";

/// One configured knowledge base: its name, vault folder and index file.
#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    pub name: String,
    pub vault: PathBuf,
    pub index: PathBuf,
}

/// The configured knowledge bases, in `--vault` order, and the one a call
/// uses when it names none.
#[derive(Debug, Clone, PartialEq)]
pub struct Instances {
    all: Vec<Instance>,
    default: usize,
}

impl Instances {
    /// Every instance, in `--vault` order.
    pub fn all(&self) -> &[Instance] {
        &self.all
    }

    /// The instance called `name`, or the default one when `None`.
    pub fn get(&self, name: Option<&str>) -> Result<&Instance> {
        let at = match name {
            Some(name) => self.position(name)?,
            None => self.default,
        };
        Ok(&self.all[at])
    }

    /// Whether more than one knowledge base is configured.
    pub fn several(&self) -> bool {
        self.all.len() > 1
    }

    /// Each instance's name, vault folder and index file, with
    /// `"default": true` on the default one.
    pub fn list(&self) -> Value {
        let rows = self.all.iter().enumerate().map(|(at, instance)| {
            json!({
                "name": instance.name,
                "vault": instance.vault.display().to_string(),
                "index": instance.index.display().to_string(),
                "default": at == self.default,
            })
        });
        Value::Array(rows.collect())
    }

    fn position(&self, name: &str) -> Result<usize> {
        self.all.iter().position(|i| i.name == name).ok_or_else(|| {
            let names: Vec<String> = self.all.iter().map(|i| format!("`{}`", i.name)).collect();
            anyhow!(
                "no instance is named `{name}`; the instances are {}",
                names.join(", ")
            )
        })
    }
}

impl Settings {
    /// The knowledge bases `--vault`, `--instance` and `--index` configure.
    /// Reads no file, so a wrong setting fails before anything is opened.
    pub fn instances(&self) -> Result<Instances> {
        if self.vault.is_empty() {
            bail!("set --vault or CHAOS_KB_VAULT");
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut all: Vec<Instance> = Vec::new();
        for value in &self.vault {
            let (name, path) = named(value);
            let name = name.unwrap_or(DEFAULT_INSTANCE);
            if path.as_os_str().is_empty() {
                bail!("--vault {} names no folder", value.display());
            }
            if all.iter().any(|i| i.name == name) {
                bail!(
                    "two vaults are named `{name}`: give each one its own name, as --vault NAME=PATH"
                );
            }
            let vault = expand_home(path, home.as_deref());
            all.push(Instance {
                name: name.to_string(),
                index: vault.join(".chaos-kb").join("index.redb"),
                vault,
            });
        }
        if let Some(index) = &self.index {
            if all.len() > 1 {
                bail!(
                    "--index (CHAOS_KB_INDEX) takes a single --vault: with several, each \
                     keeps its index at <vault>/.chaos-kb/index.redb"
                );
            }
            all[0].index = index.clone();
        }
        let mut instances = Instances { all, default: 0 };
        if let Some(name) = &self.instance {
            instances.default = instances.position(name)?;
        }
        Ok(instances)
    }
}

/// A `--vault` value as its name and folder: `NAME=PATH`, or a bare PATH
/// with no name. When the text before the first `=` is not a name, the whole
/// value is the path, so `/a/b=c` stays a path.
fn named(value: &Path) -> (Option<&str>, &Path) {
    let split = value.to_str().and_then(|v| v.split_once('='));
    match split.filter(|(name, _)| is_name(name)) {
        Some((name, path)) => (Some(name), Path::new(path)),
        None => (None, value),
    }
}

/// An instance name: ASCII letters, digits, `_` and `-`.
fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `path` with a leading `~` read as the home folder: no shell expands it
/// after `NAME=`, and an MCP client's config passes it on as it is.
fn expand_home(path: &Path, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~"), home) {
        (Ok(rest), Some(home)) if rest.as_os_str().is_empty() => home.to_path_buf(),
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

/// Open `instance` and hand it to `use_kb`. Fails when its vault folder is
/// missing; nothing else is opened or contacted before `use_kb` asks for it.
pub fn with_kb<T>(
    settings: &Settings,
    instance: &Instance,
    use_kb: impl FnOnce(Kb<'_>) -> Result<T>,
) -> Result<T> {
    let vault = ObsidianVault::new(&instance.vault)?;
    let index = RedbIndex::new(&instance.index);
    let api_key = std::env::var(API_KEY_ENV).ok();
    let embedder = HttpEmbedder::new(&settings.embed_url, &settings.embed_model, api_key);
    use_kb(Kb {
        vault: &vault,
        index: &index,
        embedder: &embedder,
    })
}

/// Run one tool: `instances` from the settings alone, every other tool on
/// the knowledge base `instance` names, or the default one when `None`. With
/// several instances, a report that is a JSON object names the one that
/// answered.
pub fn execute(settings: &Settings, instance: Option<&str>, tool: &Tool) -> Result<Value> {
    let instances = settings.instances()?;
    let on = |use_kb: &dyn Fn(Kb<'_>) -> Result<Value>| -> Result<Value> {
        let chosen = instances.get(instance)?;
        let mut report = with_kb(settings, chosen, use_kb)?;
        if instances.several()
            && let Some(fields) = report.as_object_mut()
        {
            fields.insert(INSTANCE.into(), json!(chosen.name));
        }
        Ok(report)
    };
    match tool {
        Tool::Instances => Ok(json!({ "instances": instances.list() })),
        Tool::Sync { dry_run } => on(&|kb| report(kb.sync(*dry_run))),
        Tool::Remember {
            text,
            note,
            heading,
        } => on(&|kb| report(kb.remember(text, &NotePath::parse(note)?, heading.as_deref()))),
        Tool::Forget { text, note } => on(&|kb| {
            let note = note.as_deref().map(NotePath::parse).transpose()?;
            report(kb.forget(text, note.as_ref()))
        }),
        Tool::Check { query, min_score } => on(&|kb| report(kb.check(query, *min_score))),
        Tool::Search {
            query,
            limit,
            min_score,
        } => on(&|kb| report(kb.search(query, *limit, *min_score))),
        Tool::Topics { task, limit } => on(&|kb| report(kb.topics(task, *limit))),
        Tool::Read { ids } => on(&|kb| report(kb.read(ids))),
    }
}

/// A use-case report as JSON.
fn report(report: Result<impl Serialize>) -> Result<Value> {
    Ok(serde_json::to_value(report?)?)
}

/// Parse the command line; `mcp` serves, every other command prints its
/// report as pretty JSON on stdout.
pub fn main() -> ExitCode {
    let Cli { settings, command } = Cli::parse();
    let outcome = match command {
        Command::Mcp => mcp::serve(&settings),
        Command::Dashboard { port } => dashboard::serve(&settings, port),
        Command::Tool(tool) => execute(&settings, None, &tool).and_then(|report| {
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

    fn settings(vaults: &[&str], instance: Option<&str>, index: Option<&str>) -> Settings {
        Settings {
            vault: vaults.iter().map(PathBuf::from).collect(),
            instance: instance.map(str::to_string),
            index: index.map(PathBuf::from),
            embed_url: "http://127.0.0.1:9/v1".into(),
            embed_model: "none".into(),
        }
    }

    fn instances(vaults: &[&str]) -> Instances {
        settings(vaults, None, None).instances().unwrap()
    }

    fn refusal(vaults: &[&str], instance: Option<&str>, index: Option<&str>) -> String {
        format!(
            "{:#}",
            settings(vaults, instance, index).instances().unwrap_err()
        )
    }

    /// `(name, vault, index)` of every instance.
    fn rows(instances: &Instances) -> Vec<(String, PathBuf, PathBuf)> {
        let rows = instances.all().iter();
        rows.map(|i| (i.name.clone(), i.vault.clone(), i.index.clone()))
            .collect()
    }

    fn row(name: &str, vault: &str, index: &str) -> (String, PathBuf, PathBuf) {
        (name.into(), vault.into(), index.into())
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
            "topics",
            "read",
            "check",
            "search",
            "remember",
            "forget",
            "sync",
            "instances",
            "instance",
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
    fn vault_repeats_and_instance_picks_one() {
        let cli = Cli::try_parse_from([
            "chaos-kb",
            "--vault",
            "personal=/notes",
            "--vault",
            "/work",
            "--instance",
            "personal",
            "sync",
        ])
        .unwrap();
        let vaults = [PathBuf::from("personal=/notes"), PathBuf::from("/work")];
        assert_eq!(cli.settings.vault, vaults);
        assert_eq!(cli.settings.instance.as_deref(), Some("personal"));
    }

    #[test]
    fn a_bare_path_is_named_default_and_keeps_its_index_inside() {
        assert_eq!(
            rows(&instances(&["/notes"])),
            [row("default", "/notes", "/notes/.chaos-kb/index.redb")]
        );
        let index = settings(&["/notes"], None, Some("/tmp/kb.redb"));
        assert_eq!(
            rows(&index.instances().unwrap()),
            [row("default", "/notes", "/tmp/kb.redb")]
        );
    }

    #[test]
    fn name_eq_path_names_the_vault() {
        assert_eq!(
            rows(&instances(&[
                "personal=/home/me/Knowledge",
                "work_2-b=/srv/kb"
            ])),
            [
                row(
                    "personal",
                    "/home/me/Knowledge",
                    "/home/me/Knowledge/.chaos-kb/index.redb"
                ),
                row("work_2-b", "/srv/kb", "/srv/kb/.chaos-kb/index.redb"),
            ]
        );
        // Only the first `=` splits.
        assert_eq!(
            rows(&instances(&["work=/a/b=c"])),
            [row("work", "/a/b=c", "/a/b=c/.chaos-kb/index.redb")]
        );
    }

    #[test]
    fn text_before_the_first_eq_that_is_no_name_leaves_the_whole_path() {
        for path in ["/a/b=c", "./x=y", "my notes=z", "a.b=c", "=x", "é=x"] {
            let found = rows(&instances(&[path]));
            let index = format!("{path}/.chaos-kb/index.redb");
            assert_eq!(found, [row("default", path, &index)], "{path}");
        }
    }

    #[test]
    fn two_vaults_with_one_name_are_refused() {
        let e = refusal(&["/a", "/b"], None, None);
        assert!(e.contains("`default`") && e.contains("NAME=PATH"), "{e}");
        let e = refusal(&["work=/a", "personal=/p", "work=/b"], None, None);
        assert!(e.contains("`work`") && e.contains("NAME=PATH"), "{e}");
        assert!(!e.contains("`personal`"), "{e}");
    }

    #[test]
    fn a_name_without_a_folder_is_refused() {
        let e = refusal(&["work="], None, None);
        assert!(e.contains("work="), "{e}");
    }

    #[test]
    fn index_takes_a_single_vault() {
        let e = refusal(&["a=/a", "b=/b"], None, Some("/tmp/kb.redb"));
        assert!(e.contains("--index"), "{e}");
        assert!(e.contains("<vault>/.chaos-kb/index.redb"), "{e}");
    }

    #[test]
    fn the_default_is_the_first_vault_or_the_one_instance_names() {
        let vaults = ["personal=/p", "molecule=/m"];
        let first = settings(&vaults, None, None).instances().unwrap();
        assert_eq!(first.get(None).unwrap().name, "personal");
        let chosen = settings(&vaults, Some("molecule"), None)
            .instances()
            .unwrap();
        assert_eq!(chosen.get(None).unwrap().name, "molecule");
        assert_eq!(chosen.get(Some("personal")).unwrap().name, "personal");
        assert!(chosen.several());
        assert!(!instances(&["/p"]).several());
    }

    #[test]
    fn an_unknown_instance_is_refused_with_the_names_configured() {
        let vaults = ["personal=/p", "molecule=/m"];
        let e = refusal(&vaults, Some("nope"), None);
        for name in ["`nope`", "`personal`", "`molecule`"] {
            assert!(e.contains(name), "{name}: {e}");
        }
        let configured = settings(&vaults, None, None).instances().unwrap();
        let e = format!("{:#}", configured.get(Some("nope")).unwrap_err());
        for name in ["`nope`", "`personal`", "`molecule`"] {
            assert!(e.contains(name), "{name}: {e}");
        }
    }

    #[test]
    fn a_leading_tilde_is_the_home_folder() {
        let home = Some(Path::new("/home/me"));
        let expand = |path: &str| expand_home(Path::new(path), home);
        assert_eq!(expand("~/Knowledge"), Path::new("/home/me/Knowledge"));
        assert_eq!(expand("~"), Path::new("/home/me"));
        for kept in ["~other/x", "/a/~/b", "./~", "Knowledge"] {
            assert_eq!(expand(kept), Path::new(kept), "{kept}");
        }
        let unset = expand_home(Path::new("~/Knowledge"), None);
        assert_eq!(unset, Path::new("~/Knowledge"));
    }

    #[test]
    fn execute_needs_a_vault() {
        let none = settings(&[], None, None);
        for tool in [Tool::Sync { dry_run: true }, Tool::Instances] {
            let e = execute(&none, None, &tool).unwrap_err();
            assert_eq!(e.to_string(), "set --vault or CHAOS_KB_VAULT");
        }
    }

    #[test]
    fn instances_lists_every_vault_and_opens_none() {
        // Neither folder exists, and the embedder is unreachable.
        let configured = settings(
            &["personal=/nowhere/p", "molecule=/nowhere/m"],
            Some("molecule"),
            None,
        );
        let report = execute(&configured, None, &Tool::Instances).unwrap();
        assert_eq!(
            report,
            json!({"instances": [
                {"name": "personal", "vault": "/nowhere/p",
                 "index": "/nowhere/p/.chaos-kb/index.redb", "default": false},
                {"name": "molecule", "vault": "/nowhere/m",
                 "index": "/nowhere/m/.chaos-kb/index.redb", "default": true},
            ]})
        );
        let one = execute(
            &settings(&["/nowhere/p"], None, None),
            None,
            &Tool::Instances,
        );
        assert_eq!(one.unwrap()["instances"][0]["name"], "default");
    }

    #[test]
    fn with_several_instances_a_report_names_the_one_that_answered() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(a.path().join("A.md"), "# A\n\nTea at five.\n").unwrap();
        std::fs::write(b.path().join("B.md"), "# B\n\nCoffee at nine.\n").unwrap();
        let (a, b) = (a.path().display(), b.path().display());
        let two = settings(&[&format!("a={a}"), &format!("b={b}")], None, None);
        let dry = Tool::Sync { dry_run: true };

        let first = execute(&two, None, &dry).unwrap();
        assert_eq!(
            (&first["instance"], &first["added"]),
            (&json!("a"), &json!(["A.md"]))
        );
        let named = execute(&two, Some("b"), &dry).unwrap();
        assert_eq!(
            (&named["instance"], &named["added"]),
            (&json!("b"), &json!(["B.md"]))
        );
        let e = execute(&two, Some("c"), &dry).unwrap_err().to_string();
        assert!(
            e.contains("`c`") && e.contains("`a`") && e.contains("`b`"),
            "{e}"
        );

        // With one instance the report is the use-case's alone.
        let one = execute(&settings(&[&a.to_string()], None, None), None, &dry).unwrap();
        assert_eq!(one["added"], json!(["A.md"]));
        assert!(one.get("instance").is_none(), "{one}");
    }
}
