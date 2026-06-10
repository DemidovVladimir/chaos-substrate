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
  │   OpenAI text-embedding-3-small (1536d) OR Ollama embeddinggemma (768d)
  │   FAIL-CLOSED: no real embedder ⇒ analysis fails (never fake/random vectors)
  ▼
Postgres + pgvector  ──  src/storage.rs (sqlx)
      tables: repositories, analysis_runs, files, nodes, edges, chunks, embeddings
      hierarchy (additive): communities, community_members, community_edges,
                            subtree_hash rollup, community_embeddings,
                            community_summary_cache
      projects (additive):  projects, project_repos, cross_repo_links
  │
  ▼
hybrid retrieval  ──  src/query.rs
      semantic (vector) + keyword + graph routing over the persisted index
  │
  ▼
outputs
  ├─ CLI results (JSON on stdout)              ──  src/main.rs
  ├─ MCP tools (17)                            ──  src/mcp.rs
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
methods, imports, inheritance edges (Rust traits/impls and Solidity only), and (heuristic,
file-scoped) call edges. Markdown/MDX and text PDF
are supplemental document context (lower weight). JSON is limited to config manifests
(`package.json`, `cdk.json`, `tsconfig.json`, `jsconfig.json`); `Cargo.toml` is parsed for
dependency nodes, and AWS-CDK constructs become deployment-resource nodes — all in `extractor.rs`.

Honest residual limits: no `tsc`/type inference, no path-alias resolution, and cross-file call
resolution is name-based.

### Persistence and runs

Each analysis is bracketed by an `analysis_runs` row (`begin_analysis` → `finish_analysis` with
`completed`/`failed`). `storage.replace_repo_index` swaps in the freshly extracted nodes/edges/
chunks for the repo while **preserving embeddings by content hash** (unchanged content costs zero
embedder calls; the output reports `reused_embeddings`). Chunks still missing embeddings for the
active provider/model/dimensions are then embedded in **batched requests** (16 texts per call,
`embedding::embed_missing_chunks`) and written to the `embeddings` table. Migrations run via
`sqlx::migrate!` and are tracked in `_sqlx_migrations`.

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
| Change **community detection** (L1 features)     | `src/community.rs`                                               |
| Change the **Merkle rollup** (L2 blast radius)   | `src/merkle.rs`                                                  |
| Change **community summaries** (L3)              | `src/community_summary.rs`                                       |
| Change the **change-plan** tool                  | `src/change_plan.rs`                                             |
| Change **god-node / feature-map export**         | `src/hierarchy_export.rs`                                        |

## Hierarchical (Layered) Memory

On top of the L0 multigraph, Chaos Substrate maintains an **additive, all-Rust hierarchy**. It is
purely additive: a repository indexed before the hierarchy existed still answers `query`, `stats`,
and `add` exactly as before.

- **L0 — multigraph.** The base typed knowledge graph (nodes, edges, chunks, embeddings) described
  above.
- **L1 — communities / "god-nodes" / features.** A deterministic Louvain pass in `src/community.rs`
  clusters L0 into features, persisted as a quotient graph in the `communities`,
  `community_members`, and `community_edges` tables (`migrations/002_communities.sql`).
- **L2 — Merkle rollup.** `src/merkle.rs` rolls each chunk's `content_hash` up into
  file / community / repo `subtree_hash`es, which drive `chaos add`'s per-feature **blast radius**
  (`migrations/003_subtree_hash.sql`).
- **L3 — community summaries.** `src/community_summary.rs` produces hash-gated, embedded summaries
  per community (`community_embeddings` table, `migrations/004_community_summary.sql`). The gate
  means a no-change re-index makes **zero** summary embed calls.

`src/change_plan.rs` powers the `chaos_change_plan` tool over these layers, and
`src/hierarchy_export.rs` renders the god-node Obsidian notes and feature map. Accordingly,
`chaos_refresh` / `chaos_obsidian` now also regenerate god-node community notes
(`vault/Communities/*.md` + `Feature Map.md`) and an interactive
`docs/features_memory/feature-map.html` straight from the persisted layers — with **no re-index and
no embedder**.

- **P6 — cross-repository projects.** A **project** (`src/project.rs`,
  `migrations/005_projects.sql`) groups indexed repositories (client, backend, smart contracts,
  infra, …) and maintains **feature→feature cross-repo links** between their L1 communities,
  detected by the linkers in `src/linker.rs` (`package_dep` / `abi` / `http_route`,
  consumer → provider) purely from the persisted index. Links attach at L1 — never L0, whose
  FK-protected schema stays frozen — and follow the same hash-gated pipeline as L3: every
  `analyze`/`add` ends by relinking the repo's projects, gated by the L2 `repo_root_hash`
  (`project_repos.linked_repo_hash`), so a no-change re-index relinks nothing. The project-wide
  `chaos_features` inventory (every member's features, repo-tagged and cross-link-annotated) is
  written to the project workspace (`~/.chaos/projects/<slug>/` or `$CHAOS_PROJECT_DIR`).

## MCP Tools

The stdio MCP server exposes exactly seventeen tools: `chaos_analyze`, `chaos_add`, `chaos_stats`,
`chaos_query`, `chaos_feature_context`, `chaos_impact`, `chaos_write_feature_website`,
`chaos_obsidian`, `chaos_refresh`, `chaos_write_storyboard`, `chaos_change_plan`, `chaos_components`, `chaos_features`, `chaos_project`, `chaos_help`, `chaos_clean`, and `chaos_graph`. See the **MCP Tools** section of `README.md` for the
canonical reference of names, arguments, and intended usage.

`chaos_change_plan` (CLI `chaos change-plan <repo> "<change>" [--since <ref>]`) decomposes a
proposed change into the **features** (L1 communities / god-nodes) it spans, with a
dependency-aware check order. It matches the change description against community summary
embeddings (optionally also seeding from a real git diff via `since`), then ALWAYS writes an
interactive HTML plan (light editorial theme) to `docs/features_memory/<slug>-plan.html` and returns a compact
JSON summary (per-feature label, confidence, check order, top symbols, HTML path). `chaos_query`
also gained an optional `hierarchical` flag (CLI `query --hierarchical`) for top-down retrieval that
matches feature (community) summaries first and returns the surfaced features alongside the chunk
hits, falling back to flat search when no hierarchy exists. See the layered-memory section below.

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
