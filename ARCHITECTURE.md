# Architecture

Chaos Substrate 1.0 is one Rust crate with a hexagonal layout: pure domain logic in the middle,
traits ("ports") around it, use-cases built on those traits, and concrete adapters (Postgres,
tree-sitter, Ollama/OpenAI, git, filesystem) plugged in at the edge.

## Layers and the dependency rule

```
src/domain/       pure data + algorithms; no I/O, no async, no crate-internal imports
src/ports/        traits the use-cases depend on; import domain only
src/application/  use-cases (index, add, feature, compose, correlations); import domain + ports
src/adapters/     implementations of the ports
src/entry/        CLI (clap), MCP server, doctor; wire adapters into use-cases
```

The rule: `domain` depends on nothing inside the crate; `ports` on `domain`; `application` on
`domain` + `ports`; `adapters` implement ports; `entry` wires them. It is enforced by the
`layering` tests in `src/main.rs`, which read every `.rs` file under a layer and fail if it
mentions `crate::<forbidden layer>` (`domain_is_pure`, `ports_depend_on_domain_only`,
`application_never_touches_adapters`).

Ports (`src/ports/`): `IndexStore` (repos, files, nodes, edges, chunks, embeddings),
`SearchStore` (the retrieval channels), `CommunityStore` (communities + summaries), `Embedder`
(real vectors only; tests use scripted vectors behind the trait, runtime code has no fake),
`SourceExtractor` (file-local extraction), `Vcs` (git + tree listing), `PageStore` (HTML pages
from a manifest).

`src/application/tools.rs` is the single source of truth for the six tools: it drives the CLI
help, the MCP `tools/list` roster, and a test that both stay in sync.

## Pipeline (`index`)

1. **List and read files.** `Vcs` lists the tracked tree honoring ignore rules and
   `indexing.skip_dirs`; files over 2 MiB are skipped. The entry layer reads the repository's
   Foundry layout (`adapters/foundry.rs`: `remappings.txt` and the `foundry.toml` profiles'
   `remappings`, `libs` and `src`; neither file is indexed) into `IndexOptions.solidity`, and the
   `libs` directories (default `lib/`) are dropped from the listing by path (`skip_paths`).
2. **Extract** (`adapters/treesitter`). Per file, in parallel, tree-sitter for Rust, TS/JS,
   Python, Solidity, GraphQL (plus `gql` documents embedded in TS/JS/Python) and ABI JSON
   (tree-sitter-json); a heading-based extractor for Markdown; a line scanner for `.srt`/`.vtt`
   transcripts (subtitles have no grammar). Output is file-local: symbols with byte/line ranges,
   doc comments, unresolved import and call candidates, and user surfaces (env vars read, HTTP
   routes and MCP tools registered) found by node-shape walks, never regex. Entry-point passes
   (`treesitter/entry.rs`) add MCP tools, Next.js route handlers (routes from the file path) and
   `metadata.hook` on React hooks that wrap a data hook. Transcripts, and Markdown sections that
   are mostly timestamped lines, are cut into time segments (`treesitter/transcript.rs`): one
   `Section` per segment, named by its start time.
   - Solidity (`treesitter/solidity.rs`) also emits state variables, file-level constants and
     custom errors as `Field`s, `emit` / `revert` as relations, and the metadata the ABI pass
     reads (`bases`, `params`, `returns`, `getter`, `natspec`); the module doc lists every key.
   - ABIs (`treesitter/abi.rs`): an ABI JSON file (`Language::Abi`, by path: `*.abi`,
     `*.abi.json`, `.json` under `abi/` or `abis/`) and TS/JS ABI constants become `Abi` nodes;
     objects with `functionName` / `eventName` / `abi` keys become `contract_calls`.
   - GraphQL resolvers (`treesitter/resolvers.rs`, TS/JS): each `case "<field>":` group of a
     `switch (fieldName)` is a `Resolver`; a `{typeName, fieldName}` object literal is a
     registration (`graphql_bindings`); a Lambda construct's `entry:` path is kept.
3. **Analyze** (`application/analyze.rs`). Builds the L0 graph and resolves relations
   project-wide, never fuzzily. Imports become `Imports` edges between file nodes (confidence
   1.0). A call becomes a `Calls` edge only when unambiguous: one definition in the same file
   (1.0), one among imported files (0.9), receiver-qualified `self`/`this`/type name (0.8), or
   exactly one definition repo-wide (0.7); otherwise dropped. `Extends`/`Implements` follow the
   same same-file → imports → repo-unique ladder; `Queries` and `Binds` find the SDL `Field` by
   its qualified name; `Defines` stays in the file. On top of that:
   - Solidity imports resolve as solc does: relative, then the Foundry remappings, then the
     project root, then the libs. Solidity names resolve in solc's scope (the contract, its
     ancestors most-derived first, then file-level declarations), which also places `Emits` and
     `Reverts`. An ABI pass writes `abi_signature` plus `selector` (functions, getters, errors)
     or `topic0` (events) when every type resolves, else `abi_unresolved`.
   - A GraphQL coordinate defined in several SDL files resolves to one canonical copy, so
     `Queries`, `Binds` and `Resolves` land on the same `Field`. `Resolves` joins a `Resolver`
     to its field: 0.9 when the case guards on the parent type or a registration `Binds` the
     coordinate, 0.7 when the field name alone decides (the one root field, or the only field,
     of that name).
   - `UsesAbi` joins a contract call's owner (else its file) to the `Abi` node it names: an
     imported ABI file 1.0, then same file → imports → repo-unique, then a normalized name 0.6.

   The module doc of `application/analyze.rs` holds the full resolution policy.
4. **Chunk** (`application/chunk.rs`, policy in `domain/chunking.rs`). See below.
5. **Persist.** `IndexStore::replace_repo` swaps the repo's rows; embeddings survive by content
   hash. Only chunks the store reports missing an embedding for are embedded (batches of 16).
6. **Hierarchy.** Louvain communities (L1) → summaries (L2), each embedded only when its text
   changed.
7. **Stamp** the repository with the HEAD read at the start (`commit_sha`), the paths uncommitted
   then (`worktree_paths`) and the listing rules (`listing_rules`: `skip_dirs`, Foundry `libs`,
   indexable-language version). A run marks the stamp incomplete (`worktree_paths` NULL) before
   its first index write and completes it only once everything succeeded.

`add` and `reindex` work from that stamp (`application/delta.rs`). Git names the candidates: the
two-tree diff stamped commit → HEAD (no ancestry needed, so branch switches and rebases work;
`since <ref>` adds ref → HEAD), the paths uncommitted now (untracked included) and the paths
uncommitted at the stamp. The full listing decides: each candidate goes to `Vcs::list_paths`, the
listing restricted to it (ignore files, `skip_dirs`, the Foundry `libs`, languages, symlinks never
followed; a submodule or a changed `.gitignore` is reconciled below its directory). Listed files
are re-read unless their content hash equals the stored one; indexed paths the listing no longer
has are purged. `add` runs steps 2–7 for the re-read files against the persisted graph and
resolves relations only within that set; edges from unchanged files into a re-added file are
kept by `merge_files` while their target symbol still exists. `reindex` re-reads the same delta
plus the unchanged files one hop upstream of each change (an `imports`, `calls`, `extends`,
`implements`, `queries`, `emits`, `reverts`, `binds`, `resolves` or `uses_abi` edge into it); it
returns `skipped: true` when HEAD is the stamp, the worktree is clean, nothing was uncommitted at
the stamp and the listing rules are unchanged; `force` runs the full index. When the stamp is
unusable (missing, incomplete, other listing rules, commit not in this clone, not a git
repository) `add` reconciles every file by content hash and also finishes embedding and the
hierarchy, and `reindex` runs the full index; the report's `delta.fallback` says why. Documented
limitations: an edge from a file outside the delta to a brand-new symbol waits for
`reindex --force`; so do the imports and selectors of unchanged Solidity files after a remapping
changes or a struct in another file changes; edits to `.git/info/exclude` or the global excludes
are not in the delta.

## Data model (`migrations/`, 8 tables)

| Table | Holds |
| --- | --- |
| `repositories` | `root_path` (unique); the stamp: `commit_sha`, `worktree_paths` (NULL = incomplete), `listing_rules` |
| `files` | path, language, `content_hash` |
| `nodes` | symbols, files, surfaces: `kind`, `stable_id` (unique per repo), name, signature, doc, lines, `metadata` |
| `edges` | `source → target`, `kind`, `confidence`; unique per (source, target, kind) |
| `chunks` | `header`, `body`, `content_hash`, line range, generated `search_vector` (tsvector over header + body + identifier-split text) |
| `embeddings` | one row per (chunk, provider, model, dimensions); `check (dimensions = vector_dims(embedding))` |
| `communities` | label, member count, summary text, `summary_hash`, `summary_embedding`, top members, files |
| `community_members` | community ↔ node |

Ids are deterministic (UUIDv5 of repo root path / `stable_id` / edge triple), so re-indexing
produces the same ids. Every child table cascades on repository delete. Projects are not in the
schema: they are a `chaos.toml` file (name + `[[repos]]` alias/path).

## Chunking

A chunk is a context header plus a body. The header (max 700 chars) lists path, language, line
range, container, module, kind, name, `uses`, `calls`, the first 300 chars of the doc comment
and, for a public Solidity function, an `// abi: <signature> <selector>` line. Kinds: `unit` (one
per code-bearing symbol), `part` (pieces of an oversize unit) and `file_head` (imports +
top-level statements no symbol owns, max 1500 chars; a longer head, in any language, is cut
between ranges into `part i/n` chunks, and a single range over the cap is split by lines; a
test file keeps only the first part, so test bodies do not crowd real code out of retrieval).
A head that fits keeps its historical id (`…:file:<path>:0`).

Thresholds (`domain/chunking.rs`): hard cap 6000 chars (header + body, sized for ~2048-token
encoders), target 4500, minimum segment 400, similarity threshold 0.82 (override with
`CHAOS_SPLIT_COS`). An oversize unit is planned along its AST child statements: gaps are folded
into neighbours, segments under 400 chars are merged forward, anything still over the cap is
line-split at ≤ 4500. When an embedder is present the planned segments are embedded in one batch
(header prepended to each) and adjacent segments are merged left-to-right while cosine ≥ 0.82
and the merge stays ≤ 4500 chars; a failed batch falls back to the line split (recorded in chunk
metadata). Chunk ids derive from `content_hash + stable_id + part`, so the result is deterministic
for a fixed encoder and content, which embedding reuse relies on.

## Retrieval and fusion (`feature`)

1. Embed the question once.
2. **Route** to communities by cosine against `summary_embedding`: keep every community scoring
   ≥ 0.35, never fewer than 2 nor more than 5 (`domain/fusion.rs::route_communities`).
3. **Anchor** on a GraphQL field (`application/graphql_entry.rs`) when the question names one
   (`Type.field` always; a bare root field name only next to a cue word such as query,
   mutation, subscription, resolver, graphql or field) or the `entry` argument gives one. From
   the graph it reads the field's SDL copies, the registrations that `binds` it, the `Resolver`s
   that `resolves` it, their `calls` (breadth-first, 3 hops) and the operations and hooks that
   call it (tests left out). The anchor is charged to the budget first, trimmed to 25 % of it;
   its files join the channel scope and its chunks lead the excerpts. Nothing anchors unless
   the index holds the field or an operation selecting it.
4. Run three channels restricted to those communities' files: semantic (pgvector cosine, 40),
   keyword (Postgres full-text `ts_rank_cd` over the identifier-split tsvector, 40), literal
   (case-insensitive substring on terms lifted from the question, 20).
5. **Fuse** (`domain/fusion.rs::fuse`). Each channel is normalized by its own maximum, then
   weighted: semantic 1.0, keyword 0.6, literal 0.4. A chunk's first sighting sets its score;
   each further channel adds 0.25 × its weighted score; a chunk seen by one channel only is
   damped by 0.75. Top 30 survive.
6. Expand one hop over call/import edges from the top 8 hits (at most 10 extra chunks, 40 edges),
   and collect env-var / HTTP-route surfaces of the involved files.
7. **Pack** under the token budget (`domain/budget.rs`, 4 chars ≈ 1 token): summaries ≤ 15 %,
   excerpts ~70 %, graph + surfaces ~10 %; an excerpt that does not fit is clipped at a line
   boundary (never below 600 chars) or degrades to its header line so symbol coverage stays wide.
8. Persist `docs/features_memory/<slug>.html` through the hash-gated `PageStore`.

Across repos (`application/project_feature.rs`) the members' anchors are joined into one
`entry`, and an entry link joins a calling operation in one repo to the resolver of the same
coordinate in another (graph evidence, not a name match).

## Hierarchy: L1 / L2 and what is reused

- **L0** is the flat graph (nodes + edges + chunks).
- **L1, communities** (`domain/community.rs`): deterministic Louvain modularity optimisation
  (nodes visited in `stable_id` order, ties broken toward the smallest representative, resolution
  1.0). A post-pass folds fragments smaller than 4 members into their folder-preferred
  best-coupled neighbour so the layer holds features, not per-file singletons. Community ids are
  derived from the smallest member `stable_id`, so they are stable across re-indexes. The
  synthetic repository node is excluded.
- **L2, summaries** (`domain/summary.rs`): a templated, embedder-free text per community (label,
  top symbols, doc sentences, files, neighbour communities; ≤ 1600 chars) that is then embedded
  for routing. Its key is `summary_hash = sha256(model_id \n text)`. `replace_communities` keeps
  every summary embedding under that key and gives it back to any community whose new text has
  the same key, whatever its id, so only a summary whose text changed is embedded again. A
  no-change `add` or `reindex` embeds zero summaries; so does an edit that does not reach the
  text (a comment inside a function, code moved down a few lines). Changing the embedding model
  or the template changes the text or the key and re-embeds them all.

Every reuse in the index is keyed the same way, by the hash of the thing's own input: a chunk
embedding by the chunk's `content_hash` (header + body), a file by the hash of its bytes (`add`
compares it with the stored one), a summary embedding by `summary_hash`, a generated page by the
hash of the data it shows. There is no roll-up of hashes into a tree: nothing in `feature`,
`add` or `reindex` asks "did anything below this change?", so a tree would have no reader.
`reindex` decides whether to run from git (HEAD equals the indexed commit and the worktree is
clean).

## Cross-repo correlations

`correlations` reads only the persisted index. Intra-repo: pairs of communities that share files
(a file whose nodes span both) or symbol names (same-name function/type nodes), scored by
overlap, capped at 50 per repo. Cross-repo, for every ordered pair of project members
(consumer → provider):

- `package_dep`: a consumer file's `metadata.packages` names a package the provider publishes
  per its `package.json`/`Cargo.toml`; confidence 0.9.
- `http_route`: a provider `HttpRoute` node such as `GET /users/:id` matches a call path
  recorded on a consumer file under `metadata.http_calls`, parameters normalized; confidence
  0.75.
- `graphql_field`: a consumer GraphQL operation outside tests names, in `metadata.root_fields`,
  a root field defined in the SDL of the schema owner (the repo defining the most root fields);
  confidence 0.8. When the owner has a `Resolver` that `resolves` the field, the link lands on
  the resolver's feature, names the resolver in the evidence, and takes the lower of 0.8 and
  the edge's confidence.
- `contract_abi` (`application/contract_abi.rs`, shared with the entry points): a contract call
  in `metadata.contract_calls` (through its `UsesAbi` edge), or a Solidity interface no
  contract of its own repo implements, → the Solidity function, getter or event it reaches in
  another repo. Tiers, tried in order: named 0.85 (the ABI's name names the contract; when the
  member is not in the indexed contract family but the contract has bases the index does not
  hold, such as OpenZeppelin `ERC721`, the link goes to the contract as inherited, 0.6, unless
  another indexed contract declares the member; then the call stays unlinked),
  overlap 0.7 (one contract family agrees on at least 2 and at least half of the ABI's
  functions, more than any other), unique 0.6 (no ABI or one of at most 3 items, and exactly
  one contract family with an agreeing member). A standard ERC-20/721 ABI links through the
  named tier only. Signatures agree by `abi_signature` when the provider has one, else by name
  and arity (`metadata.params`), else by name. Test files are not consumers.

Links below `min_confidence` are dropped. At most 100 cross links, 5 evidence entries each. The
report page `correlations.html` is hash-gated on the sorted link keys. Package names are read
from manifests by the entry layer; use-cases never touch the filesystem.

## Pages

`adapters/fs_pages.rs` renders every page from a JSON manifest with one light editorial theme:
a self-contained HTML file embedding the manifest (`<script type="application/json"
id="chaos-manifest">`) and a small renderer. Writing the same `content_hash` over an existing
page is a no-op; `compose` reports written vs cached counts so an agent never re-reads cached
pages.

## Entry points

`application/entry_points.rs` derives, from the index alone, where someone comes in from outside:
GraphQL root fields (in the schema owner, with every calling operation, the `Resolver`s that
`resolves` the field and the registrations that `binds` it), public/external contract functions
and public getters (with `selector`, NatSpec notice, emitted events, reverted errors, and the
callers `application/contract_abi.rs` finds in any repo given), MCP tools, HTTP routes, React
hooks (with the GraphQL fields their calls reach within three hops and the contract calls they
make or reach), the document plan of a repo that is mostly Markdown, and transcript timelines.
Test, fixture and mock files are left out. Every repo's site index lists its own; the project hub
(`compose --all-repos`, `application/project_compose.rs`) lists the whole project's next to a
feature graph across the repos. One renderer (`ENTRY_JS`) draws both.

## Provenance

Generated answers carry `Breadcrumb { source, method, detail, locator }` entries
(`domain/provenance.rs`) saying where each piece came from: git, postgres, file, tree-sitter, embedding,
graph, page. They are part of every tool return and every page manifest.
