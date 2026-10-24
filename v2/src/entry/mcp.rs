//! MCP server on stdio: newline-delimited JSON-RPC 2.0 with `initialize`,
//! `ping`, `tools/list` and `tools/call`. Nothing is opened or contacted
//! before a tool is called, so the handshake is always instant.

use super::cli::{self, Settings, Tool};
use super::tools;
use anyhow::Result;
use serde_json::{Value, json};
use std::io::{BufRead, Write};

/// Newest MCP protocol revision this server speaks: the last one with the
/// `initialize` handshake (2026-07-28 and later are stateless and use
/// `server/discover` instead).
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// Revisions accepted in `initialize`, newest first. 2025-03-26 is left out:
/// it requires accepting JSON-RPC batches, which this server rejects.
pub const SUPPORTED: &[&str] = &[PROTOCOL_VERSION, "2025-06-18", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// A JSON-RPC error: code and message.
type Failure = (i64, String);

/// Answer one JSON-RPC message; `None` for notifications and responses. `run`
/// executes a parsed tool (the real knowledge base in production).
pub fn handle(message: &Value, run: &dyn Fn(&Tool) -> Result<Value>) -> Option<Value> {
    let Some(request) = message.as_object() else {
        let failure = (INVALID_REQUEST, "expected one JSON-RPC object".into());
        return Some(error(&Value::Null, failure));
    };
    let id = request.get("id")?;
    // A response answers a request of ours; replying to it would be an error.
    let is_response = !request.contains_key("method")
        && (request.contains_key("result") || request.contains_key("error"));
    if is_response {
        return None;
    }
    let params = request.get("params").unwrap_or(&Value::Null);
    let outcome = match request.get("method").and_then(Value::as_str) {
        Some("initialize") => Ok(initialize(params)),
        Some("ping") => Ok(json!({})),
        Some("tools/list") => Ok(json!({"tools": tools::tools()})),
        Some("tools/call") => call(params, run),
        Some(method) => Err((METHOD_NOT_FOUND, format!("method not found: {method}"))),
        None => Err((INVALID_REQUEST, "a request needs a method".into())),
    };
    Some(match outcome {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(failure) => error(id, failure),
    })
}

/// Serve until stdin closes.
pub fn serve(settings: &Settings) -> Result<()> {
    let run = |tool: &Tool| cli::execute(settings, tool);
    let mut stdout = std::io::stdout();
    for line in std::io::stdin().lock().split(b'\n') {
        if let Some(reply) = answer(&line?, &run) {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// The reply line to one stdin line: `None` for blank lines, notifications and
/// responses, a parse error for anything that is not JSON.
fn answer(line: &[u8], run: &dyn Fn(&Tool) -> Result<Value>) -> Option<String> {
    if line.trim_ascii().is_empty() {
        return None;
    }
    let reply = match serde_json::from_slice::<Value>(line) {
        Ok(message) => handle(&message, run)?,
        Err(e) => error(&Value::Null, (PARSE_ERROR, format!("parse error: {e}"))),
    };
    Some(reply.to_string())
}

fn error(id: &Value, (code, message): Failure) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Echo the client's revision when we speak it, else offer our newest.
fn initialize(params: &Value) -> Value {
    let requested = params["protocolVersion"].as_str();
    let version = requested
        .filter(|v| SUPPORTED.contains(v))
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": env!("CARGO_PKG_NAME"), "version": env!("CARGO_PKG_VERSION")},
        "instructions": tools::instructions(),
    })
}

/// An unknown tool is a protocol error; invalid arguments and failures are
/// tool results with `isError`, so the model can read them and correct itself.
fn call(params: &Value, run: &dyn Fn(&Tool) -> Result<Value>) -> Result<Value, Failure> {
    let name = params["name"]
        .as_str()
        .ok_or((INVALID_PARAMS, "tools/call needs a tool `name`".into()))?;
    let none = json!({});
    let arguments = match &params["arguments"] {
        Value::Null => &none,
        given => given,
    };
    let parsed = tools::command(name, arguments)
        .ok_or_else(|| (INVALID_PARAMS, format!("unknown tool: {name}")))?;
    let (text, is_error) = match parsed.and_then(|tool| run(&tool)) {
        Ok(report) => (report.to_string(), false),
        Err(e) => (format!("{e:#}"), true),
    };
    Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::cli::Cli;
    use anyhow::anyhow;
    use clap::CommandFactory;
    use serde_json::json;

    type Run = dyn Fn(&Tool) -> Result<Value>;

    /// A stand-in knowledge base that reports the command it was given.
    fn echo(tool: &Tool) -> Result<Value> {
        Ok(json!({ "ran": format!("{tool:?}") }))
    }

    fn broken(_: &Tool) -> Result<Value> {
        Err(anyhow!("index is locked").context("search failed"))
    }

    fn untouched(tool: &Tool) -> Result<Value> {
        panic!("must not run {tool:?}")
    }

    fn request(method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params})
    }

    fn reply(message: Value, run: &Run) -> Value {
        let reply = handle(&message, run).expect("a reply");
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], message["id"]);
        reply
    }

    fn result(message: Value, run: &Run) -> Value {
        let reply = reply(message, run);
        assert!(reply.get("error").is_none(), "{reply}");
        reply["result"].clone()
    }

    fn error_code(message: Value) -> i64 {
        reply(message, &untouched)["error"]["code"]
            .as_i64()
            .unwrap()
    }

    fn negotiated(requested: Option<&str>) -> Value {
        let params = requested.map_or(json!({}), |v| json!({"protocolVersion": v}));
        result(request("initialize", params), &untouched)["protocolVersion"].clone()
    }

    fn call(name: &str, arguments: Value, run: &Run) -> Value {
        let params = json!({"name": name, "arguments": arguments});
        result(request("tools/call", params), run)
    }

    #[test]
    fn initialize_describes_the_server() {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        });
        let init = result(request("initialize", params), &untouched);
        assert_eq!(init["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(
            init["capabilities"],
            json!({"tools": {"listChanged": false}})
        );
        assert_eq!(
            init["serverInfo"],
            json!({"name": "chaos-kb", "version": env!("CARGO_PKG_VERSION")})
        );
        let about = Cli::command().get_long_about().unwrap().to_string();
        assert_eq!(init["instructions"], about);
    }

    #[test]
    fn negotiates_the_protocol_version() {
        assert_eq!(SUPPORTED[0], PROTOCOL_VERSION);
        for v in SUPPORTED {
            assert_eq!(negotiated(Some(v)), *v);
        }
        // 2025-03-26 requires accepting JSON-RPC batches, which this server rejects.
        for v in [
            Some("2025-03-26"),
            Some("2026-07-28"),
            Some("1999-01-01"),
            None,
        ] {
            assert_eq!(negotiated(v), PROTOCOL_VERSION, "{v:?}");
        }
    }

    #[test]
    fn responses_get_no_reply() {
        let done = json!({"jsonrpc": "2.0", "id": 1, "result": {}});
        let failed = json!({"jsonrpc": "2.0", "id": 1,
            "error": {"code": -32601, "message": "method not found"}});
        for response in [done, failed] {
            assert_eq!(handle(&response, &untouched), None, "{response}");
        }
        let neither = json!({"jsonrpc": "2.0", "id": 1});
        assert_eq!(error_code(neither), -32600);
    }

    #[test]
    fn notifications_get_no_reply() {
        let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert_eq!(handle(&initialized, &untouched), None);
        let call = json!({"jsonrpc": "2.0", "method": "tools/call",
            "params": {"name": "kb_sync", "arguments": {}}});
        assert_eq!(handle(&call, &untouched), None);
    }

    #[test]
    fn ping_answers_empty() {
        let ping = json!({"jsonrpc": "2.0", "id": "p", "method": "ping"});
        assert_eq!(result(ping, &untouched), json!({}));
    }

    #[test]
    fn unknown_methods_are_not_found() {
        assert_eq!(error_code(request("resources/list", json!({}))), -32601);
        assert_eq!(error_code(request("server/discover", json!({}))), -32601);
    }

    #[test]
    fn unknown_tools_are_invalid_params() {
        let unknown = request("tools/call", json!({"name": "kb_nope", "arguments": {}}));
        assert_eq!(error_code(unknown), -32602);
        assert_eq!(
            error_code(request("tools/call", json!({"name": "kb_mcp"}))),
            -32602
        );
        assert_eq!(error_code(request("tools/call", json!({}))), -32602);
    }

    #[test]
    fn non_objects_are_invalid_requests() {
        let batch = json!([{"jsonrpc": "2.0", "id": 1, "method": "ping"}]);
        let reply = handle(&batch, &untouched).expect("a reply");
        assert_eq!(reply["error"]["code"], -32600);
        assert_eq!(reply["id"], Value::Null);
    }

    #[test]
    fn calls_run_the_parsed_command() {
        let done = call("kb_search", json!({"query": "tea", "limit": 2}), &echo);
        assert_eq!(done["isError"], false);
        let search = Tool::Search {
            query: "tea".into(),
            limit: 2,
            min_score: 0.0,
        };
        let text = echo(&search).unwrap().to_string();
        assert_eq!(done["content"], json!([{"type": "text", "text": text}]));
    }

    #[test]
    fn missing_arguments_mean_none() {
        let params = json!({"name": "kb_sync"});
        let done = result(request("tools/call", params), &echo);
        let text = echo(&Tool::Sync { dry_run: false }).unwrap().to_string();
        assert_eq!(done["content"][0]["text"], text);
    }

    #[test]
    fn bad_arguments_are_a_tool_error_the_model_can_read() {
        let done = call("kb_check", json!({}), &untouched);
        assert_eq!(done["isError"], true);
        let text = done["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("required"), "{text}");
    }

    #[test]
    fn failures_are_a_tool_error_with_their_causes() {
        let done = call("kb_check", json!({"query": "tea"}), &broken);
        assert_eq!(done["isError"], true);
        let text = "search failed: index is locked";
        assert_eq!(done["content"], json!([{"type": "text", "text": text}]));
    }

    #[test]
    fn lines_are_answered_one_to_one() {
        assert_eq!(answer(b"", &untouched), None);
        assert_eq!(answer(b"  \r", &untouched), None);
        let initialized = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert_eq!(answer(initialized, &untouched), None);

        let bad: Value = serde_json::from_str(&answer(b"{nope", &untouched).unwrap()).unwrap();
        assert_eq!(bad["error"]["code"], -32700);
        assert_eq!(bad["id"], Value::Null);

        let line = br#"{"jsonrpc":"2.0","id":"a","method":"tools/call","params":{"name":"kb_check","arguments":{"query":"line\nbreak"}}}"#;
        let pong = answer(line, &echo).unwrap();
        assert!(!pong.contains('\n'), "{pong}");
        let pong: Value = serde_json::from_str(&pong).unwrap();
        assert_eq!(pong["id"], "a");
        assert_eq!(pong["result"]["isError"], false);
    }
}
