//! Core data model. Pure data: no I/O, no async, no adapter types.
//!
//! Layering contract (enforced by `tests::layering` in `main.rs`):
//! `domain` depends on nothing inside the crate; `ports` on `domain`;
//! `application` on `domain` + `ports`; `adapters` implement ports; `entry`
//! wires adapters into application use-cases.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Generates `as_str` + `parse` for a unit enum from explicit variant ⇒ string
/// pairs — the strings persisted in Postgres and printed in chunk headers.
#[macro_export]
macro_rules! impl_str_conversions {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self { $(Self::$variant => $text,)+ }
            }
            pub fn parse(value: &str) -> Option<Self> {
                match value { $($text => Some(Self::$variant),)+ _ => None }
            }
            #[allow(dead_code)]
            pub const ALL: &'static [$name] = &[$($name::$variant,)+];
        }
    };
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Rust,
    TypeScript,
    JavaScript,
    Python,
    Solidity,
    GraphQL,
    Markdown,
    /// Timestamped subtitles (`.srt`, `.vtt`): a video or podcast transcript.
    Transcript,
    /// A contract ABI as JSON: `*.abi`, `*.abi.json`, or a `.json` file under an
    /// `abi/` or `abis/` directory (plain ABI array, Foundry or Hardhat artifact).
    Abi,
}

impl_str_conversions!(Language {
    Rust => "rust",
    TypeScript => "typescript",
    JavaScript => "javascript",
    Python => "python",
    Solidity => "solidity",
    GraphQL => "graphql",
    Markdown => "markdown",
    Transcript => "transcript",
    Abi => "abi",
});

impl Language {
    /// Detect a language from a file path (extension based). `None` = not indexed.
    pub fn from_path(path: &str) -> Option<Self> {
        let ext = path.rsplit('.').next()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Self::Rust,
            "ts" | "tsx" | "mts" | "cts" => Self::TypeScript,
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "py" => Self::Python,
            "sol" => Self::Solidity,
            "graphql" | "gql" | "graphqls" => Self::GraphQL,
            "md" | "mdx" => Self::Markdown,
            "srt" | "vtt" => Self::Transcript,
            "abi" => Self::Abi,
            "json" if is_abi_json_path(path) => Self::Abi,
            _ => return None,
        })
    }

    /// The one indexable-path rule, shared by the full file listing and the
    /// git delta of `add`/`reindex`: a supported extension and no path
    /// component that is `.git` or one of `skip_dirs`. `rel` is repo-relative
    /// with `/` separators.
    pub fn indexable(rel: &str, skip_dirs: &[String]) -> Option<Self> {
        if in_skipped_dir(rel, skip_dirs) {
            return None;
        }
        Self::from_path(rel)
    }
}

/// Whether any `/`-separated component of the repo-relative `rel` is `.git`
/// or one of `skip_dirs` (directory basenames never indexed).
pub fn in_skipped_dir(rel: &str, skip_dirs: &[String]) -> bool {
    rel.split('/')
        .any(|c| c == ".git" || skip_dirs.iter().any(|d| d == c))
}

/// Version of the indexable-path rule ([`Language::from_path`],
/// [`Language::indexable`]). Bump it with any change to which paths are
/// indexable: it is part of [`listing_rules`], so stamps written under the old
/// rule make the next `add` reconcile every file and the next `reindex` run in
/// full, and files the git delta never names enter or leave the index.
///
/// 2: `.abi` files and ABI JSON (`*.abi.json`, `.json` under `abi/` / `abis/`).
pub const INDEXABLE_RULES_VERSION: u32 = 2;

/// Fingerprint of the rules that decide which paths are indexable: the rule
/// version, `skip_dirs` and `skip_paths` (the repo-relative directories
/// skipped by path, e.g. the Foundry `libs`; order and duplicates ignored).
/// Stored with the stamp ([`Repo::listing_rules`]).
pub fn listing_rules(skip_dirs: &[String], skip_paths: &[String]) -> String {
    fn sorted(v: &[String]) -> Vec<&str> {
        let mut v: Vec<&str> = v.iter().map(String::as_str).collect();
        v.sort_unstable();
        v.dedup();
        v
    }
    let base = format!(
        "v{INDEXABLE_RULES_VERSION}:{}",
        crate::domain::hash::sha256_hex(&sorted(skip_dirs).join("\n"))
    );
    if skip_paths.is_empty() {
        return base;
    }
    format!(
        "{base}:{}",
        crate::domain::hash::sha256_hex(&sorted(skip_paths).join("\n"))
    )
}

/// ABI JSON discovery by path only: `*.abi.json`, or a directory segment named
/// `abi` / `abis` (case-insensitive). Every other `.json` stays unindexed.
fn is_abi_json_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let mut segs: Vec<&str> = lower.split('/').collect();
    let file = segs.pop().unwrap_or("");
    file.ends_with(".abi.json") || segs.iter().any(|s| *s == "abi" || *s == "abis")
}

/// Kinds of graph nodes. Language-neutral: a Solidity contract, a TS class and
/// a GraphQL object type are all `Type`; a Rust trait, TS interface and GraphQL
/// interface are all `Interface`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Repository,
    File,
    Module,
    Function,
    Method,
    Type,
    Interface,
    Enum,
    TypeAlias,
    /// GraphQL field definition; Solidity event, custom error, state
    /// variable or file-level constant.
    Field,
    /// GraphQL executable operation (query/mutation/subscription).
    Operation,
    /// GraphQL fragment.
    Fragment,
    /// Markdown heading section.
    Section,
    /// User-surface: environment variable read by a file.
    EnvVar,
    /// User-surface: HTTP route registered by a file.
    HttpRoute,
    /// User-surface: MCP tool a server registers (`tools/list` name).
    McpTool,
    /// The code unit that implements one GraphQL field: today one
    /// `case "<field>":` group of a dispatcher that switches on `fieldName`
    /// (an AppSync Lambda). Never a call or heritage target.
    Resolver,
    /// A contract ABI: a JSON ABI file, a TS/JS ABI constant, a `parseAbi` list.
    Abi,
}

impl_str_conversions!(NodeKind {
    Repository => "repository",
    File => "file",
    Module => "module",
    Function => "function",
    Method => "method",
    Type => "type",
    Interface => "interface",
    Enum => "enum",
    TypeAlias => "type_alias",
    Field => "field",
    Operation => "operation",
    Fragment => "fragment",
    Section => "section",
    EnvVar => "env_var",
    HttpRoute => "http_route",
    McpTool => "mcp_tool",
    Resolver => "resolver",
    Abi => "abi",
});

impl NodeKind {
    /// Kinds that carry code and get their own chunk.
    pub fn is_unit(&self) -> bool {
        matches!(
            self,
            Self::Function
                | Self::Method
                | Self::Type
                | Self::Interface
                | Self::Enum
                | Self::TypeAlias
                | Self::Operation
                | Self::Fragment
                | Self::Section
                | Self::Resolver
                | Self::Abi
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// file → symbol, container → member.
    Contains,
    /// file → file (resolved import).
    Imports,
    /// caller → callee (resolved, never fuzzy).
    Calls,
    /// type → base type / contract → parent.
    Extends,
    /// type → interface/trait.
    Implements,
    /// host function/file → embedded GraphQL operation; file → surface node.
    Defines,
    /// GraphQL operation → SDL root field it selects.
    Queries,
    /// Solidity function → event `Field` it emits (`emit E(...)`).
    Emits,
    /// Solidity function → custom error `Field` it reverts with
    /// (`revert E(...)`, `require(cond, E(...))`).
    Reverts,
    /// Resolver registration (an infrastructure `{typeName, fieldName}`
    /// literal, or the file holding it) → the SDL field it attaches a data
    /// source to.
    Binds,
    /// `Resolver` → the SDL field it implements.
    Resolves,
    /// Contract consumer (symbol or file) → the ABI definition it names.
    UsesAbi,
}

impl_str_conversions!(EdgeKind {
    Contains => "contains",
    Imports => "imports",
    Calls => "calls",
    Extends => "extends",
    Implements => "implements",
    Defines => "defines",
    Queries => "queries",
    Emits => "emits",
    Reverts => "reverts",
    Binds => "binds",
    Resolves => "resolves",
    UsesAbi => "uses_abi",
});

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    /// One AST unit (function/type/section) with its context header.
    Unit,
    /// A unit too large for the embedder, split contextually: `part i/n`.
    Part,
    /// Imports + top-level statements not owned by any unit.
    FileHead,
}

impl_str_conversions!(ChunkKind {
    Unit => "unit",
    Part => "part",
    FileHead => "file_head",
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Repo {
    pub id: Uuid,
    pub name: String,
    pub root_path: String,
    /// Stamp, part 1: the HEAD commit the last successful `index`, `reindex`
    /// or `add` read. `None` = never stamped (not a git repository, or an
    /// `index` that did not finish).
    pub commit_sha: Option<String>,
    /// Stamp, part 2: repo-relative paths that were uncommitted at that run
    /// (modified, added, deleted, both sides of a rename, untracked), so the
    /// next run also re-checks them after a revert or a commit. `None` = the
    /// stamp is incomplete: a run started writing the index and did not
    /// finish, or the row predates worktree tracking. The next `add` then
    /// reconciles every file and finishes embedding and the hierarchy; the
    /// next `reindex` runs in full.
    #[serde(default)]
    pub worktree_paths: Option<Vec<String>>,
    /// Stamp, part 3: [`listing_rules`] of that run. A different value means
    /// files git reports no change for may have entered or left the
    /// indexable set.
    #[serde(default)]
    pub listing_rules: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceFile {
    pub id: Uuid,
    pub repo_id: Uuid,
    pub path: String,
    pub language: Language,
    pub content_hash: String,
    pub line_count: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Node {
    pub id: Uuid,
    pub repo_id: Uuid,
    pub file_id: Option<Uuid>,
    pub kind: NodeKind,
    /// `{lang}:{path}:{kind}:{container::chain::name}`; `file:{path}`; `repo`.
    pub stable_id: String,
    pub name: String,
    pub signature: Option<String>,
    pub doc: Option<String>,
    pub line_start: Option<i32>,
    pub line_end: Option<i32>,
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Edge {
    pub id: Uuid,
    pub repo_id: Uuid,
    pub source_node_id: Uuid,
    pub target_node_id: Uuid,
    pub kind: EdgeKind,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Chunk {
    pub id: Uuid,
    pub repo_id: Uuid,
    pub file_id: Option<Uuid>,
    pub node_id: Option<Uuid>,
    pub kind: ChunkKind,
    /// Context header (file, container, kind/name, uses, calls, doc) — ≤ 700 chars.
    pub header: String,
    pub body: String,
    /// sha256 over `header + "\n" + body`.
    pub content_hash: String,
    pub line_start: Option<i32>,
    pub line_end: Option<i32>,
    pub metadata: Value,
}

impl Chunk {
    /// The text that gets embedded and searched.
    pub fn text(&self) -> String {
        format!("{}\n{}", self.header, self.body)
    }
}

/// An L1 community ("feature"): a Louvain cluster of nodes with a templated
/// summary whose embedding routes feature questions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Community {
    pub id: Uuid,
    pub repo_id: Uuid,
    pub label: String,
    pub member_count: i32,
    pub summary: Option<String>,
    /// [`crate::domain::summary::summary_hash`] of `summary`: the key its
    /// embedding is stored and reused under.
    pub summary_hash: Option<String>,
    /// Top member symbol names (weighted degree order).
    pub top_members: Vec<String>,
    /// Member file paths, sorted.
    pub files: Vec<String>,
}

/// Everything the index stores persist for one repository (or, for `add`, for
/// the changed subset of its files).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexBundle {
    pub files: Vec<SourceFile>,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub chunks: Vec<Chunk>,
}

/// A vector plus the identity of the model that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRef {
    pub provider: String,
    pub model_id: String,
    pub dimensions: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_from_path() {
        assert_eq!(Language::from_path("a/b.tsx"), Some(Language::TypeScript));
        assert_eq!(Language::from_path("x.gql"), Some(Language::GraphQL));
        assert_eq!(Language::from_path("Cargo.toml"), None);
    }

    #[test]
    fn language_from_path_abi() {
        for p in [
            "lambda/common/assets/abis/AccessResolver.json",
            "bin/src/AccessResolver.abi",
            "out/X.abi.json",
            "src/ABI/Token.json",
        ] {
            assert_eq!(Language::from_path(p), Some(Language::Abi), "{p}");
        }
        for p in [
            "package.json",
            "deployments/out/deployments.generated.json",
            "tsconfig.json",
            "abis.json",
        ] {
            assert_eq!(Language::from_path(p), None, "{p}");
        }
    }

    #[test]
    fn indexable_applies_language_and_skip_dirs() {
        let skip = vec!["node_modules".to_string(), "build".to_string()];
        assert_eq!(
            Language::indexable("src/a b/é.ts", &skip),
            Some(Language::TypeScript)
        );
        assert_eq!(Language::indexable("node_modules/x/i.js", &skip), None);
        assert_eq!(Language::indexable("pkg/build/out.ts", &skip), None);
        assert_eq!(Language::indexable(".git/hooks/x.py", &skip), None);
        assert_eq!(
            Language::indexable("builder/x.rs", &skip),
            Some(Language::Rust)
        );
        assert_eq!(Language::indexable("Cargo.toml", &skip), None);
        assert!(in_skipped_dir("a/build", &skip));
        assert!(!in_skipped_dir("a/building", &skip));
    }

    #[test]
    fn listing_rules_fingerprint_skip_dirs() {
        let a = listing_rules(&["build".into(), "node_modules".into()], &[]);
        assert!(a.starts_with(&format!("v{INDEXABLE_RULES_VERSION}:")));
        assert_eq!(
            a,
            listing_rules(
                &["node_modules".into(), "build".into(), "build".into()],
                &[]
            )
        );
        assert_ne!(a, listing_rules(&["build".into()], &[]));
        assert_ne!(listing_rules(&[], &[]), a);
        let dirs = ["build".to_string()];
        // Without skip_paths the fingerprint is the version plus one hash.
        assert_eq!(listing_rules(&dirs, &[]).matches(':').count(), 1);
        let lib = listing_rules(&dirs, &["lib".into()]);
        assert_ne!(lib, listing_rules(&dirs, &[]));
        assert_ne!(lib, listing_rules(&dirs, &["dependencies".into()]));
        assert_eq!(lib, listing_rules(&dirs, &["lib".into(), "lib".into()]));
    }

    /// Guards [`INDEXABLE_RULES_VERSION`]. When this fails, `from_path`
    /// changed which paths are indexable: bump the version and update the
    /// snapshot, so existing stamps fall back once and the files the change
    /// affects enter or leave the index.
    #[test]
    fn indexable_rules_version_matches_the_rule() {
        const PROBES: &str = "a.rs a.ts a.tsx a.mts a.cts a.d.ts a.js a.jsx a.mjs a.cjs a.py a.pyi a.sol a.vy a.graphql a.gql a.graphqls a.md a.mdx a.srt a.vtt a.abi a.abi.json abi/A.json abis/A.json out/A.sol/A.json a.json a.toml a.yml a.yaml a.txt a.rst a.adoc a.html a.css a.scss a.vue a.svelte a.go a.java a.kt a.swift a.c a.h a.cpp a.cs a.rb a.php a.sh a.sql a.proto a.ipynb a.move a.cairo a.lock a.svg Makefile Dockerfile A.RS a.MD";
        let snapshot: Vec<String> = PROBES
            .split_whitespace()
            .filter_map(|p| Language::from_path(p).map(|l| format!("{p}={}", l.as_str())))
            .collect();
        assert_eq!(
            (INDEXABLE_RULES_VERSION, snapshot.join(" ")),
            (2, "a.rs=rust a.ts=typescript a.tsx=typescript a.mts=typescript a.cts=typescript a.d.ts=typescript a.js=javascript a.jsx=javascript a.mjs=javascript a.cjs=javascript a.py=python a.sol=solidity a.graphql=graphql a.gql=graphql a.graphqls=graphql a.md=markdown a.mdx=markdown a.srt=transcript a.vtt=transcript a.abi=abi a.abi.json=abi abi/A.json=abi abis/A.json=abi A.RS=rust a.MD=markdown".to_string())
        );
    }

    #[test]
    fn str_roundtrip() {
        for k in NodeKind::ALL {
            assert_eq!(NodeKind::parse(k.as_str()), Some(*k));
        }
        for k in EdgeKind::ALL {
            assert_eq!(EdgeKind::parse(k.as_str()), Some(*k));
        }
    }

    #[test]
    fn resolver_kinds_round_trip() {
        assert_eq!(NodeKind::parse("resolver"), Some(NodeKind::Resolver));
        assert_eq!(NodeKind::Resolver.as_str(), "resolver");
        assert_eq!(EdgeKind::parse("binds"), Some(EdgeKind::Binds));
        assert_eq!(EdgeKind::Binds.as_str(), "binds");
        assert_eq!(EdgeKind::parse("resolves"), Some(EdgeKind::Resolves));
        assert_eq!(EdgeKind::Resolves.as_str(), "resolves");
    }

    #[test]
    fn resolver_is_unit() {
        assert!(NodeKind::Resolver.is_unit());
        assert!(!NodeKind::Field.is_unit());
    }
}
