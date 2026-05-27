---
name: chaos-substrate
description: Use when installing, initializing, updating, querying, or operating Chaos Substrate in any Rust, TypeScript, or JavaScript repository; includes Postgres+pgvector persistence, real OpenAI/Ollama embedders, CLI, MCP stdio, generated feature-memory websites, and agent implementation context.
---

# Chaos Substrate

Use this skill when working on or operating Chaos Substrate, a Rust-only code knowledge memory for agents.

## Product Shape

- Rust, TypeScript, and JavaScript extraction.
- Persistent Postgres plus pgvector memory.
- Real OpenAI and Ollama embedders only.
- Agent surfaces are CLI, `scripts/chaos-agent`, static generated websites, feature-memory manifests,
  and MCP over stdio.

## Hard Boundaries

- Do not edit `Cargo.toml` or `src/` unless the user explicitly asks.
- Do not add mock embeddings, fake vector stores, in-memory persistence, HTTP APIs, Python services, TypeScript services, or live browser services. A standalone Rust-generated `graph.html` export is allowed for persisted graph validation. TypeScript/JavaScript project support belongs in the Rust extractor.
- Do not downgrade persistence guarantees; memory must survive process restarts.
- Do not replace real embedders with deterministic test-only behavior in production paths.

## Working Pattern

When a user asks to use Chaos Substrate in a target project, prefer `scripts/chaos-agent` over asking
the user to run raw commands.

Natural language mapping:

- "Go through this project and create sufficient index and explanation" -> `scripts/chaos-agent init <repo-path>`
- "Update index" or "refresh memory" -> `scripts/chaos-agent update <repo-path>`
- "What context do I need for X?" -> `scripts/chaos-agent context <repo-path> "X"`
- "Generate explanation for X feature" -> `scripts/chaos-agent explain <repo-path> "X"`
- "Use Ollama" or "set up local embeddings" -> `scripts/chaos-agent ollama-setup`, then rerun with `CHAOS_CONFIG=chaos-substrate.local.toml`
- "Run MCP" -> `scripts/chaos-agent mcp`

The wrapper will build the release binary if missing, start Docker Compose unless `CHAOS_NO_DOCKER=1`
is set, run migrations, analyze the project, refresh `docs/features_memory`, and generate dark
feature-context websites when requested.

For code changes inside Chaos Substrate itself:

1. Inspect existing docs and CLI help before changing behavior.
2. Keep patches narrow and Rust-native.
3. Prefer existing crate patterns and error types.
4. Validate CLI, persistence, embedding, generated websites, and MCP stdio paths when touched.
5. Report any skipped checks with concrete reasons.

## End-User Commands

From the Chaos Substrate plugin/repo root:

```sh
scripts/chaos-agent doctor
scripts/chaos-agent ollama-setup
scripts/chaos-agent init /absolute/path/to/project
scripts/chaos-agent update /absolute/path/to/project
scripts/chaos-agent context /absolute/path/to/project "authorization and RBAC"
scripts/chaos-agent explain /absolute/path/to/project "authorization and RBAC"
scripts/chaos-agent mcp
```

For Ollama:

```sh
scripts/chaos-agent ollama-setup
CHAOS_CONFIG=chaos-substrate.local.toml scripts/chaos-agent init /absolute/path/to/project
```

If Ollama is missing, tell the user to install it from `https://ollama.com/download`. On Linux the
official install command is usually `curl -fsSL https://ollama.com/install.sh | sh`. The default
local embedding model is `nomic-embed-text` with `dimensions = 768`.

Generated feature memory lives in the target project:

```text
docs/features_memory
```

Do not scan the whole `docs/` tree for generated feature memory. Use `feature-context` or read only
direct HTML files in `docs/features_memory` with `chaos-feature-manifest`.

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
