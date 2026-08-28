//! MCP server: stdio, newline-delimited JSON-RPC 2.0. Exposes the six tools
//! from `application::tools` over `tools/list` / `tools/call`.

use super::cli::{agent_guide, Services};
use crate::application::tools::{mcp_name, tools};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// What `tools/call` dispatches to — the wired [`Services`] in production, a
/// scripted fake in tests.
#[async_trait]
pub trait ToolRunner: Send + Sync {
    /// `name` is the MCP name (`chaos_index`); arguments are the raw JSON object.
    async fn call(&self, name: &str, args: Value) -> Result<Value>;
}

#[async_trait]
impl ToolRunner for Services {
    async fn call(&self, name: &str, args: Value) -> Result<Value> {
        let bare = name.strip_prefix("chaos_").unwrap_or(name);
        self.call_tool(bare, &args).await
    }
}

pub fn json_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn json_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn tool_text(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

pub fn read_message(stdin: &mut std::io::Stdin) -> Result<Option<Value>> {
    // Skip blank keep-alive lines iteratively (a recursive skip could overflow the stack).
    loop {
        let mut line = String::new();
        let bytes_read = stdin.lock().read_line(&mut line)?;
        if bytes_read == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            continue;
        }
        return Ok(Some(serde_json::from_str(trimmed)?));
    }
}

pub fn write_message(stdout: &mut std::io::Stdout, message: &Value) -> Result<()> {
    let body = serde_json::to_string(message)?;
    stdout.write_all(body.as_bytes())?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

/// The `tools/list` roster: every tool as `{name, description, inputSchema}`.
pub fn tool_list() -> Value {
    let list: Vec<Value> = tools()
        .into_iter()
        .map(|t| json!({"name": mcp_name(t.name), "description": t.description, "inputSchema": t.input_schema}))
        .collect();
    json!({"tools": list})
}

/// Handle one request. `None` for notifications (no `id`) — nothing is sent back.
pub async fn handle(runner: &dyn ToolRunner, request: &Value) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        // Notification (notifications/initialized, cancelled, …): ignore.
        return None;
    };
    Some(match method {
        "initialize" => json_result(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "chaos-substrate", "version": env!("CARGO_PKG_VERSION")},
                "instructions": agent_guide(),
            }),
        ),
        "ping" => json_result(id, json!({})),
        "tools/list" => json_result(id, tool_list()),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !tools().iter().any(|t| mcp_name(t.name) == name) {
                return Some(json_error(id, -32602, &format!("unknown tool `{name}`")));
            }
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            tracing::info!(tool = %name, "tools/call");
            match runner.call(&name, args).await {
                Ok(v) => json_result(
                    id,
                    tool_text(serde_json::to_string(&v).unwrap_or_default(), false),
                ),
                Err(e) => json_result(id, tool_text(format!("{e:#}"), true)),
            }
        }
        _ => json_error(id, -32601, &format!("method not found: {method}")),
    })
}

/// Serve until stdin closes.
pub async fn serve(services: Services) -> Result<()> {
    let mut stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    tracing::info!(
        "chaos-substrate MCP server on stdio ({} tools)",
        tools().len()
    );
    loop {
        let message = match read_message(&mut stdin) {
            Ok(Some(m)) => m,
            Ok(None) => return Ok(()),
            Err(e) => {
                write_message(
                    &mut stdout,
                    &json_error(Value::Null, -32700, &format!("parse error: {e}")),
                )?;
                continue;
            }
        };
        if let Some(response) = handle(&services, &message).await {
            write_message(&mut stdout, &response)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::tools::TOOL_NAMES;

    struct Echo;
    #[async_trait]
    impl ToolRunner for Echo {
        async fn call(&self, name: &str, args: Value) -> Result<Value> {
            if name == "chaos_add" {
                anyhow::bail!("scripted failure");
            }
            Ok(json!({"tool": name, "args": args}))
        }
    }

    #[tokio::test]
    async fn tools_list_returns_exactly_six_chaos_names() {
        let resp = handle(
            &Echo,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await
        .unwrap();
        let list = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(list.len(), 6);
        let names: Vec<&str> = list.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.iter().all(|n| n.starts_with("chaos_")), "{names:?}");
        assert_eq!(
            names,
            TOOL_NAMES
                .iter()
                .map(|n| mcp_name(n))
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        );
        for t in list {
            assert!(t["inputSchema"]["type"] == "object");
            assert!(!t["description"].as_str().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn initialize_and_notifications() {
        let resp = handle(
            &Echo,
            &json!({"jsonrpc": "2.0", "id": 7, "method": "initialize", "params": {}}),
        )
        .await
        .unwrap();
        assert_eq!(resp["id"], 7);
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(resp["result"]["serverInfo"]["name"], "chaos-substrate");
        assert!(resp["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("chaos_feature"));
        assert!(handle(
            &Echo,
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )
        .await
        .is_none());
    }

    #[tokio::test]
    async fn tools_call_dispatches_and_reports_errors() {
        let ok = handle(&Echo, &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "chaos_feature", "arguments": {"question": "q"}}})).await.unwrap();
        assert_eq!(ok["result"]["isError"], false);
        let text: Value =
            serde_json::from_str(ok["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text["args"]["question"], "q");
        let err = handle(&Echo, &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "chaos_add", "arguments": {}}})).await.unwrap();
        assert_eq!(err["result"]["isError"], true);
        assert!(err["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("scripted failure"));
        let unknown = handle(&Echo, &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "chaos_nope"}})).await.unwrap();
        assert_eq!(unknown["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn unknown_method_is_32601() {
        let resp = handle(
            &Echo,
            &json!({"jsonrpc": "2.0", "id": 5, "method": "resources/list"}),
        )
        .await
        .unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn json_error_shape() {
        let e = json_error(json!(9), -32601, "nope");
        assert_eq!(
            e,
            json!({"jsonrpc": "2.0", "id": 9, "error": {"code": -32601, "message": "nope"}})
        );
        assert!(e.get("result").is_none());
    }
}
