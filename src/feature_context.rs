use crate::embedding::Embedder;
use crate::export_util::{features_memory_dir, line_range, resolve_indexed_repo, safe_slug};
use crate::provenance::{source, Breadcrumb};
use crate::query::{query_feature_context_repo, QueryResponse};
use crate::storage::Storage;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

const MANIFEST_START: &str = r#"<script type="application/json" id="chaos-feature-manifest">"#;
const MANIFEST_END: &str = "</script>";

#[derive(Debug, Serialize)]
pub struct FeatureContextResponse {
    pub task: String,
    pub postgres: QueryResponse,
    pub features_dir: PathBuf,
    pub warnings: Vec<String>,
    pub feature_matches: Vec<FeatureMatch>,
    /// Breadcrumbs recording how this evidence was gathered (retrieval pipeline,
    /// manifests scanned). See [`feature_context_provenance`].
    #[serde(default)]
    pub provenance: Vec<Breadcrumb>,
}

#[derive(Debug, Serialize)]
pub struct FeatureMatch {
    pub page: PathBuf,
    pub feature: FeatureDefinition,
    pub title: String,
    pub subtitle: String,
    pub score: usize,
    pub claims: Vec<FeatureClaim>,
    pub modes: Vec<FeatureMode>,
    pub story: Vec<FeatureStoryStep>,
    pub matched_nodes: Vec<FeatureContextNode>,
    pub related_edges: Vec<FeatureContextEdge>,
    /// How this prior page matched the task, plus the page's own generation
    /// breadcrumbs (carried through from its embedded manifest).
    #[serde(default)]
    pub provenance: Vec<Breadcrumb>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeatureManifest {
    #[serde(default = "default_schema_version")]
    pub schema_version: String,
    #[serde(default)]
    pub feature: FeatureDefinition,
    pub title: String,
    pub subtitle: String,
    /// Plain-language answer to "what was this feature made for" — who uses it
    /// and what problem it solves. Rendered as the page's opening band, before
    /// any graph or evidence, so a reader gets the why first.
    #[serde(default)]
    pub purpose: String,
    /// Simple, concrete usage examples ("How you'd use it"). Each can point at
    /// the graph nodes it exercises so clicking the example highlights them.
    #[serde(default)]
    pub examples: Vec<FeatureExample>,
    #[serde(default)]
    pub claims: Vec<FeatureClaim>,
    #[serde(default)]
    pub modes: Vec<FeatureMode>,
    pub nodes: Vec<FeatureContextNode>,
    pub edges: Vec<FeatureContextEdge>,
    #[serde(default, deserialize_with = "deserialize_story_steps")]
    pub story: Vec<FeatureStoryStep>,
    /// Artifact-level breadcrumbs: how this page was generated (git diff,
    /// Postgres queries, file reads, AST/regex extraction, correlated manifests).
    /// Backward-compatible — older pages simply have none.
    #[serde(default)]
    pub provenance: Vec<Breadcrumb>,
    /// Previously generated feature pages this page correlates with (shared
    /// files/symbols), so a reader sees the related existing features.
    #[serde(default)]
    pub related_features: Vec<FeatureCorrelation>,
}

/// A simple, concrete usage example rendered in the page's "How you'd use it"
/// section — the actionable half of the "what was this made for" answer.
/// Everything beyond `title` is optional: prose, numbered steps, and/or a
/// short copy-pasteable snippet (a call, command, or payload).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FeatureExample {
    #[serde(default)]
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Numbered walkthrough steps in plain language.
    #[serde(default)]
    pub steps: Vec<String>,
    /// Short copy-pasteable snippet; shown verbatim in a code block.
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub language: String,
    /// Graph nodes this example exercises — clicking the example highlights them.
    #[serde(default)]
    pub node_ids: Vec<String>,
}

/// A previously generated feature page that overlaps the current change/feature
/// by shared files or symbols — the "this correlates with an existing feature"
/// signal. Produced by [`correlate_feature_manifests`].
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FeatureCorrelation {
    /// File name of the prior page (it lives under the same features directory).
    pub page: String,
    pub feature_id: String,
    pub title: String,
    pub domain: String,
    /// Files shared between this change/feature and the prior page.
    pub shared_files: Vec<String>,
    /// Symbols (node labels) shared with the prior page.
    pub shared_symbols: Vec<String>,
    /// Overlap strength: `shared_files * 2 + shared_symbols`.
    pub score: usize,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FeatureDefinition {
    pub id: String,
    pub title: String,
    pub domain: String,
    pub summary: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeatureClaim {
    pub id: String,
    pub title: String,
    pub body: String,
    pub confidence: f32,
    pub node_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeatureMode {
    pub id: String,
    pub title: String,
    pub node_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FeatureStoryStep {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub node_ids: Vec<String>,
    #[serde(default)]
    pub edge_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeatureContextNode {
    pub id: String,
    pub label: String,
    pub subtitle: String,
    pub group: String,
    pub file: String,
    pub lines: String,
    pub role: String,
    pub code: String,
    #[serde(default)]
    pub evidence: FeatureEvidence,
    #[serde(default)]
    pub confidence: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeatureContextEdge {
    pub source: String,
    pub target: String,
    pub label: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub evidence: FeatureEvidence,
    #[serde(default)]
    pub confidence: f32,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FeatureEvidence {
    pub source: String,
    pub method: String,
    pub notes: String,
}

fn default_schema_version() -> String {
    "legacy".to_string()
}

/// Max one-line evidence pointers in a compact feature-context return. The full
/// evidence — every hit, verbatim — stays in the written HTML.
const MAX_EVIDENCE_LINES: usize = 24;
/// How many of the top-ranked (strongest) evidence hits get their verbatim body
/// inlined in the compact return. Reading the decisive code is what stops an
/// agent from authoring a behavioral claim off a symbol name alone. Set to 5 (not
/// 1-2) deliberately: the single line that settles a behavioral question often
/// sits in the 3rd-5th hit, not the top one, and re-ranking shifts with query
/// phrasing — a slightly wider window is cheap insurance against missing it.
/// Every hit's full body still lives in the HTML regardless.
const INLINE_BODY_HITS: usize = 5;
/// Char cap per inlined body — enough to reach the decisive lines (for a UI
/// component the settling copy is often deep in the JSX `return`, so too tight a
/// cap truncates exactly the line that matters) without flooding context. Longer
/// bodies are cut on a char boundary and flagged; the untruncated body is in the
/// `chaos-feature-context-data` block.
const MAX_INLINE_BODY_CHARS: usize = 2400;
/// Max shared symbols listed per correlated prior page in the compact return.
const MAX_RELATED_SYMBOLS: usize = 8;
/// Weak-overlap floor for a correlated prior page to appear in the compact
/// return. `score_manifest` scores a page as `feature_text_matches * 3 +
/// sum(node_text_matches)`, so a single stray node-token match scores 1–2 while
/// any feature-text match (or the token recurring across ≥3 nodes) clears 3. The
/// floor drops the weak-overlap tail from the agent's context; the full
/// correlated set is still written to the HTML.
const MIN_RELATED_PAGE_SCORE: usize = 3;

const COMPACT_NEXT: &str = "This evidence is for a feature deep-dive. The strongest hits' bodies are inlined above as `code_excerpt`; the FULL verbatim code for every hit is in the written output_html under <script id=\"chaos-feature-context-data\">. Before writing ANY behavioral or factual claim, READ the actual body — the inlined code_excerpt, the HTML data block, or the source file — never infer behavior from a symbol name, file path, or line range alone. Re-running retrieval is unnecessary, but reading source to confirm a claim is encouraged, not discouraged. Then PERSIST your explanation as the interactive page: chaos_write_feature_website {repo, slug, title, manifest} (manifest only, omit html).";

/// Options for [`run`], shared by the CLI and MCP surfaces. Zero/None fall back to
/// the same defaults the tool has always used (limit 10, feature_limit 3,
/// nodes_per_feature 8; HTML to `docs/features_memory/<slug>-context.html`).
#[derive(Debug, Default, Clone)]
pub struct FeatureContextOptions {
    pub features_dir: Option<PathBuf>,
    pub output_html: Option<PathBuf>,
    pub limit: i64,
    pub feature_limit: usize,
    pub nodes_per_feature: usize,
}

/// Compact feature-context return. Mirrors `chaos_impact`'s `ImpactSummary`: the
/// heavy evidence (every hit's full content, every correlated node's verbatim
/// code) lives in the written HTML, while this payload carries ranked pointers —
/// PLUS the top hits' bodies inlined as `code_excerpt` — so the decisive code is
/// in the agent's context without flooding it. The agent pulls any remaining
/// verbatim code from the embedded `chaos-feature-context-data` block.
#[derive(Debug, Serialize)]
pub struct CompactFeatureContext {
    pub status: &'static str,
    pub repo_id: Uuid,
    pub task: String,
    /// Always written now — holds the full evidence + the extractable JSON block.
    pub output_html: PathBuf,
    pub counts: FeatureContextCounts,
    /// One line per deduped, ranked retrieval hit. The top hits carry an inlined
    /// `code_excerpt` (bounded); the rest are pointers. Full code is in the HTML.
    pub evidence: Vec<EvidenceLine>,
    /// Compact summaries of correlated prior feature pages (relevance-floored).
    pub related_pages: Vec<RelatedPage>,
    pub warnings: Vec<String>,
    pub provenance: Vec<Breadcrumb>,
    pub next: &'static str,
}

#[derive(Debug, Serialize)]
pub struct FeatureContextCounts {
    /// Total retrieval hits (before dedup/cap).
    pub hits: usize,
    /// Distinct evidence pointers after dedup (what `evidence` holds uncapped).
    pub distinct: usize,
    /// Per-channel contributions (OVERLAP — a hit can be in several channels).
    pub semantic: usize,
    pub keyword: usize,
    pub literal: usize,
    pub subject: usize,
}

#[derive(Debug, Serialize)]
pub struct EvidenceLine {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    pub file: String,
    pub lines: String,
    pub kind: String,
    /// Relevance relative to this query's strongest hit (=100); the raw fusion
    /// score is unbounded so a percentage is the comparable form.
    pub relevance_pct: u32,
    pub retrieved_by: Vec<String>,
    /// Inlined verbatim body for the TOP-ranked hits only (bounded length), so an
    /// agent reads the decisive code without first opening the HTML data block —
    /// the top hit usually contains the answer to a behavioral question. `None`
    /// for lower-ranked hits; their full body still lives in the written HTML.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_excerpt: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RelatedPage {
    pub title: String,
    pub domain: String,
    pub page: String,
    pub score: usize,
    pub shared_symbols: Vec<String>,
}

impl CompactFeatureContext {
    pub fn from_response(
        repo_id: Uuid,
        response: &FeatureContextResponse,
        output_html: PathBuf,
    ) -> Self {
        let (semantic, keyword, literal, subject) = channel_counts(&response.postgres);
        // Hits arrive score-desc; guard against an all-zero set.
        let top_score = response
            .postgres
            .hits
            .iter()
            .map(|h| h.score)
            .fold(0.0_f64, f64::max)
            .max(1e-9);

        // Evidence: one pointer per hit, deduped by symbol (else file+lines),
        // keeping the first (strongest) occurrence.
        let mut evidence: Vec<EvidenceLine> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for hit in &response.postgres.hits {
            let file = hit.file_path.clone().unwrap_or_default();
            let lines = line_range(hit.line_start, hit.line_end, "n/a");
            let symbol = hit
                .metadata
                .get("symbol")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            // File-qualified key: a bare symbol name (e.g. `new`, `mint`) recurs
            // across files, so keying on the name alone would merge distinct
            // same-named symbols. Mirrors impact.rs's (name, file) dedup.
            let key = match &symbol {
                Some(s) => format!("sym:{file}|{s}"),
                None => format!("loc:{file}|{lines}"),
            };
            if !seen.insert(key) {
                continue;
            }
            let kind = hit
                .metadata
                .get("kind")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    if is_documentation_hit(hit) {
                        "documentation".to_string()
                    } else {
                        "chunk".to_string()
                    }
                });
            let retrieved_by = hit
                .metadata
                .get("retrieved_by")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            // Inline the verbatim body for the strongest few hits. `evidence.len()`
            // here is the post-dedup rank (hits arrive score-desc), so the first
            // INLINE_BODY_HITS distinct hits carry their code.
            let code_excerpt = if evidence.len() < INLINE_BODY_HITS {
                Some(excerpt_body(&hit.content, MAX_INLINE_BODY_CHARS))
            } else {
                None
            };
            evidence.push(EvidenceLine {
                symbol,
                file,
                lines,
                kind,
                relevance_pct: ((hit.score / top_score) * 100.0).round().clamp(0.0, 100.0) as u32,
                retrieved_by,
                code_excerpt,
            });
        }
        let distinct = evidence.len();
        evidence.truncate(MAX_EVIDENCE_LINES);

        // Related prior pages: compact summaries only, relevance-floored.
        let related_pages: Vec<RelatedPage> = response
            .feature_matches
            .iter()
            .filter(|m| m.score >= MIN_RELATED_PAGE_SCORE)
            .map(|m| {
                let title = if m.feature.title.is_empty() {
                    m.title.clone()
                } else {
                    m.feature.title.clone()
                };
                let mut shared_symbols: Vec<String> = m
                    .matched_nodes
                    .iter()
                    .filter(|n| !n.label.is_empty())
                    .map(|n| n.label.clone())
                    .collect();
                shared_symbols.truncate(MAX_RELATED_SYMBOLS);
                RelatedPage {
                    title,
                    domain: m.feature.domain.clone(),
                    page: m
                        .page
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default()
                        .to_string(),
                    score: m.score,
                    shared_symbols,
                }
            })
            .collect();

        let dropped = response.feature_matches.len() - related_pages.len();
        let inlined = evidence.len().min(INLINE_BODY_HITS);
        let mut provenance = response.provenance.clone();
        provenance.push(Breadcrumb::new(
            source::GRAPH,
            "compact_feature_context",
            format!(
                "compacted {} hit(s) → {distinct} deduped pointer(s) (showing {}, top {inlined} with inlined bodies); kept {} correlated page(s), dropped {dropped} below the relevance floor (score < {MIN_RELATED_PAGE_SCORE}). Full evidence stays in the HTML.",
                response.postgres.hits.len(),
                evidence.len(),
                related_pages.len(),
            ),
        ));

        CompactFeatureContext {
            status: "ok",
            repo_id,
            task: response.task.clone(),
            output_html,
            counts: FeatureContextCounts {
                hits: response.postgres.hits.len(),
                distinct,
                semantic,
                keyword,
                literal,
                subject,
            },
            evidence,
            related_pages,
            warnings: response.warnings.clone(),
            provenance,
            next: COMPACT_NEXT,
        }
    }
}

/// Trim a hit body to `max_chars` on a char boundary for inlining in the compact
/// return, appending a marker when truncated so the agent knows to read the full
/// body (in the HTML data block) before relying on anything past the cut.
fn excerpt_body(content: &str, max_chars: usize) -> String {
    let trimmed = content.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(max_chars).collect();
    format!("{cut}\n… (truncated — full body in the HTML chaos-feature-context-data block)")
}

/// Run `chaos feature-context`: focused retrieval → ALWAYS-written HTML (full
/// evidence + the agent-extractable `chaos-feature-context-data` block) → a
/// COMPACT pointer-only return. The single entry point for both the CLI and MCP
/// surfaces, mirroring [`crate::impact::run`].
pub async fn run(
    storage: &Storage,
    embedder: &dyn Embedder,
    repo: &str,
    task: &str,
    opts: &FeatureContextOptions,
) -> Result<Value> {
    let (repo, repo_root) = resolve_indexed_repo(storage, repo).await?;
    let features_dir = opts
        .features_dir
        .clone()
        .unwrap_or_else(|| features_memory_dir(&repo_root));
    let limit = if opts.limit > 0 { opts.limit } else { 10 };
    let feature_limit = if opts.feature_limit > 0 {
        opts.feature_limit
    } else {
        3
    };
    let nodes_per_feature = if opts.nodes_per_feature > 0 {
        opts.nodes_per_feature
    } else {
        8
    };

    let postgres = query_feature_context_repo(storage, repo.id, embedder, task, limit).await?;
    let warnings = build_feature_context_warnings(task, &repo_root, &postgres);
    let feature_matches =
        load_feature_matches(task, &features_dir, feature_limit, nodes_per_feature)?;
    let provenance = feature_context_provenance(&postgres, &features_dir, &feature_matches);
    let response = FeatureContextResponse {
        task: task.to_string(),
        postgres,
        features_dir,
        warnings,
        feature_matches,
        provenance,
    };

    // ALWAYS write the HTML — it carries the full evidence and the
    // agent-extractable JSON; the returned payload stays compact.
    let output = opts.output_html.clone().unwrap_or_else(|| {
        features_memory_dir(&repo_root).join(format!("{}-context.html", safe_slug(task, "feature")))
    });
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    write_feature_context_html(&output, &response)?;

    let compact = CompactFeatureContext::from_response(repo.id, &response, output);
    Ok(serde_json::to_value(compact)?)
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FeatureStoryStepInput {
    Text(String),
    Step(FeatureStoryStep),
}

fn deserialize_story_steps<'de, D>(deserializer: D) -> Result<Vec<FeatureStoryStep>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let inputs = Vec::<FeatureStoryStepInput>::deserialize(deserializer)?;
    Ok(inputs
        .into_iter()
        .enumerate()
        .map(|(idx, input)| match input {
            FeatureStoryStepInput::Text(title) => FeatureStoryStep {
                id: format!("step-{}", idx + 1),
                title,
                ..FeatureStoryStep::default()
            },
            FeatureStoryStepInput::Step(step) => step,
        })
        .collect())
}

pub fn load_feature_matches(
    task: &str,
    features_dir: &Path,
    feature_limit: usize,
    nodes_per_feature: usize,
) -> Result<Vec<FeatureMatch>> {
    if !features_dir.exists() {
        return Ok(Vec::new());
    }

    let tokens = tokenize(task);
    let mut matches = Vec::new();
    for entry in fs::read_dir(features_dir)
        .with_context(|| format!("reading feature directory {}", features_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("html") {
            continue;
        }
        let Some(manifest) = read_feature_manifest(&path).unwrap_or(None) else {
            continue;
        };
        let scored = score_manifest(path, manifest, &tokens, nodes_per_feature);
        if scored.score > 0 {
            matches.push(scored);
        }
    }

    matches.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
    matches.truncate(feature_limit);
    Ok(matches)
}

pub fn build_feature_context_warnings(
    task: &str,
    repo_root: &Path,
    postgres: &QueryResponse,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for token in tokenize(task) {
        if token.len() < 4 || !repo_root.join(&token).exists() {
            continue;
        }
        let token_in_hits = postgres.hits.iter().any(|hit| {
            hit.file_path
                .as_deref()
                .is_some_and(|path| path.to_ascii_lowercase().contains(&token))
        });
        if !token_in_hits {
            warnings.push(format!(
                "filesystem path `{token}` exists under the repo, but no Postgres hits referenced it; the index may be stale or the feature context limit is too low"
            ));
        }
    }

    if repo_root.join("docs").exists() && !postgres.hits.iter().any(is_documentation_hit) {
        warnings.push(
            "repo has a docs directory, but this feature-context result contains no documentation hits; generated websites should re-query with stronger docs terms or re-index if docs were added recently"
                .to_string(),
        );
    }

    warnings
}

fn is_documentation_hit(hit: &crate::models::SearchHit) -> bool {
    hit.metadata
        .get("source_priority")
        .and_then(|v| v.as_str())
        .is_some_and(|priority| priority == "supplemental")
        || hit
            .metadata
            .get("kind")
            .and_then(|v| v.as_str())
            .is_some_and(|kind| kind == "documentation")
}

pub fn write_feature_context_html(path: &Path, response: &FeatureContextResponse) -> Result<()> {
    crate::export_util::write_report_page(path, CONTEXT_HTML, &serde_json::to_string(response)?)
}

pub(crate) fn read_feature_manifest(path: &Path) -> Result<Option<FeatureManifest>> {
    let html = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    read_feature_manifest_from_html(&html)
        .with_context(|| format!("parsing feature manifest from {}", path.display()))
}

pub(crate) fn read_feature_manifest_from_html(html: &str) -> Result<Option<FeatureManifest>> {
    let Some(start) = html.find(MANIFEST_START) else {
        return Ok(None);
    };
    let json_start = start + MANIFEST_START.len();
    let Some(end) = html[json_start..].find(MANIFEST_END) else {
        return Ok(None);
    };
    let raw = &html[json_start..json_start + end];
    let manifest = serde_json::from_str(raw.trim())?;
    Ok(Some(manifest))
}

fn score_manifest(
    page: PathBuf,
    manifest: FeatureManifest,
    tokens: &[String],
    nodes_per_feature: usize,
) -> FeatureMatch {
    let claims_text = manifest
        .claims
        .iter()
        .map(|claim| format!("{} {}", claim.title, claim.body))
        .collect::<Vec<_>>()
        .join(" ");
    let modes_text = manifest
        .modes
        .iter()
        .map(|mode| mode.title.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let mut score = score_text(
        tokens,
        &[
            manifest.feature.id.as_str(),
            manifest.feature.title.as_str(),
            manifest.feature.domain.as_str(),
            manifest.feature.summary.as_str(),
            manifest.title.as_str(),
            manifest.subtitle.as_str(),
            &claims_text,
            &modes_text,
            &manifest
                .story
                .iter()
                .map(|step| format!("{} {} {}", step.title, step.body, step.node_ids.join(" ")))
                .collect::<Vec<_>>()
                .join(" "),
        ],
    ) * 3;

    let mut node_scores = manifest
        .nodes
        .iter()
        .map(|node| {
            let node_score = score_text(
                tokens,
                &[
                    node.label.as_str(),
                    node.subtitle.as_str(),
                    node.group.as_str(),
                    node.file.as_str(),
                    node.role.as_str(),
                    node.evidence.notes.as_str(),
                    node.code.as_str(),
                ],
            );
            (node_score, node)
        })
        .collect::<Vec<_>>();
    score += node_scores
        .iter()
        .map(|(node_score, _)| node_score)
        .sum::<usize>();
    node_scores.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)));

    let matched_nodes = node_scores
        .into_iter()
        .filter(|(node_score, _)| *node_score > 0)
        .take(nodes_per_feature)
        .map(|(_, node)| node.clone())
        .collect::<Vec<_>>();
    let selected_ids = matched_nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<HashSet<_>>();
    let related_edges = manifest
        .edges
        .iter()
        .filter(|edge| {
            selected_ids.contains(edge.source.as_str())
                || selected_ids.contains(edge.target.as_str())
        })
        .cloned()
        .collect();

    // Lead with a breadcrumb explaining the match, then carry through the page's
    // own generation breadcrumbs so the reader can audit both why it surfaced
    // and how it was originally built.
    let mut provenance = vec![Breadcrumb::new(
        source::MANIFEST,
        "score_manifest",
        format!("matched generated page by {score} shared token hit(s)"),
    )
    .with_locator(page.display().to_string())];
    provenance.extend(manifest.provenance);

    FeatureMatch {
        page,
        feature: manifest.feature,
        title: manifest.title,
        subtitle: manifest.subtitle,
        score,
        claims: manifest.claims,
        modes: manifest.modes,
        story: manifest.story,
        matched_nodes,
        related_edges,
        provenance,
    }
}

/// Scan `features_dir` for previously generated feature pages whose manifests
/// overlap the given `files` or `symbols`, so a *new* feature extraction can see
/// the existing features it correlates with. Overlap is computed on the prior
/// pages' manifest node files (strong signal) and node labels (secondary).
///
/// `exclude_slug` is the `feature.id` of the page currently being (re)written, so
/// a page never correlates with its own prior version. Results are sorted by
/// overlap strength and truncated to `limit`. A missing directory or empty
/// inputs yield an empty list (never an error).
pub fn correlate_feature_manifests(
    features_dir: &Path,
    files: &HashSet<String>,
    symbols: &HashSet<String>,
    exclude_slug: &str,
    limit: usize,
) -> Result<Vec<FeatureCorrelation>> {
    if !features_dir.exists() || (files.is_empty() && symbols.is_empty()) {
        return Ok(Vec::new());
    }
    let want_files: HashSet<String> = files.iter().map(|f| f.to_ascii_lowercase()).collect();
    let want_symbols: HashSet<String> = symbols.iter().map(|s| s.to_ascii_lowercase()).collect();

    let mut out: Vec<FeatureCorrelation> = Vec::new();
    for entry in fs::read_dir(features_dir)
        .with_context(|| format!("reading feature directory {}", features_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("html") {
            continue;
        }
        let Some(manifest) = read_feature_manifest(&path).unwrap_or(None) else {
            continue;
        };
        if !exclude_slug.is_empty() && manifest.feature.id == exclude_slug {
            continue;
        }

        let mut page_files: BTreeSet<String> = BTreeSet::new();
        let mut page_symbols: BTreeSet<String> = BTreeSet::new();
        for node in &manifest.nodes {
            if !node.file.is_empty() {
                page_files.insert(node.file.clone());
            }
            if !node.label.is_empty() {
                page_symbols.insert(node.label.clone());
            }
        }
        let shared_files: Vec<String> = page_files
            .into_iter()
            .filter(|f| want_files.contains(&f.to_ascii_lowercase()))
            .collect();
        let shared_symbols: Vec<String> = page_symbols
            .into_iter()
            .filter(|s| want_symbols.contains(&s.to_ascii_lowercase()))
            .collect();
        let score = shared_files.len() * 2 + shared_symbols.len();
        if score == 0 {
            continue;
        }
        let title = if manifest.feature.title.is_empty() {
            manifest.title
        } else {
            manifest.feature.title
        };
        out.push(FeatureCorrelation {
            page: path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string(),
            feature_id: manifest.feature.id,
            title,
            domain: manifest.feature.domain,
            shared_files,
            shared_symbols,
            score,
        });
    }

    out.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
    out.truncate(limit);
    Ok(out)
}

/// Per-channel hit contributions `(semantic, keyword, literal, subject)`. These
/// OVERLAP: fusion unions every channel that found a chunk onto one hit, so a hit
/// matched by two channels is counted in both and the totals need not sum to the
/// hit count. Shared by [`feature_context_provenance`] and
/// [`CompactFeatureContext`] so the breadcrumb and the counts block never drift.
pub fn channel_counts(postgres: &QueryResponse) -> (usize, usize, usize, usize) {
    let (mut semantic, mut keyword, mut literal, mut subject) = (0usize, 0usize, 0usize, 0usize);
    for hit in &postgres.hits {
        if let Some(methods) = hit.metadata.get("retrieved_by").and_then(|v| v.as_array()) {
            for method in methods {
                match method.as_str() {
                    Some("semantic") => semantic += 1,
                    Some("keyword") => keyword += 1,
                    Some("literal") => literal += 1,
                    Some("subject") => subject += 1,
                    _ => {}
                }
            }
        }
    }
    (semantic, keyword, literal, subject)
}

/// Build the artifact-level breadcrumbs for a feature-context / impact response:
/// how the evidence was retrieved (the hybrid Postgres pipeline, with a
/// per-channel breakdown) and how many prior manifests were scanned/matched.
///
/// The channel counts are **per-channel contributions, not a partition of the
/// hits**: fusion unions every channel that found a chunk onto one hit
/// (`union_retrieved_into`), so a hit matched by two channels is counted in
/// both, and the `subject`-recall channel (files named after the query) is a
/// real channel too. The counts therefore overlap and need not sum to the hit
/// total — the label says so, and every channel that produced a hit is shown so
/// none (notably `subject`) is silently dropped from the provenance story.
pub fn feature_context_provenance(
    postgres: &QueryResponse,
    features_dir: &Path,
    feature_matches: &[FeatureMatch],
) -> Vec<Breadcrumb> {
    let (semantic, keyword, literal, subject) = channel_counts(postgres);
    vec![
        Breadcrumb::new(
            source::POSTGRES,
            "query_feature_context_repo",
            format!(
                "hybrid retrieval over pgvector chunks → {} hit(s) (by channel, overlapping: semantic {semantic}, keyword {keyword}, literal {literal}, subject {subject})",
                postgres.hits.len()
            ),
        ),
        Breadcrumb::new(
            source::MANIFEST,
            "load_feature_matches",
            format!(
                "scanned generated feature pages → {} correlated manifest(s)",
                feature_matches.len()
            ),
        )
        .with_locator(features_dir.display().to_string()),
    ]
}

fn tokenize(value: &str) -> Vec<String> {
    value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|token| token.len() > 2 && !STOP_WORDS.contains(&token.as_str()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

fn score_text(tokens: &[String], haystacks: &[&str]) -> usize {
    let haystack = haystacks.join(" ").to_ascii_lowercase();
    tokens
        .iter()
        .map(|token| haystack.matches(token).count())
        .sum()
}

const STOP_WORDS: &[&str] = &[
    "and",
    "are",
    "for",
    "from",
    "how",
    "into",
    "the",
    "this",
    "that",
    "then",
    "with",
    "would",
    "should",
    "could",
    "feature",
    "implement",
    "implementation",
    "store",
];

pub(crate) const CONTEXT_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Chaos Feature Context</title>
<style>
__THEME__
__REPORT_CSS__
/* ===== feature-context components (light editorial) ===== */
main{padding:48px 0 0;display:block}
.grid{display:grid;grid-template-columns:minmax(360px,.8fr) minmax(520px,1.2fr);gap:24px;margin-bottom:24px}
.panel{margin-bottom:24px}
.grid .panel{margin-bottom:0}
.panel>h2{font:var(--type-h5);color:var(--color-ink-700);margin:0 0 14px;letter-spacing:-.01em}
.item{border:var(--border-hairline);border-radius:var(--radius-md);background:var(--color-surface-1);padding:16px;margin-top:12px}
.item:first-child{margin-top:0}
.item strong{font:var(--type-h6);font-weight:500;color:var(--color-blue-700)}
.item.doc{border-color:var(--color-blue-300);background:var(--color-blue-50)}
.item.doc strong{color:var(--color-blue-800)}
.item.claim strong{color:var(--color-ink-700)}
.item>h2{font:var(--type-overline-sm);text-transform:uppercase;letter-spacing:.1em;color:var(--fg-tertiary);margin:16px 0 4px}
.meta{font:var(--type-body-sm);color:var(--fg-tertiary);line-height:1.5;overflow-wrap:anywhere;margin-top:4px}
.pill{display:inline-block;border:var(--border-hairline);border-radius:var(--radius-pill);padding:4px 11px;margin:6px 6px 0 0;color:var(--color-blue-700);background:var(--color-blue-100);font:500 12px/1.3 var(--font-body)}
.tag{display:inline-flex;border:var(--border-hairline);border-radius:var(--radius-pill);padding:3px 9px;margin-right:8px;font:var(--type-overline-sm);font-family:var(--font-mono);font-weight:500;color:var(--color-ink-500);background:var(--color-surface-2);text-transform:uppercase;letter-spacing:.06em}
.tag.doc{color:var(--color-blue-700);background:var(--color-blue-100);border-color:var(--color-blue-300)}
pre{margin:12px 0 0;padding:14px;border-radius:var(--radius-md);background:var(--color-ink-900);color:var(--color-blue-100);overflow:auto;font:var(--type-body-xs);font-family:var(--font-mono);line-height:1.55;border:var(--border-hairline);max-height:360px}
@media(max-width:1000px){.grid{grid-template-columns:1fr}}
</style>
</head>
<body>
<div class="topbar"><div class="wrap">__BRAND_TOPBAR__<span class="crumb">Feature context<span class="sep">&rsaquo;</span><b>evidence</b></span><span class="sp"></span><span class="pilltag">Feature context</span></div></div>
<header class="hero">
  <div class="wrap">
    <div>
      <div class="eyebrow">Feature context</div>
      <h1>Feature evidence</h1>
      <p class="lede" id="task"></p>
    </div>
  </div>
</header>
<main>
<div class="wrap">
<section class="grid">
<div class="panel"><h2>Feature Matches</h2><div id="features"></div></div>
<div class="panel"><h2>Matched Source</h2><div id="nodes"></div></div>
</section>
<section class="panel"><h2>How this was generated</h2><div class="meta" style="margin-bottom:8px">Provenance breadcrumbs &mdash; where each piece of this evidence came from.</div><div id="provenance"></div></section>
<section class="panel"><h2>Warnings</h2><div id="warnings"></div></section>
<section class="panel"><h2>Documentation Evidence</h2><div id="docs"></div></section>
<section class="panel"><h2>Postgres Retrieval</h2><div id="hits"></div></section>
</div>
</main>
<footer><div class="wrap">__BRAND_FOOTER__<span class="sp"></span><span class="meta">generated by Chaos Substrate</span></div></footer>
<script type="application/json" id="chaos-feature-context-data">__DATA__</script>
<script>
const data=JSON.parse(document.getElementById("chaos-feature-context-data").textContent);
__REPORT_JS__
function isDoc(h){return h?.metadata?.source_priority==="supplemental"||h?.metadata?.kind==="documentation"}
function sourceTag(h){return isDoc(h)?'<span class="tag doc">docs</span>':'<span class="tag">code</span>'}
function retrievedTags(h){return ((h?.metadata?.retrieved_by)||[]).map(m=>`<span class="tag">${esc(m)}</span>`).join("")}
const TOP_SCORE=Math.max(1e-9,...((data.postgres?.hits)||[]).map(h=>+h.score||0));
function relevance(h){return relbar(h.score,TOP_SCORE)}
document.getElementById("task").textContent=data.task;
renderProvenance(document.getElementById("provenance"),data.provenance);
const features=document.getElementById("features");
if(!data.feature_matches.length){features.innerHTML='<div class="meta">No generated feature manifests matched. Use Postgres hits below as starting context.</div>'}
data.feature_matches.forEach(f=>{const el=document.createElement("div");el.className="item";el.innerHTML=`<strong>${esc(f.feature?.title||f.title)}</strong><div class="meta">${esc(f.feature?.domain)} | matched by ${f.score} shared term${f.score==1?"":"s"} | ${esc(f.page)}</div><div class="meta">${(f.provenance||[]).map(c=>`<span class="tag">${esc(c.source)}</span>`).join("")}</div><div>${(f.modes||[]).map(m=>`<span class="pill">${esc(m.title)}</span>`).join("")}</div><h2 style="margin-top:14px">Claims</h2>${(f.claims||[]).map(c=>`<div class="item claim"><strong>${esc(c.title)}</strong><div>${esc(c.body)}</div><div class="meta">confidence ${Math.round((c.confidence||0)*100)}%</div></div>`).join("")}`;features.appendChild(el)});
const nodes=document.getElementById("nodes");
data.feature_matches.flatMap(f=>f.matched_nodes||[]).forEach(n=>{const el=document.createElement("div");el.className="item";el.innerHTML=`<strong>${esc(n.label)}</strong><div>${esc(n.role)}</div><div class="meta">${esc(n.file)} | lines ${esc(n.lines)} | confidence ${Math.round((n.confidence||0)*100)}%</div><pre><code>${esc(n.code)}</code></pre>`;nodes.appendChild(el)});
if(!nodes.children.length){nodes.innerHTML='<div class="meta">No feature-manifest nodes matched.</div>'}
const warnings=document.getElementById("warnings");
(data.warnings||[]).forEach(w=>{const el=document.createElement("div");el.className="item doc";el.innerHTML=`<strong>Context warning</strong><div>${esc(w)}</div>`;warnings.appendChild(el)});
if(!warnings.children.length){warnings.innerHTML='<div class="meta">No stale-index or missing-doc warnings detected.</div>'}
const docs=document.getElementById("docs");
(data.postgres?.hits||[]).filter(isDoc).forEach(h=>{const el=document.createElement("div");el.className="item doc";el.innerHTML=`<strong>${esc(h.file_path||"documentation")}</strong><div class="meta">${sourceTag(h)}${retrievedTags(h)} lines ${esc(h.line_start)}-${esc(h.line_end)} | ${relevance(h)}</div><pre><code>${esc(h.content)}</code></pre>`;docs.appendChild(el)});
if(!docs.children.length){docs.innerHTML='<div class="meta">No matching docs were returned for this query. Re-index after adding Markdown/MDX docs, or raise --limit if the task is very code-specific.</div>'}
const hits=document.getElementById("hits");
(data.postgres?.hits||[]).forEach(h=>{const el=document.createElement("div");el.className=`item ${isDoc(h)?"doc":""}`;el.innerHTML=`<strong>${esc(h.file_path||"unknown file")}</strong><div class="meta">${sourceTag(h)}${retrievedTags(h)} lines ${esc(h.line_start)}-${esc(h.line_end)} | ${relevance(h)}</div><pre><code>${esc(h.content)}</code></pre>`;hits.appendChild(el)});
</script>
</body>
</html>"##;

#[cfg(test)]
mod tests {
    use super::{
        correlate_feature_manifests, load_feature_matches, tokenize, FeatureManifest,
        MANIFEST_START,
    };
    use serde_json::json;
    use std::collections::HashSet;
    use std::fs;

    #[test]
    fn tokenizes_task_text() {
        let tokens = tokenize("implement store icon in secure upload");
        assert!(tokens.contains(&"secure".to_string()));
        assert!(tokens.contains(&"icon".to_string()));
        assert!(!tokens.contains(&"store".to_string()));
    }

    #[test]
    fn accepts_legacy_string_story_steps() {
        let manifest: FeatureManifest = serde_json::from_value(json!({
            "title": "Feature Map",
            "subtitle": "Legacy story",
            "nodes": [],
            "edges": [],
            "story": ["Start upload"]
        }))
        .unwrap();

        assert_eq!(manifest.story[0].id, "step-1");
        assert_eq!(manifest.story[0].title, "Start upload");
        assert!(manifest.story[0].node_ids.is_empty());
    }

    #[test]
    fn accepts_scoped_story_steps() {
        let manifest: FeatureManifest = serde_json::from_value(json!({
            "title": "Feature Map",
            "subtitle": "Scoped story",
            "nodes": [],
            "edges": [],
            "story": [{
                "id": "request-key",
                "title": "Client asks backend/KMS for a DEK",
                "node_ids": ["uploader", "generate-dek", "kms-key"],
                "edge_ids": ["uploader->generate-dek"]
            }]
        }))
        .unwrap();

        assert_eq!(manifest.story[0].id, "request-key");
        assert_eq!(
            manifest.story[0].node_ids,
            vec!["uploader", "generate-dek", "kms-key"]
        );
        assert_eq!(manifest.story[0].edge_ids, vec!["uploader->generate-dek"]);
    }

    #[test]
    fn correlates_prior_manifest_by_shared_file_and_excludes_self() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = json!({
            "feature": {"id": "feature-auth", "title": "Auth", "domain": "feature", "summary": ""},
            "title": "Auth",
            "subtitle": "",
            "nodes": [{
                "id": "n1", "label": "login", "subtitle": "function", "group": "backend",
                "file": "src/auth.rs", "lines": "1-10", "role": "", "code": ""
            }],
            "edges": []
        });
        fs::write(
            dir.path().join("feature-auth.html"),
            format!("{MANIFEST_START}\n{manifest}\n</script>"),
        )
        .unwrap();

        let files: HashSet<String> = ["src/auth.rs".to_string()].into_iter().collect();
        let symbols: HashSet<String> = HashSet::new();

        let correlations =
            correlate_feature_manifests(dir.path(), &files, &symbols, "feature-new", 5).unwrap();
        assert_eq!(correlations.len(), 1);
        assert_eq!(correlations[0].feature_id, "feature-auth");
        assert_eq!(
            correlations[0].shared_files,
            vec!["src/auth.rs".to_string()]
        );
        assert_eq!(correlations[0].score, 2);

        // A page never correlates with its own prior version.
        let self_excluded =
            correlate_feature_manifests(dir.path(), &files, &symbols, "feature-auth", 5).unwrap();
        assert!(self_excluded.is_empty());
    }

    #[test]
    fn skips_malformed_feature_manifests() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("broken.html"),
            format!("{MANIFEST_START}\n{{\"nodes\":[]}}\n</script>"),
        )
        .unwrap();

        let matches = load_feature_matches("OCL", dir.path(), 3, 8).unwrap();

        assert!(matches.is_empty());
    }

    #[test]
    fn retrieval_breadcrumb_counts_every_channel_and_is_honest_about_overlap() {
        use crate::models::SearchHit;
        use crate::query::QueryResponse;
        use uuid::Uuid;

        fn hit(methods: &[&str]) -> SearchHit {
            SearchHit {
                chunk_id: Uuid::nil(),
                node_id: None,
                file_path: Some("src/lab.rs".to_string()),
                line_start: None,
                line_end: None,
                score: 0.5,
                content: String::new(),
                metadata: json!({ "retrieved_by": methods }),
            }
        }

        // Mirrors the real molecule_core report that exposed the bug: 4 hits
        // matched by both semantic+keyword, 1 by semantic only, 5 by subject only.
        let mut hits = vec![hit(&["semantic", "keyword"]); 4];
        hits.push(hit(&["semantic"]));
        hits.extend(std::iter::repeat_with(|| hit(&["subject"])).take(5));
        let postgres = QueryResponse {
            hits,
            context_paths: Vec::new(),
        };

        let dir = tempfile::tempdir().unwrap();
        let crumbs = super::feature_context_provenance(&postgres, dir.path(), &[]);
        let detail = &crumbs[0].detail;

        // Per-channel contributions, including the subject channel that used to
        // be silently dropped; they overlap and need not sum to the hit total.
        assert!(detail.contains("10 hit(s)"), "{detail}");
        assert!(detail.contains("semantic 5"), "{detail}");
        assert!(detail.contains("keyword 4"), "{detail}");
        assert!(detail.contains("literal 0"), "{detail}");
        assert!(detail.contains("subject 5"), "{detail}");
        assert!(detail.contains("overlapping"), "{detail}");
    }

    #[test]
    fn compact_feature_context_dedups_evidence_and_floors_related() {
        use crate::models::SearchHit;
        use crate::query::QueryResponse;
        use std::path::PathBuf;
        use uuid::Uuid;

        fn hit(file: &str, line: i32, score: f64, symbol: Option<&str>) -> SearchHit {
            let mut meta = json!({ "retrieved_by": ["semantic"], "kind": "function" });
            if let Some(s) = symbol {
                meta["symbol"] = json!(s);
            }
            SearchHit {
                chunk_id: Uuid::new_v4(),
                node_id: None,
                file_path: Some(file.to_string()),
                line_start: Some(line),
                line_end: Some(line + 10),
                score,
                content: "x".repeat(5000), // long body — top hits inline a CAPPED excerpt; the full body stays in the HTML
                metadata: meta,
            }
        }

        fn fmatch(title: &str, score: usize) -> super::FeatureMatch {
            super::FeatureMatch {
                page: PathBuf::from(format!("/x/{title}.html")),
                feature: super::FeatureDefinition {
                    id: title.to_string(),
                    title: title.to_string(),
                    domain: "auth".to_string(),
                    summary: String::new(),
                },
                title: title.to_string(),
                subtitle: String::new(),
                score,
                claims: Vec::new(),
                modes: Vec::new(),
                story: Vec::new(),
                matched_nodes: Vec::new(),
                related_edges: Vec::new(),
                provenance: Vec::new(),
            }
        }

        let response = super::FeatureContextResponse {
            task: "service token".to_string(),
            postgres: QueryResponse {
                hits: vec![
                    hit("a.ts", 10, 0.9, Some("mint")), // strongest
                    hit("a.ts", 99, 0.5, Some("mint")), // dup SYMBOL -> dropped
                    hit("b.ts", 20, 0.45, None),        // distinct by (file,lines)
                ],
                context_paths: Vec::new(),
            },
            features_dir: PathBuf::from("/x"),
            warnings: Vec::new(),
            feature_matches: vec![fmatch("real", 9), fmatch("noise", 2)],
            provenance: Vec::new(),
        };

        let compact = super::CompactFeatureContext::from_response(
            Uuid::nil(),
            &response,
            PathBuf::from("/x/out.html"),
        );

        // counts.hits is the RAW total; evidence is deduped 3 -> 2.
        assert_eq!(compact.counts.hits, 3);
        assert_eq!(compact.counts.distinct, 2);
        assert_eq!(compact.evidence.len(), 2);
        assert_eq!(compact.counts.semantic, 3);
        // Strongest hit normalizes to 100%; the deduped survivor is the high score.
        assert_eq!(compact.evidence[0].symbol.as_deref(), Some("mint"));
        assert_eq!(compact.evidence[0].relevance_pct, 100);
        // The relevance floor drops the score-2 page, keeps the score-9 page.
        assert_eq!(compact.related_pages.len(), 1);
        assert_eq!(compact.related_pages[0].title, "real");

        // New contract: the top hits inline a BOUNDED body so the agent reads the
        // decisive code, but the full 5000-char content must still never reach the
        // payload — only a capped excerpt, with the untruncated body left in the HTML.
        let excerpt = compact.evidence[0]
            .code_excerpt
            .as_deref()
            .expect("the strongest hit inlines its body");
        assert!(excerpt.starts_with('x'));
        assert!(excerpt.chars().count() <= super::MAX_INLINE_BODY_CHARS + 80); // + truncation marker
        let serialized = serde_json::to_string(&compact).unwrap();
        assert!(!serialized.contains(&"x".repeat(5000)));
    }

    #[test]
    fn compact_feature_context_keeps_same_named_symbols_in_different_files() {
        use crate::models::SearchHit;
        use crate::query::QueryResponse;
        use std::path::PathBuf;
        use uuid::Uuid;

        fn hit(file: &str, score: f64, symbol: &str) -> SearchHit {
            SearchHit {
                chunk_id: Uuid::new_v4(),
                node_id: None,
                file_path: Some(file.to_string()),
                line_start: Some(1),
                line_end: Some(9),
                score,
                content: String::new(),
                metadata: json!({ "retrieved_by": ["semantic"], "kind": "function", "symbol": symbol }),
            }
        }

        // Two genuinely distinct `mint` symbols in different files must NOT merge.
        let response = super::FeatureContextResponse {
            task: "mint".to_string(),
            postgres: QueryResponse {
                hits: vec![hit("a.ts", 0.9, "mint"), hit("b.ts", 0.8, "mint")],
                context_paths: Vec::new(),
            },
            features_dir: PathBuf::from("/x"),
            warnings: Vec::new(),
            feature_matches: Vec::new(),
            provenance: Vec::new(),
        };
        let compact = super::CompactFeatureContext::from_response(
            Uuid::nil(),
            &response,
            PathBuf::from("/x/out.html"),
        );
        assert_eq!(
            compact.counts.distinct, 2,
            "same-named symbols in different files must stay distinct"
        );
        assert_eq!(compact.evidence.len(), 2);
    }

    #[test]
    fn feature_context_html_embeds_idtagged_json() {
        use crate::query::QueryResponse;
        use std::path::PathBuf;

        let response = super::FeatureContextResponse {
            task: "svc".to_string(),
            postgres: QueryResponse {
                hits: Vec::new(),
                context_paths: Vec::new(),
            },
            features_dir: PathBuf::from("/x"),
            warnings: Vec::new(),
            feature_matches: Vec::new(),
            provenance: Vec::new(),
        };
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("svc-context.html");
        super::write_feature_context_html(&out, &response).unwrap();
        let html = fs::read_to_string(&out).unwrap();
        assert!(html.contains(r#"id="chaos-feature-context-data""#));
        assert!(html.contains("JSON.parse"));
        assert!(html.contains("Feature evidence"));
    }
}
