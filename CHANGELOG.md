# Changelog

## 1.4.0 — 2026-11-12

Contracts and GraphQL resolvers, end to end; `add` and `reindex` start from the last indexed
commit.

### Added

- `add` and `reindex` work from a stamp. Every successful `index`, `reindex` and `add` records the
  HEAD it read, the paths uncommitted at that moment and its listing rules (`skip_dirs`, Foundry
  `libs`, the indexable languages). The next run looks only at the files changed between the
  stamped commit and the current HEAD (a two-tree diff, so branch switches and rebases work), the
  files uncommitted now (untracked included) and the files uncommitted at the stamp. Each is kept
  only when the full listing would have it, and re-read only when its content hash differs from
  the indexed one. Both reports carry a `delta` summary.
- Solidity declarations are symbols: state variables, file-level constants and custom errors
  are `Field` nodes, user-defined value types are type aliases, and `constructor`, `receive`
  and `fallback` are methods. A declaration-only file such as `src/types/Constants.sol` used to
  hold only its file node.
- Solidity metadata: contracts and interfaces record `bases` (the names after `is`), `header`
  and `abstract`; functions record `params` and `returns`; state variables record `state_var`,
  `visibility`, `type` and `constant` / `immutable`, and public ones `getter`; errors and events
  record `params` (events with `indexed`); a NatSpec comment becomes `natspec` (`notice`,
  `dev`, `params`, `returns`, `inheritdoc`).
- Selectors from source: public and external functions of contracts and interfaces, public
  getters, custom errors and events record `abi_signature` (canonical, e.g.
  `transfer(address,uint256)`) and `selector` (the first 4 bytes of its keccak-256), or `topic0`
  for an event. When a type comes from code outside the index, `abi_unresolved` names it
  instead. The chunk header of such a function gains a `// abi: <signature> <selector>` line.
- Edge kinds `emits` (function → the event it emits) and `reverts` (function → the custom error
  of a `revert E()` or `require(cond, E())`).
- Foundry layout: `remappings.txt` and the `foundry.toml` profiles (`remappings`, `libs`, `src`)
  steer Solidity imports the way solc resolves them: relative, then remapped, then from the
  project root (`import "src/X.sol"`), then from the libs. An import that reaches no indexed
  file names a package by its remapping key (`forge-std`). Neither file is indexed.
- Foundry `vm.env*` reads are env-var surfaces.
- Contract ABIs are a language (`abi`), found by path only: `*.abi`, `*.abi.json`, and `.json`
  files under a directory named `abi` or `abis` (any case). A plain ABI array or a
  Foundry/Hardhat artifact becomes one `abi` node over its ABI array; every other `.json` file
  stays unindexed. In TS/JS, `as const` ABI arrays, `parseAbi([...])` / `parseAbiItem(...)`,
  arrays of human-readable signatures and combined `[...A, ...B]` ABIs are `abi` nodes too.
- Contract calls: objects with `functionName` / `eventName` / `abi` keys (viem `readContract` /
  `writeContract`, wagmi hooks, `decodeEventLog`, `encodeFunctionData`, …) are recorded on the
  File node (`metadata.contract_calls`), and a `uses_abi` edge joins the caller to the ABI it
  names. An ABI the index does not hold (viem's `erc20Abi`) gets no edge.
- `correlations` link kind `contract_abi`: a contract call, or a Solidity interface one repo
  keeps of another repo's contract, → the feature of the function, getter or event it reaches.
  Tiers, tried in order: named (the ABI names the contract; 0.85), overlap (the ABI's functions
  match one contract; 0.7), unique (exactly one contract has the member; 0.6), inherited (the
  contract inherits the member from a base the index does not hold, such as OpenZeppelin
  `ERC721`, and no other indexed contract declares it; 0.6). Signatures must agree. Test files
  are not consumers.
- GraphQL resolvers: each `case "<field>":` group of a `switch (fieldName)` dispatcher (an
  AppSync Lambda) is a `resolver` node with a `resolves` edge to the SDL field it implements.
  An object literal with literal `typeName` and `fieldName` (AppSync `getResolvers()` entries,
  `createResolver`) `binds` that field; Lambda `entry:` paths are kept on the File node
  (`metadata.lambda_entries`). A field registered only when the CDK app runs keeps no resolver:
  it is never guessed.
- `feature` anchors on a GraphQL field: a `Type.field` coordinate in the question, a root field
  name next to a word such as query, mutation, subscription, resolver, graphql or field, or the
  new optional `entry` argument (CLI `--entry Mutation.createLab`). The answer leads with an
  `entry` block: the SDL, the registration, the resolver, the calls behind it (up to 3 hops) and
  the operations and hooks that call the field, trimmed to a quarter of the budget. Across repos
  the members' blocks are joined, and `entry_links` joins a calling operation in one repo to the
  resolver in another.
- Entry points: GraphQL rows name the resolver and its registration; contract rows add public
  getters, each function's selector, NatSpec notice, emitted events and reverted errors, each
  contract's custom errors, and the callers found across the project with their tier; client
  hooks list the contract calls they make. `compose` returns two more counts,
  `graphql_fields_resolved` and `contract_callers`.

### Changed

- A `file_head` chunk longer than 1500 characters becomes `part i/n` chunks, in every language,
  instead of being cut off. A file whose head fits keeps its chunk id. A test file keeps only
  the first part: its top level is test bodies, which would crowd real code out of retrieval.
- Foundry dependency directories are skipped by path in `index`, `reindex` and `add`: every
  directory a `foundry.toml` names in `libs` (`lib/` when it names none). No flag is needed.
- Solidity names resolve in solc's scope: the contract, then its ancestors (most-derived
  first), then file-level declarations, never another contract's member. This resolves
  `super.f()`, `T(x).f()`, inherited state-variable receivers and modifier invocations. A call
  on a receiver of unknown type reaches free functions only, so `token.mint()` no longer binds
  the caller's own `mint`. An override `implements` the one method of a direct base with the
  same name and parameter types.
- `graphql_field` links: operations in test files are not consumers, and when the schema owner
  has a resolver for the field, the link lands on the resolver's feature, the evidence names
  the resolver, and the confidence is capped by the `resolves` edge.
- Custom errors, state variables and constants never name a feature.
- `chaos-substrate.toml` and `chaos-substrate.local.toml` also skip `.next` and `coverage`, so
  they hold the built-in default as their comment says.
- `add` no longer hashes every file on disk. It reconciles every file only when the stamp is
  unusable, once, with the reason in `delta.fallback`: no stamp yet, a stamp written before
  1.4.0, a run that failed (the reconcile then also finishes embedding and the hierarchy), other
  listing rules, or a stamped commit this clone does not have.
- `add --since <ref>` adds `ref..HEAD` to the stamp delta; a ref that names no commit is an
  error.
- `reindex` without `force` re-reads the `add` delta plus the unchanged files one hop upstream
  of each change (an `imports`, `calls`, `extends`, `implements`, `queries`, `emits`, `reverts`,
  `binds`, `resolves` or `uses_abi` edge into it) instead of running a full index. It is a no-op
  only when HEAD is the stamp, the worktree is clean, nothing was uncommitted at the stamp and
  the listing rules are unchanged.
- `index` writes its stamp only after it succeeded, so an interrupted run leaves none to trust.

### Fixed

- Python GraphQL documents written as an implicit string concatenation
  (`"mutation X " "{ x }"`, f-string pieces included) parse as one document.
- `new A.B(...)` in Solidity is a call candidate for `B` on `A`, not for a member of `new A`.
- Git paths are relative to the indexed directory when it sits below the git top level, and a
  `--since` ref that looks like an option cannot reach git as one.
- Docs: README lists `graphql_field` among the cross-repo links, and the ARCHITECTURE
  correlations paragraph no longer splices `graphql_field` into the `min_confidence` sentence.

### Upgrade

Rebuild every binary that reads the database first (the MCP server too): 1.3 cannot parse the
new node and edge kinds. Run `chaos migrate` (migration 003 adds the stamp columns
`worktree_paths` and `listing_rules`). Extraction changed for files whose content did not, and
the delta re-extracts changed files only, so run `chaos reindex --force --repo <alias>` on every
repository of the project, then `chaos compose --all-repos`. Only chunks whose text changed are
embedded. Without the forced reindex, the first `add` of each repository still reconciles every
file once and the first `reindex` runs in full, because their stamps predate worktree tracking.

## 1.3.1 — 2026-11-11

- The `chaos` wrapper injects its config when global flags come before the subcommand
  (`chaos --repo X feature ...` failed with "DATABASE_URL is required"). A caller's own
  `--config` still wins.
- Docs: the wrapper help and RUNBOOK list `--repos`/`--all-repos` for `feature` and `compose`
  and the project hub's location; RUNBOOK covers `reindex --force` after an extractor upgrade
  and deleting a page to pick up a new layout. Feature pages name MCP tools among user surfaces.

## 1.3.0 — 2026-11-11

A whole project on one page.

- `compose --repos a b` / `--all-repos` composes every selected repo's site, runs
  `correlations`, and writes a project hub (`<project>/docs/features_memory/index.html`): what was
  indexed per repo, a feature graph across the repos (features as nodes, coloured by dominant
  language; call/import weight inside a repo and cross-repo links as edges; a detail panel per
  feature), every feature with a filter, the cross-repo links with evidence, and the project
  feature pages in that directory.
- `correlations` links GraphQL across repos (`graphql_field`): a GraphQL operation (in a
  `.graphql` file or embedded in TS/Python) → the feature defining the root field it calls in the
  schema owner's SDL. Operations record their root fields in `metadata.root_fields`; re-index a
  consumer repo to fill it.
- Generated pages follow the viewer's dark theme (token-level light and dark palettes).
- The project hub lists the entry points: GraphQL root fields in the schema owner's repo (one-line
  header with arguments and auth directives, plus every calling operation) and every
  public/external Solidity function per contract and interface (read/write, modifiers). Features
  holding entry points are always drawn and carry a dotted ring.
- Solidity functions record `metadata.visibility`, `mutability`, `modifiers` and a one-line
  `header`; GraphQL SDL fields record a one-line `header` without descriptions. The first-line
  `signature` (used in chunk text) is unchanged, so no re-embedding is needed — `reindex --force`
  fills the metadata.

Entry points for every kind of content.

- Entry points are found per content type and listed on the project hub AND on every repo's site
  index, with one filter: the GraphQL API, smart-contract functions, MCP tools, HTTP routes,
  client hooks, the document plan, and transcript timelines. One module
  (`application::entry_points`) derives them from the index for both pages.
- MCP tools are a new surface (`mcp_tool` nodes, with a one-line `detail`): TS
  `server.registerTool` / `server.tool` / `server.addTool` (a tool name held in a same-file
  `const` is resolved), Python `@x.tool(...)` and `Tool(name=…)`, Rust rmcp `#[tool(...)]`. They
  also show in `feature` answers and on feature pages.
- Next.js route handlers are HTTP routes: `export function GET` in `app/**/route.ts` →
  `GET /path`, a default export in `pages/api/**` → `ANY /api/…`.
- React hooks that wrap a data hook (`useQuery`, `useMutation`, Apollo, urql, SWR, wagmi, tRPC)
  record `metadata.hook` (`kinds`, `wraps`); the entry points add the GraphQL fields their calls
  reach.
- Transcripts: `.srt` / `.vtt` files are indexed (language `transcript`), and a Markdown section
  that is mostly timestamped lines is too: both are cut into time segments, each a section named
  by its start time. A heading that opens with a timestamp is a chapter mark.
- A repository whose files are mostly Markdown gets a document plan: each file's title and
  chapters.
- Fix: YAML front matter (`---` … `---` at the top of a Markdown file) is one block. Its
  `title: x` line followed by `---` was read as a heading, and its `# comments` as H1 sections.
  Its `title:` is kept on the File node (`metadata.title`) and names the document in the plan.
- `compose` returns `entry_points` (count per category) instead of `graphql_fields` /
  `contract_functions`.

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
