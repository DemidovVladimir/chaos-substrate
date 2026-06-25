# Chaos Substrate Agent Instructions

Chaos Substrate is a Rust-only code knowledge memory for agents.

Use it to create and query a persistent knowledge base for Rust, Solidity, TypeScript, JavaScript, and Python repositories, with Markdown/MDX and PDF context. The memory is stored in Postgres + pgvector and survives process restarts.

## Hard Rules

- Do not add mock embedders, fake vectors, or random vectors.
- Do not replace Postgres/pgvector persistence with in-memory storage.
- Keep MCP on stdio with newline-delimited JSON-RPC.
- Keep runtime implementation in Rust.
- TypeScript/JavaScript, Python, and Solidity support must remain Rust-side extraction, not a Node or Python service.
- When answering feature/extraction questions about an indexed TARGET repository, use chaos tools only — no `rg`/`grep`/`ls` fallbacks and no generated scripts (python/bash/ts/js) against that repo. Page discovery goes through `chaos pages`/`chaos_pages`. A missing capability is a chaos feature request, not a shell workaround.

## Common Commands

```sh
cargo run -- help [<command>]   # agent guide: commands + workflows + examples (no DB/config needed)
cargo run -- migrate
cargo run -- doctor
cargo run -- analyze /path/to/repo
cargo run -- add /path/to/repo -m "what changed"   # index git-diff, refresh vault, write feature/bug page
cargo run -- stats /path/to/repo
cargo run -- stack /path/to/repo   # tech-stack inventory: declared deps, scripts, CDK stacks/resources, configs
cargo run -- pages /path/to/repo   # list the generated feature-memory pages (what's already extracted) — never ls
cargo run -- gaps /path/to/repo [--folder <prefix>]   # knowledge gaps: code retrieval can't find (no chunks, or no distinctive vocabulary) — fix = docstring/README + chaos add, never pause indexing
cargo run -- gaps --project <name>                    # same scan across EVERY member repo of a project
cargo run -- refresh /path/to/repo --all-features
cargo run -- query /path/to/repo "question"            # add --hierarchical to route through features first
cargo run -- feature-context /path/to/repo "task" --output-html out.html
cargo run -- impact /path/to/repo "<feature>"
cargo run -- usage /path/to/repo "<symbol-or-surface-string>"   # who consumes X across subfolders (index-only, embedder-free) — never rg
cargo run -- sui-migration-impact /path/to/repo --source auto   # map an EVM/Solana repo's features onto Sui (Walrus/Seal), embedder-free
cargo run -- change-plan /path/to/repo "<change>" [--since <ref>]   # decompose a change into features (god-nodes)
cargo run -- components /path/to/repo ["<area>"]   # explain a big area's core components (overview before feature extraction)
cargo run -- features /path/to/repo ["<filter>"]   # list ALL god-node features (auto: folder | layer like "client" | topic), grouped by layer
cargo run -- features --project <name> ["<filter>"]   # same listing across EVERY repo of a project (repo-tagged, cross-link-annotated)
cargo run -- compose /path/to/repo --sections features,correlations,stack --persona "<audience>" [--style blade-runner] [--brand-preset molecule] [--filter <folder|layer|topic>] [--feature-pages]   # ONE page (or a clickable per-feature SITE) from KB-backed sections, persona-tailored, hash-gated
cargo run -- project create <name>                       # cross-repo project (client + backend + contracts + infra …)
cargo run -- project add-repo <name> /path/to/repo --alias client   # attach an indexed repo; links it immediately
cargo run -- project list | status <name> | relink <name> [--force]
cargo run -- feature-story <project> "<feature>" [--style blade-runner] [--brand-preset molecule]   # cross-repo STORY of ONE feature: match it per repo, follow cross-repo links, order client→backend→contracts, write a clickable multi-page site (hash-gated)
cargo run -- storyboard /path/to/repo --manifest story.json   # render a client-facing user-story page
cargo run -- graph /path/to/repo -o graph.html   # add --serve [--port 7878] for live semantic search in the page (validates the same retrieval pipeline the tools use)
cargo run -- obsidian /path/to/repo -o vault
cargo run -- setup --dry-run
cargo run -- hook --event PreToolUse
cargo run -- mcp
```

Full ops reference: see RUNBOOK.md.

## MCP Tool Surface

Agents should prefer MCP tools when available:

- `chaos_analyze`: index or refresh a repository.
- `chaos_add`: incrementally index the files changed in git (or explicit `paths`), refresh the Obsidian vault, and write an interactive feature/bug page — in one call. Use after making changes instead of a full `chaos_analyze` when you only touched a few files. The page records **provenance breadcrumbs** (git diff, AST/language extraction, Postgres graph load, file reads, manifest correlation) plus per-node evidence, and **correlates the change with previously generated feature pages** by shared files/symbols (surfaced as `related_features` + a correlation claim) so the new extraction understands the existing features it overlaps.
- `chaos_stats`: report index statistics for an already-indexed repository read from Postgres — totals (files, nodes, edges, chunks, embedded vs missing, split chunks) plus breakdowns of nodes by kind, edges by kind, chunks by type, and files by language. Read-only and embedder-free; use to explain or sanity-check what an `chaos_analyze`/`chaos_add` produced.
- `chaos_stack`: report the TECH STACK of an already-indexed repository, LISTED rather than counted (`chaos_stats` only counts these node kinds): manifest-declared dependencies by ecosystem (npm/cargo — name, versions, runtime-vs-dev scope, how many workspace manifests declare each, widest-declared first), npm scripts, deployment resources (AWS CDK app entrypoints, Stack classes, L2 constructs grouped by cloud service), indexed JS/TS configs, and the file-language breakdown. Read-only and embedder-free. ALWAYS writes an interactive HTML inventory to `docs/features_memory/stack.html` (embedded `chaos-stack-manifest`) and returns a COMPACT JSON summary (capped lists with `*_omitted` counts; every entry lives in the HTML) with explicit COVERAGE notes — what the index extracts vs what it does not yet (Dockerfiles, CI workflows, pyproject.toml, foundry.toml, Terraform). Use it to answer "what is this repo built with / what infrastructure does it use?" without grepping manifests.
- `chaos_pages`: list the GENERATED feature-memory pages of a repository — what chaos has ALREADY extracted. Scans `docs/features_memory` (or `features_dir`, e.g. a project workspace) and returns every HTML page with its KIND (`feature`/`story`/`components`/`features`/`stack`/`impact`/`change-plan`/`feature-map`; unrecognised files are listed as `other`, never hidden), the tool that writes that kind, its title, and its modified time, newest first, plus by-kind counts. Read-only, embedder-free, pure filesystem. Use it INSTEAD of `ls`/globbing to check whether a feature page already exists before a new deep-dive, and to find the page to reopen.
- `chaos_gaps`: list the KNOWLEDGE GAPS — code retrieval cannot find. Two kinds: `coverage_gaps` (files that produced NO chunks — invisible to every method) and `vocabulary_gaps` (chunked, but carrying almost no distinctive vocabulary after identifier splitting; background words are corpus-derived, never a hardcoded list). Pass `repo` for one repository, `repo` + `folder` for a sub-app inside a monorepo-indexed repo, or `project` to scan EVERY member repo in one repo-tagged report. Read-only, embedder-free, compact return. The fix for a vocabulary gap is repo content: ask the user what the file is for, write a file-top docstring or folder README, then `chaos_add` those paths — never pause or block indexing waiting for answers.
- `chaos_query`: answer focused source-grounded questions. Pass `hierarchical: true` for top-down retrieval — the query is matched against feature (L1 community) summaries first and the surfaced features are returned alongside the chunk hits (boosted toward them), falling back to flat search when the repo has no hierarchy.
- `chaos_change_plan`: decompose a proposed change into the FEATURES (L1 communities / god-nodes) it spans, with a dependency-aware check order. Matches the change description against community summary embeddings, **also seeding from a real git diff (`since`) and from previously generated feature pages it correlates with** (shared files → communities); ALWAYS writes an interactive HTML plan to `docs/features_memory/<slug>-plan.html` and returns a COMPACT JSON summary (per-feature label, confidence, `via` source [`semantic`/`diff`/`manifest`], `matched_by` breadcrumbs, check order, top symbols + top-level `provenance` + the HTML path). Use it to answer "how many features does this change involve, and in what order should I check them?".
- `chaos_components`: explain the CORE COMPONENTS of a big area — the orientation step BEFORE feature extraction. An area like "OCL" is bigger than one feature (it spans several L1 communities); given an `area` (or none, for a repo-level overview) it surfaces those communities as COMPONENTS, each with its L3 summary, key symbols/files, languages, and a quotient-graph ROLE (entry/interface/core/foundation), plus how they connect and a dependency-first READ ORDER. Matches the area against community summary embeddings AND community labels (path-derived, so a directory-named area is caught), and correlates the area with previously generated feature pages (shared files → `related_features`). ALWAYS writes an interactive HTML overview to `docs/features_memory/<slug>-components.html` (with an embedded `chaos-components-manifest` an agent can extract) and returns a COMPACT JSON summary (component count, per-component label/role/read_order/top symbols/`matched_by`, relationships, related pages, top-level `provenance`, the HTML path). Use it to understand a large subsystem before drilling into any single feature.
- `chaos_features`: list ALL god-node FEATURES (L1 communities) that match a filter, grouped by journey layer (entry → interface → core → foundation) — the EXHAUSTIVE, uncurated counterpart to `chaos_components` (which gives ONE area's curated, capped, ordered read-through). The single `filter` is AUTO-DETECTED: a path or real directory → FOLDER scope (features whose code lives under it); a single layer word like `client`/`ui`/`api`/`core`/`contracts` → that journey LAYER (so "give me all client features" = every entry-layer feature); any other phrase is first tried as a layer BY MEANING (embedding cosine against per-layer prototype phrasings — "backend", "client app", "devops" resolve semantically, no keyword list; "backend" spans interface+core) and only then falls to a TOPIC match (summary-embedding cosine + label/summary keywords); omit it for the whole repo. Force the interpretation with `layer`/`folder`/`topic`. Exact layer words, folders and whole-repo listing are embedder-free; semantic layer routing and topic matching use the embedder. ALWAYS writes an interactive HTML inventory to `docs/features_memory/<slug>-features.html` (embedded `chaos-features-manifest`) and returns a COMPACT JSON summary sized to stay inline in agent context (resolved filter + how detected, total, per-layer + language counts, domain group names, ONE READABLE LINE PER FEATURE — label, layer role, member count, extra folders, short symbols, why-it-matched for topic queries — top-level `provenance`, the HTML path; full per-feature detail lives in the HTML manifest). The HTML groups features into HUMAN-READABLE DOMAINS — folder-derived automatically; after composing a curated grouping with notes for your answer, call again with the same repo/filter plus `curation` ({groups: [{title, icon?, blurb?, features: [{label, note?}]}]}) so the page carries your domains and one-line notes as its primary sections (cheap re-render, tiny receipt return; unplaced features stay auto-grouped). Use it to answer "give me all the features in this layer/folder/topic". Pass `project` instead of `repo` to list features across EVERY member repo of a project in one journey-layered inventory — cards are tagged with repo aliases and annotated with the project's cross-repo links; the HTML goes to the project workspace (`$CHAOS_PROJECT_DIR/<slug>/` or `~/.chaos/projects/<slug>/`).
- `chaos_compose`: THE page-generation surface — whenever the user asks for a webpage, website, or interactive info page over chaos knowledge, route it HERE instead of stitching the side-pages of `chaos_features`/`chaos_stack`/`chaos_components` (those stay data/inventory tools). Composes ONE page — or a SITE with `feature_pages: true`, which adds one page per feature under `<slug>-composed/`, makes the index's feature cards CLICKABLE links, and gives every per-feature page the feature's code/files, its quotient-graph relations to the rest of the stack (Solidity neighbours tagged as smart contracts, in-scope neighbours cross-linked), prior overlapping pages, and a deterministic persona-adapted walkthrough built only from indexed data (each page says so honestly). Every page (index AND per-feature) embeds its own `chaos-composed-manifest` with its own `content_hash` and is individually hash-gated — the return reports written vs cached counts, and `cached` pages must NOT be re-ingested into context. Pick `sections` ('features' — the inventory with each feature's concise L3 explanation; 'correlations' — files shared between those features plus prior generated pages overlapping them; 'stack'), an AUDIENCE (free-text `persona` resolved to beginner|practitioner|expert BY MEANING via prototype embeddings — or explicit `level`, embedder-free) and a STYLE preset ('editorial' light default | 'blade-runner' dark neon; `brand_preset` e.g. 'molecule'). Resolves EVERYTHING from the persisted index + prior generated manifests — never parses source files; an unservable section (repo not indexed, no L1 hierarchy, unknown section/style) is a LOUD ERROR naming the fix, and a compose failure must be REPORTED to the user, never papered over with rg/scripts. Writes `docs/features_memory/<slug>-composed.html` (embedded `chaos-composed-manifest` with every section's full data for agents) and returns a COMPACT JSON summary. CONTENT-HASHED: same request over unchanged knowledge returns `cached: true` without writing — the hash is the agent's dedup key; do not re-ingest a composition you already hold.
- `chaos_project`: work ACROSS REPOSITORIES — the layer above single-repo memory. A project is a named set of indexed repos (client, backend, smart contracts, infra, …); Chaos detects FEATURE→FEATURE CROSS-REPO LINKS between members from the persisted index (consumer → provider): `package_dep` (one repo imports a package the other publishes), `abi` (client/backend code references a Solidity contract defined in the contracts repo), `http_route` (a fetch/axios call path matches a route registered in another repo). Links attach at the feature (L1) level, carry evidence + provenance breadcrumbs, and refresh AUTOMATICALLY after `chaos_analyze`/`chaos_add` on any member — gated by the L2 repo root hash, so a no-change re-index relinks nothing. Actions: `create`, `add_repo` (attach an INDEXED repo under an alias; links it immediately), `list` (also returns EVERY indexed repository — the discovery call when you don't know what Chaos already knows; a sub-app inside one indexed repo is a `chaos_features` folder/layer filter, not a project), `status` (members, staleness, links by kind, embedder consistency), `relink` (`force` overrides the gate). Member repos must share one embedder config; `status` warns on mismatch.
- `chaos_feature_story`: tell the cross-repo STORY of ONE feature across a PROJECT — the focused, single-feature counterpart to `chaos_features --project` (which inventories ALL features). Given `project` + a free-text `feature`, it matches that feature in EVERY member repo (L1 community semantic search + a lexical label fallback), loads the persisted cross-repo links and TRAVERSES them — pulling in a link's other endpoint (e.g. the Solidity contract a client calls) even when the query didn't match it directly — then orders the involved features into a journey-layer SPINE (entry → interface → core → foundation = client → backend → contracts). Writes a CLICKABLE MULTI-PAGE SITE to the project workspace (index page = the spine + the cross-repo link chain + repos not involved; one hash-gated drill-down page per involved feature with its code/files, cross-repo links cross-linked + smart-contract tagged, prior overlapping pages, a deterministic walkthrough) and returns a COMPACT JSON summary (involved repos, the ordered link chain, links_by_kind, not-involved repos, the site summary, `output_html`, provenance, `content_hash`). Deterministic and embedder-light (ONE embed for the whole query, reused across repos). Narrate the returned spine for the user. Distinct from `chaos_compose` (one repo's composed page).
- `chaos_feature_context`: gather evidence for feature understanding and — mirroring `chaos_impact` — ALWAYS write the interactive HTML, returning only a COMPACT pointer-only payload (counts, one-line deduped evidence with NO chunk content/code, relevance-floored `related_pages`, warnings, provenance, `output_html`, and a `next` reminder) instead of dumping the raw evidence into context. The FULL verbatim evidence (with code) lives only in the written HTML, under an extractable block `id="chaos-feature-context-data"`. Each retrieval hit is tagged with its retrieval method (`retrieved_by`: semantic/keyword/literal), each prior-page match carries that page's own provenance, and the response includes top-level **provenance breadcrumbs** (how the evidence was gathered).
- `chaos_impact`: build a feature-vs-existing-code impact report for an indexed repo and ALWAYS write an interactive HTML (impact summary + evidence dashboard) to `docs/features_memory/<slug>-impact.html`; returns only a compact JSON summary (counts, the existing files/symbols the feature touches, warnings, **provenance breadcrumbs** [hybrid retrieval with per-method hit breakdown, manifests scanned, aggregation], and the HTML path) so it won't flood agent context (the full evidence stays in the HTML). Use it to see how a proposed feature maps onto the codebase as it is today (the before).
- `chaos_usage`: who CONSUMES a symbol or surface string (env var, HTTP header, route, function) across the repo, grouped by top-level subfolder — the cross-folder "who uses this?" answer from the persisted index (user-surface `env_var`/`http_route`/`cli_command` nodes + reverse graph edges + a literal chunk sweep), so you never fall back to `rg`/`grep` on the target repo. ALWAYS writes `docs/features_memory/<slug>-usage.html` (manifest under `chaos-usage-manifest`) and returns a COMPACT per-folder summary (capped, with `sites_omitted` counts). Read-only and embedder-free. Honest limitation surfaced as a warning: call/import edges resolve cross-file only for repo-unique names.
- `chaos_sui_migration_impact`: produce a Sui migration impact report for an indexed Ethereum/Solana/mixed Web3 repo — auto-detects the source stack (or pass `source`: auto/ethereum/solana/mixed), maps each L1 feature onto Sui primitives (objects/dynamic fields, Coin/Kiosk/Display, capabilities, PTBs, events+GraphQL) with Walrus/Seal storage and access-control verdicts, each citing the compiled-in Sui official docs profile. Read-only and embedder-free. ALWAYS writes `docs/features_memory/sui-migration-impact.html` and returns a COMPACT JSON summary (per-feature Sui mappings, verdicts, provenance, and the HTML path). Maps impact only — generates no Move code.
- `chaos_write_feature_website`: write an engineer-facing feature page. Pass the structured manifest ONLY (omit `html`) — Chaos renders the interactive page deterministically, so no tokens are spent generating or transmitting raw HTML; an explicit `html` argument remains as a legacy path. The manifest's REQUIRED `purpose` opens the page with a plain-language "what this feature was made for" band (before any graph or evidence), and `examples` (`{title, description, steps[], code, language, node_ids}`) render a clickable "How you'd use it" section whose `node_ids` highlight the code path on the graph — include at least one simple example whenever the feature has a callable surface, so the page matches the Feature-guide reading experience while staying engineer-grade.
- `chaos_obsidian`: export an already-indexed repository as an Obsidian vault from the persisted graph (run after `chaos_analyze`, which never writes files).
- `chaos_refresh`: regenerate project-local artifacts (Obsidian vault, god-node community notes + `docs/features_memory/feature-map.html`, and with `all_features` the `docs/features_memory` pages) from the persisted index without re-indexing or calling the embedder.
- `chaos_write_storyboard`: write a CLIENT/USER-FACING **"Feature guide"** — a code-free UI/UX user-story page (role-card personas, "As a … I want … so that …" stories, a scrollytelling walkthrough, outcomes) rendered in the shared light editorial theme (Access-Control lineage, with scroll-unlock gamification) to `docs/features_memory/<slug>-story.html`. You pass a structured, code-free manifest only and Rust owns the styling. Each walkthrough step pairs with a device mockup built from the frame's optional `preview` (a REAL captured screenshot/clip, or a live `iframe` of a running app route) — Chaos can't synthesise the client's screens, so a frame with no `preview` renders text-only (full-width copy, no mockup or placeholder); add real captures when the user provides them. Confidence values are optional metadata and are not shown to end users. Optional, backward-compatible extras match the full guide look: `brand_preset` (e.g. "molecule" — a preset shipped inside Chaos, no local files) or `hero_image` + `brand` to set your own logo/company, persona `who`/`icon`/`includes`/`tier`, a permission `matrix`, an agent-style `callout`, and an end-of-page `game` (a click-to-check mini-game). This is the user-facing sibling of `chaos_write_feature_website` (which is for engineers: graph, architecture, code).
- `chaos_graph`: export the indexed repo as a standalone interactive HTML graph (the full L0 node/edge view) from the persisted index. Embedder-free. Defaults to `docs/features_memory/graph.html` inside the repo. Pass `--serve [--port 7878]` from the CLI for a live "Semantic search" panel that runs the same hierarchical retrieval pipeline as `chaos_query`. The feature-level map (`feature-map.html`) comes from `chaos_obsidian`/`chaos_refresh` instead.
- `chaos_help`: returns the agent guide — recommended tool order, typical workflows, and token notes — as static text. No DB or embedder work; call it once when first meeting the server or whenever unsure which tool fits.
- `chaos_clean`: **Destructive.** Wipes the persisted index — one repo (`repo`) or everything (omit it); `artifacts: true` also deletes generated files on disk (vault, feature pages, project workspaces). Requires `confirm: true`. Schema survives; the index stays empty until a `chaos_analyze` is requested. Use ONLY on explicit user request.

Do not synthesize feature pages from `chaos_query` alone when `chaos_feature_context` and
`chaos_write_feature_website` are available.

A feature DEEP-DIVE is not done until the page exists. When a user asks to drill into a feature or
flow (`chaos_feature_context` → code reads → explanation), end the drill-down by persisting the
composed explanation with `chaos_write_feature_website` (engineer page, manifest only) — or
`chaos_write_storyboard` for a stakeholder audience. An explanation that lives only in chat is lost
when the session ends; `chaos_feature_context`'s return carries a `next` reminder for exactly this.

## Hierarchical memory (L0 / L1 / L2 / L3)

On top of the flat multigraph (**L0**), `analyze`/`add` derive a layered memory (see the
Hierarchical (Layered) Memory section of `ARCHITECTURE.md`):

- **L1 — communities / "god-nodes" / features.** Deterministic Louvain (`src/community.rs`) groups
  L0 nodes into features with a quotient graph of typed edges between them (`communities`,
  `community_members`, `community_edges`). A deterministic post-Louvain consolidation pass folds
  sub-threshold fragments (< 4 members) into their folder-preferred best-coupled neighbor so the
  layer holds features, not per-file singletons.
- **L2 — Merkle rollup.** `content_hash` leaves roll up to file → community → repo `subtree_hash`es
  (`src/merkle.rs`). This drives `chaos add`'s feature **blast radius** and gates L3.
- **L3 — community summaries.** A hash-gated, real-embedder summary per community
  (`community_embeddings`); a no-change re-index recomputes **zero** summaries.

These power `chaos_change_plan` (top-down decomposition) and `chaos_query --hierarchical` (feature
routing). All of it is additive — a repo indexed before the hierarchy still answers
`query`/`stats`/`add`.

## Cross-repository projects (P6)

On top of the per-repo layers, a **project** groups indexed repositories (client, backend, smart
contracts, infra, …) under one name (`projects`, `project_repos`, `cross_repo_links` —
`migrations/005_projects.sql`). The linkers in `src/linker.rs` detect **feature→feature cross-repo
links** (consumer → provider) purely from the persisted index: `package_dep` (manifest `name`
imported elsewhere), `abi` (Solidity contract referenced from non-Solidity code), `http_route`
(client call path matches a registered route, params normalized to `*`). Links attach at L1 — never
L0, whose FK-protected schema stays frozen — and every `analyze`/`add` ends by relinking the repo's
projects, gated by the L2 `repo_root_hash` (`project_repos.linked_repo_hash`), so the project layer
follows the same hash-gated pipeline as L3 summaries. `src/project.rs` owns the commands;
`chaos_features` with `project` lists every member repo's features in one journey-layered,
cross-link-annotated inventory written to the project workspace (`~/.chaos/projects/<slug>/` or
`$CHAOS_PROJECT_DIR`). All member repos must share one embedder config (warned on mismatch).

## Provenance breadcrumbs & manifest correlation

Every generated feature artifact records **provenance breadcrumbs** — `Breadcrumb { source, method,
detail, locator }` from `src/provenance.rs` — so you can audit *where each piece of information came
from* (git diff, AST/language extraction, Postgres queries, file reads, embedding cosine, or a prior
feature manifest). They are embedded in the manifest JSON / compact MCP return and rendered as a
"How this was generated" panel. The `source` vocabulary is the `provenance::source` constants
(`git`, `postgres`, `file`, `ast`, `regex`, `embedding`, `feature-manifest`, `merkle`, `graph`).
Retrieval hits also carry `metadata.retrieved_by` (`semantic`/`keyword`/`literal`).

New feature extractions **consider previously generated feature pages**: `chaos add` correlates a
change with existing `docs/features_memory/*.html` manifests by shared files/symbols
(`correlate_feature_manifests`, surfaced as `related_features`), and `chaos_change_plan` seeds
features from prior manifests (`via: "manifest"`). `chaos_feature_context` and `chaos_impact` already
scored prior manifests via `load_feature_matches`. This is the "if the new extraction is correlated,
it understands better" path — additive and backward-compatible (older pages simply have no
provenance/related blocks).

## Claude Code / Cowork MCP

Prefer the wrapper when registering this repository as an MCP server:

```sh
bin/chaos claude-code-add local
bin/chaos claude-code-add project /absolute/path/to/target-repo
```

Use `local` for private setup and `project` when a target repository should receive a shareable
`.mcp.json`. The optional path argument selects the Claude Code project directory; if omitted, the
current working directory is used. Keep MCP on stdio and launch the release binary directly.

## Validation

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

For real repository indexing, configure either OpenAI or Ollama embeddings. If the embedder is unavailable, analysis must fail rather than producing fake vectors.

See `docs/EDITOR_SETUP.md` for install/registration and `RUNBOOK.md` for the ops and validation reference.
