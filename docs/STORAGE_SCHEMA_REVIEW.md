# Storage Schema Review Checklist

Validation checklist for a Rust code knowledge memory backed by Postgres and pgvector.

## Required Tables

- [ ] `code_objects`: canonical symbols or chunks with stable ID, project/repo ID, path, language, object kind, symbol name, byte/line ranges, content hash, and timestamps.
- [ ] `embeddings`: one row per embedded object with object ID foreign key, embedder name/version, embedding dimension, `vector` column, content hash used for embedding, and timestamps.
- [ ] `relationships`: directed code graph edges with source object ID, target object ID, relationship kind, confidence/source, and uniqueness over `(source, target, kind)`.
- [ ] `ingestion_runs`: durable ingest metadata with run ID, project/repo ID, source revision, status, started/finished timestamps, error details, and counters.
- [ ] `source_files`: file-level state with project/repo ID, path, revision, content hash, parser status, and deletion marker.
- [ ] `schema_migrations`: migration version, checksum, applied timestamp, and executor identity.

## Required Indexes

- [ ] Primary keys on every table use stable UUIDs or deterministic IDs; foreign keys are explicit and indexed.
- [ ] Unique index prevents duplicate code objects for the same project, revision/content hash, path, kind, and range/symbol identity.
- [ ] B-tree indexes support lookup by project/repo, path, symbol name, object kind, content hash, and ingestion run status.
- [ ] `embeddings.vector` has a pgvector ANN index (`hnsw` preferred, or `ivfflat` with documented list/probe settings) using the correct distance operator for the model.
- [ ] Exact vector scans remain available for small datasets, tests, and ANN recall validation.
- [ ] Relationship traversal indexes exist for both outgoing and incoming graph queries: `(source_object_id, kind)` and `(target_object_id, kind)`.
- [ ] Partial indexes cover common live-row filters if soft deletes or tombstones are used.

## Restart And Recovery

- [ ] Ingestion runs are idempotent: restarting after a crash can resume or safely retry without duplicate objects, embeddings, or edges.
- [ ] Writes are transactional at a coherent unit of work, such as file, batch, or run phase; partial failures leave inspectable state.
- [ ] Content hashes gate re-embedding so unchanged code is not embedded again after process restart.
- [ ] Failed runs persist enough error context to diagnose parser, database, and embedder failures.
- [ ] Database startup validates required extensions (`vector`), migration version, embedding dimension, and expected indexes before serving queries.
- [ ] Recovery tests cover interrupted ingest, repeated ingest of the same revision, deleted/renamed files, and schema migration rollback/forward expectations.

## No-Mock Embedder Checks

- [ ] Integration tests use a deterministic local or real embedder path, not a mock that bypasses tokenization, dimensions, or model metadata.
- [ ] Tests assert stored vector dimension matches the configured model and database column definition.
- [ ] Tests verify embedder name/version and source content hash are stored with each embedding.
- [ ] Similarity-search tests insert real vectors and assert ordered nearest-neighbor behavior, including a negative or unrelated query case.
- [ ] Failure tests cover embedder timeout/error handling and confirm no orphaned embedding rows are committed.
- [ ] CI or acceptance tests clearly separate fast unit tests from no-mock embedder validation so regressions cannot pass by stubbing embedding behavior.
