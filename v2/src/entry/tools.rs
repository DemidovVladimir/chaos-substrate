//! The MCP tool roster, derived from the clap definition in `entry::cli`:
//! one tool per subcommand (except `mcp`), named `kb_<subcommand>`, described
//! by the subcommand's doc comment, with a JSON Schema built from its
//! arguments. Calls are turned back into argv and parsed by the same clap
//! definition, so defaults and validation live in exactly one place.

use super::cli::{Cli, Command, api_key_note};
use anyhow::{Context, Result, anyhow, bail};
use clap::builder::StyledStr;
use clap::{Arg, ArgAction, CommandFactory, Parser, Subcommand};
use serde_json::{Map, Value, json};
use std::any::TypeId;

/// Prefix of every MCP tool name.
pub const PREFIX: &str = "kb_";

/// The subcommand that serves the tools rather than being one.
const SERVER: &str = "mcp";

/// One MCP tool.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    /// MCP name, e.g. `kb_check`.
    pub name: String,
    pub description: String,
    /// JSON Schema (`type: object`) of the arguments.
    pub input_schema: Value,
}

/// Every tool, in subcommand order.
pub fn tools() -> Vec<Tool> {
    subcommands(&cli()).map(tool).collect()
}

/// Parse an MCP call (`kb_check`, `{"query": "…"}`) into a [`Command`] through
/// clap, exactly as the CLI would. Unknown tools and invalid arguments fail
/// with clap's message.
pub fn command(name: &str, arguments: &Value) -> Result<Command> {
    let cli = cli();
    let sub = subcommand(&cli, name)?;
    let given = arguments
        .as_object()
        .with_context(|| format!("{name} arguments must be a JSON object"))?;
    let mut argv = vec![cli.get_name().to_string(), sub.get_name().to_string()];
    let mut positionals = Vec::new();
    for (key, value) in given {
        let arg = own_args(sub)
            .find(|a| a.get_id() == key.as_str())
            .ok_or_else(|| unknown_key(sub, name, key))?;
        match (Kind::of(arg), value) {
            (_, Value::Null) => {}
            (Kind::Boolean, Value::Bool(set)) => {
                if *set {
                    argv.push(format!("--{}", long(arg)?));
                }
            }
            (Kind::Boolean, _) => bail!("`{key}` must be true or false"),
            (_, Value::String(_) | Value::Number(_)) => {
                let text = value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_string);
                match arg.get_index() {
                    Some(index) => positionals.push((index, text)),
                    None => argv.push(format!("--{}={text}", long(arg)?)),
                }
            }
            (kind, _) => bail!("`{key}` must be a {}", kind.name()),
        }
    }
    if !positionals.is_empty() {
        positionals.sort();
        argv.push("--".into());
        argv.extend(positionals.into_iter().map(|(_, text)| text));
    }
    Call::try_parse_from(argv)
        .map(|call| call.command)
        .map_err(|e| anyhow!("{}", e.to_string().trim_end()))
}

/// The agent workflow guide: the CLI's about text.
pub fn instructions() -> String {
    let cli = Cli::command();
    help(cli.get_long_about(), cli.get_about())
}

/// The Markdown tool reference embedded in README.md between the
/// `<!-- tools:start -->` and `<!-- tools:end -->` markers.
pub fn markdown() -> String {
    let cli = cli();
    let mut sections: Vec<String> = tools().iter().map(|t| section(&cli, t)).collect();
    sections.push(settings(&cli));
    sections.join("\n")
}

/// The tool subcommands without the CLI's settings: what one MCP call may set.
#[derive(Parser)]
struct Call {
    #[command(subcommand)]
    command: Command,
}

/// The JSON type of an argument, read from its clap action and value parser.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Boolean,
    Integer { unsigned: bool },
    Number,
    String,
}

impl Kind {
    fn of(arg: &Arg) -> Kind {
        if matches!(arg.get_action(), ArgAction::SetTrue) {
            return Kind::Boolean;
        }
        // clap reports the parsed value's type; `AnyValueId` compares with `TypeId`.
        let parsed = arg.get_value_parser().type_id();
        let is_any = |types: &[TypeId]| types.iter().any(|t| parsed == *t);
        let unsigned = [
            TypeId::of::<u8>(),
            TypeId::of::<u16>(),
            TypeId::of::<u32>(),
            TypeId::of::<u64>(),
            TypeId::of::<usize>(),
        ];
        let signed = [
            TypeId::of::<i8>(),
            TypeId::of::<i16>(),
            TypeId::of::<i32>(),
            TypeId::of::<i64>(),
            TypeId::of::<isize>(),
        ];
        if is_any(&unsigned) {
            Kind::Integer { unsigned: true }
        } else if is_any(&signed) {
            Kind::Integer { unsigned: false }
        } else if is_any(&[TypeId::of::<f32>(), TypeId::of::<f64>()]) {
            Kind::Number
        } else {
            Kind::String
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Boolean => "boolean",
            Kind::Integer { .. } => "integer",
            Kind::Number => "number",
            Kind::String => "string",
        }
    }

    /// A clap value (a default) as JSON of this type.
    fn json(self, raw: &str) -> Value {
        let typed = match self {
            Kind::Boolean => raw.parse::<bool>().ok().map(Value::from),
            Kind::Integer { .. } => raw.parse::<i64>().ok().map(Value::from),
            Kind::Number => raw.parse::<f64>().ok().map(Value::from),
            Kind::String => None,
        };
        typed.unwrap_or_else(|| Value::from(raw))
    }
}

/// The clap definition, built: help flags added, settings propagated to every
/// subcommand, implied defaults (`false` for flags) filled in.
fn cli() -> clap::Command {
    let mut cli = Cli::command();
    cli.build();
    cli
}

/// The subcommands that are tools: those of [`Command`] except the server
/// (clap's generated `help` subcommand is not one of them).
fn subcommands(cli: &clap::Command) -> impl Iterator<Item = &clap::Command> {
    cli.get_subcommands()
        .filter(|s| Command::has_subcommand(s.get_name()) && s.get_name() != SERVER)
}

/// The subcommand behind an MCP tool name.
fn subcommand<'a>(cli: &'a clap::Command, tool: &str) -> Result<&'a clap::Command> {
    let name = tool.strip_prefix(PREFIX);
    subcommands(cli)
        .find(|s| Some(s.get_name()) == name)
        .ok_or_else(|| {
            let known: Vec<String> = subcommands(cli)
                .map(|s| format!("{PREFIX}{}", s.get_name()))
                .collect();
            anyhow!("unknown tool `{tool}`; the tools are {}", known.join(", "))
        })
}

/// A subcommand's own arguments: no help flag, no global settings.
fn own_args(sub: &clap::Command) -> impl Iterator<Item = &Arg> {
    sub.get_arguments().filter(|a| {
        let generated = matches!(
            a.get_action(),
            ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
        );
        !generated && !a.is_global_set()
    })
}

fn unknown_key(sub: &clap::Command, tool: &str, key: &str) -> anyhow::Error {
    let known: Vec<String> = own_args(sub).map(|a| format!("`{}`", a.get_id())).collect();
    anyhow!(
        "{tool} has no argument `{key}`; it takes {}",
        known.join(", ")
    )
}

fn long(arg: &Arg) -> Result<&str> {
    arg.get_long()
        .with_context(|| format!("`{}` has no --long flag", arg.get_id()))
}

/// What `--help` shows: the long text when there is one, else the short one.
fn help(long: Option<&StyledStr>, short: Option<&StyledStr>) -> String {
    long.or(short).map(ToString::to_string).unwrap_or_default()
}

fn tool(sub: &clap::Command) -> Tool {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for arg in own_args(sub) {
        let id = arg.get_id().to_string();
        if arg.is_required_set() {
            required.push(Value::from(id.as_str()));
        }
        properties.insert(id, property(arg));
    }
    let mut input_schema = json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false,
    });
    if !required.is_empty() {
        input_schema["required"] = Value::Array(required);
    }
    Tool {
        name: format!("{PREFIX}{}", sub.get_name()),
        description: help(sub.get_long_about(), sub.get_about()),
        input_schema,
    }
}

fn property(arg: &Arg) -> Value {
    let kind = Kind::of(arg);
    let mut property = json!({
        "type": kind.name(),
        "description": help(arg.get_long_help(), arg.get_help()),
    });
    if kind == (Kind::Integer { unsigned: true }) {
        property["minimum"] = json!(0);
    }
    if let [default] = arg.get_default_values() {
        property["default"] = kind.json(&default.to_string_lossy());
    }
    property
}

/// One tool: heading, description, and an args table (required args first).
fn section(cli: &clap::Command, tool: &Tool) -> String {
    let sub = &tool.name[PREFIX.len()..];
    let mut out = format!(
        "### `{}` · `{} {sub}`\n\n{}\n",
        tool.name,
        cli.get_name(),
        prose(&tool.description)
    );
    let schema = &tool.input_schema;
    let required = |name: &str| {
        schema["required"]
            .as_array()
            .is_some_and(|r| r.iter().any(|v| v == name))
    };
    let mut rows: Vec<_> = schema["properties"]
        .as_object()
        .into_iter()
        .flatten()
        .collect();
    if rows.is_empty() {
        return out;
    }
    rows.sort_by_key(|(name, _)| !required(name));
    out.push_str("\n| arg | type | default | description |\n|---|---|---|---|\n");
    for (name, property) in rows {
        let mark = if required(name) { " (required)" } else { "" };
        let mut kind = property["type"].as_str().unwrap_or_default().to_string();
        if let Some(minimum) = property.get("minimum") {
            kind = format!("{kind} ≥ {minimum}");
        }
        let default = match property.get("default") {
            Some(Value::String(s)) => format!("`{s}`"),
            Some(v) => format!("`{v}`"),
            None => String::new(),
        };
        let description = cell(property["description"].as_str().unwrap_or_default());
        out.push_str(&format!(
            "| `{name}`{mark} | {kind} | {default} | {description} |\n"
        ));
    }
    out
}

/// The global settings every command takes, with their environment variables.
fn settings(cli: &clap::Command) -> String {
    let mut out = String::from(
        "### Settings\n\n| flag | env var | default | description |\n|---|---|---|---|\n",
    );
    for arg in cli.get_arguments().filter(|a| a.is_global_set()) {
        let flag = arg
            .get_long()
            .map(|l| format!("`--{l}`"))
            .unwrap_or_default();
        let env = arg
            .get_env()
            .map(|e| format!("`{}`", e.to_string_lossy()))
            .unwrap_or_default();
        let default: Vec<String> = arg
            .get_default_values()
            .iter()
            .map(|d| format!("`{}`", d.to_string_lossy()))
            .collect();
        let description = cell(&help(arg.get_long_help(), arg.get_help()));
        out.push_str(&format!(
            "| {flag} | {env} | {} | {description} |\n",
            default.join(", ")
        ));
    }
    out.push_str(&format!("\n{}\n", api_key_note()));
    out
}

/// Markdown prose: `<` outside code spans escaped, so `<vault>` is not HTML.
fn prose(text: &str) -> String {
    let parts: Vec<String> = text
        .split('`')
        .enumerate()
        .map(|(i, part)| match i % 2 {
            0 => part.replace('<', "&lt;"),
            _ => part.to_string(),
        })
        .collect();
    parts.join("`")
}

/// One table cell: prose on a single line, pipes escaped.
fn cell(text: &str) -> String {
    prose(text).replace('|', "\\|").replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::remember::INBOX;
    use crate::domain::presence::DEFAULT_MIN_SCORE;
    use crate::entry::cli::Cli;
    use clap::CommandFactory;
    use serde_json::json;

    fn tool(name: &str) -> Tool {
        tools().into_iter().find(|t| t.name == name).expect(name)
    }

    fn schema(name: &str) -> Value {
        tool(name).input_schema
    }

    fn call(name: &str, arguments: Value) -> Result<Command> {
        command(name, &arguments)
    }

    fn rejection(name: &str, arguments: Value) -> String {
        format!("{:#}", call(name, arguments).unwrap_err())
    }

    #[test]
    fn one_tool_per_subcommand_except_mcp() {
        let names: Vec<String> = tools().into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["kb_sync", "kb_remember", "kb_check", "kb_search"]);
    }

    #[test]
    fn descriptions_are_the_cli_help() {
        let mut cli = Cli::command();
        for t in tools() {
            let sub = cli.find_subcommand_mut(&t.name[PREFIX.len()..]).unwrap();
            let help = sub.render_long_help().to_string();
            assert!(!t.description.is_empty(), "{}", t.name);
            assert!(help.contains(&t.description), "{}:\n{help}", t.name);
            for (arg, property) in t.input_schema["properties"].as_object().unwrap() {
                let description = property["description"].as_str().unwrap();
                assert!(help.contains(description), "{}.{arg}", t.name);
            }
        }
        let check = tool("kb_check").description;
        assert!(check.starts_with("Is this already in the knowledge base?"));
    }

    #[test]
    fn check_requires_a_query_and_defaults_min_score() {
        let check = schema("kb_check");
        assert_eq!(check["type"], "object");
        assert_eq!(check["additionalProperties"], false);
        assert_eq!(check["required"], json!(["query"]));
        assert_eq!(check["properties"]["query"]["type"], "string");
        let min_score = &check["properties"]["min_score"];
        assert_eq!(min_score["type"], "number");
        let default = min_score["default"].as_f64().map(|d| d as f32);
        assert_eq!(default, Some(DEFAULT_MIN_SCORE));
    }

    #[test]
    fn search_limit_is_a_non_negative_integer() {
        let search = schema("kb_search");
        let limit = &search["properties"]["limit"];
        assert_eq!(limit["type"], "integer");
        assert_eq!(limit["minimum"], 0);
        assert_eq!(limit["default"], 5);
        assert_eq!(search["properties"]["min_score"]["default"], json!(0.0));
        assert!(search["properties"]["min_score"].get("minimum").is_none());
    }

    #[test]
    fn sync_dry_run_is_a_boolean() {
        let sync = schema("kb_sync");
        assert_eq!(sync["properties"]["dry_run"]["type"], "boolean");
        assert_eq!(sync["properties"]["dry_run"]["default"], false);
        assert!(sync.get("required").is_none(), "{sync}");
    }

    #[test]
    fn remember_note_defaults_to_the_inbox() {
        let remember = schema("kb_remember");
        assert_eq!(remember["required"], json!(["text"]));
        assert_eq!(remember["properties"]["note"]["default"], INBOX);
        assert!(remember["properties"]["heading"].get("default").is_none());
    }

    #[test]
    fn global_settings_never_appear() {
        let mut cli = Cli::command();
        cli.build();
        let globals: Vec<String> = cli
            .get_arguments()
            .map(|a| a.get_id().to_string())
            .collect();
        assert!(globals.iter().any(|g| g == "vault"), "{globals:?}");
        for t in tools() {
            let properties = t.input_schema["properties"].as_object().unwrap().clone();
            for g in &globals {
                assert!(!properties.contains_key(g), "{} exposes {g}", t.name);
            }
        }
    }

    #[test]
    fn calls_parse_like_the_cli() {
        let sync = |dry_run| Command::Sync { dry_run };
        assert_eq!(call("kb_sync", json!({})).unwrap(), sync(false));
        assert_eq!(
            call("kb_sync", json!({"dry_run": true})).unwrap(),
            sync(true)
        );
        assert_eq!(
            call("kb_sync", json!({"dry_run": false})).unwrap(),
            sync(false)
        );
        assert_eq!(
            call(
                "kb_remember",
                json!({"text": "Tea at 5", "heading": "Habits"})
            )
            .unwrap(),
            Command::Remember {
                text: "Tea at 5".into(),
                note: INBOX.into(),
                heading: Some("Habits".into()),
            }
        );
        assert_eq!(
            call("kb_check", json!({"query": "tea"})).unwrap(),
            Command::Check {
                query: "tea".into(),
                min_score: DEFAULT_MIN_SCORE,
            }
        );
        assert_eq!(
            call(
                "kb_search",
                json!({"query": "tea", "limit": 2, "min_score": 0.25})
            )
            .unwrap(),
            Command::Search {
                query: "tea".into(),
                limit: 2,
                min_score: 0.25,
            }
        );
    }

    #[test]
    fn values_starting_with_a_dash_stay_values() {
        assert_eq!(
            call("kb_check", json!({"query": "--help", "min_score": -0.5})).unwrap(),
            Command::Check {
                query: "--help".into(),
                min_score: -0.5,
            }
        );
        let remember = json!({"text": "-v is verbose", "note": "-x", "heading": "--y"});
        assert_eq!(
            call("kb_remember", remember).unwrap(),
            Command::Remember {
                text: "-v is verbose".into(),
                note: "-x".into(),
                heading: Some("--y".into()),
            }
        );
    }

    #[test]
    fn strings_are_parsed_by_clap() {
        assert_eq!(
            call("kb_check", json!({"query": "tea", "min_score": "0.7"})).unwrap(),
            Command::Check {
                query: "tea".into(),
                min_score: 0.7,
            }
        );
        let e = rejection("kb_search", json!({"query": "tea", "limit": "many"}));
        assert!(e.contains("'many'"), "{e}");
        let e = rejection("kb_search", json!({"query": "tea", "limit": -1}));
        assert!(e.contains("'-1'"), "{e}");
        let e = rejection("kb_check", json!({}));
        assert!(e.contains("required"), "{e}");
    }

    #[test]
    fn null_means_absent() {
        assert_eq!(
            call("kb_remember", json!({"text": "tea", "heading": null})).unwrap(),
            Command::Remember {
                text: "tea".into(),
                note: INBOX.into(),
                heading: None,
            }
        );
    }

    #[test]
    fn unknown_keys_are_rejected_by_name() {
        let e = rejection("kb_check", json!({"query": "tea", "vault": "/notes"}));
        assert!(e.contains("`vault`"), "{e}");
        assert!(e.contains("query") && e.contains("min_score"), "{e}");
        let e = rejection("kb_check", json!({"query": "tea", "help": true}));
        assert!(e.contains("`help`"), "{e}");
    }

    #[test]
    fn malformed_arguments_are_rejected() {
        assert!(rejection("kb_check", json!(["tea"])).contains("object"));
        assert!(rejection("kb_sync", json!({"dry_run": "yes"})).contains("`dry_run`"));
        assert!(rejection("kb_check", json!({"query": ["tea"]})).contains("`query`"));
    }

    #[test]
    fn mcp_and_unknown_tools_are_not_tools() {
        for name in ["kb_mcp", "kb_help", "kb_nope", "check", "mcp", ""] {
            let e = rejection(name, json!({}));
            assert!(e.contains("unknown tool"), "{name}: {e}");
        }
    }

    #[test]
    fn markdown_has_one_section_per_tool_and_the_settings() {
        let md = markdown();
        for t in tools() {
            let sub = &t.name[PREFIX.len()..];
            let heading = format!("### `{}` · `chaos-kb {sub}`", t.name);
            assert!(md.contains(&heading), "{heading}\n{md}");
            assert!(md.contains(&t.description), "{md}");
        }
        assert!(md.contains("| `query` (required) |"), "{md}");
        assert!(md.contains("| `--vault` | `CHAOS_KB_VAULT` |"), "{md}");
        assert!(md.contains("`http://localhost:11434/v1`"), "{md}");
        assert!(
            !md.contains("<vault>"),
            "angle brackets would render as HTML"
        );
    }

    #[test]
    fn the_api_key_variable_is_documented_but_is_no_flag() {
        let help = Cli::command().render_long_help().to_string();
        let md = markdown();
        for text in [&help, &md] {
            assert!(text.contains("`CHAOS_KB_EMBED_API_KEY`"), "{text}");
        }
        assert!(!help.contains("--embed-api-key") && !help.contains("--api-key"));
    }

    #[test]
    fn readme_embeds_the_generated_reference() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/README.md");
        let readme = std::fs::read_to_string(path).unwrap();
        let block = readme
            .split_once("<!-- tools:start -->")
            .and_then(|(_, rest)| rest.split_once("<!-- tools:end -->"))
            .map(|(block, _)| block.trim())
            .expect("README.md has the tools markers");
        let expected = markdown();
        assert!(
            block == expected.trim(),
            "README.md is stale; put this between the markers:\n\n{expected}"
        );
    }
}
