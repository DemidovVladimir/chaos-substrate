//! P6 — the cross-repository PROJECT layer.
//!
//! A project is a named set of indexed repositories (client, backend, smart
//! contracts, infra, …). Member repos keep their own L0–L3 layers; the project
//! layer maintains CROSS-REPO LINKS between their L1 features (detected by
//! `src/linker.rs`) and is kept fresh by the SAME layered pipeline as
//! everything else: after `analyze`/`add` rebuild L1–L3 for one repo,
//! [`relink_projects_for_repo`] re-runs the linkers for every project that
//! contains it — gated by the L2 repo root hash (`project_repos.
//! linked_repo_hash`), so a no-change re-index relinks nothing.
//!
//! Commands (CLI `chaos project …`, MCP `chaos_project`):
//!   * `create <name>` — create a project (idempotent).
//!   * `add-repo <project> <repo> [--alias client]` — attach an indexed repo;
//!     immediately links it against the existing members.
//!   * `list` — all projects with members and link counts.
//!   * `status <project>` — members, staleness vs the current root hashes,
//!     links by kind, embedder consistency.
//!   * `relink <project> [--force]` — re-run the linkers (hash-gated).

use crate::{
    linker,
    models::{CrossRepoLink, Project, ProjectRepo},
    provenance::{source, Breadcrumb},
    storage::Storage,
};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

/// Compact links returned inline by relink/status (full set stays in Postgres).
const MAX_COMPACT_LINKS: usize = 40;

/// Where project-level artifacts (e.g. the project feature inventory HTML)
/// live: `$CHAOS_PROJECT_DIR/<slug>` or `~/.chaos/projects/<slug>`. A project
/// spans repos, so no single repo's `docs/features_memory` can own its pages.
pub fn project_workspace_dir(project_name: &str) -> PathBuf {
    let base = std::env::var("CHAOS_PROJECT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".chaos")
                .join("projects")
        });
    base.join(safe_slug(project_name))
}

pub async fn create(storage: &Storage, name: &str) -> Result<Value> {
    let name = name.trim();
    anyhow::ensure!(!name.is_empty(), "project name must not be empty");
    let project = storage.create_project(name).await?;
    Ok(json!({
        "status": "ok",
        "project": project.name,
        "project_id": project.id,
        "hint": format!("add indexed repositories with `chaos project add-repo {} <repo-path> --alias <client|backend|contracts|infra>`", project.name),
    }))
}

pub async fn add_repo(
    storage: &Storage,
    project_name: &str,
    repo: &str,
    alias: Option<&str>,
) -> Result<Value> {
    let project = storage
        .find_project(project_name)
        .await?
        .with_context(|| format!("project does not exist: {project_name} (create it with `chaos project create {project_name}`)"))?;
    let repository = storage
        .find_repository(repo)
        .await?
        .with_context(|| format!("repository is not indexed: {repo} (run chaos analyze first)"))?;
    let alias = alias
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(String::from)
        .unwrap_or_else(|| repository.name.clone());
    // The schema enforces unique (project_id, alias); check up front so a
    // collision is a clear message, not a raw 23505 constraint error.
    let members = storage.project_member_repos(project.id).await?;
    if let Some(taken) = members
        .iter()
        .find(|m| m.alias == alias && m.repo.id != repository.id)
    {
        anyhow::bail!(
            "alias `{alias}` is already used by repository {} in project {} — pass --alias to pick a different one",
            taken.repo.name,
            project.name
        );
    }
    storage
        .add_repo_to_project(project.id, repository.id, &alias)
        .await?;

    // Link the new member against the existing ones right away (the gate sees
    // its NULL linked hash as stale, so this always runs on first add).
    let relink = relink_project(storage, &project, false).await?;
    Ok(json!({
        "status": "ok",
        "project": project.name,
        "repo": repository.name,
        "alias": alias,
        "relink": relink,
    }))
}

pub async fn list(storage: &Storage) -> Result<Value> {
    let projects = storage.list_projects().await?;
    let mut out: Vec<Value> = Vec::new();
    for p in &projects {
        let members = storage.project_member_repos(p.id).await?;
        let links = storage.load_project_links(p.id).await?;
        out.push(json!({
            "project": p.name,
            "repos": members.iter().map(|m| json!({
                "alias": m.alias,
                "name": m.repo.name,
                "root_path": m.repo.root_path,
            })).collect::<Vec<_>>(),
            "cross_repo_links": links.len(),
        }));
    }
    Ok(json!({ "status": "ok", "projects": out }))
}

pub async fn status(storage: &Storage, project_name: &str) -> Result<Value> {
    let project = storage
        .find_project(project_name)
        .await?
        .with_context(|| format!("project does not exist: {project_name}"))?;
    let members = storage.project_member_repos(project.id).await?;
    let links = storage.load_project_links(project.id).await?;

    let mut member_rows: Vec<Value> = Vec::new();
    let mut stale = 0usize;
    for m in &members {
        let current = storage.get_repo_root_hash(m.repo.id).await?;
        // Same staleness rule as the relink gate: links are stale when the hash
        // they were computed from differs from the current one.
        let is_stale = m.linked_repo_hash != current;
        if is_stale {
            stale += 1;
        }
        member_rows.push(json!({
            "alias": m.alias,
            "name": m.repo.name,
            "root_path": m.repo.root_path,
            "indexed_root_hash": current,
            "linked_root_hash": m.linked_repo_hash,
            "links_stale": is_stale,
        }));
    }

    let identities = storage.project_embedder_identities(project.id).await?;
    let mut warnings: Vec<String> = Vec::new();
    if identities.len() > 1 {
        warnings.push(embedder_mismatch_warning(&identities));
    }

    Ok(json!({
        "status": "ok",
        "project": project.name,
        "repos": member_rows,
        "stale_repos": stale,
        "links_by_kind": links_by_kind(&links),
        "cross_repo_links": links.len(),
        "links": compact_links(storage, &members, &links).await?,
        "embedders": identities.iter().map(|(p, m, d)| format!("{p}/{m}/{d}")).collect::<Vec<_>>(),
        "workspace": project_workspace_dir(&project.name),
        "warnings": warnings,
    }))
}

pub async fn relink(storage: &Storage, project_name: &str, force: bool) -> Result<Value> {
    let project = storage
        .find_project(project_name)
        .await?
        .with_context(|| format!("project does not exist: {project_name}"))?;
    relink_project(storage, &project, force).await
}

/// Re-run the linkers for every project containing `repo_id` — the pipeline
/// hook `analyze`/`add` call after L1–L3 are rebuilt. Best-effort: a project-
/// layer failure is reported in the returned value, never fails the index run
/// that already succeeded. Returns an empty array when the repo is in no
/// project (the single-repo flow is untouched).
pub async fn relink_projects_for_repo(storage: &Storage, repo_id: Uuid) -> Value {
    let projects = match storage.projects_containing_repo(repo_id).await {
        Ok(p) => p,
        Err(err) => {
            return json!([{ "error": format!("loading projects failed: {err:#}") }]);
        }
    };
    let mut out: Vec<Value> = Vec::new();
    for project in &projects {
        match relink_project(storage, project, false).await {
            Ok(summary) => out.push(summary),
            Err(err) => out.push(json!({
                "project": project.name,
                "error": format!("relink failed: {err:#}"),
            })),
        }
    }
    Value::Array(out)
}

/// The hash-gated relink: skip when every member's current L2 root hash equals
/// the hash its links were computed from (mirror of the L3 summary gate).
async fn relink_project(storage: &Storage, project: &Project, force: bool) -> Result<Value> {
    let members = storage.project_member_repos(project.id).await?;
    let mut current_hashes: HashMap<Uuid, Option<String>> = HashMap::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut stale = force;
    for m in &members {
        let current = storage.get_repo_root_hash(m.repo.id).await?;
        if current.is_none() {
            warnings.push(format!(
                "{}: no repo root hash yet — run chaos analyze/add on it so the L2 layer exists",
                m.alias
            ));
        }
        // Stale when the hash its links were computed from differs from the
        // current one. A repo with no root hash at all (both sides None) is NOT
        // stale — there is nothing new to link from, and treating it as stale
        // would hold the gate open forever (full relink on every add).
        if m.linked_repo_hash != current {
            stale = true;
        }
        current_hashes.insert(m.repo.id, current);
    }

    if !stale {
        let links = storage.load_project_links(project.id).await?;
        return Ok(json!({
            "status": "up_to_date",
            "project": project.name,
            "repos": members.len(),
            "cross_repo_links": links.len(),
            "links_by_kind": links_by_kind(&links),
            "detail": "every member repo's root hash matches its last link run — nothing recomputed (pass force=true to override)",
        }));
    }

    let outcome = linker::detect_project_links(storage, project.id, &members).await?;
    storage
        .replace_project_links(project.id, &outcome.links)
        .await?;
    for m in &members {
        if let Some(Some(hash)) = current_hashes.get(&m.repo.id) {
            storage
                .set_linked_repo_hash(project.id, m.repo.id, hash)
                .await?;
        }
    }

    let identities = storage.project_embedder_identities(project.id).await?;
    warnings.extend(outcome.warnings);
    if identities.len() > 1 {
        warnings.push(embedder_mismatch_warning(&identities));
    }

    let mut provenance = outcome.provenance;
    provenance.push(Breadcrumb::new(
        source::MERKLE,
        "relink_gate",
        format!(
            "relink ran because at least one member's L2 root hash moved since the last link run ({} member repo(s))",
            members.len()
        ),
    ));

    Ok(json!({
        "status": "linked",
        "project": project.name,
        "repos": members.len(),
        "cross_repo_links": outcome.links.len(),
        "links_by_kind": links_by_kind(&outcome.links),
        "links": compact_links(storage, &members, &outcome.links).await?,
        "provenance": provenance,
        "warnings": warnings,
    }))
}

fn links_by_kind(links: &[CrossRepoLink]) -> Value {
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for l in links {
        *counts.entry(l.kind.as_str()).or_insert(0) += 1;
    }
    json!(counts)
}

/// Human-readable `alias:feature-label → alias:feature-label` link rows,
/// capped — the full set stays in Postgres for the surfacing tools.
async fn compact_links(
    storage: &Storage,
    members: &[ProjectRepo],
    links: &[CrossRepoLink],
) -> Result<Vec<Value>> {
    let alias_by_repo: HashMap<Uuid, &str> = members
        .iter()
        .map(|m| (m.repo.id, m.alias.as_str()))
        .collect();
    let mut ids: Vec<Uuid> = links
        .iter()
        .flat_map(|l| [l.source_community_id, l.target_community_id])
        .collect();
    ids.sort();
    ids.dedup();
    let labels = storage.community_labels_for(&ids).await?;
    let endpoint = |repo: Uuid, community: Uuid| {
        format!(
            "{}:{}",
            alias_by_repo.get(&repo).copied().unwrap_or("?"),
            labels.get(&community).map(String::as_str).unwrap_or("?")
        )
    };
    let mut out: Vec<Value> = links
        .iter()
        .take(MAX_COMPACT_LINKS)
        .map(|l| {
            json!({
                "kind": l.kind,
                "confidence": l.confidence,
                "source": endpoint(l.source_repo_id, l.source_community_id),
                "target": endpoint(l.target_repo_id, l.target_community_id),
                "matched": l.evidence.get("matched").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    if links.len() > MAX_COMPACT_LINKS {
        out.push(json!({
            "note": format!("{} more link(s) persisted but not shown", links.len() - MAX_COMPACT_LINKS)
        }));
    }
    Ok(out)
}

fn embedder_mismatch_warning(identities: &[(String, String, i32)]) -> String {
    format!(
        "member repos were embedded with different embedders ({}) — project-wide semantic matching would compare incompatible vector spaces; re-analyze the outliers with one embedder config",
        identities
            .iter()
            .map(|(p, m, d)| format!("{p}/{m}/{d}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn safe_slug(input: &str) -> String {
    let slug = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "project".to_string()
    } else {
        slug.chars().take(80).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_dir_is_slugged_and_env_overridable() {
        std::env::set_var("CHAOS_PROJECT_DIR", "/tmp/chaos-projects");
        let dir = project_workspace_dir("Molecule DeSci!");
        assert_eq!(dir, PathBuf::from("/tmp/chaos-projects/molecule-desci"));
        std::env::remove_var("CHAOS_PROJECT_DIR");
    }
}
