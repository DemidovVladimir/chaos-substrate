# Chaos Substrate Agent Instructions

Chaos Substrate is a Rust-only code knowledge memory for agents.

Use it to create and query a persistent knowledge base for Rust, Solidity, TypeScript, JavaScript, and Python repositories, with Markdown/MDX and PDF context. The memory is stored in Postgres + pgvector and survives process restarts.

TypeScript, JavaScript, Python, and Solidity are analysis targets only; they are extracted Rust-side and never run as a separate service.

## Hard Rules

- Do not add mock embedders, fake vectors, or random vectors.
- Do not replace Postgres/pgvector persistence with in-memory storage.
- Keep MCP on stdio with newline-delimited JSON-RPC.
- Keep runtime implementation in Rust.
- TypeScript/JavaScript, Python, and Solidity support must remain Rust-side extraction, not a Node or Python service.

## Common Commands

```sh
cargo run -- migrate
cargo run -- doctor
cargo run -- analyze /path/to/repo
cargo run -- add /path/to/repo -m "what changed"
cargo run -- stats /path/to/repo
cargo run -- query /path/to/repo "question"
cargo run -- feature-context /path/to/repo "task"
cargo run -- impact /path/to/repo "<feature>"
cargo run -- graph /path/to/repo --output graph.html
cargo run -- mcp
```

## MCP Tool Surface

Agents should prefer MCP tools when available:

- `chaos_analyze`: index or refresh a repository.
- `chaos_add`: incrementally index git-diff changes (or explicit `paths`), refresh the Obsidian vault, and write a feature/bug page in one call.
- `chaos_stats`: report index statistics for an already-indexed repository from Postgres — totals (files, nodes, edges, chunks, embedded vs missing, split chunks) plus breakdowns of nodes by kind, edges by kind, chunks by type, and files by language. Read-only and embedder-free; use to explain or sanity-check what an analyze/add produced.
- `chaos_query`: answer focused source-grounded questions.
- `chaos_feature_context`: gather evidence for feature understanding.
- `chaos_impact`: build a feature-vs-existing-code impact report for an indexed repo and ALWAYS write an interactive HTML (impact summary + evidence dashboard) to `docs/features_memory/<slug>-impact.html`; returns a compact JSON summary (counts plus the existing files/symbols the feature touches, warnings, and the HTML path) so it does not flood agent context, framing how the feature maps onto the codebase as it is today (the "before").
- `chaos_write_feature_website`: write an LLM-composed feature page with a manifest.
- `chaos_obsidian`: export an already-indexed repository as an Obsidian vault from the persisted graph (run after `chaos_analyze`, which never writes files).
- `chaos_refresh`: regenerate project-local artifacts (Obsidian vault and, with `all_features`, the feature pages) from the persisted index without re-indexing.

Do not synthesize feature pages from `chaos_query` alone when `chaos_feature_context` and
`chaos_write_feature_website` are available.

## Validation

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

For real repository indexing, configure either OpenAI or Ollama embeddings. If the embedder is unavailable, analysis must fail rather than producing fake vectors.

See `docs/CLAUDE_MCP_INSTALL.md` and `docs/CLAUDE_VALIDATION_BRIEF.md`.
