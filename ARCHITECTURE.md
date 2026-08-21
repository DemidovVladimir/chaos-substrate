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
  ├─ JS / TS         ──  src/lang/javascript.rs      (oxc; also collects embedded gql documents)
  ├─ Python          ──  src/lang/python.rs          (rustpython-parser; also collects gql("…") calls)
  ├─ Solidity        ──  src/lang/solidity.rs        (solang-parser)
  ├─ GraphQL         ──  src/lang/graphql.rs         (apollo-parser; SDL + executable documents)
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
Postgres + pgvector  ──  src/storage/ (sqlx; one Storage type, impl blocks split by concern)
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
  ├─ MCP tools (24)                            ──  src/mcp.rs
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
- **GraphQL** — `apollo-parser`, in `src/lang/graphql.rs` (`.graphql`/`.gql`/`.graphqls`).

The language extractors in `src/lang/*` share a `LineIndex` and produce a `FileExtraction`.
Captured symbols include functions, classes/structs, interfaces/traits, enums, type aliases, class
methods, imports, inheritance edges (Rust traits/impls and Solidity only), and (heuristic,
file-scoped) call edges. Markdown/MDX and text PDF
are supplemental document context (lower weight). JSON is limited to config manifests
(`package.json`, `cdk.json`, `tsconfig.json`, `jsconfig.json`); `Cargo.toml` is parsed for
dependency nodes, and AWS-CDK constructs become deployment-resource nodes — all in `extractor.rs`.

Honest residual limits: no `tsc`/type inference, no path-alias resolution, and cross-file call
resolution is name-based.

Concrete capture specifics: the TS/JS extractor (`oxc`) reads `.ts/.tsx/.mts/.cts/.d.ts` and
`.js/.jsx/.mjs/.cjs`, harvests all four npm dependency scopes (`dependencies`, `devDependencies`,
`peerDependencies`, `optionalDependencies`) plus npm scripts from `package.json`, and tags test
symbols by path (`*.test.*`, `*.spec.*`, `__tests__/`, `__test__/`). From `cdk.json` it extracts the
CDK app command, `Stack` classes, and L2 construct/resource types (Lambda, DynamoDB, queues,
buckets, API resources) as deployment-resource nodes. Rust extraction (`syn`) emits no byte offsets
(nodes carry `line_start`/`line_end` only); `EdgeKind::UsesType` is emitted only by the GraphQL
post-pass (Rust extraction still never emits it), and a method-on-type is represented with a
`Contains` edge rather than a dedicated `method_of` edge.

GraphQL extraction (`apollo-parser`, error-tolerant — a broken file still yields its recoverable
definitions, and zero recovered chunks degrade to the shared whole-file fallback) covers SDL type
systems and executable documents. Type definitions map onto existing node kinds (object/input →
struct, interface → trait, enum, union/scalar/directive → type alias) with
`metadata.language: "graphql"`; operations and fragments get their own `graphql_operation` /
`graphql_fragment` kinds, and schema root fields become `graphql_field` user-surface nodes
(qualified `Query.user`, honoring custom `schema { query: … }` root mappings). References are
resolved *after* the file loop against a graphql-only name map — never through the shared,
walk-order-populated `symbol_names` map, which would drop forward references and let a
same-named TS class capture a GraphQL edge — yielding `uses_type` (field/arg types, operation
selections, `extend` → base tagged `relation: "extends"`), `implements`, and fragment-spread
`calls` edges. God-node protection mirrors the external-import drop: no edges to scalar/directive
targets, and `uses_type` edges to a type are capped at 12 distinct referencing files (the 13th
file's edge is dropped); extension→base links are exempt from both gates. Documents embedded in host code (`` gql`…` `` tagged templates and `gql(…)` calls
in TS/JS, `gql("…")` calls in Python) run through the same extractor with host-file line anchors
and the document itself as the chunk body. Roadmap: code-first schema recovery (async-graphql,
TypeGraphQL, graphene/strawberry) is not extracted yet — SDL is the only schema source — and an
incremental `chaos add` resolves GraphQL cross-file edges only within the changed-file batch.

### Persistence and runs

Each analysis is bracketed by an `analysis_runs` row (`begin_analysis` → `finish_analysis` with
`completed`/`failed`). `storage.replace_repo_index` swaps in the freshly extracted nodes/edges/
chunks for the repo while **preserving embeddings by content hash** (unchanged content costs zero
embedder calls; the output reports `reused_embeddings`). Chunks still missing embeddings for the
active provider/model/dimensions are then embedded in **batched requests** (16 texts per call,
`embedding::embed_missing_chunks`) and written to the `embeddings` table. Migrations run via
`sqlx::migrate!` and are tracked in `_sqlx_migrations`.

### Known limitations

- **No ANN vector index.** `embeddings.embedding` has no HNSW/IVFFlat index — only a B-tree on
  `(provider, model_id, dimensions)` — so every vector search is an exact scan over the candidate
  set. Adequate at current corpus sizes; revisit if a single repo's chunk count grows large.
- **Shallow health check.** `Storage::health()` verifies Postgres connectivity (`select version()`)
  and that the `vector` extension is present; it does **not** validate `pgcrypto`, the migration
  version, embedding dimensions, or index existence. `chaos doctor` layers the real embedder probe
  on top.

### Feature page manifest

Every generated feature website embeds a stable machine contract as
`<script type="application/json" id="chaos-feature-manifest">`, and agents read **that JSON**, never
the rendered DOM (markup/CSS can change; the manifest is the contract). Fields: `schema_version`;
`feature` (`id`, `title`, `domain`, `summary`); `purpose` (plain-language "what this is for",
required by `chaos_write_feature_website`); `claims[]` (`id`, `title`, `body`, `confidence`,
`node_ids`); `modes`; `nodes`; `edges`; `story[]` (`id`, `title`, `body`, `node_ids`, `edge_ids`);
`examples[]`; `provenance` (breadcrumbs); and `related_features`. A story step highlights exactly its
own `node_ids`/`edge_ids` — renderers must **not** auto-expand the graph from the first node; broader
views belong in `modes`. `chaos_feature_context` / `chaos_refresh` scan `docs/features_memory/*.html`
for this block and ignore pages without it.

## Where to Change What

| If you want to…                                  | Edit                                                              |
| ------------------------------------------------ | ---------------------------------------------------------------- |
| Add / change a **language**                      | add `src/lang/<lang>.rs`, wire a `FileKind` variant + dispatch in `src/extractor.rs`, add the variant to `Language` in `src/models.rs` |
| Change **GraphQL extraction** (SDL / documents / edge resolution) | `src/lang/graphql.rs` (embedded-document collection: `src/lang/javascript.rs`, `src/lang/python.rs`) |
| Change **edge weights** (cost / confidence)      | `src/weights.rs`                                                  |
| Add / change a **CLI command**                   | `src/main.rs` (clap `Commands`)                                  |
| Add / change an **MCP tool**                     | `src/mcp.rs` (`tools/list` schema + `handle_tool_call`)          |
| Change the **database schema**                   | add a file in `migrations/` *and* update the matching `src/storage/*.rs` module |
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
  detected by the linkers in `src/linker.rs` (`package_dep` / `abi` / `http_route` / `graphql`,
  consumer → provider) purely from the persisted index — the `http_route` provider side anchors on
  persisted `HttpRoute` surface nodes unioned with the chunk scan (nodes win on a shared path),
  and `graphql` matches executable
  operations against another member's SDL-derived `graphql_field` nodes (operation types must
  agree; code-first servers expose no provider facet yet). Links attach at L1 — never L0, whose
  FK-protected schema stays frozen — and follow the same hash-gated pipeline as L3: every
  `analyze`/`add` ends by relinking the repo's projects, gated by the L2 `repo_root_hash`
  (`project_repos.linked_repo_hash`), so a no-change re-index relinks nothing. The project-wide
  `chaos_features` inventory (every member's features, repo-tagged and cross-link-annotated) is
  written to the project workspace (`~/.chaos/projects/<slug>/` or `$CHAOS_PROJECT_DIR`).

### Design decisions (rationale)

The layered memory is **two trees, not one**: a *summary tree* (community L3 summaries + embeddings,
RAPTOR-style) and a *hash tree* (the L2 Merkle rollup of `content_hash`es). The hash tree gates the
summary tree — a community's summary is recomputed only when its `subtree_hash` changes, so a
no-change re-index does zero summary work. Recorded design verdicts:

- **D1 — deterministic Louvain at γ=1.0**, edge weight = `confidence / cost` (not `1 − cost/max_cost`,
  which would zero out `imports`/`calls`). The repository node is **excluded** from community
  detection (its `contains` star would otherwise collapse everything into one community).
- **D2** communities live in their own tables, not as overloaded `NodeKind`s; **D3** the quotient
  graph uses typed edges with per-kind counts; **D4** summaries get a dedicated `community_embeddings`
  table; **D5** a single summary level for v1; **D6** the change-decomposition tool is named
  `chaos_change_plan`.
- **Louvain, not Leiden.** Measured internal connectivity was already fully connected (e.g. 256/256
  communities on a large repo, 28/28 here, zero disconnected), so Leiden's randomized refinement
  would break the determinism invariant for no benefit; the fallback for any disconnected community
  is a connected-components post-split. A post-Louvain consolidation folds sub-threshold fragments
  (`MIN_COMMUNITY_SIZE = 4`, `src/community.rs`) into their folder-preferred best-coupled neighbour.
- **Hexagonal verdict (0.24.0): two paying ports, no `StoragePort`.** The ports that earn their
  ceremony are `Embedder` (`src/embedding.rs` — two real HTTP adapters plus fail-closed test stubs)
  and `ChunkSearch` (`src/storage/search.rs` — the retrieval channels plus the edge lookup the query
  pipeline fuses; `Storage` is the production adapter, and the fusion-pipeline tests in
  `src/query.rs` drive the composed normalization/merge/rerank/recall-floor logic through a scripted
  fake with zero embed calls). A full `StoragePort` (~75 methods) was REJECTED: its only conceivable
  second adapter is an in-memory store, which the hard rules forbid — that trait would be pure
  ceremony. Instead `src/storage.rs` was split by concern into
  `src/storage/{repo,index,merkle,community,search,graph,project,stack_queries}.rs` behind the one
  unchanged `Storage` type (multiple `impl` blocks; zero public-API churn).
- **Blanket impls.** `impl<T: ChunkSearch + ?Sized> ChunkSearch for &T` and
  `impl<E: Embedder + ?Sized> Embedder for Arc<E>` make borrowed/shared handles first-class
  implementors; `build_embedder` returns `Arc<dyn Embedder>`, so the CLI pipelines, the MCP loop,
  and `graph --serve` share one built embedder instead of each re-wrapping a `Box`.
- **Deferred: MCP tool-registry macro.** The tool roster in `src/mcp.rs` lives in three places
  (`TOOL_NAMES`, the `tools/list` builder, the dispatch match) that a `register_tool!`-style macro
  could collapse into one item per tool. Deferred deliberately: the anti-drift tests already pin the
  three sites to each other, and a macro would bury the long per-tool description strings that
  agents (and doc reviews) read in place.
- **Report-template exclusions (theme consolidation).** The shared `theme::REPORT_CSS`/`REPORT_JS`
  splice covers the 8 report templates (impact, usage, change-plan, components, stack,
  feature-inventory, sui-migration, feature-context). Excluded as structurally divergent — their
  copies of the shared rules are not byte-identical, so splicing would change rendered pages: the
  compose pages (`compose.rs`), the feature-story site (`feature_story.rs`), the engineer feature
  page (`feature_export.rs`), the feature map (`hierarchy_export.rs`), and the storyboard
  (`user_story.rs`). They keep their own template CSS until a deliberate redesign (see the note on
  `theme::REPORT_CSS`).

## MCP Tools

The stdio MCP server exposes exactly twenty-four tools: `chaos_analyze`, `chaos_add`, `chaos_stats`, `chaos_stack`,
`chaos_pages`, `chaos_gaps`, `chaos_query`, `chaos_feature_context`, `chaos_impact`, `chaos_usage`,
`chaos_sui_migration_impact`, `chaos_write_feature_website`,
`chaos_obsidian`, `chaos_refresh`, `chaos_write_storyboard`, `chaos_change_plan`, `chaos_components`, `chaos_features`, `chaos_compose`, `chaos_project`, `chaos_feature_story`, `chaos_help`, `chaos_clean`, and `chaos_graph`. See the **MCP Tools** section of `README.md` for the
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
4. **Rust-only extraction.** TypeScript/JavaScript, Python, Solidity, and GraphQL support must
   remain Rust-side AST extraction — never a Node or Python sidecar service.

> MCP and plugin configuration must launch the **release binary** directly
> (`target/release/chaos --config <cfg> mcp`) over stdio. `cargo run` is acceptable only for
> one-off CLI setup, never in MCP/plugin config.
