# Changelog

## 1.0.0 — 2026-08-28

Ground-up rewrite. Chaos Substrate is now a small hexagonal Rust crate
(`src/domain`, `src/ports`, `src/application`, `src/adapters`, `src/entry`)
with one job: build a knowledge graph of one or more repositories so an agent
can answer feature questions from memory instead of re-reading source.

The previous code base (0.24.0) is preserved as the git tag
`archive/v0.24-legacy`. There is no upgrade path for its database schema:
run `chaos migrate --reset` once to drop the legacy schema and create the new
one, then `chaos index` each repository.

### The six tools

Each is a CLI subcommand (`chaos <name>`) and an MCP tool (`chaos_<name>`);
`src/application/tools.rs` is the single source of truth for both.

- `index` — full index of a repository: tree-sitter extraction → project-wide
  import/call resolution → contextual chunks → real embeddings → feature
  communities (L1) → Merkle hashes (L2) → hash-gated summaries (L3).
- `reindex [--force]` — full re-run; a no-op when HEAD is indexed and the
  worktree is clean. Unchanged chunks keep their embeddings (content-hash reuse).
- `add [--since <ref>]` — incremental index of the files changed in git;
  deleted and renamed-away files are purged; communities and summaries are
  recomputed only where a feature's hash changed.
- `feature "<question>"` — token-bounded answer from the index: matching
  features, code excerpts with exact line ranges, a 1-hop call/import graph,
  and the env vars / HTTP routes involved. Persisted as
  `docs/features_memory/<slug>.html` when its hash changes.
- `compose` — the feature website (index + one page per feature) under
  `docs/features_memory/site/`, content-hash gated per page.
- `correlations` — intra-repo pairs of features sharing files or symbols, plus
  cross-repo consumer→provider links (`package_dep`, `http_route`) across the
  repos of a `chaos.toml` project. Writes `correlations.html`.

CLI-only plumbing: `migrate [--reset]`, `doctor`, `mcp`, `help`.

### New

- Projects are a `chaos.toml` file next to the repos (`name` + `[[repos]]`
  with `alias`/`path`), found by walking up from the current directory or
  passed with `--project`. `--repo <alias|path>` selects a member.
- tree-sitter parsing for Rust, TypeScript, JavaScript, Python, Solidity and
  GraphQL (including `gql`-embedded documents in TS/JS/Python); Markdown via
  pulldown-cmark. Per-file extraction runs in parallel.
- Contextual chunks: one AST unit plus a context header; oversize units are
  split by an embedding-guided merge using the same encoder that indexes them.
- One migration (`migrations/001_init.sql`, 8 tables) replaces the 0.x series.
- Runtime config: `chaos-substrate.toml` (`[storage]`, `[embedding]`,
  `[indexing].skip_dirs`) with `DATABASE_URL` / `CHAOS_EMBED_*` env overrides.

### Removed

- Every 0.x tool and command that is not one of the six above, the
  `project` subcommands and their database tables, the Claude Code hooks
  (`.claude-plugin/hooks/`), and PDF / AWS CDK extraction. Cross-repo `abi`
  and `graphql` link kinds are not carried over.
- Hand-written parser integrations (syn, oxc, rustpython, solang,
  apollo-parser) in favour of tree-sitter grammars.
- Mock or random embedders never existed and still do not.
