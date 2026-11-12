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
            description: "Fully index a repository: tree-sitter structural extraction → project-wide import/call resolution → contextual chunks → real embeddings → feature communities (L1) → feature summaries (L2), each embedded only when its text changed. Run once per repo; afterwards use `add` for changes.",
            input_schema: json!({"type": "object", "properties": {"repo": repo_prop()}, "required": []}),
        },
        ToolSpec {
            name: "reindex",
            description: "Bring the index up to the current HEAD. No-op when HEAD is the stamped commit, the worktree is clean, nothing was uncommitted at the last run and the indexing rules (skip_dirs, Foundry libs, languages) are unchanged. Otherwise re-reads the same git delta as `add` plus the unchanged files one hop upstream of each change (files with an imports, calls, extends, implements, queries, emits, reverts, binds, resolves or uses_abi edge into it), so their edges reach new symbols. Runs a full index instead, and says why, when the stamp is unusable: missing, left incomplete by a run that failed, written under other indexing rules, or its commit is not in this clone. `force` always runs the full index (use it after a Chaos upgrade that changes extraction). Unchanged chunks keep their embeddings (content-hash reuse).",
            input_schema: json!({"type": "object", "properties": {"repo": repo_prop(), "force": {"type": "boolean", "default": false}}, "required": []}),
        },
        ToolSpec {
            name: "add",
            description: "Incrementally index what changed since the last run. Every index/reindex/add stamps the repo with HEAD, the paths uncommitted at that moment and its indexing rules; `add` re-reads only the git delta from that stamp: files changed between the stamped commit and the current HEAD (a two-tree diff, so branch switches and rebases work), files uncommitted now (untracked included) and files uncommitted at the stamp. Git only names candidates: each is kept exactly when the full listing would have it (.gitignore/.ignore, skip_dirs, Foundry libs, languages, no symlinks; a changed .gitignore/.ignore re-checks its directory), and re-read unless its content hash equals the indexed one. Deleted, renamed-away and no-longer-listed files are purged. No full scan: only without a usable stamp (none yet, left incomplete by a failed run, other indexing rules, commit not in this clone, not a git repository) does it reconcile every file by content hash, with a warning saying why; after a failed run it also finishes embedding and the hierarchy. Re-read files are analyzed against the persisted graph, only new chunks are embedded, and communities are rebuilt with a summary embedded again only where its text changed. A run with nothing to re-read writes only the new stamp, or nothing when the stamp is current. Use after every edit, commit, pull or checkout.",
            input_schema: json!({"type": "object", "properties": {"repo": repo_prop(), "since": {"type": "string", "description": "Optional git ref (e.g. HEAD~3, main): also re-check every file changed between it and HEAD, on top of the stamp delta. Must name a commit."}}, "required": []}),
        },
        ToolSpec {
            name: "feature",
            description: "Answer ONE feature question from the index WITHOUT reading source: routes to the matching feature communities, retrieves the relevant code chunks (semantic + keyword + literal), expands one hop over call/import edges, and returns a token-bounded payload — feature summaries, code excerpts with exact line ranges, the 1-hop graph, and the env vars / HTTP routes / MCP tools involved. Persists the answer as a page with a visual feature map (hash-gated). With `repos` or `all_repos` the question is answered in each selected repo of the project (chaos.toml) and joined by cross-repo links (an excerpt's symbol name found verbatim in another repo's excerpt) on ONE page under the project's docs/features_memory. When the question names a GraphQL field (`Mutation.createLab`, or a root field next to a word such as query, mutation or resolver) or `entry` is given, the answer leads with that field: its SDL, the resolver registration, the resolver code, the calls behind it and the operations that call it. Read the returned `excerpts[].code` instead of opening files.",
            input_schema: json!({"type": "object", "properties": {
                "question": {"type": "string", "description": "What you want to understand, e.g. 'how are embeddings reused on reindex'."},
                "repo": repo_prop(),
                "repos": {"type": "array", "items": {"type": "string"}, "description": "Answer across these project repos (aliases/paths) on one page."},
                "all_repos": {"type": "boolean", "default": false, "description": "Answer across every repo of the project."},
                "project": {"type": "string", "description": "Path to chaos.toml for `repos`/`all_repos`. Default: nearest ancestor of the current directory."},
                "budget_tokens": {"type": "integer", "default": 6000, "minimum": 1000, "maximum": 16000, "description": "Across repos, the total split evenly between them (at least 1000 each)."},
                "persist": {"type": "boolean", "default": true},
                "entry": {"type": "string", "description": "GraphQL field to start from: a coordinate (Mutation.createLab) or a root field name (createLab). The answer leads with its SDL, resolver registration, resolver code, the calls behind it (up to 3 hops) and the operations and hooks that call it. A coordinate written in the question is picked up without this."}
            }, "required": ["question"]}),
        },
        ToolSpec {
            name: "compose",
            description: "Write the feature website of a repository: an index page listing every feature community and the repository's entry points (GraphQL root fields with their resolvers and callers, public contract functions and getters with their selectors and callers, MCP tools, HTTP routes, React data hooks; for text, the document plan and transcript timestamps), plus one drill-down page per feature (members, files, relations to other features, summary). With `repos` or `all_repos`: every selected project repo's site, the correlations report, and ONE project hub (<project>/docs/features_memory/index.html) — what was indexed per repo, a feature graph across repos (features as nodes; call/import weight and cross-repo links as edges), the entry points of the whole project, every feature, and links to all pages. Content-hash gated per page — the return reports written vs cached counts and entry points per category; cached pages must not be re-read.",
            input_schema: json!({"type": "object", "properties": {
                "repo": repo_prop(),
                "out": {"type": "string", "description": "Output directory (one repository only). Default: <repo>/docs/features_memory/site"},
                "limit": {"type": "integer", "description": "Max feature pages per repository (largest communities first)."},
                "repos": {"type": "array", "items": {"type": "string"}, "description": "Compose these project repos (aliases/paths) and the project hub."},
                "all_repos": {"type": "boolean", "default": false, "description": "Compose every repo of the project and the project hub."},
                "project": {"type": "string", "description": "Path to chaos.toml for `repos`/`all_repos`. Default: nearest ancestor of the current directory."}
            }, "required": []}),
        },
        ToolSpec {
            name: "correlations",
            description: "Feature correlations. Intra-repo: pairs of features sharing files or symbols. Cross-repo (project from chaos.toml): consumer→provider links by `package_dep` (one repo imports a package another publishes), `http_route` (a client call path matches a route another repo registers), `graphql_field` (a GraphQL operation outside tests calls a root field defined in the schema of the repo that owns it; the link lands on the feature of the resolver code where the index found one, and the evidence names it) and `contract_abi` (a contract call through an ABI, or a Solidity interface of another repo's contract, reaches a public function, getter or event of a Solidity contract in another repo; tier named 0.85, overlap 0.7, unique 0.6, or inherited from an unindexed base 0.6; tests left out). Writes a report page and returns a compact list with evidence and confidence.",
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

    #[test]
    fn feature_schema_has_optional_entry() {
        let feature = tools().into_iter().find(|t| t.name == "feature").unwrap();
        let entry = &feature.input_schema["properties"]["entry"];
        assert_eq!(entry["type"], "string");
        assert!(entry["description"]
            .as_str()
            .unwrap()
            .contains("Mutation.createLab"));
        assert_eq!(feature.input_schema["required"], json!(["question"]));
        assert_eq!(TOOL_NAMES.len(), 6);
    }
}
