use crate::{
    embedding::{build_embedder, Embedder},
    extractor::{current_commit, RustRepositoryExtractor},
    query::query_repo,
    storage::Storage,
    Config,
};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    io::{BufRead, Write},
    path::Path,
};

pub async fn run(config: Config) -> Result<()> {
    let storage = Storage::connect(&config.storage.database_url).await?;
    let embedder = build_embedder(&config.embedding)?;
    let mut stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    while let Some(message) = read_message(&mut stdin)? {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "chaos-substrate", "version": env!("CARGO_PKG_VERSION")}
                }
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        {
                            "name": "chaos_analyze",
                            "description": "Analyze and persist a repository knowledge graph and real embeddings. Replaces stale indexed data for that repository.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo_path": {"type": "string"}
                                },
                                "required": ["repo_path"]
                            }
                        },
                        {
                            "name": "chaos_query",
                            "description": "Query persisted code knowledge memory with hybrid semantic, keyword, and graph context routing.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo": {"type": "string"},
                                    "question": {"type": "string"},
                                    "limit": {"type": "integer", "default": 10}
                                },
                                "required": ["repo", "question"]
                            }
                        }
                    ]
                }
            }),
            "tools/call" => {
                let params = message.get("params").cloned().unwrap_or_default();
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match handle_tool_call(
                    name,
                    params.get("arguments").cloned().unwrap_or_default(),
                    &config,
                    &storage,
                    embedder.as_ref(),
                )
                .await
                {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err(err) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "isError": true,
                            "content": [{"type": "text", "text": err.to_string()}]
                        }
                    }),
                }
            }
            "notifications/initialized" => continue,
            _ => json_error(id, -32601, "unknown method"),
        };
        write_message(&mut stdout, &response)?;
    }
    Ok(())
}

async fn handle_tool_call(
    name: &str,
    args: Value,
    config: &Config,
    storage: &Storage,
    embedder: &dyn Embedder,
) -> Result<Value> {
    match name {
        "chaos_analyze" => {
            let repo_path = args
                .get("repo_path")
                .and_then(Value::as_str)
                .context("repo_path is required")?;
            let summary = analyze_repo(config, storage, embedder, Path::new(repo_path)).await?;
            Ok(tool_text(summary))
        }
        "chaos_query" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let question = args
                .get("question")
                .and_then(Value::as_str)
                .context("question is required")?;
            let limit = args.get("limit").and_then(Value::as_i64).unwrap_or(10);
            let repo = storage
                .find_repository(repo)
                .await?
                .context("repository is not indexed")?;
            let answer = query_repo(storage, repo.id, embedder, question, limit).await?;
            Ok(tool_text(serde_json::to_string_pretty(&answer)?))
        }
        _ => anyhow::bail!("unknown tool: {name}"),
    }
}

async fn analyze_repo(
    config: &Config,
    storage: &Storage,
    embedder: &dyn Embedder,
    repo_path: &Path,
) -> Result<String> {
    let commit = current_commit(repo_path);
    let repo = storage
        .upsert_repository(repo_path, commit.as_deref())
        .await?;
    let run_id = storage.begin_analysis(repo.id, commit.as_deref()).await?;
    let outcome = async {
        let extractor = RustRepositoryExtractor::new(config.indexing.clone());
        let result = extractor.extract(repo_path, repo.id, commit)?;
        storage.replace_repo_index(repo.id, &result).await?;
        let missing = storage
            .chunks_missing_embeddings(
                repo.id,
                embedder.provider(),
                embedder.model_id(),
                embedder.dimensions(),
            )
            .await?;
        for chunk in &missing {
            let embedding = embedder.embed(&chunk.content).await?;
            storage
                .insert_embedding(
                    chunk,
                    embedder.provider(),
                    embedder.model_id(),
                    embedder.dimensions(),
                    &embedding,
                )
                .await?;
        }
        Result::<_, anyhow::Error>::Ok(json!({
            "repo_id": repo.id,
            "files": result.files.len(),
            "nodes": result.nodes.len(),
            "edges": result.edges.len(),
            "chunks": result.chunks.len(),
            "embedded_chunks": missing.len()
        }))
    }
    .await;

    match outcome {
        Ok(summary) => {
            storage.finish_analysis(run_id, "completed", None).await?;
            Ok(serde_json::to_string_pretty(&summary)?)
        }
        Err(err) => {
            storage
                .finish_analysis(run_id, "failed", Some(&err.to_string()))
                .await?;
            Err(err)
        }
    }
}

fn tool_text(text: String) -> Value {
    json!({"content": [{"type": "text", "text": text}]})
}

fn json_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn read_message(stdin: &mut std::io::Stdin) -> Result<Option<Value>> {
    let mut line = String::new();
    let bytes_read = stdin.lock().read_line(&mut line)?;
    if bytes_read == 0 {
        return Ok(None);
    }
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return read_message(stdin);
    }
    Ok(Some(serde_json::from_str(trimmed)?))
}

fn write_message(stdout: &mut std::io::Stdout, message: &Value) -> Result<()> {
    let body = serde_json::to_string(message)?;
    stdout.write_all(body.as_bytes())?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}
