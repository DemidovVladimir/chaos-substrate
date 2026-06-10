---
name: chaos-substrate
description: Use when installing, initializing, updating, querying, or operating Chaos Substrate in any Rust, Solidity, TypeScript, JavaScript, or Python repository; includes Markdown/MDX and PDF context, Postgres+pgvector persistence, real OpenAI/Ollama embedders, CLI, MCP stdio, generated feature-memory websites, and agent implementation context.
---

# Chaos Substrate

Use this skill when working on or operating Chaos Substrate, a Rust-only code knowledge memory for agents.

## Product Shape

- Rust, Solidity, TypeScript, JavaScript, Python, Markdown, MDX, JSON config, and text PDF extraction.
- Persistent Postgres plus pgvector memory.
- Real OpenAI and Ollama embedders only.
- Agent surfaces are Codex plugin, Claude Code plugin, CLI, the `chaos` wrapper, static
  graph/Obsidian exports, generated feature-context websites, feature-memory manifests, and MCP over
  stdio.
- Source code is prioritized over docs during code-repository queries. Markdown/MDX docs and
  extractable text PDFs are indexed as supplemental context, not ignored.

## Hierarchical Memory (L0/L1/L2/L3)

The base graph is the L0 multigraph (files, symbols, chunks, edges). Layered on top of it — all in
Rust, all additive, never replacing L0 — is a hierarchy that lets agents reason at the feature level:

- L1 communities / "god-nodes" / features: a deterministic Louvain partition of the graph
  (`src/community.rs`) persisted as the `communities`, `community_members`, and `community_edges`
  (quotient graph) tables. Migration `migrations/002_communities.sql`.
- L2 Merkle rollup: each chunk `content_hash` rolls up into file -> community -> repo `subtree_hash`
  (`src/merkle.rs`), driving `chaos add`'s feature "blast radius". Migration
  `migrations/003_subtree_hash.sql`.
- L3 hash-gated community summaries embedded by the REAL embedder (`src/community_summary.rs`,
  `community_embeddings` table). Summaries are regenerated only when a community's subtree hash
  changes, so a no-change re-index makes ZERO summary embed calls. Migration
  `migrations/004_community_summary.sql`.
- `src/change_plan.rs` powers `chaos_change_plan`; `src/hierarchy_export.rs` writes the Obsidian
  god-node notes and the feature-map HTML.

Everything is additive: a repository indexed before the hierarchy existed still answers
`chaos_query`, `chaos_stats`, and `chaos_add`. The hierarchy surfaces through `chaos_query`'s
`hierarchical` option, `chaos_change_plan`, and the god-node community notes / feature map that
`chaos_obsidian` and `chaos_refresh` regenerate.

## Hard Boundaries

- Do not edit `Cargo.toml` or `src/` unless the user explicitly asks.
- Do not add mock embeddings, fake vector stores, in-memory persistence, HTTP APIs, Python services, TypeScript services, or live browser services. A standalone Rust-generated `graph.html` export is allowed for persisted graph validation. TypeScript, JavaScript, Python, and Solidity are analysis targets only: their extraction belongs in the Rust extractor, never in a sidecar runtime in another language.
- Do not downgrade persistence guarantees; memory must survive process restarts.
- Do not replace real embedders with deterministic test-only behavior in production paths.

## Working Pattern

When a user asks to use Chaos Substrate in a target project, prefer MCP tools over shell commands.
Use the `chaos` wrapper for setup, debugging, or when MCP is unavailable.

If MCP tools are available, prefer them over shelling out:

1. Use `chaos_analyze` to index or refresh a repository.
2. Use `chaos_add` for one-shot incremental indexing of git-changed files (or explicit `paths`); it
   re-embeds only the changed chunks, refreshes the Obsidian vault, and writes a feature/bug page in
   a single call. A full `chaos_analyze` is still needed to rebuild cross-file call edges into
   unchanged files. The page records **provenance breadcrumbs** (git diff, AST/language extraction,
   Postgres graph load, file reads, manifest correlation) plus per-node evidence, and **correlates
   the change with previously generated feature pages** by shared files/symbols (`related_features` +
   a correlation claim) so a new extraction understands the existing features it overlaps.
3. Use `chaos_stats` to report index statistics for an already-indexed repository, read from
   Postgres: totals (files, nodes, edges, chunks, embedded vs missing, split chunks) plus breakdowns
   of nodes by kind, edges by kind, chunks by type, and files by language. It is read-only and
   embedder-free; use it to explain or sanity-check what an `chaos_analyze`/`chaos_add` produced.
4. Use `chaos_query` for focused questions. It takes an optional `hierarchical` boolean (CLI
   `query --hierarchical`): top-down retrieval that matches feature (community) summaries first and
   returns the surfaced features alongside the chunk hits, falling back to flat search when no
   hierarchy exists.
5. Use `chaos_feature_context` when the user asks to explain a feature, prepare implementation
   context, or generate a feature explanation. Hits carry `metadata.retrieved_by`
   (semantic/keyword/literal), and the response includes top-level **provenance breadcrumbs**.
6. Use `chaos_impact` to build a feature-vs-existing-code impact report for an indexed repo. It
   ALWAYS writes an interactive HTML (impact summary plus evidence dashboard) to
   `docs/features_memory/<slug>-impact.html` and returns a COMPACT JSON summary — counts plus the
   existing files/symbols the feature touches, warnings, **provenance breadcrumbs**, and the HTML
   path. The full evidence lives only in the HTML, so it will not flood an agent context like a raw
   `chaos_feature_context` dump. Use it to see how a proposed feature maps onto the codebase as it
   exists today (the before). It mirrors the `chaos impact <repo> <feature>` CLI command.
7. Use `chaos_write_feature_website` only after reading `chaos_feature_context` output. Compose a
   feature-specific MANIFEST (feature, title, subtitle, claims, modes, nodes with file/lines/code,
   edges, story) and pass it WITHOUT the `html` argument — Chaos renders the full interactive page
   deterministically (the same renderer `chaos add` uses), so you never spend tokens authoring or
   transmitting raw HTML. The LLM still decides the story, claims, nodes, and flow from evidence;
   the tool owns the rendering. (Passing `html` yourself is a legacy path.)
8. Use `chaos_obsidian` to export an already-indexed repository as an Obsidian vault from the
   persisted graph; run it after `chaos_analyze` (which never writes files) when you want browsable
   docs. This lets an MCP-only agent generate the vault without shelling out to the CLI.
9. Use `chaos_refresh` to regenerate project-local artifacts from the persisted index without
   re-indexing: it rewrites the Obsidian vault and, with `all_features=true`, re-renders the
   deterministic feature pages in `docs/features_memory` from their embedded manifests. This lets an
   MCP-only agent refresh pages without shelling out to the CLI; run `chaos_analyze` or `chaos_add`
   first.
10. Use `chaos_write_storyboard` to produce a CLIENT/USER-FACING explanation of a feature — a
    UI/UX user-story page with NO code, meant to be handed to a stakeholder or end user as an
    interactive presentation. You supply only a structured, code-free manifest: `personas`,
    `stories` ("As a … I want … so that …" with plain-language `acceptance` criteria), `frames`
    (clickable steps grouped into `stage` lanes, each with a `summary`, a click-to-reveal `detail`,
    `user_value`, and an optional `ui_hint`), `outcomes`, and a `confidence` (0..1) on every
    frame/story/outcome plus an `overall_confidence`. A frame may also carry an optional `preview`
    that shows the REAL client UI (not code): `{"kind":"image","src":"previews/x.png","alt":"…",
    "caption":"…"}` for a screenshot/clip you captured (offline, leaks nothing — preferred) or
    `{"kind":"iframe","url":"http://localhost:5173/route","caption":"…"}` to live-embed a running
    app route (renders only while that server is up). Chaos only embeds it — it never runs a browser
    and CANNOT synthesise the client's screens, so capture the screenshot yourself (host/Playwright)
    or point at the user's running dev server; a frame with no `preview` renders an honest "add a
    screenshot" placeholder, so ASK the user/dev for real captures rather than faking the UI. The tool
    renders a light editorial **"Feature guide"** page (Access-Control lineage): a scrollytelling
    walkthrough where each frame becomes an alternating step with a device mockup (your `preview`, or
    the placeholder when none), role-card personas, and scroll-unlock gamification (a sticky progress
    HUD, per-stage "cleared" badges, a completion reward). `confidence` values are optional metadata
    and are NOT shown to the end user. OPTIONAL, all backward-compatible: top-level `hero_image` (banner
    src) and `brand` {name, tagline, logo_src, href} for your own branding; per-persona `who`,
    `icon`, `includes`, and `tier` (>0 places it on the role ladder); a top-level `matrix`
    {columns, rows:[{capability, allowed[]}], caption} for a permission table; a `callout`
    {kicker, heading, intro, title, body, points[]} for an agent-style highlight; and a `game`
    {kicker, heading, intro, instructions, rounds:[{prompt, context[], options:[{label, correct,
    explain}]}], win_message} for an end-of-page click-to-check mini-game. It writes to
    `docs/features_memory/<slug>-story.html` and embeds a `chaos-storyboard-manifest` for agentic
    reads. Compose it from real understanding (run `chaos_feature_context`/`chaos_impact` first);
    never invent UI that does not exist. This is the user-facing sibling of
    `chaos_write_feature_website`: storyboard = users & experience (no code); feature website =
    engineers, graph, architecture, and source. It mirrors the
    `chaos storyboard <repo> --manifest <file.json>` CLI command.

    **Composing an ACCURATE guide (do this, in order):**
    1. **Validate every frame is a real, user-facing UI step.** Use `chaos_query` to check whether
       the action actually has a screen the end user touches, or is backend/server-only. A step the
       user never performs in the UI (e.g. a sponsored/server-submitted on-chain write, or an
       admin/CLI-only operation) does NOT belong in a user guide — drop it. Match each frame to the
       real screen and route (e.g. `Members › Invite member`), not an imagined one.
    2. **Use real screen captures.** Each frame's `preview` must be a REAL screenshot or a live
       route — Chaos cannot draw the client UI, and a frame with no `preview` renders an honest
       "add a screenshot" placeholder. ASK the user/dev to capture the screens (or point you at a
       running dev-server route); never hand-wave a mock.
    3. **Don't show confidence.** `confidence`/`overall_confidence` are optional internal metadata
       and are not rendered — omit them or leave them, but never describe them to the reader.
    4. **Brand it.** Easiest: set `"brand_preset": "molecule"` — a preset **shipped inside Chaos**
       (embedded in the binary, available to every install with no local files) that fills the
       logo, hero banner, and company for any branding fields you leave empty. Or set `brand`
       (name, tagline, logo_src, href) and `hero_image` yourself. Explicit values always win over the
       preset. (Run with no branding to get the neutral "Add your logo" placeholder.)
    5. **Keep images portable.** A `preview.src`/`hero_image` must be a `data:` URI (self-contained)
       or a path **relative to the output HTML** whose file you place alongside it — never an
       absolute or temp path, or the images vanish when the page is shared.
11. Use `chaos_change_plan` to decompose a proposed change into the FEATURES (L1 communities /
    god-nodes) it spans, with a dependency-aware check order. It matches the change description
    against community summary embeddings, **also seeding from a real git diff via `since` and from
    previously generated feature pages it correlates with** (shared files → communities), then ALWAYS
    writes an interactive HTML plan to `docs/features_memory/<slug>-plan.html` and returns a COMPACT
    JSON summary — per-feature label, confidence, `via` source (semantic/diff/manifest), `matched_by`
    breadcrumbs, check order, top symbols, top-level `provenance`, and the HTML path. The full plan
    lives only in the HTML, so it will not flood an agent context. It mirrors the
    `chaos change-plan <repo> "<change>" [--since <ref>]` CLI command.
12. Use `chaos_components` to explain the CORE COMPONENTS of a big area — the orientation step
    BEFORE feature extraction. An area like "OCL" is bigger than a single feature (it spans several
    L1 communities); pass an `area` (or omit it for a repo-level overview) and it surfaces those
    communities as COMPONENTS, each with its L3 summary, key symbols/files, languages, and a
    quotient-graph ROLE (entry/interface/core/foundation), plus how they connect and a
    dependency-first READ ORDER. It matches the area against community summary embeddings AND
    community labels (path-derived, so a directory-named area is caught) and correlates it with
    previously generated feature pages (shared files → `related_features`). It ALWAYS writes an
    interactive HTML overview to `docs/features_memory/<slug>-components.html` (embedding a
    `chaos-components-manifest` an agent can extract) and returns a COMPACT JSON summary. Use it to
    understand a large subsystem before drilling into any single feature, then follow up with
    `chaos_feature_context` / `chaos_write_feature_website` per component. It mirrors the
    `chaos components <repo> ["<area>"]` CLI command.
13. Use `chaos_features` to list ALL god-node FEATURES (L1 communities) that match a filter, grouped
    by journey layer (entry → interface → core → foundation) — the EXHAUSTIVE, uncurated counterpart
    to `chaos_components`. Where `chaos_components` curates and orders ONE area, `chaos_features`
    answers "give me EVERY feature [in this layer / under this folder / about this topic]". The single
    `filter` is AUTO-DETECTED: a path or real directory → FOLDER scope; a single layer word like
    `client`/`ui`/`api`/`core`/`contracts` → that journey LAYER (so "all client features" = every
    entry-layer feature); anything else → a TOPIC match (summary cosine + label/summary keywords);
    omit it for the whole repo. Force the interpretation with `layer`/`folder`/`topic`. Only a topic
    filter needs the embedder; layer/folder/whole-repo listing is embedder-free. It ALWAYS writes an
    interactive HTML inventory to `docs/features_memory/<slug>-features.html` (embedding a
    `chaos-features-manifest` an agent can extract) and returns a COMPACT JSON summary (resolved
    filter + how detected, per-layer + language counts, per-feature label/role/folders/symbols/
    `matched_by`, provenance). It mirrors the `chaos features <repo> ["<filter>"]` CLI command.
    Pass `project` instead of `repo` to list features across EVERY repo of a project in one
    journey-layered inventory — each card tagged with its repo alias and annotated with the
    project's cross-repo links; the HTML goes to the project workspace
    (`$CHAOS_PROJECT_DIR/<slug>/` or `~/.chaos/projects/<slug>/`).
14. Use `chaos_project` to work ACROSS REPOSITORIES. A project is a named set of indexed repos
    (client, backend, smart contracts, infra, …); Chaos detects FEATURE→FEATURE CROSS-REPO LINKS
    between members from the persisted index (consumer → provider): `package_dep` (one repo imports
    a package the other publishes), `abi` (client/backend code references a Solidity contract defined
    in the contracts repo), `http_route` (a fetch/axios call path matches a route registered in
    another repo). Links attach at the feature (L1) level, carry evidence + provenance breadcrumbs,
    and refresh AUTOMATICALLY after `chaos_analyze`/`chaos_add` on any member — gated by the L2 repo
    root hash, so a no-change re-index relinks nothing. Actions: `create` (idempotent), `add_repo`
    (attach an INDEXED repo under an alias like client/backend/contracts; links it immediately),
    `list`, `status` (members, staleness, links by kind, embedder consistency), `relink` (manual,
    `force` overrides the gate). All member repos must share ONE embedder config; `status` warns on
    mismatch. It mirrors the `chaos project create|add-repo|list|status|relink` CLI commands.

Treat `chaos_feature_context.warnings` as blocking for generated feature websites. If it says a
filesystem path exists but no Postgres hits referenced it, or that docs exist but no docs were
returned, do not call `chaos_write_feature_website` yet. Run `chaos_analyze`/`update`, or make a
more targeted `chaos_feature_context` call that names the missing subtree/docs, then compose the
website only after the missing evidence appears.

Feature websites are interactive, not prettified Markdown — Chaos's renderer guarantees the
interactive graph/story/architecture/code/evidence surfaces when you pass a manifest without
`html`. Only if you author `html` yourself (legacy) must it include the
`data-chaos-feature-website` / `data-chaos-graph` (clickable `data-node-id`) / `data-chaos-story`
(`data-story-step`) / `data-chaos-architecture` / `data-chaos-flow` / `data-chaos-code` /
`data-chaos-evidence` markers and JavaScript `addEventListener` interactivity.

The manifest must include at least three claims, two modes, five nodes, three edges, and three story
steps. If evidence is too thin for that, do not write a weak page; ask to index/query more first.
If the feature context has warnings, preserve them in your response and resolve them before writing
the page.

Do not tell the user "I only have chaos_query" if `chaos_feature_context` or
`chaos_write_feature_website` is available. If the MCP server cannot write the website because of
filesystem permissions, still return the feature context from MCP and say exactly that HTML
generation was blocked by filesystem access.

Never assume the current working directory is the Chaos Substrate checkout. The agent is usually
standing in the target project. Resolve the wrapper with this order:

1. If `chaos` is on `PATH`, use `chaos`.
2. Else if `CHAOS_SUBSTRATE_HOME` is set, use `$CHAOS_SUBSTRATE_HOME/bin/chaos`.
3. Else if the user gave an absolute Chaos Substrate checkout path, use
   `/absolute/path/to/chaos-substrate/bin/chaos`.
4. Else ask for the absolute Chaos Substrate checkout path.

In command examples, call this resolved wrapper as `$CHAOS`. If `chaos` is already on
`PATH`, set `CHAOS=chaos`. Always pass the target project as an absolute path or as
`"$PWD"` after confirming the shell is in the target project.

Natural language mapping:

- "Set up Chaos Substrate here" or "make this project use Chaos" -> MCP `chaos_analyze`; otherwise `$CHAOS onboard <repo-path>`
- "Go through this project and create sufficient index and explanation" -> MCP `chaos_analyze`, then `chaos_feature_context`, then compose and write with `chaos_write_feature_website`; otherwise `$CHAOS onboard <repo-path>` plus `$CHAOS explain <repo-path> "feature"`
- "Update index" or "refresh memory" -> MCP `chaos_analyze`; otherwise `$CHAOS update <repo-path>`
- "Add Chaos instructions to this project" -> `$CHAOS project-instructions <repo-path>` because this edits instruction files.
- "What context do I need for X?" -> MCP `chaos_feature_context` when available; otherwise `$CHAOS context <repo-path> "X"`
- "Generate explanation for X feature" -> MCP `chaos_feature_context`, then compose the website and call `chaos_write_feature_website`; otherwise `$CHAOS explain <repo-path> "X"`
- "Add this to Claude Code" or "use with Claude Cowork" -> `$CHAOS claude-code-add local <project-path>` or `$CHAOS claude-code-add project <project-path>`. The path is the target Claude Code project where config should be applied.
- "Use Ollama" or "set up local embeddings" -> run with `CHAOS_CONFIG=/absolute/path/to/chaos-substrate/chaos-substrate.local.toml`; `$CHAOS bootstrap`, `doctor`, `onboard`, `init`, and `update` enforce Ollama readiness.
- "Run MCP" -> `$CHAOS mcp`

The wrapper can install itself onto `PATH` with `bootstrap` or `install-agent`. It will build the
release binary if missing, start Docker Compose unless `CHAOS_NO_DOCKER=1` is set for `bootstrap`,
`doctor`, `onboard`, `init`, and `update`, run migrations, analyze the project, regenerate the
Obsidian vault, and generate dark feature-context websites when requested. `onboard` also writes
portable `AGENTS.md` and `CLAUDE.md` sections into the target project. `refresh` regenerates the
Obsidian vault from Postgres. In plugin/MCP mode, feature explanation websites must be composed by
the LLM from `chaos_feature_context` evidence and written with `chaos_write_feature_website`.

`context`, `explain`, `mcp`, and `claude-code-add` assume Postgres and the selected embedder are
already available. If they are not, run `$CHAOS doctor` or `$CHAOS init <repo-path>`
first. In sandboxed agents such as Claude Cowork, prefer the MCP tools exposed by the host-side MCP
server instead of trying to reach host Postgres directly from the sandbox.

For code changes inside Chaos Substrate itself:

1. Inspect existing docs and CLI help before changing behavior.
2. Keep patches narrow and Rust-native.
3. Prefer existing crate patterns and error types.
4. Validate CLI, persistence, embedding, generated websites, and MCP stdio paths when touched.
5. Report any skipped checks with concrete reasons.

## End-User Commands

Plugin package layout:

```text
.codex-plugin/plugin.json
.claude-plugin/plugin.json
.mcp.json
bin/chaos
skills/chaos-substrate/SKILL.md
```

Portable setup from any target project:

```sh
export CHAOS_SUBSTRATE_HOME=/absolute/path/to/chaos-substrate
export CHAOS="$CHAOS_SUBSTRATE_HOME/bin/chaos"

"$CHAOS" bootstrap
"$CHAOS" onboard "$PWD"
"$CHAOS" project-instructions "$PWD"
"$CHAOS" doctor
"$CHAOS" ollama-setup
"$CHAOS" init "$PWD"
"$CHAOS" update "$PWD"
"$CHAOS" context "$PWD" "authorization and RBAC"
"$CHAOS" explain "$PWD" "authorization and RBAC"
"$CHAOS" claude-code-add local "$PWD"
"$CHAOS" claude-code-add project "$PWD"
"$CHAOS" mcp
```

After `bootstrap`, future projects can usually use the shorter form:

```sh
chaos onboard "$PWD"
chaos context "$PWD" "authorization and RBAC"
chaos explain "$PWD" "authorization and RBAC"
```

If `chaos` is not found immediately after bootstrap, use:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

For Ollama:

```sh
export CHAOS_SUBSTRATE_HOME=/absolute/path/to/chaos-substrate
export CHAOS="$CHAOS_SUBSTRATE_HOME/bin/chaos"

CHAOS_CONFIG="$CHAOS_SUBSTRATE_HOME/chaos-substrate.local.toml" "$CHAOS" init "$PWD"
```

`bootstrap`, `doctor`, `onboard`, `init`, and `update` call the Ollama readiness check
automatically when the active config uses Ollama.

If Ollama is missing, tell the user to install it from `https://ollama.com/download`. On Linux the
official install command is usually `curl -fsSL https://ollama.com/install.sh | sh`. The default
local embedding model is `nomic-embed-text` with `dimensions = 768`.

Generated artifacts live in the target project:

```text
chaos-obsidian-vault
docs/features_memory
```

Do not scan the whole `docs/` tree for generated feature memory. Use `feature-context` or read only
direct HTML files in `docs/features_memory` with `chaos-feature-manifest`.

Use `graph` / `graph.html` for full indexed-graph validation and Obsidian for broad topic/node
exploration. Use `chaos_feature_context` plus `chaos_write_feature_website` for focused
implementation briefs and human-readable feature pages when MCP is available.

When creating or updating feature-memory websites, make story steps explicit. Each step should have
curated `node_ids` and, when useful, `edge_ids` in the manifest. Do not derive a story-step
highlight by transitive graph expansion from the first node. A step should highlight only the
subflow it explains; broader architecture or security-boundary highlights belong in `modes`.

## Validation Commands

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Use a real Postgres database with pgvector for persistence tests. Use real OpenAI or Ollama credentials/endpoints for embedding smoke tests.

## MCP Expectations

- MCP transport is stdio.
- The process should be launched directly by the agent client.
- Keep stdout protocol-clean; diagnostics should go to stderr or structured logging that does not corrupt MCP messages.
- The MCP server exposes THIRTEEN tools: `chaos_analyze`, `chaos_add`, `chaos_stats`, `chaos_query`,
  `chaos_feature_context`, `chaos_impact`, `chaos_write_feature_website`, `chaos_obsidian`,
  `chaos_refresh`, `chaos_write_storyboard`, `chaos_change_plan`, `chaos_components`, `chaos_features`, and `chaos_project`.
- `chaos_add` incrementally indexes only git-changed files (or explicit `paths`), refreshes the
  Obsidian vault, and writes a feature/bug page in one call; use it instead of a full
  `chaos_analyze` after small edits. The page carries provenance breadcrumbs and correlates the
  change with previously generated feature pages (`related_features`).
- `chaos_stats` is a read-only, embedder-free stats/breakdown tool: it reports index totals (files,
  nodes, edges, chunks, embedded vs missing, split chunks) plus breakdowns of nodes by kind, edges
  by kind, chunks by type, and files by language for an already-indexed repository; use it to
  explain or sanity-check what an analyze/add produced. It mirrors the `chaos stats <repo>` CLI
  command.
- `chaos_query` answers focused source-grounded questions; its optional `hierarchical` boolean (CLI
  `query --hierarchical`) does top-down retrieval that matches feature (community) summaries first and
  returns the surfaced features alongside the chunk hits, falling back to flat search when no
  hierarchy exists.
- `chaos_feature_context` is the MCP equivalent of `chaos context`; hits carry
  `metadata.retrieved_by` (semantic/keyword/literal) and the response includes provenance breadcrumbs.
- `chaos_impact` builds a feature-vs-existing-code impact report for an indexed repo; it ALWAYS
  writes an interactive HTML (impact summary plus evidence dashboard) to
  `docs/features_memory/<slug>-impact.html` and returns a COMPACT summary — counts plus the existing
  files/symbols the feature touches, warnings, provenance breadcrumbs, and the HTML path — keeping the
  full evidence in the HTML only so it will not flood an agent context like a raw
  `chaos_feature_context` dump. It mirrors the `chaos impact <repo> <feature>` CLI command.
- `chaos_write_feature_website` is the MCP-safe write path for LLM-composed feature explanation
  pages with embedded `chaos-feature-manifest` JSON.
- `chaos_obsidian` exports an already-indexed repository as an Obsidian vault read from the
  persisted graph; it lets an MCP-only agent generate the vault without shelling out to the CLI. It
  now also regenerates god-node community notes (`vault/Communities/*.md` plus a `Feature Map.md`)
  and an interactive `docs/features_memory/feature-map.html` from the persisted layers — no re-index
  and no embedder.
- `chaos_refresh` regenerates the Obsidian vault and, with `all_features=true`, re-renders the
  deterministic feature pages in `docs/features_memory` from their embedded manifests without
  re-indexing; it lets an MCP-only agent refresh pages without shelling out to the CLI. It also
  regenerates the god-node community notes (`vault/Communities/*.md` plus `Feature Map.md`) and the
  interactive `docs/features_memory/feature-map.html` from the persisted layers, with no re-index and
  no embedder.
- `chaos_write_storyboard` is the MCP-safe write path for a client/user-facing **"Feature guide"**: a
  code-free UI/UX user-story page (role-card personas, "As a … I want … so that …" stories, a
  scrollytelling walkthrough, outcomes) rendered in the
  shared light editorial theme (Access-Control lineage) and written to
  `docs/features_memory/<slug>-story.html` with an embedded `chaos-storyboard-manifest`. You pass a
  structured manifest only (no HTML); Rust owns the styling and the scroll-unlock gamification.
  Manifest minimums: at least 1 persona, 2 stories, 3 frames, and 1 outcome; every `confidence` and
  `overall_confidence` in `[0,1]` (confidence is optional metadata, NOT shown to end users); and every
  `story.frame_ids`/persona reference must resolve. Each walkthrough step pairs with a device mockup
  built from the frame's optional `preview` — `image` (a REAL screenshot/clip you captured; offline
  and private) or `iframe` (a live embed of a running app route); Chaos can't synthesise the client's
  screens, so a frame with no `preview` shows an honest "add a screenshot" placeholder (ask the
  user/dev for real captures). `src`/`url` must not use
  `javascript:`/`data:text/html`. Optional, backward-compatible extras let you match the full
  Access-Control look: `hero_image` + `brand` (your logo/company), persona `who`/`icon`/`includes`/
  `tier`, a permission `matrix`, an agent-style `callout`, and an end-of-page `game` (a click-to-check
  mini-game). It is user-facing — use
  `chaos_write_feature_website` for the engineer-facing graph/architecture/code page. It mirrors the
  `chaos storyboard <repo> --manifest <file.json>` CLI command.
- `chaos_change_plan` decomposes a proposed change into the FEATURES (L1 communities / god-nodes) it
  spans, with a dependency-aware check order. It matches the change description against community
  summary embeddings, also seeding from a real git diff via `since` and from previously generated
  feature pages it correlates with, then ALWAYS writes an interactive HTML plan to
  `docs/features_memory/<slug>-plan.html` and returns a COMPACT JSON summary — per-feature label,
  confidence, `via` source (semantic/diff/manifest), `matched_by` breadcrumbs, check order, top
  symbols, top-level `provenance`, and the HTML path — keeping the full plan in the HTML only so it
  will not flood an agent context. It mirrors the `chaos change-plan <repo> "<change>"
  [--since <ref>]` CLI command.
- `chaos_components` explains the CORE COMPONENTS of a big area — the orientation step BEFORE feature
  extraction. An area like "OCL" spans several L1 communities; given an `area` (or none, for a
  repo-level overview) it surfaces those communities as COMPONENTS, each with its L3 summary, key
  symbols/files, languages, and a quotient-graph ROLE (entry/interface/core/foundation), plus how
  they connect and a dependency-first READ ORDER. It matches the area against community summary
  embeddings AND community labels (path-derived, so a directory-named area is caught) and correlates
  it with previously generated feature pages (`related_features`). It ALWAYS writes an interactive
  HTML overview to `docs/features_memory/<slug>-components.html` (embedding a
  `chaos-components-manifest` an agent can extract) and returns a COMPACT JSON summary — component
  count, per-component label/role/read_order/top symbols/`matched_by`, relationships, related pages,
  top-level `provenance`, and the HTML path — keeping the full overview in the HTML only. Use it to
  understand a large subsystem before drilling into any single feature. It mirrors the
  `chaos components <repo> ["<area>"]` CLI command.
- `chaos_features` lists ALL god-node FEATURES (L1 communities) that match a filter, grouped by
  journey layer (entry → interface → core → foundation) — the EXHAUSTIVE, uncurated counterpart to
  `chaos_components`. The single `filter` is AUTO-DETECTED: a path or real directory → FOLDER scope
  (features whose code lives under it); a single layer word like `client`/`ui`/`api`/`core`/
  `contracts` → that journey LAYER (so "all client features" = every entry-layer feature); anything
  else → a TOPIC match (summary cosine + label/summary keywords); omit it for the whole repo. Force
  the interpretation with `layer`/`folder`/`topic`. Only a topic filter needs the embedder. It ALWAYS
  writes an interactive HTML inventory to `docs/features_memory/<slug>-features.html` (embedding a
  `chaos-features-manifest` an agent can extract) and returns a COMPACT JSON summary — resolved filter
  + how detected, total, per-layer + language counts, per-feature label/role/member_count/folders/top
  symbols/`matched_by`, top-level `provenance`, and the HTML path. It mirrors the
  `chaos features <repo> ["<filter>"]` CLI command.
