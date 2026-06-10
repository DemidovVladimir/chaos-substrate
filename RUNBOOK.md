# Chaos Substrate Ops Runbook

Copy-paste operational reference for running Chaos Substrate — a portable, persistent
code-knowledge memory for AI agents (Postgres + pgvector), queried via the `chaos` CLI and a
stdio MCP server.

The binary is named `chaos`. The global flag `--config <PATH>` selects the config file
(default: `chaos-substrate.toml`).

- For MCP and plugin wiring, always launch the **release binary** directly over stdio
  (`target/release/chaos ... mcp`). Do **not** use `cargo run` in MCP/plugin config.
- `cargo run -- <subcommand>` is fine for one-off CLI work (bootstrap, ad-hoc queries).

Build the release binary once:

```sh
cargo build --release
# binary at: target/release/chaos
```

## Orientation

```sh
chaos help              # every command + typical workflows; works anywhere, needs no DB/config
chaos help <command>    # full flags for one command
```

## Bootstrap

```sh
# 1. Start bundled Postgres + pgvector (pgvector/pgvector:pg16, host port 54329)
docker compose up -d

# 2. Provide a config (committed default targets Ollama)
cp chaos-substrate.example.toml chaos-substrate.toml   # if you keep an example; otherwise edit chaos-substrate.toml

# 3. Apply database migrations (sqlx::migrate!, tracked in _sqlx_migrations)
#    Includes the hierarchy layers: 002_communities (L1 god-nodes),
#    003_subtree_hash (L2 Merkle rollup), 004_community_summary (L3 summaries).
cargo run -- migrate
# or: target/release/chaos --config chaos-substrate.toml migrate

# 4. Verify connectivity, schema, and embedder
cargo run -- doctor
# or: target/release/chaos --config chaos-substrate.toml doctor
```

Default bundled connection:
`DATABASE_URL=postgres://chaos:chaos@localhost:54329/chaos_substrate`
(`DATABASE_URL` overrides the config file when set.)

## Index / Refresh

```sh
# Index (or re-index) a repository into the knowledge memory
chaos analyze /path/to/repo

# Refresh an already-indexed repo; optionally regenerate exports/features
chaos refresh /path/to/repo
chaos refresh /path/to/repo --obsidian-output vault
chaos refresh /path/to/repo --features-dir features
chaos refresh /path/to/repo --all-features
```

`analyze` requires a real embedder (OpenAI or Ollama). If none is configured, analysis
**fails by design** — never produces fake/random vectors.

Re-running `analyze` is cheap: chunk embeddings are **preserved by content hash** across the
re-index (the output reports `reused_embeddings`), L3 summaries are hash-gated, and a
content-addressed summary cache covers community-ID churn (`summaries.reused_from_cache`) — so a
full re-analyze of unchanged code makes **zero** embedder calls. Embedding requests are batched
(16 texts per call) for both providers.

`refresh` (and `obsidian`) also regenerate god-node community notes from the persisted layers —
`vault/Communities/*.md` plus a `Feature Map.md`, and an interactive
`docs/features_memory/feature-map.html` — with **no** re-index and **no** embedder.

## Add (incremental)

`chaos add` is the one-shot "index what I just changed" command: it detects changed files from
git (no file list needed), merges **only those files** into the existing index (delete + re-extract
+ re-embed just them), refreshes the Obsidian vault, and writes an interactive feature/bug page into
`docs/features_memory`.

```sh
# Index the current git working-tree changes (staged + unstaged + untracked)
chaos add /path/to/repo -m "what this change does"

# Diff a committed range instead of the working tree
chaos add /path/to/repo --since HEAD~3

# Index specific files (e.g. a Notion/Markdown export or PDF), bypassing git
chaos add /path/to/repo --path notes/spec.md --path docs/design.pdf

# Force classification / skip an artifact
chaos add /path/to/repo --kind bug -m "fix null deref"
chaos add /path/to/repo --no-obsidian        # skip vault refresh
chaos add /path/to/repo --no-page            # skip the feature/bug page
```

Feature vs bug is auto-detected from the branch name + latest commit subject (`fix`/`bug`/`hotfix`/…
→ bug, else feature); override with `--kind`. Generated artifact directories (the vault,
`features_memory`, plus everything in `indexing.skip_dirs`) are excluded, so `chaos add` never
re-indexes its own output. Cross-file call edges into *unchanged* files are not rebuilt incrementally
— run `chaos analyze` (or `chaos refresh`) for a full graph rebuild. Like `analyze`, it requires a
real embedder.

## Clean / Reset

```sh
# Wipe every indexed repository from the database
chaos clean

# Wipe only one repository (by absolute path or repository name)
chaos clean /path/to/repo

# ALSO delete the generated files on disk — a truly clean slate for validation
chaos clean --artifacts                  # all repos + project workspaces (~/.chaos/projects)
chaos clean /path/to/repo --artifacts    # one repo's chaos-obsidian-vault/ + docs/features_memory/
```

`clean` removes persisted index data but leaves the schema in place — no `migrate` is needed
before re-indexing. By default it touches ONLY the database; generated files survive because
feature pages are often committed to git as durable feature memory. `--artifacts` additionally
deletes the two Chaos-owned directories inside each repo (`chaos-obsidian-vault/`,
`docs/features_memory/`) and, when clearing everything, the project workspaces — never anything
else. Exports written to caller-chosen paths (`graph -o`, explicit `--output-html`) are not
tracked and must be removed by hand.

## Query

```sh
chaos query /path/to/repo "How does the embedder retry on failure?"
chaos query /path/to/repo "Where are call edges built?" --limit 20
chaos query /path/to/repo "Where is auth handled?" --hierarchical
```

`--limit N` controls the number of retrieved results (default 10).

`--hierarchical` switches to top-down retrieval: it matches feature (community) summaries first and
returns the surfaced features alongside the chunk hits, falling back to flat search when the repo has
no hierarchy.

## Stats

```sh
# Report index statistics for an already-indexed repository (read-only, no embedder)
chaos stats /path/to/repo
```

Reads from Postgres and prints totals (files, nodes, edges, chunks, embedded vs missing
embeddings, split chunks, nodes with chunks) plus breakdowns of nodes by kind, edges by kind,
chunks by type, and files by language. Use it to explain or sanity-check what an `analyze`/`add`
produced.

## Feature Context

```sh
chaos feature-context /path/to/repo "Add a new language extractor"
chaos feature-context /path/to/repo "Add a new language extractor" --output-html out.html
chaos feature-context /path/to/repo "task" \
  --features-dir features \
  --output-html out.html \
  --limit 10 \
  --feature-limit 3 \
  --nodes-per-feature 8
```

Flags: `--limit N` (=10), `--feature-limit N` (=3), `--nodes-per-feature N` (=8),
`--features-dir P`, `--output-html P`.

## Impact

```sh
chaos impact /path/to/repo "Add a new language extractor"
```

Builds a feature-vs-existing-code impact report and **always** writes an interactive HTML (an
impact summary + the evidence dashboard) to `docs/features_memory/<slug>-impact.html`, showing how
a feature maps onto the codebase as it is today (the "before"). Unlike `feature-context` (which only
writes HTML when `--output-html` is passed), `impact` always produces the page.

## Feature guide (storyboard)

```sh
chaos storyboard /path/to/repo --manifest guide.json --output-html out/guide.html
```

Renders a client/user-facing **"Feature guide"** (light editorial scrollytelling page) from a
code-free manifest. Agents normally compose the manifest via `chaos_write_storyboard`; this CLI
path renders one you already have. Notes for an accurate, shippable page:

- **Frames must be real user-facing UI.** Validate with `chaos_query` whether a step is something
  the end user does in a screen vs. backend/server-only — drop the latter (it doesn't belong in a
  user guide).
- **Previews are real captures.** Each frame's `preview` is a real screenshot/clip or a live route;
  Chaos never fakes screens — a frame with no preview shows an "add a screenshot" placeholder.
- **Branding:** pass `--brand-preset molecule` (or set `"brand_preset": "molecule"` in the manifest)
  to apply a preset **shipped inside Chaos** — embedded in the binary, so it works on any install
  with no local files. It fills the logo/hero/company for any empty `brand`/`hero_image` fields;
  explicit manifest values win. Without a preset the renderer stays de-branded ("Add your logo").
- **Portable images:** use `data:` URIs (self-contained) or paths **relative to the output HTML**
  with the files placed alongside — never absolute/temp paths, or images break when shared.
- `confidence` values are optional metadata and are not shown to the reader.
- With `--output-html` the page goes exactly where you point it; without it, the default is
  `docs/features_memory/<slug>-story.html` **inside the target repo** — pass an explicit path if you
  don't want generated HTML landing in your source tree.

## Change Plan

```sh
chaos change-plan /path/to/repo "Add OAuth login and refresh tokens"
chaos change-plan /path/to/repo "Add OAuth login and refresh tokens" --since HEAD~3
```

Decomposes a proposed change into the **features** (L1 communities / god-nodes) it spans, with a
dependency-aware check order. It matches the change description against the community summary
embeddings, **also seeding from a real git diff via `--since` and from previously generated feature
pages it correlates with** (shared files → communities), then **always** writes an interactive HTML
plan to `docs/features_memory/<slug>-plan.html` and prints a compact summary (per-feature label,
confidence, `via` source [semantic/diff/manifest], `matched_by` breadcrumbs, check order, top
symbols, top-level `provenance`, HTML path).

## Components

```sh
chaos components /path/to/repo "OCL"        # explain one big area
chaos components /path/to/repo              # repo-level overview of the core components
```

Explains the **core components** of a big area — the orientation step *before* feature extraction.
An area like "OCL" spans several L1 communities; given an `area` (or none, for a repo-level
overview) it surfaces those communities as components, each with its summary, key symbols/files,
languages, and a quotient-graph role (entry/interface/core/foundation), plus how they connect and a
dependency-first read order. **Always** writes an interactive HTML overview to
`docs/features_memory/<slug>-components.html` and prints a compact JSON summary. Curated and capped
(`--limit`, default 8) — for the *exhaustive* list use `chaos features`.

## Features

```sh
chaos features /path/to/repo client            # every entry-layer ("client") feature
chaos features /path/to/repo onchainlabs/src   # every feature with code under that folder
chaos features /path/to/repo "access control"  # every feature matching a topic
chaos features /path/to/repo                    # all features, grouped by layer
chaos features /path/to/repo --layer core       # force the interpretation
```

Lists **all** god-node features (L1 communities) that match a filter, grouped by journey layer
(entry → interface → core → foundation) — the exhaustive, uncurated counterpart to `components`. The
optional positional filter is **auto-detected**: a path or real directory → **folder** scope; a
single layer word (`client`/`ui`/`api`/`core`/`contracts`) → that **layer** (so "client features" =
every entry-layer feature); anything else → a **topic** match; omit it for the whole repo. Force it
with `--layer`/`--folder`/`--topic`. Only a topic filter needs the embedder; layer/folder/whole-repo
listing is embedder-free. **Always** writes an interactive HTML inventory to
`docs/features_memory/<slug>-features.html` and prints a compact JSON summary (resolved filter + how
detected, per-layer + language counts, per-feature label/role/folders/symbols/`matched_by`,
provenance). `--limit 0` (default) returns everything.

## Projects (cross-repository)

```sh
chaos project create molecule
chaos project add-repo molecule /path/to/client --alias client     # repo must be indexed
chaos project add-repo molecule /path/to/contracts --alias contracts
chaos project list
chaos project status molecule          # members, link staleness, links by kind, embedder check
chaos project relink molecule          # hash-gated; --force to override
chaos features --project molecule      # every member repo's features in ONE layered inventory
chaos features --project molecule client   # …filtered (same auto-detection as single-repo)
```

A **project** groups indexed repositories (client, backend, smart contracts, infra, …) and maintains
**feature→feature cross-repo links** between them, detected from the persisted index only
(consumer → provider): `package_dep` (a manifest `name` one repo publishes is imported by another),
`abi` (non-Solidity code references a contract/interface defined in another repo), `http_route` (a
fetch/axios call path matches a route registered elsewhere; params normalize to `*`). Links attach
at the feature (L1) level with evidence + provenance breadcrumbs and live in `cross_repo_links`
(`migrations/005_projects.sql`).

The project layer follows the same layered pipeline as L1–L3: **every `analyze`/`add` on a member
repo ends by relinking its projects**, gated by the L2 repo root hash
(`project_repos.linked_repo_hash` vs `repositories.repo_root_hash`) — a no-change re-index relinks
nothing, and `add-repo` always links the new member (its gate hash starts NULL). The project-wide
feature inventory is written to the project workspace — `~/.chaos/projects/<slug>/` or
`$CHAOS_PROJECT_DIR/<slug>/` — because no single repo's `docs/` can own a multi-repo page. All
member repos must share one embedder config; `status`/`relink` warn on mismatch.

## Provenance breadcrumbs

Every generated feature artifact (the `add` feature/bug page, the `change-plan` plan, the `impact`
report, and `feature-context` evidence) records **provenance breadcrumbs** — `{ source, method,
detail, locator }` from `src/provenance.rs` — answering *where each piece of information came from*
(git diff, AST/language extraction, Postgres queries, file reads, embedding cosine, or a prior
feature manifest). They render as a "How this was generated" panel and ride along in the compact MCP
returns. Retrieval hits also carry `metadata.retrieved_by` (semantic/keyword/literal). New
extractions additionally **correlate with previously generated feature pages**: `add` links a change
to overlapping pages (`related_features`) and `change-plan` seeds features from prior manifests
(`via: manifest`). All additive and backward-compatible.

## Exports

```sh
# Interactive HTML graph of nodes/edges
chaos graph /path/to/repo -o graph.html

# Obsidian vault export
chaos obsidian /path/to/repo
chaos obsidian /path/to/repo -o vault
```

`obsidian` also emits god-node community notes (`vault/Communities/*.md` + `Feature Map.md`) and
`docs/features_memory/feature-map.html` from the persisted layers — no re-index, no embedder.

## MCP Server

Run the MCP server over stdio (newline-delimited JSON-RPC, **no** Content-Length framing).
Use the release binary directly:

```sh
target/release/chaos --config chaos-substrate.toml mcp
```

Exposes exactly 15 tools: `chaos_analyze`, `chaos_add`, `chaos_stats`, `chaos_query`,
`chaos_feature_context`, `chaos_impact`, `chaos_write_feature_website`, `chaos_obsidian`,
`chaos_refresh`, `chaos_write_storyboard`, `chaos_change_plan`, `chaos_components`, `chaos_features`, `chaos_project`, `chaos_help` (see README.md "MCP Tools" for the
full reference).

Validate the server responds with a single JSON line:

```sh
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"runbook","version":"0"}}}' \
  | target/release/chaos --config chaos-substrate.toml mcp
```

A correctly configured server prints one JSON-RPC response line to stdout.

## Editor Install

Auto-detect installed editors (Claude Code / Codex / Cursor / Windsurf / OpenCode) and register
chaos-substrate as an MCP server in each (merge-not-clobber):

```sh
chaos setup --dry-run                 # show what would change, write nothing
chaos setup                           # apply
chaos setup --scope user              # scope: user | local | project
chaos setup --scope project
```

Per-editor manual setup details: see `docs/EDITOR_SETUP.md`.

## Plugin Hook

`chaos hook` is the Claude Code / Cursor plugin hook. It reads event JSON on stdin and injects
code-memory context for Grep/Glob/Bash tool calls. It always exits 0 and is a safe no-op when the
DB/index is unavailable (no embedder dependency).

```sh
chaos hook --event PreToolUse
chaos hook --event PostToolUse
chaos hook --event PreToolUse --format claude     # format: claude | cursor
chaos hook --event PreToolUse --format cursor
```

Normally invoked by the editor, not by hand (the plugin ships `.claude-plugin/hooks/hooks.json`
and `.cursor/hooks.json`).

## Troubleshooting

- **Embedder not configured / analysis fails.** This is by design (fail-closed — no fake
  vectors). Configure a real embedder in `chaos-substrate.toml`:
  - OpenAI: `text-embedding-3-small` (1536 dims), needs `OPENAI_API_KEY`.
  - Ollama: `nomic-embed-text` (768 dims), `base_url http://localhost:11434`
    (committed default). Ensure the model is pulled: `ollama pull nomic-embed-text`.
  Re-run `chaos doctor` to confirm the embedder probe passes.

- **Postgres not reachable.** Confirm the container is up and the port is published:

  ```sh
  docker compose up -d
  docker compose ps
  ```

  Verify `DATABASE_URL` (or config) points at `postgres://chaos:chaos@localhost:54329/chaos_substrate`.
  `DATABASE_URL` overrides the config file when set.

- **Schema / migration issues.** Re-run migrations; they are tracked in `_sqlx_migrations`:

  ```sh
  chaos migrate
  ```

- **General health check.** `chaos doctor` probes database connectivity, schema/migrations,
  and the configured embedder. Run it first whenever something misbehaves.

- **Diagnostics vs. results.** Diagnostics (tracing) go to **stderr**; program results go to
  **stdout**. When capturing output, keep the streams separate.

## Validation (development)

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```
