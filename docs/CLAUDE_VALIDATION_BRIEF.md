# Claude Validation Brief

Use this document to validate Chaos Substrate from product intent through implementation quality.

## Product Goal

Chaos Substrate is a Rust-only code knowledge memory for agents. It analyzes repositories, extracts source-grounded knowledge, persists graph/chunk/embedding data to disk-backed Postgres + pgvector, exposes query access through a CLI and stdio MCP server, exports a standalone `graph.html` page for visual graph validation, and can generate focused feature-memory websites with agent-readable manifests.

The system must survive laptop/process restarts. Indexed knowledge must be reusable from persisted storage without relying on in-memory state.

## Hard Requirements

- Runtime implementation must be Rust-only.
- No mock embedders, fake vectors, random vectors, or placeholder semantic search.
- Embeddings must come from a real provider: OpenAI or Ollama.
- Knowledge graph, chunks, files, nodes, edges, analysis runs, and embeddings must persist in Postgres.
- Vector dimensions and embedding model metadata must be stored and validated.
- Query flow should combine semantic search, keyword search, and graph context routing.
- CLI graph export should render persisted nodes and edges in a standalone HTML file without introducing an HTTP server.
- CLI feature-context should combine live Postgres retrieval with generic generated feature
  manifests from `docs/features_memory` without scanning unrelated docs.
- MCP must use stdio, not an HTTP wrapper.
- MCP stdio messages must be newline-delimited JSON-RPC messages, not `Content-Length` framed LSP messages.
- Ollama embeddings must use `/api/embed` with `{model, input}` and read the first vector from `embeddings`.

## Current Scope

Implemented language/project support:

- Rust (`syn`)
- Solidity (`solang-parser` AST): contracts, interfaces, libraries, functions, constructors, events, modifiers, imports, and inheritance
- Python (`rustpython-parser` AST): functions, classes, class methods, imports, and file-scoped call edges
- TypeScript (`oxc` AST)
- JavaScript (`oxc` AST)
- `Cargo.toml`
- `package.json`
- `tsconfig.json`
- `jsconfig.json`

Supported extracted knowledge:

- source files
- functions
- Rust structs/enums/traits/impls/modules/tests
- Solidity contracts/interfaces/libraries/functions/events/modifiers/imports/inheritance
- TypeScript/JavaScript functions, arrow functions, classes, class methods, interfaces, enums, type aliases
- Python functions, classes, class methods, and imports
- Markdown/MDX and extractable PDF documentation as supplemental context
- imports/re-exports/CommonJS `require`
- Cargo dependencies
- npm dependencies and scripts
- source line ranges where available
- graph edges for contains/imports/depends-on/calls/defines
- symbol-aware chunks linked to graph nodes

## Main Files To Review

- `README.md` - usage and project summary
- `docs/GRAPH_WEBPAGE.md` - graph export tutorial
- `docs/REFRESH_EXPORTS.md` - generated feature memory and Obsidian refresh tutorial
- `docs/FEATURE_CONTEXT.md` - focused implementation context tutorial
- `docs/CLAUDE_CODE_COWORK.md` - Claude Code / Cowork MCP setup
- `Cargo.toml` - Rust package and dependencies
- `src/main.rs` - CLI entrypoint
- `src/models.rs` - persisted data model
- `src/extractor.rs` - extraction orchestration plus Rust/Cargo/Markdown/PDF/JSON/AWS-CDK extraction and call edges
- `src/lang/javascript.rs` - TypeScript/JavaScript AST extraction (`oxc`)
- `src/lang/python.rs` - Python AST extraction (`rustpython-parser`)
- `src/lang/solidity.rs` - Solidity AST extraction (`solang-parser`)
- `src/weights.rs` - edge cost/confidence weighting
- `src/storage.rs` - Postgres persistence
- `src/embedding.rs` - OpenAI/Ollama real embedders
- `src/query.rs` - hybrid query flow
- `src/graph.rs` - context path selection
- `src/graph_export.rs` - standalone graph webpage export
- `src/simple_graph_optimizer.rs` - weighted graph path engine
- `src/setup.rs` - editor MCP auto-registration (`chaos setup`)
- `src/hook.rs` - Claude Code/Cursor plugin hook (`chaos hook`)
- `src/mcp.rs` - stdio MCP server
- `migrations/001_init.sql` - database schema
- `docker-compose.yml` - local pgvector Postgres
- `.codex-plugin/plugin.json` - Codex plugin metadata
- `.claude-plugin/plugin.json` - Claude Code plugin metadata
- `.mcp.json` - shared plugin MCP server definition
- `bin/chaos-agent` - plugin-safe wrapper entrypoint
- `skills/chaos-substrate/SKILL.md` - agent guidance

## Validation Commands

From project root:

```sh
cd /absolute/path/to/chaos-substrate
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Database validation:

```sh
docker compose up -d
cargo run -- --config chaos-substrate.local.toml migrate
cargo run -- --config chaos-substrate.local.toml doctor
```

CLI help validation:

```sh
cargo run -- --help
cargo run -- analyze --help
cargo run -- query --help
cargo run -- feature-context --help
cargo run -- graph --help
cargo run -- mcp --help
```

Real embedding validation requires one of:

```sh
export OPENAI_API_KEY=...
cargo run -- analyze /path/to/repo
```

or an available Ollama endpoint matching `chaos-substrate.local.toml`.

Do not replace this with fake embeddings.

## Functional Smoke Test

Use a small real repository containing Rust, Solidity, TypeScript, JavaScript, or Python.

```sh
docker compose up -d
cargo run -- --config chaos-substrate.local.toml migrate
cargo run -- --config chaos-substrate.local.toml analyze /path/to/repo
cargo run -- --config chaos-substrate.local.toml query /path/to/repo "where is request validation handled?"
cargo run -- --config chaos-substrate.local.toml feature-context /path/to/repo "implement related feature"
cargo run -- --config chaos-substrate.local.toml graph /path/to/repo --output graph.html
cargo run -- --config chaos-substrate.local.toml refresh /path/to/repo
```

Expected behavior:

- `analyze` persists repository, files, nodes, edges, chunks, and embeddings.
- `query` returns relevant chunks with file paths and line ranges.
- `feature-context` returns Postgres hits plus any matching generated feature manifests from
  `docs/features_memory`.
- `graph` writes a standalone interactive webpage that shows persisted nodes and edges.
- `refresh` writes an Obsidian vault from persisted graph data.
- `feature-context --output-html` writes generated feature-memory pages with
  `chaos-feature-manifest` JSON. Manifests include `schema_version`, `feature`, `claims`, `modes`,
  `nodes`, `edges`, story-step scopes, evidence, and confidence.
- Re-running `query` after restarting the process still uses persisted data.
- If the embedder is unavailable, analyze/query should fail rather than produce fake vectors.

## Persistence Checks

After migration and analysis, inspect Postgres:

```sql
select count(*) from repositories;
select count(*) from files;
select count(*) from nodes;
select count(*) from edges;
select count(*) from chunks;
select provider, model_id, dimensions, count(*) from embeddings group by provider, model_id, dimensions;
```

Validate:

- `embeddings.dimensions = vector_dims(embedding)` is enforced.
- `provider`, `model_id`, `dimensions`, and `content_hash` are stored.
- Deleting the process does not delete indexed memory.

## Review Focus

Check for these risks:

- Any mock/fake embedding path in runtime code.
- Any storage path that keeps primary knowledge only in memory.
- MCP stdout pollution that could break stdio protocol.
- LSP-style `Content-Length` framing in MCP stdio code.
- Ollama `/api/embeddings` legacy usage with the wrong `input` field.
- TS/JS extractor overclaiming compiler-level precision.
- Missing source grounding for chunks or nodes.
- Schema drift between `models.rs`, `storage.rs`, and `migrations/001_init.sql`.
- Query path using embeddings without validating provider/model/dimensions.
- AST parse failures (oxc/rustpython-parser/solang-parser) not degrading gracefully, e.g. aborting a whole run instead of skipping the unparseable file.
- Incorrect AST span-to-line mapping (`LineIndex` offsets) producing wrong source line ranges for nodes.

## PRD Summary

Chaos Substrate should become a modular code-to-knowledge memory system:

1. Scan a local repository.
2. Detect supported languages and project metadata.
3. Extract source-grounded graph nodes and edges.
4. Generate symbol-aware chunks from graph nodes.
5. Embed chunks with a real embedding provider.
6. Persist everything in Postgres + pgvector.
7. Query using semantic search, keyword search, and graph context routing.
8. Export a standalone graph webpage for human validation of the persisted graph.
9. Refresh Obsidian vaults from persisted knowledge.
10. Build focused implementation context and feature-memory websites from Postgres retrieval plus
    generated feature manifests.
11. Expose access through CLI and MCP for coding agents.
12. Keep architecture modular for future Go, Kubernetes, Terraform, and framework-specific extractors.

## Current Known Limits

- TypeScript/JavaScript extraction uses the `oxc` AST parser, but there is no `tsc`-level type inference or path-alias resolution.
- Python extraction uses the `rustpython-parser` AST; it captures structure and call sites but does not perform type inference.
- Solidity extraction uses the `solang-parser` AST; it is not a full compiler frontend with semantic analysis.
- Rust extraction uses `syn` plus heuristics; it is not rust-analyzer/MIR-level semantic analysis.
- Call edges are file-scoped heuristics; cross-file call resolution is name-based, not type-resolved.
- No Go/Kubernetes/Terraform adapter yet.
- No full integration test with a real embedder was run unless the validator provides OpenAI or Ollama.
- MCP has a focused tool surface: `chaos_analyze`, `chaos_query`, `chaos_feature_context`, and
  `chaos_write_feature_website`.

## Pass Criteria

Consider the current implementation valid if:

- Formatting, tests, and Clippy pass.
- Migration succeeds against real Postgres + pgvector.
- `doctor` reports pgvector and configured real embedder metadata.
- Runtime code has no fake embedding implementation.
- Analysis fails cleanly when real embedding provider is unavailable.
- `graph.html` can be exported for an indexed repository and opened directly in a browser.
- `feature-context` can return implementation context without reading arbitrary files from `docs/`.
- A real analyzed repo can be queried after process restart.
- Docs accurately describe current limits.
