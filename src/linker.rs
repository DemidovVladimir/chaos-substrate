//! P6 cross-repository linkers — detect feature→feature links BETWEEN the
//! member repositories of a project.
//!
//! Each linker runs entirely off the persisted index (plus manifest reads from
//! the repo roots already recorded in Postgres) and attaches its links at L1
//! (community ↔ community), never L0: cross-repo references are name/path
//! matches, so a feature-level link is the honest precision. Direction is
//! consumer → provider (the client feature that calls a route points at the
//! backend feature that serves it).
//!
//! Three linkers, in decreasing confidence:
//!   * **package_dep** — repo B imports a package whose `name` is published by
//!     a manifest in repo A (`package.json` / `Cargo.toml`).
//!   * **abi** — a non-Solidity chunk references a contract / interface /
//!     library defined in another repo's Solidity sources (word-boundary,
//!     CamelCase-gated).
//!   * **http_route** — a fetch/axios/client call path in one repo matches a
//!     route registered in another (normalized: params → `*`).
//!
//! Everything is deterministic: anchors and matches are collected in sorted
//! order and link ids are UUIDv5 over `(project, kind, src, dst)`. Each link
//! carries evidence (matched names/paths, example files) and provenance
//! breadcrumbs, like every other Chaos artifact.

use crate::{
    models::{CrossRepoLink, ProjectRepo},
    provenance::{source, Breadcrumb},
    storage::Storage,
};
use anyhow::Result;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use uuid::Uuid;

/// Link kinds — the `cross_repo_links.kind` vocabulary.
pub mod kind {
    pub const PACKAGE_DEP: &str = "package_dep";
    pub const ABI: &str = "abi";
    pub const HTTP_ROUTE: &str = "http_route";
}

/// Per-kind confidence: package imports are exact-name, ABI references are
/// word-boundary CamelCase, route paths are normalized string matches.
fn kind_confidence(k: &str) -> f64 {
    match k {
        kind::PACKAGE_DEP => 0.9,
        kind::ABI => 0.8,
        _ => 0.65,
    }
}

/// Max foreign anchor names sent into one consumer chunk scan.
const MAX_ANCHOR_NAMES: usize = 100;
/// Max chunks pulled per repo scan (SQL prefilter cap).
const SCAN_LIMIT: i64 = 4000;
/// Max matched values / example files recorded per link's evidence.
const MAX_EVIDENCE: usize = 6;

pub struct LinkOutcome {
    pub links: Vec<CrossRepoLink>,
    pub provenance: Vec<Breadcrumb>,
    pub warnings: Vec<String>,
}

/// One repo's outward-facing anchors (what other repos might reference).
struct RepoFacets {
    /// Published package names → manifest directory ("" = repo root).
    packages: Vec<(String, String)>,
    /// Solidity contract/interface/library names → defining file.
    contracts: Vec<(String, String)>,
    /// Normalized route paths registered here → registering file.
    routes: Vec<(String, String)>,
    /// Normalized call paths used here → calling file (the consumer side).
    calls: Vec<(String, String)>,
}

/// A raw cross-repo match before community resolution.
struct RawMatch {
    kind: &'static str,
    /// The matched name or path.
    value: String,
    consumer_repo: usize,
    consumer_file: String,
    provider_repo: usize,
    /// Provider attachment: a concrete file, or a package directory.
    provider_anchor: ProviderAnchor,
}

enum ProviderAnchor {
    File(String),
    PackageDir(String),
}

/// Detect all cross-repo links for a project. Pure read (no embedder); the
/// caller persists the outcome via `Storage::replace_project_links`.
pub async fn detect_project_links(
    storage: &Storage,
    project_id: Uuid,
    members: &[ProjectRepo],
) -> Result<LinkOutcome> {
    let mut provenance: Vec<Breadcrumb> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    if members.len() < 2 {
        warnings.push("project has fewer than two repositories — nothing to link".into());
        return Ok(LinkOutcome {
            links: Vec::new(),
            provenance,
            warnings,
        });
    }

    // 1. Facets per member repo.
    let mut facets: Vec<RepoFacets> = Vec::with_capacity(members.len());
    for member in members {
        let f = collect_facets(storage, member, &mut provenance, &mut warnings).await?;
        facets.push(f);
    }

    // 2. Raw matches per consumer repo against every other repo's anchors.
    let mut raw: Vec<RawMatch> = Vec::new();
    for (ci, consumer) in members.iter().enumerate() {
        // package_dep: foreign package names imported in this repo's chunks.
        let foreign_packages: Vec<(String, String, usize)> = facets
            .iter()
            .enumerate()
            .filter(|(pi, _)| *pi != ci)
            .flat_map(|(pi, f)| {
                f.packages
                    .iter()
                    .map(move |(name, dir)| (name.clone(), dir.clone(), pi))
            })
            .collect();
        let foreign_packages = capped_anchors(foreign_packages, &mut warnings, "package");
        if !foreign_packages.is_empty() {
            let patterns: Vec<String> = foreign_packages
                .iter()
                .flat_map(|(name, _, _)| {
                    let crate_name = name.replace('-', "_");
                    vec![
                        format!("%{}%", escape_like(name)),
                        format!("%use {}%", escape_like(&crate_name)),
                        format!("%extern crate {}%", escape_like(&crate_name)),
                    ]
                })
                .collect();
            let chunks = storage
                .scan_chunks(consumer.repo.id, &patterns, SCAN_LIMIT)
                .await?;
            for (file, content) in &chunks {
                for (name, dir, pi) in &foreign_packages {
                    if js_imports_package(content, name) || rust_uses_crate(content, name) {
                        raw.push(RawMatch {
                            kind: kind::PACKAGE_DEP,
                            value: name.clone(),
                            consumer_repo: ci,
                            consumer_file: file.clone(),
                            provider_repo: *pi,
                            provider_anchor: ProviderAnchor::PackageDir(dir.clone()),
                        });
                    }
                }
            }
        }

        // abi: foreign contract names referenced in this repo's non-Solidity chunks.
        let foreign_contracts: Vec<(String, String, usize)> = facets
            .iter()
            .enumerate()
            .filter(|(pi, _)| *pi != ci)
            .flat_map(|(pi, f)| {
                f.contracts
                    .iter()
                    .map(move |(name, file)| (name.clone(), file.clone(), pi))
            })
            .collect();
        let foreign_contracts = capped_anchors(foreign_contracts, &mut warnings, "contract");
        if !foreign_contracts.is_empty() {
            let patterns: Vec<String> = foreign_contracts
                .iter()
                .map(|(name, _, _)| format!("%{}%", escape_like(name)))
                .collect();
            let chunks = storage
                .scan_chunks(consumer.repo.id, &patterns, SCAN_LIMIT)
                .await?;
            for (file, content) in &chunks {
                if file.ends_with(".sol") {
                    continue;
                }
                for (name, provider_file, pi) in &foreign_contracts {
                    if references_symbol(content, name) {
                        raw.push(RawMatch {
                            kind: kind::ABI,
                            value: name.clone(),
                            consumer_repo: ci,
                            consumer_file: file.clone(),
                            provider_repo: *pi,
                            provider_anchor: ProviderAnchor::File(provider_file.clone()),
                        });
                    }
                }
            }
        }

        // http_route: this repo's call paths vs other repos' registered routes.
        for (call_path, call_file) in &facets[ci].calls {
            for (pi, provider) in facets.iter().enumerate() {
                if pi == ci {
                    continue;
                }
                for (route_path, route_file) in &provider.routes {
                    if routes_match(call_path, route_path) {
                        raw.push(RawMatch {
                            kind: kind::HTTP_ROUTE,
                            value: route_path.clone(),
                            consumer_repo: ci,
                            consumer_file: call_file.clone(),
                            provider_repo: pi,
                            provider_anchor: ProviderAnchor::File(route_file.clone()),
                        });
                    }
                }
            }
        }
    }

    provenance.push(Breadcrumb::new(
        source::REGEX,
        "cross_repo_match",
        format!(
            "lexically matched {} candidate reference(s) across {} repo pair(s)",
            raw.len(),
            members.len() * (members.len() - 1)
        ),
    ));

    // 3. Resolve files → dominant feature communities, per repo in one query.
    let mut paths_by_repo: Vec<BTreeSet<String>> = vec![BTreeSet::new(); members.len()];
    for m in &raw {
        if !m.consumer_file.is_empty() {
            paths_by_repo[m.consumer_repo].insert(m.consumer_file.clone());
        }
        if let ProviderAnchor::File(f) = &m.provider_anchor {
            if !f.is_empty() {
                paths_by_repo[m.provider_repo].insert(f.clone());
            }
        }
    }
    let mut community_by_file: Vec<BTreeMap<String, Uuid>> = Vec::with_capacity(members.len());
    for (i, member) in members.iter().enumerate() {
        let paths: Vec<String> = paths_by_repo[i].iter().cloned().collect();
        let map = storage
            .dominant_community_for_files(member.repo.id, &paths)
            .await?;
        community_by_file.push(map.into_iter().collect());
    }

    // Package dirs → the largest feature community under that directory
    // ("" = repo root → the repo's largest feature overall).
    let mut community_by_pkg_dir: Vec<BTreeMap<String, Option<Uuid>>> =
        vec![BTreeMap::new(); members.len()];
    for m in &raw {
        if let ProviderAnchor::PackageDir(dir) = &m.provider_anchor {
            let entry = community_by_pkg_dir[m.provider_repo].entry(dir.clone());
            if let std::collections::btree_map::Entry::Vacant(v) = entry {
                let repo_id = members[m.provider_repo].repo.id;
                let resolved = if dir.is_empty() {
                    storage
                        .community_labels(repo_id)
                        .await?
                        .first()
                        .map(|(id, _, _)| *id)
                } else {
                    let ids = storage
                        .communities_under_paths(repo_id, std::slice::from_ref(dir))
                        .await?;
                    let briefs = storage.load_community_briefs(repo_id, &ids).await?;
                    briefs.first().map(|b| b.id)
                };
                v.insert(resolved);
            }
        }
    }

    // 4. Aggregate raw matches into deduplicated feature→feature links.
    #[derive(Default)]
    struct Agg {
        values: BTreeSet<String>,
        examples: Vec<(String, String, String)>,
        total: usize,
    }
    let mut by_link: BTreeMap<(String, Uuid, Uuid, usize, usize), Agg> = BTreeMap::new();
    let mut unmapped = 0usize;
    for m in &raw {
        let Some(src) = community_by_file[m.consumer_repo]
            .get(&m.consumer_file)
            .copied()
        else {
            unmapped += 1;
            continue;
        };
        let dst = match &m.provider_anchor {
            ProviderAnchor::File(f) => community_by_file[m.provider_repo].get(f).copied(),
            ProviderAnchor::PackageDir(d) => community_by_pkg_dir[m.provider_repo]
                .get(d)
                .copied()
                .flatten(),
        };
        let Some(dst) = dst else {
            unmapped += 1;
            continue;
        };
        if src == dst {
            continue;
        }
        let agg = by_link
            .entry((
                m.kind.to_string(),
                src,
                dst,
                m.consumer_repo,
                m.provider_repo,
            ))
            .or_default();
        agg.total += 1;
        if agg.values.len() < MAX_EVIDENCE {
            agg.values.insert(m.value.clone());
        }
        if agg.examples.len() < MAX_EVIDENCE {
            let provider_file = match &m.provider_anchor {
                ProviderAnchor::File(f) => f.clone(),
                ProviderAnchor::PackageDir(d) => {
                    if d.is_empty() {
                        "<repo root>".to_string()
                    } else {
                        d.clone()
                    }
                }
            };
            agg.examples
                .push((m.value.clone(), m.consumer_file.clone(), provider_file));
        }
    }
    if unmapped > 0 {
        warnings.push(format!(
            "{unmapped} matched reference(s) could not be attached to a feature community on one side (file outside any feature) and were dropped"
        ));
    }

    let mut links: Vec<CrossRepoLink> = Vec::with_capacity(by_link.len());
    for ((k, src, dst, ci, pi), agg) in by_link {
        let id = Uuid::new_v5(
            &crate::community::COMMUNITY_NAMESPACE,
            format!("{project_id}:link:{k}:{src}:{dst}").as_bytes(),
        );
        let evidence = json!({
            "matched": agg.values.iter().collect::<Vec<_>>(),
            "examples": agg.examples.iter().map(|(v, cf, pf)| json!({
                "value": v,
                "consumer_file": format!("{}:{}", members[ci].alias, cf),
                "provider_file": format!("{}:{}", members[pi].alias, pf),
            })).collect::<Vec<_>>(),
            "total_matches": agg.total,
            "breadcrumbs": [Breadcrumb::new(
                source::REGEX,
                format!("link_{k}"),
                format!(
                    "{} reference(s) from {} to {} matched by the {k} linker",
                    agg.total, members[ci].alias, members[pi].alias
                ),
            )],
        });
        links.push(CrossRepoLink {
            id,
            source_repo_id: members[ci].repo.id,
            source_community_id: src,
            target_repo_id: members[pi].repo.id,
            target_community_id: dst,
            kind: k.clone(),
            evidence,
            confidence: kind_confidence(&k),
        });
    }
    links.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then(a.source_community_id.cmp(&b.source_community_id))
            .then(a.target_community_id.cmp(&b.target_community_id))
    });

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for l in &links {
        *counts.entry(l.kind.as_str()).or_insert(0) += 1;
    }
    provenance.push(Breadcrumb::new(
        source::GRAPH,
        "aggregate_links",
        format!(
            "aggregated into {} feature→feature link(s) ({})",
            links.len(),
            counts
                .iter()
                .map(|(k, c)| format!("{c} {k}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ));

    Ok(LinkOutcome {
        links,
        provenance,
        warnings,
    })
}

/// Collect one repo's anchors and consumer-side call paths.
async fn collect_facets(
    storage: &Storage,
    member: &ProjectRepo,
    provenance: &mut Vec<Breadcrumb>,
    warnings: &mut Vec<String>,
) -> Result<RepoFacets> {
    // Published package names: indexed manifest paths, contents read from disk.
    // Unreadable manifests (moved/absent checkout) are surfaced, not silently
    // skipped — a relink on degraded input would otherwise quietly REPLACE a
    // project's previously valid package links with an emptier set.
    let mut packages: Vec<(String, String)> = Vec::new();
    let mut unreadable = 0usize;
    for rel in storage.manifest_file_paths(member.repo.id).await? {
        let abs = Path::new(&member.repo.root_path).join(&rel);
        match std::fs::read_to_string(&abs) {
            Ok(content) => {
                if let Some(name) = manifest_package_name(&rel, &content) {
                    packages.push((name, dir_of(&rel).to_string()));
                }
            }
            Err(_) => unreadable += 1,
        }
    }
    if unreadable > 0 {
        warnings.push(format!(
            "{}: {unreadable} indexed manifest file(s) could not be read under {} — package links may be incomplete (checkout moved?)",
            member.alias, member.repo.root_path
        ));
    }
    packages.sort();
    packages.dedup();

    // Solidity ABI anchors (CamelCase-gated to avoid common-word noise).
    let contracts: Vec<(String, String)> = storage
        .solidity_contract_nodes(member.repo.id)
        .await?
        .into_iter()
        .filter(|(name, _, _)| contract_name_is_linkable(name))
        .map(|(name, _, file)| (name, file))
        .collect();

    // One chunk scan feeds both route anchors and consumer call paths.
    let scan_patterns: Vec<String> = [
        "%fetch(%",
        "%axios%",
        "%.route(%",
        "%.get(%",
        "%.post(%",
        "%.put(%",
        "%.patch(%",
        "%.delete(%",
        "%@controller(%",
        "%#[get(%",
        "%#[post(%",
        "%useswr%",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let chunks = storage
        .scan_chunks(member.repo.id, &scan_patterns, SCAN_LIMIT)
        .await?;

    let mut routes: Vec<(String, String)> = Vec::new();
    let mut calls: Vec<(String, String)> = Vec::new();
    for (file, content) in &chunks {
        for raw in extract_route_paths(content) {
            if let Some(norm) = normalize_route(&raw) {
                routes.push((norm, file.clone()));
            }
        }
        for raw in extract_call_paths(content) {
            if let Some(norm) = normalize_route(&raw) {
                calls.push((norm, file.clone()));
            }
        }
    }
    routes.sort();
    routes.dedup();
    calls.sort();
    calls.dedup();

    provenance.push(Breadcrumb::new(
        source::POSTGRES,
        "collect_facets",
        format!(
            "{}: {} package name(s), {} contract anchor(s), {} route(s), {} call path(s) from {} scanned chunk(s)",
            member.alias,
            packages.len(),
            contracts.len(),
            routes.len(),
            calls.len(),
            chunks.len()
        ),
    ));
    if chunks.len() as i64 >= SCAN_LIMIT {
        warnings.push(format!(
            "{}: chunk scan hit the {SCAN_LIMIT}-row cap — route/call detection may be incomplete",
            member.alias
        ));
    }

    Ok(RepoFacets {
        packages,
        contracts,
        routes,
        calls,
    })
}

/// Cap anchors deterministically (sorted by name), warning when dropped.
fn capped_anchors(
    mut anchors: Vec<(String, String, usize)>,
    warnings: &mut Vec<String>,
    what: &str,
) -> Vec<(String, String, usize)> {
    anchors.sort();
    anchors.dedup();
    if anchors.len() > MAX_ANCHOR_NAMES {
        warnings.push(format!(
            "capped {what} anchors at {MAX_ANCHOR_NAMES} (had {}); some links may be missed",
            anchors.len()
        ));
        anchors.truncate(MAX_ANCHOR_NAMES);
    }
    anchors
}

// ---- pure lexical helpers (unit-tested) ------------------------------------

/// The published `name` of a manifest file (`package.json` / `Cargo.toml`).
pub fn manifest_package_name(rel_path: &str, content: &str) -> Option<String> {
    let file = rel_path.rsplit('/').next().unwrap_or(rel_path);
    let name = match file {
        "package.json" => serde_json::from_str::<serde_json::Value>(content)
            .ok()?
            .get("name")?
            .as_str()?
            .to_string(),
        "Cargo.toml" => toml::from_str::<toml::Value>(content)
            .ok()?
            .get("package")?
            .get("name")?
            .as_str()?
            .to_string(),
        _ => return None,
    };
    let name = name.trim().to_string();
    (name.len() >= 3).then_some(name)
}

/// Contract names worth cross-repo matching: capitalized and either long or
/// CamelCase, so `Token`-like common words don't fire on prose.
pub fn contract_name_is_linkable(name: &str) -> bool {
    name.len() >= 5
        && name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && (name.len() >= 8 || name[1..].chars().any(|c| c.is_ascii_uppercase()))
}

/// True when `content` imports the JS/TS package `name` (`from`/`require`/
/// `import` context, exact name or a `name/...` subpath).
pub fn js_imports_package(content: &str, name: &str) -> bool {
    for q in ['\'', '"'] {
        for needle in [format!("{q}{name}{q}"), format!("{q}{name}/")] {
            let mut from = 0;
            while let Some(p) = content[from..].find(&needle) {
                let at = from + p;
                if import_context_precedes(&content[..at]) {
                    return true;
                }
                from = at + needle.len();
            }
        }
    }
    false
}

/// True when the text immediately before a string literal is an import site:
/// `from`, `import`, `import(`, or `require(` as the LAST token — not merely
/// appearing somewhere in a nearby window (a comment like
/// `// important: 'pkg' is deprecated` must not count as an import).
fn import_context_precedes(before: &str) -> bool {
    let trimmed = before.trim_end();
    if trimmed.ends_with("require(") || trimmed.ends_with("import(") {
        return true;
    }
    for kw in ["from", "import"] {
        if let Some(prior) = trimmed.strip_suffix(kw) {
            let boundary = prior
                .chars()
                .next_back()
                .map(|c| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(true);
            if boundary {
                return true;
            }
        }
    }
    false
}

/// True when `content` uses the Rust crate published as `name` (dashes map to
/// underscores in `use` paths).
pub fn rust_uses_crate(content: &str, name: &str) -> bool {
    let norm = name.replace('-', "_");
    content.contains(&format!("use {norm}::"))
        || content.contains(&format!("use {norm};"))
        || content.contains(&format!("extern crate {norm}"))
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Word-boundary occurrence of an ASCII identifier.
pub fn references_symbol(content: &str, name: &str) -> bool {
    if name.is_empty() || !name.is_ascii() {
        return false;
    }
    let bytes = content.as_bytes();
    let mut from = 0;
    while let Some(p) = content[from..].find(name) {
        let start = from + p;
        let end = start + name.len();
        let before_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_word_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

/// String literals that directly follow any of `markers` (case-insensitive),
/// e.g. the `'/users'` in `router.get('/users', …)`.
fn literals_after_markers(content: &str, markers: &[&str]) -> Vec<String> {
    let lower = content.to_ascii_lowercase();
    let bytes = content.as_bytes();
    let mut out: Vec<String> = Vec::new();
    for marker in markers {
        let mut from = 0;
        while let Some(p) = lower[from..].find(marker) {
            let mut i = from + p + marker.len();
            from = i;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }
            let q = bytes[i];
            if q == b'\'' || q == b'"' || q == b'`' {
                let lit_start = i + 1;
                if let Some(rel) = content[lit_start..].find(q as char) {
                    if rel <= 200 {
                        out.push(content[lit_start..lit_start + rel].to_string());
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Route paths *registered* in this content (server side). Plain `.get(` is
/// deliberately NOT a provider marker — only framework-shaped registrations —
/// so an axios client instance doesn't masquerade as a server.
pub fn extract_route_paths(content: &str) -> Vec<String> {
    const PROVIDER_MARKERS: &[&str] = &[
        "app.get(",
        "app.post(",
        "app.put(",
        "app.patch(",
        "app.delete(",
        "app.all(",
        "app.use(",
        "router.get(",
        "router.post(",
        "router.put(",
        "router.patch(",
        "router.delete(",
        "router.all(",
        "fastify.get(",
        "fastify.post(",
        "fastify.put(",
        "fastify.delete(",
        ".route(",
        "@get(",
        "@post(",
        "@put(",
        "@patch(",
        "@delete(",
        "@controller(",
        "#[get(",
        "#[post(",
        "#[put(",
        "#[patch(",
        "#[delete(",
    ];
    literals_after_markers(content, PROVIDER_MARKERS)
        .into_iter()
        .map(|l| {
            if l.starts_with('/') {
                l
            } else {
                format!("/{l}")
            }
        })
        .collect()
}

/// Route paths *called* in this content (client side). Includes generic
/// `.get(`-style instance clients; cross-repo matching keeps this honest.
pub fn extract_call_paths(content: &str) -> Vec<String> {
    const CONSUMER_MARKERS: &[&str] = &[
        "fetch(",
        "axios(",
        "axios.get(",
        "axios.post(",
        "axios.put(",
        "axios.patch(",
        "axios.delete(",
        "axios.request(",
        "useswr(",
        ".get(",
        ".post(",
        ".put(",
        ".patch(",
        ".delete(",
        "ky.get(",
        "ky.post(",
        "got(",
    ];
    literals_after_markers(content, CONSUMER_MARKERS)
        .into_iter()
        .filter(|l| l.starts_with('/'))
        .collect()
}

/// File extensions that mark a static asset path, not an API route.
const STATIC_EXTS: &[&str] = &[
    "js", "mjs", "cjs", "css", "map", "png", "jpg", "jpeg", "gif", "svg", "ico", "webp", "woff",
    "woff2", "ttf", "html", "json", "txt", "xml", "pdf",
];

/// Normalize a route/call path for matching: lowercase, strip query/fragment
/// and trailing slash, parameter segments (`:id`, `{id}`, `[id]`, `${id}`,
/// `<id>`, `*`) become `*`. Returns None for non-routes (static assets,
/// protocol-relative URLs, all-wildcard paths).
pub fn normalize_route(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    if let Some(i) = s.find(['?', '#']) {
        s = &s[..i];
    }
    if !s.starts_with('/') || s.starts_with("//") || s.contains(' ') || s.contains("..") {
        return None;
    }
    let mut segs: Vec<String> = Vec::new();
    for seg in s.split('/').filter(|x| !x.is_empty()) {
        let wild = seg.starts_with(':')
            || seg.starts_with('{')
            || seg.starts_with('<')
            || seg.starts_with('[')
            || seg.contains("${")
            || seg.contains('*');
        if wild {
            segs.push("*".to_string());
        } else {
            let lower = seg.to_ascii_lowercase();
            if let Some((_, ext)) = lower.rsplit_once('.') {
                if STATIC_EXTS.contains(&ext) {
                    return None;
                }
            }
            segs.push(lower);
        }
    }
    if segs.is_empty() || segs.iter().all(|x| x == "*") {
        return None;
    }
    Some(format!("/{}", segs.join("/")))
}

fn seg_eq(a: &str, b: &str) -> bool {
    a == "*" || b == "*" || a == b
}

/// Do two normalized paths denote the same route? Exact segment match, or the
/// shorter path is a (≥2-segment) suffix of the longer — which lets a Nest
/// method path match a fully-qualified client call. Requires at least one
/// concretely-equal segment so wildcards alone never link.
pub fn routes_match(a: &str, b: &str) -> bool {
    let sa: Vec<&str> = a.split('/').filter(|x| !x.is_empty()).collect();
    let sb: Vec<&str> = b.split('/').filter(|x| !x.is_empty()).collect();
    if sa.is_empty() || sb.is_empty() {
        return false;
    }
    let concrete = |xs: &[&str], ys: &[&str]| {
        xs.iter()
            .zip(ys.iter())
            .any(|(x, y)| x != &"*" && y != &"*" && x == y)
    };
    if sa.len() == sb.len() {
        let all = sa.iter().zip(sb.iter()).all(|(x, y)| seg_eq(x, y));
        if !all || !concrete(&sa, &sb) {
            return false;
        }
        // Single-segment routes must be meaningfully named to count.
        return sa.len() > 1 || sa[0].len() >= 4;
    }
    let (short, long) = if sa.len() < sb.len() {
        (sa, sb)
    } else {
        (sb, sa)
    };
    if short.len() < 2 {
        return false;
    }
    let tail = &long[long.len() - short.len()..];
    short.iter().zip(tail.iter()).all(|(x, y)| seg_eq(x, y)) && concrete(&short, tail)
}

fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// Escape SQL LIKE metacharacters (backslash is the default escape char).
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_names_parse_for_both_ecosystems() {
        assert_eq!(
            manifest_package_name("packages/ui/package.json", r#"{"name": "@org/ui"}"#),
            Some("@org/ui".to_string())
        );
        assert_eq!(
            manifest_package_name("Cargo.toml", "[package]\nname = \"chaos-substrate\"\n"),
            Some("chaos-substrate".to_string())
        );
        assert_eq!(manifest_package_name("package.json", "not json"), None);
        assert_eq!(
            manifest_package_name("tsconfig.json", r#"{"name": "x"}"#),
            None
        );
    }

    #[test]
    fn js_import_detection_requires_import_context() {
        assert!(js_imports_package("import { x } from '@org/ui'", "@org/ui"));
        assert!(js_imports_package(
            "const ui = require(\"@org/ui/button\")",
            "@org/ui"
        ));
        assert!(js_imports_package("import '@org/ui'", "@org/ui")); // side-effect import
        assert!(!js_imports_package("// '@org/ui' is great", "@org/ui"));
        // a keyword merely NEAR the literal is not an import site
        assert!(!js_imports_package(
            "// important: '@org/ui' is deprecated",
            "@org/ui"
        ));
        assert!(!js_imports_package(
            "// migrated from legacy '@org/ui'",
            "@org/ui"
        ));
    }

    #[test]
    fn rust_use_detection_normalizes_dashes() {
        assert!(rust_uses_crate(
            "use chaos_substrate::Storage;",
            "chaos-substrate"
        ));
        assert!(!rust_uses_crate("// chaos_substrate", "chaos-substrate"));
    }

    #[test]
    fn symbol_reference_is_word_bounded() {
        assert!(references_symbol("new AccessToken(addr)", "AccessToken"));
        assert!(!references_symbol("MyAccessTokenFactory", "AccessToken"));
    }

    #[test]
    fn contract_name_gate_drops_common_words() {
        assert!(contract_name_is_linkable("AccessControl"));
        assert!(contract_name_is_linkable("IPNFTRegistry"));
        assert!(!contract_name_is_linkable("Token")); // short, no inner caps
        assert!(!contract_name_is_linkable("token"));
    }

    #[test]
    fn route_extraction_finds_registrations_not_clients() {
        let server = r#"
            app.get('/api/users/:id', handler);
            router.post("/api/projects", create);
        "#;
        let routes = extract_route_paths(server);
        assert!(routes.contains(&"/api/users/:id".to_string()));
        assert!(routes.contains(&"/api/projects".to_string()));
        // an axios client call is NOT a provider route
        assert!(extract_route_paths("api.get('/api/users')").is_empty());
    }

    #[test]
    fn call_extraction_finds_fetches_and_clients() {
        let client = r#"
            const r = await fetch(`/api/users/${id}`);
            api.post('/api/projects', body);
        "#;
        let calls = extract_call_paths(client);
        assert!(calls.contains(&"/api/users/${id}".to_string()));
        assert!(calls.contains(&"/api/projects".to_string()));
    }

    #[test]
    fn normalization_wildcards_params_and_drops_assets() {
        assert_eq!(
            normalize_route("/api/users/:id"),
            Some("/api/users/*".to_string())
        );
        assert_eq!(
            normalize_route("/api/users/${id}?tab=1"),
            Some("/api/users/*".to_string())
        );
        assert_eq!(normalize_route("/static/app.css"), None);
        assert_eq!(normalize_route("//cdn.example.com/x"), None);
        assert_eq!(normalize_route("/:a/:b"), None);
    }

    #[test]
    fn route_matching_exact_suffix_and_guards() {
        assert!(routes_match("/api/users/*", "/api/users/*"));
        // client calls full path, Nest method registered the tail
        assert!(routes_match("/api/projects/list", "/projects/list"));
        // 1-segment overlap is not enough for a suffix match
        assert!(!routes_match("/api/users", "/users"));
        // wildcards alone never link
        assert!(!routes_match("/a/*", "/b/*"));
        // short single-segment routes don't count
        assert!(!routes_match("/up", "/up"));
        assert!(routes_match("/health", "/health"));
    }
}
