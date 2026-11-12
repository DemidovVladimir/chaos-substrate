-- 1.4: the stamp of a run is HEAD plus the paths uncommitted at that moment
-- plus the listing rules it used. `add` and `reindex` re-read the git delta
-- from that commit, plus these paths, so a later revert or commit of a
-- once-dirty file is seen.
--
-- `worktree_paths` NULL = the stamp is incomplete: a run marks it so before
-- its first index write and completes it at the end. Rows stamped before
-- this migration start NULL too (whether their worktree was dirty is
-- unknown), so their next `add` reconciles every file once and their next
-- `reindex` runs in full.
alter table repositories add column if not exists worktree_paths jsonb;
-- Fingerprint of skip_dirs and the indexable-path rule version; a different
-- value at the next run falls back the same way.
alter table repositories add column if not exists listing_rules text;
