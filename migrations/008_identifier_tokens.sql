-- Identifier-aware full-text tokenization.
--
-- Code carries most of its meaning in compound identifiers, but Postgres FTS
-- treats "listAllOnChainLabs" as ONE lexeme, so a query for "on chain labs"
-- could never keyword-match the code that implements it (found via the
-- graph --serve validation surface: desci-infra/lambda/ocl-processor was
-- unreachable for the very phrase it exists for).
--
-- chaos_identifier_text() splits camelCase / PascalCase / ACRONYMWord
-- boundaries (and _ - . separators) into spaces. The search_vector indexes
-- the ORIGINAL content plus the split rendering, so both vocabularies match:
-- "listAllOnChainLabs" (exact identifier) and "on chain labs" (the words).
-- Pure SQL and deterministic: the backfill below rebuilds every existing
-- chunk's search_vector from already-stored text — no re-analyze, no
-- re-embedding, embedder-free.

create or replace function chaos_identifier_text(source text)
returns text
language sql
immutable
parallel safe
returns null on null input
as $$
  select regexp_replace(
           regexp_replace(
             regexp_replace(source, '([A-Z]+)([A-Z][a-z])', '\1 \2', 'g'),
             '([a-z0-9])([A-Z])', '\1 \2', 'g'),
           '[_\./-]+', ' ', 'g')
$$;

-- One-time backfill over existing chunks (chunk content is bounded by the
-- chunker, so the doubled text stays far below tsvector limits).
update chunks
set search_vector = to_tsvector('english', content || ' ' || chaos_identifier_text(content));
