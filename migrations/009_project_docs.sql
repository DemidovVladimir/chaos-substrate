-- P6.1: project-level DOCS members.
--
-- A chaos project (005_projects.sql) is a registry of indexed code repos. But
-- cross-repo design docs — architecture decision records, migration spikes,
-- "X replaces Y" notes — frequently live ABOVE the member repos (at the project
-- root) and were never indexed, so the prose that explains how the repos relate
-- (e.g. supersession) was invisible to both queries and feature stories.
--
-- `project add-docs <project> <dir>` registers such a directory as a synthetic
-- member repo indexed through the normal pipeline (its markdown/PDF become
-- `documentation` chunks). We flag those members here so callers can keep them
-- as a searchable DOC SOURCE while excluding them from "code repos involved"
-- counts. Fully additive; existing members default to false.

alter table project_repos
  add column if not exists is_project_docs boolean not null default false;
