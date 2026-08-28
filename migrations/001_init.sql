-- Chaos Substrate 1.0 schema. Eight tables. Projects live in chaos.toml, not here.
create extension if not exists vector;

-- camelCase / snake_case identifier splitting so FTS matches both vocabularies.
create or replace function chaos_identifier_text(source text)
returns text language sql immutable parallel safe returns null on null input as $$
  select regexp_replace(
           regexp_replace(
             regexp_replace(source, '([A-Z]+)([A-Z][a-z])', '\1 \2', 'g'),
             '([a-z0-9])([A-Z])', '\1 \2', 'g'),
           '[_\./-]+', ' ', 'g')
$$;

create table repositories (
  id uuid primary key,
  name text not null,
  root_path text not null unique,
  commit_sha text,
  root_hash text,
  indexed_at timestamptz not null default now()
);

create table files (
  id uuid primary key,
  repo_id uuid not null references repositories(id) on delete cascade,
  path text not null,
  language text not null,
  content_hash text not null,
  line_count integer not null,
  subtree_hash text,
  unique (repo_id, path)
);

create table nodes (
  id uuid primary key,
  repo_id uuid not null references repositories(id) on delete cascade,
  file_id uuid references files(id) on delete cascade,
  kind text not null,
  stable_id text not null,
  name text not null,
  signature text,
  doc text,
  line_start integer,
  line_end integer,
  metadata jsonb not null default '{}'::jsonb,
  unique (repo_id, stable_id)
);

create table edges (
  id uuid primary key,
  repo_id uuid not null references repositories(id) on delete cascade,
  source_node_id uuid not null references nodes(id) on delete cascade,
  target_node_id uuid not null references nodes(id) on delete cascade,
  kind text not null,
  confidence real not null,
  unique (source_node_id, target_node_id, kind)
);

create table chunks (
  id uuid primary key,
  repo_id uuid not null references repositories(id) on delete cascade,
  file_id uuid references files(id) on delete cascade,
  node_id uuid references nodes(id) on delete set null,
  kind text not null,
  header text not null,
  body text not null,
  content_hash text not null,
  line_start integer,
  line_end integer,
  metadata jsonb not null default '{}'::jsonb,
  search_vector tsvector generated always as
    (to_tsvector('english', header || ' ' || body || ' ' || chaos_identifier_text(header || ' ' || body))) stored
);

create table embeddings (
  chunk_id uuid not null references chunks(id) on delete cascade,
  provider text not null,
  model_id text not null,
  dimensions integer not null,
  content_hash text not null,
  embedding vector not null,
  primary key (chunk_id, provider, model_id, dimensions),
  check (dimensions = vector_dims(embedding))
);

create table communities (
  id uuid primary key,
  repo_id uuid not null references repositories(id) on delete cascade,
  label text not null,
  member_count integer not null,
  subtree_hash text not null,
  summary text,
  summary_hash text,
  summary_provider text,
  summary_model text,
  summary_embedding vector,
  top_members jsonb not null default '[]'::jsonb,
  files jsonb not null default '[]'::jsonb,
  updated_at timestamptz not null default now()
);

create table community_members (
  community_id uuid not null references communities(id) on delete cascade,
  node_id uuid not null references nodes(id) on delete cascade,
  primary key (community_id, node_id)
);

create index files_repo_path_idx on files(repo_id, path);
create index nodes_repo_kind_idx on nodes(repo_id, kind);
create index nodes_repo_name_idx on nodes(repo_id, name);
create index nodes_file_idx on nodes(file_id);
create index edges_repo_source_idx on edges(repo_id, source_node_id);
create index edges_repo_target_idx on edges(repo_id, target_node_id);
create index chunks_repo_file_idx on chunks(repo_id, file_id);
create index chunks_repo_node_idx on chunks(repo_id, node_id);
create index chunks_search_idx on chunks using gin(search_vector);
create index embeddings_model_idx on embeddings(provider, model_id, dimensions);
create index communities_repo_idx on communities(repo_id);
create index community_members_node_idx on community_members(node_id);
