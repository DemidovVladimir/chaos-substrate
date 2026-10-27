# Changelog

## 1.2.0 — 2026-10-27

Simpler: the Merkle tree is gone.

The index used to roll content hashes up into a tree (chunk → file → feature → repository) and
re-embedded a feature summary whenever its feature's hash moved. Nothing read the tree's root,
and search never used the tree at all. Its only job was that summary gate, and the gate was too
coarse: any edit inside a feature (a comment, a moved line) re-embedded the summary, even though
the summary text had not changed.

- A summary embedding is now stored under the hash of the summary text and the model
  (`summary_hash`). It is kept across re-clustering and handed back to any feature whose text
  has the same hash, whatever its id, so only a summary whose text changed is embedded again.
- Generated website pages are keyed by the hash of the data they show.
- Removed: `domain/merkle.rs`, the `repositories.root_hash`, `files.subtree_hash` and
  `communities.subtree_hash` columns (migration `002_drop_merkle.sql`), and the store methods
  that fed them. `set_repo_hashes` became `set_repo_commit`.
- `chaos doctor` names migrations that are not applied yet, and every tool stops with
  "run `chaos migrate`" instead of failing on a missing column.

Fixed: `add` lost the edges that point into an edited file from files that did not change
(purging the file cascaded them, and only the edited file is re-analyzed). The lost edges moved
feature boundaries, so summaries changed for no reason. `merge_files` now keeps those edges.

Measured on this repository (97 files, 1723 chunks, 33 features, embeddinggemma), with the same
edits for 1.1 and 1.2:

| Edit, then `add` | 1.1 | 1.2 |
| --- | --- | --- |
| nothing changed | 0 chunks, 0 summaries | 0 chunks, 0 summaries |
| one comment inside a test function | 1 chunk, 2 summaries, 2 edges lost | 1 chunk, 0 summaries, 0 edges lost |
| a blank line (the code below moves down) | 9 chunks, 1 summary | 9 chunks, 0 summaries |

Upgrade: run `chaos migrate`. The first `index`, or the first `add` that finds a change, embeds
every summary once (32 of 33 here), because the summary key changed; chunk embeddings are kept.

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
