//! Use-case `feature` across a PROJECT: the same question answered in every
//! selected member repository (each through the single-repo pipeline, not
//! persisted on its own), joined by cross-repo links and persisted as ONE
//! page under the project's features directory.
//!
//! A cross-repo link is literal evidence, never inferred: an excerpt symbol
//! retrieved from repo A whose name (identifier-shaped, ≥ 6 chars) occurs as a
//! whole word in the code of an excerpt retrieved from repo B. The chunk
//! header lines that describe the chunk itself (`// file:`, `// in:`,
//! `// module:`, `// kind:`) are never evidence.

use super::feature::{self, Deps, FeatureAnswer, FeatureRequest};
use crate::domain::hash;
use crate::domain::model::Repo;
use crate::domain::provenance::{source, Breadcrumb};
use crate::ports::pages::PageKind;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Cap on cross-repo links kept (sorted, so the cut is deterministic).
pub const MAX_LINKS: usize = 60;
const MIN_TERM_CHARS: usize = 6;
const SELF_HEADER: [&str; 4] = ["// file:", "// in:", "// module:", "// kind:"];

/// One selected repository of the project.
#[derive(Debug, Clone)]
pub struct Member {
    pub alias: String,
    pub repo: Repo,
    pub root: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoAnswer {
    pub alias: String,
    #[serde(flatten)]
    pub answer: FeatureAnswer,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct CrossLink {
    /// Repo alias + excerpt symbol whose name is `term`.
    pub from_repo: String,
    pub from: String,
    pub from_path: String,
    /// Repo alias + excerpt symbol whose code mentions `term`.
    pub to_repo: String,
    pub to: String,
    pub to_path: String,
    pub term: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectFeatureAnswer {
    pub question: String,
    pub project: String,
    pub repos: Vec<RepoAnswer>,
    pub links: Vec<CrossLink>,
    /// Absolute page path (when persisted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    pub page_cached: bool,
    pub content_hash: String,
    pub tokens_used: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub provenance: Vec<Breadcrumb>,
}

/// One member's share of the token budget: the (clamped) total split evenly,
/// never below [`feature::MIN_BUDGET`] — so the whole answer stays about as
/// large as a single-repo one.
pub fn per_repo_budget(total: usize, repos: usize) -> usize {
    (total.clamp(feature::MIN_BUDGET, feature::MAX_BUDGET) / repos.max(1)).max(feature::MIN_BUDGET)
}

/// Last segment of a qualified symbol (`A.b`, `a::b`, `A > B`).
pub fn symbol_tail(symbol: &str) -> &str {
    let s = symbol.rsplit(" > ").next().unwrap_or(symbol);
    let s = s.rsplit("::").next().unwrap_or(s);
    s.rsplit('.').next().unwrap_or(s).trim()
}

fn is_link_term(term: &str) -> bool {
    term.chars().count() >= MIN_TERM_CHARS
        && term.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && feature::looks_like_identifier(term)
}

/// The excerpt code minus the header lines that describe the chunk itself.
fn evidence(code: &str) -> String {
    code.lines()
        .filter(|l| !SELF_HEADER.iter().any(|p| l.starts_with(p)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `term` occurs in `text` as a whole identifier.
fn mentions(text: &str, term: &str) -> bool {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$';
    text.match_indices(term).any(|(i, _)| {
        !text[..i].chars().next_back().is_some_and(ident)
            && !text[i + term.len()..].chars().next().is_some_and(ident)
    })
}

/// Cross-repo links between the excerpts of different member answers.
/// Two same-named symbols in two repos yield one link (earlier repo → later).
pub fn cross_links(repos: &[RepoAnswer]) -> Vec<CrossLink> {
    let evidences: Vec<Vec<String>> = repos
        .iter()
        .map(|r| {
            r.answer
                .excerpts
                .iter()
                .map(|e| evidence(&e.code))
                .collect()
        })
        .collect();
    let mut out = BTreeSet::new();
    for (i, a) in repos.iter().enumerate() {
        for def in &a.answer.excerpts {
            let term = symbol_tail(&def.symbol);
            if !is_link_term(term) {
                continue;
            }
            for (j, b) in repos.iter().enumerate() {
                if i == j {
                    continue;
                }
                for (k, user) in b.answer.excerpts.iter().enumerate() {
                    if j < i && symbol_tail(&user.symbol) == term {
                        continue;
                    }
                    if mentions(&evidences[j][k], term) {
                        out.insert(CrossLink {
                            from_repo: a.alias.clone(),
                            from: def.symbol.clone(),
                            from_path: def.path.clone(),
                            to_repo: b.alias.clone(),
                            to: user.symbol.clone(),
                            to_path: user.path.clone(),
                            term: term.to_string(),
                        });
                    }
                }
            }
        }
    }
    out.into_iter().take(MAX_LINKS).collect()
}

/// Answer `req.question` in every member (the budget is split across them),
/// link the answers, and persist one hash-gated page at `<out_dir>/<slug>.html`.
pub async fn project_feature(
    deps: Deps<'_>,
    project: &str,
    members: &[Member],
    out_dir: &Path,
    req: FeatureRequest,
) -> Result<ProjectFeatureAnswer> {
    anyhow::ensure!(!members.is_empty(), "feature: no repositories selected");
    let question = req.question.trim().to_string();
    let share = per_repo_budget(req.budget_tokens, members.len());
    let mut repos = Vec::with_capacity(members.len());
    let mut warnings = Vec::new();
    let mut provenance = vec![Breadcrumb::new(
        source::GRAPH,
        "budget",
        format!(
            "{share} tokens per repo: {} split across {} repos (at least {})",
            req.budget_tokens
                .clamp(feature::MIN_BUDGET, feature::MAX_BUDGET),
            members.len(),
            feature::MIN_BUDGET
        ),
    )];
    for m in members {
        let sub = FeatureRequest {
            persist: false,
            budget_tokens: share,
            ..req.clone()
        };
        let answer = feature::feature(deps, &m.repo, &m.root, sub)
            .await
            .with_context(|| format!("feature in {}", m.alias))?;
        warnings.extend(answer.warnings.iter().map(|w| format!("{}: {w}", m.alias)));
        provenance.push(
            Breadcrumb::new(
                source::GRAPH,
                "repo",
                format!(
                    "{} features, {} excerpts, {} edges, {} tokens",
                    answer.features.len(),
                    answer.excerpts.len(),
                    answer.graph.len(),
                    answer.tokens_used
                ),
            )
            .with_locator(m.alias.clone()),
        );
        repos.push(RepoAnswer {
            alias: m.alias.clone(),
            answer,
        });
    }
    let links = cross_links(&repos);
    provenance.push(Breadcrumb::new(
        source::GRAPH,
        "cross-repo",
        format!(
            "{} links: excerpt symbol names found verbatim in another repo's excerpts (cap {MAX_LINKS})",
            links.len()
        ),
    ));
    let content_hash = hash::sha256_hex(&format!(
        "{question}\n{}",
        repos
            .iter()
            .map(|r| format!("{}:{}", r.alias, r.answer.content_hash))
            .collect::<Vec<_>>()
            .join("\n")
    ));
    let mut answer = ProjectFeatureAnswer {
        question: question.clone(),
        project: project.to_string(),
        tokens_used: repos.iter().map(|r| r.answer.tokens_used).sum(),
        repos,
        links,
        page: None,
        page_cached: false,
        content_hash,
        warnings,
        provenance,
    };
    if req.persist {
        let abs = out_dir.join(format!("{}.html", feature::slug(&question)));
        let written = deps
            .pages
            .write_page(&abs, PageKind::ProjectFeature, &manifest(&answer))
            .with_context(|| format!("writing {}", abs.display()))?;
        let path = abs.to_string_lossy().replace('\\', "/");
        answer.provenance.push(
            Breadcrumb::new(
                source::PAGE,
                if written { "write" } else { "cached" },
                if written {
                    "project feature page written"
                } else {
                    "project feature page unchanged (content hash matched)"
                },
            )
            .with_locator(path.clone()),
        );
        answer.page = Some(path);
        answer.page_cached = !written;
    }
    Ok(answer)
}

/// The page manifest: the answer plus each repo in the single-feature manifest
/// shape (so both templates read excerpts/edges the same way).
pub fn manifest(answer: &ProjectFeatureAnswer) -> Value {
    let mut v = serde_json::to_value(answer).unwrap_or_else(|_| json!({}));
    v["title"] = json!(answer.question);
    v["repos"] = json!(answer
        .repos
        .iter()
        .map(|r| {
            let mut m = feature::manifest(&r.answer);
            m["alias"] = json!(r.alias);
            m
        })
        .collect::<Vec<_>>());
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::feature::Excerpt;

    fn excerpt(symbol: &str, path: &str, code: &str) -> Excerpt {
        Excerpt {
            path: path.into(),
            lines: [1, 2],
            symbol: symbol.into(),
            kind: "function".into(),
            score: 1.0,
            why: vec!["semantic"],
            code: code.into(),
            code_truncated: false,
        }
    }

    fn repo(alias: &str, excerpts: Vec<Excerpt>) -> RepoAnswer {
        RepoAnswer {
            alias: alias.into(),
            answer: FeatureAnswer {
                question: "q".into(),
                repo: alias.into(),
                features: vec![],
                excerpts,
                graph: vec![],
                surfaces: vec![],
                page: None,
                page_cached: false,
                content_hash: "h".into(),
                tokens_used: 0,
                warnings: vec![],
                provenance: vec![],
            },
        }
    }

    #[test]
    fn budget_is_split_across_repos() {
        assert_eq!(per_repo_budget(6000, 1), 6000);
        assert_eq!(per_repo_budget(6000, 4), 1500);
        assert_eq!(per_repo_budget(16000, 4), 4000);
        assert_eq!(per_repo_budget(99_000, 4), 4000, "total clamped first");
        assert_eq!(per_repo_budget(6000, 10), 1000, "floor per repo");
    }

    #[test]
    fn tails() {
        assert_eq!(symbol_tail("OclTokenizer.attachToken"), "attachToken");
        assert_eq!(symbol_tail("Store::replace_repo"), "replace_repo");
        assert_eq!(symbol_tail("Guide > Install"), "Install");
        assert!(is_link_term("attachToken") && is_link_term("replace_repo"));
        assert!(!is_link_term("tokenize") && !is_link_term("getId"));
    }

    #[test]
    fn whole_word_mentions_only() {
        assert!(mentions("functionName: 'attachToken',", "attachToken"));
        assert!(!mentions("attachTokenFlow()", "attachToken"));
        assert!(!mentions("$attachToken", "attachToken"));
        let code =
            "// file: web/a.ts (typescript)  L1-3\n// kind: function  name: attachToken\nfoo()";
        assert!(!mentions(&evidence(code), "attachToken"), "self header");
        assert!(mentions(
            &evidence("// calls: attachToken\nx"),
            "attachToken"
        ));
    }

    #[test]
    fn links_are_cross_repo_directed_and_deduped() {
        let repos = vec![
            repo(
                "contracts",
                vec![
                    excerpt(
                        "OclTokenizer.attachToken",
                        "src/T.sol",
                        "function attachToken() {}",
                    ),
                    excerpt(
                        "OclTokenizer.tokenize",
                        "src/T.sol",
                        "function tokenize() {}",
                    ),
                ],
            ),
            repo(
                "web",
                vec![
                    excerpt(
                        "useAttach",
                        "app/a.ts",
                        "write({ functionName: 'attachToken' })",
                    ),
                    excerpt(
                        "attachToken",
                        "app/abi.ts",
                        "export function attachToken() {}",
                    ),
                    excerpt("helper", "app/h.ts", "attachToken2()"),
                ],
            ),
        ];
        let links = cross_links(&repos);
        let pairs: Vec<(&str, &str, &str)> = links
            .iter()
            .map(|l| (l.from_repo.as_str(), l.from.as_str(), l.to.as_str()))
            .collect();
        // `tokenize` is not identifier-shaped; web's same-named `attachToken`
        // does not link back; `attachToken2` is not a whole-word mention.
        assert_eq!(
            pairs,
            [
                ("contracts", "OclTokenizer.attachToken", "attachToken"),
                ("contracts", "OclTokenizer.attachToken", "useAttach"),
            ]
        );
        assert!(links.iter().all(|l| l.term == "attachToken"));
    }
}
