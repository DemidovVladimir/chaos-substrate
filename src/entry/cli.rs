//! CLI entry (clap) + the shared `Services` wiring and tool dispatcher that the
//! MCP server reuses. One tokio runtime is created in [`run`].

use crate::adapters::{
    config::{Config, Project},
    embedding::build_embedder,
    fs_pages::FsPages,
    git::GitCli,
    postgres::Postgres,
    treesitter::TreeSitterExtractor,
};
use crate::application::{
    self,
    index::{identify, IndexOptions},
    tools::{mcp_name, tools, TOOL_NAMES},
};
use crate::ports::{embedder::Embedder, index_store::IndexStore, pages::PageStore};
use anyhow::{bail, Context, Result};
use clap::{ArgAction, Parser, Subcommand};
use serde_json::{json, Value};
use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Parser, Debug)]
#[command(
    name = "chaos",
    version,
    about = "Chaos Substrate — knowledge-graph memory for multi-repository projects (six agent tools)",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Path to chaos-substrate.toml (default: ./chaos-substrate.toml or env).
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    /// Path to a project chaos.toml (default: nearest ancestor of the cwd).
    #[arg(long, global = true)]
    pub project: Option<PathBuf>,
    /// Repository alias (from chaos.toml) or path. Default: current directory.
    #[arg(long, global = true)]
    pub repo: Option<String>,
    /// Verbose logging (-v debug, -vv trace).
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Fully index a repository (extraction → resolution → chunks → embeddings → communities → summaries).
    Index {
        /// Extract, analyze and chunk only; write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Bring the index up to HEAD: the git delta since the stamped run plus the
    /// files importing or referencing what changed (no-op when HEAD is the
    /// stamp, nothing is or was uncommitted and the indexing rules are
    /// unchanged; full index if the stamp is missing, incomplete, from other
    /// rules or its commit is not in this clone).
    Reindex {
        /// Run the full index even when nothing changed (needed after a Chaos
        /// upgrade that changes extraction).
        #[arg(long)]
        force: bool,
    },
    /// Incrementally index the git delta since the stamped run: committed
    /// since the stamped commit, uncommitted now, uncommitted at the stamp,
    /// each kept only when the full listing would have it.
    Add {
        /// Also re-check every file changed between this git ref and HEAD.
        #[arg(long)]
        since: Option<String>,
    },
    /// Answer one feature question from the index (no source reads).
    Feature {
        /// The question, e.g. "how are embeddings reused on reindex".
        question: String,
        /// Token budget for the answer (1000..=16000).
        #[arg(long, default_value_t = application::feature::DEFAULT_BUDGET)]
        budget: usize,
        /// Do not persist the answer as a page.
        #[arg(long)]
        no_persist: bool,
        /// Print JSON instead of markdown.
        #[arg(long)]
        json: bool,
        /// Answer across these project repos (aliases/paths) on one page.
        #[arg(long, num_args = 1..)]
        repos: Vec<String>,
        /// Answer across every repo of the project (chaos.toml).
        #[arg(long)]
        all_repos: bool,
        /// GraphQL field to start from: `Mutation.createLab` or `createLab`.
        #[arg(long)]
        entry: Option<String>,
    },
    /// Write the feature website of a repository (index + per-feature pages);
    /// with --repos/--all-repos, every member's site plus the project hub.
    Compose {
        /// Output directory (default: the repo's docs/features_memory/site).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Max feature pages (largest communities first).
        #[arg(long)]
        limit: Option<usize>,
        /// Compose these project repos (aliases/paths) and the project hub.
        #[arg(long, num_args = 1..)]
        repos: Vec<String>,
        /// Compose every repo of the project (chaos.toml) and the project hub.
        #[arg(long)]
        all_repos: bool,
    },
    /// Intra- and cross-repo feature correlations for the project.
    Correlations {
        /// Drop cross-repo links below this confidence.
        #[arg(long, default_value_t = 0.5)]
        min_confidence: f32,
        /// Restrict to these aliases/paths.
        #[arg(long, num_args = 1..)]
        repos: Vec<String>,
    },
    /// Apply database migrations.
    Migrate {
        /// Drop and recreate the schema first (required to leave a legacy 0.x schema).
        #[arg(long)]
        reset: bool,
    },
    /// Check config, Postgres, embedder and git; exit non-zero on any failure.
    Doctor,
    /// Serve the six tools over MCP (stdio, newline-delimited JSON-RPC).
    Mcp,
    /// Print the agent guide (tools + workflow). Needs no config or database.
    Help,
}

/// Adapters wired once, shared by every tool call (CLI and MCP).
pub struct Services {
    pub config: Config,
    pub store: Postgres,
    pub embedder: Arc<dyn Embedder>,
    pub vcs: GitCli,
    pub extractor: TreeSitterExtractor,
    pub pages: FsPages,
    /// The project (chaos.toml) resolved from `--project` or the process cwd.
    pub project: Option<Project>,
    /// Directory holding that chaos.toml.
    pub project_dir: Option<PathBuf>,
}

impl Services {
    pub async fn connect(config_path: Option<&Path>, project_path: Option<&Path>) -> Result<Self> {
        let config = Config::load(config_path)?;
        let store = Postgres::connect(&config.storage.database_url).await?;
        let embedder = build_embedder(&config.embedding)?;
        let (project, project_dir) = load_project(project_path)?;
        Ok(Self {
            config,
            store,
            embedder,
            vcs: GitCli,
            extractor: TreeSitterExtractor,
            pages: FsPages,
            project,
            project_dir,
        })
    }

    fn index_options(&self) -> IndexOptions {
        let mut opts = IndexOptions {
            skip_dirs: self.config.indexing.skip_dirs.clone(),
            ..IndexOptions::default()
        };
        if let Some(cos) = env::var("CHAOS_SPLIT_COS")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
        {
            opts.split_cos = cos;
        }
        opts
    }

    /// [`Self::index_options`] plus the repository's Foundry layout (import
    /// remappings, vendored `libs` skipped by path).
    fn index_options_for(&self, root: &Path) -> IndexOptions {
        let mut opts = self.index_options();
        opts.solidity = crate::adapters::foundry::read_layout(root, &opts.skip_dirs);
        opts.skip_paths = opts.solidity.skip_paths();
        opts
    }

    fn index_deps(&self) -> application::index::Deps<'_> {
        application::index::Deps {
            store: &self.store,
            communities: &self.store,
            embedder: self.embedder.as_ref(),
            extractor: &self.extractor,
            vcs: &self.vcs,
        }
    }

    fn resolve_root(&self, repo: Option<&str>) -> Result<PathBuf> {
        Project::resolve_repo(self.project.as_ref(), repo)
    }

    async fn indexed_repo(&self, root: &Path) -> Result<crate::domain::model::Repo> {
        let identity = identify(root)?;
        self.store
            .find_repo(&identity.root_path)
            .await?
            .with_context(|| {
                format!(
                    "repository {} is not indexed yet — run `index` first",
                    identity.root_path
                )
            })
    }

    /// Dispatch one of the six tools by bare name with JSON arguments
    /// (the MCP `tools/call` shape; the CLI builds the same arguments).
    pub async fn call_tool(&self, name: &str, args: &Value) -> Result<Value> {
        // Checked per call, not at connect, so `chaos mcp` still starts and
        // every tool answers with the next command to run.
        let pending = self.store.pending_migrations().await?;
        anyhow::ensure!(
            pending.is_empty(),
            "the database schema is older than this binary (migrations {pending:?} not applied); run `chaos migrate`"
        );
        let repo_arg = args.get("repo").and_then(Value::as_str);
        match name {
            "index" => {
                let root = self.resolve_root(repo_arg)?;
                let mut opts = self.index_options_for(&root);
                opts.dry_run = args
                    .get("dry_run")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let report = application::index::index(self.index_deps(), &root, opts).await?;
                Ok(serde_json::to_value(report)?)
            }
            "reindex" => {
                let root = self.resolve_root(repo_arg)?;
                let mut opts = self.index_options_for(&root);
                opts.force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
                let report = application::index::reindex(self.index_deps(), &root, opts).await?;
                Ok(serde_json::to_value(report)?)
            }
            "add" => {
                let root = self.resolve_root(repo_arg)?;
                let since = args
                    .get("since")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty());
                let opts = self.index_options_for(&root);
                let report = application::add::add(self.index_deps(), &root, since, opts).await?;
                Ok(serde_json::to_value(report)?)
            }
            "feature" => {
                let question = args
                    .get("question")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|q| !q.is_empty())
                    .context("`question` is required")?;
                let mut req = application::feature::FeatureRequest::new(question);
                if let Some(b) = args.get("budget_tokens").and_then(Value::as_u64) {
                    req.budget_tokens = b as usize;
                }
                if let Some(p) = args.get("persist").and_then(Value::as_bool) {
                    req.persist = p;
                }
                req.entry = args
                    .get("entry")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|e| !e.is_empty())
                    .map(str::to_string);
                let deps = application::feature::Deps {
                    search: &self.store,
                    communities: &self.store,
                    embedder: self.embedder.as_ref(),
                    pages: &self.pages,
                };
                let only = string_list(args, "repos");
                let all_repos = args
                    .get("all_repos")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if all_repos || !only.is_empty() {
                    let (project, project_dir) = self.project_arg(args)?;
                    if all_repos && project.is_none() {
                        bail!("`all_repos` needs a project: no chaos.toml found (pass `project`)");
                    }
                    let members = select_members(project.as_ref(), &only)?;
                    let mut selected = Vec::with_capacity(members.len());
                    for (alias, root) in members {
                        let repo = self.indexed_repo(&root).await?;
                        selected.push(application::project_feature::Member { alias, repo, root });
                    }
                    let name = project
                        .as_ref()
                        .map(|p| p.name.clone())
                        .unwrap_or_else(|| selected[0].alias.clone());
                    let out_dir = features_dir(project_dir.as_deref(), &selected[0].root);
                    let answer = application::project_feature::project_feature(
                        deps, &name, &selected, &out_dir, req,
                    )
                    .await?;
                    return Ok(serde_json::to_value(answer)?);
                }
                let root = self.resolve_root(repo_arg)?;
                let repo = self.indexed_repo(&root).await?;
                let answer = application::feature::feature(deps, &repo, &root, req).await?;
                Ok(serde_json::to_value(answer)?)
            }
            "compose" => {
                let only = string_list(args, "repos");
                let all_repos = args
                    .get("all_repos")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if all_repos || !only.is_empty() {
                    let report = self.compose_project(args, &only, all_repos).await?;
                    return Ok(serde_json::to_value(report)?);
                }
                let root = self.resolve_root(repo_arg)?;
                let repo = self.indexed_repo(&root).await?;
                let out = args.get("out").and_then(Value::as_str).map(PathBuf::from);
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|l| l as usize);
                let deps = application::compose::Deps {
                    store: &self.store,
                    communities: &self.store,
                    search: &self.store,
                    pages: &self.pages,
                };
                let report = application::compose::compose(deps, &repo, &root, out, limit).await?;
                Ok(serde_json::to_value(report)?)
            }
            "correlations" => {
                let min_confidence = args
                    .get("min_confidence")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.5) as f32;
                let only = string_list(args, "repos");
                let (project, project_dir) = self.project_arg(args)?;
                let report = self
                    .correlations(project, project_dir, &only, min_confidence)
                    .await?;
                Ok(serde_json::to_value(report)?)
            }
            other => bail!(
                "unknown tool `{other}` (expected one of {})",
                TOOL_NAMES.join(", ")
            ),
        }
    }

    /// `compose` across project members: each member's feature site, the
    /// correlations report, then the project hub page.
    async fn compose_project(
        &self,
        args: &Value,
        only: &[String],
        all_repos: bool,
    ) -> Result<application::project_compose::ProjectComposeReport> {
        if args.get("out").is_some_and(|o| !o.is_null()) {
            bail!("`out` applies to one repository; across repos each site goes to <repo>/docs/features_memory/site and the hub to <project>/docs/features_memory");
        }
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|l| l as usize);
        let (project, project_dir) = self.project_arg(args)?;
        if all_repos && project.is_none() {
            bail!("`all_repos` needs a project: no chaos.toml found (pass `project`)");
        }
        let selected = select_members(project.as_ref(), only)?;
        let mut members = Vec::with_capacity(selected.len());
        for (alias, root) in &selected {
            let repo = self.indexed_repo(root).await?;
            let deps = application::compose::Deps {
                store: &self.store,
                communities: &self.store,
                search: &self.store,
                pages: &self.pages,
            };
            let site = application::compose::compose(deps, &repo, root, None, limit).await?;
            members.push(application::project_compose::Member {
                alias: alias.clone(),
                repo,
                root: root.clone(),
                site,
            });
        }
        let correlations = self
            .correlations(project.clone(), project_dir.clone(), only, 0.5)
            .await?;
        let out_dir = features_dir(project_dir.as_deref(), &selected[0].1);
        let questions = question_pages(&self.pages, &out_dir);
        let name = project
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| selected[0].0.clone());
        let deps = application::project_compose::Deps {
            store: &self.store,
            communities: &self.store,
            pages: &self.pages,
        };
        application::project_compose::compose_project(
            deps,
            &name,
            &members,
            &correlations,
            &questions,
            &out_dir,
        )
        .await
    }

    /// The project named by the `project` argument, else the one resolved at startup.
    fn project_arg(&self, args: &Value) -> Result<(Option<Project>, Option<PathBuf>)> {
        match args.get("project").and_then(Value::as_str) {
            Some(p) => load_project(Some(Path::new(p))),
            None => Ok((self.project.clone(), self.project_dir.clone())),
        }
    }

    async fn correlations(
        &self,
        project: Option<Project>,
        project_dir: Option<PathBuf>,
        only: &[String],
        min_confidence: f32,
    ) -> Result<application::correlations::CorrelationReport> {
        let members = select_members(project.as_ref(), only)?;
        let mut repos = Vec::new();
        let mut manifests = Vec::new();
        let mut unindexed = Vec::new();
        for (alias, root) in &members {
            let identity = identify(root)?;
            match self.store.find_repo(&identity.root_path).await? {
                Some(repo) => {
                    for name in package_names(&identity.root) {
                        manifests.push((alias.clone(), name));
                    }
                    repos.push(application::correlations::RepoRef {
                        alias: alias.clone(),
                        repo,
                    });
                }
                None => unindexed.push(format!("{alias} ({})", identity.root_path)),
            }
        }
        if !unindexed.is_empty() {
            bail!(
                "repositories not indexed yet — run `index` first: {}",
                unindexed.join(", ")
            );
        }
        let out_dir = features_dir(project_dir.as_deref(), &members[0].1);
        let deps = application::correlations::Deps {
            store: &self.store,
            communities: &self.store,
            pages: &self.pages,
        };
        application::correlations::correlations(&deps, &repos, &manifests, &out_dir, min_confidence)
            .await
    }
}

/// Project feature pages (one question across repos) directly in `dir`.
fn question_pages(
    pages: &dyn PageStore,
    dir: &Path,
) -> Vec<application::project_compose::QuestionPage> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<_> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "html"))
        .filter_map(|path| {
            let manifest = pages.read_manifest(&path)?;
            let (title, repos) = application::project_compose::question_page(&manifest)?;
            Some(application::project_compose::QuestionPage { title, path, repos })
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// A JSON string-array argument (missing or malformed → empty).
fn string_list(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Where project-level pages go: `<project dir>/docs/features_memory`, or the
/// first member's features directory when there is no chaos.toml.
fn features_dir(project_dir: Option<&Path>, first_root: &Path) -> PathBuf {
    project_dir
        .unwrap_or(first_root)
        .join(application::feature::FEATURES_DIR)
}

/// Members of a multi-repo call: the project's repos (filtered by `only`,
/// aliases or paths), or — without a chaos.toml — `only` (or the cwd) as paths.
fn select_members(project: Option<&Project>, only: &[String]) -> Result<Vec<(String, PathBuf)>> {
    let mut members: Vec<(String, PathBuf)> = Vec::new();
    match project {
        Some(p) => {
            for entry in &p.repos {
                let selected = only.is_empty()
                    || only
                        .iter()
                        .any(|o| o == &entry.alias || Path::new(o) == Path::new(&entry.path));
                if selected {
                    members.push((entry.alias.clone(), PathBuf::from(&entry.path)));
                }
            }
            for o in only {
                if !members.iter().any(|(a, _)| a == o)
                    && p.repos.iter().all(|e| Path::new(o) != Path::new(&e.path))
                {
                    let root = Project::resolve_repo(Some(p), Some(o))?;
                    let alias = root
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| o.clone());
                    members.push((alias, root));
                }
            }
        }
        None => {
            let targets: Vec<Option<&str>> = if only.is_empty() {
                vec![None]
            } else {
                only.iter().map(|s| Some(s.as_str())).collect()
            };
            for t in targets {
                let root = Project::resolve_repo(None, t)?;
                let alias = root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "repo".into());
                members.push((alias, root));
            }
        }
    }
    if members.is_empty() {
        bail!("no repositories selected");
    }
    Ok(members)
}

/// `--project <chaos.toml>` or the nearest chaos.toml above the cwd.
fn load_project(explicit: Option<&Path>) -> Result<(Option<Project>, Option<PathBuf>)> {
    let path = match explicit {
        Some(p) => {
            let p = if p.is_dir() {
                p.join(crate::adapters::config::PROJECT_FILE)
            } else {
                p.to_path_buf()
            };
            if !p.is_file() {
                bail!("project file {} not found", p.display());
            }
            Some(p)
        }
        None => env::current_dir().ok().and_then(|cwd| Project::find(&cwd)),
    };
    match path {
        Some(p) => {
            // Absolute, so pages written under the project dir report real paths.
            let p = p.canonicalize().unwrap_or(p);
            let project = Project::load(&p)?;
            let dir = p
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            Ok((Some(project), Some(dir)))
        }
        None => Ok((None, None)),
    }
}

/// Package names a repository publishes: `package.json` `name` (+ workspaces)
/// and `Cargo.toml` `[package] name` (+ `[workspace] members`). Sorted, deduped.
pub fn package_names(root: &Path) -> Vec<String> {
    let mut names = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    let mut seen = std::collections::HashSet::new();
    while let Some(dir) = dirs.pop() {
        if !seen.insert(dir.clone()) {
            continue;
        }
        if let Ok(text) = fs::read_to_string(dir.join("package.json")) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(n) = v.get("name").and_then(Value::as_str) {
                    names.push(n.to_string());
                }
                let ws = v.get("workspaces");
                let globs: Vec<String> = match ws {
                    Some(Value::Array(a)) => a
                        .iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect(),
                    Some(Value::Object(o)) => o
                        .get("packages")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(String::from)
                                .collect()
                        })
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
                dirs.extend(expand_members(&dir, &globs));
            }
        }
        if let Ok(text) = fs::read_to_string(dir.join("Cargo.toml")) {
            if let Ok(v) = toml::from_str::<toml::Value>(&text) {
                if let Some(n) = v
                    .get("package")
                    .and_then(|p| p.get("name"))
                    .and_then(toml::Value::as_str)
                {
                    names.push(n.to_string());
                }
                let globs: Vec<String> = v
                    .get("workspace")
                    .and_then(|w| w.get("members"))
                    .and_then(toml::Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(toml::Value::as_str)
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                dirs.extend(expand_members(&dir, &globs));
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Expand workspace member patterns: plain paths, or a trailing `*` /`**`
/// component listing the sub-directories of its parent.
fn expand_members(base: &Path, globs: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for g in globs {
        let g = g.trim_end_matches('/');
        if g.starts_with('!') {
            continue;
        }
        match g.rsplit_once('/') {
            Some((parent, last)) if last == "*" || last == "**" => {
                if let Ok(rd) = fs::read_dir(base.join(parent)) {
                    let mut subs: Vec<PathBuf> = rd
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| p.is_dir())
                        .collect();
                    subs.sort();
                    out.extend(subs);
                }
            }
            _ if g == "*" => {
                if let Ok(rd) = fs::read_dir(base) {
                    let mut subs: Vec<PathBuf> = rd
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| p.is_dir())
                        .collect();
                    subs.sort();
                    out.extend(subs);
                }
            }
            _ if !g.contains('*') => out.push(base.join(g)),
            _ => {}
        }
    }
    out
}

/// The agent guide printed by `chaos help` and sent as MCP `instructions`.
pub fn agent_guide() -> String {
    let mut s = format!(
        "Chaos Substrate {} — persistent code knowledge memory (six tools).\n\
         Each tool is a CLI subcommand (`chaos <name>`) and an MCP tool (`chaos_<name>`).\n\n",
        env!("CARGO_PKG_VERSION")
    );
    for t in tools() {
        s.push_str(&format!(
            "{} / chaos {}\n  {}\n\n",
            mcp_name(t.name),
            t.name,
            t.description
        ));
    }
    s.push_str(
        "Workflow:\n\
         1. `index` a repository once (needs Postgres + a real embedder; `doctor` checks both).\n\
         2. After every edit, commit, pull or checkout run `add` — it re-reads only the git delta since the last stamped run.\n\
         3. Ask `feature \"<question>\"` to answer from the index; read the returned excerpts instead of source files. To follow a GraphQL field, name it (`Mutation.createLab`) or pass `entry` (CLI `--entry`).\n\
         4. `compose` writes the clickable feature website (index with the entry points + one page per feature, hash-gated); `--all-repos` adds the project hub.\n\
         5. `correlations` links features across the repos listed in chaos.toml (package_dep / http_route / graphql_field / contract_abi).\n\
         6. `reindex` re-reads that delta plus the files that import or reference it; a no-op when HEAD is the stamp and nothing is uncommitted; `--force` for a full run. After upgrading Chaos, `reindex --force` every repo: the delta never re-extracts files whose content did not change.\n",
    );
    s
}

/// Fenced-code language for an excerpt path.
fn fence_lang(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "rs" => "rust",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "sol" => "solidity",
        "graphql" | "gql" | "graphqls" => "graphql",
        "md" | "mdx" => "markdown",
        _ => "",
    }
}

/// Markdown rendering of a project-wide `feature` answer: one
/// [`feature_markdown`] block per repo (retitled by alias), then the links.
pub fn project_feature_markdown(answer: &Value) -> String {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let repos = answer
        .get("repos")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let links = answer
        .get("links")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = format!(
        "# {}

project: {}  ·  repos: {}  ·  tokens: {}",
        s(answer, "question"),
        s(answer, "project"),
        repos
            .iter()
            .map(|r| s(r, "alias"))
            .collect::<Vec<_>>()
            .join(", "),
        answer
            .get("tokens_used")
            .and_then(Value::as_u64)
            .unwrap_or(0)
    );
    if let Some(p) = answer.get("page").and_then(Value::as_str) {
        let cached = answer
            .get("page_cached")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        out.push_str(&format!(
            "  ·  page: {p}{}",
            if cached { " (cached)" } else { "" }
        ));
    }
    out.push_str("\n\n");
    if let Some(e) = answer.get("entry").filter(|e| e.is_object()) {
        out.push_str(&entry_markdown(e));
        let links = answer
            .get("entry_links")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for l in &links {
            out.push_str(&format!(
                "- {} `{}` ({}) → {} `{}` ({}) — field `{}`\n",
                s(l, "from_repo"),
                s(l, "from"),
                s(l, "from_path"),
                s(l, "to_repo"),
                s(l, "to"),
                s(l, "to_path"),
                s(l, "term")
            ));
        }
        if !links.is_empty() {
            out.push('\n');
        }
    }
    for r in &repos {
        // The joined entry above replaces each member's own.
        let mut r = r.clone();
        if let Some(o) = r.as_object_mut() {
            o.remove("entry");
        }
        let body = feature_markdown(&r);
        let body = body.split_once('\n').map(|(_, rest)| rest).unwrap_or("");
        out.push_str(&format!("---\n\n# [{}]\n{body}\n", s(&r, "alias")));
    }
    out.push_str("## Cross-repo links\n\n");
    if links.is_empty() {
        out.push_str("_none found_\n");
    }
    for l in &links {
        out.push_str(&format!(
            "- {} `{}` ({}) → {} `{}` ({}) — name `{}`\n",
            s(l, "from_repo"),
            s(l, "from"),
            s(l, "from_path"),
            s(l, "to_repo"),
            s(l, "to"),
            s(l, "to_path"),
            s(l, "term")
        ));
    }
    out
}

/// `## Entry: <coordinates>` block of a GraphQL-anchored answer: schema,
/// registration, resolver, call trace, callers. Rows of a project answer
/// carry their member as `[repo]`.
fn entry_markdown(entry: &Value) -> String {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
    let list = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let words = |v: &Value, k: &str| -> Vec<String> {
        list(v, k)
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    };
    let repo = |v: &Value| match s(v, "repo") {
        r if r.is_empty() => String::new(),
        r => format!("[{r}] "),
    };
    let mut out = format!("## Entry: {}\n\n", words(entry, "coordinates").join(", "));
    for f in list(entry, "sdl") {
        out.push_str(&format!(
            "- schema: `{}: {}` — {}:{}\n",
            s(&f, "coordinate"),
            s(&f, "header"),
            s(&f, "path"),
            n(&f, "line")
        ));
    }
    let sdl_repos = words(entry, "sdl_repos");
    if !sdl_repos.is_empty() {
        out.push_str(&format!("- schema: SDL in {}\n", sdl_repos.join(", ")));
    }
    for b in list(entry, "bindings") {
        let lambda = words(&b, "lambda_entries");
        out.push_str(&format!(
            "- registered: {}`{}` — {}:{}{}\n",
            repo(&b),
            s(&b, "symbol"),
            s(&b, "path"),
            n(&b, "line"),
            if lambda.is_empty() {
                String::new()
            } else {
                format!(" → {}", lambda.join(", "))
            }
        ));
    }
    let resolvers = list(entry, "resolvers");
    if resolvers.is_empty() {
        out.push_str("- resolver: _none found_ (bound at deploy time, or in another repository)\n");
    }
    for r in &resolvers {
        let lines = list(r, "lines");
        let at = |i: usize| lines.get(i).and_then(Value::as_i64).unwrap_or(0);
        out.push_str(&format!(
            "- resolver: {}`{}` — {}:{}-{} (confidence {:.1})\n",
            repo(r),
            s(r, "symbol"),
            s(r, "path"),
            at(0),
            at(1),
            r.get("confidence").and_then(Value::as_f64).unwrap_or(0.0)
        ));
    }
    for t in list(entry, "trace") {
        out.push_str(&format!(
            "- calls (hop {}): `{}` → `{}` ({})\n",
            n(&t, "hop"),
            s(&t, "from"),
            s(&t, "to"),
            s(&t, "to_path")
        ));
    }
    for c in list(entry, "callers") {
        let hooks = words(&c, "hooks");
        out.push_str(&format!(
            "- called by: {}`{}` — {}:{}{}\n",
            repo(&c),
            s(&c, "op"),
            s(&c, "path"),
            n(&c, "line"),
            if hooks.is_empty() {
                String::new()
            } else {
                format!(" (hooks {})", hooks.join(", "))
            }
        ));
    }
    out.push('\n');
    out
}

/// Markdown rendering of a `feature` answer (features, then excerpts as fenced code).
pub fn feature_markdown(answer: &Value) -> String {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut out = format!("# {}\n\n", s(answer, "question"));
    out.push_str(&format!(
        "repo: {}  ·  tokens: {}",
        s(answer, "repo"),
        answer
            .get("tokens_used")
            .and_then(Value::as_u64)
            .unwrap_or(0)
    ));
    if let Some(p) = answer.get("page").and_then(Value::as_str) {
        out.push_str(&format!(
            "  ·  page: {p}{}",
            if answer
                .get("page_cached")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                " (cached)"
            } else {
                ""
            }
        ));
    }
    out.push_str("\n\n");
    if let Some(e) = answer.get("entry").filter(|e| e.is_object()) {
        out.push_str(&entry_markdown(e));
    }
    out.push_str("## Features\n\n");
    let features = answer
        .get("features")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if features.is_empty() {
        out.push_str("_none matched_\n");
    }
    for f in &features {
        let score = f.get("score").and_then(Value::as_f64).unwrap_or(0.0);
        out.push_str(&format!("- **{}** ({:.2})", s(f, "label"), score));
        let summary = s(f, "summary");
        if !summary.is_empty() {
            out.push_str(&format!(" — {summary}"));
        }
        out.push('\n');
        let files: Vec<&str> = f
            .get("files")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if !files.is_empty() {
            out.push_str(&format!("  files: {}\n", files.join(", ")));
        }
    }
    out.push_str("\n## Excerpts\n\n");
    for e in answer
        .get("excerpts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let path = s(&e, "path");
        let lines = e
            .get("lines")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let (l0, l1) = (
            lines.first().and_then(Value::as_i64).unwrap_or(0),
            lines.get(1).and_then(Value::as_i64).unwrap_or(0),
        );
        let why: Vec<&str> = e
            .get("why")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        out.push_str(&format!(
            "### {path}:{l0}-{l1} — {} ({}) [{}]\n\n```{}\n{}\n```\n{}\n",
            s(&e, "symbol"),
            s(&e, "kind"),
            why.join(", "),
            fence_lang(&path),
            s(&e, "code").trim_end(),
            if e.get("code_truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                "_(truncated)_\n"
            } else {
                ""
            }
        ));
    }
    let graph = answer
        .get("graph")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !graph.is_empty() {
        out.push_str("## Graph\n\n");
        for g in &graph {
            out.push_str(&format!(
                "- {} —{}→ {}\n",
                s(g, "from"),
                s(g, "kind"),
                s(g, "to")
            ));
        }
        out.push('\n');
    }
    let surfaces = answer
        .get("surfaces")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !surfaces.is_empty() {
        out.push_str("## Surfaces\n\n");
        for x in &surfaces {
            out.push_str(&format!(
                "- {} `{}` {}:{}\n",
                s(x, "kind"),
                s(x, "name"),
                s(x, "path"),
                x.get("line").and_then(Value::as_i64).unwrap_or(0)
            ));
        }
        out.push('\n');
    }
    let warnings = answer
        .get("warnings")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !warnings.is_empty() {
        out.push_str("## Warnings\n\n");
        for w in &warnings {
            out.push_str(&format!("- {}\n", w.as_str().unwrap_or("")));
        }
    }
    out
}

fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

/// Parse the command line, build one tokio runtime and dispatch.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    if matches!(cli.command, Command::Help) {
        print!("{}", agent_guide());
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?;
    runtime.block_on(run_command(cli))
}

async fn run_command(cli: Cli) -> Result<()> {
    let config_path = cli.config.as_deref();
    let project_path = cli.project.as_deref();
    match cli.command {
        Command::Help => {
            print!("{}", agent_guide());
            Ok(())
        }
        Command::Doctor => {
            let ok = super::doctor::run(config_path).await?;
            if !ok {
                std::process::exit(2);
            }
            Ok(())
        }
        Command::Migrate { reset } => {
            let config = Config::load(config_path)?;
            let store = Postgres::connect(&config.storage.database_url).await?;
            store.migrate(reset).await?;
            println!(
                "migrations applied{}",
                if reset { " (schema reset)" } else { "" }
            );
            Ok(())
        }
        Command::Mcp => {
            let services = Services::connect(config_path, project_path).await?;
            super::mcp::serve(services).await
        }
        Command::Index { dry_run } => {
            let services = Services::connect(config_path, project_path).await?;
            let v = services
                .call_tool("index", &json!({"repo": cli.repo, "dry_run": dry_run}))
                .await?;
            print_json(&v)
        }
        Command::Reindex { force } => {
            let services = Services::connect(config_path, project_path).await?;
            let v = services
                .call_tool("reindex", &json!({"repo": cli.repo, "force": force}))
                .await?;
            print_json(&v)
        }
        Command::Add { since } => {
            let services = Services::connect(config_path, project_path).await?;
            let v = services
                .call_tool("add", &json!({"repo": cli.repo, "since": since}))
                .await?;
            print_json(&v)
        }
        Command::Feature {
            question,
            budget,
            no_persist,
            json: as_json,
            repos,
            all_repos,
            entry,
        } => {
            let services = Services::connect(config_path, project_path).await?;
            let args = json!({"repo": cli.repo, "question": question, "budget_tokens": budget,
                "persist": !no_persist, "repos": repos, "all_repos": all_repos, "entry": entry});
            let v = services.call_tool("feature", &args).await?;
            if as_json {
                print_json(&v)
            } else if v.get("repos").is_some() {
                print!("{}", project_feature_markdown(&v));
                Ok(())
            } else {
                print!("{}", feature_markdown(&v));
                Ok(())
            }
        }
        Command::Compose {
            out,
            limit,
            repos,
            all_repos,
        } => {
            let services = Services::connect(config_path, project_path).await?;
            let args = json!({"repo": cli.repo, "out": out.map(|p| p.to_string_lossy().into_owned()), "limit": limit,
                "repos": repos, "all_repos": all_repos});
            let v = services.call_tool("compose", &args).await?;
            print_json(&v)
        }
        Command::Correlations {
            min_confidence,
            repos,
        } => {
            let services = Services::connect(config_path, project_path).await?;
            let mut repos = repos;
            if repos.is_empty() {
                if let Some(r) = cli.repo.clone() {
                    repos.push(r);
                }
            }
            let args = json!({"repos": repos, "min_confidence": min_confidence});
            let v = services.call_tool("correlations", &args).await?;
            print_json(&v)
        }
    }
}

fn print_json(v: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn every_tool_has_a_cli_subcommand() {
        let cmd = Cli::command();
        let subs: Vec<String> = cmd
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        for name in TOOL_NAMES {
            assert!(
                subs.contains(&name.to_string()),
                "missing subcommand `{name}` in {subs:?}"
            );
        }
        for plumbing in ["migrate", "doctor", "mcp", "help"] {
            assert!(subs.contains(&plumbing.to_string()), "missing `{plumbing}`");
        }
    }

    #[test]
    fn cli_parses_feature_flags() {
        let cli = Cli::try_parse_from([
            "chaos",
            "--repo",
            "api",
            "feature",
            "how?",
            "--budget",
            "2000",
            "--no-persist",
            "--json",
            "--entry",
            "Mutation.createLab",
        ])
        .unwrap();
        assert_eq!(cli.repo.as_deref(), Some("api"));
        match cli.command {
            Command::Feature {
                question,
                budget,
                no_persist,
                json,
                entry,
                ..
            } => {
                assert_eq!(question, "how?");
                assert_eq!(budget, 2000);
                assert!(no_persist && json);
                assert_eq!(entry.as_deref(), Some("Mutation.createLab"));
            }
            other => panic!("unexpected {other:?}"),
        }
        let cli = Cli::try_parse_from([
            "chaos",
            "feature",
            "byot?",
            "--repos",
            "contracts",
            "web",
            "--all-repos",
        ])
        .unwrap();
        match cli.command {
            Command::Feature {
                repos, all_repos, ..
            } => {
                assert_eq!(repos, ["contracts", "web"]);
                assert!(all_repos);
            }
            other => panic!("unexpected {other:?}"),
        }
        let cli = Cli::try_parse_from([
            "chaos",
            "correlations",
            "--repos",
            "a",
            "b",
            "--min-confidence",
            "0.7",
        ])
        .unwrap();
        match cli.command {
            Command::Correlations {
                repos,
                min_confidence,
            } => {
                assert_eq!(repos, vec!["a", "b"]);
                assert!((min_confidence - 0.7).abs() < 1e-6);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn guide_mentions_every_tool() {
        let guide = agent_guide();
        for name in TOOL_NAMES {
            assert!(guide.contains(&mcp_name(name)), "guide lacks {name}");
        }
        assert!(guide.contains("Workflow:"));
    }

    #[test]
    fn package_names_reads_manifests_and_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("package.json"),
            r#"{"name":"@acme/root","workspaces":["packages/*"]}"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("packages/ui")).unwrap();
        fs::write(
            root.join("packages/ui/package.json"),
            r#"{"name":"@acme/ui"}"#,
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/core\"]\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("crates/core")).unwrap();
        fs::write(
            root.join("crates/core/Cargo.toml"),
            "[package]\nname = \"acme-core\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        assert_eq!(
            package_names(root),
            vec!["@acme/root", "@acme/ui", "acme-core"]
        );
    }

    #[test]
    fn feature_markdown_renders_excerpts_as_fences() {
        let v = json!({
            "question": "q", "repo": "r", "tokens_used": 12, "page": "docs/features_memory/q.html",
            "features": [{"label": "Storage", "score": 0.5, "summary": "s", "files": ["a.rs"]}],
            "excerpts": [{"path": "src/a.rs", "lines": [1, 3], "symbol": "f", "kind": "function", "why": ["semantic"], "code": "fn f() {}", "code_truncated": false}],
            "graph": [], "surfaces": [], "warnings": []
        });
        let md = feature_markdown(&v);
        assert!(md.contains("### src/a.rs:1-3 — f (function) [semantic]"));
        assert!(md.contains("```rust\nfn f() {}\n```"));
        assert!(md.contains("- **Storage** (0.50) — s"));
        assert!(!md.contains("## Entry"));
    }

    #[test]
    fn feature_markdown_renders_entry() {
        let v = json!({
            "question": "Mutation.createLab", "repo": "desci-infra", "tokens_used": 12,
            "features": [], "excerpts": [], "graph": [], "surfaces": [], "warnings": [],
            "entry": {
                "coordinates": ["Mutation.createLab"],
                "sdl": [{"coordinate": "Mutation.createLab", "path": "src/appsync/labs.graphql", "line": 40,
                         "header": "createLab(input: CreateLabInput!): Lab!"}],
                "bindings": [{"coordinate": "Mutation.createLab", "symbol": "AppSyncLabsLambdaConfig::getResolvers",
                              "path": "lib/labs/config.ts", "line": 666,
                              "lambda_entries": ["lambda/appsync-resolver-labs-lambda/index.ts"]}],
                "resolvers": [{"coordinate": "Mutation.createLab", "symbol": "originalHandler::createLab",
                               "path": "lambda/appsync-resolver-labs-lambda/index.ts", "lines": [13, 20], "confidence": 0.9}],
                "trace": [{"from": "originalHandler::createLab", "kind": "calls", "to": "DataRoomResolvers::handleCreateLab",
                           "from_path": "lambda/appsync-resolver-labs-lambda/index.ts",
                           "to_path": "lambda/appsync-resolver-labs-lambda/resolvers.ts", "hop": 1, "confidence": 1.0}],
                "callers": []
            }
        });
        let md = feature_markdown(&v);
        let entry = md
            .find("## Entry: Mutation.createLab")
            .expect("entry heading");
        assert!(entry < md.find("## Features").unwrap(), "the anchor leads");
        assert!(md.contains(
            "- registered: `AppSyncLabsLambdaConfig::getResolvers` — lib/labs/config.ts:666 → lambda/appsync-resolver-labs-lambda/index.ts"
        ));
        assert!(md.contains(
            "- resolver: `originalHandler::createLab` — lambda/appsync-resolver-labs-lambda/index.ts:13-20 (confidence 0.9)"
        ));
        assert!(md.contains(
            "- calls (hop 1): `originalHandler::createLab` → `DataRoomResolvers::handleCreateLab`"
        ));
        // Project answers render the joined entry once, with members and links.
        let p = json!({
            "question": "Mutation.createLab", "project": "molecule", "tokens_used": 1,
            "repos": [{"alias": "web", "question": "q", "repo": "web", "entry": {"coordinates": ["Mutation.createLab"]}}],
            "links": [],
            "entry": {"coordinates": ["Mutation.createLab"], "sdl_repos": ["api"], "bindings": [], "resolvers": [],
                      "callers": [{"repo": "web", "coordinate": "Mutation.createLab", "op": "CreateLab",
                                   "path": "apps/web/hooks/useCreateLab.ts", "line": 4, "hooks": ["useCreateLab"]}]},
            "entry_links": [{"from_repo": "web", "from": "CreateLab", "from_path": "apps/web/hooks/useCreateLab.ts",
                             "to_repo": "api", "to": "originalHandler::createLab",
                             "to_path": "lambda/appsync-resolver-labs-lambda/index.ts", "term": "Mutation.createLab"}]
        });
        let md = project_feature_markdown(&p);
        assert_eq!(md.matches("## Entry:").count(), 1);
        assert!(md.contains("- resolver: _none found_"));
        assert!(md.contains(
            "- called by: [web] `CreateLab` — apps/web/hooks/useCreateLab.ts:4 (hooks useCreateLab)"
        ));
        assert!(md.contains(
            "- web `CreateLab` (apps/web/hooks/useCreateLab.ts) → api `originalHandler::createLab`"
        ));
    }
}
