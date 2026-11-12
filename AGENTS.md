# Chaos Substrate — Agent Guide

> Scope: these rules cover the 1.0 crate at the repository root. `v2/` is a separate crate
> (chaos-kb, a personal knowledge base) with its own rules in `v2/AGENTS.md`.

Chaos Substrate (1.0) is a Rust crate that indexes repositories (Rust, TS, JS, Python, Solidity,
contract ABIs, GraphQL, Markdown, `.srt`/`.vtt` transcripts) into a Postgres + pgvector knowledge graph so agents answer feature questions
from the index — with code excerpts — instead of re-reading source.

## Hard rules

- No mock, fake, or random embedding vectors; indexing fails when the embedder is down.
- Persistence is Postgres + pgvector only (`migrations/001_init.sql`). No in-memory storage.
- MCP is stdio, newline-delimited JSON-RPC 2.0. Runtime is Rust only; extraction is tree-sitter
  (ABI JSON via tree-sitter-json; Markdown via pulldown-cmark, transcripts via a line scanner).
- On an indexed target repo, use chaos tools only — no `rg`/`grep`/`ls`, no ad-hoc scripts.
- Layering (tested in `src/main.rs`): `domain` -> nothing; `ports` -> domain;
  `application` -> domain + ports; `adapters` implement ports; `entry` wires everything.

## Six tools (CLI `chaos <name>` / MCP `chaos_<name>`; schemas in `src/application/tools.rs`)

| Tool | What it does |
|---|---|
| `index` | Full index: extraction -> import/call resolution -> chunks -> embeddings -> communities -> summaries. Once per repo. |
| `reindex [--force]` | The `add` delta plus the files one hop upstream of each change; no-op when HEAD is the stamp and nothing is uncommitted; `--force` = full re-run. Embeddings reused by content hash. |
| `add [--since <ref>]` | Index the git delta since the last stamped run (stamped commit → HEAD, uncommitted now, uncommitted at the stamp); purge removed files. Use after every edit, commit, pull or checkout. |
| `feature "<q>" [--budget N] [--no-persist] [--json] [--entry <field>]` | Answer one question from the index: feature summaries, excerpts with line ranges, 1-hop graph, env vars/routes/MCP tools. A GraphQL field in the question (`Mutation.createLab`) or `--entry` leads the answer with its SDL, registration, resolver, calls and callers. Writes `docs/features_memory/<slug>.html`. |
| `compose [--out <dir>] [--limit N] [--repos a b \| --all-repos]` | Feature website (`docs/features_memory/site/`): index with the repo's entry points (GraphQL fields + resolvers, contract functions + getters with selectors and callers, MCP tools, HTTP routes, React hooks, document plan, transcript timestamps) + one page per feature, hash-gated; across repos also one project hub. |
| `correlations [--repos ...] [--min-confidence 0.5]` | Features sharing files/symbols; cross-repo `package_dep` / `http_route` / `graphql_field` / `contract_abi` links across a `chaos.toml` project. |

CLI-only: `migrate [--reset]`, `doctor`, `mcp`, `help`. Global flags: `--config`, `--project`, `--repo`, `-v`.

Terms: *chunk* = one AST unit plus context header, embedded; *community* = feature-sized node group
from deterministic Louvain; *content hash* = SHA-256 of a piece's own input (chunk, file, summary
text, page), so an unchanged piece is recognised and reused.

## Config and workflow

`chaos-substrate.toml` (`[storage] database_url`; `[embedding] provider = "ollama"|"open_ai"`, `model`,
`dimensions`, `base_url`; optional `[indexing] skip_dirs`, Foundry `libs` skipped by path anyway) or env
`DATABASE_URL`, `CHAOS_EMBED_PROVIDER|MODEL|DIMENSIONS|BASE_URL`.
A project is a `chaos.toml` next to the repos: `name` + `[[repos]]` with `alias` and `path`.

`doctor` -> `migrate` -> `index` once -> `add` after edits -> `feature` to answer (read the
returned excerpts, not files) -> `compose` -> `correlations`. Cached pages: do not re-read.
After an upgrade that changes extraction, `reindex --force` every repo (the delta skips unchanged files).
Bump `INDEXABLE_RULES_VERSION` (`src/domain/model.rs`) with any change to which paths are indexable.
Validate: `cargo fmt --check && cargo test && cargo clippy --all-targets -- -D warnings`.
