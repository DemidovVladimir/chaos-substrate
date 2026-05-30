/// Claude Code / Cursor plugin hook subcommand.
///
/// Reads a Claude Code hook event JSON from stdin and — when the current repo
/// is indexed — writes a PreToolUse augmentation JSON to stdout that injects
/// relevant code-memory context.  The hook MUST be fast and MUST degrade to a
/// safe no-op (exit 0, no stdout) on ANY problem so it never breaks the host
/// tool call.
///
/// Robustness contract:
///   - `run()` wraps everything; on any `Err` it logs to stderr and returns
///     without writing anything to stdout.
///   - Exit code is always 0 (the caller in main.rs does `std::process::exit(0)`
///     after this function returns, regardless of whether we produced output).
use crate::storage::{Storage, SymbolHit};
use anyhow::Result;
use serde_json::{json, Value};
use std::io::Read;
use std::time::Duration;
use tracing::{debug, warn};

// ── public entry point ────────────────────────────────────────────────────────

/// Run the hook subcommand.
///
/// Reads stdin, optionally produces one JSON line on stdout, then returns.
/// Never propagates errors to the caller – all failures are silent no-ops.
pub async fn run(event: &str, format: Option<&str>) {
    match try_run(event, format).await {
        Ok(Some(line)) => println!("{line}"),
        Ok(None) => {} // no-op
        Err(err) => {
            // Log to stderr only; do NOT write anything to stdout.
            warn!("chaos hook: {err:#}");
        }
    }
}

// ── internal implementation ───────────────────────────────────────────────────

async fn try_run(event: &str, format: Option<&str>) -> Result<Option<String>> {
    // 1. Read all of stdin.
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| anyhow::anyhow!("reading stdin: {e}"))?;

    let trimmed = input.trim();
    if trimmed.is_empty() {
        debug!("chaos hook: empty stdin, no-op");
        return Ok(None);
    }

    // 2. Parse the event JSON.
    let payload: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => {
            debug!("chaos hook: invalid JSON on stdin ({e}), no-op");
            return Ok(None);
        }
    };

    let tool_name = payload["tool_name"].as_str().unwrap_or("").to_string();
    let cwd = payload["cwd"].as_str().unwrap_or("").to_string();

    if cwd.is_empty() {
        debug!("chaos hook: no cwd in payload, no-op");
        return Ok(None);
    }

    // 3. Handle PostToolUse separately.
    if event == "PostToolUse" {
        return handle_post_tool_use(&tool_name, &payload, format);
    }

    // 4. PreToolUse: derive a search term.
    let search_term = match derive_search_term(&tool_name, &payload) {
        Some(t) => t,
        None => {
            debug!("chaos hook: no meaningful search term for tool {tool_name}, no-op");
            return Ok(None);
        }
    };

    // 5. Connect to the database with a short timeout.
    let db_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        // Try to load from env var that might be set via config; we can't call
        // Config::load here without a config path, so just fall back to the
        // default.  Callers that set --config will have DATABASE_URL in env.
        "postgres://chaos:chaos@localhost:54329/chaos_substrate".to_string()
    });

    let storage = match Storage::connect_fast(&db_url, Duration::from_secs(3)).await {
        Ok(s) => s,
        Err(e) => {
            debug!("chaos hook: DB connect failed ({e}), no-op");
            return Ok(None);
        }
    };

    // 6. Look up the repository by cwd.
    let repo = match storage.find_repository(&cwd).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            debug!("chaos hook: repo not indexed for cwd={cwd}, no-op");
            return Ok(None);
        }
        Err(e) => {
            debug!("chaos hook: repo lookup failed ({e}), no-op");
            return Ok(None);
        }
    };

    // 7. Fast symbol/name lookup (no embedder).
    let hits = match storage
        .search_symbols_by_name(repo.id, &search_term, 5)
        .await
    {
        Ok(h) => h,
        Err(e) => {
            debug!("chaos hook: symbol search failed ({e}), no-op");
            return Ok(None);
        }
    };

    if hits.is_empty() {
        debug!("chaos hook: no symbols matched \"{search_term}\", no-op");
        return Ok(None);
    }

    // 8. Format context.
    let context = format_context(&search_term, &hits);
    let output = build_pre_tool_use_output(&context, "PreToolUse", format);
    Ok(Some(output))
}

// ── PostToolUse ───────────────────────────────────────────────────────────────

fn handle_post_tool_use(
    tool_name: &str,
    payload: &Value,
    format: Option<&str>,
) -> Result<Option<String>> {
    if tool_name != "Bash" {
        return Ok(None);
    }
    let command = payload["tool_input"]["command"].as_str().unwrap_or("");
    if !is_repo_mutating_git_command(command) {
        return Ok(None);
    }

    let context = "The repository may have changed (git operation detected). \
        The Chaos Substrate index may now be stale — run `chaos analyze <repo>` \
        or use the `chaos_analyze` MCP tool to refresh the code-memory index."
        .to_string();

    let output = build_pre_tool_use_output(&context, "PostToolUse", format);
    Ok(Some(output))
}

// ── pure helpers (unit-testable, no DB) ──────────────────────────────────────

/// Returns true if the bash command looks like it mutated the git history.
pub fn is_repo_mutating_git_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.contains("git commit")
        || lower.contains("git merge")
        || lower.contains("git pull")
        || lower.contains("git checkout")
}

/// Derive a search term from the tool name + payload.
///
/// Returns `None` when no useful term can be extracted (→ no-op).
pub fn derive_search_term(tool_name: &str, payload: &Value) -> Option<String> {
    match tool_name {
        "Grep" => {
            let pattern = payload["tool_input"]["pattern"].as_str()?;
            Some(strip_simple_regex_meta(pattern))
        }
        "Glob" => {
            let pattern = payload["tool_input"]["pattern"].as_str()?;
            Some(strip_simple_regex_meta(pattern))
        }
        "Bash" => {
            let command = payload["tool_input"]["command"].as_str()?;
            extract_bash_identifier(command)
        }
        _ => None,
    }
}

/// Strip simple regex metacharacters that appear at the start/end of a
/// Grep/Glob pattern so the remaining token is a plain identifier.
pub fn strip_simple_regex_meta(pattern: &str) -> String {
    // Remove common anchors / quantifiers / character-class brackets from the
    // edges of the string. Keep the core alphanumeric/underscore word.
    let stripped = pattern.trim_matches(|c: char| {
        matches!(
            c,
            '^' | '$' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '\\'
        )
    });
    // If after stripping nothing useful remains, return the original.
    if stripped.is_empty() {
        pattern.to_string()
    } else {
        stripped.to_string()
    }
}

/// Extract a meaningful identifier from a bash command string.
///
/// Strategy: split on whitespace + common shell operators and return the
/// longest alphanumeric/underscore token that does not look like a flag or a
/// common shell keyword.
pub fn extract_bash_identifier(command: &str) -> Option<String> {
    // Tokens to skip (common shell keywords / commands that carry no semantic
    // identifier useful for a code-memory lookup).
    const SKIP_TOKENS: &[&str] = &[
        "git", "cargo", "npm", "yarn", "pnpm", "make", "bash", "sh", "zsh", "echo", "cat", "ls",
        "cd", "pwd", "grep", "rg", "find", "rm", "cp", "mv", "mkdir", "touch", "chmod", "chown",
        "curl", "wget", "sudo", "true", "false", "if", "then", "else", "fi", "for", "do", "done",
        "while", "case", "esac", "in", "return", "export", "source", ".", "status", "diff", "log",
        "pull", "push", "fetch", "add", "commit", "checkout", "merge", "rebase", "stash", "reset",
        "clean", "run", "test", "build", "check", "fmt", "clippy", "install", "update", "init",
        "new", "help", "version", "--", "-", "&&", "||", "|", ";",
    ];

    command
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| {
            !t.is_empty()
                && !t.starts_with('-') // skip flags
                && t.chars().any(|c| c.is_alphabetic()) // at least one letter
                && !SKIP_TOKENS.contains(&t.to_ascii_lowercase().as_str())
        })
        .max_by_key(|t| t.len())
        .filter(|t| t.len() >= 4) // skip very short tokens
        .map(|t| t.to_string())
}

/// Build the Claude Code PreToolUse (or PostToolUse) augmentation JSON string.
pub fn build_pre_tool_use_output(
    additional_context: &str,
    hook_event_name: &str,
    format: Option<&str>,
) -> String {
    match format.unwrap_or("claude") {
        "cursor" => {
            let v = json!({
                "permission": "allow",
                "agent_message": additional_context
            });
            serde_json::to_string(&v).unwrap_or_default()
        }
        _ => {
            // Default: Claude Code format.
            let v = json!({
                "continue": true,
                "hookSpecificOutput": {
                    "hookEventName": hook_event_name,
                    "permissionDecision": "allow",
                    "additionalContext": additional_context
                }
            });
            serde_json::to_string(&v).unwrap_or_default()
        }
    }
}

/// Format the top symbol hits into a short one-line context string.
fn format_context(term: &str, hits: &[SymbolHit]) -> String {
    let parts: Vec<String> = hits
        .iter()
        .map(|h| {
            if let Some(line) = h.line_start {
                format!("{} ({}) at {}:{}", h.name, h.kind, h.file, line)
            } else {
                format!("{} ({}) at {}", h.name, h.kind, h.file)
            }
        })
        .collect();
    format!(
        "Chaos Substrate memory for \"{term}\": {}",
        parts.join("; ")
    )
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── derive_search_term ────────────────────────────────────────────────────

    #[test]
    fn grep_pattern_is_returned_as_term() {
        let payload =
            json!({ "tool_name": "Grep", "tool_input": { "pattern": "Storage", "path": "/repo" } });
        assert_eq!(
            derive_search_term("Grep", &payload),
            Some("Storage".to_string())
        );
    }

    #[test]
    fn glob_pattern_is_returned_as_term() {
        let payload =
            json!({ "tool_name": "Glob", "tool_input": { "pattern": "*.rs", "path": "/repo" } });
        let term = derive_search_term("Glob", &payload).unwrap();
        // After meta-stripping '*.rs' → '.rs' → 'rs'; the dot is also stripped.
        assert!(!term.is_empty());
    }

    #[test]
    fn grep_regex_anchor_stripped() {
        let payload = json!({ "tool_input": { "pattern": "^fn query_repo" } });
        let term = derive_search_term("Grep", &payload).unwrap();
        assert!(!term.starts_with('^'));
        assert!(term.contains("fn") || term.contains("query"));
    }

    #[test]
    fn bash_long_identifier_extracted() {
        let payload = json!({ "tool_input": { "command": "cargo test search_symbols_by_name" } });
        let term = derive_search_term("Bash", &payload).unwrap();
        assert_eq!(term, "search_symbols_by_name");
    }

    #[test]
    fn bash_git_status_is_noop() {
        let payload = json!({ "tool_input": { "command": "git status" } });
        // "status" is in the skip list; "git" is in the skip list → None
        assert_eq!(derive_search_term("Bash", &payload), None);
    }

    #[test]
    fn bash_short_token_is_noop() {
        let payload = json!({ "tool_input": { "command": "ls" } });
        assert_eq!(derive_search_term("Bash", &payload), None);
    }

    #[test]
    fn unknown_tool_is_noop() {
        let payload = json!({ "tool_input": {} });
        assert_eq!(derive_search_term("Write", &payload), None);
        assert_eq!(derive_search_term("Read", &payload), None);
        assert_eq!(derive_search_term("Edit", &payload), None);
    }

    // ── build_pre_tool_use_output ─────────────────────────────────────────────

    #[test]
    fn pre_tool_use_output_has_correct_shape() {
        let out = build_pre_tool_use_output("some context", "PreToolUse", None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["continue"], json!(true));
        assert_eq!(
            v["hookSpecificOutput"]["hookEventName"],
            json!("PreToolUse")
        );
        assert_eq!(
            v["hookSpecificOutput"]["permissionDecision"],
            json!("allow")
        );
        assert_eq!(
            v["hookSpecificOutput"]["additionalContext"],
            json!("some context")
        );
    }

    #[test]
    fn post_tool_use_output_has_correct_hook_event_name() {
        let out = build_pre_tool_use_output("stale index", "PostToolUse", None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["hookSpecificOutput"]["hookEventName"],
            json!("PostToolUse")
        );
        assert_eq!(
            v["hookSpecificOutput"]["permissionDecision"],
            json!("allow")
        );
    }

    #[test]
    fn cursor_format_emits_flat_shape() {
        let out = build_pre_tool_use_output("ctx", "PreToolUse", Some("cursor"));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["permission"], json!("allow"));
        assert_eq!(v["agent_message"], json!("ctx"));
        // Must NOT have the Claude Code nested shape.
        assert!(v.get("hookSpecificOutput").is_none());
    }

    // ── is_repo_mutating_git_command ──────────────────────────────────────────

    #[test]
    fn git_commit_is_mutating() {
        assert!(is_repo_mutating_git_command("git commit -m 'fix'"));
        assert!(is_repo_mutating_git_command("git merge origin/main"));
        assert!(is_repo_mutating_git_command("git pull origin main"));
        assert!(is_repo_mutating_git_command("git checkout -b feature"));
    }

    #[test]
    fn git_status_is_not_mutating() {
        assert!(!is_repo_mutating_git_command("git status"));
        assert!(!is_repo_mutating_git_command("git log --oneline"));
        assert!(!is_repo_mutating_git_command("git diff HEAD"));
        assert!(!is_repo_mutating_git_command("echo hello"));
    }

    // ── strip_simple_regex_meta ───────────────────────────────────────────────

    #[test]
    fn anchors_stripped_from_pattern() {
        assert_eq!(strip_simple_regex_meta("^Storage"), "Storage");
        assert_eq!(strip_simple_regex_meta("Storage$"), "Storage");
        assert_eq!(strip_simple_regex_meta("^Storage$"), "Storage");
    }

    #[test]
    fn plain_word_unchanged() {
        assert_eq!(strip_simple_regex_meta("Storage"), "Storage");
        assert_eq!(strip_simple_regex_meta("query_repo"), "query_repo");
    }

    // ── extract_bash_identifier ───────────────────────────────────────────────

    #[test]
    fn extracts_longest_meaningful_token() {
        assert_eq!(
            extract_bash_identifier("cargo test search_symbols_by_name"),
            Some("search_symbols_by_name".to_string())
        );
        assert_eq!(
            extract_bash_identifier("rg 'fn query_repo' src/"),
            Some("query_repo".to_string())
        );
    }

    #[test]
    fn all_skipped_tokens_returns_none() {
        assert_eq!(extract_bash_identifier("git status"), None);
        assert_eq!(extract_bash_identifier("ls -la"), None);
        assert_eq!(extract_bash_identifier("cd /tmp"), None);
    }
}
