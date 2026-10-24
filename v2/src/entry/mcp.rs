//! MCP server on stdio: newline-delimited JSON-RPC 2.0 with `initialize`,
//! `ping`, `tools/list` and `tools/call`. Nothing is opened or contacted
//! before a tool is called, so the handshake is always instant.

use super::cli::{Command, Settings};
use anyhow::Result;
use serde_json::Value;

/// Newest MCP protocol revision this server speaks.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Answer one JSON-RPC message; `None` for notifications. `run` executes a
/// parsed tool command (the real knowledge base in production).
pub fn handle(message: &Value, run: &dyn Fn(&Command) -> Result<Value>) -> Option<Value> {
    let _ = (message, run);
    todo!()
}

/// Serve until stdin closes.
pub fn serve(settings: &Settings) -> Result<()> {
    let _ = settings;
    todo!()
}
