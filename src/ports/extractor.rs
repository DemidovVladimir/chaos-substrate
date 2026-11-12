//! Port: per-file structural extraction (tree-sitter adapter implements it).
//!
//! Extraction is FILE-LOCAL and pure: it never sees other files. Imports and
//! call candidates are left unresolved; `application::analyze` resolves them
//! project-wide. Everything here is in source order and deterministic.

use crate::domain::model::{EdgeKind, Language, NodeKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct FileInput {
    /// Repo-relative path with `/` separators.
    pub path: String,
    pub language: Language,
    pub content: String,
}

/// A declared symbol. `stable_id` = `{lang}:{path}:{kind}:{container::chain::name}`
/// (`#2`, `#3` suffix for same-name duplicates in source order).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Symbol {
    pub stable_id: String,
    pub kind: NodeKind,
    pub name: String,
    /// `container::chain::name` (no lang/path prefix).
    pub qualified_name: String,
    /// stable_id of the enclosing symbol (impl/class/contract/type), if any.
    pub parent: Option<String>,
    /// First line of the declaration (e.g. `fn verify(&self, t: &str) -> bool`).
    pub signature: Option<String>,
    /// Leading doc comment / docstring, trimmed (≤ 1000 chars).
    pub doc: Option<String>,
    /// 1-based inclusive lines.
    pub line_start: u32,
    pub line_end: u32,
    /// Byte range in `FileInput.content`.
    pub byte_start: usize,
    pub byte_end: usize,
    /// Byte ranges of the direct child statements of the body block, in order —
    /// the candidate split points for oversize units. Empty when unknown.
    pub segments: Vec<(usize, usize)>,
    #[serde(default)]
    pub metadata: Value,
}

/// A raw import as written. Resolution happens in `application::analyze`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImportRef {
    /// `crate::a::b`, `./x`, `react`, `os.path`, `./IERC20.sol`.
    pub specifier: String,
    /// Imported names (`{ a, b }`, `use x::{a, b}`); empty for whole-module imports.
    pub names: Vec<String>,
    pub line: u32,
}

/// A call site whose target is not yet resolved.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CallCandidate {
    /// stable_id of the enclosing symbol (or the file's `file:{path}` id).
    pub caller: String,
    /// Bare callee name (`foo`, `bar` for `x.bar()`).
    pub callee: String,
    /// Receiver text when present (`self`, `Storage`, `this`, a variable name).
    pub receiver: Option<String>,
    pub line: u32,
}

/// A by-name relation between symbols (extends/implements/queries/binds/
/// resolves), resolved later.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Relation {
    pub source: String,
    pub kind: EdgeKind,
    /// Target NAME (not stable_id) — e.g. `IERC20`, `Query.user`. For
    /// `Binds` a GraphQL coordinate (`Mutation.createLab`); for `Resolves`
    /// either a coordinate (`Lab.members`, the dispatcher guards on the parent
    /// type) or a bare field name (`createLab`, the type is unknown at the
    /// dispatch site).
    pub target_name: String,
}

/// A GraphQL resolver registration found in infrastructure code: an object
/// literal whose `typeName` and `fieldName` are both literal strings (AppSync
/// `getResolvers()` entries, `createResolver(id, {typeName, fieldName})`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GraphqlBinding {
    /// `Type.field`, e.g. `Mutation.createLab`.
    pub coordinate: String,
    /// 1-based line of the object literal.
    pub line: u32,
    /// stable_id of the enclosing symbol; `None` at file level.
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceKind {
    EnvVar,
    HttpRoute,
    McpTool,
}

/// A user-facing surface found in the file (env var read, HTTP route
/// registered, MCP tool registered).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Surface {
    pub kind: SurfaceKind,
    /// `DATABASE_URL`, or `GET /users/:id` (method upper-case, path as written).
    pub name: String,
    pub line: u32,
    /// stable_id of the enclosing symbol, if any.
    pub owner: Option<String>,
    /// One line a reader needs next to the name (an MCP tool's title or
    /// description); never embedded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What a contract call site names: a function (`functionName`), an event
/// (`eventName`), or only an ABI (`decodeEventLog({abi})`, a dynamic
/// `functionName`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContractMemberKind {
    Function,
    Event,
    Abi,
}

/// Where a call site's ABI came from: its own object (`{abi, functionName}`)
/// or the single ABI named elsewhere in the enclosing symbol
/// (`useAdaptiveTransaction({abi})` … `execute({functionName})`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum AbiScope {
    Object,
    Symbol,
}

/// A contract call site as written (viem/wagmi object arguments), left
/// unresolved: `application::analyze` links `abi` to an ABI definition and
/// `application::contract_abi` links the member to a contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ContractCall {
    /// `ownerOf`, `RoleGranted`; `None` when only the ABI is named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    pub kind: ContractMemberKind,
    /// The ABI identifier as written, first segment of a member path
    /// (`IPNFT_CONTRACT` for `IPNFT_CONTRACT.abi`), `as`/`satisfies`
    /// unwrapped, followed through same-file aliases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abi: Option<String>,
    /// The import specifier when `abi` is a default import of a `.json` or
    /// `.abi` file (`../assets/abis/LabNFT.json`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abi_import: Option<String>,
    pub scope: AbiScope,
    /// The function the object is passed to (`readContract`, `useReadContract`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// stable_id of the enclosing symbol; `None` at file level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub line: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FileExtraction {
    pub path: String,
    pub language: Option<Language>,
    pub content_hash: String,
    pub line_count: u32,
    pub symbols: Vec<Symbol>,
    pub imports: Vec<ImportRef>,
    pub calls: Vec<CallCandidate>,
    pub relations: Vec<Relation>,
    pub surfaces: Vec<Surface>,
    /// Bare package specifiers imported (`react`, `serde`) — for cross-repo package_dep.
    pub packages: Vec<String>,
    /// Byte ranges NOT owned by any symbol (imports, top-level statements) → file_head chunk.
    pub free_ranges: Vec<(usize, usize)>,
    /// Non-fatal parse problems (file still indexed at file level).
    pub warnings: Vec<String>,
    /// The document's own title when it declares one (Markdown front matter
    /// `title:`); stored on the File node as `metadata.title`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// GraphQL resolver registrations in this file (see [`GraphqlBinding`]);
    /// stored on the File node as `metadata.graphql_bindings`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub graphql_bindings: Vec<GraphqlBinding>,
    /// Repo-relative handler files this file deploys (CDK `entry:` props of a
    /// Lambda construct); stored on the File node as `metadata.lambda_entries`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lambda_entries: Vec<String>,
    /// Contract call sites (TS/JS `functionName`/`eventName`/`abi` objects);
    /// stored raw on the File node as `metadata.contract_calls`.
    #[serde(default)]
    pub contract_calls: Vec<ContractCall>,
}

pub trait SourceExtractor: Send + Sync {
    /// Extract one file. Must never panic or fail: on parse errors return a
    /// `FileExtraction` with `warnings` and whatever symbols were recovered.
    fn extract(&self, file: &FileInput) -> FileExtraction;

    /// Extract many files (adapter parallelises with rayon). Output order = input order.
    fn extract_all(&self, files: &[FileInput]) -> Vec<FileExtraction> {
        files.iter().map(|f| self.extract(f)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_calls_defaults_to_empty() {
        let old = serde_json::json!({
            "path": "a.ts", "language": "rust", "content_hash": "h", "line_count": 1,
            "symbols": [], "imports": [], "calls": [], "relations": [], "surfaces": [],
            "packages": [], "free_ranges": [], "warnings": [],
        });
        let x: FileExtraction = serde_json::from_value(old).unwrap();
        assert!(x.contract_calls.is_empty());
        let call = ContractCall {
            member: Some("ownerOf".into()),
            kind: ContractMemberKind::Function,
            abi: Some("LABNFT_OWNER_OF_ABI".into()),
            abi_import: None,
            scope: AbiScope::Object,
            via: Some("readContract".into()),
            owner: None,
            line: 44,
        };
        let v = serde_json::to_value(&call).unwrap();
        assert_eq!(v["kind"], "function");
        assert!(v.get("owner").is_none() && v.get("abi_import").is_none());
        assert_eq!(serde_json::from_value::<ContractCall>(v).unwrap(), call);
    }
}
