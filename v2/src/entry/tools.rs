//! The MCP tool roster, derived from the clap definition in `entry::cli`:
//! one tool per [`Tool`] subcommand, named `kb_<subcommand>`, described by its
//! doc comment, with a JSON Schema built from its arguments. Calls are turned
//! back into argv and parsed by the same clap definition, so defaults and
//! validation live in exactly one place.

use super::cli::{Cli, Tool, api_key_note};
use anyhow::{Context, Result, anyhow, bail};
use clap::builder::StyledStr;
use clap::{Arg, ArgAction, CommandFactory, Parser};
use serde_json::{Map, Value, json};
use std::any::TypeId;

/// Prefix of every MCP tool name.
pub const PREFIX: &str = "kb_";

/// What one MCP call may set: a tool, without the CLI's settings. Left
/// unbuilt, its subcommands hold only their own arguments (no help flag).
#[derive(Parser)]
struct Call {
    #[command(subcommand)]
    tool: Tool,
}

/// Every tool as a `tools/list` entry, in subcommand order.
pub fn tools() -> Vec<Value> {
    Call::command().get_subcommands().map(tool).collect()
}

/// Parse an MCP call (`kb_check`, `{"query": "…"}`) into a [`Tool`] through
/// clap, exactly as the CLI would. `None` when no tool has that name; invalid
/// arguments fail with clap's message.
pub fn command(name: &str, arguments: &Value) -> Option<Result<Tool>> {
    let call = Call::command();
    let sub = call.find_subcommand(name.strip_prefix(PREFIX)?)?;
    let parsed = argv(call.get_name(), sub, arguments).and_then(|argv| {
        Call::try_parse_from(argv)
            .map(|call| call.tool)
            .map_err(|e| anyhow!("{}", e.to_string().trim_end()))
    });
    Some(parsed)
}

/// The agent workflow guide: the CLI's about text.
pub fn instructions() -> String {
    let cli = Cli::command();
    help(cli.get_long_about(), cli.get_about())
}

/// The Markdown tool reference embedded in README.md between the
/// `<!-- tools:start -->` and `<!-- tools:end -->` markers.
pub fn markdown() -> String {
    let call = Call::command();
    let mut sections: Vec<String> = call
        .get_subcommands()
        .map(|sub| section(call.get_name(), sub))
        .collect();
    sections.push(settings());
    sections.join("\n")
}

/// The command line the CLI would get for these arguments: options as
/// `--long=value`, flags as `--long` when true, a list as one value per
/// item, positionals after `--` in definition order and a list's items in
/// their own order (so values starting with `-` stay values).
fn argv(bin: &str, sub: &clap::Command, arguments: &Value) -> Result<Vec<String>> {
    let name = sub.get_name();
    let given = arguments
        .as_object()
        .with_context(|| format!("{PREFIX}{name} arguments must be a JSON object"))?;
    let mut argv = vec![bin.to_string(), name.to_string()];
    let mut positionals = Vec::new();
    for (key, value) in given {
        let (position, arg) = sub
            .get_arguments()
            .enumerate()
            .find(|(_, a)| a.get_id() == key.as_str())
            .ok_or_else(|| unknown_key(sub, key))?;
        match (Kind::of(arg), value) {
            (_, Value::Null) => {}
            (Kind::Boolean, Value::Bool(set)) => {
                if *set {
                    argv.push(format!("--{}", long(arg)?));
                }
            }
            (Kind::Boolean, _) => bail!("`{key}` must be true or false"),
            (Kind::List, Value::Array(items)) => {
                for item in items {
                    let text = item
                        .as_str()
                        .with_context(|| format!("`{key}` must be a list of strings"))?;
                    push(arg, position, text.to_string(), &mut argv, &mut positionals)?;
                }
            }
            (_, Value::String(_) | Value::Number(_)) => {
                let text = value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_string);
                push(arg, position, text, &mut argv, &mut positionals)?;
            }
            (kind, _) => bail!("`{key}` must be a {}", kind.name()),
        }
    }
    if !positionals.is_empty() {
        // Stable: a list's items keep their order.
        positionals.sort_by_key(|(position, _)| *position);
        argv.push("--".into());
        argv.extend(positionals.into_iter().map(|(_, text)| text));
    }
    Ok(argv)
}

/// One value of `arg`: an option on `argv`, or a positional for later.
fn push(
    arg: &Arg,
    position: usize,
    text: String,
    argv: &mut Vec<String>,
    positionals: &mut Vec<(usize, String)>,
) -> Result<()> {
    if arg.is_positional() {
        positionals.push((position, text));
    } else {
        argv.push(format!("--{}={text}", long(arg)?));
    }
    Ok(())
}

/// The JSON type of a tool argument, read from its clap action and value
/// parser. Only the types the tools use; anything else is passed as text.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    /// A `bool` flag.
    Boolean,
    /// A `usize`: a non-negative integer.
    Count,
    /// An `f32`.
    Number,
    String,
    /// A `Vec<String>`: an array of strings.
    List,
}

impl Kind {
    fn of(arg: &Arg) -> Kind {
        // clap reports the parsed value's type; `AnyValueId` compares with `TypeId`.
        let parsed = arg.get_value_parser().type_id();
        if matches!(arg.get_action(), ArgAction::SetTrue) {
            Kind::Boolean
        } else if matches!(arg.get_action(), ArgAction::Append) {
            Kind::List
        } else if parsed == TypeId::of::<usize>() {
            Kind::Count
        } else if parsed == TypeId::of::<f32>() {
            Kind::Number
        } else {
            Kind::String
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Boolean => "boolean",
            Kind::Count => "integer",
            Kind::Number => "number",
            Kind::String => "string",
            Kind::List => "array",
        }
    }

    /// The argument's default as JSON of this type. A flag's implied `false`
    /// is only filled in when clap builds the command, so it is stated here.
    fn default(self, arg: &Arg) -> Option<Value> {
        if self == Kind::Boolean {
            return Some(Value::Bool(false));
        }
        let raw = arg.get_default_values().first()?.to_string_lossy();
        let typed = match self {
            Kind::Count => raw.parse::<u64>().ok().map(Value::from),
            Kind::Number => raw.parse::<f64>().ok().map(Value::from),
            Kind::Boolean | Kind::String | Kind::List => None,
        };
        Some(typed.unwrap_or_else(|| Value::from(raw.as_ref())))
    }
}

/// One tool argument, as both the MCP schema and the README table show it.
struct Param {
    id: String,
    kind: Kind,
    required: bool,
    default: Option<Value>,
    description: String,
}

/// A tool's arguments, in definition order.
fn params(sub: &clap::Command) -> Vec<Param> {
    sub.get_arguments()
        .map(|arg| {
            let kind = Kind::of(arg);
            Param {
                id: arg.get_id().to_string(),
                kind,
                required: arg.is_required_set(),
                default: kind.default(arg),
                description: help(arg.get_long_help(), arg.get_help()),
            }
        })
        .collect()
}

fn unknown_key(sub: &clap::Command, key: &str) -> anyhow::Error {
    let known: Vec<String> = sub
        .get_arguments()
        .map(|a| format!("`{}`", a.get_id()))
        .collect();
    anyhow!(
        "{PREFIX}{} has no argument `{key}`; it takes {}",
        sub.get_name(),
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

/// One `tools/list` entry: name, description, and the JSON Schema of the
/// arguments.
fn tool(sub: &clap::Command) -> Value {
    let params = params(sub);
    let mut properties = Map::new();
    for p in &params {
        let mut property = json!({"type": p.kind.name(), "description": p.description});
        if p.kind == Kind::Count {
            property["minimum"] = json!(0);
        }
        if p.kind == Kind::List {
            property["items"] = json!({"type": "string"});
            if p.required {
                property["minItems"] = json!(1);
            }
        }
        if let Some(default) = &p.default {
            property["default"] = default.clone();
        }
        properties.insert(p.id.clone(), property);
    }
    let required: Vec<&str> = params
        .iter()
        .filter(|p| p.required)
        .map(|p| p.id.as_str())
        .collect();
    let mut schema =
        json!({"type": "object", "properties": properties, "additionalProperties": false});
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    json!({
        "name": format!("{PREFIX}{}", sub.get_name()),
        "description": help(sub.get_long_about(), sub.get_about()),
        "inputSchema": schema,
    })
}

/// One tool: heading, description, and an args table (required args first,
/// then by name).
fn section(bin: &str, sub: &clap::Command) -> String {
    let name = sub.get_name();
    let about = help(sub.get_long_about(), sub.get_about());
    let mut out = format!(
        "### `{PREFIX}{name}` · `{bin} {name}`\n\n{}\n",
        prose(&about)
    );
    let mut params = params(sub);
    if params.is_empty() {
        return out;
    }
    params.sort_by(|a, b| (!a.required, &a.id).cmp(&(!b.required, &b.id)));
    out.push_str("\n| arg | type | default | description |\n|---|---|---|---|\n");
    for p in params {
        let mark = if p.required { " (required)" } else { "" };
        let kind = match p.kind {
            Kind::Count => "integer ≥ 0",
            Kind::List => "list of strings",
            kind => kind.name(),
        };
        let default = match &p.default {
            Some(Value::String(s)) => format!("`{s}`"),
            Some(v) => format!("`{v}`"),
            None => String::new(),
        };
        out.push_str(&format!(
            "| `{}`{mark} | {kind} | {default} | {} |\n",
            p.id,
            cell(&p.description)
        ));
    }
    out
}

/// The global settings every command takes, with their environment variables.
fn settings() -> String {
    let mut out = String::from(
        "### Settings\n\n| flag | env var | default | description |\n|---|---|---|---|\n",
    );
    // Unbuilt, the CLI's own arguments are exactly the settings.
    for arg in Cli::command().get_arguments() {
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
    use crate::application::topics::DEFAULT_TOPICS;
    use crate::domain::presence::DEFAULT_MIN_SCORE;
    use crate::entry::cli::Cli;
    use clap::CommandFactory;
    use serde_json::json;

    fn tool(name: &str) -> Value {
        tools().into_iter().find(|t| t["name"] == name).expect(name)
    }

    fn call(name: &str, arguments: Value) -> Result<Tool> {
        command(name, &arguments).expect("a tool")
    }

    fn rejection(name: &str, arguments: Value) -> String {
        format!("{:#}", call(name, arguments).unwrap_err())
    }

    #[test]
    fn one_tool_per_subcommand_except_mcp() {
        let names: Vec<Value> = tools().into_iter().map(|t| t["name"].clone()).collect();
        assert_eq!(
            names,
            [
                "kb_sync",
                "kb_remember",
                "kb_check",
                "kb_search",
                "kb_topics",
                "kb_read"
            ]
        );
    }

    #[test]
    fn descriptions_are_the_cli_help() {
        let mut cli = Cli::command();
        for t in tools() {
            let name = t["name"].as_str().unwrap();
            let sub = cli.find_subcommand_mut(&name[PREFIX.len()..]).unwrap();
            let help = sub.render_long_help().to_string();
            let description = t["description"].as_str().unwrap();
            assert!(!description.is_empty(), "{name}");
            assert!(help.contains(description), "{name}:\n{help}");
            for (arg, property) in t["inputSchema"]["properties"].as_object().unwrap() {
                let description = property["description"].as_str().unwrap();
                assert!(help.contains(description), "{name}.{arg}");
            }
        }
        let check = tool("kb_check");
        let check = check["description"].as_str().unwrap();
        assert!(check.starts_with("Is this already in the knowledge base?"));
    }

    #[test]
    fn schemas_have_typed_defaults() {
        let schema = |name| tool(name)["inputSchema"].clone();
        let sync = schema("kb_sync");
        assert_eq!(sync["additionalProperties"], false);
        assert!(sync.get("required").is_none(), "{sync}");
        let dry_run = &sync["properties"]["dry_run"];
        assert_eq!(dry_run["type"], "boolean");
        assert_eq!(dry_run["default"], false);

        let remember = schema("kb_remember");
        assert_eq!(remember["required"], json!(["text"]));
        assert_eq!(remember["properties"]["note"]["default"], INBOX);
        assert!(remember["properties"]["heading"].get("default").is_none());

        let check = schema("kb_check");
        assert_eq!(check["required"], json!(["query"]));
        assert_eq!(check["properties"]["query"]["type"], "string");
        let min_score = &check["properties"]["min_score"];
        assert_eq!(min_score["type"], "number");
        let default = min_score["default"].as_f64().map(|d| d as f32);
        assert_eq!(default, Some(DEFAULT_MIN_SCORE));

        let search = schema("kb_search");
        let limit = &search["properties"]["limit"];
        assert_eq!(limit["type"], "integer");
        assert_eq!(limit["minimum"], 0);
        assert_eq!(limit["default"], 5);
        assert_eq!(search["properties"]["min_score"]["default"], json!(0.0));
        assert!(search["properties"]["min_score"].get("minimum").is_none());

        let topics = schema("kb_topics");
        assert_eq!(topics["required"], json!(["task"]));
        assert_eq!(topics["properties"]["limit"]["default"], DEFAULT_TOPICS);

        let read = schema("kb_read");
        assert_eq!(read["required"], json!(["ids"]));
        let ids = &read["properties"]["ids"];
        assert_eq!(ids["type"], "array");
        assert_eq!(ids["items"], json!({"type": "string"}));
        assert_eq!(ids["minItems"], 1);
    }

    #[test]
    fn a_list_keeps_the_order_given() {
        let ids = ["Food.md:3-6", "Car.md:1-2", "-dash.md:1-1"];
        assert_eq!(
            call("kb_read", json!({"ids": ids})).unwrap(),
            Tool::Read {
                ids: ids.iter().map(|id| id.to_string()).collect()
            }
        );
        assert_eq!(
            call("kb_topics", json!({"task": "cook dinner", "limit": 3})).unwrap(),
            Tool::Topics {
                task: "cook dinner".into(),
                limit: 3,
            }
        );
    }

    #[test]
    fn a_list_holds_strings_only_and_at_least_one() {
        let e = rejection("kb_read", json!({"ids": ["Food.md:3-6", 7]}));
        assert!(e.contains("`ids`") && e.contains("strings"), "{e}");
        let e = rejection("kb_read", json!({"ids": []}));
        assert!(e.contains("required"), "{e}");
        let e = rejection("kb_check", json!({"query": ["tea"]}));
        assert!(e.contains("`query` must be a string"), "{e}");
    }

    #[test]
    fn calls_parse_like_the_cli() {
        let sync = |dry_run| Tool::Sync { dry_run };
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
            Tool::Remember {
                text: "Tea at 5".into(),
                note: INBOX.into(),
                heading: Some("Habits".into()),
            }
        );
        assert_eq!(
            call("kb_check", json!({"query": "tea"})).unwrap(),
            Tool::Check {
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
            Tool::Search {
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
            Tool::Check {
                query: "--help".into(),
                min_score: -0.5,
            }
        );
        let remember = json!({"text": "-v is verbose", "note": "-x", "heading": "--y"});
        assert_eq!(
            call("kb_remember", remember).unwrap(),
            Tool::Remember {
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
            Tool::Check {
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
            Tool::Remember {
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
            assert!(command(name, &json!({})).is_none(), "{name}");
        }
    }

    #[test]
    fn prose_escapes_angle_brackets_only_outside_code() {
        assert_eq!(prose("a <b> `<c>` <d>"), "a &lt;b> `<c>` &lt;d>");
    }

    #[test]
    fn markdown_has_one_section_per_tool_and_the_settings() {
        let md = markdown();
        for t in tools() {
            let name = t["name"].as_str().unwrap();
            let heading = format!("### `{name}` · `chaos-kb {}`", &name[PREFIX.len()..]);
            assert!(md.contains(&heading), "{heading}\n{md}");
            assert!(md.contains(t["description"].as_str().unwrap()), "{md}");
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
