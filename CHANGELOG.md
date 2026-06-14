# Changelog

All notable changes to Chaos Substrate are documented here. Versions before
0.12.0 predate this file; see the git history (`P0`–`P5` commits) for the
hierarchical-memory build-out.

## 0.20.0 — 2026-06-14

The retrieval-quality + Sui-impact release, and the point where the accumulated
0.16–0.19 working tree was consolidated onto `main` as the single source of
truth. Headline: identifier-aware keyword tokenization (migration 008) and the
acronym-aware hierarchical router make camelCase code reachable from
natural-language queries; two new read-only surfaces (`chaos gaps`,
`graph --serve`) validate and harden retrieval; and a 22nd tool,
`chaos sui-migration-impact`, maps a Web3 repo's features onto Sui primitives.
Migration-only upgrade for the index changes: run `chaos migrate` for the
identifier tokens (no re-analyze); a `chaos analyze` is only needed for the
0.19 chunk/community shapes if you skipped that release.

### Added — `chaos sui-migration-impact` / `chaos_sui_migration_impact`: Sui migration impact report (22nd tool)

- A read-only, embedder-free report that answers "if this project moves to Sui,
  which existing features are affected, which Sui primitives map to them, and
  what should be reviewed first?". It detects the SOURCE Web3 stack from the
  persisted index (Solidity/Hardhat/OpenZeppelin/ERC, Anchor/SPL/Metaplex/PDAs,
  ethers/viem/wagmi/@solana clients, IPFS/Arweave/S3 storage, encryption/
  token-gating) plus disk probes for toolchain configs; maps the evidence files
  onto the L1 feature communities; triggers source-pattern → Sui concept
  mappings (objects/dynamic fields, Coin/Kiosk/Display, capabilities, PTBs,
  package upgrades, events+GraphQL); classifies storage flows (Walrus blob /
  Walrus+Seal / keep-offchain / review) with explicit Seal verdicts including
  `not-needed`; correlates prior generated feature pages; and proposes a review
  order. Every mapping cites the compiled-in `sui-official` docs profile
  (`src/sui_docs.rs` — official Sui / Walrus / Seal pages with URL + verified
  date) via a new `docs-profile` provenance bucket. Evidence-triggered only —
  no signal, no claim. ALWAYS writes
  `docs/features_memory/sui-migration-impact.html` and returns a compact JSON
  summary (capped lists with `*_omitted` counts). It MAPS IMPACT: it generates
  no Move code and makes no correctness claims. (`examples/sui-migration-demo`
  ships a minimal Hardhat+OpenZeppelin+ethers fixture with a sample report.)

### Changed — L3 summaries v4, acronym routing, embedder robustness

- Community summaries v4 (`SUMMARY_ALGO_VERSION` 4): key symbols are also
  rendered AS WORDS ("In words: list all on chain labs; …") and labels are
  camel-split — the embedding-side counterpart of migration 008, so features
  named in camelCase embed near natural-language queries. One-time full
  re-summarize on the next analyze.
- Hierarchical router: label routes are now ADDITIVE to cosine routes (was:
  fallback only when cosine found nothing), and short queries (2–4 words)
  contribute their ACRONYM as a label token — "on chain labs" → `ocl` →
  routes `ocl-repository`/`ocl-*` features no embedder could reach. Bounded
  by the existing LABEL_ROUTE_LIMIT, deduped against cosine routes.
- Hierarchical retrieval re-rank: the flat pool is fetched 3× wider than the
  requested limit and truncated AFTER boosting (a boost can only promote a
  hit that exists); short-phrase ACRONYMS are also literal-search terms and
  files whose path segment IS the acronym get a 2× boost inside the flat
  pipeline; the final hierarchical return collapses same-file duplicates
  toward the tail (a landscape surface should span files, not return two
  chunks each of the few strongest docs). Literal budget raised to half the
  candidate pool. Net effect, validated live: "on chain labs" went from one
  folder's doc headers to 25 distinct OCL-related files spanning
  onchainlabs + desci-ecosystem + desci-infra, with desci-infra's
  ocl-repository feature routed. KNOWN GAP: the ocl-processor lambda's
  chunks still score below peer ocl files for the broad phrase (targeted
  queries hit it #1) — needs per-hit score instrumentation of the
  merge/rerank arithmetic, and L1 membership quality (the lambda clusters
  into a generic utils community, so feature boosts miss it).

- Embedding client: HTTP timeout 60s → 300s and batch concurrency 2 → 1.
  Observed live: a local Ollama serializes requests, so concurrent batches
  plus timeout-retries created a self-inflicted thundering herd that timed
  out full analyzes ("operation timed out" while the server was healthy).

### Added — identifier-aware keyword tokenization (migration 008)

- Code carries most of its meaning in compound identifiers, but Postgres FTS
  treated `listAllOnChainLabs` as ONE lexeme, so a query for "on chain labs"
  could never keyword-match the code implementing it. Migration
  `008_identifier_tokens.sql` adds `chaos_identifier_text()` (SQL, immutable:
  splits camelCase/PascalCase/ACRONYMWord boundaries and `_-.` separators)
  and rebuilds every chunk's `search_vector` as original content PLUS the
  split rendering — both vocabularies match. `insert_chunk` uses the same
  expression for new chunks. **`chaos migrate` is the only step** — the
  backfill recomputes from already-stored text: no re-analyze, no
  re-embedding, embedder-free. Validated live: "ocl processor" now returns
  the OCL processor lambda's handler as hit #1 via all three retrieval
  methods; "on chain labs" keyword-reaches code that only says `OnChainLab`.

### Added — `chaos gaps` / `chaos_gaps`: knowledge-gap detection

- Retrieval can only be as good as the words a file brings to the index. The
  new read-only, embedder-free surface (`src/gaps.rs`) flags two kinds of
  unfindable code: `vocabulary_gaps` — chunked files with almost no
  DISTINCTIVE vocabulary left after identifier splitting (background words
  are derived from the repo's own document frequencies, not a hardcoded stop
  list); and `coverage_gaps` — files that produced NO chunks at all,
  invisible to every retrieval method. The fix for a vocabulary gap is repo
  content (file-top docstring or folder README, then `chaos add` on those
  paths) — indexing is NEVER paused to ask; a coverage gap is a chunking
  finding to re-add or report. Scopes match the rest of the surface: `repo`
  for one repository, `repo` + `folder` for a sub-app inside a
  monorepo-indexed repo (background vocabulary still comes from the whole
  corpus), or `project` for every member repo of a cross-repo project in one
  repo-tagged report (background stays per member — contracts boilerplate is
  not client boilerplate). First run on molecule_core: 272 code files with
  zero chunks (mostly configs, some e2e specs, and 300+-line GraphQL query
  files under desci-infra) and 2 docstring candidates — the no-chunks count
  is itself a chunking-coverage signal.

### Fixed — literal retrieval: folder flooding and top-of-file bias

First findings from the `graph --serve` validation surface (searching "ocl" /
"on chain labs" on molecule_core):

- `literal_search` scored a path match 1.5 vs a content match 0.35 and
  tie-broke by line number, so a term matching a whole folder's path (e.g.
  `onchainlabs/` for "onchainlabs") flooded every slot with that folder's
  line-1 chunks — env-var declarations and doc headers shadowed the actual
  logic, and content-matching files OUTSIDE the folder never surfaced. Now:
  path and content matches weigh equally (0.75 each), at most 2 hits per
  file (content-matching chunks preferred within a file), ordered
  deterministically.
- The per-term literal budget was a fixed 12 hits (vs 40–200 candidates for
  the semantic/keyword methods); it now scales with the candidate pool
  (24–50). The reranker can only promote candidates that exist.

Known remaining gap (by design of chunk-level retrieval, documented for the
next calibration pass): identifier vocabulary does not tokenize — a chunk
saying `listAllOnChainLabs` matches neither FTS keyword search for "on chain
labs" (one lexeme) nor the embedder strongly; identifier-aware tokenization
at index time is the structural fix.

### Added — `graph --serve`: live semantic search in the graph page

- `chaos graph <repo> --serve [--port 7878]` serves the interactive graph
  from a localhost HTTP server (`src/graph_serve.rs`, no new crates — tokio
  `net` only) and adds a **Semantic search** panel to the page. The panel's
  `GET /api/search?q=…` runs the SAME hierarchical retrieval pipeline the
  agent tools use (`query_repo_hierarchical`: real embedder → L1 community
  routing over L3 summary embeddings → hybrid semantic/keyword/literal chunk
  search with merge + rerank) and renders the result as a validation
  surface: matched features with cosine scores, ranked chunk hits with
  `retrieved_by` badges, and score-sized halos on the graph nodes (non-hits
  dim; hits stay rendered even when kind/substring filters would drop them;
  clicking a hit focuses its node). `GET /api/health` reports the embedder;
  a static `file://` export keeps today's behavior and shows a hint to run
  `--serve`. An embedder/database failure is a loud in-page error — never a
  substring fallback. The sidebar's existing "Filter nodes" box remains a
  plain case-insensitive substring filter (it matches `ocl` inside
  `LogoCloudItem`); the semantic panel is the meaning-based counterpart.

### Fixed — parallel-test flake in the summary hash-gate

- `summary_hash_gate_skips_unchanged_communities` could fail under the default
  multi-threaded `cargo test` (never in isolation): its `two_cluster_fixture`
  built byte-identical chunk content across tests, so the rolled-up community
  subtree-hashes collided, and the content-addressed `community_summary_cache`
  (which deliberately survives repo wipes) could be pre-populated by a
  concurrent test — making the "first pass embeds everything" assertion see a
  cache hit (`embed_calls 1 != total 2`). The fixture now folds the per-test
  `repo_id` into the chunk content, so each test's hashes are unique and the
  cache can no longer bleed between tests. Whole suite is green under the
  default parallel runner.

## 0.19.0 — 2026-06-12

The graph-deflation + smart-chunking release: the index stops over-producing
nodes/edges/communities, and chunks follow code/document STRUCTURE instead of
blind 2,000-char slices. **Run a full `chaos analyze` on existing repos to get
the new shapes** — until then old indexes keep answering queries unchanged.
The first re-analyze after upgrading re-embeds most chunks once (headers and
line stamps change content hashes) and re-summarizes communities once; both
are hash-gated afterwards as usual.

### Changed — tiny-community consolidation (detection v2)

- Louvain can never merge zero-edge nodes, so every isolated file became its
  own singleton "community" (molecule_core: 1,212 communities, only 287 of
  them real features). A deterministic post-Louvain pass now folds
  sub-threshold fragments (< 4 members) into real communities: connected
  fragments merge into their folder-preferred best-coupled neighbor, isolates
  fold into the folder-dominant community (deepest ancestor first), and
  no-match isolates stay singletons (already filtered by `member_count >= 2`).
- `DETECTION_VERSION` bumped to 2; `detection_params` records
  `merge: {min_size, merged}`. One-time community-id churn on the first
  re-detect (ids are UUIDv5 of the min-member stable_id; absorbing a
  lower-sorting member renames the community). `repo_root_hash` is unchanged;
  only communities whose membership changed re-summarize, and cross-repo
  projects relink automatically through the existing hash gate.

### Changed — source-import dedup + external-import filtering (all languages)

- Internal imports now dedupe to ONE node per imported module repo-wide:
  `import:path:{normalized}` for relative specifiers (pure-lexical resolution
  against the importing file, so `./utils` and `../lib/utils` meet at one
  node), `import:alias:{module}` for root-anchored aliases, `import:bare:`
  unchanged, `import:rust:{hash}` for identical `use` statements. Shared nodes
  carry no `file_id`; the per-importer file/line lives on each `Imports` edge.
  (molecule_core had 4,189 dependency nodes — 31% of the graph — mostly
  one-per-import-per-file.)
- External imports are dropped at extraction for Rust (`std`/`core`/`alloc`/
  third-party crates; workspace crate names from indexed `Cargo.toml`s) and
  Python (stdlib/site-packages; repo module roots from top-level `.py` files
  and packages), matching the JS/TS behavior. Solidity npm-style imports
  (`@openzeppelin/...`) now go through the same workspace filter. Declared
  dependency lists still live in the per-manifest nodes (`chaos stack` is
  unaffected).

### Changed — call-edge ambiguity gate

- The global fallback that resolved a callee name across files bound ambiguous
  names (`new`, `run`, `handle`) to whichever file was walked first, gluing
  unrelated features. It now applies only when the name has exactly ONE
  definition in the repo; same-file resolution is unchanged.

### Fixed — quadratic FK cascades made `chaos clean`/`chaos add` crawl

- A repo-scoped `chaos clean` on molecule_core ran 16+ minutes. Cause: the FK
  columns the delete triggers probe (`edges.source_node_id`,
  `edges.target_node_id`, `chunks.node_id`, `chunks.file_id`, `nodes.file_id`)
  were only covered by composite `(repo_id, …)` indexes, which a
  single-column FK lookup cannot use — every deleted node seq-scanned edges
  twice and chunks once, and rows deleted earlier in the same transaction are
  dead but still scannable, so the purge went quadratic (pg_stat: 617M tuples
  read across 178k seq scans on edges). The same cascade cost hit every
  incremental `chaos add` (`files → nodes → edges/chunks` against live rows).
  Migration `007_fk_indexes.sql` adds the five plain FK indexes; cascades are
  btree probes now. Run `chaos migrate` once after upgrading.

### Changed — context paths share the community strength model

- `chaos_query`'s context paths (the "how do these hits relate" routes) now
  traverse edges at `cost / confidence` — the exact inverse of the
  `coupling_weight` used by L1 community detection — so a low-confidence
  heuristic call edge no longer beats a parser-certain route of equal raw
  cost. Edge order is canonicalized before routing (equal-cost routes used to
  tie-break on database row order), and when paths are truncated, CROSS-FILE
  paths are kept ahead of trivial same-file adjacencies. Query-time only — no
  re-index needed.

### Changed — chunking follows structure

- `MAX_CHUNK_CHARS` 2,000 → 6,000 (EmbeddingGemma's 2,048-token window was
  never the constraint at 2,000 chars ≈ 500 tokens). Oversized chunks now
  split at STRUCTURAL boundaries — blank lines, dedents back to the part's
  base indentation — instead of raw character cuts, and every split part
  carries its REAL source line range instead of inheriting the parent's.
- Markdown is parsed (pulldown-cmark) into HEADING SECTIONS: one
  `documentation` chunk per section (depth ≤ 3) with a heading-path header
  (`README.md > Setup > Docker`), real line ranges, and `heading_path`
  metadata; the preamble is its own section; files without headings keep the
  single whole-file chunk. Previously 96–99% of documentation chunks were
  blind 2,000-char slices. Sections are chunks only — no new graph nodes.
- OVERSIZED sections are packed at markdown BLOCK boundaries instead of
  falling through to the generic splitter (which happily cut inside a
  GraphQL fence or mid-table — observed live in `labs-api.md` / `ipt.md`):
  whole blocks (paragraphs, fenced code, tables, lists) are packed greedily;
  a fence bigger than the cap is split at blank lines INSIDE it and each
  piece re-wrapped in fences with the original language tag; an oversized
  table repeats its header + separator rows on every slice; sub-headings
  start a new part once one is ~60% full. Every part keeps the full
  heading-path context header (`Documentation: file > path (part i/n)`) and
  exact line ranges — no more `Symbol: unknown` orphan fragments.
- Oversized PDF text is packed at page (`\f`) and paragraph boundaries with a
  `PDF document: file (part i/n)` header and a `pages` range in metadata when
  the extractor emitted form feeds. Documentation/PDF chunks now never pass
  through the generic splitter.
- Rust impl METHODS are first-class: each `ImplItem::Fn` becomes a `method`
  node + chunk (stable_id `{file}:method:{Impl}::{name}`), contained by its
  impl and registered for call resolution; the impl chunk shrinks to header +
  consts/types + a method roster (one impl here used to split into 46 blind
  parts). Inline `mod` items are extracted individually with a `tests::`-style
  stable_id prefix; module chunks shrink to their header. All Rust symbol
  line ranges now come from syn spans (doc comments included) instead of text
  search.
- TS/JS class chunks no longer duplicate method bodies (methods were already
  separate symbols): header + fields + a method roster.

### Added — user-surface extraction (`cli_command` / `http_route` / `env_var` nodes)

The index now captures HOW A USER OPERATES THE PRODUCT as first-class,
parser-certain facts — the raw material a storyboard/usage page needs that
previously only lived in docs:

- **CLI commands** (`cli_command`): clap derive (`#[derive(Parser)]` programs,
  `#[derive(Subcommand)]` variants with their `///` help and
  `#[command(name = …)]` overrides) and builder (`Command::new`), commander/
  yargs-style `.command('name …')` in JS/TS, argparse `add_parser` (with
  `help=`) and click `@cli.command()` in Python.
- **HTTP routes** (`http_route`, named `METHOD /path`): framework-shaped
  registrations only, mirroring the linker's provider markers so axios clients
  don't masquerade as servers — `app/router/fastify/server.get('/x')` in JS/TS,
  FastAPI `@app.get("/x")` and Flask `@app.route("/x", methods=[…])` in Python,
  axum `.route("/x", get(h))` and actix/rocket `#[get("/x")]` in Rust.
- **Environment variables** (`env_var`): `std::env::var` / `env!` /
  `option_env!`, `process.env.X` / `process.env["X"]`,
  `os.environ["X"]` / `os.environ.get` / `os.getenv` — with the access path
  recorded in metadata.
- Nodes are PER-FILE (`{path}:env:{VAR}`), never repo-wide hubs: a shared
  `DATABASE_URL` node would glue unrelated files into one Louvain community
  (the god-node failure mode the bare-import drop already solved). Entrypoints
  attach via `Defines` (new `DEFINES_ENTRYPOINT` weight, parser-certain), env
  reads via `Configures` (new `READS_ENV`). Each node carries a typed chunk
  (`User surface: …`), so operator questions rank them in retrieval.
- Journey layering now uses the node kind as a stronger signal than folder
  names: `cli_command` → entry, `http_route` → interface.
- New `src/user_surface.rs` owns the shared entry shape, the emitter, and the
  syn-based Rust collector (with `proc-macro2` `span-locations` for real line
  numbers); the oxc/rustpython collectors live in their language modules.
- Purely additive: kinds are stored as text, no migration; older indexes are
  unaffected. Verified live on this repo: 37 `cli_command` + 15 `env_var`
  nodes, and "environment variable CHAOS_PROJECT_DIR project workspace
  directory" now returns the `env_var` chunk as the top hit.

## 0.18.0 — 2026-06-11

The composed-site release: `chaos_compose` is now THE page-generation surface —
when a user asks for a webpage/website/interactive info page, agents route it
here instead of stitching the side-pages of other tools — and it can build a
whole static SITE, not just one page.

### Added — site mode (`feature_pages: true` / `--feature-pages`)

- The composed index's feature cards become CLICKABLE links to one page per
  feature, written under `docs/features_memory/<slug>-composed/` with readable
  slugged file names.
- Each per-feature page shows, all from the persisted graph: the feature's
  code/files (symbols table, key files; expanded for experts, collapsed for
  beginners), its QUOTIENT-GRAPH RELATIONS to the rest of the stack (direction,
  kind, weight; in-scope neighbours cross-linked to their own pages,
  out-of-scope ones honestly labelled; Solidity neighbours tagged as **smart
  contracts**), prior generated pages that overlap it, and a deterministic
  persona-adapted WALKTHROUGH (What this is / Where it sits / How it connects /
  Where the code lives). Every page carries an honesty note: the walkthrough
  describes real structure, not invented user journeys — `chaos_write_storyboard`
  remains the tool for UX storyboards with real screens.
- **Per-page hash gating.** Every page — index and per-feature alike — embeds
  its own `chaos-composed-manifest` with its own `content_hash`. The index hash
  covers all per-page hashes; the return reports `written` vs `cached` page
  counts, so an agent never re-ingests an unchanged page. Verified live on
  molecule_core/desci-infra: 45 feature pages written, then 45 cached / 0
  written on the identical re-run.
- Routing rule encoded in the tool description, agent guide, SKILL.md,
  CLAUDE.md, README: user-facing webpage requests go to `chaos_compose`;
  `chaos_features`/`chaos_stack`/`chaos_components` remain data/inventory
  tools.
- Internals: `feature_inventory::language_tally` made crate-visible (languages
  for out-of-scope relation neighbours from their member files).

## 0.17.0 — 2026-06-11

The composable-page release: instead of generating a bunch of similar
standalone pages, the caller says which sections they need, for whom, and in
which style — and Chaos assembles ONE page from the knowledge base.

### Added — `chaos_compose` MCP tool + `chaos compose` CLI (20 tools)

- **Sections** (`features` — the inventory with each feature's concise L3
  explanation; `correlations` — files shared between those features plus prior
  generated pages that overlap them; `stack`), rendered in request order on one
  page. `filter` scopes features exactly like `chaos_features` (folder | layer
  | topic, auto-detected). Unknown section names are a loud error listing the
  vocabulary.
- **Persona by meaning.** Free-text `persona` ("a very beginner software
  engineer who has no idea about the stack") resolves to
  beginner|practitioner|expert via prototype-embedding cosine — no query-side
  keyword list (`PERSONA_PROTOTYPES`, floor 0.45 with an explicit
  default-to-practitioner warning below it). Explicit `level` is the
  embedder-free path. The level adapts rendering density: beginners get plain
  explanations and read-order hints, experts get symbols/files expanded.
- **Style presets.** `theme.rs` gains `style_preset()`: `editorial` (the light
  default) and `blade-runner` — a dark neon TOKEN OVERRIDE (near-black blue
  surfaces, cyan/magenta accents, glow shadows) appended after `THEME_CSS`, so
  the same components restyle wholesale. `brand_preset` (e.g. `molecule`)
  brands the chrome. Unknown style = error, no improvisation.
- **Chaos-only, honest failures.** Every section resolves from the persisted
  index + prior generated manifests; compose never parses source files. An
  unservable section (unindexed repo, missing L1 hierarchy) errors naming the
  fix, and the tool description instructs agents to REPORT a compose failure
  rather than faking the page with shell tools.
- **Content-hash dedup for agent memory.** The composed manifest (request +
  section data) is sha256-hashed; the hash lives in the embedded
  `chaos-composed-manifest` and the compact return. Re-composing the same
  request over unchanged knowledge returns `cached: true` and skips the write —
  the agent's signal to not re-ingest a memory it already holds.
- `chaos_pages` recognises the new kind (`chaos-composed-manifest` →
  `composed`).
- Internals: `stack::build_manifest` and `feature_inventory::collect` split the
  build-only halves out of their page-writing `run` paths so compose can embed
  their data without spawning side artifacts.

Verified end-to-end against the live molecule_core index: a beginner persona
routed at cosine 0.96, `desci-infra` auto-detected as a folder (45 features),
and an identical re-run returned `cached: true` with zero writes.

## 0.16.0 — 2026-06-11

The purpose-first page release: the engineer feature page now reads like the
Feature guide. Observed in a real molecule_core RBAC session: the generated
page showed the graph, claims, and correlated files, but never said what the
feature was *made for* or how you'd use it — a reader had to reverse-engineer
the why from the evidence, while the storyboard sibling opened with exactly
that. The pages share one theme; what differed was content structure.

### Changed — feature pages open with purpose and usage examples

`FeatureManifest` gains two additive fields, rendered by the shared
deterministic renderer (so `chaos add` pages and `chaos_write_feature_website`
pages both carry them, and `chaos_refresh --all-features` re-renders older
pages unchanged):

- `purpose` (REQUIRED for new writes): a plain-language "what this feature was
  made for — who uses it, what problem it solves", rendered as the page's
  opening nutshell band before any graph or evidence (the same band the
  storyboard opens with). `chaos add` derives a grounded one automatically;
  the MCP write path rejects manifests without it. Older pages on disk still
  parse and render (the band simply stays hidden).
- `examples[]` (recommended): simple usage examples
  `{title, description, steps[], code, language, node_ids}` rendered as a
  full-width "How you'd use it" section; clicking an example highlights the
  graph nodes its `node_ids` name, keeping the page interactive for humans
  while the embedded `chaos-feature-manifest` stays complete for agents.

Tool description, agent guide (`chaos_help`), SKILL.md, README, and CLAUDE.md
document the new contract.

## 0.15.0 — 2026-06-11

The page-discovery release: "what has chaos already extracted here?" is now a
first-class question. Observed in a real desci-infra session: the agent used
chaos correctly for retrieval, then fell back to `ls`/globbing
`docs/features_memory` (and regex over source) because no tool would list the
generated pages. Listing extracted features is chaos's job — shelling out is
now also called out as a hard anti-pattern in CLAUDE.md and SKILL.md.

### Added — `chaos_pages` MCP tool + `chaos pages` CLI (19 tools)

Lists the generated feature-memory pages of a repository. Scans
`docs/features_memory` (or `--features-dir`, e.g. a project workspace),
recognises every chaos-generated HTML page by its embedded manifest/data block
(`chaos-feature-manifest`, `chaos-storyboard-manifest`,
`chaos-components-manifest`, `chaos-features-manifest`, `chaos-stack-manifest`,
`chaos-impact-data`, `chaos-plan-data`, `chaos-feature-map-data`), and returns
each page's kind, the tool that writes that kind, its title (whichever field
the manifest kind uses: `title` / `task` / `change`), and its modified time —
newest first, with by-kind counts and a provenance breadcrumb. HTML files
without a recognised block are listed as `other` (title from `<title>`) —
nothing in the directory is hidden.

Read-only, embedder-free, pure filesystem: the repo argument resolves against
the index first, but a plain directory path works even when unindexed.

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

## 0.13.0 — 2026-06-10

The discoverability release: an agent that doesn't know what Chaos already
knows can now find out, and `chaos_features` filters resolve **by meaning**,
not by exact word match.

### Changed — semantic layer routing in `chaos_features`

A filter that isn't a folder or an exact layer word is now first tried as a
layer request **semantically**: the filter is embedded once and max-pooled
against a few prototype phrasings per journey layer, so "backend",
"client app", "devops", "web frontend" or "API endpoints" select layer(s) by
embedding cosine — there is no query keyword list to maintain, and an unseen
phrasing still lands. "backend" legitimately spans two layers and resolves to
**interface + core**; layers within a small margin of the best join the set.
Genuine topics ("access control", "ipnft minting") stay below the calibrated
floor and fall through to the topic match exactly as before
(`feature_inventory.rs::maybe_route_layers_by_meaning`; thresholds calibrated
against the default EmbeddingGemma — a model with a flatter cosine
distribution simply routes fewer filters, degrading to the old behavior).
`--layer backend` resolves the same way; the routed layer set is returned in
the compact JSON (`filter.layers`) and recorded as an `embedding` provenance
breadcrumb. Prototype embeddings are cached per process, so the long-lived
MCP server embeds the constant phrasings once.

### Changed — `chaos project list` is the discovery call

`project list` now always returns **every indexed repository** (name, root
path, last indexed) alongside the projects, and an empty project list carries
a hint instead of being a dead end: with no project configured an agent could
not discover that the whole stack may already be ONE indexed repo whose
sub-apps are `chaos_features` folder/layer filters, not project members — it
fell back to listing the filesystem. Tool descriptions, SKILL.md, README,
RUNBOOK and CLAUDE.md updated to match (tool count stays 17).

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
