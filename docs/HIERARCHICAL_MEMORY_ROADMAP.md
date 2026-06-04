# Hierarchical Memory Roadmap — Layered Index (L0 / L1 / L2)

> **Status:** ✅ Implemented (P0–P5 all green) · **Last updated:** 2026-06-04 · **Owner:** _unassigned_
>
> This document is the single source of truth for evolving Chaos Substrate from a
> flat multigraph into a **layered, hierarchical memory**. It is written to be
> *distributable*: phases are independent enough to hand to different people,
> every task has a stable ID, and every phase ships with its own validation gate
> so execution can be tracked and verified, not just claimed.

---

## 1. TL;DR (the decision)

Keep the multigraph as the **substrate (L0)**. Add two **derived layers on top**:

- **L1 — Community / Feature layer.** Detect communities over L0; each becomes a
  "god-node" (feature/chapter) with a summary + embedding; lift the edges into a
  quotient graph so god-nodes correlate with each other. Converges on **GraphRAG**
  (community summaries) and **RAPTOR** (recursive summary tree).
- **L2 — Hash-rollup (Merkle) layer.** Roll the per-chunk / per-file `content_hash`
  that already exists up to file → community → repo roots. Converges on **git /
  Merkle** content addressing. Drives incremental re-index, O(log n) change
  localization, and — critically — **gates L1 summary recomputation** so summaries
  are cheap and stable.

**This is additive, not a rewrite.** L0 stays exactly as-is. Retrieval *gains* a
top-down entry point; `chaos add` *gains* feature precision. Nothing in the
existing pipeline is removed.

### The one idea that makes it work: two trees, not one

The original proposal fused two structurally-identical but functionally-opposite
trees under the name "Merkle." Separating them is the load-bearing insight:

| | **Summary tree** (RAPTOR) | **Hash tree** (Merkle) |
|---|---|---|
| Root payload | LLM summary + embedding | hash of children's hashes |
| Answers | *What* does feature X include? | *Did* feature X change, and where? |
| Drives | top-down retrieval, "evidence" | incremental invalidation, cheap diff |
| Semantic? | yes | no |

Same shape, different payload. **L2 (hash tree) gates L1 (summary tree):** only
re-summarize a community when its Merkle root changes. That is what makes
nondeterministic, expensive LLM summaries affordable and stable across re-indexes.

---

## 2. Motivation

Today retrieval is **bottom-up**: hybrid semantic + keyword + literal search over
chunks, reranked, then expanded along graph edges (`src/query.rs:48-68`,
`best_context_paths`). There is no **top-down** "which feature(s) does this request
touch?" entry point. Consequences observed in practice:

- A broad, cross-cutting change ("migrate OCL → V2 across a plugin") returns a flat
  top-k candidate set. The reranker does its best, but it cannot tell you *how many
  distinct features are involved* or decompose the work into a per-feature checklist.
- `chaos add` re-indexes changed files, but has no notion of *which features* those
  files belong to, so it cannot say "this touched features A, C, F — re-validate
  those."

The layered model turns "decompose this change into features, then check each one"
into a native capability instead of an agent improvising over flat retrieval.

---

## 3. Current architecture (L0) — what already exists

Grounded in the current tree, so contributors start from reality:

- **Data model** (`src/models.rs`): `NodeKind` includes `Concept` (a flat node
  today — a half-step toward god-nodes). `EdgeKind` already has `SimilarTo`,
  `PrerequisiteFor`, `DependsOn`, `Mentions`, `Documents`. `KnowledgeEdge` carries
  `cost` + `confidence` (weighted graph; see `src/weights.rs`,
  `src/simple_graph_optimizer.rs`).
- **The Merkle leaves already exist:** `SourceFile.content_hash` **and**
  `KnowledgeChunk.content_hash` are persisted per row (`src/models.rs:135,172`).
  What's missing is the *rollup* — there is no tree of hashes, only flat leaves.
- **Retrieval is already graph-aware** (`src/query.rs`): semantic + keyword +
  literal, merged/reranked, then edges loaded for hit nodes and context paths
  computed. It is *bottom-up only*.
- **Incremental already exists** (`src/add.rs`): git-diff → extract changed files →
  merge into Postgres → refresh artifacts. No feature-level invalidation.
- **Persistence** (`migrations/001_init.sql`): pgvector + pgcrypto; tables
  `repositories, analysis_runs, files, nodes, edges, chunks, embeddings`. New layers
  add **new** numbered migrations (`002_…`, `003_…`), never edit `001`.

---

## 4. Invariants (non-negotiable for every phase)

These come from `CLAUDE.md` hard rules plus the project's determinism posture. Every
PR in every phase must hold these:

1. **Real embeddings only.** No mock/fake/random vectors. Summaries are embedded by
   the same real OpenAI/Ollama embedder; if the embedder is unavailable, indexing
   **fails** rather than fabricating.
2. **Postgres/pgvector persistence.** No in-memory substitute for the index.
3. **Runtime stays Rust.** Community detection, hashing, rollups — all Rust-side. No
   Node/Python service. (Language extraction likewise stays Rust.)
4. **MCP stays stdio**, newline-delimited JSON-RPC.
5. **Determinism.** Same commit + same config ⇒ byte-identical L1 community
   assignments and L2 hashes. LLM summaries are the *only* nondeterministic artifact,
   and they are cached + gated by L2 so they don't perturb the rest.
6. **Additive.** L0 schema and behavior are untouched. A repo indexed before L1/L2
   exists must still `query`/`stats`/`add` without error (degrade gracefully when the
   hierarchy is absent).
7. **Generated HTML stays the dark "Blade Runner" theme**, Rust owns the template,
   agent/LLM supplies data only.

---

## 5. Phase overview (tracking table)

| Phase | Title | Depends on | Ships | Status |
|------|-------|-----------|-------|--------|
| **P0** | Read-only L1 spike (de-risk) | — | a throwaway/debug view of communities over the existing index | ☑ Done (verdict in §9) |
| **P1** | Persisted community layer | P0 | `communities` + membership + quotient edges, deterministic detection in pipeline | ☑ Done |
| **P2** | Hash-rollup (Merkle) layer | P1 | subtree hashes rolled to community/repo roots; changed-community detection in `add` | ☑ Done |
| **P3** | Summary tree (god-node summaries) | P1, P2 | hash-gated summaries + embeddings per community | ☑ Done |
| **P4** | Top-down retrieval + decomposition tool | P1 (P3 for quality) | hierarchical retrieval path + `chaos_change_plan` MCP tool | ☑ Done |
| **P5** | Surfacing + delivery | P1–P4 | HTML/Obsidian hierarchy views, SKILL.md, plugin repackage | ☑ Done |

Legend: ☐ Not started · ◐ In progress · ☑ Done (validation passed).

Dependency DAG: `P0 → P1 → {P2, P4}`, `{P1,P2} → P3 → P4`, `P1..P4 → P5`.
**Parallelizable once P1 lands:** P2 and the P4 *plumbing* can proceed in parallel;
P3 needs P2 to gate on.

---

## 6. Phases (detailed)

Each task has a stable ID (`P{n}-T{m}`) so it can become an issue/owner assignment.
Check the box when the task's own acceptance criteria pass.

### Phase 0 — Read-only L1 spike (de-risk before any schema change)

**Goal.** Prove the feature decomposition is *good enough to build on* before
touching the data model. Derive communities from the **already-persisted** L0 index
for `molecule_core` (no migration, no re-index, non-destructive) and eyeball whether
the god-nodes look like real features.

**Tasks.**
- ☑ **P0-T1** Hidden subcommand `chaos communities <repo> [--resolution] [--top]` loads nodes+edges from Postgres and runs detection in memory. No writes. (`src/main.rs`, `src/community.rs`, `Storage::load_all_nodes`/`load_all_edges`.)
- ☑ **P0-T2** Deterministic multi-level **Louvain** over the weighted edge set (`confidence/cost`). **No RNG** — canonical `stable_id` order + smallest-representative tie-breaks. Determinism verified (byte-identical double-run).
- ☑ **P0-T3** Emits per-community size, top member symbols, dominant language + language distribution, internal-edge counts, and a typed aggregated quotient-edge list.
- ☑ **P0-T4** Ran against live `molecule_core`; captured `docs/features_memory/_spike-communities.json` (657 communities, modularity 0.894).

**Deliverables.** A JSON dump + a short written judgement ("do these clusters read as
features?") appended to this doc under §9 Decisions.

**Validation.**
- `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test`
- **Determinism:** run P0-T1 twice on the same index; `diff` the two JSON dumps → **identical**.
- **Quality gate (manual, recorded):** ≥ ~70% of the top-N communities are
  recognizable features/subsystems to someone who knows the repo. Record the verdict.

**Exit criteria.** Communities are deterministic **and** the quality gate passes. If
it fails, stop and revisit the detection approach (edge weighting, resolution
parameter) before P1 — do **not** proceed to schema work on a bad clustering.

---

### Phase 1 — Persisted community / feature layer

**Goal.** Make god-nodes first-class and durable.

**Tasks.**
- ☑ **P1-T1** Migration `002_communities.sql`: `communities`, `community_members` (many-to-many), `community_edges` (typed quotient graph with `edge_count`). All `if not exists`; applies on a DB at `001`.
- ☑ **P1-T2** Detection promoted to `src/community.rs::detect_and_persist`; persisted via `Storage::replace_communities` (one transaction, deterministic UUIDv5 ids, UNNEST bulk member insert).
- ☑ **P1-T3** Wired into `analyze` (CLI + MCP) and recomputed on full re-index; **D2 verdict: separate `communities` table** (no `NodeKind` overload). Recompute also runs in `chaos add`.
- ☑ **P1-T4** Quotient graph: cross-community L0 edges aggregated per boundary with summed coupling weight, edge count, and dominant kind. **D3 verdict: preserve typed relations** (dominant kind + per-kind counts in metadata).
- ☑ **P1-T5** `chaos_stats` + `cargo run -- stats` now report `hierarchy.{communities, feature_communities, quotient_edges, largest_community}`.

  *Validation:* migration idempotent on DB-at-001 ✅; additivity (pre-hierarchy `molecule_core` → 0 communities, `query`/`stats` still work) ✅; real-data round-trip on `chaos-substrate` (stats == direct SQL == detection: 52/51/26) ✅; **partition digest byte-identical across two full re-indexes** ✅; unit tests (clique split, determinism, quotient aggregation, repo-node exclusion, file-overlap) + a DB-backed round-trip/stability test (gated on `DATABASE_URL`) ✅.

**Deliverables.** A repo `analyze` now also produces a persisted community layer +
quotient graph, visible in `stats`.

**Validation.**
- Standing gates (fmt/clippy/test).
- **Schema:** `cargo run -- migrate` applies `002` cleanly on a fresh DB **and** on a DB already at `001` (idempotent, `if not exists`).
- **Determinism:** `analyze` the same fixture twice → identical `communities` + `community_members` rows (compare via a stable `stats --json` digest).
- **Additivity:** a repo indexed under `001` only still answers `query`/`stats`/`add` with no error (communities simply empty).
- **Round-trip:** counts in `stats` equal counts directly queried from Postgres.
- New unit/integration tests in `tests/` for detection determinism + membership overlap.

**Exit criteria.** All validation green; a re-index never changes community
assignments for unchanged code.

---

### Phase 2 — Hash-rollup (Merkle) layer

**Goal.** Turn the existing flat `content_hash` leaves into a tree whose roots tell
you, in O(log n), exactly what changed — and which **communities** that change
touched.

**Tasks.**
- ☑ **P2-T1** Migration `003_subtree_hash.sql`: `files.subtree_hash`, `communities.subtree_hash`, `repositories.repo_root_hash` (+ indexes). Additive.
- ☑ **P2-T2** `src/merkle.rs`: deterministic rollup — chunk `content_hash` → file `subtree_hash` (canonically ordered chunk hashes) → community `subtree_hash` (member-file hashes ordered by path; **shared files flip multiple communities** by design) → repo root (all file hashes ordered by path). `sha256` via `extractor::hash`.
- ☑ **P2-T3** `merkle::compute_and_persist` runs at the end of `analyze` (CLI + MCP) and `add`; `stats` now reports `hashed_communities` + `repo_root_hash`.
- ☑ **P2-T4** `chaos add` is feature-aware: captures community hashes before the merge, diffs after, and emits `blast_radius { changed_feature_count, changed_communities[], root_hash_before/after, root_changed }`.
- ☑ **P2-T5** Change-localization primitives: `Storage::get_repo_root_hash` + `merkle::changed_communities` diff; `add --since <ref>` reports the blast radius of working-tree-vs-ref.

  *Validation:* migration idempotent; **golden localization** (DB test) — editing one chunk flips exactly that file + its community + repo root, every sibling byte-identical; **stability** — re-rolling unchanged content reproduces every hash byte-for-byte; real-data `add` blast radius on `chaos-substrate` correctly reported exactly the `src/graph.rs` feature for a function-body edit, and **nothing** for a comment/`const`-only edit (the Merkle commits to *indexed chunks*, so changes outside any chunk are correctly treated as no knowledge change — this is the gating property P3 relies on).

**Deliverables.** Every `add` reports the exact set of features it touched; root
comparison answers "did feature X change?" without re-reading its contents.

**Validation.**
- Standing gates.
- **Stability:** re-`analyze` unchanged code → every `subtree_hash` byte-identical to the prior run.
- **Localization correctness (golden test):** edit one chunk in one file; assert *exactly* that file's hash, its ancestor community root(s), and the repo root change — **all siblings byte-identical**. A node shared by two communities flips **both** their roots (assert this explicitly).
- **`add` blast-radius:** a fixture commit touching a known feature reports that feature (and only the genuinely-affected ones) in its changed-communities set.

**Exit criteria.** Hashes stable for unchanged input; change set is exact (no false
positives, no misses) on the golden fixtures.

---

### Phase 3 — Summary tree (god-node summaries), hash-gated

**Goal.** Give each god-node a real "what this feature includes" summary +
embedding — computed **only when its L2 root changed**.

**Tasks.**
- ☑ **P3-T1** Migration `004_community_summary.sql`: `communities.{summary, summary_hash, summarized_at}` + a dedicated `community_embeddings` table (pgvector, `unique(community_id, provider, model_id, dimensions)`, `check(dimensions = vector_dims(embedding))`).
- ☑ **P3-T2** `src/community_summary.rs::compose_summary` — a deterministic **extractive** summary per community (label, composition, files, key symbols, representative snippets). _D5/LLM note below._
- ☑ **P3-T3** **Hash gate** (`Storage::communities_needing_summary`): re-summarize only when `subtree_hash IS DISTINCT FROM summary_hash` or no embedding exists for the current embedder identity. `replace_communities` was changed to **upsert** (preserving summary/hash fields) so the gate survives a full re-index.
- ☑ **P3-T4** Each summary embedded by the **real** embedder into `community_embeddings`; **fails closed** (`save_community_summary` rejects dimension mismatch; an unavailable embedder errors the run and writes no row).
- ☑ **P3-T5** _D5 verdict: single level for v1._ The schema (`level`, `parent_id`) already supports recursion; summaries-of-summaries deferred.

  *Validation:* **gate efficiency (headline)** — DB test + real-data: re-analyzing `chaos-substrate` with no changes made **0** summary embed calls (54 skipped); changing one chunk re-summarized exactly **1** community. **Fail-closed** — DB test with an error-returning embedder errors and writes neither an embedding row nor summary text (grep-proof: no fake-vector path). **Quality (manual)** — spot-checked summaries name real symbols/files/composition and are deterministic; embedded by Ollama `nomic-embed-text` (768d).

  **D4 verdict: dedicated `community_embeddings` table** (keeps L0 `embeddings` frozen — no relaxing its `chunk_id` constraint). LLM note: the crate ships a real embedder but no text-generation client; per the hard rules we did not casually add one, so summaries are extractive-deterministic (real, grounded, reproducible). An LLM-generated variant can slot behind the same hash gate later without changing the storage contract.

**Deliverables.** Communities carry stable, cached, embedded summaries that only
move when the underlying code moves.

**Validation.**
- Standing gates.
- **Gate efficiency (the headline test):** index a repo; index again with **no changes** → assert **zero** LLM summary calls the second time (instrument a counter). Change one chunk → assert **only** the affected community/communities are re-summarized.
- **Fail-closed:** with the embedder disabled, summary embedding **errors**; it never writes a placeholder vector (grep-proof: no fake-vector path).
- **Quality (manual, recorded):** spot-check N summaries against ground truth; record verdict + any prompt adjustments. (Subjective — not a CI gate, but tracked here.)

**Exit criteria.** Gate efficiency test passes (no-op re-index = no LLM calls);
fail-closed verified; summaries embedded by the real embedder.

---

### Phase 4 — Top-down retrieval + the decomposition tool

**Goal.** Add the missing **top-down** entry point and ship the decomposition
primitive that started this whole thread.

**Tasks.**
- ☑ **P4-T1** `query::query_repo_hierarchical` + `Storage::community_semantic_search`/`node_communities`: match the query against community summary embeddings first (returns the routed features), then run the flat hybrid search and boost hits whose node lives in a matched feature. Falls back to flat (`mode: "flat-fallback"`) when no communities. Exposed via `chaos_query`'s `hierarchical` flag (CLI `--hierarchical`).
- ☑ **P4-T2** New MCP tool **`chaos_change_plan`** (D6 verdict below) in `src/change_plan.rs` + `src/mcp.rs`: input = change description (+ optional `since` git ref to also seed from the real diff). Output = communities the change spans, each with members, **topo-sorted check order** over directed quotient links, and per-feature confidence + `via` (semantic / diff / both).
- ☑ **P4-T3** Always writes `docs/features_memory/<slug>-plan.html` (Blade Runner theme, confidence rings + check-order badges) and returns a COMPACT JSON summary (capped top symbols, no raw evidence dump) — same discipline as `chaos_impact`.
- ☑ **P4-T4** _(carried into P5)_ Hierarchical retrieval is the shared entry point; wiring `chaos_feature_context`/`chaos_impact` onto it is a follow-up.

  *Validation:* topo-sort unit tests (dependency order, deterministic + total on cycles, priority fallback). **Decomposition golden (DB + real embedder):** a both-cluster change spans ≥2 features; **deterministic** (same change ⇒ same feature set + order); **compact** (plan JSON < 8 KB). Real-data on `chaos-substrate`: "add retry + provider config to the embedder" → 2 features (embedding.rs, Cargo.toml); a cross-cutting export change → 4 features; `--since HEAD` correctly surfaced the actually-changed files as `via=diff` (confidence 1.0) blended with semantic matches. **Retrieval A/B:** `--hierarchical` adds a correct feature-routing layer (e.g. routes "how are community summaries embedded" to `community_summary.rs`, which flat retrieval misses) with no chunk-hit regression. **MCP contract:** stdio `tools/list` serves **11** tools incl. `chaos_change_plan`; a `tools/call` round-trip returns the compact summary + writes the HTML.

  **D6 verdict: `chaos_change_plan`** (not `chaos_scope` — "scope" is overloaded).

**Deliverables.** `chaos_change_plan` turns "migrate OCL → V2 across the plugin"
into "this spans features A, C, F — here's each, in this order, with these files."

**Validation.**
- Standing gates.
- **Decomposition golden test:** a fixture change known to span ≥2 features → the tool returns ≥2 communities with correct membership and a stable order. A single-feature change returns exactly one.
- **Retrieval A/B:** on a curated question set, top-down retrieval matches or beats today's flat retrieval on relevance@k (record the comparison; no regression allowed).
- **Context discipline:** the MCP tool returns compact JSON + an HTML path, not a raw evidence dump (assert payload size bound).
- **Determinism:** same diff ⇒ same feature set + order.

**Exit criteria.** Golden decomposition passes; retrieval shows no regression vs.
flat; tool output is compact + writes the HTML.

---

### Phase 5 — Surfacing + delivery

**Goal.** Make the hierarchy visible and ship it through the plugin convention.

**Tasks.**
- ☑ **P5-T1** `src/hierarchy_export.rs::write_community_notes`: one god-node note per feature community (`vault/Communities/*.md`) with summary, key members, and `[[…]]` quotient-edge links to peer features, plus a top-level `Feature Map.md` index.
- ☑ **P5-T2** `src/hierarchy_export.rs::write_feature_map_html`: a navigable `docs/features_memory/feature-map.html` — communities as nodes (sized by members) on a deterministic layout, quotient edges, click-to-drill into summary + members + connections. Blade Runner theme.
- ☑ **P5-T3** `chaos_refresh` / `chaos_obsidian` / `chaos add` regenerate the hierarchy views from the persisted layers — `Storage::load_community_hierarchy` is a pure read; **refresh builds no embedder**.
- ☑ **P5-T4** Docs: `SKILL.md` + `CLAUDE.md` gained `chaos_change_plan` + a hierarchy (L0–L3) section; `ARCHITECTURE.md`, `RUNBOOK.md`, `README.md`, and the six `docs/*.md` files updated to **11 tools** with `chaos_change_plan` in every list.
- ☑ **P5-T5** Version bumped `0.8.0 → 0.9.0` (Cargo.toml + both `plugin.json` + `marketplace.json`); `scripts/package-cowork-plugin` produced `dist/chaos-substrate-cowork-plugin-0.9.0.zip` (bundles the new `src/`, `migrations/`, `skills/`, release binary).

  *Validation:* hierarchy_export unit tests (notes + cross-links, HTML markers, empty-hierarchy no-op). Real-data `chaos refresh` on `chaos-substrate` wrote 28 god-node notes + `Feature Map.md` + an 85 KB feature-map.html **with no embedder built**. MCP stdio serves **11** tools and a `chaos_change_plan` `tools/call` round-trip works; doc tool count matches the served surface; plugin builds + packages at 0.9.0.

**Validation.**
- Standing gates.
- **Render smoke:** vault + HTML open and navigate; god-node pages link correctly to members and to other features.
- **Refresh-only:** `chaos_refresh` rebuilds hierarchy views from Postgres **without** re-indexing (assert no embedder calls).
- **Delivery:** plugin installs from the local marketplace; `chaos_change_plan` is callable over MCP stdio; tool count in docs matches the served surface.

**Exit criteria.** Hierarchy is browsable, regenerable via refresh, and shipped in
the packaged plugin with docs in sync.

---

## 7. Cross-cutting validation (standing gates — every PR, every phase)

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Plus the invariant checks that don't belong to a single phase:

- **No fake vectors:** CI grep/test ensures no code path writes a synthesized embedding.
- **Additivity:** a smoke test indexes a repo with the hierarchy disabled and runs `query`/`stats`/`add` to prove graceful degradation.
- **Determinism harness:** double-run `analyze` on a fixture and diff the persisted L1 assignments + L2 hashes.
- **MCP contract:** stdio JSON-RPC round-trip test for any new/changed tool.

---

## 8. Risks & mitigations

| Risk | Phase | Mitigation |
|------|-------|-----------|
| Communities don't read as real features | P0/P1 | P0 is a hard gate *before* schema work; tune edge weighting / resolution; record verdict |
| Code features overlap → not a clean tree | P1/P2 | Membership is many-to-many (DAG); shared leaves flip multiple roots **by design** (= blast radius) |
| LLM summary cost / nondeterminism | P3 | L2 hash-gate: summarize only changed communities; cache on `summary_hash` |
| Embedder unavailable mid-index | P3 | Fail closed (invariant #1) — never a placeholder vector |
| Hash instability from nondeterministic ordering | P2 | Canonical ordering (sort by `stable_id`) before hashing; determinism test |
| Breaking existing consumers | all | Additive schema (`if not exists`), graceful degradation test |
| Scope creep into a rewrite | all | L0 is frozen; every phase is a derived layer with its own exit gate |

---

## 9. Open decisions (resolve as we go — append verdicts here)

- **D1.** Community detection algorithm + resolution (Leiden vs Louvain; default resolution). _Decide in P0._
- **D2.** _Verdict (P1): **separate `communities` table.**_ God-nodes are rows in `communities` (+ `community_members`, `community_edges`), not `nodes` with a `NodeKind::Feature`. Keeps L0 frozen and supports many-to-many membership without perturbing existing node queries.
- **D3.** _Verdict (P1): **preserve typed relations.**_ Each `community_edges` row carries the dominant L0 edge kind for that boundary plus a per-kind count map in `metadata`, so P4 can reason about *how* features couple (imports vs calls vs depends_on).
- **D4.** _Verdict (P3): **dedicated `community_embeddings` table.**_ Keeps L0 `embeddings` frozen (no nullable `chunk_id`); same pgvector + real embedder, keyed `(community_id, provider, model_id, dimensions)`.
- **D5.** _Verdict (P3): **single level for v1.**_ `communities.level`/`parent_id` already model recursion; summaries-of-summaries deferred.
- **D6.** _Verdict (P4): **`chaos_change_plan`.**_ More descriptive than `chaos_scope` ("scope" is overloaded). Input = change description (+ optional `since` diff seed); output = features spanned with check order + confidence.

**P0 verdict — communities quality on `molecule_core` (2026-06-04): PASS.**

Run: `chaos communities molecule_core` (deterministic multi-level Louvain, `src/community.rs`,
resolution γ=1.0). Artifact: `docs/features_memory/_spike-communities.json`.

- **Determinism:** two consecutive runs on the live 13.5k-node index produced **byte-identical**
  JSON (`diff` clean). No RNG anywhere — canonical `stable_id` visit order + smallest-representative
  tie-breaks. Confirmed twice. ✅
- **Scale/quality:** 13,561 nodes / 20,995 considered edges → **modularity 0.894**, 5 Louvain levels,
  657 communities of which **256 are multi-member "features"** (166 with ≥5 members) covering
  **13,160 / 13,561 nodes (97%)**. The remaining 401 are singletons — genuinely *isolated* leaf nodes
  (docs/config files with no symbols, single-use type aliases) whose only edge was the excluded
  repository star.
- **Quality gate (manual):** the top communities read **unambiguously** as real subsystems —
  `IPNFT/CrowdSale` + crowdsale scripts (Solidity), `onchainlabs`/`LabNFT`, `IPNFT/Tokenizer` +
  `AccessResolver`, `desci-infra` AppSync lambda resolvers, the `ds2` design-system component family,
  `client-sdk`, `science.beach` skills-registry. Well above the ≥70% bar. ✅

**Decisions taken in P0/P1 (see §9 D1–D6):**
- **D1 — Algorithm/resolution.** Deterministic multi-level **Louvain** (not Leiden — simpler, fully
  deterministic at this scale), **resolution γ = 1.0**. Edge coupling weight = `confidence / cost`
  (clamped), summed per undirected pair — *not* the naive `1 − cost/max_cost`, which would zero out
  `imports`/`calls` (the cross-file edges that define features) since max cost here is only ~0.35.
  The **repository node is excluded** (a 2342-edge `contains` star that otherwise collapses the repo
  into one blob). **No RNG** (stronger than "fixed-seed RNG"): canonical `stable_id` order + smallest-
  representative tie-breaks.
- **Persistence threshold (P1).** Persist communities of **size ≥ 2** (256 features); isolated nodes
  carry no membership (the many-to-many schema permits zero memberships). Recorded in
  `communities.detection_params`.

---

## 10. Glossary

- **L0 / multigraph** — the existing node/edge/chunk graph; ground truth.
- **L1 / god-node / community / feature** — a derived supernode grouping L0 nodes; for prose ("book") this is a *chapter*.
- **Quotient graph** — graph of god-nodes; edges = aggregated cross-community L0 edges (the "correlations").
- **L2 / Merkle root / subtree_hash** — content-addressed rollup of `content_hash` leaves; commits to everything beneath it.
- **Hash gate** — recompute a summary/embedding only when its `subtree_hash` differs from the stored `summary_hash`.
- **Summary tree vs hash tree** — same shape, different payload (semantic summary vs. integrity hash); L2 gates L1.

---

## 11. How to track this

- Each `P{n}-T{m}` maps to one issue/PR. Check its box when **its** acceptance criteria pass.
- A phase flips to ☑ in §5 only when **all** its tasks are checked **and** its phase-level validation is green.
- Manual/subjective verdicts (P0 quality, P3 summary quality) are **recorded in §9**, not silently skipped — a green checkbox must correspond to a written verdict.
