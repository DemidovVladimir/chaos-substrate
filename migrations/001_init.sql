create extension if not exists vector;
create extension if not exists pgcrypto;

create table if not exists repositories (
  id uuid primary key,
  name text not null,
  root_path text not null unique,
  remote_url text,
  current_commit_sha text,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create table if not exists analysis_runs (
  id uuid primary key,
  repo_id uuid not null references repositories(id) on delete cascade,
  commit_sha text not null,
  status text not null,
  error text,
  started_at timestamptz not null default now(),
  finished_at timestamptz
);

create table if not exists files (
  id uuid primary key,
  repo_id uuid not null references repositories(id) on delete cascade,
  commit_sha text not null,
  path text not null,
  language text not null,
  content_hash text not null,
  line_count integer not null,
  indexed_at timestamptz not null default now(),
  unique (repo_id, path, commit_sha)
);

create table if not exists nodes (
  id uuid primary key,
  repo_id uuid not null references repositories(id) on delete cascade,
  file_id uuid references files(id) on delete cascade,
  kind text not null,
  stable_id text not null,
  name text not null,
  line_start integer,
  line_end integer,
  metadata jsonb not null default '{}'::jsonb,
  unique (repo_id, stable_id)
);

create table if not exists edges (
  id uuid primary key,
  repo_id uuid not null references repositories(id) on delete cascade,
  source_node_id uuid not null references nodes(id) on delete cascade,
  target_node_id uuid not null references nodes(id) on delete cascade,
  kind text not null,
  cost double precision not null,
  confidence double precision not null,
  metadata jsonb not null default '{}'::jsonb
);

create table if not exists chunks (
  id uuid primary key,
  repo_id uuid not null references repositories(id) on delete cascade,
  file_id uuid references files(id) on delete cascade,
  node_id uuid references nodes(id) on delete set null,
  chunk_type text not null,
  content text not null,
  content_hash text not null,
  line_start integer,
  line_end integer,
  metadata jsonb not null default '{}'::jsonb,
  search_vector tsvector not null
);

create table if not exists embeddings (
  id uuid primary key,
  chunk_id uuid not null references chunks(id) on delete cascade,
  provider text not null,
  model_id text not null,
  dimensions integer not null,
  content_hash text not null,
  embedding vector not null,
  created_at timestamptz not null default now(),
  unique (chunk_id, provider, model_id, dimensions, content_hash),
  check (dimensions = vector_dims(embedding))
);

create index if not exists repositories_root_path_idx on repositories(root_path);
create index if not exists files_repo_path_idx on files(repo_id, path);
create index if not exists nodes_repo_kind_idx on nodes(repo_id, kind);
create index if not exists nodes_repo_stable_idx on nodes(repo_id, stable_id);
create index if not exists edges_repo_source_idx on edges(repo_id, source_node_id);
create index if not exists edges_repo_target_idx on edges(repo_id, target_node_id);
create index if not exists chunks_repo_node_idx on chunks(repo_id, node_id);
create index if not exists chunks_search_idx on chunks using gin(search_vector);
create index if not exists embeddings_model_idx on embeddings(provider, model_id, dimensions);
