-- 1.2: no Merkle roll-up. A summary embedding is keyed by the hash of the
-- summary text itself (communities.summary_hash), so the per-file,
-- per-community and per-repository roll-up hashes no longer have a reader.
-- Summary hashes written by 1.1 do not match the new key; the next `index`
-- or `add` embeds every summary once.
alter table repositories drop column if exists root_hash;
alter table files drop column if exists subtree_hash;
alter table communities drop column if exists subtree_hash;
