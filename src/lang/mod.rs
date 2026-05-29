//! Pure-Rust AST extraction for non-Rust languages.
//!
//! Each submodule parses one language with a real parser and emits the same
//! node/edge/chunk shapes the regex extractors used to produce. Shared glue —
//! the byte-offset→line index and the per-file extraction context — lives here.

use crate::models::{ExtractionResult, SourceFile};
use std::collections::HashMap;
use uuid::Uuid;

pub(crate) mod javascript;
pub(crate) mod python;
pub(crate) mod solidity;

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
