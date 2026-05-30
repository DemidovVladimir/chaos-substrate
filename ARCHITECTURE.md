# Architecture

Chaos Substrate is a **single Rust binary** (`chaos`) backed by **Postgres + pgvector**. It is a
portable, persistent code-knowledge *memory* for AI agents: it walks a repository, extracts a
typed knowledge graph (nodes, edges, chunks), embeds the chunks with a real embedder, and persists
everything to Postgres so the memory survives process restarts. Agents and humans read that memory
back through two surfaces over the same code path: a **CLI** (`src/main.rs`, clap) and a **stdio
MCP server** (`src/mcp.rs`, newline-delimited JSON-RPC, no `Content-Length` framing). Diagnostics
go to stderr; program results go to stdout.

## End-to-End Data Flow

```
repo files
  │
  ▼
walk  ────────────────  src/extractor.rs (ignore::WalkBuilder, respects .gitignore;
  │                      skips indexing.skip_dirs by directory name)
  ▼
per-language extraction
  ├─ Rust            ──  src/extractor.rs            (syn)
  ├─ JS / TS         ──  src/lang/javascript.rs      (oxc)
  ├─ Python          ──  src/lang/python.rs          (rustpython-parser)
  ├─ Solidity        ──  src/lang/solidity.rs        (solang-parser)
  └─ Markdown / PDF / JSON / AWS-CDK ── src/extractor.rs (supplemental / config context)
  │
  ▼
KnowledgeNode / KnowledgeEdge / KnowledgeChunk  ──  src/models.rs
  │   (Language, NodeKind, EdgeKind enums; edges carry cost + confidence)
  ▼
edge cost / confidence weighting  ──  src/weights.rs
  │
  ▼
embeddings (chunk content → vectors)  ──  src/embedding.rs
  │   OpenAI text-embedding-3-small (1536d) OR Ollama nomic-embed-text (768d)
  │   FAIL-CLOSED: no real embedder ⇒ analysis fails (never fake/random vectors)
  ▼
Postgres + pgvector  ──  src/storage.rs (sqlx)
      tables: repositories, analysis_runs, files, nodes, edges, chunks, embeddings
  │
  ▼
hybrid retrieval  ──  src/query.rs
      semantic (vector) + keyword + graph routing over the persisted index
  │
  ▼
outputs
  ├─ CLI results (JSON on stdout)              ──  src/main.rs
  ├─ MCP tools (4)                             ──  src/mcp.rs
  ├─ interactive graph.html                    ──  src/graph_export.rs
  ├─ Obsidian vault                            ──  src/obsidian_export.rs
  └─ feature context + feature websites        ──  src/feature_context.rs, src/feature_export.rs
```

### Extraction in detail

`RustRepositoryExtractor::extract` (in `src/extractor.rs`) is the orchestrator. It walks the repo
with `ignore::WalkBuilder` (honoring `.gitignore` and skipping any directory whose name is in
`indexing.skip_dirs`), classifies each file to a `Language` by extension, and dispatches to the
right extractor. All non-Rust extraction uses **real AST parsers** — never regex/pattern matching:

- **Rust** — `syn`, inline in `extractor.rs`.
- **TypeScript / JavaScript** — `oxc`, in `src/lang/javascript.rs`.
- **Python** — `rustpython-parser`, in `src/lang/python.rs`.
- **Solidity** — `solang-parser`, in `src/lang/solidity.rs`.

The language extractors in `src/lang/*` share a `LineIndex` and produce a `FileExtraction`.
Captured symbols include functions, classes/structs, interfaces/traits, enums, type aliases, class
methods, imports, inheritance, and (heuristic, file-scoped) call edges. Markdown/MDX and text PDF
are supplemental document context (lower weight). JSON is limited to config manifests
(`package.json`, `cdk.json`, `tsconfig.json`, `jsconfig.json`); `Cargo.toml` is parsed for
dependency nodes, and AWS-CDK constructs become deployment-resource nodes — all in `extractor.rs`.

Honest residual limits: no `tsc`/type inference, no path-alias resolution, and cross-file call
resolution is name-based.

### Persistence and runs

Each analysis is bracketed by an `analysis_runs` row (`begin_analysis` → `finish_analysis` with
`completed`/`failed`). `storage.replace_repo_index` swaps in the freshly extracted nodes/edges/
chunks for the repo, then chunks **missing embeddings** for the active provider/model/dimensions
are embedded with bounded concurrency (`EMBED_CONCURRENCY = 8`) and written to the `embeddings`
table. Migrations run via `sqlx::migrate!` and are tracked in `_sqlx_migrations`.

## Where to Change What

| If you want to…                                  | Edit                                                              |
| ------------------------------------------------ | ---------------------------------------------------------------- |
| Add / change a **language**                      | add `src/lang/<lang>.rs`, wire dispatch + extension mapping in `src/extractor.rs`, add the variant to `Language` in `src/models.rs` |
| Change **edge weights** (cost / confidence)      | `src/weights.rs`                                                  |
| Add / change a **CLI command**                   | `src/main.rs` (clap `Commands`)                                  |
| Add / change an **MCP tool**                     | `src/mcp.rs` (`tools/list` schema + `handle_tool_call`)          |
| Change the **database schema**                   | add a file in `migrations/` *and* update `src/storage.rs`        |
| Change **retrieval / ranking**                   | `src/query.rs`                                                   |
| Change **embeddings** (provider / model / dims)  | `src/embedding.rs` (+ `src/config.rs` for config & `DATABASE_URL`/env) |
| Change **node / edge / chunk shapes** or enums   | `src/models.rs`                                                  |
| Change **editor install / registration**         | `src/setup.rs`                                                   |
| Change the **plugin hook** behavior              | `src/hook.rs`                                                    |
| Change **HTML generation / escaping**            | `src/export_util.rs`, `src/graph_export.rs` (feature pages: `src/feature_export.rs`) |
| Change **Obsidian vault** output                 | `src/obsidian_export.rs`                                         |

## MCP Tools

The stdio MCP server exposes exactly four tools: `chaos_analyze`, `chaos_query`,
`chaos_feature_context`, and `chaos_write_feature_website`. See the **MCP Tools** section of
`README.md` for the canonical reference of names, arguments, and intended usage.

## Hard Rules (non-negotiable)

These are architectural invariants — any change that violates one is wrong by definition:

1. **Real embeddings only.** No mock embedders, fake vectors, or random vectors. If no real
   embedder (OpenAI or Ollama) is available, analysis must **fail closed**, never fabricate
   vectors.
2. **Postgres + pgvector persistence.** Do not replace persistent storage with in-memory storage;
   the memory must survive process restarts.
3. **stdio MCP.** The MCP server stays on stdio with newline-delimited JSON-RPC (no
   `Content-Length` framing). Never write logs or diagnostics to stdout in the `mcp` path.
4. **Rust-only extraction.** TypeScript/JavaScript, Python, and Solidity support must remain
   Rust-side AST extraction — never a Node or Python sidecar service.

> MCP and plugin configuration must launch the **release binary** directly
> (`target/release/chaos --config <cfg> mcp`) over stdio. `cargo run` is acceptable only for
> one-off CLI setup, never in MCP/plugin config.
