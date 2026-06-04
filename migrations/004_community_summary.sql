-- P3: hash-gated community ("god-node") summaries + their embeddings.
--
-- `summary_hash` records the L2 `subtree_hash` the summary was computed from.
-- The gate (P3): re-summarize a community only when its current `subtree_hash`
-- differs from the stored `summary_hash` (or it has no embedding yet) — so a
-- re-index with no content change makes ZERO embedder calls.
--
-- D4 verdict: a dedicated `community_embeddings` table (not the L0 `embeddings`
-- table) keeps L0 frozen and avoids relaxing the `embeddings.chunk_id`
-- not-null/foreign-key constraints. Same pgvector storage + real embedder.
--
-- Additive: `if not exists`. Summaries are NULL until the next analyze/add.

alter table communities add column if not exists summary text;
alter table communities add column if not exists summary_hash text;
alter table communities add column if not exists summarized_at timestamptz;

create index if not exists communities_summary_hash_idx on communities(repo_id, summary_hash);

create table if not exists community_embeddings (
  id uuid primary key,
  community_id uuid not null references communities(id) on delete cascade,
  provider text not null,
  model_id text not null,
  dimensions integer not null,
  content_hash text not null,
  embedding vector not null,
  created_at timestamptz not null default now(),
  unique (community_id, provider, model_id, dimensions),
  check (dimensions = vector_dims(embedding))
);

create index if not exists community_embeddings_model_idx
  on community_embeddings(provider, model_id, dimensions);
