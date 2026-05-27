# Chaos Substrate Agent Instructions

Chaos Substrate is a Rust-only code knowledge memory for agents.

Use it to create and query a persistent knowledge base for Rust, TypeScript, and JavaScript repositories. The memory is stored in Postgres + pgvector and survives process restarts.

## Hard Rules

- Do not add mock embedders, fake vectors, or random vectors.
- Do not replace Postgres/pgvector persistence with in-memory storage.
- Keep MCP on stdio with newline-delimited JSON-RPC.
- Keep runtime implementation in Rust.
- TypeScript/JavaScript support must remain Rust-side extraction, not a Node service.

## Common Commands

```sh
cargo run -- migrate
cargo run -- doctor
cargo run -- analyze /path/to/repo
cargo run -- query /path/to/repo "question"
cargo run -- graph /path/to/repo --output graph.html
cargo run -- mcp
```

## Validation

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

For real repository indexing, configure either OpenAI or Ollama embeddings. If the embedder is unavailable, analysis must fail rather than producing fake vectors.

See `docs/CLAUDE_MCP_INSTALL.md` and `docs/CLAUDE_VALIDATION_BRIEF.md`.

## Imported Claude Cowork project instructions
