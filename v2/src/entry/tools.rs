//! The MCP tool roster, derived from the clap definition in `entry::cli`:
//! one tool per subcommand (except `mcp`), named `kb_<subcommand>`, described
//! by the subcommand's doc comment, with a JSON Schema built from its
//! arguments. Calls are turned back into argv and parsed by the same clap
//! definition, so defaults and validation live in exactly one place.

use super::cli::Command;
use anyhow::Result;
use serde_json::Value;

/// Prefix of every MCP tool name.
pub const PREFIX: &str = "kb_";

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
    todo!()
}

/// Parse an MCP call (`kb_check`, `{"query": "…"}`) into a [`Command`] through
/// clap, exactly as the CLI would. Unknown tools and invalid arguments fail
/// with clap's message.
pub fn command(name: &str, arguments: &Value) -> Result<Command> {
    let _ = (name, arguments);
    todo!()
}

/// The agent workflow guide: the CLI's about text.
pub fn instructions() -> String {
    todo!()
}

/// The Markdown tool reference embedded in README.md between the
/// `<!-- tools:start -->` and `<!-- tools:end -->` markers.
pub fn markdown() -> String {
    todo!()
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
