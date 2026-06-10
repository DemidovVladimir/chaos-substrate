//! Pure-Rust AST extraction for non-Rust languages.
//!
//! Each submodule parses one language with a real parser and emits the same
//! node/edge/chunk shapes the regex extractors used to produce. Shared glue —
//! the byte-offset→line index and the per-file extraction context — lives here.

use crate::{
    extractor::{
        chunk_for_node, edge, import_stable_id, is_bare_module_specifier, is_external_import,
        slice_lines,
    },
    models::{EdgeKind, ExtractionResult, KnowledgeNode, NodeKind, SourceFile},
    weights::EdgeWeight,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

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

/// Per-file extraction context shared by the language submodules.
pub(crate) struct FileExtraction<'a> {
    pub repo_id: Uuid,
    pub file: &'a SourceFile,
    pub file_node_id: Uuid,
    pub lines: LineIndex,
    pub symbol_names: &'a mut HashMap<String, Uuid>,
    pub result: &'a mut ExtractionResult,
    pub calls: &'a mut Vec<CallSite>,
    /// The repo's own workspace package names, for JS/TS only. When `Some`, an
    /// import that resolves outside the repo (a third-party `node_modules`
    /// package) is dropped instead of becoming a god-node feature. `None` for
    /// languages where we can't yet tell internal-absolute from third-party
    /// imports (Python, Solidity) — those keep every import, unchanged.
    pub workspace_packages: Option<&'a HashSet<String>>,
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
    ) {
        let line = self.lines.line(start_off);
        let end_line = self.lines.line(end_off);
        let code = slice_lines(&self.file.content, line, end_line);

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

        self.symbol_names.entry(name.to_string()).or_insert(node.id);

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

        self.result.nodes.push(node);
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
        // Drop third-party (node_modules) imports from the graph entirely: they
        // form and name giant "features" (a shared `import:bare:react` hub glues
        // every file that imports react into one blob and gets picked as its
        // label). The repo's real dependency list still lives in the package.json
        // dependency nodes. Only applied when we have the workspace package set
        // (JS/TS); internal/workspace imports fall through and are kept.
        if let Some(workspace) = self.workspace_packages {
            if is_external_import(module, workspace) {
                return;
            }
        }

        let line = self.lines.line(offset) as i32;
        let is_bare = is_bare_module_specifier(module);

        let node = KnowledgeNode {
            id: Uuid::new_v4(),
            repo_id: self.repo_id,
            file_id: if is_bare { None } else { Some(self.file.id) },
            kind: NodeKind::Dependency,
            stable_id: import_stable_id(self.file, module, is_bare),
            name: module.to_string(),
            line_start: if is_bare { None } else { Some(line) },
            line_end: if is_bare { None } else { Some(line) },
            metadata: serde_json::json!({
                "module": module,
                "language": language,
                "scope": if is_bare { "bare" } else { "relative" }
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
