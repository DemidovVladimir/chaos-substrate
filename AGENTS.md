# Chaos Substrate Agent Instructions

Chaos Substrate is a Rust-only code knowledge memory for agents.

Use it to create and query a persistent knowledge base for Rust, Solidity, TypeScript, JavaScript, and Python repositories, with Markdown/MDX and PDF context. The memory is stored in Postgres + pgvector and survives process restarts.

TypeScript, JavaScript, Python, and Solidity are analysis targets only; they are extracted Rust-side and never run as a separate service.

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
cargo run -- add /path/to/repo -m "what changed"
cargo run -- stats /path/to/repo
cargo run -- query /path/to/repo "question"
cargo run -- feature-context /path/to/repo "task"
cargo run -- impact /path/to/repo "<feature>"
cargo run -- storyboard /path/to/repo --manifest story.json
cargo run -- graph /path/to/repo --output graph.html
cargo run -- mcp
```

## MCP Tool Surface

Agents should prefer MCP tools when available:

- `chaos_analyze`: index or refresh a repository.
- `chaos_add`: incrementally index git-diff changes (or explicit `paths`), refresh the Obsidian vault, and write a feature/bug page in one call.
- `chaos_stats`: report index statistics for an already-indexed repository from Postgres — totals (files, nodes, edges, chunks, embedded vs missing, split chunks) plus breakdowns of nodes by kind, edges by kind, chunks by type, and files by language. Read-only and embedder-free; use to explain or sanity-check what an analyze/add produced.
- `chaos_query`: answer focused source-grounded questions.
- `chaos_feature_context`: gather evidence for feature understanding.
- `chaos_impact`: build a feature-vs-existing-code impact report for an indexed repo and ALWAYS write an interactive HTML (impact summary + evidence dashboard) to `docs/features_memory/<slug>-impact.html`; returns a compact JSON summary (counts plus the existing files/symbols the feature touches, warnings, and the HTML path) so it does not flood agent context, framing how the feature maps onto the codebase as it is today (the "before").
- `chaos_sui_migration_impact`: produce a Sui migration impact report for an indexed Ethereum/Solana/mixed Web3 repo — auto-detects the source stack, maps each L1 feature onto Sui primitives (objects/dynamic fields, Coin/Kiosk/Display, capabilities, PTBs, events+GraphQL) with Walrus/Seal storage and access-control verdicts, always writes `docs/features_memory/sui-migration-impact.html`. Read-only, embedder-free, generates no Move code.
- `chaos_write_feature_website`: write an LLM-composed feature page with a manifest.
- `chaos_obsidian`: export an already-indexed repository as an Obsidian vault from the persisted graph (run after `chaos_analyze`, which never writes files).
- `chaos_refresh`: regenerate project-local artifacts (Obsidian vault and, with `all_features`, the feature pages) from the persisted index without re-indexing.
- `chaos_write_storyboard`: write a client/user-facing storyboard — a code-free UI/UX user-story page (personas, "As a … I want … so that …" stories, clickable frames, outcomes, confidence rings) in the shared light editorial theme to `docs/features_memory/<slug>-story.html`. Pass a structured, code-free manifest only; Rust owns the styling. Each frame can embed the real UI via an optional `preview` (screenshot/clip or live `iframe`). User-facing sibling of `chaos_write_feature_website` (engineers: graph/architecture/code).
- `chaos_stack`: report the tech stack of an already-indexed repository (dependencies by ecosystem, npm scripts, AWS CDK resources, JS/TS configs, language breakdown). Always writes `docs/features_memory/stack.html`. Read-only and embedder-free.
- `chaos_pages`: list the generated feature-memory pages of a repository — what chaos has already extracted. Scans `docs/features_memory` and returns every HTML page with its kind, title, and modified time. Read-only, embedder-free, pure filesystem.
- `chaos_gaps`: list knowledge gaps — files with no chunks (`coverage_gaps`) or no distinctive vocabulary (`vocabulary_gaps`). Pass `repo`, `repo`+`folder`, or `project`. Read-only and embedder-free.
- `chaos_change_plan`: decompose a proposed change into the features (L1 communities) it spans with a dependency-aware check order. Always writes `docs/features_memory/<slug>-plan.html` and returns a compact JSON summary.
- `chaos_components`: explain the core components of a big area before feature extraction. Always writes `docs/features_memory/<slug>-components.html` and returns a compact JSON summary.
- `chaos_features`: list all god-node features (L1 communities) that match a filter, grouped by journey layer. Always writes `docs/features_memory/<slug>-features.html` and returns a compact JSON summary. Pass `project` to span all member repos.
- `chaos_compose`: compose one page (or a per-feature site with `feature_pages: true`) from knowledge-base-backed sections (`features`, `correlations`, `stack`), tailored to a persona and style. Content-hashed — `cached: true` means do not re-ingest. Always writes `docs/features_memory/<slug>-composed.html`.
- `chaos_project`: manage cross-repository projects — create, add repos, list, check status, relink. Detects feature→feature cross-repo links (`package_dep`, `abi`, `http_route`) from the persisted index.
- `chaos_help`: returns the agent guide (tool order, typical workflows, token notes) as static text. No DB or embedder work.
- `chaos_clean`: **Destructive.** Wipes the persisted index (one repo or all); `artifacts: true` also deletes generated files on disk. Requires `confirm: true`. Use only on explicit user request.
- `chaos_graph`: export the indexed repo as a standalone interactive HTML graph from the persisted index. Embedder-free. Defaults to `docs/features_memory/graph.html`.

Do not synthesize feature pages from `chaos_query` alone when `chaos_feature_context` and
`chaos_write_feature_website` are available.

## Validation

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

For real repository indexing, configure either OpenAI or Ollama embeddings. If the embedder is unavailable, analysis must fail rather than producing fake vectors.

See `docs/EDITOR_SETUP.md` for install/registration and `RUNBOOK.md` for the ops and validation reference.
