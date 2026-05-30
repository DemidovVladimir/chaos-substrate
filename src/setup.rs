use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use tracing::{info, warn};

const DEFAULT_DATABASE_URL: &str = "postgres://chaos:chaos@localhost:54329/chaos_substrate";
const SERVER_NAME: &str = "chaos-substrate";

// ── editor detection ──────────────────────────────────────────────────────────

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

fn is_on_path(cmd: &str) -> bool {
    which_path(cmd).is_some()
}

fn which_path(cmd: &str) -> Option<PathBuf> {
    // Walk PATH entries; stat each candidate.
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(cmd);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// ── merge helpers ─────────────────────────────────────────────────────────────

/// Build the JSON server entry for the standard `{ "mcpServers": … }` shape.
fn mcp_server_entry(
    bin: &str,
    config: &str,
    database_url: &str,
    openai_key: Option<&str>,
) -> Value {
    let mut env_map = serde_json::Map::new();
    env_map.insert(
        "DATABASE_URL".into(),
        Value::String(database_url.to_string()),
    );
    if let Some(key) = openai_key {
        env_map.insert("OPENAI_API_KEY".into(), Value::String(key.to_string()));
    }

    serde_json::json!({
        "command": bin,
        "args": ["--config", config, "mcp"],
        "env": Value::Object(env_map)
    })
}

/// Merge `{ "mcpServers": { "<name>": entry } }` into `existing`.
/// Returns the merged value.  Existing keys other than `<name>` are preserved.
pub fn merge_mcp_servers_json(existing: Value, name: &str, entry: Value) -> Value {
    let mut root = match existing {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };

    let servers = root
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));

    if let Value::Object(ref mut map) = servers {
        map.insert(name.to_string(), entry);
    }

    Value::Object(root)
}

/// Merge `{ "mcp": { "<name>": entry } }` into `existing` (OpenCode shape).
pub fn merge_opencode_mcp_json(
    existing: Value,
    name: &str,
    bin: &str,
    config: &str,
    database_url: &str,
    openai_key: Option<&str>,
) -> Value {
    let mut env_map = serde_json::Map::new();
    env_map.insert(
        "DATABASE_URL".into(),
        Value::String(database_url.to_string()),
    );
    if let Some(key) = openai_key {
        env_map.insert("OPENAI_API_KEY".into(), Value::String(key.to_string()));
    }

    let entry = serde_json::json!({
        "type": "local",
        "command": [bin, "--config", config, "mcp"],
        "environment": Value::Object(env_map)
    });

    let mut root = match existing {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };

    let mcp = root
        .entry("mcp")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));

    if let Value::Object(ref mut map) = mcp {
        map.insert(name.to_string(), entry);
    }

    Value::Object(root)
}

// ── JSON file read-modify-write ───────────────────────────────────────────────

fn read_json_or_empty(path: &Path) -> Result<Value> {
    if path.exists() {
        let raw =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(Value::Object(serde_json::Map::new()));
        }
        serde_json::from_str(trimmed).with_context(|| format!("parsing JSON at {}", path.display()))
    } else {
        Ok(Value::Object(serde_json::Map::new()))
    }
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating dirs for {}", path.display()))?;
    }
    let text = serde_json::to_string_pretty(value).context("serialising JSON")?;
    fs::write(path, text + "\n").with_context(|| format!("writing {}", path.display()))
}

// ── per-editor registration ───────────────────────────────────────────────────

#[derive(Debug)]
pub enum EditorResult {
    Configured,
    DryRun(String),
    Skipped(String),
    Failed(String),
}

struct SetupArgs<'a> {
    bin: &'a str,
    config: &'a str,
    database_url: &'a str,
    openai_key: Option<&'a str>,
    scope: &'a str,
    dry_run: bool,
}

// Claude Code: delegate entirely to the `claude mcp add` CLI.
fn setup_claude_code(args: &SetupArgs<'_>) -> EditorResult {
    if !is_on_path("claude") {
        return EditorResult::Skipped(
            "claude CLI not found on PATH — install Claude Code first".into(),
        );
    }

    let mut cmd_args: Vec<String> = vec![
        "mcp".into(),
        "add".into(),
        SERVER_NAME.into(),
        "--scope".into(),
        args.scope.to_string(),
        "--env".into(),
        format!("DATABASE_URL={}", args.database_url),
    ];
    if let Some(key) = args.openai_key {
        cmd_args.push("--env".into());
        cmd_args.push(format!("OPENAI_API_KEY={key}"));
    }
    cmd_args.push("--".into());
    cmd_args.push(args.bin.to_string());
    cmd_args.push("--config".into());
    cmd_args.push(args.config.to_string());
    cmd_args.push("mcp".into());

    let cmd_display = format!("claude {}", cmd_args.join(" "));

    if args.dry_run {
        return EditorResult::DryRun(cmd_display);
    }

    info!("running: {cmd_display}");
    let status = Command::new("claude").args(&cmd_args).status();

    match status {
        Ok(s) if s.success() => EditorResult::Configured,
        Ok(s) => EditorResult::Failed(format!("`claude mcp add` exited with status {s}")),
        Err(e) => EditorResult::Failed(format!("failed to run claude CLI: {e}")),
    }
}

// Codex: try `codex mcp add` CLI first; fall back to ~/.codex/config.toml.
fn setup_codex(args: &SetupArgs<'_>) -> EditorResult {
    let codex_on_path = is_on_path("codex");
    let home = match home_dir() {
        Some(h) => h,
        None => return EditorResult::Skipped("HOME not set".into()),
    };
    let codex_dir = home.join(".codex");
    let dir_exists = codex_dir.exists();

    if !codex_on_path && !dir_exists {
        return EditorResult::Skipped("codex not on PATH and ~/.codex not found".into());
    }

    // Prefer CLI when available.
    if codex_on_path {
        let mut cmd_args: Vec<String> = vec![
            "mcp".into(),
            "add".into(),
            SERVER_NAME.into(),
            "--".into(),
            args.bin.to_string(),
            "--config".into(),
            args.config.to_string(),
            "mcp".into(),
        ];

        // Pass env flags if the CLI supports them (best-effort; ignore failure
        // and fall through to the config-file path).
        cmd_args.insert(3, format!("DATABASE_URL={}", args.database_url));
        cmd_args.insert(3, "--env".into());
        if let Some(key) = args.openai_key {
            cmd_args.insert(5, format!("OPENAI_API_KEY={key}"));
            cmd_args.insert(5, "--env".into());
        }

        let cmd_display = format!("codex {}", cmd_args.join(" "));

        if args.dry_run {
            return EditorResult::DryRun(cmd_display);
        }

        info!("running: {cmd_display}");
        let status = Command::new("codex").args(&cmd_args).status();
        match status {
            Ok(s) if s.success() => return EditorResult::Configured,
            Ok(s) => {
                warn!("codex CLI exited with {s}; falling back to ~/.codex/config.toml");
            }
            Err(e) => {
                warn!("codex CLI error ({e}); falling back to ~/.codex/config.toml");
            }
        }
    }

    // Config-file fallback.
    let config_path = codex_dir.join("config.toml");
    merge_codex_toml(args, &config_path)
}

fn merge_codex_toml(args: &SetupArgs<'_>, config_path: &Path) -> EditorResult {
    // Read existing TOML (or start empty).
    let raw = if config_path.exists() {
        match fs::read_to_string(config_path) {
            Ok(s) => s,
            Err(e) => {
                return EditorResult::Failed(format!("reading {}: {e}", config_path.display()))
            }
        }
    } else {
        String::new()
    };

    let mut doc: toml::Value = if raw.trim().is_empty() {
        toml::Value::Table(toml::map::Map::new())
    } else {
        match toml::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                return EditorResult::Failed(format!("parsing {}: {e}", config_path.display()))
            }
        }
    };

    // Build the server entry.
    let mut server_table = toml::map::Map::new();
    server_table.insert("command".into(), toml::Value::String(args.bin.to_string()));
    server_table.insert(
        "args".into(),
        toml::Value::Array(vec![
            toml::Value::String("--config".into()),
            toml::Value::String(args.config.to_string()),
            toml::Value::String("mcp".into()),
        ]),
    );
    let mut env_table = toml::map::Map::new();
    env_table.insert(
        "DATABASE_URL".into(),
        toml::Value::String(args.database_url.to_string()),
    );
    if let Some(key) = args.openai_key {
        env_table.insert(
            "OPENAI_API_KEY".into(),
            toml::Value::String(key.to_string()),
        );
    }
    server_table.insert("env".into(), toml::Value::Table(env_table));
    let server_entry = toml::Value::Table(server_table);

    // Navigate/create [mcp_servers] table.
    let root = match doc {
        toml::Value::Table(ref mut t) => t,
        _ => return EditorResult::Failed("config.toml root is not a table".into()),
    };
    let mcp_servers = root
        .entry("mcp_servers".to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));

    match mcp_servers {
        toml::Value::Table(ref mut t) => {
            t.insert(SERVER_NAME.to_string(), server_entry);
        }
        _ => return EditorResult::Failed("[mcp_servers] is not a table in config.toml".into()),
    }

    let serialized = match toml::to_string_pretty(&doc) {
        Ok(s) => s,
        Err(e) => return EditorResult::Failed(format!("serialising TOML: {e}")),
    };

    if args.dry_run {
        return EditorResult::DryRun(format!(
            "write {} with [mcp_servers.{}]:\n{}",
            config_path.display(),
            SERVER_NAME,
            serialized
        ));
    }

    if let Some(parent) = config_path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return EditorResult::Failed(format!("creating dirs: {e}"));
        }
    }
    if let Err(e) = fs::write(config_path, serialized) {
        return EditorResult::Failed(format!("writing {}: {e}", config_path.display()));
    }
    EditorResult::Configured
}

// Cursor: ~/.cursor/mcp.json
fn setup_cursor(args: &SetupArgs<'_>) -> EditorResult {
    let home = match home_dir() {
        Some(h) => h,
        None => return EditorResult::Skipped("HOME not set".into()),
    };
    let cursor_dir = home.join(".cursor");
    if !cursor_dir.exists() {
        return EditorResult::Skipped("~/.cursor/ not found".into());
    }

    let config_path = cursor_dir.join("mcp.json");
    let entry = mcp_server_entry(args.bin, args.config, args.database_url, args.openai_key);

    if args.dry_run {
        let preview = serde_json::json!({ "mcpServers": { SERVER_NAME: entry } });
        return EditorResult::DryRun(format!(
            "merge {} → mcpServers.{}: {}",
            config_path.display(),
            SERVER_NAME,
            serde_json::to_string_pretty(&preview).unwrap_or_default()
        ));
    }

    match read_json_or_empty(&config_path) {
        Ok(existing) => {
            let merged = merge_mcp_servers_json(existing, SERVER_NAME, entry);
            match write_json(&config_path, &merged) {
                Ok(()) => EditorResult::Configured,
                Err(e) => EditorResult::Failed(e.to_string()),
            }
        }
        Err(e) => EditorResult::Failed(format!("malformed JSON at {}: {e}", config_path.display())),
    }
}

// Windsurf: ~/.codeium/windsurf/mcp_config.json
fn setup_windsurf(args: &SetupArgs<'_>) -> EditorResult {
    let home = match home_dir() {
        Some(h) => h,
        None => return EditorResult::Skipped("HOME not set".into()),
    };
    let windsurf_dir = home.join(".codeium").join("windsurf");
    if !windsurf_dir.exists() {
        return EditorResult::Skipped("~/.codeium/windsurf/ not found".into());
    }

    let config_path = windsurf_dir.join("mcp_config.json");
    let entry = mcp_server_entry(args.bin, args.config, args.database_url, args.openai_key);

    if args.dry_run {
        let preview = serde_json::json!({ "mcpServers": { SERVER_NAME: entry } });
        return EditorResult::DryRun(format!(
            "merge {} → mcpServers.{}: {}",
            config_path.display(),
            SERVER_NAME,
            serde_json::to_string_pretty(&preview).unwrap_or_default()
        ));
    }

    match read_json_or_empty(&config_path) {
        Ok(existing) => {
            let merged = merge_mcp_servers_json(existing, SERVER_NAME, entry);
            match write_json(&config_path, &merged) {
                Ok(()) => EditorResult::Configured,
                Err(e) => EditorResult::Failed(e.to_string()),
            }
        }
        Err(e) => EditorResult::Failed(format!("malformed JSON at {}: {e}", config_path.display())),
    }
}

// OpenCode: ~/.config/opencode/config.json
fn setup_opencode(args: &SetupArgs<'_>) -> EditorResult {
    let home = match home_dir() {
        Some(h) => h,
        None => return EditorResult::Skipped("HOME not set".into()),
    };
    let opencode_dir = home.join(".config").join("opencode");
    if !opencode_dir.exists() {
        return EditorResult::Skipped("~/.config/opencode/ not found".into());
    }

    let config_path = opencode_dir.join("config.json");

    if args.dry_run {
        let mut env_map = serde_json::Map::new();
        env_map.insert(
            "DATABASE_URL".into(),
            Value::String(args.database_url.to_string()),
        );
        if let Some(key) = args.openai_key {
            env_map.insert("OPENAI_API_KEY".into(), Value::String(key.to_string()));
        }
        let preview = serde_json::json!({
            "mcp": {
                SERVER_NAME: {
                    "type": "local",
                    "command": [args.bin, "--config", args.config, "mcp"],
                    "environment": Value::Object(env_map)
                }
            }
        });
        return EditorResult::DryRun(format!(
            "merge {} → mcp.{}: {}",
            config_path.display(),
            SERVER_NAME,
            serde_json::to_string_pretty(&preview).unwrap_or_default()
        ));
    }

    match read_json_or_empty(&config_path) {
        Ok(existing) => {
            let merged = merge_opencode_mcp_json(
                existing,
                SERVER_NAME,
                args.bin,
                args.config,
                args.database_url,
                args.openai_key,
            );
            match write_json(&config_path, &merged) {
                Ok(()) => EditorResult::Configured,
                Err(e) => EditorResult::Failed(e.to_string()),
            }
        }
        Err(e) => EditorResult::Failed(format!("malformed JSON at {}: {e}", config_path.display())),
    }
}

// ── public entry point ────────────────────────────────────────────────────────

pub fn run(
    config_path: Option<&std::path::Path>,
    dry_run: bool,
    scope: Option<String>,
) -> Result<()> {
    let scope = scope.as_deref().unwrap_or("user").to_string();

    // 1. Resolve binary path.
    let bin = std::env::current_exe()
        .context("resolving current executable path")?
        .canonicalize()
        .context("canonicalizing current executable path")?;
    let bin_str = bin.to_string_lossy().to_string();

    // 2. Resolve config path (absolute).
    let config_abs = match config_path {
        Some(p) => p
            .canonicalize()
            .with_context(|| format!("canonicalizing config path {}", p.display()))?,
        None => {
            let default = std::path::Path::new("chaos-substrate.toml");
            if default.exists() {
                default
                    .canonicalize()
                    .context("canonicalizing default config path")?
            } else {
                // If the file doesn't exist yet, just use the absolute CWD-relative path.
                std::env::current_dir()
                    .context("getting cwd")?
                    .join("chaos-substrate.toml")
            }
        }
    };
    let config_str = config_abs.to_string_lossy().to_string();

    // 3. Resolve DATABASE_URL.
    let database_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string());

    // 4. Optional OPENAI_API_KEY.
    let openai_key_owned = std::env::var("OPENAI_API_KEY").ok();
    let openai_key = openai_key_owned.as_deref();

    if dry_run {
        println!("-- DRY RUN: no files written, no commands executed --");
    }

    println!("Binary:      {bin_str}");
    println!("Config:      {config_str}");
    println!("DatabaseURL: {database_url}");
    if openai_key.is_some() {
        println!("OpenAI key:  set (will be forwarded)");
    }
    println!();

    let sa = SetupArgs {
        bin: &bin_str,
        config: &config_str,
        database_url: &database_url,
        openai_key,
        scope: &scope,
        dry_run,
    };

    let results: Vec<(&str, EditorResult)> = vec![
        ("Claude Code", setup_claude_code(&sa)),
        ("Codex", setup_codex(&sa)),
        ("Cursor", setup_cursor(&sa)),
        ("Windsurf", setup_windsurf(&sa)),
        ("OpenCode", setup_opencode(&sa)),
    ];

    println!("── Setup summary ────────────────────────────────────────");
    let mut any_configured = false;
    for (editor, result) in &results {
        match result {
            EditorResult::Configured => {
                any_configured = true;
                println!("  {editor}: Configured");
            }
            EditorResult::DryRun(detail) => {
                any_configured = true;
                println!("  {editor}: [dry-run] would run/write:");
                for line in detail.lines() {
                    println!("    {line}");
                }
            }
            EditorResult::Skipped(reason) => {
                println!("  {editor}: Skipped ({reason})");
            }
            EditorResult::Failed(reason) => {
                warn!("setup failed for {editor}: {reason}");
                println!("  {editor}: Failed — {reason}");
            }
        }
    }
    println!("─────────────────────────────────────────────────────────");

    if any_configured {
        println!();
        println!("Next steps:");
        println!("  1. Restart your editor so it picks up the new MCP server.");
        println!("  2. Run `chaos doctor` to verify the database and embedder.");
    } else {
        println!();
        println!("No editors were detected. Install Claude Code, Cursor, Windsurf,");
        println!("OpenCode, or Codex and re-run `chaos setup`.");
    }

    Ok(())
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_mcp_servers_json_preserves_existing_server() {
        let existing = json!({
            "mcpServers": {
                "other-tool": {
                    "command": "other",
                    "args": []
                }
            }
        });
        let entry =
            json!({ "command": "/usr/bin/chaos", "args": ["--config", "/cfg.toml", "mcp"] });
        let merged = merge_mcp_servers_json(existing, "chaos-substrate", entry.clone());

        // The existing server must still be present.
        assert_eq!(
            merged["mcpServers"]["other-tool"]["command"],
            json!("other")
        );
        // The new server must be present.
        assert_eq!(
            merged["mcpServers"]["chaos-substrate"]["command"],
            json!("/usr/bin/chaos")
        );
    }

    #[test]
    fn merge_mcp_servers_json_replaces_existing_entry() {
        let existing = json!({
            "mcpServers": {
                "chaos-substrate": {
                    "command": "/old/chaos",
                    "args": []
                }
            }
        });
        let entry = json!({ "command": "/new/chaos", "args": ["--config", "/new.toml", "mcp"] });
        let merged = merge_mcp_servers_json(existing, "chaos-substrate", entry);

        assert_eq!(
            merged["mcpServers"]["chaos-substrate"]["command"],
            json!("/new/chaos")
        );
    }

    #[test]
    fn merge_mcp_servers_json_creates_servers_key_when_absent() {
        let existing = json!({ "someOtherKey": true });
        let entry = json!({ "command": "/usr/bin/chaos" });
        let merged = merge_mcp_servers_json(existing, "chaos-substrate", entry);

        assert!(merged["mcpServers"].is_object());
        assert_eq!(
            merged["mcpServers"]["chaos-substrate"]["command"],
            json!("/usr/bin/chaos")
        );
        // Pre-existing key must survive.
        assert_eq!(merged["someOtherKey"], json!(true));
    }

    #[test]
    fn merge_opencode_mcp_json_preserves_existing_server() {
        let existing = json!({
            "mcp": {
                "other-tool": { "type": "local", "command": ["other"] }
            }
        });
        let merged = merge_opencode_mcp_json(
            existing,
            "chaos-substrate",
            "/usr/bin/chaos",
            "/cfg.toml",
            "postgres://localhost/db",
            None,
        );

        assert!(merged["mcp"]["other-tool"].is_object());
        assert_eq!(merged["mcp"]["chaos-substrate"]["type"], json!("local"));
        assert_eq!(
            merged["mcp"]["chaos-substrate"]["environment"]["DATABASE_URL"],
            json!("postgres://localhost/db")
        );
    }

    #[test]
    fn merge_opencode_mcp_json_includes_openai_key_when_set() {
        let existing = json!({});
        let merged = merge_opencode_mcp_json(
            existing,
            "chaos-substrate",
            "/usr/bin/chaos",
            "/cfg.toml",
            "postgres://localhost/db",
            Some("sk-test-key"),
        );

        assert_eq!(
            merged["mcp"]["chaos-substrate"]["environment"]["OPENAI_API_KEY"],
            json!("sk-test-key")
        );
    }

    #[test]
    fn merge_opencode_mcp_json_omits_openai_key_when_not_set() {
        let existing = json!({});
        let merged = merge_opencode_mcp_json(
            existing,
            "chaos-substrate",
            "/usr/bin/chaos",
            "/cfg.toml",
            "postgres://localhost/db",
            None,
        );

        assert!(merged["mcp"]["chaos-substrate"]["environment"]["OPENAI_API_KEY"].is_null());
    }

    #[test]
    fn mcp_server_entry_includes_openai_key_when_provided() {
        let entry = mcp_server_entry("/bin/chaos", "/cfg.toml", "postgres://x/db", Some("sk-abc"));
        assert_eq!(entry["env"]["OPENAI_API_KEY"], json!("sk-abc"));
        assert_eq!(entry["command"], json!("/bin/chaos"));
    }

    #[test]
    fn mcp_server_entry_omits_openai_key_when_none() {
        let entry = mcp_server_entry("/bin/chaos", "/cfg.toml", "postgres://x/db", None);
        assert!(entry["env"]["OPENAI_API_KEY"].is_null());
    }
}
