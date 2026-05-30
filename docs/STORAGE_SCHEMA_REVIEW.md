# Storage Schema Review Checklist

Validation checklist for a Rust code knowledge memory backed by Postgres and pgvector.

The generic requirement names below are mapped to the **real Chaos Substrate tables**
(`migrations/001_init.sql`): `repositories`, `analysis_runs`, `files`, `nodes`, `edges`,
`chunks`, and `embeddings`, plus the `_sqlx_migrations` ledger maintained by `sqlx::migrate!`.
Use the mapped names when validating so this checklist does not report false schema drift.

## Required Tables

- [ ] **Repositories** → `repositories`: per-repo identity with stable UUID, name, unique
  `root_path`, optional remote URL, current commit SHA, and timestamps.
- [ ] **Code objects (symbols/chunks)** → `nodes` (+ `chunks`): `nodes` holds canonical
  symbols with stable UUID, `repo_id`, optional `file_id`, `kind`, `stable_id`, name,
  `line_start`/`line_end`, and `metadata` jsonb (note: line ranges only, no byte ranges).
  `chunks` holds the embeddable text units with `content`, `content_hash`, line range,
  `chunk_type`, and a `search_vector` tsvector for lexical search.
- [ ] **Embeddings** → `embeddings`: one row per embedded chunk with `chunk_id` foreign key,
  `provider`, `model_id`, `dimensions`, content hash, a `vector` `embedding` column, timestamps,
  a `(chunk_id, provider, model_id, dimensions, content_hash)` uniqueness constraint, and a
  `check (dimensions = vector_dims(embedding))` guard.
- [ ] **Relationships** → `edges`: directed code-graph edges with `source_node_id`,
  `target_node_id`, `kind`, plus `cost` and `confidence` columns.
- [ ] **Ingestion runs** → `analysis_runs`: durable ingest metadata with run UUID, `repo_id`,
  `commit_sha` revision, `status`, `error` details, and `started_at`/`finished_at` timestamps.
- [ ] **Source files** → `files`: file-level state with `repo_id`, `path`, `commit_sha`
  revision, `language`, `content_hash`, `line_count`, `indexed_at`, and a
  `(repo_id, path, commit_sha)` uniqueness constraint.
- [ ] **Schema migrations** → `_sqlx_migrations`: migration version, checksum, applied
  timestamp, and success/duration metadata, managed by `sqlx::migrate!`.

## Required Indexes

- [ ] Primary keys on every table use stable UUIDs; foreign keys (`repo_id`, `file_id`,
  `node_id`, `source_node_id`, `target_node_id`, `chunk_id`) are explicit and cascade-aware.
- [ ] Unique constraints prevent duplicate rows: `files (repo_id, path, commit_sha)`,
  `nodes (repo_id, stable_id)`, and the `embeddings`
  `(chunk_id, provider, model_id, dimensions, content_hash)` key.
- [ ] B-tree indexes support lookup by repo/path (`files_repo_path_idx`), node kind
  (`nodes_repo_kind_idx`), node stable ID (`nodes_repo_stable_idx`), chunk node
  (`chunks_repo_node_idx`), and embedding model (`embeddings_model_idx`).
- [ ] `embeddings.embedding` has a pgvector ANN index (`hnsw` preferred, or `ivfflat` with
  documented list/probe settings) using the correct distance operator for the model.
- [ ] Exact vector scans remain available for small datasets, tests, and ANN recall validation.
- [ ] Edge traversal indexes exist for both outgoing and incoming graph queries:
  `edges_repo_source_idx (repo_id, source_node_id)` and
  `edges_repo_target_idx (repo_id, target_node_id)`.
- [ ] A GIN index (`chunks_search_idx`) backs lexical search over the `chunks.search_vector`
  tsvector for hybrid retrieval.

## Restart And Recovery

- [ ] Ingestion runs are idempotent: restarting after a crash can resume or safely retry without duplicate objects, embeddings, or edges.
- [ ] Writes are transactional at a coherent unit of work, such as file, batch, or run phase; partial failures leave inspectable state.
- [ ] Content hashes gate re-embedding so unchanged code is not embedded again after process restart.
- [ ] Failed runs persist enough error context to diagnose parser, database, and embedder failures.
- [ ] Database startup validates required extensions (`vector`, `pgcrypto`), migration version
  (`_sqlx_migrations`), embedding dimension, and expected indexes before serving queries.
- [ ] Recovery tests cover interrupted ingest, repeated ingest of the same revision, deleted/renamed files, and schema migration rollback/forward expectations.

## No-Mock Embedder Checks

- [ ] Integration tests use a deterministic local or real embedder path, not a mock that bypasses tokenization, dimensions, or model metadata.
- [ ] Tests assert stored vector dimension matches the configured model and database column definition.
- [ ] Tests verify embedder name/version and source content hash are stored with each embedding.
- [ ] Similarity-search tests insert real vectors and assert ordered nearest-neighbor behavior, including a negative or unrelated query case.
- [ ] Failure tests cover embedder timeout/error handling and confirm no orphaned embedding rows are committed.
- [ ] CI or acceptance tests clearly separate fast unit tests from no-mock embedder validation so regressions cannot pass by stubbing embedding behavior.
