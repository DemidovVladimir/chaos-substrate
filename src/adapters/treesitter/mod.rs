//! tree-sitter adapter for `ports::extractor::SourceExtractor`.
//!
//! `walker` is the generic cursor walk; `lang` is the table-driven spec each
//! language module fills in (`rust`, `typescript`, `python`, `solidity`,
//! `graphql`); `markdown` is a heading-based extractor (not tree-sitter);
//! `surface` provides user-surface passes (env vars, HTTP routes).

pub mod graphql;
pub mod lang;
pub mod markdown;
pub mod python;
pub mod rust;
pub mod solidity;
pub mod surface;
pub mod typescript;
pub mod walker;

use crate::domain::model::Language;
use crate::ports::extractor::{FileExtraction, FileInput, SourceExtractor};
use rayon::prelude::*;

/// Stateless extractor: a fresh `tree_sitter::Parser` per file, so it is
/// `Send + Sync` and parallel-safe.
#[derive(Debug, Default, Clone, Copy)]
pub struct TreeSitterExtractor;

impl SourceExtractor for TreeSitterExtractor {
    fn extract(&self, file: &FileInput) -> FileExtraction {
        if file.language == Language::Markdown {
            return markdown::extract(file);
        }
        match lang::spec_for(file.language, &file.path) {
            Some(spec) => walker::extract(spec, file),
            None => walker::file_level(file),
        }
    }

    fn extract_all(&self, files: &[FileInput]) -> Vec<FileExtraction> {
        files.par_iter().map(|f| self.extract(f)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_all_preserves_order_and_falls_back() {
        let files = vec![
            FileInput {
                path: "b.rs".into(),
                language: Language::Rust,
                content: "fn b() {}".into(),
            },
            FileInput {
                path: "a.md".into(),
                language: Language::Markdown,
                content: "# T\ntext".into(),
            },
            FileInput {
                path: "c.rs".into(),
                language: Language::Rust,
                content: "fn c() {}".into(),
            },
        ];
        let out = TreeSitterExtractor.extract_all(&files);
        let paths: Vec<&str> = out.iter().map(|o| o.path.as_str()).collect();
        assert_eq!(paths, ["b.rs", "a.md", "c.rs"]);
        assert_eq!(out[0].symbols[0].name, "b");
        assert_eq!(out[1].language, Some(Language::Markdown));
        assert_eq!(out[1].content_hash.len(), 64);
    }
}
