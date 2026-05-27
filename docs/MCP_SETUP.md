# MCP Setup

Chaos Substrate exposes an MCP server over stdio for Claude, Codex, and other MCP-capable agents.

## Prerequisites

- Rust toolchain.
- Postgres with pgvector enabled.
- One real embedding backend:
  - OpenAI with `OPENAI_API_KEY`.
  - Ollama with a reachable local or remote Ollama endpoint.

## Required Environment

Set the database URL used by the binary:

```sh
export DATABASE_URL="postgres://USER:PASSWORD@HOST:PORT/DB"
```

For OpenAI embeddings:

```sh
export OPENAI_API_KEY="..."
```

For Ollama embeddings, use `chaos-substrate.local.toml` or set equivalent environment variables.
See `docs/OLLAMA_SETUP.md`.

## Build

```sh
cargo build
```

## Agent Configuration

Configure the MCP client to launch the Chaos Substrate binary over stdio.
The server uses MCP stdio newline-delimited JSON-RPC. Do not wrap it with LSP-style `Content-Length` framing.

Example shape:

```json
{
  "mcpServers": {
    "chaos-substrate": {
      "command": "cargo",
      "args": ["run", "--", "mcp"],
      "cwd": "/absolute/path/to/chaos-substrate",
      "env": {
        "DATABASE_URL": "postgres://USER:PASSWORD@HOST:PORT/DB",
        "OPENAI_API_KEY": "..."
      }
    }
  }
}
```

Use the exact subcommand exposed by the current CLI if it differs from `mcp`.

## Validation

```sh
cargo test
cargo run -- mcp
```

The MCP process should remain attached to stdio. Do not wrap it in an HTTP server.

## Graph Export

MCP remains stdio-only, but indexed repositories can be inspected visually with a static HTML export:

```sh
cargo run -- graph /absolute/path/to/repo --output graph.html
```

The export reads persisted nodes and edges from Postgres and writes a standalone webpage. It is not
an MCP transport, HTTP API, or long-running browser service.

## Feature Context

For CLI-driven agent work, use `feature-context` before implementing related subfeatures:

```sh
cargo run -- feature-context /absolute/path/to/repo "implement secure upload icon"
```

This reads Postgres retrieval results and generated feature manifests from
`/absolute/path/to/repo/docs/features_memory`. It scans only direct HTML files in that directory and
ignores pages without `chaos-feature-manifest`.
