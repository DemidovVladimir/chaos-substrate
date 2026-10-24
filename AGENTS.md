# Chaos Substrate — Agent Guide

> Scope: these rules cover the 1.0 crate at the repository root. `v2/` is a separate crate
> (chaos-kb, a personal knowledge base) with its own rules in `v2/AGENTS.md`.

Chaos Substrate (1.0) is a Rust crate that indexes repositories (Rust, TS, JS, Python, Solidity,
GraphQL, Markdown) into a Postgres + pgvector knowledge graph so agents answer feature questions
from the index — with code excerpts — instead of re-reading source.

## Hard rules

- No mock, fake, or random embedding vectors; indexing fails when the embedder is down.
- Persistence is Postgres + pgvector only (`migrations/001_init.sql`). No in-memory storage.
- MCP is stdio, newline-delimited JSON-RPC 2.0. Runtime is Rust only; extraction is tree-sitter.
- On an indexed target repo, use chaos tools only — no `rg`/`grep`/`ls`, no ad-hoc scripts.
- Layering (tested in `src/main.rs`): `domain` -> nothing; `ports` -> domain;
  `application` -> domain + ports; `adapters` implement ports; `entry` wires everything.

## Six tools (CLI `chaos <name>` / MCP `chaos_<name>`; schemas in `src/application/tools.rs`)

| Tool | What it does |
|---|---|
| `index` | Full index: extraction -> import/call resolution -> chunks -> embeddings -> communities -> Merkle hashes -> summaries. Once per repo. |
| `reindex [--force]` | Full re-run; no-op when HEAD is indexed and the worktree is clean. Embeddings reused by content hash. |
| `add [--since <ref>]` | Index only git-changed files; purge deleted ones. Use after every edit. |
| `feature "<q>" [--budget N] [--no-persist] [--json]` | Answer one question from the index: feature summaries, excerpts with line ranges, 1-hop graph, env vars/routes. Writes `docs/features_memory/<slug>.html`. |
| `compose [--out <dir>] [--limit N]` | Feature website (`docs/features_memory/site/`): index + one page per feature, hash-gated. |
| `correlations [--repos ...] [--min-confidence 0.5]` | Features sharing files/symbols; cross-repo `package_dep` / `http_route` links across a `chaos.toml` project. |

CLI-only: `migrate [--reset]`, `doctor`, `mcp`, `help`. Global flags: `--config`, `--project`, `--repo`, `-v`.

Terms: *chunk* = one AST unit plus context header, embedded; *community* = feature-sized node group
from deterministic Louvain; *Merkle hash* = content hashes rolled up chunk -> file -> community -> repo.

## Config and workflow

`chaos-substrate.toml` (`[storage] database_url`; `[embedding] provider = "ollama"|"open_ai"`, `model`,
`dimensions`, `base_url`) or env `DATABASE_URL`, `CHAOS_EMBED_PROVIDER|MODEL|DIMENSIONS|BASE_URL`.
A project is a `chaos.toml` next to the repos: `name` + `[[repos]]` with `alias` and `path`.

`doctor` -> `migrate` -> `index` once -> `add` after edits -> `feature` to answer (read the
returned excerpts, not files) -> `compose` -> `correlations`. Cached pages: do not re-read.
Validate: `cargo fmt --check && cargo test && cargo clippy --all-targets -- -D warnings`.
