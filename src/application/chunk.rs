//! Chunk construction over an [`Analyzed`] repository: one `unit` chunk per
//! code-bearing symbol (context header + body), contextual `part` chunks for
//! oversize units (semantic merge when an embedder is available, line split
//! otherwise) and one `file_head` chunk per file for the bytes no symbol owns.
//!
//! Deterministic for a fixed embedder + content (Merkle gating relies on it):
//! chunk ids derive from `content_hash + stable_id + part`.

use crate::application::analyze::{file_stable_id, Analyzed, Unit};
use crate::domain::chunking::{
    self, HeaderInput, FILE_HEAD_MAX_CHARS, MAX_CHUNK_CHARS, TARGET_CHUNK_CHARS,
};
use crate::domain::hash;
use crate::domain::model::{Chunk, ChunkKind};
use crate::ports::embedder::Embedder;
use anyhow::{ensure, Result};
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use uuid::Uuid;

/// Counters reported by [`build_chunks`].
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ChunkStats {
    /// Units that became a single `unit` chunk.
    pub units: usize,
    /// `part` chunks produced by the similarity merge.
    pub parts_semantic: usize,
    /// `part` chunks produced by the line-split fallback.
    pub parts_lines: usize,
    pub file_heads: usize,
    /// `embed_batch` calls made (one per semantically split unit).
    pub embed_calls: usize,
    /// Largest `header + "\n" + body` length seen (always ≤ [`MAX_CHUNK_CHARS`]).
    pub max_chars: usize,
}

/// Room kept for the `// part i/n` stamp on split headers.
const PART_STAMP_CHARS: usize = 20;
/// Max import specifiers listed in a `file_head` header.
const HEAD_IMPORTS_CAP: usize = 24;
const RANGE_GAP: &str = "\n…\n";

/// Build every chunk of the analyzed repository. `contents` maps repo-relative
/// paths to file contents (files without content produce no chunks). Only
/// oversize units call the embedder — one `embed_batch` per unit; a failing
/// batch degrades to the line split (recorded in chunk metadata).
pub async fn build_chunks(
    repo_id: Uuid,
    analyzed: &Analyzed,
    contents: &[(String, String)],
    embedder: Option<&dyn Embedder>,
    split_cos: f32,
) -> Result<(Vec<Chunk>, ChunkStats)> {
    let by_path: HashMap<&str, &str> = contents
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    let mut chunks: Vec<Chunk> = Vec::with_capacity(analyzed.units.len() + analyzed.heads.len());
    let mut stats = ChunkStats::default();

    for unit in &analyzed.units {
        let file = &analyzed.bundle.files[unit.file_index];
        let Some(content) = by_path.get(file.path.as_str()) else {
            continue;
        };
        let sym = &unit.symbol;
        let body = slice(content, sym.byte_start, sym.byte_end);
        let module = module_of(&file.path);
        let header = chunking::render_header(&HeaderInput {
            path: &file.path,
            language: file.language.as_str(),
            line_start: sym.line_start as usize,
            line_end: sym.line_end as usize,
            container: unit.container.as_deref(),
            module: Some(&module),
            kind: sym.kind.as_str(),
            name: &sym.name,
            uses: &unit.uses,
            calls: &unit.calls,
            doc: sym.doc.as_deref(),
        });

        if header.len() + 1 + body.len() <= MAX_CHUNK_CHARS {
            chunks.push(make_chunk(
                repo_id,
                file.id,
                unit,
                header,
                body.to_string(),
                0,
                sym.line_start as usize,
                sym.line_end as usize,
                json!({}),
            ));
            stats.units += 1;
            continue;
        }

        // Oversize: plan segments (relative to the body), then merge or fall back.
        let cap = (MAX_CHUNK_CHARS - header.len() - PART_STAMP_CHARS).min(TARGET_CHUNK_CHARS);
        let rel: Vec<(usize, usize)> = sym
            .segments
            .iter()
            .filter_map(|&(s, e)| {
                let s = s.checked_sub(sym.byte_start)?;
                let e = e.checked_sub(sym.byte_start)?;
                Some((s, e))
            })
            .collect();
        let planned = enforce_cap(body, &chunking::plan_segments(body, &rel), cap);
        let (parts, split) = match embedder {
            Some(emb) if planned.len() > 1 => {
                let texts: Vec<String> = planned
                    .iter()
                    .map(|&(s, e)| format!("{header}\n{}", &body[s..e]))
                    .collect();
                stats.embed_calls += 1;
                match emb.embed_batch(&texts).await {
                    Ok(vectors) => {
                        let merged =
                            chunking::merge_by_similarity(body, &planned, &vectors, split_cos);
                        (enforce_cap(body, &merged, cap), "semantic")
                    }
                    Err(err) => {
                        tracing::warn!(path = %file.path, symbol = %sym.stable_id, error = %err, "segment embedding failed; falling back to line split");
                        (chunking::line_split(body, cap), "lines")
                    }
                }
            }
            Some(_) => (planned, "semantic"),
            None => (chunking::line_split(body, cap), "lines"),
        };
        let n = parts.len();
        for (i, &(s, e)) in parts.iter().enumerate() {
            let part = i + 1;
            let part_header = chunking::part_header(&header, part, n);
            let line_start = sym.line_start as usize + count_newlines(&body[..s]);
            let line_end = sym.line_start as usize + count_newlines(body[..e].trim_end());
            chunks.push(make_chunk(
                repo_id,
                file.id,
                unit,
                part_header,
                body[s..e].to_string(),
                part,
                line_start,
                line_end,
                json!({"split": split, "part": part, "parts": n}),
            ));
        }
        if split == "semantic" {
            stats.parts_semantic += n;
        } else {
            stats.parts_lines += n;
        }
    }

    for head in &analyzed.heads {
        let file = &analyzed.bundle.files[head.file_index];
        let Some(content) = by_path.get(file.path.as_str()) else {
            continue;
        };
        let Some((body, line_start, line_end)) = file_head_body(content, &head.free_ranges) else {
            continue;
        };
        let imports: Vec<String> = head
            .imports
            .iter()
            .take(HEAD_IMPORTS_CAP)
            .cloned()
            .collect();
        let header = chunking::render_header(&HeaderInput {
            path: &file.path,
            language: file.language.as_str(),
            line_start,
            line_end,
            container: None,
            module: Some(&module_of(&file.path)),
            kind: ChunkKind::FileHead.as_str(),
            name: &file.path,
            uses: &imports,
            calls: &[],
            doc: None,
        });
        let stable_id = file_stable_id(&file.path);
        let content_hash = chunking::content_hash(&header, &body);
        let id = hash::scoped_id(repo_id, &format!("{content_hash}:{stable_id}:0"));
        chunks.push(Chunk {
            id,
            repo_id,
            file_id: Some(file.id),
            node_id: Some(hash::scoped_id(repo_id, &stable_id)),
            kind: ChunkKind::FileHead,
            header,
            body,
            content_hash,
            line_start: Some(line_start as i32),
            line_end: Some(line_end as i32),
            metadata: json!({"imports": head.imports.len(), "ranges": head.free_ranges.len()}),
        });
        stats.file_heads += 1;
    }

    stats.max_chars = chunks
        .iter()
        .map(|c| c.header.len() + 1 + c.body.len())
        .max()
        .unwrap_or(0);
    ensure!(
        stats.max_chars <= MAX_CHUNK_CHARS,
        "chunk of {} chars exceeds the {} char cap",
        stats.max_chars,
        MAX_CHUNK_CHARS
    );
    tracing::info!(
        chunks = chunks.len(),
        units = stats.units,
        parts_semantic = stats.parts_semantic,
        parts_lines = stats.parts_lines,
        file_heads = stats.file_heads,
        embed_calls = stats.embed_calls,
        max_chars = stats.max_chars,
        "chunks built"
    );
    Ok((chunks, stats))
}

#[allow(clippy::too_many_arguments)]
fn make_chunk(
    repo_id: Uuid,
    file_id: Uuid,
    unit: &Unit,
    header: String,
    body: String,
    part: usize,
    line_start: usize,
    line_end: usize,
    metadata: serde_json::Value,
) -> Chunk {
    let content_hash = chunking::content_hash(&header, &body);
    let id = hash::scoped_id(
        repo_id,
        &format!("{content_hash}:{}:{part}", unit.symbol.stable_id),
    );
    Chunk {
        id,
        repo_id,
        file_id: Some(file_id),
        node_id: Some(unit.node_id),
        kind: if part == 0 {
            ChunkKind::Unit
        } else {
            ChunkKind::Part
        },
        header,
        body,
        content_hash,
        line_start: Some(line_start as i32),
        line_end: Some(line_end.max(line_start) as i32),
        metadata,
    }
}

/// `src/a/b.rs` → `src::a::b`.
fn module_of(path: &str) -> String {
    let no_ext = match (path.rfind('/'), path.rfind('.')) {
        (Some(slash), Some(dot)) if dot > slash => &path[..dot],
        (None, Some(dot)) => &path[..dot],
        _ => path,
    };
    no_ext.trim_start_matches("./").replace('/', "::")
}

fn count_newlines(s: &str) -> usize {
    s.bytes().filter(|b| *b == b'\n').count()
}

/// Char-boundary-safe slice with clamping.
fn slice(content: &str, start: usize, end: usize) -> &str {
    let len = content.len();
    let mut s = start.min(len);
    let mut e = end.min(len).max(s);
    while s > 0 && !content.is_char_boundary(s) {
        s -= 1;
    }
    while e < len && !content.is_char_boundary(e) {
        e += 1;
    }
    &content[s..e]
}

/// Line-split any segment longer than `cap` so header + body never exceeds the hard cap.
fn enforce_cap(body: &str, segments: &[(usize, usize)], cap: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(segments.len());
    for &(s, e) in segments {
        if e - s > cap {
            for (ls, le) in chunking::line_split(&body[s..e], cap) {
                out.push((s + ls, s + le));
            }
        } else {
            out.push((s, e));
        }
    }
    out
}

/// Join the free ranges of a file (`\n…\n` between non-adjacent ones), capped at
/// [`FILE_HEAD_MAX_CHARS`]. `None` when there is nothing but whitespace.
fn file_head_body(content: &str, free_ranges: &[(usize, usize)]) -> Option<(String, usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = free_ranges
        .iter()
        .map(|&(s, e)| (s.min(content.len()), e.min(content.len())))
        .filter(|(s, e)| e > s)
        .collect();
    ranges.sort_unstable();
    let mut body = String::new();
    let mut prev_end: Option<usize> = None;
    let mut first_start = None;
    let mut last_end = 0;
    for (s, e) in ranges {
        let text = slice(content, s, e);
        if text.trim().is_empty() {
            continue;
        }
        if let Some(pe) = prev_end {
            body.push_str(if pe == s { "\n" } else { RANGE_GAP });
        }
        body.push_str(text.trim_end_matches('\n'));
        prev_end = Some(e);
        first_start.get_or_insert(s);
        last_end = e;
        if body.len() >= FILE_HEAD_MAX_CHARS {
            break;
        }
    }
    let first_start = first_start?;
    if body.len() > FILE_HEAD_MAX_CHARS {
        let mut cut = FILE_HEAD_MAX_CHARS;
        while cut > 0 && !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
    }
    if body.trim().is_empty() {
        return None;
    }
    let line_start = 1 + count_newlines(&content[..first_start]);
    let line_end = 1 + count_newlines(content[..last_end].trim_end());
    Some((body, line_start, line_end.max(line_start)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::analyze::{analyze, FileHead};
    use crate::domain::model::{Language, NodeKind};
    use crate::ports::extractor::{FileExtraction, FileInput, ImportRef, Symbol};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Scripted embedder for use-case tests: topic-A segments map to one axis,
    /// everything else to the other. Never used outside tests.
    struct Scripted {
        calls: AtomicUsize,
        fail: bool,
    }

    #[async_trait]
    impl Embedder for Scripted {
        fn provider(&self) -> &'static str {
            "scripted"
        }
        fn model_id(&self) -> &str {
            "fixed"
        }
        fn dimensions(&self) -> usize {
            2
        }
        async fn embed(&self, input: &str) -> Result<Vec<f32>> {
            self.embed_batch(&[input.to_string()])
                .await
                .map(|mut v| v.remove(0))
        }
        async fn embed_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            anyhow::ensure!(!self.fail, "scripted failure");
            Ok(inputs
                .iter()
                .map(|t| {
                    if t.contains("alpha") {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    }
                })
                .collect())
        }
    }

    fn repo() -> Uuid {
        hash::repo_id("/chunk/test")
    }

    /// A function with four ~2200-char statements: alpha, alpha, beta, beta.
    fn big_source() -> (String, Vec<(usize, usize)>, usize) {
        let mut src = String::from("use crate::util::helper;\n\nfn big() {\n");
        let body_start = src.len();
        let mut segs = Vec::new();
        for topic in ["alpha", "alpha", "beta", "beta"] {
            let s = src.len();
            for i in 0..55 {
                src.push_str(&format!(
                    "    let {topic}_{i:03} = {topic}({i}); // {topic}\n"
                ));
            }
            segs.push((s, src.len()));
        }
        src.push_str("}\n");
        (src, segs, body_start)
    }

    fn analyzed_big() -> (Analyzed, Vec<(String, String)>) {
        let (src, segs, _) = big_source();
        let sym = Symbol {
            stable_id: "rust:src/big.rs:function:big".into(),
            kind: NodeKind::Function,
            name: "big".into(),
            qualified_name: "big".into(),
            parent: None,
            signature: Some("fn big()".into()),
            doc: Some("Does a lot.".into()),
            line_start: 3,
            line_end: src.lines().count() as u32,
            byte_start: src.find("fn big").unwrap(),
            byte_end: src.len(),
            segments: segs,
            metadata: json!({}),
        };
        let mut x = FileExtraction {
            path: "src/big.rs".into(),
            language: Some(Language::Rust),
            content_hash: hash::sha256_hex(&src),
            line_count: src.lines().count() as u32,
            symbols: vec![sym],
            ..Default::default()
        };
        x.imports = vec![ImportRef {
            specifier: "crate::util::helper".into(),
            names: vec![],
            line: 1,
        }];
        x.free_ranges = vec![(0, src.find("fn big").unwrap())];
        let files = vec![(
            FileInput {
                path: "src/big.rs".into(),
                language: Language::Rust,
                content: src.clone(),
            },
            x,
        )];
        let analyzed = analyze(repo(), &files);
        (analyzed, vec![("src/big.rs".to_string(), src)])
    }

    #[tokio::test]
    async fn semantic_split_merges_similar_segments_in_one_batch() {
        let (analyzed, contents) = analyzed_big();
        let emb = Scripted {
            calls: AtomicUsize::new(0),
            fail: false,
        };
        let (chunks, stats) = build_chunks(
            repo(),
            &analyzed,
            &contents,
            Some(&emb),
            chunking::DEFAULT_SPLIT_COS,
        )
        .await
        .unwrap();
        assert_eq!(emb.calls.load(Ordering::SeqCst), 1);
        assert_eq!(stats.embed_calls, 1);
        assert_eq!(stats.units, 0);
        // alpha+alpha and beta+beta merged (≤ target), topics kept apart → 2 parts.
        assert_eq!(stats.parts_semantic, 2);
        assert_eq!(stats.parts_lines, 0);
        assert_eq!(stats.file_heads, 1);
        assert!(stats.max_chars <= MAX_CHUNK_CHARS);
        let parts: Vec<&Chunk> = chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::Part)
            .collect();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].header.ends_with("// part 1/2"));
        assert!(parts[1].header.ends_with("// part 2/2"));
        assert_eq!(parts[0].metadata["split"], "semantic");
        assert_eq!(parts[0].metadata["parts"], 2);
        assert!(parts[0].body.contains("alpha_000") && parts[0].body.contains("alpha_054"));
        assert!(!parts[0].body.contains("beta"));
        assert!(parts[1].body.contains("beta_000") && !parts[1].body.contains("alpha"));
        // Line ranges are contiguous and start at the symbol's first line.
        assert_eq!(parts[0].line_start, Some(3));
        assert_eq!(parts[1].line_start, Some(parts[0].line_end.unwrap() + 1));
        assert_eq!(
            parts[1].line_end,
            Some(analyzed.units[0].symbol.line_end as i32)
        );
        // Header carries file/module/kind/uses/doc.
        assert!(parts[0]
            .header
            .starts_with("// file: src/big.rs (rust)  L3-"));
        assert!(parts[0].header.contains("// module: src::big"));
        assert!(parts[0].header.contains("// kind: function  name: big"));
        assert!(parts[0].header.contains("// doc: Does a lot."));
        // Ids are content-derived + distinct; hash matches header+body.
        assert_ne!(parts[0].id, parts[1].id);
        assert_eq!(
            parts[0].content_hash,
            chunking::content_hash(&parts[0].header, &parts[0].body)
        );
        assert_eq!(parts[0].node_id, Some(analyzed.units[0].node_id));
        // file_head lists the import and points at the file node.
        let head = chunks
            .iter()
            .find(|c| c.kind == ChunkKind::FileHead)
            .unwrap();
        assert!(head.header.contains("// kind: file_head"));
        assert!(head.header.contains("// uses: crate::util::helper"));
        assert!(head.body.contains("use crate::util::helper;"));
        assert_eq!(head.line_start, Some(1));
        assert_eq!(
            head.node_id,
            Some(hash::scoped_id(repo(), "file:src/big.rs"))
        );
        // Deterministic.
        let (again, _) = build_chunks(
            repo(),
            &analyzed,
            &contents,
            Some(&emb),
            chunking::DEFAULT_SPLIT_COS,
        )
        .await
        .unwrap();
        assert_eq!(chunks, again);
    }

    #[tokio::test]
    async fn line_fallback_without_embedder_and_on_failure() {
        let (analyzed, contents) = analyzed_big();
        let (chunks, stats) = build_chunks(repo(), &analyzed, &contents, None, 0.82)
            .await
            .unwrap();
        assert_eq!(stats.embed_calls, 0);
        assert_eq!(stats.parts_semantic, 0);
        assert!(stats.parts_lines >= 2);
        assert!(stats.max_chars <= MAX_CHUNK_CHARS);
        let parts: Vec<&Chunk> = chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::Part)
            .collect();
        assert_eq!(parts.len(), stats.parts_lines);
        for (i, p) in parts.iter().enumerate() {
            assert_eq!(p.metadata["split"], "lines");
            assert_eq!(p.metadata["part"], i + 1);
            assert!(p
                .header
                .ends_with(&format!("// part {}/{}", i + 1, parts.len())));
        }
        // Parts reassemble the body exactly.
        let joined: String = parts.iter().map(|p| p.body.as_str()).collect();
        let sym = &analyzed.units[0].symbol;
        assert_eq!(joined, contents[0].1[sym.byte_start..sym.byte_end]);

        // Embedder present but failing → same line fallback, one attempted call.
        let emb = Scripted {
            calls: AtomicUsize::new(0),
            fail: true,
        };
        let (_, stats2) = build_chunks(repo(), &analyzed, &contents, Some(&emb), 0.82)
            .await
            .unwrap();
        assert_eq!(stats2.embed_calls, 1);
        assert_eq!(stats2.parts_lines, stats.parts_lines);
        assert_eq!(stats2.parts_semantic, 0);
    }

    #[tokio::test]
    async fn small_units_become_unit_chunks_and_empty_heads_are_skipped() {
        let src = "fn tiny() -> u8 { 1 }\n\n\n";
        let sym = Symbol {
            stable_id: "rust:src/t.rs:function:tiny".into(),
            kind: NodeKind::Function,
            name: "tiny".into(),
            qualified_name: "tiny".into(),
            parent: None,
            signature: None,
            doc: None,
            line_start: 1,
            line_end: 1,
            byte_start: 0,
            byte_end: src.find('\n').unwrap(),
            segments: vec![],
            metadata: json!({}),
        };
        let x = FileExtraction {
            path: "src/t.rs".into(),
            language: Some(Language::Rust),
            content_hash: hash::sha256_hex(src),
            line_count: 1,
            symbols: vec![sym],
            free_ranges: vec![(src.find('\n').unwrap(), src.len())],
            ..Default::default()
        };
        let files = vec![(
            FileInput {
                path: "src/t.rs".into(),
                language: Language::Rust,
                content: src.into(),
            },
            x,
        )];
        let analyzed = analyze(repo(), &files);
        let contents = vec![("src/t.rs".to_string(), src.to_string())];
        let emb = Scripted {
            calls: AtomicUsize::new(0),
            fail: false,
        };
        let (chunks, stats) = build_chunks(repo(), &analyzed, &contents, Some(&emb), 0.82)
            .await
            .unwrap();
        assert_eq!(
            stats,
            ChunkStats {
                units: 1,
                max_chars: chunks[0].header.len() + 1 + chunks[0].body.len(),
                ..Default::default()
            }
        );
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, ChunkKind::Unit);
        assert_eq!(chunks[0].body, "fn tiny() -> u8 { 1 }");
        assert_eq!(emb.calls.load(Ordering::SeqCst), 0);
        // A file with no content supplied yields nothing.
        let (none, _) = build_chunks(repo(), &analyzed, &[], None, 0.82)
            .await
            .unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn file_head_joins_ranges_and_caps() {
        let content = "import a;\nimport b;\n\nconst x = 1;\nfunction f() {}\nconst y = 2;\n";
        let f = content.find("function").unwrap();
        let y = content.find("const y").unwrap();
        let (body, ls, le) = file_head_body(content, &[(0, f), (y, content.len())]).unwrap();
        assert_eq!(
            body,
            "import a;\nimport b;\n\nconst x = 1;\n…\nconst y = 2;"
        );
        assert_eq!((ls, le), (1, 6));
        assert!(file_head_body(content, &[(20, 21)]).is_none());
        let huge = "x".repeat(5000);
        let (body, _, _) = file_head_body(&huge, &[(0, 5000)]).unwrap();
        assert_eq!(body.len(), FILE_HEAD_MAX_CHARS);
        assert_eq!(module_of("src/a/b.rs"), "src::a::b");
        assert_eq!(module_of("README.md"), "README");
        let _ = FileHead::default();
    }
}
