//! The SIX user-facing tools — single source of truth for CLI help, the MCP
//! `tools/list` roster and the roster test.

use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct ToolSpec {
    /// Bare name (`index`); MCP exposes it as `chaos_<name>`.
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

pub const TOOL_NAMES: [&str; 6] = [
    "index",
    "reindex",
    "add",
    "feature",
    "compose",
    "correlations",
];

pub fn mcp_name(name: &str) -> String {
    format!("chaos_{name}")
}

fn repo_prop() -> Value {
    json!({"type": "string", "description": "Repository alias (from chaos.toml) or path. Defaults to the current directory."})
}

pub fn tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "index",
            description: "Fully index a repository: tree-sitter structural extraction → project-wide import/call resolution → contextual chunks → real embeddings → feature communities (L1) → Merkle hashes (L2) → hash-gated feature summaries (L3). Run once per repo; afterwards use `add` for changes.",
            input_schema: json!({"type": "object", "properties": {"repo": repo_prop()}, "required": []}),
        },
        ToolSpec {
            name: "reindex",
            description: "Re-run a full index. No-op when git HEAD equals the indexed commit and the worktree is clean (unless `force`). Unchanged chunks keep their embeddings (content-hash reuse), so cost is proportional to what changed.",
            input_schema: json!({"type": "object", "properties": {"repo": repo_prop(), "force": {"type": "boolean", "default": false}}, "required": []}),
        },
        ToolSpec {
            name: "add",
            description: "Incrementally index what changed since the last index: files whose content hash differs from the stored one (plus, with `since`, everything git reports in that range) are re-extracted and analyzed against the persisted graph, files gone from disk are purged, only new chunks are embedded, and communities/summaries are recomputed only where a feature's hash changed. Use after every edit instead of `reindex`.",
            input_schema: json!({"type": "object", "properties": {"repo": repo_prop(), "since": {"type": "string", "description": "Optional git ref: also re-index everything git reports changed since it (e.g. HEAD~1, main). Content-hash drift is always detected regardless."}}, "required": []}),
        },
        ToolSpec {
            name: "feature",
            description: "Answer ONE feature question from the index WITHOUT reading source: routes to the matching feature communities, retrieves the relevant code chunks (semantic + keyword + literal), expands one hop over call/import edges, and returns a token-bounded payload — feature summaries, code excerpts with exact line ranges, the 1-hop graph, and the env vars / HTTP routes involved. Persists the answer as a page with a visual feature map (hash-gated). With `repos` or `all_repos` the question is answered in each selected repo of the project (chaos.toml) and joined by cross-repo links (an excerpt's symbol name found verbatim in another repo's excerpt) on ONE page under the project's docs/features_memory. Read the returned `excerpts[].code` instead of opening files.",
            input_schema: json!({"type": "object", "properties": {
                "question": {"type": "string", "description": "What you want to understand, e.g. 'how are embeddings reused on reindex'."},
                "repo": repo_prop(),
                "repos": {"type": "array", "items": {"type": "string"}, "description": "Answer across these project repos (aliases/paths) on one page."},
                "all_repos": {"type": "boolean", "default": false, "description": "Answer across every repo of the project."},
                "project": {"type": "string", "description": "Path to chaos.toml for `repos`/`all_repos`. Default: nearest ancestor of the current directory."},
                "budget_tokens": {"type": "integer", "default": 6000, "minimum": 1000, "maximum": 16000, "description": "Across repos, the total split evenly between them (at least 1000 each)."},
                "persist": {"type": "boolean", "default": true}
            }, "required": ["question"]}),
        },
        ToolSpec {
            name: "compose",
            description: "Write the feature website of a repository: an index page listing every feature community plus one drill-down page per feature (members, files, relations to other features, summary). Content-hash gated per page — the return reports written vs cached counts; cached pages must not be re-read.",
            input_schema: json!({"type": "object", "properties": {"repo": repo_prop(), "out": {"type": "string", "description": "Output directory. Default: <repo>/docs/features_memory/site"}, "limit": {"type": "integer", "description": "Max feature pages (largest communities first)."}}, "required": []}),
        },
        ToolSpec {
            name: "correlations",
            description: "Feature correlations. Intra-repo: pairs of features sharing files or symbols. Cross-repo (project from chaos.toml): consumer→provider links by `package_dep` (one repo imports a package another publishes) and `http_route` (a client call path matches a route another repo registers). Writes a report page and returns a compact list with evidence and confidence.",
            input_schema: json!({"type": "object", "properties": {
                "project": {"type": "string", "description": "Path to chaos.toml. Default: nearest ancestor of the current directory."},
                "repos": {"type": "array", "items": {"type": "string"}, "description": "Restrict to these aliases/paths."},
                "min_confidence": {"type": "number", "default": 0.5}
            }, "required": []}),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roster_matches_names() {
        let names: Vec<&str> = tools().iter().map(|t| t.name).collect();
        assert_eq!(names, TOOL_NAMES.to_vec());
        for t in tools() {
            assert!(t.input_schema["type"] == "object");
            assert!(!t.description.is_empty());
        }
    }
}
