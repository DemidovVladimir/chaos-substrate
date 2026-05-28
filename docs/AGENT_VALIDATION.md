# Agent Validation

Chaos Substrate is a Rust-only code knowledge memory service.

## Scope Guardrails

- Do not edit `Cargo.toml` or `src/` unless the user explicitly changes scope.
- Keep implementation assumptions aligned with the MVP:
  - Rust, Solidity, TypeScript, JavaScript, Markdown/MDX, and text PDF extraction.
  - Persistent Postgres plus pgvector storage.
  - Real OpenAI or Ollama embedders only.
  - CLI, static graph export, and MCP stdio as the agent interfaces.
- Do not add mock embedders, in-memory persistence, Python services, HTTP servers, live browser services, or non-Rust runtime paths. TypeScript/JavaScript and Solidity support must be implemented in Rust.

## Validation Checklist

Run the strongest available checks before handing off:

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

If Postgres-backed tests exist, run them with a real database that has pgvector enabled. Do not replace them with fake storage.

## Functional Smoke Tests

Validate these paths when relevant:

- CLI can ingest a Rust project.
- CLI can ingest a Rust, Solidity, TypeScript, or JavaScript project with source/docs context.
- CLI can query the memory after ingestion.
- CLI can export `graph.html` for an indexed repository, and the page shows persisted nodes and edges.
- CLI can run `feature-context` for an implementation task, returning Postgres hits plus generated
  feature manifests from `docs/features_memory` without scanning unrelated docs.
- MCP server starts over stdio and responds to its declared tools.
- OpenAI embedding mode works with `OPENAI_API_KEY`.
- Ollama embedding mode works against a reachable Ollama server.
- Stored vectors persist across process restarts.

## Agent Handoff Notes

- Prefer small, targeted patches.
- Preserve existing user edits.
- Document skipped checks with the exact reason.
- Treat failing persistence, embedding, or MCP stdio behavior as release blockers.
