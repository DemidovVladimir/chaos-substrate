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
//!
//! When the question anchors on a GraphQL field (see
//! [`super::graphql_entry`]), the members' traces are joined into one
//! [`ProjectEntry`]: the repos with the SDL, the resolvers and registrations,
//! and the calling operations, each tagged with its repo. An entry link joins
//! a calling operation in one repo to a resolver of the same coordinate in
//! another (graph evidence, not a name match).

use super::feature::{self, Deps, FeatureAnswer, FeatureRequest};
use super::graphql_entry::{EntryBinding, EntryCaller, EntryResolver};
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

/// One row of a joined [`ProjectEntry`], tagged with its member repo.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct InRepo<T> {
    pub repo: String,
    #[serde(flatten)]
    pub item: T,
}

/// The members' GraphQL entry traces joined (module docs).
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct ProjectEntry {
    pub coordinates: Vec<String>,
    /// Members holding an SDL copy of the fields.
    pub sdl_repos: Vec<String>,
    pub resolvers: Vec<InRepo<EntryResolver>>,
    pub bindings: Vec<InRepo<EntryBinding>>,
    pub callers: Vec<InRepo<EntryCaller>>,
}

/// Join the members' entry traces; `None` when no member anchored.
pub fn join_entries(repos: &[RepoAnswer]) -> Option<ProjectEntry> {
    let mut out = ProjectEntry::default();
    let mut any = false;
    for r in repos {
        let Some(e) = &r.answer.entry else { continue };
        any = true;
        for c in &e.coordinates {
            if !out.coordinates.contains(c) {
                out.coordinates.push(c.clone());
            }
        }
        if !e.sdl.is_empty() && !out.sdl_repos.contains(&r.alias) {
            out.sdl_repos.push(r.alias.clone());
        }
        out.resolvers.extend(tagged(&r.alias, &e.resolvers));
        out.bindings.extend(tagged(&r.alias, &e.bindings));
        out.callers.extend(tagged(&r.alias, &e.callers));
    }
    any.then_some(out)
}

fn tagged<T: Clone>(repo: &str, items: &[T]) -> Vec<InRepo<T>> {
    items
        .iter()
        .map(|item| InRepo {
            repo: repo.to_string(),
            item: item.clone(),
        })
        .collect()
}

/// Tokens the joined entry and its links add on top of the members' answers
/// (they repeat member rows, so they are charged again).
pub fn joined_entry_tokens(entry: Option<&ProjectEntry>, links: &[CrossLink]) -> usize {
    let chars = entry
        .and_then(|e| serde_json::to_string(e).ok())
        .map_or(0, |j| j.len())
        + if links.is_empty() {
            0
        } else {
            serde_json::to_string(links).map_or(0, |j| j.len())
        };
    chars.div_ceil(crate::domain::budget::CHARS_PER_TOKEN)
}

/// Calling operation in one member → resolver of the same coordinate in
/// another; sorted, deduplicated, capped at [`MAX_LINKS`].
pub fn entry_links(entry: &ProjectEntry) -> Vec<CrossLink> {
    let mut out = BTreeSet::new();
    for c in &entry.callers {
        for r in &entry.resolvers {
            if r.repo == c.repo || r.item.coordinate != c.item.coordinate {
                continue;
            }
            out.insert(CrossLink {
                from_repo: c.repo.clone(),
                from: c.item.op.clone(),
                from_path: c.item.path.clone(),
                to_repo: r.repo.clone(),
                to: r.item.symbol.clone(),
                to_path: r.item.path.clone(),
                term: c.item.coordinate.clone(),
            });
        }
    }
    out.into_iter().take(MAX_LINKS).collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectFeatureAnswer {
    pub question: String,
    pub project: String,
    pub repos: Vec<RepoAnswer>,
    pub links: Vec<CrossLink>,
    /// The joined GraphQL entry trace, when the question anchors on a field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<ProjectEntry>,
    /// Calling operation → resolver links across members ([`entry_links`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub entry_links: Vec<CrossLink>,
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
    let entry = join_entries(&repos);
    let entry_links = entry.as_ref().map(entry_links).unwrap_or_default();
    if let Some(e) = &entry {
        provenance.push(Breadcrumb::new(
            source::GRAPH,
            "entry",
            format!(
                "{}: SDL in {}; {} resolvers, {} bindings, {} callers; {} operation → resolver links across repos",
                e.coordinates.join(", "),
                if e.sdl_repos.is_empty() {
                    "no member".to_string()
                } else {
                    e.sdl_repos.join(", ")
                },
                e.resolvers.len(),
                e.bindings.len(),
                e.callers.len(),
                entry_links.len()
            ),
        ));
    }
    let content_hash = hash::sha256_hex(&format!(
        "{question}\n{}",
        repos
            .iter()
            .map(|r| format!("{}:{}", r.alias, r.answer.content_hash))
            .collect::<Vec<_>>()
            .join("\n")
    ));
    let tokens_used = repos.iter().map(|r| r.answer.tokens_used).sum::<usize>()
        + joined_entry_tokens(entry.as_ref(), &entry_links);
    let mut answer = ProjectFeatureAnswer {
        question: question.clone(),
        project: project.to_string(),
        tokens_used,
        repos,
        links,
        entry,
        entry_links,
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
                entry: None,
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
    fn project_entry_joins_resolvers_and_callers_across_repos() {
        use crate::application::graphql_entry::EntryTrace;
        let trace = |resolvers, callers| EntryTrace {
            coordinates: vec!["Mutation.createLab".into()],
            sdl: vec![],
            bindings: vec![],
            resolvers,
            trace: vec![],
            callers,
        };
        let mut api = repo("api", vec![]);
        api.answer.entry = Some(trace(
            vec![EntryResolver {
                coordinate: "Mutation.createLab".into(),
                symbol: "originalHandler::createLab".into(),
                path: "lambda/appsync-resolver-labs-lambda/index.ts".into(),
                lines: [13, 20],
                confidence: 0.9,
            }],
            vec![],
        ));
        let mut web = repo("web", vec![]);
        web.answer.entry = Some(trace(
            vec![],
            vec![EntryCaller {
                coordinate: "Mutation.createLab".into(),
                op: "CreateLab".into(),
                path: "apps/web/hooks/useCreateLab.ts".into(),
                line: 4,
                hooks: vec!["useCreateLab".into()],
            }],
        ));
        let plain = repo("docs", vec![]);
        let repos = vec![api, web, plain];
        let joined = join_entries(&repos).expect("two members anchored");
        assert_eq!(joined.coordinates, vec!["Mutation.createLab"]);
        assert_eq!(joined.resolvers.len(), 1);
        assert_eq!(joined.resolvers[0].repo, "api");
        assert_eq!(joined.callers.len(), 1);
        assert_eq!(joined.callers[0].repo, "web");
        assert!(joined.sdl_repos.is_empty());
        assert_eq!(
            entry_links(&joined),
            vec![CrossLink {
                from_repo: "web".into(),
                from: "CreateLab".into(),
                from_path: "apps/web/hooks/useCreateLab.ts".into(),
                to_repo: "api".into(),
                to: "originalHandler::createLab".into(),
                to_path: "lambda/appsync-resolver-labs-lambda/index.ts".into(),
                term: "Mutation.createLab".into(),
            }]
        );
        // Flattened rows serialize with their repo.
        let v = serde_json::to_value(&joined).unwrap();
        assert_eq!(v["callers"][0]["repo"], "web");
        assert_eq!(v["callers"][0]["op"], "CreateLab");
        assert!(join_entries(&repos[2..]).is_none());
        // The joined rows repeat member rows and are charged on top of them.
        let links = entry_links(&joined);
        let chars = serde_json::to_string(&joined).unwrap().len()
            + serde_json::to_string(&links).unwrap().len();
        assert_eq!(
            joined_entry_tokens(Some(&joined), &links),
            chars.div_ceil(4)
        );
        assert_eq!(joined_entry_tokens(None, &[]), 0);
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
