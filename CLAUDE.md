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
- `chaos_add`: incrementally index the files changed in git (or explicit `paths`), refresh the Obsidian vault, and write an interactive feature/bug page — in one call. Use after making changes instead of a full `chaos_analyze` when you only touched a few files.
- `chaos_stats`: report index statistics for an already-indexed repository read from Postgres — totals (files, nodes, edges, chunks, embedded vs missing, split chunks) plus breakdowns of nodes by kind, edges by kind, chunks by type, and files by language. Read-only and embedder-free; use to explain or sanity-check what an `chaos_analyze`/`chaos_add` produced.
- `chaos_query`: answer focused source-grounded questions. Pass `hierarchical: true` for top-down retrieval — the query is matched against feature (L1 community) summaries first and the surfaced features are returned alongside the chunk hits (boosted toward them), falling back to flat search when the repo has no hierarchy.
- `chaos_change_plan`: decompose a proposed change into the FEATURES (L1 communities / god-nodes) it spans, with a dependency-aware check order. Matches the change description against community summary embeddings, optionally also seeding from a real git diff (`since`); ALWAYS writes an interactive Blade-Runner HTML plan to `docs/features_memory/<slug>-plan.html` and returns a COMPACT JSON summary (per-feature label, confidence, check order, top symbols + the HTML path). Use it to answer "how many features does this change involve, and in what order should I check them?".
- `chaos_feature_context`: gather evidence for feature understanding.
- `chaos_impact`: build a feature-vs-existing-code impact report for an indexed repo and ALWAYS write an interactive HTML (impact summary + evidence dashboard) to `docs/features_memory/<slug>-impact.html`; returns only a compact JSON summary (counts, the existing files/symbols the feature touches, warnings, and the HTML path) so it won't flood agent context like a raw `chaos_feature_context` dump. Use it to see how a proposed feature maps onto the codebase as it is today (the before).
- `chaos_write_feature_website`: write an LLM-composed feature page with a manifest.
- `chaos_obsidian`: export an already-indexed repository as an Obsidian vault from the persisted graph (run after `chaos_analyze`, which never writes files).
- `chaos_refresh`: regenerate project-local artifacts (Obsidian vault, god-node community notes + `docs/features_memory/feature-map.html`, and with `all_features` the `docs/features_memory` pages) from the persisted index without re-indexing or calling the embedder.
- `chaos_write_storyboard`: write a CLIENT/USER-FACING storyboard — a code-free UI/UX user-story page (personas, "As a … I want … so that …" stories, clickable frames, outcomes, confidence rings) rendered in a fixed dark Blade Runner theme to `docs/features_memory/<slug>-story.html`. You pass a structured, code-free manifest only and Rust owns the styling. Each frame can embed the real UI via an optional `preview` (a captured screenshot/clip, or a live `iframe` of a running app route). This is the user-facing sibling of `chaos_write_feature_website` (which is for engineers: graph, architecture, code).

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

## Claude Code / Cowork MCP

Prefer the wrapper when registering this repository as an MCP server:

```sh
scripts/chaos-agent claude-code-add local
scripts/chaos-agent claude-code-add project /absolute/path/to/target-repo
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
