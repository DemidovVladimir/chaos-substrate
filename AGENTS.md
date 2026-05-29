# Chaos Substrate Agent Instructions

Chaos Substrate is a Rust-only code knowledge memory for agents.

Use it to create and query a persistent knowledge base for Rust, Solidity, TypeScript, JavaScript, and Python repositories, with Markdown/MDX and PDF context. The memory is stored in Postgres + pgvector and survives process restarts.

TypeScript, JavaScript, Python, and Solidity are analysis targets only; they are extracted Rust-side and never run as a separate service.

## Hard Rules

See **Hard Rules** in `CLAUDE.md`.

## Common Commands

```sh
cargo run -- migrate
cargo run -- doctor
cargo run -- analyze /path/to/repo
cargo run -- query /path/to/repo "question"
cargo run -- feature-context /path/to/repo "task"
cargo run -- graph /path/to/repo --output graph.html
cargo run -- mcp
```

## MCP Tool Surface

Agents should prefer MCP tools when available:

- `chaos_analyze`: index or refresh a repository.
- `chaos_query`: answer focused source-grounded questions.
- `chaos_feature_context`: gather evidence for feature understanding.
- `chaos_write_feature_website`: write an LLM-composed feature page with a manifest.

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
