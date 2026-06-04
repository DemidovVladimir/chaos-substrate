-- P1: persisted L1 community / feature layer.
--
-- Communities (god-nodes) are derived from the L0 multigraph by deterministic
-- Louvain detection (src/community.rs). Membership is many-to-many so a node
-- can belong to several communities (overlap/DAG); today detection yields a
-- partition, but a single *file* is routinely split across communities because
-- its symbols are — that shared-file overlap is what drives L2 blast radius.
--
-- Additive: all `if not exists`. L0 (nodes/edges/chunks/embeddings) is
-- untouched; a repo indexed before this migration simply has zero communities.

create table if not exists communities (
  id uuid primary key,
  repo_id uuid not null references repositories(id) on delete cascade,
  level integer not null default 0,
  parent_id uuid references communities(id) on delete set null,
  label text not null,
  member_count integer not null default 0,
  detection_params jsonb not null default '{}'::jsonb,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create table if not exists community_members (
  community_id uuid not null references communities(id) on delete cascade,
  node_id uuid not null references nodes(id) on delete cascade,
  weight double precision not null default 1.0,
  primary key (community_id, node_id)
);

-- Quotient graph: typed, aggregated edges between communities (D3 verdict —
-- preserve the dominant L0 edge kind per boundary, not a single collapsed
-- relation).
create table if not exists community_edges (
  id uuid primary key,
  repo_id uuid not null references repositories(id) on delete cascade,
  source_community_id uuid not null references communities(id) on delete cascade,
  target_community_id uuid not null references communities(id) on delete cascade,
  kind text not null,
  weight double precision not null default 1.0,
  edge_count integer not null default 0,
  metadata jsonb not null default '{}'::jsonb
);

create index if not exists communities_repo_idx on communities(repo_id);
create index if not exists communities_repo_level_idx on communities(repo_id, level);
create index if not exists communities_parent_idx on communities(parent_id);
create index if not exists community_members_community_idx on community_members(community_id);
create index if not exists community_members_node_idx on community_members(node_id);
create index if not exists community_edges_repo_idx on community_edges(repo_id);
create index if not exists community_edges_source_idx on community_edges(source_community_id);
create index if not exists community_edges_target_idx on community_edges(target_community_id);
