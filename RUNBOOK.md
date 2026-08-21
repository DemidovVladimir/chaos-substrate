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

## Fresh machine (zero to running)

Install the system prerequisites once, in this order:

```sh
# 1. Docker — runs the bundled Postgres + pgvector.
#    macOS: brew install --cask docker (or Docker Desktop from docker.com)
#    Linux: Docker Engine + compose plugin — https://docs.docker.com/engine/install/
docker compose version          # must succeed

# 2. Rust toolchain — the runtime is a single Rust binary.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
cargo --version                 # must succeed

# 3. Clone and bootstrap everything in one shot:
#    builds the release binary, starts Postgres, installs/starts Ollama and
#    pulls embeddinggemma, runs `chaos migrate`, then `chaos doctor`.
git clone https://github.com/chaos-substrate/chaos-substrate.git
cd chaos-substrate
cp chaos-substrate.example.toml chaos-substrate.toml
bin/chaos bootstrap
export PATH="$HOME/.local/bin:$PATH"    # `chaos` wrapper on PATH
```

Variants: OpenAI embeddings instead of Ollama (uncomment `open_ai` in the config, comment out
`ollama`, `export OPENAI_API_KEY=...`); external Postgres instead of Docker (`CHAOS_NO_DOCKER=1`
plus your own `DATABASE_URL`).

Then register the agent integration — see [Editor Install](#editor-install) below for Claude Code
and Codex (and Windsurf / OpenCode).

The manual step-by-step equivalent of `bin/chaos bootstrap` follows.

## Bootstrap

```sh
# 1. Start bundled Postgres + pgvector (pgvector/pgvector:pg16, host port 54329)
docker compose up -d

# 2. Provide a config (committed default targets Ollama)
cp chaos-substrate.example.toml chaos-substrate.toml   # if you keep an example; otherwise edit chaos-substrate.toml

# 3. Apply database migrations (sqlx::migrate!, tracked in _sqlx_migrations)
#    Includes the layered memory: 002_communities (L1 god-nodes),
#    003_subtree_hash (L2 Merkle rollup), 004_community_summary (L3 summaries),
#    005_projects (cross-repo projects), 006_summary_cache (summary reuse cache),
#    007_fk_indexes (covering FK indexes), 008_identifier_tokens (identifier-split
#    keyword search — backfills from stored text, no re-analyze), and
#    009_project_docs (docs-only project members).
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
(the same holds for GraphQL type/fragment edges, which resolve only within the changed-file batch)
— run `chaos analyze` for a full graph rebuild. Like `analyze`, it requires a
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

## Stack

```sh
# Report the tech stack of an already-indexed repository (read-only, no embedder)
chaos stack /path/to/repo
chaos stack /path/to/repo --output-html out.html   # default: docs/features_memory/stack.html
```

Lists (not just counts) what the repo is built with, read from the persisted index:
manifest-declared dependencies by ecosystem (npm/cargo — versions, runtime-vs-dev scope, how many
workspace manifests declare each, widest-declared first), npm scripts, deployment resources (AWS
CDK app entrypoints, Stack classes, L2 constructs grouped by cloud service), indexed JS/TS configs,
the repo's exposed **API surface** (HTTP routes with method + path, GraphQL root fields grouped
`Query.`/`Mutation.`/`Subscription.`, and CLI commands — all from persisted user-surface nodes),
and the file-language breakdown. Always writes an interactive HTML inventory (every entry) and
prints a compact JSON summary (capped lists with `*_omitted` counts). The output states its
coverage explicitly — Dockerfiles, CI workflows, pyproject.toml, foundry.toml and Terraform are not
indexed yet, and the GraphQL rows are SDL-derived only (code-first schemas are named as a gap
rather than silently omitted).

## Pages

```sh
# List the generated feature-memory pages — what chaos has already extracted
chaos pages /path/to/repo
chaos pages /path/to/repo --features-dir ~/.chaos/projects/myapp   # scan a project workspace instead
```

The chaos-native replacement for `ls docs/features_memory`: scans the features directory, recognises
every chaos-generated HTML page by its embedded manifest block, and lists each one with its kind
(`feature` / `story` / `components` / `features` / `composed` / `stack` / `impact` /
`change-plan` / `feature-map`), the tool that writes that kind, its title, and its modified time, newest first, plus
by-kind counts. HTML files without a recognised block are listed as `other` — nothing is hidden.
Read-only and embedder-free; the repo argument is resolved against the index first, but a plain
directory path works even if unindexed (the scan is pure filesystem). Use it to check whether a
feature was already extracted before running a new deep-dive.

## Gaps

```sh
# What can code retrieval NEVER find in this repo?
chaos gaps /path/to/repo
chaos gaps /path/to/repo --folder apps/processor    # scope to a sub-app of a monorepo
chaos gaps --project molecule                       # scan EVERY member repo of a project
```

Lists the knowledge gaps of an indexed repository, in two kinds: `coverage_gaps` (files that
produced **no** chunks — invisible to every retrieval method; re-add them, and report a chunking
bug if they stay empty) and `vocabulary_gaps` (chunked code whose indexed text carries too little
distinctive vocabulary to match any meaningful query — single-letter names, abbreviation soup, no
docstrings). Corpus-driven and deterministic: the background vocabulary is derived from the repo's
own document frequencies, never a hardcoded stop list. Read-only, embedder-free, compact output
with per-file evidence samples. The fix for a vocabulary gap is repo content — write a file-top
docstring or folder README saying what the file is for, then `chaos add` those paths; never pause
indexing waiting for it.

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
a feature maps onto the codebase as it is today (the "before"). Like `feature-context` (which always
writes `docs/features_memory/<slug>-context.html`, overridable with `--output-html`), `impact`
always produces the page.

## Usage

```sh
# Who consumes X across the repo's subfolders?
chaos usage /path/to/repo "DATABASE_URL"
chaos usage /path/to/repo "x-service-token"
chaos usage /path/to/repo "Query.user"          # or the bare field name: "user"
chaos usage /path/to/repo "merge_files_index" --limit 10
```

Answers "who uses this?" for a symbol or surface string — an env var, an HTTP header, a route, a
GraphQL field, a function — grouped by top-level subfolder, entirely from the persisted index (the
chaos-native replacement for `rg`/`grep` on the target repo). Three embedder-free sources: the
user-surface nodes (`env_var` / `http_route` / `cli_command` / `graphql_field`; a bare GraphQL
field name matches qualified nodes by suffix, so `user` finds `Query.user`), the reverse graph
edges (`calls` / `imports` / `uses_type` / `implements` / `tests` / `depends_on`), and a literal
chunk sweep as the cross-language catch-all. **Always** writes an interactive HTML report to
`docs/features_memory/<slug>-usage.html` and prints a compact per-folder summary (capped lists
with `sites_omitted` counts). Honest limitation, surfaced as a warning: call/import edges resolve
cross-file only for repo-unique names.

## Sui Migration Impact

```sh
chaos sui-migration-impact /path/to/repo
chaos sui-migration-impact /path/to/repo --source ethereum
chaos sui-migration-impact /path/to/repo --source auto --output-html out/sui-plan.html
chaos sui-migration-impact /path/to/repo --source mixed --limit 12
```

Produces a Sui migration impact report for an indexed Ethereum, Solana, or mixed Web3 repo.
Auto-detects the source stack by default (`--source auto`); override with `ethereum`, `solana`, or
`mixed`. Maps each L1 feature onto Sui primitives — objects/dynamic fields, Coin/Kiosk/Display,
capabilities, PTBs, events+GraphQL — with Walrus/Seal storage and access-control verdicts, each
citing the compiled-in Sui official docs profile. **Always** writes
`docs/features_memory/sui-migration-impact.html` and prints a compact JSON summary. Read-only and
embedder-free. Maps impact only — does not generate Move code.

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
  Chaos never fakes screens — a frame with no preview renders text-only (no mockup, no placeholder).
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
every entry-layer feature); any other phrase is first tried as a layer **by meaning** (embedding
cosine against per-layer prototype phrasings — "backend", "client app", "devops" resolve
semantically, no keyword list; "backend" spans interface+core), then falls to a **topic** match;
omit it for the whole repo. Force it with `--layer`/`--folder`/`--topic`. Exact layer words, folders
and whole-repo listing are embedder-free; semantic layer routing and topic matching use the
embedder. **Always** writes an interactive HTML inventory to
`docs/features_memory/<slug>-features.html` and prints a compact JSON summary (resolved filter + how
detected, per-layer + language counts, per-feature label/role/folders/symbols/`matched_by`,
provenance). `--limit 0` (default) returns everything.

## Compose

```sh
chaos compose /path/to/repo --sections features,correlations,stack --persona "a beginner engineer new to this stack"
chaos compose /path/to/repo --sections features --level expert --style blade-runner --brand-preset molecule
chaos compose /path/to/repo --sections features,stack --filter desci-infra --feature-pages
```

Composes **one** page (or, with `--feature-pages`, a clickable per-feature site under
`<slug>-composed/`) from knowledge-base-backed sections instead of several similar standalone
pages: `features` (the inventory with each feature's concise L3 explanation), `correlations`
(files shared between those features plus prior generated pages that overlap them), and `stack`.
The audience is a free-text `--persona` resolved to beginner/practitioner/expert **by meaning**
(prototype embeddings; `--level` is the embedder-free path), and the look is a style preset
(`editorial` light default, `blade-runner` dark neon) plus an optional `--brand-preset`. Every
section resolves from the persisted index and prior manifests only — a section it cannot serve
(repo not indexed, no L1 hierarchy, unknown section/style) is a loud error naming the fix. The
composition is content-hashed: re-composing the same request over unchanged knowledge returns
`cached: true` without writing. The page lands at `docs/features_memory/<slug>-composed.html`
with an embedded `chaos-composed-manifest`.

## Projects (cross-repository)

```sh
chaos project create molecule
chaos project add-repo molecule /path/to/client --alias client     # repo must be indexed
chaos project add-repo molecule /path/to/contracts --alias contracts
chaos project add-docs molecule /path/to/workspace --alias docs    # project-level docs (ADRs, design notes)
chaos project list                     # projects + EVERY indexed repo (the discovery call)
chaos project status molecule          # members, link staleness, links by kind, embedder check
chaos project relink molecule          # hash-gated; --force to override
chaos features --project molecule      # every member repo's features in ONE layered inventory
chaos features --project molecule client   # …filtered (same auto-detection as single-repo)
```

A **project** groups indexed repositories (client, backend, smart contracts, infra, …) and maintains
**feature→feature cross-repo links** between them, detected from the persisted index only
(consumer → provider): `package_dep` (a manifest `name` one repo publishes is imported by another),
`abi` (non-Solidity code references a contract/interface defined in another repo), `http_route` (a
fetch/axios call path matches a route registered elsewhere; params normalize to `*`; the provider
side anchors on persisted `HttpRoute` surface nodes unioned with the chunk scan — nodes win on a
shared path, scan-only registrations still anchor), and `graphql` (an executable GraphQL operation in one repo selects
a root field another repo's SDL schema defines; operation types must agree, and code-first servers
expose no provider facet yet). Links attach at the feature (L1) level with evidence + provenance
breadcrumbs and live in `cross_repo_links` (`migrations/005_projects.sql`).

`add-docs` indexes a directory of **project-level documentation** — the cross-repo design notes,
ADRs, and migration spikes no single member repo owns — as a docs-only member through the normal
pipeline. The directory may sit *above* the member repos (e.g. the workspace root): nested member
repos are pruned from the walk, and re-running on the same directory is an idempotent refresh
(`migrations/009_project_docs.sql` marks docs members so they don't count as code repos).

The project layer follows the same layered pipeline as L1–L3: **every `analyze`/`add` on a member
repo ends by relinking its projects**, gated by the L2 repo root hash
(`project_repos.linked_repo_hash` vs `repositories.repo_root_hash`) — a no-change re-index relinks
nothing, and `add-repo` always links the new member (its gate hash starts NULL). The project-wide
feature inventory is written to the project workspace — `~/.chaos/projects/<slug>/` or
`$CHAOS_PROJECT_DIR/<slug>/` — because no single repo's `docs/` can own a multi-repo page. All
member repos must share one embedder config; `status`/`relink` warn on mismatch.

## Feature story (cross-repo)

```sh
chaos feature-story molecule "lab tokenization and access control"
chaos feature-story molecule "membership invites" --style blade-runner --brand-preset molecule
```

Tells the cross-repo story of **one** feature across a project — the focused counterpart to
`chaos features --project` (which inventories *all* features). It matches the feature in every
member repo (L1 community semantic search plus a lexical label fallback), traverses the persisted
cross-repo links — pulling in a link's other endpoint (e.g. the Solidity contract a client calls)
even when the query didn't match it directly — and orders the involved features into a
journey-layer spine (entry → interface → core → foundation = client → backend → contracts).
Features whose identity the indexed docs mark as replaced land in a separate "Legacy / superseded"
band instead of being interleaved with their replacements. Writes a clickable multi-page site to
the project workspace (an index page plus one hash-gated drill-down page per involved feature) and
prints a compact summary — involved repos, the ordered link chain, links by kind, repos not
involved, provenance, `content_hash`. Deterministic and embedder-light: one embed for the whole
query, reused across repos.

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

# Serve the graph with LIVE semantic search — a validation surface for the
# retrieval pipeline. The page gains a "Semantic search" panel that calls
# http://127.0.0.1:7878/api/search, which runs the SAME hierarchical
# retrieval the agent tools use (real embedder → L1 community routing →
# hybrid semantic/keyword/literal chunk search) and highlights the hits on
# the graph with cosine scores and retrieved_by badges. Embedder down =
# loud error, never a substring fallback. The sidebar's plain "Filter
# nodes" box stays a substring filter.
chaos graph /path/to/repo --serve            # default port 7878
chaos graph /path/to/repo --serve --port 9000

# Obsidian vault export
chaos obsidian /path/to/repo
chaos obsidian /path/to/repo -o vault
```

`obsidian` also emits god-node community notes (`vault/Communities/*.md` + `Feature Map.md`) and
`docs/features_memory/feature-map.html` from the persisted layers — no re-index, no embedder.

The vault layout is `README.md` (counts + links), `Topics/` notes, `Nodes/` notes (one per graph
node: source file + line range, node kind + stable id, chunk count, outgoing/incoming relationships,
raw metadata JSON), `Edges.md` (the relationship manifest), and `.obsidian/` defaults. For a large
repo, start from `Topics/` rather than the global graph view. The standalone `graph.html` export is
the lighter alternative: pan/zoom, filter by node kind, search by name/path/stable-id, click a node
for its source metadata; it runs no web server and calls no embedder (the `--serve` panel does call
the real retrieval pipeline). Quick sanity check on what was extracted:

```sql
select kind, count(*) from edges group by kind order by kind;   -- contains / imports / depends_on / calls / defines / configures / deploys
```

## MCP Server

Run the MCP server over stdio (newline-delimited JSON-RPC, **no** Content-Length framing).
Use the release binary directly:

```sh
target/release/chaos --config chaos-substrate.toml mcp
```

Exposes exactly 24 tools: `chaos_analyze`, `chaos_add`, `chaos_stats`, `chaos_stack`, `chaos_pages`, `chaos_gaps`, `chaos_query`,
`chaos_feature_context`, `chaos_impact`, `chaos_usage`, `chaos_sui_migration_impact`, `chaos_write_feature_website`, `chaos_obsidian`,
`chaos_refresh`, `chaos_write_storyboard`, `chaos_change_plan`, `chaos_components`, `chaos_features`, `chaos_compose`, `chaos_project`, `chaos_feature_story`, `chaos_help`, `chaos_clean`, `chaos_graph` (see README.md "MCP Tools" for the
full reference).

Validate the server responds with a single JSON line:

```sh
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"runbook","version":"0"}}}' \
  | target/release/chaos --config chaos-substrate.toml mcp
```

A correctly configured server prints one JSON-RPC response line to stdout.

## Editor Install

Auto-detect installed editors (Claude Code / Codex / Windsurf / OpenCode) and register
chaos-substrate as an MCP server in each (merge-not-clobber):

```sh
chaos setup --dry-run                 # show what would change, write nothing
chaos setup                           # apply
chaos setup --scope user              # scope: user | local | project
chaos setup --scope project
```

Claude Code — full plugin (skill + 24 MCP tools + hooks) or MCP server only:

```sh
claude --plugin-dir /abs/path/to/chaos-substrate     # plugin, local testing
# real install: add .claude-plugin/marketplace.json, install from the /plugin UI

bin/chaos claude-code-add local                              # MCP only, machine-local
bin/chaos claude-code-add project /abs/path/to/target-repo   # MCP only, shareable .mcp.json
```

Codex — plugin marketplace or MCP server only:

```sh
codex plugin marketplace add /abs/path/to/chaos-substrate    # reads .agents/plugins/marketplace.json
# restart Codex, enable chaos-substrate from the plugin UI

codex mcp add chaos-substrate -- /abs/path/to/chaos-substrate/target/release/chaos \
  --config /abs/path/to/chaos-substrate/chaos-substrate.toml mcp
```

Per-editor manual setup details (incl. Windsurf / OpenCode), the plugin packaging/marketplace flow,
and Claude Desktop / Cowork: see `docs/EDITOR_SETUP.md`.

## Plugin Hook

`chaos hook` is the Claude Code plugin hook. It reads event JSON on stdin and injects
code-memory context for Grep/Glob/Bash tool calls. It always exits 0 and is a safe no-op when the
DB/index is unavailable (no embedder dependency).

```sh
chaos hook --event PreToolUse
chaos hook --event PostToolUse
```

Normally invoked by the editor, not by hand (the plugin ships `.claude-plugin/hooks/hooks.json`,
whose launcher `.claude-plugin/hooks/chaos-hook.sh` self-locates the binary and no-ops silently when
it is unavailable).

## Troubleshooting

- **Embedder not configured / analysis fails.** This is by design (fail-closed — no fake
  vectors). Configure a real embedder in `chaos-substrate.toml`:
  - OpenAI: `text-embedding-3-small` (1536 dims), needs `OPENAI_API_KEY`.
  - Ollama: `embeddinggemma` (768 dims), `base_url http://localhost:11434`
    (committed default). Ensure the model is pulled: `ollama pull embeddinggemma`.
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

**Functional smoke test.** Index a repo containing Rust/Solidity/TS/JS/Python/GraphQL, run a query, export
`graph.html` and confirm it shows the persisted nodes/edges, exercise the MCP server over stdio, and
verify vectors survive a process restart. The MCP server speaks **newline-delimited JSON-RPC**, not
Content-Length-framed LSP messages.

**Token-efficiency invariant.** A second `analyze` of unchanged code must make zero embedder calls:
the output reports `embedded_chunks: 0`, `reused_embeddings: <all>`, and `summaries.embed_calls: 0`;
a second `project relink` reports `up_to_date`.

**Persistence checks (Postgres).**

```sql
select count(*) from repositories; select count(*) from files; select count(*) from nodes;
select count(*) from edges; select count(*) from chunks;
select provider, model_id, dimensions, count(*) from embeddings group by provider, model_id, dimensions;
```

The schema enforces `embeddings.dimensions = vector_dims(embedding)`, so an embedder whose output
dimension disagrees with the configured value is rejected rather than stored. The Ollama path uses
`/api/embed` with `{model, input}` and reads the first vector from the `embeddings` field (not the
legacy `/api/embeddings` endpoint).

**Code-review focus.** Watch for schema drift between `src/models.rs`, the `src/storage/` modules,
and `migrations/001_init.sql`; AST span-to-line (`LineIndex`) offset errors; parser failures
(`syn`/`oxc`/`rustpython`/`solang`; `apollo-parser` never fails, it leaves a partial tree)
degrading gracefully (warn + whole-file fallback chunk, never abort the run); and the
query path validating provider/model/dimensions before searching.
