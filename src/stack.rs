//! `chaos stack` — the TECH-STACK inventory of an indexed repository.
//!
//! Answers *"what is this repo built with?"* from the persisted index alone:
//! the manifest-DECLARED dependencies (package.json / Cargo.toml entries, with
//! versions and runtime-vs-dev scope), npm scripts, deployment resources (AWS
//! CDK apps, Stack classes, and L2 constructs grouped by cloud service),
//! indexed JS/TS configs, and the file-language breakdown. `chaos_stats` only
//! *counts* these node kinds; this tool *lists* them — so an agent asked "what
//! is the stack / infrastructure here?" never has to fall back to grepping
//! manifests off disk.
//!
//! Read-only and embedder-free, like `chaos_stats`. Like the other surfacing
//! tools it ALWAYS writes an interactive HTML inventory (default
//! `docs/features_memory/stack.html`, a repo-level singleton like `graph.html`)
//! and returns a COMPACT JSON summary — capped lists inline, every entry in the
//! HTML — with provenance breadcrumbs throughout.
//!
//! Honesty contract: the report states its COVERAGE explicitly. It can only
//! list what the extractor persists (npm + cargo manifests, AWS CDK, npm
//! scripts, tsconfig/jsconfig); Dockerfiles, CI workflows, pyproject.toml,
//! foundry.toml, Terraform etc. are not indexed yet and are named as such
//! rather than silently omitted.

use crate::{
    export_util::escape_script_json,
    provenance::{source, Breadcrumb},
    storage::{StackDependencyRow, StackDeploymentRow, StackScriptRow, Storage},
};
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

/// Inline caps for the compact MCP/CLI return. The HTML keeps every entry;
/// omission counts are lifted to top-level `*_omitted` fields (uniform arrays —
/// never a mixed-shape "note" row).
const MAX_COMPACT_PACKAGES: usize = 30;
const MAX_COMPACT_MANIFESTS: usize = 20;
const MAX_COMPACT_STACKS: usize = 40;
const MAX_COMPACT_SCRIPTS: usize = 15;
const MAX_COMPACT_CONFIGS: usize = 15;

#[derive(Debug, Default, Clone)]
pub struct StackOptions {
    pub output_html: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StackManifest {
    pub schema_version: String,
    pub repo_name: String,
    pub title: String,
    pub subtitle: String,
    pub overview: String,
    /// `[{name, count}]` files by language, straight from the index.
    pub languages: Value,
    pub totals: StackTotals,
    pub ecosystems: Vec<EcosystemStack>,
    /// Distinct script names, most widely declared first.
    pub scripts: Vec<ScriptSummary>,
    pub scripts_total: usize,
    pub infrastructure: Vec<TechnologyStack>,
    pub config_files: Vec<String>,
    pub coverage: Coverage,
    pub provenance: Vec<Breadcrumb>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct StackTotals {
    /// Distinct package names across all ecosystems.
    pub packages: usize,
    /// Dependency manifests (package.json / Cargo.toml) the declarations live in.
    pub manifests: usize,
    pub scripts: usize,
    pub deployment_resources: usize,
    /// CDK Stack classes (the unit people mean by "how many stacks").
    pub stacks: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct EcosystemStack {
    /// `npm` | `cargo`.
    pub ecosystem: String,
    /// Manifests of this ecosystem, most dependencies first.
    pub manifests: Vec<ManifestSummary>,
    /// Distinct packages, most widely declared first.
    pub packages: Vec<PackageSummary>,
    /// Total declaration rows (a package declared in 3 manifests counts 3).
    pub declared_total: usize,
    pub runtime_packages: usize,
    pub dev_packages: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ManifestSummary {
    pub path: String,
    pub dependencies: usize,
    pub runtime: usize,
    pub dev: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageSummary {
    pub name: String,
    /// Distinct declared version requirements (empty for Cargo — not extracted).
    pub versions: Vec<String>,
    /// `runtime` | `dev` | `mixed`.
    pub scope: String,
    /// Declared in this many manifests — the workspace-importance signal.
    pub manifests: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScriptSummary {
    pub name: String,
    /// Declared in this many manifests.
    pub manifests: usize,
    /// One representative command (deterministic: first by manifest path).
    pub example: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TechnologyStack {
    /// e.g. `aws_cdk`.
    pub technology: String,
    /// Deployment app entrypoints (cdk.json), `{file, command}`.
    pub apps: Vec<DeploymentApp>,
    /// Stack classes, `{name, file, line}`.
    pub stacks: Vec<DeploymentStack>,
    /// L2 constructs grouped by cloud service, biggest first.
    pub services: Vec<ServiceSummary>,
    /// Construct (resource) count across all services.
    pub resources_total: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeploymentApp {
    pub file: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeploymentStack {
    pub name: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceSummary {
    pub service: String,
    pub count: usize,
    /// Up to 3 representative resources ("lambda.Function ApiHandler").
    pub examples: Vec<String>,
}

/// What the report covers — and, just as importantly, what the index does not
/// extract yet, so an agent never mistakes this inventory for a complete scan.
#[derive(Debug, Clone, Serialize)]
pub struct Coverage {
    pub included: Vec<String>,
    pub not_indexed: Vec<String>,
}

impl Coverage {
    fn current() -> Self {
        Self {
            included: vec![
                "package.json dependencies/devDependencies/peerDependencies/optionalDependencies + scripts".into(),
                "Cargo.toml dependencies/dev-dependencies/build-dependencies".into(),
                "AWS CDK: cdk.json app entrypoints, Stack classes, L2 constructs by service".into(),
                "tsconfig/jsconfig configuration files".into(),
                "file-language breakdown of everything indexed".into(),
            ],
            not_indexed: vec![
                "Dockerfile / docker-compose.yml".into(),
                "CI workflows (.github/workflows, GitLab CI)".into(),
                "pyproject.toml / requirements.txt / go.mod".into(),
                "foundry.toml / hardhat config contents (the TS/JS code itself is indexed, the toolchain config is not extracted)".into(),
                "Terraform / CloudFormation templates".into(),
            ],
        }
    }
}

/// Run the inventory: query the persisted facets → aggregate → write HTML →
/// return the compact JSON.
pub async fn run(storage: &Storage, repo: &str, opts: &StackOptions) -> Result<Value> {
    let repo = storage
        .find_repository(repo)
        .await?
        .with_context(|| format!("repository is not indexed: {repo}"))?;
    let repo_root = PathBuf::from(&repo.root_path);

    let mut provenance: Vec<Breadcrumb> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    let dependencies = storage.stack_dependencies(repo.id).await?;
    provenance.push(
        Breadcrumb::new(
            source::POSTGRES,
            "stack_dependencies",
            format!(
                "{} manifest-declared dependency row(s) (import-derived nodes excluded)",
                dependencies.len()
            ),
        )
        .with_locator("nodes"),
    );
    let scripts = storage.stack_scripts(repo.id).await?;
    provenance.push(
        Breadcrumb::new(
            source::POSTGRES,
            "stack_scripts",
            format!("{} npm script row(s)", scripts.len()),
        )
        .with_locator("nodes"),
    );
    let deployments = storage.stack_deployment_resources(repo.id).await?;
    provenance.push(
        Breadcrumb::new(
            source::POSTGRES,
            "stack_deployment_resources",
            format!("{} deployment resource(s)", deployments.len()),
        )
        .with_locator("nodes"),
    );
    let config_files = storage.stack_config_files(repo.id).await?;
    provenance.push(
        Breadcrumb::new(
            source::POSTGRES,
            "stack_config_files",
            format!("{} JS/TS config file(s)", config_files.len()),
        )
        .with_locator("chunks"),
    );
    let languages = storage.group_counts(repo.id, "files", "language").await?;
    provenance.push(
        Breadcrumb::new(
            source::POSTGRES,
            "group_counts",
            "files grouped by language",
        )
        .with_locator("files"),
    );

    if dependencies.is_empty() && deployments.is_empty() {
        warnings.push(
            "no declared dependencies or deployment resources in the index — if the repo has manifests, re-run chaos_analyze (the last index may predate manifest extraction)"
                .into(),
        );
    }

    let ecosystems = aggregate_ecosystems(&dependencies);
    let script_summaries = aggregate_scripts(&scripts);
    let infrastructure = aggregate_infrastructure(&deployments);

    let totals = StackTotals {
        packages: ecosystems.iter().map(|e| e.packages.len()).sum(),
        manifests: ecosystems.iter().map(|e| e.manifests.len()).sum(),
        scripts: scripts.len(),
        deployment_resources: deployments.len(),
        stacks: infrastructure.iter().map(|t| t.stacks.len()).sum(),
    };
    let overview = compose_overview(
        &repo.name,
        &languages,
        &ecosystems,
        &infrastructure,
        &totals,
    );

    let manifest = StackManifest {
        schema_version: "stack-inventory-1".to_string(),
        repo_name: repo.name.clone(),
        title: format!("{} — tech stack", repo.name),
        subtitle: "Declared dependencies, scripts, deployment resources, and configs read from the persisted index — what this repository is built with, with explicit coverage notes."
            .to_string(),
        overview,
        languages,
        totals,
        ecosystems,
        scripts: script_summaries,
        scripts_total: scripts.len(),
        infrastructure,
        config_files,
        coverage: Coverage::current(),
        provenance,
        warnings,
    };

    let output = opts
        .output_html
        .clone()
        .unwrap_or_else(|| repo_root.join("docs/features_memory/stack.html"));
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    write_stack_html(&output, &manifest)?;

    Ok(compact_return(&manifest, &output, repo.id))
}

/// Map a manifest section to runtime/dev scope. These are the section names the
/// extractor itself emits, not a query-side keyword list.
fn section_scope(section: &str) -> &'static str {
    match section {
        "devDependencies" | "dev-dependencies" | "build-dependencies" => "dev",
        _ => "runtime",
    }
}

/// Per-package accumulator while folding dependency rows.
#[derive(Default)]
struct PackageAcc {
    versions: Vec<String>,
    runtime: bool,
    dev: bool,
    manifests: BTreeSet<String>,
}

fn aggregate_ecosystems(rows: &[StackDependencyRow]) -> Vec<EcosystemStack> {
    // (ecosystem, name) -> accumulator; BTreeMaps keep it deterministic.
    let mut packages: BTreeMap<(String, String), PackageAcc> = BTreeMap::new();
    let mut manifests: BTreeMap<(String, String), (usize, usize, usize)> = BTreeMap::new();
    let mut declared: BTreeMap<String, usize> = BTreeMap::new();
    for row in rows {
        let entry = packages
            .entry((row.ecosystem.clone(), row.name.clone()))
            .or_default();
        if !row.version.is_empty() && !entry.versions.contains(&row.version) {
            entry.versions.push(row.version.clone());
        }
        match section_scope(&row.section) {
            "dev" => entry.dev = true,
            _ => entry.runtime = true,
        }
        entry.manifests.insert(row.manifest.clone());

        let m = manifests
            .entry((row.ecosystem.clone(), row.manifest.clone()))
            .or_default();
        m.0 += 1;
        match section_scope(&row.section) {
            "dev" => m.2 += 1,
            _ => m.1 += 1,
        }
        *declared.entry(row.ecosystem.clone()).or_default() += 1;
    }

    let mut by_eco: BTreeMap<String, EcosystemStack> = BTreeMap::new();
    for ((eco, name), acc) in packages {
        let mut versions = acc.versions;
        versions.truncate(4);
        let scope = match (acc.runtime, acc.dev) {
            (true, true) => "mixed",
            (false, true) => "dev",
            _ => "runtime",
        };
        let stack = by_eco.entry(eco.clone()).or_insert_with(|| EcosystemStack {
            ecosystem: eco.clone(),
            manifests: Vec::new(),
            packages: Vec::new(),
            declared_total: declared.get(&eco).copied().unwrap_or(0),
            runtime_packages: 0,
            dev_packages: 0,
        });
        match scope {
            "dev" => stack.dev_packages += 1,
            _ => stack.runtime_packages += 1,
        }
        stack.packages.push(PackageSummary {
            name,
            versions,
            scope: scope.to_string(),
            manifests: acc.manifests.len(),
        });
    }
    for ((eco, path), (total, runtime, dev)) in manifests {
        if let Some(stack) = by_eco.get_mut(&eco) {
            stack.manifests.push(ManifestSummary {
                path,
                dependencies: total,
                runtime,
                dev,
            });
        }
    }

    let mut out: Vec<EcosystemStack> = by_eco.into_values().collect();
    for stack in &mut out {
        // Widest-declared first — a package every workspace member pulls in is
        // the load-bearing one; runtime before dev on ties, then name.
        stack.packages.sort_by(|a, b| {
            b.manifests
                .cmp(&a.manifests)
                .then_with(|| (a.scope == "dev").cmp(&(b.scope == "dev")))
                .then_with(|| a.name.cmp(&b.name))
        });
        stack.manifests.sort_by(|a, b| {
            b.dependencies
                .cmp(&a.dependencies)
                .then_with(|| a.path.cmp(&b.path))
        });
    }
    // Biggest ecosystem first.
    out.sort_by(|a, b| {
        b.packages
            .len()
            .cmp(&a.packages.len())
            .then_with(|| a.ecosystem.cmp(&b.ecosystem))
    });
    out
}

fn aggregate_scripts(rows: &[StackScriptRow]) -> Vec<ScriptSummary> {
    let mut by_name: BTreeMap<String, (BTreeSet<String>, String)> = BTreeMap::new();
    for row in rows {
        let entry = by_name.entry(row.name.clone()).or_default();
        entry.0.insert(row.manifest.clone());
        // Rows arrive ordered by (name, manifest), so the first command seen is
        // the deterministic representative.
        if entry.1.is_empty() {
            entry.1 = row.command.clone();
        }
    }
    let mut out: Vec<ScriptSummary> = by_name
        .into_iter()
        .map(|(name, (manifests, example))| ScriptSummary {
            name,
            manifests: manifests.len(),
            example,
        })
        .collect();
    out.sort_by(|a, b| {
        b.manifests
            .cmp(&a.manifests)
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

fn aggregate_infrastructure(rows: &[StackDeploymentRow]) -> Vec<TechnologyStack> {
    let mut by_tech: BTreeMap<String, TechnologyStack> = BTreeMap::new();
    for row in rows {
        let tech = if row.technology.is_empty() {
            "unknown".to_string()
        } else {
            row.technology.clone()
        };
        let stack = by_tech
            .entry(tech.clone())
            .or_insert_with(|| TechnologyStack {
                technology: tech,
                apps: Vec::new(),
                stacks: Vec::new(),
                services: Vec::new(),
                resources_total: 0,
            });
        match row.resource_kind.as_str() {
            "app" => stack.apps.push(DeploymentApp {
                file: row.file.clone(),
                command: row.app.clone(),
            }),
            "stack" => stack.stacks.push(DeploymentStack {
                name: row.name.clone(),
                file: row.file.clone(),
                line: row.line_start,
            }),
            _ => {
                stack.resources_total += 1;
                let service = if row.service.is_empty() {
                    "other".to_string()
                } else {
                    row.service.clone()
                };
                match stack.services.iter_mut().find(|s| s.service == service) {
                    Some(s) => {
                        s.count += 1;
                        if s.examples.len() < 3 {
                            s.examples.push(row.name.clone());
                        }
                    }
                    None => stack.services.push(ServiceSummary {
                        service,
                        count: 1,
                        examples: vec![row.name.clone()],
                    }),
                }
            }
        }
    }
    let mut out: Vec<TechnologyStack> = by_tech.into_values().collect();
    for tech in &mut out {
        tech.apps.sort_by(|a, b| a.file.cmp(&b.file));
        tech.stacks
            .sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.file.cmp(&b.file)));
        tech.services.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.service.cmp(&b.service))
        });
    }
    out.sort_by(|a, b| a.technology.cmp(&b.technology));
    out
}

/// Deterministic extractive overview (pure — same inputs ⇒ same text).
fn compose_overview(
    repo_name: &str,
    languages: &Value,
    ecosystems: &[EcosystemStack],
    infrastructure: &[TechnologyStack],
    totals: &StackTotals,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(top) = languages.as_array().and_then(|a| a.first()) {
        let name = top.get("name").and_then(Value::as_str).unwrap_or("?");
        let count = top.get("count").and_then(Value::as_i64).unwrap_or(0);
        parts.push(format!("mostly {name} ({count} files)"));
    }
    for eco in ecosystems {
        parts.push(format!(
            "{} distinct {} package(s) across {} manifest(s)",
            eco.packages.len(),
            eco.ecosystem,
            eco.manifests.len()
        ));
    }
    for tech in infrastructure {
        let services = tech
            .services
            .iter()
            .take(3)
            .map(|s| format!("{} {}", s.count, s.service))
            .collect::<Vec<_>>()
            .join(", ");
        let mut line = format!(
            "{}: {} stack(s), {} resource(s)",
            tech.technology,
            tech.stacks.len(),
            tech.resources_total
        );
        if !services.is_empty() {
            line.push_str(&format!(" (top: {services})"));
        }
        parts.push(line);
    }
    if totals.scripts > 0 {
        parts.push(format!("{} npm script(s)", totals.scripts));
    }
    if parts.is_empty() {
        return format!(
            "{repo_name} has no declared stack data in the index — re-run chaos_analyze if the repository has manifests."
        );
    }
    format!(
        "{repo_name}'s declared stack: {}. Full inventory in the HTML; coverage notes state what the index does not extract yet.",
        parts.join("; ")
    )
}

/// The compact MCP/CLI return: capped lists, lifted omission counts, full
/// detail in the HTML.
fn compact_return(manifest: &StackManifest, output: &Path, repo_id: uuid::Uuid) -> Value {
    let ecosystems: Vec<Value> = manifest
        .ecosystems
        .iter()
        .map(|eco| {
            let packages_omitted = eco.packages.len().saturating_sub(MAX_COMPACT_PACKAGES);
            let manifests_omitted = eco.manifests.len().saturating_sub(MAX_COMPACT_MANIFESTS);
            json!({
                "ecosystem": eco.ecosystem,
                "packages_total": eco.packages.len(),
                "runtime_packages": eco.runtime_packages,
                "dev_packages": eco.dev_packages,
                "declared_total": eco.declared_total,
                "manifests_total": eco.manifests.len(),
                "manifests": eco.manifests.iter().take(MAX_COMPACT_MANIFESTS).map(|m| json!({
                    "path": m.path, "dependencies": m.dependencies, "runtime": m.runtime, "dev": m.dev
                })).collect::<Vec<_>>(),
                "manifests_omitted": manifests_omitted,
                "top_packages": eco.packages.iter().take(MAX_COMPACT_PACKAGES).map(|p| json!({
                    "name": p.name, "versions": p.versions, "scope": p.scope, "manifests": p.manifests
                })).collect::<Vec<_>>(),
                "packages_omitted": packages_omitted,
            })
        })
        .collect();

    let infrastructure: Vec<Value> = manifest
        .infrastructure
        .iter()
        .map(|tech| {
            let stacks_omitted = tech.stacks.len().saturating_sub(MAX_COMPACT_STACKS);
            json!({
                "technology": tech.technology,
                "apps": tech.apps,
                "stacks_total": tech.stacks.len(),
                "stacks": tech.stacks.iter().take(MAX_COMPACT_STACKS).collect::<Vec<_>>(),
                "stacks_omitted": stacks_omitted,
                "services": tech.services,
                "resources_total": tech.resources_total,
            })
        })
        .collect();

    let scripts_omitted = manifest.scripts.len().saturating_sub(MAX_COMPACT_SCRIPTS);
    let configs_omitted = manifest
        .config_files
        .len()
        .saturating_sub(MAX_COMPACT_CONFIGS);

    json!({
        "status": "ok",
        "repo": manifest.repo_name,
        "repo_id": repo_id,
        "overview": manifest.overview,
        "totals": manifest.totals,
        "languages": manifest.languages,
        "ecosystems": ecosystems,
        "scripts_total": manifest.scripts_total,
        "scripts": manifest.scripts.iter().take(MAX_COMPACT_SCRIPTS).collect::<Vec<_>>(),
        "scripts_omitted": scripts_omitted,
        "infrastructure": infrastructure,
        "config_files": manifest.config_files.iter().take(MAX_COMPACT_CONFIGS).collect::<Vec<_>>(),
        "config_files_omitted": configs_omitted,
        "coverage": manifest.coverage,
        "provenance": manifest.provenance,
        "output_html": output,
        "warnings": manifest.warnings,
    })
}

fn write_stack_html(path: &Path, manifest: &StackManifest) -> Result<()> {
    let json = serde_json::to_string(manifest)?;
    fs::write(
        path,
        STACK_HTML
            .replace("__THEME__", crate::theme::THEME_CSS)
            .replace(
                "__BRAND_TOPBAR__",
                &crate::theme::render_brand(&crate::theme::Brand::default(), "topbar"),
            )
            .replace(
                "__BRAND_FOOTER__",
                &crate::theme::render_brand(&crate::theme::Brand::default(), "footer"),
            )
            .replace("__DATA__", &escape_script_json(&json)),
    )?;
    Ok(())
}

const STACK_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Tech stack</title>
<style>
__THEME__
/* ===== tech stack inventory (light editorial) ===== */
header.ov{background:var(--bg-sky-soft);border-bottom:var(--border-hairline)}
header.ov .wrap{padding:48px 32px 36px}
header.ov .eyebrow{font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.16em;color:var(--color-blue-700);margin-bottom:16px;display:flex;align-items:center;gap:10px}
header.ov .eyebrow::before{content:"";width:22px;height:1px;background:var(--color-blue-500);display:inline-block}
header.ov h1{font:var(--type-display-lg);letter-spacing:-.01em;color:var(--color-ink-700);margin:0 0 10px}
#overview{font:var(--type-body-lg);color:var(--color-ink-500);line-height:1.55;max-width:80ch}
.sub{color:var(--color-ink-400);max-width:76ch;margin-top:14px;font:var(--type-body-sm);line-height:1.6}
main{padding:40px 0 64px;display:grid;gap:24px}
.panel{background:var(--color-surface-0);border:var(--border-hairline);border-radius:var(--radius-lg);box-shadow:var(--shadow-sm);padding:24px}
h2{font:var(--type-h4);color:var(--color-ink-700);margin:0 0 16px}
h3{font:var(--type-h5);color:var(--color-ink-700);margin:18px 0 8px}
.muted{color:var(--fg-tertiary);line-height:1.5}
.stats{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:16px}
.stat{border:var(--border-hairline);border-radius:var(--radius-md);background:var(--color-surface-2);padding:18px}
.stat b{display:block;font:var(--type-h2);font-family:var(--font-display);color:var(--color-ink-700);line-height:1}
.stat span{display:block;color:var(--fg-tertiary);font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.08em;margin-top:8px}
.lang{display:inline-flex;align-items:center;gap:6px;border-radius:var(--radius-pill);padding:3px 10px;margin:6px 6px 0 0;font:var(--type-overline-sm);font-family:var(--font-mono);background:var(--color-blue-50);color:var(--color-blue-700)}
table{width:100%;border-collapse:collapse;font:var(--type-body-sm)}
th{font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.06em;color:var(--fg-tertiary);text-align:left;padding:8px 10px;border-bottom:var(--border-hairline)}
td{padding:7px 10px;border-bottom:var(--border-hairline);color:var(--color-ink-500);vertical-align:top}
td.mono,th.num{font-family:var(--font-mono)}
td.num,th.num{text-align:right}
.scope{display:inline-flex;align-items:center;border-radius:var(--radius-pill);padding:2px 9px;font:var(--type-overline-sm);font-family:var(--font-mono);text-transform:uppercase;letter-spacing:.05em}
.scope.runtime{color:#007f76;background:rgba(0,200,187,.12)}
.scope.dev{color:var(--color-purple-500);background:var(--color-purple-100)}
.scope.mixed{color:#9a6700;background:rgba(255,193,7,.16)}
.cov{display:grid;grid-template-columns:1fr 1fr;gap:16px}
@media(max-width:760px){.cov{grid-template-columns:1fr}}
.cov .box{border:var(--border-hairline);border-radius:var(--radius-md);padding:16px 18px}
.cov .box.ok{background:rgba(0,200,187,.06)}
.cov .box.gap{background:var(--color-blue-50)}
.cov h4{margin:0 0 8px;font:var(--type-h6);color:var(--color-ink-700)}
.cov li{color:var(--color-ink-500);font:var(--type-body-sm);line-height:1.6;margin:4px 0 4px 16px}
.matched{margin-top:10px;display:grid;gap:4px}
.matched div{color:var(--color-ink-500);font:var(--type-body-xs);line-height:1.5}
.matched b{color:var(--color-ink-700);font-weight:500;font-family:var(--font-mono);text-transform:uppercase;letter-spacing:.04em;font-size:10px}
.item.warn{border:1px solid var(--color-blue-300);border-radius:var(--radius-md);background:var(--color-blue-50);padding:14px 16px;margin-top:12px}
.item.warn strong{color:var(--color-blue-700);font:var(--type-h6);display:block;margin-bottom:4px}
.item.warn div{color:var(--color-ink-500);font:var(--type-body-sm);line-height:1.5}
details{border:var(--border-hairline);border-radius:var(--radius-md);padding:10px 14px;margin-top:10px}
summary{cursor:pointer;color:var(--color-ink-700);font:var(--type-h6)}
</style>
</head>
<body data-chaos-stack>
<div class="topbar"><div class="wrap">__BRAND_TOPBAR__<span class="crumb">Stack<span class="sep">&rsaquo;</span><b>inventory</b></span><span class="sp"></span><span class="pilltag">Tech stack</span></div></div>

<header class="ov">
  <div class="wrap">
    <div class="eyebrow">Tech stack</div>
    <h1 id="title">Tech stack</h1>
    <div id="overview"></div>
    <div class="sub" id="subtitle"></div>
  </div>
</header>

<main>
  <div class="wrap">
    <section class="panel"><div id="stats" class="stats"></div><div id="langs" style="margin-top:14px"></div></section>
    <section class="panel" data-stack-ecosystems><h2>Dependencies by ecosystem</h2><div class="muted" style="margin-bottom:10px">Manifest-declared packages only (what package.json / Cargo.toml name) &mdash; widest-declared first; imports inside source files are graph edges, not stack entries.</div><div id="ecosystems"></div></section>
    <section class="panel" data-stack-infra><h2>Deployment &amp; infrastructure</h2><div id="infrastructure"></div></section>
    <section class="panel" data-stack-scripts><h2>Scripts</h2><div id="scripts"></div></section>
    <section class="panel" data-stack-configs><h2>Configuration files</h2><div id="configs"></div></section>
    <section class="panel" data-stack-coverage><h2>Coverage</h2><div class="muted" style="margin-bottom:10px">What this inventory reads from the index &mdash; and what the extractor does not persist yet (read those files directly if you need them).</div><div class="cov" id="coverage"></div></section>
    <section class="panel" data-stack-provenance><h2>How this was generated</h2><div id="provenance"></div></section>
    <section class="panel"><h2>Warnings</h2><div id="warnings"></div></section>
  </div>
</main>

<footer><div class="wrap">__BRAND_FOOTER__<span class="sp"></span><span class="meta">generated by Chaos Substrate</span></div></footer>

<script type="application/json" id="chaos-stack-manifest">__DATA__</script>
<script>
(function(){
var D=JSON.parse(document.getElementById("chaos-stack-manifest").textContent);
function esc(v){return String(v==null?"":v).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;").replace(/"/g,"&quot;").replace(/'/g,"&#039;");}
document.getElementById("title").textContent=D.title||"Tech stack";
document.getElementById("overview").textContent=D.overview||"";
document.getElementById("subtitle").textContent=D.subtitle||"";
var T=D.totals||{};
var stat=[[T.packages||0,"packages"],[T.manifests||0,"manifests"],[T.stacks||0,"cdk stacks"],[T.deployment_resources||0,"deploy resources"],[T.scripts||0,"scripts"]];
document.getElementById("stats").innerHTML=stat.map(function(s){return '<div class="stat"><b>'+s[0]+'</b><span>'+s[1]+'</span></div>';}).join("");
document.getElementById("langs").innerHTML=(D.languages||[]).map(function(l){return '<span class="lang">'+esc(l.name)+' &middot; '+l.count+'</span>';}).join("");
var eco=document.getElementById("ecosystems");
(D.ecosystems||[]).forEach(function(e){
  var manifests='<details><summary>'+e.manifests.length+' manifest(s)</summary><table><tr><th>Manifest</th><th class="num">deps</th><th class="num">runtime</th><th class="num">dev</th></tr>'+
    e.manifests.map(function(m){return '<tr><td class="mono">'+esc(m.path)+'</td><td class="num">'+m.dependencies+'</td><td class="num">'+m.runtime+'</td><td class="num">'+m.dev+'</td></tr>';}).join("")+'</table></details>';
  var pkgs='<table><tr><th>Package</th><th>Versions</th><th>Scope</th><th class="num">declared in</th></tr>'+
    e.packages.map(function(p){return '<tr><td class="mono">'+esc(p.name)+'</td><td class="mono">'+esc((p.versions||[]).join(", "))+'</td><td><span class="scope '+esc(p.scope)+'">'+esc(p.scope)+'</span></td><td class="num">'+p.manifests+' manifest(s)</td></tr>';}).join("")+'</table>';
  var sec=document.createElement("div");
  sec.innerHTML='<h3>'+esc(e.ecosystem)+' &mdash; '+e.packages.length+' package(s) ('+e.runtime_packages+' runtime, '+e.dev_packages+' dev; '+e.declared_total+' declaration(s))</h3>'+manifests+pkgs;
  eco.appendChild(sec);
});
if(!eco.children.length)eco.innerHTML='<div class="muted">No manifest-declared dependencies in the index.</div>';
var infra=document.getElementById("infrastructure");
(D.infrastructure||[]).forEach(function(t){
  var apps=(t.apps||[]).map(function(a){return '<tr><td class="mono">'+esc(a.file)+'</td><td class="mono">'+esc(a.command)+'</td></tr>';}).join("");
  var stacks=(t.stacks||[]).map(function(s){return '<tr><td class="mono">'+esc(s.name)+'</td><td class="mono">'+esc(s.file)+(s.line?':'+s.line:'')+'</td></tr>';}).join("");
  var svcs=(t.services||[]).map(function(s){return '<tr><td class="mono">'+esc(s.service)+'</td><td class="num">'+s.count+'</td><td class="mono">'+esc((s.examples||[]).join(", "))+'</td></tr>';}).join("");
  var sec=document.createElement("div");
  sec.innerHTML='<h3>'+esc(t.technology)+' &mdash; '+(t.stacks||[]).length+' stack(s), '+t.resources_total+' resource(s)</h3>'+
    (apps?'<h4 class="muted">App entrypoints</h4><table><tr><th>Config</th><th>Command</th></tr>'+apps+'</table>':'')+
    (stacks?'<details open><summary>Stacks ('+(t.stacks||[]).length+')</summary><table><tr><th>Stack</th><th>Defined at</th></tr>'+stacks+'</table></details>':'')+
    (svcs?'<details open><summary>Resources by service</summary><table><tr><th>Service</th><th class="num">count</th><th>Examples</th></tr>'+svcs+'</table></details>':'');
  infra.appendChild(sec);
});
if(!infra.children.length)infra.innerHTML='<div class="muted">No deployment resources in the index.</div>';
var sc=document.getElementById("scripts");
if((D.scripts||[]).length){
  sc.innerHTML='<table><tr><th>Script</th><th class="num">declared in</th><th>Example command</th></tr>'+
    D.scripts.map(function(s){return '<tr><td class="mono">'+esc(s.name)+'</td><td class="num">'+s.manifests+' manifest(s)</td><td class="mono">'+esc(s.example)+'</td></tr>';}).join("")+'</table>'+
    '<div class="muted" style="margin-top:8px">'+ (D.scripts_total||0)+' script declaration(s) total.</div>';
}else{sc.innerHTML='<div class="muted">No npm scripts in the index.</div>';}
var cf=document.getElementById("configs");
cf.innerHTML=(D.config_files||[]).length?(D.config_files.map(function(c){return '<span class="lang">'+esc(c)+'</span>';}).join("")):'<div class="muted">No JS/TS config files in the index.</div>';
var cov=document.getElementById("coverage");var C=D.coverage||{};
cov.innerHTML='<div class="box ok"><h4>Read from the index</h4><ul>'+(C.included||[]).map(function(x){return '<li>'+esc(x)+'</li>';}).join("")+'</ul></div>'+
  '<div class="box gap"><h4>Not indexed yet</h4><ul>'+(C.not_indexed||[]).map(function(x){return '<li>'+esc(x)+'</li>';}).join("")+'</ul></div>';
var prov=document.getElementById("provenance");
(D.provenance||[]).forEach(function(c){var el=document.createElement("div");el.className="matched";el.innerHTML='<div><b>'+esc(c.source)+'</b> '+esc(c.method)+'</div><div class="muted">'+esc(c.detail)+(c.locator?' &middot; '+esc(c.locator):'')+'</div>';prov.appendChild(el);});
if(!prov.children.length)prov.innerHTML='<div class="muted">No breadcrumbs recorded.</div>';
var w=document.getElementById("warnings");
(D.warnings||[]).forEach(function(x){var el=document.createElement("div");el.className="item warn";el.innerHTML='<strong>Note</strong><div>'+esc(x)+'</div>';w.appendChild(el);});
if(!w.children.length)w.innerHTML='<div class="muted">No warnings.</div>';
})();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(
        eco: &str,
        name: &str,
        version: &str,
        section: &str,
        manifest: &str,
    ) -> StackDependencyRow {
        StackDependencyRow {
            ecosystem: eco.into(),
            name: name.into(),
            version: version.into(),
            section: section.into(),
            manifest: manifest.into(),
        }
    }

    #[test]
    fn ecosystems_group_rank_and_scope() {
        let rows = vec![
            dep(
                "npm",
                "react",
                "^18.2.0",
                "dependencies",
                "packages/a/package.json",
            ),
            dep(
                "npm",
                "react",
                "^18.2.0",
                "dependencies",
                "packages/b/package.json",
            ),
            dep(
                "npm",
                "typescript",
                "^5.4.0",
                "devDependencies",
                "packages/a/package.json",
            ),
            dep(
                "npm",
                "zod",
                "^3.23.0",
                "dependencies",
                "packages/a/package.json",
            ),
            dep("cargo", "serde", "", "dependencies", "Cargo.toml"),
        ];
        let out = aggregate_ecosystems(&rows);
        assert_eq!(out.len(), 2);
        // npm is bigger, so it comes first.
        assert_eq!(out[0].ecosystem, "npm");
        // react is declared in 2 manifests → widest-declared, ranked first.
        assert_eq!(out[0].packages[0].name, "react");
        assert_eq!(out[0].packages[0].manifests, 2);
        assert_eq!(out[0].packages[0].scope, "runtime");
        let ts = out[0]
            .packages
            .iter()
            .find(|p| p.name == "typescript")
            .unwrap();
        assert_eq!(ts.scope, "dev");
        assert_eq!(out[0].runtime_packages, 2);
        assert_eq!(out[0].dev_packages, 1);
        assert_eq!(out[0].declared_total, 4);
        assert_eq!(out[0].manifests.len(), 2);
        // Cargo dependency without a version stays version-less, runtime scope.
        assert_eq!(out[1].ecosystem, "cargo");
        assert!(out[1].packages[0].versions.is_empty());
    }

    #[test]
    fn mixed_scope_when_runtime_and_dev() {
        let rows = vec![
            dep("npm", "esbuild", "0.21.0", "dependencies", "a/package.json"),
            dep(
                "npm",
                "esbuild",
                "0.21.0",
                "devDependencies",
                "b/package.json",
            ),
        ];
        let out = aggregate_ecosystems(&rows);
        assert_eq!(out[0].packages[0].scope, "mixed");
    }

    #[test]
    fn scripts_group_by_name_with_deterministic_example() {
        let rows = vec![
            StackScriptRow {
                name: "build".into(),
                command: "tsc -b".into(),
                manifest: "a/package.json".into(),
            },
            StackScriptRow {
                name: "build".into(),
                command: "vite build".into(),
                manifest: "b/package.json".into(),
            },
            StackScriptRow {
                name: "test".into(),
                command: "vitest".into(),
                manifest: "a/package.json".into(),
            },
        ];
        let out = aggregate_scripts(&rows);
        assert_eq!(out[0].name, "build");
        assert_eq!(out[0].manifests, 2);
        assert_eq!(out[0].example, "tsc -b");
        assert_eq!(out[1].name, "test");
    }

    #[test]
    fn infrastructure_splits_apps_stacks_services() {
        let row = |name: &str, kind: &str, service: &str, app: &str| StackDeploymentRow {
            name: name.into(),
            technology: "aws_cdk".into(),
            resource_kind: kind.into(),
            service: service.into(),
            app: app.into(),
            file: "infra/x.ts".into(),
            line_start: Some(10),
        };
        let rows = vec![
            row("AWS CDK app", "app", "", "npx ts-node bin/app.ts"),
            row("ApiStack", "stack", "", ""),
            row("lambda.Function Handler", "construct", "lambda", ""),
            row("lambda.Function Worker", "construct", "lambda", ""),
            row("dynamodb.Table Events", "construct", "dynamodb", ""),
        ];
        let out = aggregate_infrastructure(&rows);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].apps.len(), 1);
        assert_eq!(out[0].apps[0].command, "npx ts-node bin/app.ts");
        assert_eq!(out[0].stacks.len(), 1);
        assert_eq!(out[0].resources_total, 3);
        assert_eq!(out[0].services[0].service, "lambda");
        assert_eq!(out[0].services[0].count, 2);
    }

    #[test]
    fn overview_is_deterministic_and_grounded() {
        let ecosystems = aggregate_ecosystems(&[dep(
            "npm",
            "react",
            "^18.2.0",
            "dependencies",
            "package.json",
        )]);
        let langs = json!([{"name": "typescript", "count": 120}]);
        let totals = StackTotals {
            packages: 1,
            manifests: 1,
            scripts: 3,
            ..Default::default()
        };
        let a = compose_overview("molecule_core", &langs, &ecosystems, &[], &totals);
        let b = compose_overview("molecule_core", &langs, &ecosystems, &[], &totals);
        assert_eq!(a, b);
        assert!(a.contains("typescript"));
        assert!(a.contains("1 distinct npm package(s)"));
        assert!(a.contains("3 npm script(s)"));
    }

    #[test]
    fn compact_return_caps_and_lifts_omissions() {
        let rows: Vec<StackDependencyRow> = (0..40)
            .map(|i| {
                dep(
                    "npm",
                    &format!("pkg{i:02}"),
                    "1.0.0",
                    "dependencies",
                    "package.json",
                )
            })
            .collect();
        let manifest = StackManifest {
            schema_version: "stack-inventory-1".into(),
            repo_name: "r".into(),
            title: "t".into(),
            subtitle: "s".into(),
            overview: "o".into(),
            languages: json!([]),
            totals: StackTotals::default(),
            ecosystems: aggregate_ecosystems(&rows),
            scripts: Vec::new(),
            scripts_total: 0,
            infrastructure: Vec::new(),
            config_files: Vec::new(),
            coverage: Coverage::current(),
            provenance: Vec::new(),
            warnings: Vec::new(),
        };
        let out = compact_return(&manifest, Path::new("/tmp/stack.html"), uuid::Uuid::nil());
        let eco = &out["ecosystems"][0];
        assert_eq!(
            eco["top_packages"].as_array().unwrap().len(),
            MAX_COMPACT_PACKAGES
        );
        assert_eq!(eco["packages_omitted"], json!(10));
        assert_eq!(eco["packages_total"], json!(40));
        // Uniform rows — no mixed-shape sentinel objects inside the array.
        for row in eco["top_packages"].as_array().unwrap() {
            assert!(row.get("name").is_some());
        }
    }

    #[test]
    fn stack_html_renders_with_embedded_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stack.html");
        let manifest = StackManifest {
            schema_version: "stack-inventory-1".into(),
            repo_name: "demo".into(),
            title: "demo — tech stack".into(),
            subtitle: "s".into(),
            overview: "o".into(),
            languages: json!([{"name": "rust", "count": 3}]),
            totals: StackTotals::default(),
            ecosystems: Vec::new(),
            scripts: Vec::new(),
            scripts_total: 0,
            infrastructure: Vec::new(),
            config_files: Vec::new(),
            coverage: Coverage::current(),
            provenance: vec![Breadcrumb::new(
                source::POSTGRES,
                "stack_dependencies",
                "0 rows",
            )],
            warnings: Vec::new(),
        };
        write_stack_html(&path, &manifest).unwrap();
        let html = fs::read_to_string(&path).unwrap();
        assert!(html.contains("chaos-stack-manifest"));
        assert!(html.contains("data-chaos-stack"));
        assert!(html.contains("Not indexed yet") || html.contains("not_indexed"));
    }
}
