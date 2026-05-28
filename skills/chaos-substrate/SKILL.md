---
name: chaos-substrate
description: Use when installing, initializing, updating, querying, or operating Chaos Substrate in any Rust, TypeScript, or JavaScript repository; includes Postgres+pgvector persistence, real OpenAI/Ollama embedders, CLI, MCP stdio, generated feature-memory websites, and agent implementation context.
---

# Chaos Substrate

Use this skill when working on or operating Chaos Substrate, a Rust-only code knowledge memory for agents.

## Product Shape

- Rust, TypeScript, JavaScript, Markdown, and MDX extraction.
- Persistent Postgres plus pgvector memory.
- Real OpenAI and Ollama embedders only.
- Agent surfaces are Codex plugin, Claude Code plugin, CLI, the `chaos-agent` wrapper, static
  graph/Obsidian exports, generated feature-context websites, feature-memory manifests, and MCP over
  stdio.
- Source code is prioritized over docs during code-repository queries. Markdown/MDX docs are indexed
  as supplemental context, not ignored.

## Hard Boundaries

- Do not edit `Cargo.toml` or `src/` unless the user explicitly asks.
- Do not add mock embeddings, fake vector stores, in-memory persistence, HTTP APIs, Python services, TypeScript services, or live browser services. A standalone Rust-generated `graph.html` export is allowed for persisted graph validation. TypeScript/JavaScript project support belongs in the Rust extractor.
- Do not downgrade persistence guarantees; memory must survive process restarts.
- Do not replace real embedders with deterministic test-only behavior in production paths.

## Working Pattern

When a user asks to use Chaos Substrate in a target project, prefer MCP tools over shell commands.
Use the `chaos-agent` wrapper for setup, debugging, or when MCP is unavailable.

If MCP tools are available, prefer them over shelling out:

1. Use `chaos_analyze` to index or refresh a repository.
2. Use `chaos_query` for focused questions.
3. Use `chaos_feature_context` when the user asks to explain a feature, prepare implementation
   context, or generate a feature explanation.
4. Use `chaos_write_feature_website` only after reading `chaos_feature_context` output and composing
   a feature-specific website plus manifest. The LLM must decide the feature story, claims, nodes,
   and flow from evidence; the tool only writes the artifact.

Do not tell the user "I only have chaos_query" if `chaos_feature_context` or
`chaos_write_feature_website` is available. If the MCP server cannot write the website because of
filesystem permissions, still return the feature context from MCP and say exactly that HTML
generation was blocked by filesystem access.

Never assume the current working directory is the Chaos Substrate checkout. The agent is usually
standing in the target project. Resolve the wrapper with this order:

1. If `chaos-agent` is on `PATH`, use `chaos-agent`.
2. Else if `CHAOS_SUBSTRATE_HOME` is set, use `$CHAOS_SUBSTRATE_HOME/bin/chaos-agent`.
3. Else if the user gave an absolute Chaos Substrate checkout path, use
   `/absolute/path/to/chaos-substrate/bin/chaos-agent`.
4. Else ask for the absolute Chaos Substrate checkout path.

In command examples, call this resolved wrapper as `$CHAOS_AGENT`. If `chaos-agent` is already on
`PATH`, set `CHAOS_AGENT=chaos-agent`. Always pass the target project as an absolute path or as
`"$PWD"` after confirming the shell is in the target project.

Natural language mapping:

- "Set up Chaos Substrate here" or "make this project use Chaos" -> MCP `chaos_analyze`; otherwise `$CHAOS_AGENT onboard <repo-path>`
- "Go through this project and create sufficient index and explanation" -> MCP `chaos_analyze`, then `chaos_feature_context`, then compose and write with `chaos_write_feature_website`; otherwise `$CHAOS_AGENT onboard <repo-path>` plus `$CHAOS_AGENT explain <repo-path> "feature"`
- "Update index" or "refresh memory" -> MCP `chaos_analyze`; otherwise `$CHAOS_AGENT update <repo-path>`
- "Add Chaos instructions to this project" -> `$CHAOS_AGENT project-instructions <repo-path>` because this edits instruction files.
- "What context do I need for X?" -> MCP `chaos_feature_context` when available; otherwise `$CHAOS_AGENT context <repo-path> "X"`
- "Generate explanation for X feature" -> MCP `chaos_feature_context`, then compose the website and call `chaos_write_feature_website`; otherwise `$CHAOS_AGENT explain <repo-path> "X"`
- "Add this to Claude Code" or "use with Claude Cowork" -> `$CHAOS_AGENT claude-code-add local <project-path>` or `$CHAOS_AGENT claude-code-add project <project-path>`. The path is the target Claude Code project where config should be applied.
- "Use Ollama" or "set up local embeddings" -> run with `CHAOS_CONFIG=/absolute/path/to/chaos-substrate/chaos-substrate.local.toml`; `$CHAOS_AGENT bootstrap`, `doctor`, `onboard`, `init`, and `update` enforce Ollama readiness.
- "Run MCP" -> `$CHAOS_AGENT mcp`

The wrapper can install itself onto `PATH` with `bootstrap` or `install-agent`. It will build the
release binary if missing, start Docker Compose unless `CHAOS_NO_DOCKER=1` is set for `bootstrap`,
`doctor`, `onboard`, `init`, and `update`, run migrations, analyze the project, regenerate the
Obsidian vault, and generate dark feature-context websites when requested. `onboard` also writes
portable `AGENTS.md` and `CLAUDE.md` sections into the target project. `refresh` regenerates the
Obsidian vault from Postgres. In plugin/MCP mode, feature explanation websites must be composed by
the LLM from `chaos_feature_context` evidence and written with `chaos_write_feature_website`.

`context`, `explain`, `mcp`, and `claude-code-add` assume Postgres and the selected embedder are
already available. If they are not, run `$CHAOS_AGENT doctor` or `$CHAOS_AGENT init <repo-path>`
first. In sandboxed agents such as Claude Cowork, prefer the MCP tools exposed by the host-side MCP
server instead of trying to reach host Postgres directly from the sandbox.

For code changes inside Chaos Substrate itself:

1. Inspect existing docs and CLI help before changing behavior.
2. Keep patches narrow and Rust-native.
3. Prefer existing crate patterns and error types.
4. Validate CLI, persistence, embedding, generated websites, and MCP stdio paths when touched.
5. Report any skipped checks with concrete reasons.

## End-User Commands

Plugin package layout:

```text
.codex-plugin/plugin.json
.claude-plugin/plugin.json
.mcp.json
bin/chaos-agent
skills/chaos-substrate/SKILL.md
```

Portable setup from any target project:

```sh
export CHAOS_SUBSTRATE_HOME=/absolute/path/to/chaos-substrate
export CHAOS_AGENT="$CHAOS_SUBSTRATE_HOME/bin/chaos-agent"

"$CHAOS_AGENT" bootstrap
"$CHAOS_AGENT" onboard "$PWD"
"$CHAOS_AGENT" project-instructions "$PWD"
"$CHAOS_AGENT" doctor
"$CHAOS_AGENT" ollama-setup
"$CHAOS_AGENT" init "$PWD"
"$CHAOS_AGENT" update "$PWD"
"$CHAOS_AGENT" context "$PWD" "authorization and RBAC"
"$CHAOS_AGENT" explain "$PWD" "authorization and RBAC"
"$CHAOS_AGENT" claude-code-add local "$PWD"
"$CHAOS_AGENT" claude-code-add project "$PWD"
"$CHAOS_AGENT" mcp
```

After `bootstrap`, future projects can usually use the shorter form:

```sh
chaos-agent onboard "$PWD"
chaos-agent context "$PWD" "authorization and RBAC"
chaos-agent explain "$PWD" "authorization and RBAC"
```

If `chaos-agent` is not found immediately after bootstrap, use:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

For Ollama:

```sh
export CHAOS_SUBSTRATE_HOME=/absolute/path/to/chaos-substrate
export CHAOS_AGENT="$CHAOS_SUBSTRATE_HOME/bin/chaos-agent"

CHAOS_CONFIG="$CHAOS_SUBSTRATE_HOME/chaos-substrate.local.toml" "$CHAOS_AGENT" init "$PWD"
```

`bootstrap`, `doctor`, `onboard`, `init`, and `update` call the Ollama readiness check
automatically when the active config uses Ollama.

If Ollama is missing, tell the user to install it from `https://ollama.com/download`. On Linux the
official install command is usually `curl -fsSL https://ollama.com/install.sh | sh`. The default
local embedding model is `nomic-embed-text` with `dimensions = 768`.

Generated artifacts live in the target project:

```text
chaos-obsidian-vault
docs/features_memory
```

Do not scan the whole `docs/` tree for generated feature memory. Use `feature-context` or read only
direct HTML files in `docs/features_memory` with `chaos-feature-manifest`.

Use `graph` / `graph.html` for full indexed-graph validation and Obsidian for broad topic/node
exploration. Use `chaos_feature_context` plus `chaos_write_feature_website` for focused
implementation briefs and human-readable feature pages when MCP is available.

When creating or updating feature-memory websites, make story steps explicit. Each step should have
curated `node_ids` and, when useful, `edge_ids` in the manifest. Do not derive a story-step
highlight by transitive graph expansion from the first node. A step should highlight only the
subflow it explains; broader architecture or security-boundary highlights belong in `modes`.

## Validation Commands

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Use a real Postgres database with pgvector for persistence tests. Use real OpenAI or Ollama credentials/endpoints for embedding smoke tests.

## MCP Expectations

- MCP transport is stdio.
- The process should be launched directly by the agent client.
- Keep stdout protocol-clean; diagnostics should go to stderr or structured logging that does not corrupt MCP messages.
- MCP tools are `chaos_analyze`, `chaos_query`, `chaos_feature_context`, and
  `chaos_write_feature_website`.
- `chaos_feature_context` is the MCP equivalent of `chaos-agent context`.
- `chaos_write_feature_website` is the MCP-safe write path for LLM-composed feature explanation
  pages with embedded `chaos-feature-manifest` JSON.
