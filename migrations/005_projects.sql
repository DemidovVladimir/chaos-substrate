-- P6: cross-repository PROJECT layer.
--
-- A project is a named set of indexed repositories (client, backend, smart
-- contracts, infra, …). Each member repo keeps its own L0–L3 layers untouched;
-- the project layer adds CROSS-REPO LINKS between L1 communities (features) of
-- different member repos, detected by the linkers in src/linker.rs
-- (package_dep / abi / http_route) purely from the persisted index.
--
-- Links attach at L1 (feature ↔ feature), NOT L0: cross-repo references are
-- name/path matches, so pinning them to AST nodes would be false precision —
-- and it keeps the FK-protected L0 schema frozen. `on delete cascade` from
-- `communities` means a re-detection that reshapes a repo's features drops its
-- stale links automatically; the next relink (hash-gated via
-- `project_repos.linked_repo_hash` vs `repositories.repo_root_hash`) rebuilds
-- them. Fully additive (`if not exists`); single-repo flows are unaffected.

create table if not exists projects (
  id uuid primary key,
  name text not null unique,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now()
);

create table if not exists project_repos (
  project_id uuid not null references projects(id) on delete cascade,
  repo_id uuid not null references repositories(id) on delete cascade,
  alias text not null,
  -- repositories.repo_root_hash at the last successful link run; the L2 gate
  -- for relinking (mirror of the L3 summary gate).
  linked_repo_hash text,
  added_at timestamptz not null default now(),
  primary key (project_id, repo_id),
  unique (project_id, alias)
);

create table if not exists cross_repo_links (
  id uuid primary key,
  project_id uuid not null references projects(id) on delete cascade,
  source_repo_id uuid not null references repositories(id) on delete cascade,
  source_community_id uuid not null references communities(id) on delete cascade,
  target_repo_id uuid not null references repositories(id) on delete cascade,
  target_community_id uuid not null references communities(id) on delete cascade,
  -- package_dep | abi | http_route (consumer feature → provider feature).
  kind text not null,
  -- matched names/paths + provenance breadcrumbs (how the link was detected).
  evidence jsonb not null default '{}'::jsonb,
  confidence double precision not null,
  created_at timestamptz not null default now(),
  unique (project_id, source_community_id, target_community_id, kind)
);

create index if not exists project_repos_repo_idx on project_repos(repo_id);
create index if not exists cross_repo_links_project_idx on cross_repo_links(project_id);
create index if not exists cross_repo_links_source_idx on cross_repo_links(source_community_id);
create index if not exists cross_repo_links_target_idx on cross_repo_links(target_community_id);
