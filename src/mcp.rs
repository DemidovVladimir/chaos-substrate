use crate::{
    embedding::{build_embedder, Embedder},
    extractor::{current_commit, RustRepositoryExtractor},
    feature_context::{
        build_feature_context_warnings, load_feature_matches, write_feature_context_html,
        FeatureContextResponse,
    },
    query::{query_feature_context_repo, query_repo},
    storage::Storage,
    Config,
};
use anyhow::{Context, Result};
use futures::{StreamExt, TryStreamExt};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, Write},
    path::{Path, PathBuf},
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
                        },
                        {
                            "name": "chaos_feature_context",
                            "description": "Build focused implementation context for a feature or task. Reads Postgres retrieval plus generated feature-memory manifests and returns warnings when expected paths/docs are missing. Use this before composing any feature website; treat warnings as blockers before writing.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo": {"type": "string"},
                                    "task": {"type": "string"},
                                    "limit": {"type": "integer", "default": 10},
                                    "feature_limit": {"type": "integer", "default": 3},
                                    "nodes_per_feature": {"type": "integer", "default": 8},
                                    "features_dir": {"type": "string"},
                                    "output_html": {"type": "string"}
                                },
                                "required": ["repo", "task"]
                            }
                        },
                        {
                            "name": "chaos_write_feature_website",
                            "description": "Write an LLM-composed interactive feature website into docs/features_memory with an embedded chaos-feature-manifest JSON block. Use after chaos_feature_context, not as a substitute for understanding the feature. HTML must include interactive graph, story flow, architecture, code, and evidence sections.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "repo": {"type": "string"},
                                    "slug": {"type": "string"},
                                    "title": {"type": "string"},
                                    "html": {"type": "string"},
                                    "manifest": {"type": "object"}
                                },
                                "required": ["repo", "slug", "title", "html", "manifest"]
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
        "chaos_feature_context" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let task = args
                .get("task")
                .and_then(Value::as_str)
                .context("task is required")?;
            let limit = args.get("limit").and_then(Value::as_i64).unwrap_or(10);
            let feature_limit = args
                .get("feature_limit")
                .and_then(Value::as_u64)
                .unwrap_or(3) as usize;
            let nodes_per_feature = args
                .get("nodes_per_feature")
                .and_then(Value::as_u64)
                .unwrap_or(8) as usize;
            let repo = storage
                .find_repository(repo)
                .await?
                .context("repository is not indexed")?;
            let repo_root = PathBuf::from(&repo.root_path);
            let features_dir = args
                .get("features_dir")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| repo_root.join("docs/features_memory"));
            let postgres =
                query_feature_context_repo(storage, repo.id, embedder, task, limit).await?;
            let warnings = build_feature_context_warnings(task, &repo_root, &postgres);
            let feature_matches =
                load_feature_matches(task, &features_dir, feature_limit, nodes_per_feature)?;
            let response = FeatureContextResponse {
                task: task.to_string(),
                postgres,
                features_dir,
                warnings,
                feature_matches,
            };
            let output_html = args.get("output_html").and_then(Value::as_str);
            if let Some(output_html) = output_html {
                write_feature_context_html(Path::new(output_html), &response)?;
            }
            Ok(tool_text(serde_json::to_string_pretty(&json!({
                "wrote_html": output_html,
                "context": response
            }))?))
        }
        "chaos_write_feature_website" => {
            let repo = args
                .get("repo")
                .and_then(Value::as_str)
                .context("repo is required")?;
            let slug = args
                .get("slug")
                .and_then(Value::as_str)
                .context("slug is required")?;
            let title = args
                .get("title")
                .and_then(Value::as_str)
                .context("title is required")?;
            let html = args
                .get("html")
                .and_then(Value::as_str)
                .context("html is required")?;
            let manifest = args.get("manifest").context("manifest is required")?;
            let repo = storage
                .find_repository(repo)
                .await?
                .context("repository is not indexed")?;
            let path = write_llm_feature_website(&repo.root_path, slug, title, html, manifest)?;
            Ok(tool_text(serde_json::to_string_pretty(&json!({
                "output_html": path,
                "manifest_embedded": true
            }))?))
        }
        _ => anyhow::bail!("unknown tool: {name}"),
    }
}

fn write_llm_feature_website(
    repo_root: &str,
    slug: &str,
    title: &str,
    html: &str,
    manifest: &Value,
) -> Result<PathBuf> {
    let slug = safe_slug(slug);
    let output = Path::new(repo_root)
        .join("docs/features_memory")
        .join(format!("{slug}-explanation.html"));
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let manifest_json = serde_json::to_string_pretty(manifest)?;
    let manifest_block = format!(
        r#"<script type="application/json" id="chaos-feature-manifest">
{}
</script>"#,
        escape_script_json_for_html(&manifest_json)
    );
    if html.contains("id=\"chaos-feature-manifest\"")
        || html.contains("id='chaos-feature-manifest'")
    {
        anyhow::bail!(
            "html must not include chaos-feature-manifest; pass the manifest argument and the tool will embed it"
        );
    }
    validate_feature_website_contract(html, manifest)?;
    let page = format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{}</title>
</head>
<body>
{}
{}
</body>
</html>
"#,
        escape_html(title),
        html,
        manifest_block
    );
    fs::write(&output, page)?;
    Ok(output)
}

fn validate_feature_website_contract(html: &str, manifest: &Value) -> Result<()> {
    let required_manifest = [
        ("claims", 3usize),
        ("modes", 2usize),
        ("nodes", 5usize),
        ("edges", 3usize),
        ("story", 3usize),
    ];
    for (field, minimum) in required_manifest {
        let count = manifest
            .get(field)
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        if count < minimum {
            anyhow::bail!(
                "manifest.{field} must contain at least {minimum} items for an evidence-backed feature website; got {count}"
            );
        }
    }

    let required_html_markers = [
        "data-chaos-feature-website",
        "data-chaos-graph",
        "data-node-id",
        "data-chaos-story",
        "data-story-step",
        "data-chaos-architecture",
        "data-chaos-flow",
        "data-chaos-code",
        "data-chaos-evidence",
    ];
    for marker in required_html_markers {
        if !html.contains(marker) {
            anyhow::bail!("html is missing required interactive feature website marker `{marker}`");
        }
    }

    let lowercase = html.to_ascii_lowercase();
    if !lowercase.contains("<script") || !html.contains("addEventListener") {
        anyhow::bail!(
            "html must include JavaScript interactivity with event listeners for graph/story/code navigation"
        );
    }

    Ok(())
}

fn safe_slug(input: &str) -> String {
    let slug = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "feature-context".to_string()
    } else {
        slug
    }
}

fn escape_script_json_for_html(json: &str) -> String {
    json.replace("</script", "<\\/script")
}

fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
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
        futures::stream::iter(missing.iter().map(|chunk| async move {
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
            Result::<_, anyhow::Error>::Ok(())
        }))
        .buffer_unordered(crate::EMBED_CONCURRENCY)
        .try_collect::<()>()
        .await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_manifest() -> Value {
        json!({
            "claims": [{}, {}, {}],
            "modes": [{}, {}],
            "nodes": [{}, {}, {}, {}, {}],
            "edges": [{}, {}, {}],
            "story": [{}, {}, {}]
        })
    }

    #[test]
    fn feature_website_contract_rejects_readme_like_html() {
        let err = validate_feature_website_contract(
            "<section><h1>Feature</h1></section>",
            &valid_manifest(),
        )
        .expect_err("plain prose should not pass as a feature website");
        assert!(err.to_string().contains("data-chaos-feature-website"));
    }

    #[test]
    fn feature_website_contract_accepts_interactive_surface() {
        let html = r#"
          <main data-chaos-feature-website>
            <section data-chaos-architecture></section>
            <section data-chaos-flow></section>
            <svg data-chaos-graph><g data-node-id="a"></g></svg>
            <ol data-chaos-story><li data-story-step="one"></li></ol>
            <pre data-chaos-code></pre>
            <aside data-chaos-evidence></aside>
          </main>
          <script>document.querySelector('[data-node-id]').addEventListener('click', () => {});</script>
        "#;
        validate_feature_website_contract(html, &valid_manifest()).unwrap();
    }
}
