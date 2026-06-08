-- P2: L2 hash-rollup (Merkle) layer.
--
-- Rolls the per-chunk / per-file `content_hash` leaves that already exist up to
-- file → community → repo roots. These content-addressed hashes answer "did X
-- change, and where" in O(log n), drive `chaos add` feature blast-radius, and
-- gate L1 summary recomputation in P3 (only re-summarize a community whose root
-- moved).
--
-- Fully additive (`if not exists`). Hashes are NULL for any repo indexed before
-- this layer until the next analyze/add recomputes them.

alter table files add column if not exists subtree_hash text;
alter table communities add column if not exists subtree_hash text;
alter table repositories add column if not exists repo_root_hash text;

create index if not exists files_subtree_hash_idx on files(repo_id, subtree_hash);
create index if not exists communities_subtree_hash_idx on communities(repo_id, subtree_hash);
