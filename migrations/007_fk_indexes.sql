-- FK-trigger support indexes.
--
-- Deleting a node fires per-row FK lookups on edges.source_node_id,
-- edges.target_node_id (cascade) and chunks.node_id (set null); deleting a
-- file fires nodes.file_id and chunks.file_id (cascade). Only composite
-- (repo_id, …) indexes existed, which a single-column FK lookup cannot use,
-- so every lookup was a SEQUENTIAL SCAN — and rows deleted earlier in the
-- same transaction are dead but still scannable, so a repo purge went
-- quadratic: a molecule_core `chaos clean` ran 16+ minutes, with pg_stat
-- showing 617M tuples read across 178k seq scans on edges alone. The same
-- cascade cost hit every incremental `chaos add` (files → nodes → edges).
-- With these indexes each FK lookup is a btree probe.
create index if not exists edges_source_node_idx on edges(source_node_id);
create index if not exists edges_target_node_idx on edges(target_node_id);
create index if not exists chunks_node_idx on chunks(node_id);
create index if not exists chunks_file_idx on chunks(file_id);
create index if not exists nodes_file_idx on nodes(file_id);
