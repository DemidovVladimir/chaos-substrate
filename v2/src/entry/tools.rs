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

/// The Markdown tool reference embedded in README.md between the
/// `<!-- tools:start -->` and `<!-- tools:end -->` markers.
pub fn markdown() -> String {
    todo!()
}
