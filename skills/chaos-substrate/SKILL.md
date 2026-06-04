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
- Agent surfaces are Codex plugin, Claude Code plugin, CLI, the `chaos-agent` wrapper, static
  graph/Obsidian exports, generated feature-context websites, feature-memory manifests, and MCP over
  stdio.
- Source code is prioritized over docs during code-repository queries. Markdown/MDX docs and
  extractable text PDFs are indexed as supplemental context, not ignored.

## Hard Boundaries

- Do not edit `Cargo.toml` or `src/` unless the user explicitly asks.
- Do not add mock embeddings, fake vector stores, in-memory persistence, HTTP APIs, Python services, TypeScript services, or live browser services. A standalone Rust-generated `graph.html` export is allowed for persisted graph validation. TypeScript, JavaScript, Python, and Solidity are analysis targets only: their extraction belongs in the Rust extractor, never in a sidecar runtime in another language.
- Do not downgrade persistence guarantees; memory must survive process restarts.
- Do not replace real embedders with deterministic test-only behavior in production paths.

## Working Pattern

When a user asks to use Chaos Substrate in a target project, prefer MCP tools over shell commands.
Use the `chaos-agent` wrapper for setup, debugging, or when MCP is unavailable.

If MCP tools are available, prefer them over shelling out:

1. Use `chaos_analyze` to index or refresh a repository.
2. Use `chaos_add` for one-shot incremental indexing of git-changed files (or explicit `paths`); it
   re-embeds only the changed chunks, refreshes the Obsidian vault, and writes a feature/bug page in
   a single call. A full `chaos_analyze` is still needed to rebuild cross-file call edges into
   unchanged files.
3. Use `chaos_stats` to report index statistics for an already-indexed repository, read from
   Postgres: totals (files, nodes, edges, chunks, embedded vs missing, split chunks) plus breakdowns
   of nodes by kind, edges by kind, chunks by type, and files by language. It is read-only and
   embedder-free; use it to explain or sanity-check what an `chaos_analyze`/`chaos_add` produced.
4. Use `chaos_query` for focused questions.
5. Use `chaos_feature_context` when the user asks to explain a feature, prepare implementation
   context, or generate a feature explanation.
6. Use `chaos_impact` to build a feature-vs-existing-code impact report for an indexed repo. It
   ALWAYS writes an interactive HTML (impact summary plus evidence dashboard) to
   `docs/features_memory/<slug>-impact.html` and returns a COMPACT JSON summary — counts plus the
   existing files/symbols the feature touches, warnings, and the HTML path. The full evidence lives
   only in the HTML, so it will not flood an agent context like a raw `chaos_feature_context` dump.
   Use it to see how a proposed feature maps onto the codebase as it exists today (the before). It
   mirrors the `chaos impact <repo> <feature>` CLI command.
7. Use `chaos_write_feature_website` only after reading `chaos_feature_context` output and composing
   a feature-specific website plus manifest. The LLM must decide the feature story, claims, nodes,
   and flow from evidence; the tool only writes the artifact.
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
    app route (renders only while that server is up). Chaos only embeds it — it never runs a browser;
    capture the screenshot yourself (e.g. via the host or Playwright) or point at the user's running
    dev server. The tool renders a fixed dark Blade Runner
    page with click-a-frame detail and confidence rings, writes it to
    `docs/features_memory/<slug>-story.html`, and embeds a `chaos-storyboard-manifest` for agentic
    reads. Compose it from real understanding (run `chaos_feature_context`/`chaos_impact` first);
    never invent UI that does not exist. This is the user-facing sibling of
    `chaos_write_feature_website`: storyboard = users & experience (no code); feature website =
    engineers, graph, architecture, and source. It mirrors the
    `chaos storyboard <repo> --manifest <file.json>` CLI command.

Treat `chaos_feature_context.warnings` as blocking for generated feature websites. If it says a
filesystem path exists but no Postgres hits referenced it, or that docs exist but no docs were
returned, do not call `chaos_write_feature_website` yet. Run `chaos_analyze`/`update`, or make a
more targeted `chaos_feature_context` call that names the missing subtree/docs, then compose the
website only after the missing evidence appears.

Feature websites must be interactive, not prettified Markdown. Before calling
`chaos_write_feature_website`, the HTML must include:

- `data-chaos-feature-website` root
- `data-chaos-graph` graph surface with clickable `data-node-id` nodes
- `data-chaos-story` with clickable `data-story-step` entries
- `data-chaos-architecture` section
- `data-chaos-flow` section
- `data-chaos-code` source/code context section
- `data-chaos-evidence` evidence/uncertainty section
- JavaScript `addEventListener` handlers for graph/story/code navigation

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

1. If `chaos-agent` is on `PATH`, use `chaos-agent`.
2. Else if `CHAOS_SUBSTRATE_HOME` is set, use `$CHAOS_SUBSTRATE_HOME/bin/chaos-agent`.
3. Else if the user gave an absolute Chaos Substrate checkout path, use
   `/absolute/path/to/chaos-substrate/bin/chaos-agent`.
4. Else ask for the absolute Chaos Substrate checkout path.

In command examples, call this resolved wrapper as `$CHAOS_AGENT`. If `chaos-agent` is already on
`PATH`, set `CHAOS_AGENT=chaos-agent`. Always pass the target project as an absolute path or as
`"$PWD"` after confirming the shell is in the target project.

Natural language mapping:

- "Set up Chaos Substrate here" or "make this project use Chaos" -> MCP `chaos_analyze`; otherwise `$CHAOS_AGENT onboard <repo-path>`
- "Go through this project and create sufficient index and explanation" -> MCP `chaos_analyze`, then `chaos_feature_context`, then compose and write with `chaos_write_feature_website`; otherwise `$CHAOS_AGENT onboard <repo-path>` plus `$CHAOS_AGENT explain <repo-path> "feature"`
- "Update index" or "refresh memory" -> MCP `chaos_analyze`; otherwise `$CHAOS_AGENT update <repo-path>`
- "Add Chaos instructions to this project" -> `$CHAOS_AGENT project-instructions <repo-path>` because this edits instruction files.
- "What context do I need for X?" -> MCP `chaos_feature_context` when available; otherwise `$CHAOS_AGENT context <repo-path> "X"`
- "Generate explanation for X feature" -> MCP `chaos_feature_context`, then compose the website and call `chaos_write_feature_website`; otherwise `$CHAOS_AGENT explain <repo-path> "X"`
- "Add this to Claude Code" or "use with Claude Cowork" -> `$CHAOS_AGENT claude-code-add local <project-path>` or `$CHAOS_AGENT claude-code-add project <project-path>`. The path is the target Claude Code project where config should be applied.
- "Use Ollama" or "set up local embeddings" -> run with `CHAOS_CONFIG=/absolute/path/to/chaos-substrate/chaos-substrate.local.toml`; `$CHAOS_AGENT bootstrap`, `doctor`, `onboard`, `init`, and `update` enforce Ollama readiness.
- "Run MCP" -> `$CHAOS_AGENT mcp`

The wrapper can install itself onto `PATH` with `bootstrap` or `install-agent`. It will build the
release binary if missing, start Docker Compose unless `CHAOS_NO_DOCKER=1` is set for `bootstrap`,
`doctor`, `onboard`, `init`, and `update`, run migrations, analyze the project, regenerate the
Obsidian vault, and generate dark feature-context websites when requested. `onboard` also writes
portable `AGENTS.md` and `CLAUDE.md` sections into the target project. `refresh` regenerates the
Obsidian vault from Postgres. In plugin/MCP mode, feature explanation websites must be composed by
the LLM from `chaos_feature_context` evidence and written with `chaos_write_feature_website`.

`context`, `explain`, `mcp`, and `claude-code-add` assume Postgres and the selected embedder are
already available. If they are not, run `$CHAOS_AGENT doctor` or `$CHAOS_AGENT init <repo-path>`
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
bin/chaos-agent
skills/chaos-substrate/SKILL.md
```

Portable setup from any target project:

```sh
export CHAOS_SUBSTRATE_HOME=/absolute/path/to/chaos-substrate
export CHAOS_AGENT="$CHAOS_SUBSTRATE_HOME/bin/chaos-agent"

"$CHAOS_AGENT" bootstrap
"$CHAOS_AGENT" onboard "$PWD"
"$CHAOS_AGENT" project-instructions "$PWD"
"$CHAOS_AGENT" doctor
"$CHAOS_AGENT" ollama-setup
"$CHAOS_AGENT" init "$PWD"
"$CHAOS_AGENT" update "$PWD"
"$CHAOS_AGENT" context "$PWD" "authorization and RBAC"
"$CHAOS_AGENT" explain "$PWD" "authorization and RBAC"
"$CHAOS_AGENT" claude-code-add local "$PWD"
"$CHAOS_AGENT" claude-code-add project "$PWD"
"$CHAOS_AGENT" mcp
```

After `bootstrap`, future projects can usually use the shorter form:

```sh
chaos-agent onboard "$PWD"
chaos-agent context "$PWD" "authorization and RBAC"
chaos-agent explain "$PWD" "authorization and RBAC"
```

If `chaos-agent` is not found immediately after bootstrap, use:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

For Ollama:

```sh
export CHAOS_SUBSTRATE_HOME=/absolute/path/to/chaos-substrate
export CHAOS_AGENT="$CHAOS_SUBSTRATE_HOME/bin/chaos-agent"

CHAOS_CONFIG="$CHAOS_SUBSTRATE_HOME/chaos-substrate.local.toml" "$CHAOS_AGENT" init "$PWD"
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
- MCP tools are `chaos_analyze`, `chaos_add`, `chaos_stats`, `chaos_query`, `chaos_feature_context`,
  `chaos_impact`, `chaos_write_feature_website`, `chaos_obsidian`, `chaos_refresh`, and
  `chaos_write_storyboard`.
- `chaos_add` incrementally indexes only git-changed files (or explicit `paths`), refreshes the
  Obsidian vault, and writes a feature/bug page in one call; use it instead of a full
  `chaos_analyze` after small edits.
- `chaos_stats` is a read-only, embedder-free stats/breakdown tool: it reports index totals (files,
  nodes, edges, chunks, embedded vs missing, split chunks) plus breakdowns of nodes by kind, edges
  by kind, chunks by type, and files by language for an already-indexed repository; use it to
  explain or sanity-check what an analyze/add produced. It mirrors the `chaos stats <repo>` CLI
  command.
- `chaos_feature_context` is the MCP equivalent of `chaos-agent context`.
- `chaos_impact` builds a feature-vs-existing-code impact report for an indexed repo; it ALWAYS
  writes an interactive HTML (impact summary plus evidence dashboard) to
  `docs/features_memory/<slug>-impact.html` and returns a COMPACT summary — counts plus the existing
  files/symbols the feature touches, warnings, and the HTML path — keeping the full evidence in the
  HTML only so it will not flood an agent context like a raw `chaos_feature_context` dump. It mirrors
  the `chaos impact <repo> <feature>` CLI command.
- `chaos_write_feature_website` is the MCP-safe write path for LLM-composed feature explanation
  pages with embedded `chaos-feature-manifest` JSON.
- `chaos_obsidian` exports an already-indexed repository as an Obsidian vault read from the
  persisted graph; it lets an MCP-only agent generate the vault without shelling out to the CLI.
- `chaos_refresh` regenerates the Obsidian vault and, with `all_features=true`, re-renders the
  deterministic feature pages in `docs/features_memory` from their embedded manifests without
  re-indexing; it lets an MCP-only agent refresh pages without shelling out to the CLI.
- `chaos_write_storyboard` is the MCP-safe write path for a client/user-facing storyboard: a
  code-free UI/UX user-story page (personas, "As a … I want … so that …" stories, clickable frames,
  outcomes, and confidence rings) rendered in a fixed dark Blade Runner theme and written to
  `docs/features_memory/<slug>-story.html` with an embedded `chaos-storyboard-manifest`. You pass a
  structured manifest only (no HTML); Rust owns the styling. Manifest minimums: at least 1 persona,
  2 stories, 3 frames, and 1 outcome; every `confidence` and `overall_confidence` in `[0,1]`; and
  every `story.frame_ids`/persona reference must resolve. Each frame may add an optional `preview`
  to embed the REAL UI — `image` (a screenshot/clip you captured; offline and private) or `iframe`
  (a live embed of a running app route); `src`/`url` must not use `javascript:`/`data:text/html`.
  It is user-facing — use
  `chaos_write_feature_website` for the engineer-facing graph/architecture/code page. It mirrors the
  `chaos storyboard <repo> --manifest <file.json>` CLI command.
