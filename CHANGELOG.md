# Changelog

All notable changes to Chaos Substrate are documented here. Versions before
0.12.0 predate this file; see the git history (`P0`–`P5` commits) for the
hierarchical-memory build-out.

## 0.14.0 — 2026-06-10

The tech-stack release: "what is this repo built with?" is now a first-class
question, answered from the persisted index — no more agents falling back to
grepping `package.json` (observed in a real molecule_core session: the agent
used chaos correctly for orientation, then shelled out for the dependency
list because no tool would name it).

### Added — `chaos_stack` MCP tool + `chaos stack` CLI (18 tools)

`chaos_stats` only *counts* dependency and deployment-resource nodes;
`chaos_stack` *lists* them. Read-only and embedder-free, it reports:

- **Dependencies by ecosystem** (npm / cargo): manifest-DECLARED entries only
  (import-derived `dependency` nodes are excluded by their stable_id shape),
  each with name, distinct version requirements, runtime-vs-dev scope from the
  manifest section, and how many workspace manifests declare it —
  widest-declared first, the non-hardcoded "load-bearing package" signal.
- **npm scripts** grouped by name with a deterministic example command.
- **Deployment & infrastructure**: AWS CDK app entrypoints (cdk.json), Stack
  classes, and L2 constructs grouped by cloud service with examples.
- **JS/TS config files** and the **file-language breakdown**.

Like the other surfacing tools it ALWAYS writes an interactive HTML inventory
(default `docs/features_memory/stack.html`, a repo-level singleton like
`graph.html`, swept by `chaos_clean --artifacts`; embedded
`chaos-stack-manifest`) and returns a COMPACT JSON summary — capped lists with
lifted `*_omitted` counts, uniform array rows, provenance breadcrumbs.

**Honesty contract:** the return carries explicit `coverage` notes naming what
the extractor does **not** persist yet (Dockerfiles, CI workflows,
pyproject.toml, foundry.toml, Terraform), so an agent never mistakes the
inventory for a complete scan — widening extractor coverage is the follow-up.

New read-only storage facets back it: `stack_dependencies`, `stack_scripts`,
`stack_deployment_resources`, `stack_config_files` (`src/storage.rs`), all
keyed off existing tables — no schema change.

## 0.12.0 — 2026-06-10

The cross-repository release: Chaos now understands features that span
**multiple repos** (client, backend, smart contracts, infra), and the whole
pipeline was audited and reworked so that **unchanged content never costs an
embedder call** and tool returns never flood an agent's context.

PR: [#3](https://github.com/DemidovVladimir/chaos-substrate/pull/3) ·
Migrations: `005_projects.sql`, `006_summary_cache.sql` · MCP tools: 13 → **17**

### New — cross-repository projects (P6)

A **project** is a named set of indexed repositories. Chaos detects
**feature→feature cross-repo links** between members, purely from the
persisted index (consumer → provider):

| Linker | What it matches | Confidence |
| --- | --- | --- |
| `package_dep` | one repo imports a package another member publishes (`package.json` / `Cargo.toml` name, import-context checked) | 0.9 |
| `abi` | non-Solidity code references a contract/interface defined in another repo (word-boundary, CamelCase-gated) | 0.8 |
| `http_route` | a fetch/axios/client call path matches a route registered in another repo (params normalize to `*`) | 0.65 |

- Links attach at the **feature (L1) level**, never L0 — the FK-protected base
  schema stays frozen, and a re-detection that reshapes a repo's features
  drops its stale links automatically (FK cascade).
- **The project layer follows the same layered pipeline as L1–L3:** every
  `analyze`/`add` ends by relinking the repo's projects, gated by the L2 repo
  root hash (`project_repos.linked_repo_hash`). A no-change re-index relinks
  nothing.
- New CLI: `chaos project create | add-repo | list | status | relink`.
  New MCP tool: `chaos_project`. Every link carries evidence (matched
  names/paths, example files) and provenance breadcrumbs.
- `chaos features --project <name>` (MCP: `project` param): every member
  repo's features in **one journey-layered inventory**, each card tagged with
  its repo alias and annotated with cross-repo links
  (`→ backend:auth-api (http_route)`). Project artifacts live in
  `~/.chaos/projects/<slug>/` (or `$CHAOS_PROJECT_DIR`) — no single repo's
  `docs/` can own a multi-repo page.
- All member repos must share one embedder config; `status`/`relink` warn on
  mismatch.

### New — surfacing tools and feature quality

- **`chaos_components`** — the orientation step before feature extraction:
  given an area (or nothing, for a repo overview) it surfaces the communities
  that make it up, how they connect, and a dependency-first read order.
  Always writes an interactive HTML overview; returns compact JSON.
- **`chaos_features`** — the exhaustive god-node inventory, grouped by journey
  layer (entry → interface → core → foundation). The single filter is
  auto-detected: path/directory → folder scope; a layer word (`client`/`api`/
  `contracts`…) → that layer; anything else → a topic match. Only the topic
  filter needs the embedder.
- **Journey layering** (`src/layering.rs`): deterministic, path-based
  classification of features into entry/interface/core/foundation — the
  vocabulary that lets a cross-repo project read client → backend →
  contracts/infra naturally.
- **Summary v3**: extractive community summaries now lead with a humanized
  label and journey role, prefer definitions over imports for key symbols, and
  name neighboring features. Manifest-dependency nodes are excluded from
  community detection (no more god-nodes named after the most-imported npm
  package); external imports are dropped from the graph.
- **`chaos struct-features`** (hidden debug command): the structure-first
  feature-extraction prototype, printed side-by-side with the Louvain
  communities, to ground the planned partition redesign.

### Improved — LLM token efficiency

A full audit of every embedder call and every byte returned into an agent's
context, followed by fixes for everything found:

| Surface | Before | After |
| --- | --- | --- |
| Full `chaos analyze` of an unchanged repo | re-embedded **every** chunk | **0 embed calls** — embeddings are preserved by content hash across the wipe (reported as `reused_embeddings`) |
| Community-ID churn (partition shuffle renames an unchanged community) | re-summarized + re-embedded it | **0 embed calls** — content-addressed summary cache (`community_summary_cache`, reported as `summaries.reused_from_cache`) |
| Hierarchical query | embedded the same question twice | once (routing embedding reused for the flat search) |
| Project-wide topic listing (N repos) | embedded the same filter N times | once |
| Indexing HTTP traffic | one request per chunk | batched, 16 texts per request (OpenAI and Ollama array inputs) |
| `chaos_write_feature_website` | the LLM authored 20–60 KB of raw HTML, paid as completion tokens AND again as the tool argument | **manifest-driven**: pass the structured manifest, Chaos renders the interactive page deterministically (same renderer as `chaos add`); `html` remains as a legacy option |
| `chaos_query` / `chaos_feature_context` returns | unbounded chunk contents (~5–12k tokens per call) | excerpted at the return boundary — hits 800 chars, node code 600, route summaries 400, each marked `[+N chars in the indexed chunk]`; generated HTML keeps the **full** evidence |
| `chaos_features` inline list | unbounded (every feature) | capped at 80 entries with a pointer to the exhaustive HTML inventory |
| Per-session tools/list payload | ~9.9 KB | ~7 KB (largest descriptions rewritten) |

What was already efficient stays untouched: `chaos add` embeds only changed
chunks, L3 summaries are extractive (no generation tokens) and hash-gated, and
all exports/refresh/hook/linkers are embedder-free.

### New — `chaos help`

- `chaos help [<command>]`: an agent-friendly guide — every command with its
  purpose (generated from the CLI definition itself, so it can never drift),
  typical workflows with copy-paste examples, and config pointers. Works from
  any directory with **no database or config**, so an agent can orient itself
  without `cd`-ing into the checkout and compiling. `chaos help <command>`
  prints that command's full flags; `--help` still works everywhere.
- The MCP twin: a `chaos_help` tool returns the same workflow guide on demand
  (zero tokens until called), and the server's `initialize` response now
  carries compact MCP `instructions` so every session starts with the tool
  order and a pointer to `chaos_help`.

### New — wrapper pass-through

- The `chaos` wrapper (`bin/chaos` → `scripts/chaos`, the PATH-installed
  entrypoint) now passes every unrecognized command straight through to the
  real binary with the repo's config — `chaos analyze/add/query/features/
  components/project/clean/help/…` all work from anywhere, with the binary
  auto-rebuilt when sources changed. Previously the wrapper rejected
  everything outside its own setup verbs (`bootstrap`, `init`, `update`, …),
  which contradicted every documented command. `chaos help` through the
  wrapper shows the binary's agent guide plus the wrapper-only extras.
  The wrapper is now ONE file — `bin/chaos` (the path `.mcp.json` and the
  PATH symlink already used); `scripts/chaos` is gone.

### New — `chaos_clean` MCP tool

- The clean-slate flow is reachable from agent sessions too (17 tools total):
  `chaos_clean {repo?, artifacts?, confirm: true}` mirrors
  `chaos clean [--artifacts]`. It is guarded — the call fails without
  `confirm: true`, and the description instructs agents to use it only on
  explicit user request. Previously agents had to cd into the checkout and
  drive the CLI to reset state.

### New — `chaos_graph` MCP tool

- The standalone interactive graph export is reachable from agent sessions
  (17 tools total): `chaos_graph {repo, output?}` mirrors
  `chaos graph <repo> -o graph.html`, defaulting to
  `docs/features_memory/graph.html` inside the repo so `chaos_clean
  --artifacts` sweeps it. Embedder-free, read-only over the persisted index.

### Changed — default local embedding model: EmbeddingGemma

- The recommended/default Ollama model is now **`embeddinggemma`** (Google,
  308M, 768 dims — best-in-class code retrieval under 500M params), replacing
  `nomic-embed-text`. Same dimensions, so only the model name changes in
  config. Existing vectors are unaffected (embeddings are keyed per model);
  the first analyze per repo under the new model re-embeds once, then the
  content-hash gates apply as usual. `bin/chaos ollama-setup` now pulls
  whatever model the config names instead of a hardcoded one.

### New — clean slate for validation

- `chaos clean [--artifacts]`: the database wipe (all repos or one) can now
  ALSO delete the generated files on disk — each repo's
  `chaos-obsidian-vault/` and `docs/features_memory/`, plus (when clearing
  everything) the project workspaces under `~/.chaos/projects/`. Off by
  default because feature pages are often committed as durable feature
  memory; the output lists exactly what was removed (`artifacts_removed`).

### Fixed — pre-release audit (7-angle review)

- The project relink hash gate no longer stays permanently open for a member
  repo that has no root hash yet (it used to force a full relink on **every**
  `analyze`/`add` of any member, forever).
- Alias collisions on `project add-repo` produce a clear message instead of a
  raw Postgres unique-constraint error.
- `chaos clean` truncates the project tables, and the removal report counts
  them.
- The hierarchical query's lexical label fallback is now a **true fallback**
  (only when the cosine pass routed nothing), with a 6-char prefix floor
  (`auth` no longer matches `author`) and `api`/`app`/`src`/`lib`/`web` added
  to the route stopwords — generic queries no longer route to the largest
  communities at an inflated score.
- `chaos components` with no area keeps the largest communities — semantic
  expansion (which used to evict them and emit a breadcrumb referencing a
  nonexistent "area") now runs only when an area is given.
- The topic filter ignores the summary's "Related features:" line, so a topic
  no longer matches every neighbor of a feature named after it.
- JS package-import detection requires `from`/`import`/`require(` to directly
  precede the string literal — a comment like
  `// important: '@org/ui' is deprecated` can no longer fabricate a
  high-confidence cross-repo link.
- `extern crate` imports reach the package linker's scanner; unreadable
  manifests (a moved checkout) warn instead of silently shrinking a project's
  link set on the next relink.
- Tool/doc role vocabulary corrected to what the code emits
  (`entry/interface/core/foundation`; `standalone` was never produced).

### Schema & upgrade notes

- Run `chaos migrate`. Two additive migrations:
  - `005_projects.sql` — `projects`, `project_repos`, `cross_repo_links`.
  - `006_summary_cache.sql` — `community_summary_cache`.
- **One-time costs on the first `analyze` per repo after upgrading** (steady
  state afterwards is zero-cost for unchanged content):
  - chunk embeddings are re-created once (pre-existing embeddings die with
    the old chunks before the preservation logic has anything to restore);
  - the summary-v3 algo bump re-summarizes every community once.
- Behavior changes agents may notice:
  - `chaos_query`/`chaos_feature_context` returns contain excerpts with
    explicit truncation markers (full text remains in the index and in
    generated HTML);
  - `chaos_write_feature_website` no longer requires `html` — omit it and let
    Chaos render (the minimum-evidence contract still applies);
  - analyze/add output gained `reused_embeddings`,
    `summaries.reused_from_cache`, and a `projects` relink report.

### Validation

173 tests; `cargo clippy --all-targets --all-features -- -D warnings` and
`cargo fmt --check` clean. Verified live against Postgres+pgvector with a real
embedder: second full analyze of an unchanged repo → `embedded_chunks: 0,
reused_embeddings: 6, embed_calls: 0`; simulated community-ID churn →
`reused_from_cache: 3, embed_calls: 0`; project create/add-repo/relink
round-trip with the hash gate returning `up_to_date`. Plugin packaged as
`dist/chaos-substrate-cowork-plugin-0.12.0.zip`.

### Known follow-ups (deliberately not in this release)

- Structure-constrained community partition (the `struct-features` spike's
  verdict) plus a full re-analyze — recommended before heavy cross-repo use;
  the summary cache already removes its token cost.
- Project modes for `chaos_components`, `chaos_change_plan`, and
  `chaos_query`.
- Linker throughput (single-pass scans / Aho-Corasick) and helper
  consolidation (`safe_slug` ×8, LIKE-escaping ×3, language tables ×3).
