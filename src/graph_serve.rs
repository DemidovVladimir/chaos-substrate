//! Localhost HTTP server behind `chaos graph --serve`: serves the exported
//! interactive graph page plus a semantic-search API that runs the EXACT SAME
//! retrieval the FEATURE-EXTRACTION tools use — `query::query_feature_context_repo`
//! (the path behind `chaos_feature_context` / `chaos_write_feature_website` /
//! `chaos_impact`): real embedder, the multi-query feature-context expansion,
//! hybrid semantic/keyword/literal search, and the shared subject-recall floor.
//! This page is therefore a DEBUG MIRROR: the nodes it highlights are exactly
//! the evidence feature extraction would gather for the same text, so it is how
//! you verify feature (and, via the graph's stack nodes, stack) extraction has
//! the proper outcome. If the embedder or database is unavailable the API
//! returns a loud error — it never falls back to substring matching or
//! fabricated scores.

use crate::{embedding::Embedder, query::query_feature_context_repo, storage::Storage};
use anyhow::{Context, Result};
use serde_json::json;
use std::{collections::HashMap, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use uuid::Uuid;

/// Shared state for every request: one storage pool, one embedder, one
/// pre-rendered HTML page for one repository.
pub struct GraphServer {
    pub storage: Storage,
    pub embedder: Arc<dyn Embedder>,
    pub repo_id: Uuid,
    pub html: String,
}

/// Hits returned per semantic query unless the request overrides `limit`.
const DEFAULT_SEARCH_LIMIT: i64 = 12;
/// Upper bound on a requested `limit` — this is a human validation UI, not a
/// bulk export path.
const MAX_SEARCH_LIMIT: i64 = 50;
/// Request heads larger than this are cut off (the API is GET-only; no body).
const MAX_REQUEST_HEAD: usize = 16 * 1024;

/// Bind 127.0.0.1:`port` and serve until the process is interrupted.
pub async fn serve(server: GraphServer, port: u16) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("cannot bind 127.0.0.1:{port} (is another serve running?)"))?;
    let addr = listener.local_addr()?;
    eprintln!("chaos graph: serving http://{addr}/ — semantic search live, Ctrl-C to stop");
    let server = Arc::new(server);
    loop {
        let (stream, _) = listener.accept().await?;
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, server).await {
                tracing::debug!("graph serve: connection error: {err:#}");
            }
        });
    }
}

async fn handle_connection(mut stream: TcpStream, server: Arc<GraphServer>) -> Result<()> {
    let mut head = Vec::new();
    let mut buf = [0u8; 2048];
    loop {
        let read = stream.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        head.extend_from_slice(&buf[..read]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") || head.len() > MAX_REQUEST_HEAD {
            break;
        }
    }
    let request_line = std::str::from_utf8(&head)
        .ok()
        .and_then(|text| text.lines().next())
        .unwrap_or("")
        .to_string();
    let (status, content_type, body) = respond(&request_line, &server).await;
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

/// Route one request line to (status, content type, body).
async fn respond(request_line: &str, server: &GraphServer) -> (&'static str, &'static str, String) {
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    if method != "GET" {
        return (
            "405 Method Not Allowed",
            "application/json",
            json!({"error": "GET only"}).to_string(),
        );
    }
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    };
    match path {
        "/" | "/index.html" => ("200 OK", "text/html; charset=utf-8", server.html.clone()),
        "/api/health" => (
            "200 OK",
            "application/json",
            json!({
                "ok": true,
                "embedder": embedder_summary(server.embedder.as_ref()),
            })
            .to_string(),
        ),
        "/api/search" => search_response(query, server).await,
        _ => (
            "404 Not Found",
            "application/json",
            json!({"error": "not found"}).to_string(),
        ),
    }
}

async fn search_response(
    query_string: &str,
    server: &GraphServer,
) -> (&'static str, &'static str, String) {
    let params = parse_query(query_string);
    let question = params
        .get("q")
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    if question.is_empty() {
        return (
            "400 Bad Request",
            "application/json",
            json!({"error": "missing q parameter"}).to_string(),
        );
    }
    let limit = params
        .get("limit")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT);
    // The SAME retrieval feature extraction runs (chaos_feature_context et al.):
    // multi-query expansion + hybrid search + the shared subject-recall floor.
    // The highlighted nodes are exactly the evidence that pipeline would gather.
    match query_feature_context_repo(
        &server.storage,
        server.repo_id,
        server.embedder.as_ref(),
        &question,
        limit,
    )
    .await
    {
        Ok(response) => (
            "200 OK",
            "application/json",
            json!({
                "query": question,
                "embedder": embedder_summary(server.embedder.as_ref()),
                "hits": response.hits,
            })
            .to_string(),
        ),
        // A failed search (embedder down, database gone) is a loud error —
        // the page must show the failure, not quietly degrade to substring.
        Err(err) => (
            "500 Internal Server Error",
            "application/json",
            json!({"error": format!("{err:#}")}).to_string(),
        ),
    }
}

fn embedder_summary(embedder: &dyn Embedder) -> serde_json::Value {
    json!({
        "provider": embedder.provider(),
        "model": embedder.model_id(),
        "dimensions": embedder.dimensions(),
    })
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some((percent_decode(key), percent_decode(value)))
        })
        .collect()
}

/// Minimal application/x-www-form-urlencoded decoding (`%XX` + `+` as space);
/// malformed escapes are kept literally rather than rejected.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|pair| u8::from_str_radix(pair, 16).ok());
                match hex {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_percent_escapes_and_plus() {
        assert_eq!(percent_decode("on+chain%20labs"), "on chain labs");
        assert_eq!(percent_decode("a%2Bb"), "a+b");
    }

    #[test]
    fn keeps_malformed_escapes_literal() {
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn parses_query_pairs() {
        let params = parse_query("q=access+control&limit=15");
        assert_eq!(params.get("q").map(String::as_str), Some("access control"));
        assert_eq!(params.get("limit").map(String::as_str), Some("15"));
    }

    #[test]
    fn ignores_pairs_without_equals() {
        let params = parse_query("q=x&flag&=v");
        assert_eq!(params.len(), 2);
        assert_eq!(params.get("q").map(String::as_str), Some("x"));
        assert_eq!(params.get("").map(String::as_str), Some("v"));
    }
}
