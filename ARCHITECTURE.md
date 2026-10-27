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
   `indexing.skip_dirs`; files over 2 MiB are skipped.
2. **Extract** (`adapters/treesitter`). Per file, in parallel, tree-sitter for Rust, TS/JS,
   Python, Solidity and GraphQL (plus `gql` documents embedded in TS/JS/Python); a heading-based
   extractor for Markdown. Output is file-local: symbols with byte/line ranges, doc comments,
   unresolved import and call candidates, and user surfaces (env vars read, HTTP routes
   registered) found by node-shape walks, never regex.
3. **Analyze** (`application/analyze.rs`). Builds the L0 graph and resolves relations
   project-wide, never fuzzily. Imports become `Imports` edges between file nodes (confidence
   1.0). A call becomes a `Calls` edge only when unambiguous: one definition in the same file
   (1.0), one among imported files (0.9), receiver-qualified `self`/`this`/type name (0.8), or
   exactly one definition repo-wide (0.7); otherwise dropped. `Extends`/`Implements`/`Queries`/
   `Defines` follow the same same-file → imports → repo-unique ladder.
4. **Chunk** (`application/chunk.rs`, policy in `domain/chunking.rs`). See below.
5. **Persist.** `IndexStore::replace_repo` swaps the repo's rows; embeddings survive by content
   hash. Only chunks the store reports missing an embedding for are embedded (batches of 16).
6. **Hierarchy.** Louvain communities (L1) → summaries (L2), each embedded only when its text
   changed.
7. **Stamp** the repository with the git `commit_sha`.

`reindex` runs the same pipeline unless HEAD equals the indexed commit and the worktree is clean
(then it returns `skipped: true`; `force` overrides). `add` runs steps 2–7 for the files git
whose on-disk content hash differs from the stored one (plus what git reports with `since <ref>`), purges files gone from disk
and renamed-from paths, and resolves relations only within the changed set. Edges from
unchanged files into a re-added file are kept by `merge_files` while their target symbol still
exists. Documented limitation: an edge from an unchanged file to a brand-new symbol waits for
the next full `index`.

## Data model (`migrations/`, 8 tables)

| Table | Holds |
| --- | --- |
| `repositories` | `root_path` (unique), `commit_sha` |
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
range, container, module, kind, name, `uses`, `calls` and the first 300 chars of the doc comment.
Kinds: `unit` (one per code-bearing symbol), `part` (pieces of an oversize unit) and `file_head`
(imports + top-level statements no symbol owns, max 1500 chars).

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
3. Run three channels restricted to those communities' files: semantic (pgvector cosine, 40),
   keyword (Postgres full-text `ts_rank_cd` over the identifier-split tsvector, 40), literal
   (case-insensitive substring on terms lifted from the question, 20).
4. **Fuse** (`domain/fusion.rs::fuse`). Each channel is normalized by its own maximum, then
   weighted: semantic 1.0, keyword 0.6, literal 0.4. A chunk's first sighting sets its score;
   each further channel adds 0.25 × its weighted score; a chunk seen by one channel only is
   damped by 0.75. Top 30 survive.
5. Expand one hop over call/import edges from the top 8 hits (at most 10 extra chunks, 40 edges),
   and collect env-var / HTTP-route surfaces of the involved files.
6. **Pack** under the token budget (`domain/budget.rs`, 4 chars ≈ 1 token): summaries ≤ 15 %,
   excerpts ~70 %, graph + surfaces ~10 %; an excerpt that does not fit is clipped at a line
   boundary (never below 600 chars) or degrades to its header line so symbol coverage stays wide.
7. Persist `docs/features_memory/<slug>.html` through the hash-gated `PageStore`.

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
(consumer → provider): `package_dep` (a consumer file's `metadata.packages` names a package the
provider publishes per its `package.json`/`Cargo.toml`; confidence 0.9) and `http_route` (a
provider `HttpRoute` node such as `GET /users/:id` matches a call path recorded on a consumer
file under `metadata.http_calls`, parameters normalized; confidence 0.75). Links below
`min_confidence` are dropped; at most 100 cross links, 5 evidence files each. The report page
`correlations.html` is hash-gated on the sorted link keys. Package names are read from manifests
by the entry layer; use-cases never touch the filesystem.

## Pages

`adapters/fs_pages.rs` renders every page from a JSON manifest with one light editorial theme:
a self-contained HTML file embedding the manifest (`<script type="application/json"
id="chaos-manifest">`) and a small renderer. Writing the same `content_hash` over an existing
page is a no-op; `compose` reports written vs cached counts so an agent never re-reads cached
pages.

## Provenance

Generated answers carry `Breadcrumb { source, method, detail, locator }` entries
(`domain/provenance.rs`) saying where each piece came from: git, postgres, file, tree-sitter, embedding,
graph, page. They are part of every tool return and every page manifest.
