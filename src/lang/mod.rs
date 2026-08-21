//! Pure-Rust AST extraction for non-Rust languages.
//!
//! Each submodule parses one language with a real parser and emits the same
//! node/edge/chunk shapes the regex extractors used to produce. Shared glue —
//! the byte-offset→line index and the per-file extraction context — lives here.

use crate::{
    extractor::{
        chunk_for_node, edge, is_bare_module_specifier, is_external_import,
        normalize_relative_import, python_relative_target, slice_lines,
    },
    models::{EdgeKind, ExtractionResult, KnowledgeNode, NodeKind, SourceFile},
    weights::EdgeWeight,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub(crate) mod graphql;
pub(crate) mod javascript;
pub(crate) mod python;
pub(crate) mod solidity;

/// Emit a consistent warning when a file fails to parse. The file node is still
/// recorded by `begin_file`, so extraction degrades to file-level context
/// instead of aborting the run or fabricating symbols.
pub(crate) fn warn_parse_failure(path: &str, detail: &str) {
    tracing::warn!(
        path,
        "parse failed: {detail}; indexing file without symbols"
    );
}

/// Emit a whole-file chunk when symbol extraction produced no chunks for a
/// file, mirroring the Markdown sectionless fallback. Files made of
/// `export default {...}` configs, `.d.ts` declarations, top-level test
/// calls, or data literals — and files whose parser FAILED — collect no
/// symbols and would otherwise be invisible to retrieval (the file gets a
/// graph node but zero chunks). The chunk attaches to the file node;
/// oversized content is split later by `split_large_chunks`.
///
/// `chunks_before` is `result.chunks.len()` captured before the language
/// module started emitting for this file (`result` accumulates across files,
/// so an absolute emptiness check would be wrong).
pub(crate) fn emit_whole_file_fallback(
    repo_id: Uuid,
    file: &SourceFile,
    file_node_id: Uuid,
    chunks_before: usize,
    result: &mut ExtractionResult,
    reason: &str,
) {
    if result.chunks.len() > chunks_before {
        return;
    }
    result.chunks.push(chunk_for_node(
        repo_id,
        Some(file.id),
        Some(file_node_id),
        "code",
        &format!(
            "Language: {language}\nFile: {path}\nKind: whole-file fallback\nLines: 1-{line_count}\n\n{content}",
            language = file.language.as_str(),
            path = file.path,
            line_count = file.line_count,
            content = file.content,
        ),
        Some(1),
        Some(file.line_count),
        serde_json::json!({
            "kind": "whole_file_fallback",
            "file": file.path,
            "reason": reason,
        }),
    ));
}

/// Maps a UTF-8 byte offset (as produced by every AST node span) to a 1-based
/// line number. Built once per file.
pub(crate) struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    pub(crate) fn new(content: &str) -> Self {
        let mut line_starts = vec![0usize];
        for (i, b) in content.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        Self { line_starts }
    }

    /// 1-based line number containing `byte_offset`.
    /// Offsets past the end of `content` map to the last line + 1.
    pub(crate) fn line(&self, byte_offset: usize) -> usize {
        match self.line_starts.binary_search(&byte_offset) {
            Ok(idx) => idx + 1,
            Err(idx) => idx,
        }
    }
}

/// A function/method call discovered in source, pending resolution to a target symbol.
pub(crate) struct CallSite {
    pub file: String,
    pub callee: String,
    pub line: i32,
}

/// How `emit_dependency` decides which imports are internal (kept, deduped
/// repo-wide) and which are external (dropped — the repo's real dependency
/// list lives in the manifest nodes).
pub(crate) enum ImportFilter<'a> {
    /// JS/TS and Solidity: bare specifiers are kept only when they name one of
    /// the repo's own workspace packages; relative/alias paths are internal.
    NpmWorkspace(&'a HashSet<String>),
    /// Python: absolute modules are kept only when their first dotted segment
    /// is one of the repo's own top-level module roots; leading-dot relative
    /// imports are internal. Stdlib/site-packages imports are dropped.
    PythonRoots(&'a HashSet<String>),
}

/// Per-file extraction context shared by the language submodules.
pub(crate) struct FileExtraction<'a> {
    pub repo_id: Uuid,
    pub file: &'a SourceFile,
    pub file_node_id: Uuid,
    pub lines: LineIndex,
    pub symbol_names: &'a mut HashMap<String, Uuid>,
    pub result: &'a mut ExtractionResult,
    pub calls: &'a mut Vec<CallSite>,
    /// GraphQL type/fragment references pending the graphql-only post-pass
    /// ([`graphql::resolve_graphql_edges`]). Kept separate from `symbol_names`
    /// on purpose: that map is walk-order-populated and shared across
    /// languages, so resolving through it would drop forward references and
    /// let a TS class named `User` capture a GraphQL edge.
    pub graphql_refs: &'a mut Vec<graphql::PendingRef>,
    /// Internal-vs-external import classification for this file's language.
    pub import_filter: ImportFilter<'a>,
}

impl<'a> FileExtraction<'a> {
    /// Emit a code symbol node (function, class, enum, etc.) with its
    /// `Contains` edge and text chunk.
    ///
    /// Covers the common pattern shared by JavaScript/TypeScript and Python
    /// extraction.  Callers supply the already-resolved `kind` (post test-file
    /// detection), a pre-formatted `stable_id`, and the language-specific
    /// metadata/label values so that each language's output remains
    /// byte-identical to what its own local helper produced previously.
    ///
    /// # Parameters
    /// - `name`            – symbol name (already trimmed, non-empty)
    /// - `kind`            – final `NodeKind` (test detection applied by caller)
    /// - `stable_id`       – pre-computed stable ID string
    /// - `language`        – language string stored in node/chunk metadata
    /// - `contains_weight` – edge weight (e.g. `CONTAINS_CODE` / `CONTAINS_MEMBER`)
    /// - `edge_meta`       – metadata value attached to the `Contains` edge
    /// - `node_meta`       – full metadata object for the `KnowledgeNode`
    /// - `kind_label`      – value shown in `"Kind: {kind_label}"` of the chunk
    /// - `start_off`       – byte offset of the symbol start
    /// - `end_off`         – byte offset of the symbol end
    /// - `chunk_meta`      – full metadata object for the `KnowledgeChunk`
    /// - `code_override`   – chunk body to use instead of the full span slice
    ///   (e.g. a class header + fields + method roster, when the methods are
    ///   extracted as their own symbols and a full-body chunk would duplicate
    ///   them); `None` keeps the span slice
    ///
    /// Returns the new node's id so callers that link symbols after the walk
    /// (the GraphQL post-pass) can reference it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn emit_code_symbol(
        &mut self,
        name: &str,
        kind: NodeKind,
        stable_id: String,
        language: &str,
        contains_weight: EdgeWeight,
        edge_meta: Value,
        node_meta: Value,
        kind_label: &str,
        start_off: usize,
        end_off: usize,
        chunk_meta: Value,
        code_override: Option<&str>,
    ) -> Uuid {
        let line = self.lines.line(start_off);
        let end_line = self.lines.line(end_off);
        let code = match code_override {
            Some(code) => code.to_string(),
            None => slice_lines(&self.file.content, line, end_line),
        };

        let node = KnowledgeNode {
            id: Uuid::new_v4(),
            repo_id: self.repo_id,
            file_id: Some(self.file.id),
            kind: kind.clone(),
            stable_id,
            name: name.to_string(),
            line_start: Some(line as i32),
            line_end: Some(end_line as i32),
            metadata: node_meta,
        };

        // GraphQL nodes stay out of the shared cross-language symbol map: SDL
        // types reuse the classic kinds (Struct/Trait/…), so registering them
        // would let the generic call pass resolve a Python/TS `User(...)` call
        // to a schema type (or make a repo-unique code symbol ambiguous).
        // GraphQL's own refs resolve in [`graphql::resolve_graphql_edges`].
        if language != "graphql" {
            self.symbol_names.entry(name.to_string()).or_insert(node.id);
        }

        self.result.edges.push(edge(
            self.repo_id,
            self.file_node_id,
            node.id,
            EdgeKind::Contains,
            contains_weight,
            edge_meta,
        ));

        self.result.chunks.push(chunk_for_node(
            self.repo_id,
            Some(self.file.id),
            Some(node.id),
            kind.as_str(),
            &format!(
                "Language: {language}\nFile: {path}\nSymbol: {name}\nKind: {kind_label}\nLines: {line}-{end_line}\n\n{code}",
                path = self.file.path,
            ),
            Some(line as i32),
            Some(end_line as i32),
            chunk_meta,
        ));

        let node_id = node.id;
        self.result.nodes.push(node);
        node_id
    }

    /// Thin delegate to the free [`emit_whole_file_fallback`], for language
    /// modules that need the fallback at a specific point (JS/TS emits it
    /// before user-surface chunks so a lone env-read chunk doesn't mask an
    /// unindexed file body). The extractor's language wrapper also calls the
    /// free fn centrally after each module runs.
    pub(crate) fn emit_whole_file_fallback(&mut self, chunks_before: usize, reason: &str) {
        emit_whole_file_fallback(
            self.repo_id,
            self.file,
            self.file_node_id,
            chunks_before,
            self.result,
            reason,
        );
    }

    /// Emit the user-surface entries (CLI commands, HTTP routes, env-var
    /// reads) collected by a language module. See [`crate::user_surface`].
    pub(crate) fn emit_user_surface(&mut self, entries: Vec<crate::user_surface::SurfaceEntry>) {
        crate::user_surface::emit_surface_entries(
            self.repo_id,
            self.file,
            self.file_node_id,
            entries,
            self.result,
        );
    }

    /// Emit a dependency (import) node and its `Imports` edge.
    ///
    /// Covers the common pattern shared by JavaScript/TypeScript, Python, and
    /// Solidity extraction. No chunk is emitted for imports — only a node and
    /// an edge — matching the previous per-language implementations.
    ///
    /// # Parameters
    /// - `module`         – module/path string (non-empty, already validated)
    /// - `language`       – language string stored in node metadata
    /// - `import_weight`  – edge weight (e.g. `IMPORTS_MODULE` / `IMPORTS_SOLIDITY`)
    /// - `offset`         – byte offset used to compute the 1-based line number
    pub(crate) fn emit_dependency(
        &mut self,
        module: &str,
        language: &str,
        import_weight: EdgeWeight,
        offset: usize,
    ) {
        // External (third-party / stdlib) imports are dropped from the graph
        // entirely: a shared `import:bare:react` hub glues every file that
        // imports react into one blob and gets picked as its label. The repo's
        // real dependency list still lives in the manifest dependency nodes.
        //
        // Internal imports dedupe to ONE node per imported module repo-wide
        // (two files importing the same `../lib/utils` share a node — they ARE
        // coupled through it). Shared nodes carry no file_id/lines; the
        // per-importer file and line live on each `Imports` edge.
        let Some((stable_id, canonical, scope)) = self.classify_import(module) else {
            return;
        };
        let line = self.lines.line(offset) as i32;

        let node = KnowledgeNode {
            id: Uuid::new_v4(),
            repo_id: self.repo_id,
            file_id: None,
            kind: NodeKind::Dependency,
            stable_id,
            name: canonical.clone(),
            line_start: None,
            line_end: None,
            metadata: serde_json::json!({
                "module": canonical,
                "language": language,
                "scope": scope
            }),
        };

        self.result.edges.push(edge(
            self.repo_id,
            self.file_node_id,
            node.id,
            EdgeKind::Imports,
            import_weight,
            serde_json::json!({"file": self.file.path, "module": module, "line": line}),
        ));

        self.result.nodes.push(node);
    }

    /// Classify an import as internal (returning its repo-wide stable id,
    /// canonical module name, and scope label) or external (`None` — dropped).
    fn classify_import(&self, module: &str) -> Option<(String, String, &'static str)> {
        match &self.import_filter {
            ImportFilter::NpmWorkspace(workspace) => {
                if is_bare_module_specifier(module) {
                    if is_external_import(module, workspace) {
                        return None;
                    }
                    Some((format!("import:bare:{module}"), module.to_string(), "bare"))
                } else if module.starts_with('.') {
                    let target = normalize_relative_import(&self.file.path, module);
                    Some((format!("import:path:{target}"), target, "path"))
                } else {
                    // Root-anchored alias (`@/`, `~/`, `/`): the same specifier
                    // means the same target from any file.
                    Some((
                        format!("import:alias:{module}"),
                        module.to_string(),
                        "alias",
                    ))
                }
            }
            ImportFilter::PythonRoots(roots) => {
                if module.starts_with('.') {
                    let target = python_relative_target(&self.file.path, module);
                    Some((format!("import:path:{target}"), target, "path"))
                } else {
                    let first = module.split('.').next().unwrap_or("");
                    if !roots.contains(first) {
                        return None;
                    }
                    let target = module.replace('.', "/");
                    Some((format!("import:path:{target}"), target, "path"))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LineIndex;

    #[test]
    fn line_index_maps_offsets_to_one_based_lines() {
        let src = "alpha\nbeta\ngamma\n";
        let idx = LineIndex::new(src);
        assert_eq!(idx.line(0), 1);
        assert_eq!(idx.line(6), 2);
        assert_eq!(idx.line(11), 3);
        assert_eq!(idx.line(src.len()), 4);
    }
}
