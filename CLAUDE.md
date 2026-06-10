# Chaos Substrate Agent Instructions

Chaos Substrate is a Rust-only code knowledge memory for agents.

Use it to create and query a persistent knowledge base for Rust, Solidity, TypeScript, JavaScript, and Python repositories, with Markdown/MDX and PDF context. The memory is stored in Postgres + pgvector and survives process restarts.

## Hard Rules

- Do not add mock embedders, fake vectors, or random vectors.
- Do not replace Postgres/pgvector persistence with in-memory storage.
- Keep MCP on stdio with newline-delimited JSON-RPC.
- Keep runtime implementation in Rust.
- TypeScript/JavaScript, Python, and Solidity support must remain Rust-side extraction, not a Node or Python service.

## Common Commands

```sh
cargo run -- help [<command>]   # agent guide: commands + workflows + examples (no DB/config needed)
cargo run -- migrate
cargo run -- doctor
cargo run -- analyze /path/to/repo
cargo run -- add /path/to/repo -m "what changed"   # index git-diff, refresh vault, write feature/bug page
cargo run -- stats /path/to/repo
cargo run -- refresh /path/to/repo --all-features
cargo run -- query /path/to/repo "question"            # add --hierarchical to route through features first
cargo run -- feature-context /path/to/repo "task" --output-html out.html
cargo run -- impact /path/to/repo "<feature>"
cargo run -- change-plan /path/to/repo "<change>" [--since <ref>]   # decompose a change into features (god-nodes)
cargo run -- components /path/to/repo ["<area>"]   # explain a big area's core components (overview before feature extraction)
cargo run -- features /path/to/repo ["<filter>"]   # list ALL god-node features (auto: folder | layer like "client" | topic), grouped by layer
cargo run -- features --project <name> ["<filter>"]   # same listing across EVERY repo of a project (repo-tagged, cross-link-annotated)
cargo run -- project create <name>                       # cross-repo project (client + backend + contracts + infra …)
cargo run -- project add-repo <name> /path/to/repo --alias client   # attach an indexed repo; links it immediately
cargo run -- project list | status <name> | relink <name> [--force]
cargo run -- storyboard /path/to/repo --manifest story.json   # render a client-facing user-story page
cargo run -- graph /path/to/repo -o graph.html
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
- `chaos_query`: answer focused source-grounded questions. Pass `hierarchical: true` for top-down retrieval — the query is matched against feature (L1 community) summaries first and the surfaced features are returned alongside the chunk hits (boosted toward them), falling back to flat search when the repo has no hierarchy.
- `chaos_change_plan`: decompose a proposed change into the FEATURES (L1 communities / god-nodes) it spans, with a dependency-aware check order. Matches the change description against community summary embeddings, **also seeding from a real git diff (`since`) and from previously generated feature pages it correlates with** (shared files → communities); ALWAYS writes an interactive HTML plan to `docs/features_memory/<slug>-plan.html` and returns a COMPACT JSON summary (per-feature label, confidence, `via` source [`semantic`/`diff`/`manifest`], `matched_by` breadcrumbs, check order, top symbols + top-level `provenance` + the HTML path). Use it to answer "how many features does this change involve, and in what order should I check them?".
- `chaos_components`: explain the CORE COMPONENTS of a big area — the orientation step BEFORE feature extraction. An area like "OCL" is bigger than one feature (it spans several L1 communities); given an `area` (or none, for a repo-level overview) it surfaces those communities as COMPONENTS, each with its L3 summary, key symbols/files, languages, and a quotient-graph ROLE (entry/interface/core/foundation), plus how they connect and a dependency-first READ ORDER. Matches the area against community summary embeddings AND community labels (path-derived, so a directory-named area is caught), and correlates the area with previously generated feature pages (shared files → `related_features`). ALWAYS writes an interactive HTML overview to `docs/features_memory/<slug>-components.html` (with an embedded `chaos-components-manifest` an agent can extract) and returns a COMPACT JSON summary (component count, per-component label/role/read_order/top symbols/`matched_by`, relationships, related pages, top-level `provenance`, the HTML path). Use it to understand a large subsystem before drilling into any single feature.
- `chaos_features`: list ALL god-node FEATURES (L1 communities) that match a filter, grouped by journey layer (entry → interface → core → foundation) — the EXHAUSTIVE, uncurated counterpart to `chaos_components` (which gives ONE area's curated, capped, ordered read-through). The single `filter` is AUTO-DETECTED: a path or real directory → FOLDER scope (features whose code lives under it); a single layer word like `client`/`ui`/`api`/`core`/`contracts` → that journey LAYER (so "give me all client features" = every entry-layer feature); anything else → a TOPIC match (summary-embedding cosine + label/summary keywords); omit it for the whole repo. Force the interpretation with `layer`/`folder`/`topic`. Only a topic filter needs the embedder; layer/folder/whole-repo listing is embedder-free. ALWAYS writes an interactive HTML inventory to `docs/features_memory/<slug>-features.html` (embedded `chaos-features-manifest`) and returns a COMPACT JSON summary (resolved filter + how detected, total, per-layer + language counts, per-feature label/role/member_count/folders/top symbols/`matched_by`, top-level `provenance`, the HTML path). Use it to answer "give me all the features in this layer/folder/topic". Pass `project` instead of `repo` to list features across EVERY member repo of a project in one journey-layered inventory — cards are tagged with repo aliases and annotated with the project's cross-repo links; the HTML goes to the project workspace (`$CHAOS_PROJECT_DIR/<slug>/` or `~/.chaos/projects/<slug>/`).
- `chaos_project`: work ACROSS REPOSITORIES — the layer above single-repo memory. A project is a named set of indexed repos (client, backend, smart contracts, infra, …); Chaos detects FEATURE→FEATURE CROSS-REPO LINKS between members from the persisted index (consumer → provider): `package_dep` (one repo imports a package the other publishes), `abi` (client/backend code references a Solidity contract defined in the contracts repo), `http_route` (a fetch/axios call path matches a route registered in another repo). Links attach at the feature (L1) level, carry evidence + provenance breadcrumbs, and refresh AUTOMATICALLY after `chaos_analyze`/`chaos_add` on any member — gated by the L2 repo root hash, so a no-change re-index relinks nothing. Actions: `create`, `add_repo` (attach an INDEXED repo under an alias; links it immediately), `list`, `status` (members, staleness, links by kind, embedder consistency), `relink` (`force` overrides the gate). Member repos must share one embedder config; `status` warns on mismatch.
- `chaos_feature_context`: gather evidence for feature understanding. Each retrieval hit is tagged with its retrieval method (`retrieved_by`: semantic/keyword/literal), each prior-page match carries that page's own provenance, and the response includes top-level **provenance breadcrumbs** (how the evidence was gathered).
- `chaos_impact`: build a feature-vs-existing-code impact report for an indexed repo and ALWAYS write an interactive HTML (impact summary + evidence dashboard) to `docs/features_memory/<slug>-impact.html`; returns only a compact JSON summary (counts, the existing files/symbols the feature touches, warnings, **provenance breadcrumbs** [hybrid retrieval with per-method hit breakdown, manifests scanned, aggregation], and the HTML path) so it won't flood agent context like a raw `chaos_feature_context` dump. Use it to see how a proposed feature maps onto the codebase as it is today (the before).
- `chaos_write_feature_website`: write an engineer-facing feature page. Pass the structured manifest ONLY (omit `html`) — Chaos renders the interactive page deterministically, so no tokens are spent generating or transmitting raw HTML; an explicit `html` argument remains as a legacy path.
- `chaos_obsidian`: export an already-indexed repository as an Obsidian vault from the persisted graph (run after `chaos_analyze`, which never writes files).
- `chaos_refresh`: regenerate project-local artifacts (Obsidian vault, god-node community notes + `docs/features_memory/feature-map.html`, and with `all_features` the `docs/features_memory` pages) from the persisted index without re-indexing or calling the embedder.
- `chaos_write_storyboard`: write a CLIENT/USER-FACING **"Feature guide"** — a code-free UI/UX user-story page (role-card personas, "As a … I want … so that …" stories, a scrollytelling walkthrough, outcomes) rendered in the shared light editorial theme (Access-Control lineage, with scroll-unlock gamification) to `docs/features_memory/<slug>-story.html`. You pass a structured, code-free manifest only and Rust owns the styling. Each walkthrough step pairs with a device mockup built from the frame's optional `preview` (a REAL captured screenshot/clip, or a live `iframe` of a running app route) — Chaos can't synthesise the client's screens, so a frame with no `preview` shows an honest "add a screenshot" placeholder; ask the user/dev to capture real ones. Confidence values are optional metadata and are not shown to end users. Optional, backward-compatible extras match the full guide look: `brand_preset` (e.g. "molecule" — a preset shipped inside Chaos, no local files) or `hero_image` + `brand` to set your own logo/company, persona `who`/`icon`/`includes`/`tier`, a permission `matrix`, an agent-style `callout`, and an end-of-page `game` (a click-to-check mini-game). This is the user-facing sibling of `chaos_write_feature_website` (which is for engineers: graph, architecture, code).

Do not synthesize feature pages from `chaos_query` alone when `chaos_feature_context` and
`chaos_write_feature_website` are available.

## Hierarchical memory (L0 / L1 / L2 / L3)

On top of the flat multigraph (**L0**), `analyze`/`add` derive a layered memory (see
`docs/HIERARCHICAL_MEMORY_ROADMAP.md`):

- **L1 — communities / "god-nodes" / features.** Deterministic Louvain (`src/community.rs`) groups
  L0 nodes into features with a quotient graph of typed edges between them (`communities`,
  `community_members`, `community_edges`).
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

See `docs/CLAUDE_MCP_INSTALL.md` and `docs/CLAUDE_VALIDATION_BRIEF.md`.
