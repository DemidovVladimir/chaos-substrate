-- P7: token-efficiency layer — content-addressed L3 summary cache.
--
-- Community summaries (and their embeddings) are keyed by community ID, but
-- community IDs are derived from the partition (UUIDv5 over the minimum member
-- stable_id). A small edit can reshuffle the Louvain partition and change a
-- community's ID even when its member CONTENT is identical — the old row (and
-- its embedding) cascade-deletes, the hash gate sees a "new" community, and the
-- summary is recomputed + re-embedded for nothing.
--
-- This cache stores each composed summary + embedding by the CONTENT it was
-- computed from: (subtree content_hash, summary algo version, embedder
-- identity). When a community needs a summary and an identical-content entry
-- exists, the summary and embedding are restored with ZERO embedder calls.
-- One row per unique community content per embedder; rows are tiny relative to
-- chunk embeddings and are upserted (latest wins). Fully additive.

create table if not exists community_summary_cache (
  content_hash text not null,
  algo text not null,
  provider text not null,
  model_id text not null,
  dimensions integer not null,
  summary text not null,
  embedding vector not null,
  created_at timestamptz not null default now(),
  primary key (content_hash, algo, provider, model_id, dimensions),
  check (dimensions = vector_dims(embedding))
);
