//! Chunk construction over an [`Analyzed`] repository: one `unit` chunk per
//! code-bearing symbol (context header + body), contextual `part` chunks for
//! oversize units (semantic merge when an embedder is available, line split
//! otherwise) and `file_head` chunks for the bytes no symbol owns (one per
//! file, or `part i/n` chunks when they exceed [`FILE_HEAD_MAX_CHARS`]; a
//! test file keeps only its first part).
//!
//! Deterministic for a fixed embedder + content (embedding reuse relies on it):
//! chunk ids derive from `content_hash + stable_id + part`.

use crate::application::analyze::{file_stable_id, Analyzed, Unit};
use crate::application::entry_points::is_test_path;
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
            abi: unit.abi.as_deref(),
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
        let mut parts = file_head_parts(content, &head.free_ranges, FILE_HEAD_MAX_CHARS);
        // A test file's top level is test bodies, not a file head: one part,
        // so dozens of parts do not crowd real code out of retrieval.
        if is_test_path(&file.path) {
            parts.truncate(1);
        }
        let imports: Vec<String> = head
            .imports
            .iter()
            .take(HEAD_IMPORTS_CAP)
            .cloned()
            .collect();
        let stable_id = file_stable_id(&file.path);
        let n = parts.len();
        for (i, (body, line_start, line_end)) in parts.into_iter().enumerate() {
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
                abi: None,
            });
            // One part keeps the historical id and metadata; parts are `1..=n`.
            let (header, part, metadata) = if n == 1 {
                let meta = json!({"imports": head.imports.len(), "ranges": head.free_ranges.len()});
                (header, 0, meta)
            } else {
                let meta = json!({"imports": head.imports.len(), "ranges": head.free_ranges.len(), "part": i + 1, "parts": n});
                (chunking::part_header(&header, i + 1, n), i + 1, meta)
            };
            let content_hash = chunking::content_hash(&header, &body);
            let id = hash::scoped_id(repo_id, &format!("{content_hash}:{stable_id}:{part}"));
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
                metadata,
            });
            stats.file_heads += 1;
        }
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

/// Join the free ranges of a file (`\n…\n` between non-adjacent ones) into
/// `file_head` parts of at most `max` bytes each, in source order, so no
/// top-level text is lost: a part is flushed when the next range would not
/// fit, and a single range longer than `max` is line-split. Each part is
/// `(body, line_start, line_end)`. A file whose head fits in `max` yields
/// exactly one part, identical to the historical single `file_head`. Empty
/// when there is nothing but whitespace.
fn file_head_parts(
    content: &str,
    free_ranges: &[(usize, usize)],
    max: usize,
) -> Vec<(String, usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = free_ranges
        .iter()
        .map(|&(s, e)| (s.min(content.len()), e.min(content.len())))
        .filter(|(s, e)| e > s)
        .collect();
    ranges.sort_unstable();
    let lines = |start: usize, end: usize| {
        let line_start = 1 + count_newlines(&content[..start]);
        let line_end = 1 + count_newlines(content[..end].trim_end());
        (line_start, line_end.max(line_start))
    };
    let mut parts: Vec<(String, usize, usize)> = Vec::new();
    // The open part: (body, first byte, end byte of its last range).
    let mut cur: Option<(String, usize, usize)> = None;
    let flush = |cur: &mut Option<(String, usize, usize)>,
                 parts: &mut Vec<(String, usize, usize)>| {
        if let Some((body, first, last)) = cur.take() {
            if !body.trim().is_empty() {
                let (ls, le) = lines(first, last);
                parts.push((body, ls, le));
            }
        }
    };
    for (s, e) in ranges {
        let text = slice(content, s, e);
        if text.trim().is_empty() {
            continue;
        }
        let text = text.trim_end_matches('\n');
        let joined_len = cur.as_ref().map(|(body, _, last)| {
            body.len() + if *last == s { 1 } else { RANGE_GAP.len() } + text.len()
        });
        match joined_len {
            Some(len) if len <= max => {
                let (body, _, last) = cur.as_mut().expect("checked above");
                body.push_str(if *last == s { "\n" } else { RANGE_GAP });
                body.push_str(text);
                *last = e;
            }
            _ => {
                flush(&mut cur, &mut parts);
                if text.len() <= max {
                    cur = Some((text.to_string(), s, e));
                } else {
                    for (ps, pe) in chunking::line_split(text, max) {
                        let piece = &text[ps..pe];
                        if piece.trim().is_empty() {
                            continue;
                        }
                        let (ls, le) = lines(s + ps, s + pe);
                        parts.push((piece.trim_end_matches('\n').to_string(), ls, le));
                    }
                }
            }
        }
    }
    flush(&mut cur, &mut parts);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::analyze::{analyze, FileHead};
    use crate::domain::model::{Language, NodeKind};
    use crate::ports::extractor::{FileExtraction, FileInput, ImportRef, Symbol};
    use async_trait::async_trait;
    use std::collections::HashSet;
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
    fn file_head_joins_ranges_and_splits() {
        let content = "import a;\nimport b;\n\nconst x = 1;\nfunction f() {}\nconst y = 2;\n";
        let f = content.find("function").unwrap();
        let y = content.find("const y").unwrap();
        let parts = file_head_parts(content, &[(0, f), (y, content.len())], FILE_HEAD_MAX_CHARS);
        assert_eq!(
            parts,
            vec![(
                "import a;\nimport b;\n\nconst x = 1;\n…\nconst y = 2;".to_string(),
                1,
                6
            )]
        );
        assert!(file_head_parts(content, &[(20, 21)], FILE_HEAD_MAX_CHARS).is_empty());
        // One range past the cap is line-split, never truncated.
        let huge: String = (0..250).map(|i| format!("let v{i:03} = {i};\n")).collect();
        let parts = file_head_parts(&huge, &[(0, huge.len())], FILE_HEAD_MAX_CHARS);
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|(b, _, _)| b.len() <= FILE_HEAD_MAX_CHARS));
        assert_eq!(parts[0].1, 1);
        assert_eq!(parts.last().unwrap().2, 250);
        for w in parts.windows(2) {
            assert_eq!(w[0].2 + 1, w[1].1, "contiguous lines");
        }
        let joined: String = parts.iter().map(|(b, _, _)| format!("{b}\n")).collect();
        assert_eq!(joined, huge);
        assert_eq!(module_of("src/a/b.rs"), "src::a::b");
        assert_eq!(module_of("README.md"), "README");
        let _ = FileHead::default();
    }

    /// A 45-constant file modelled on onchainlabs src/types/Constants.sol
    /// (the Solidity extractor puts every file-level constant, with its doc
    /// comment, into the free ranges): every constant lands in exactly one
    /// `file_head` part, each part fits the cap, and two runs are identical.
    /// A test file's top-level bodies are not a file head: it keeps one
    /// `file_head` chunk (historical id, no part header) however long.
    #[tokio::test]
    async fn test_file_head_keeps_one_part() {
        let mut src = String::new();
        let mut free_ranges = Vec::new();
        for i in 0..60 {
            let start = src.len();
            src.push_str(&format!(
                "it(\"maps contact {i} to the CRM record\", async () => {{ expect(await sync({i})).toEqual({{ id: {i} }}); }});\n"
            ));
            free_ranges.push((start, src.len() - 1));
        }
        let path = "packages/tracking/src/crm/attio-provider.test.ts";
        let x = FileExtraction {
            path: path.into(),
            language: Some(Language::TypeScript),
            content_hash: hash::sha256_hex(&src),
            line_count: src.lines().count() as u32,
            free_ranges: free_ranges.clone(),
            ..Default::default()
        };
        let input = FileInput {
            path: path.into(),
            language: Language::TypeScript,
            content: src.clone(),
        };
        let analyzed = analyze(repo(), &[(input, x)]);
        let contents = vec![(path.to_string(), src.clone())];
        let (chunks, stats) = build_chunks(repo(), &analyzed, &contents, None, 0.82)
            .await
            .unwrap();
        let heads: Vec<&Chunk> = chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::FileHead)
            .collect();
        assert_eq!(heads.len(), 1);
        assert_eq!(stats.file_heads, 1);
        assert!(!heads[0].header.contains("// part "), "{}", heads[0].header);
        assert!(heads[0].metadata.get("part").is_none());
        assert!(heads[0].body.len() <= FILE_HEAD_MAX_CHARS);
        assert!(heads[0].body.starts_with("it(\"maps contact 0 "));
        // The same body outside a test path splits.
        assert!(file_head_parts(&src, &free_ranges, FILE_HEAD_MAX_CHARS).len() > 1);
    }

    #[tokio::test]
    async fn solidity_file_head_splits_without_loss() {
        let mut src = String::from("// SPDX-License-Identifier: MIT\npragma solidity ^0.8.33;\n\nimport {CallType} from \"./Types.sol\";\n\n");
        let mut free_ranges = vec![(0, src.len() - 1)];
        for i in 0..45 {
            let start = src.len();
            src.push_str(&format!(
                "/// @dev Typehash number {i} of the DID registry link request.\nbytes32 constant DID_REGISTRY_TYPEHASH_{i:02} = keccak256(\"LinkDidRequest{i}(bytes32 oclId,bytes32 provider)\");\n"
            ));
            free_ranges.push((start, src.len() - 1));
            src.push('\n');
        }
        let start = src.len();
        src.push_str("uint256 constant MAX_LAB_MINT_FEE_WEI = 1 ether;\n");
        free_ranges.push((start, src.len() - 1));
        let x = FileExtraction {
            path: "src/types/Constants.sol".into(),
            language: Some(Language::Solidity),
            content_hash: hash::sha256_hex(&src),
            line_count: src.lines().count() as u32,
            free_ranges,
            ..Default::default()
        };
        let input = FileInput {
            path: "src/types/Constants.sol".into(),
            language: Language::Solidity,
            content: src.clone(),
        };
        let analyzed = analyze(repo(), &[(input, x)]);
        let contents = vec![("src/types/Constants.sol".to_string(), src.clone())];
        let (chunks, stats) = build_chunks(repo(), &analyzed, &contents, None, 0.82)
            .await
            .unwrap();
        let heads: Vec<&Chunk> = chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::FileHead)
            .collect();
        assert!(heads.len() > 1, "{}", heads.len());
        assert_eq!(stats.file_heads, heads.len());
        for i in 0..45 {
            let name = format!("constant DID_REGISTRY_TYPEHASH_{i:02} ");
            assert_eq!(
                heads.iter().filter(|c| c.body.contains(&name)).count(),
                1,
                "{name}"
            );
        }
        assert!(heads
            .last()
            .unwrap()
            .body
            .contains("uint256 constant MAX_LAB_MINT_FEE_WEI = 1 ether;"));
        for (i, h) in heads.iter().enumerate() {
            assert!(h.header.len() + 1 + h.body.len() <= MAX_CHUNK_CHARS);
            assert!(h.body.len() <= FILE_HEAD_MAX_CHARS);
            assert!(h
                .header
                .ends_with(&format!("// part {}/{}", i + 1, heads.len())));
            assert_eq!(h.metadata["part"], i + 1);
            assert_eq!(h.metadata["parts"], heads.len());
        }
        let ids: HashSet<Uuid> = heads.iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), heads.len());
        let (again, _) = build_chunks(repo(), &analyzed, &contents, None, 0.82)
            .await
            .unwrap();
        assert_eq!(chunks, again);
    }

    /// A head that fits keeps the historical single-chunk id
    /// (`{content_hash}:file:{path}:0`), whatever the language.
    #[tokio::test]
    async fn single_part_file_head_id_unchanged() {
        let src = "use std::fmt;\nstatic X: u8 = 1;\n";
        let x = FileExtraction {
            path: "src/s.rs".into(),
            language: Some(Language::Rust),
            content_hash: hash::sha256_hex(src),
            line_count: 2,
            free_ranges: vec![(0, 13), (14, 31)],
            ..Default::default()
        };
        let files = vec![(
            FileInput {
                path: "src/s.rs".into(),
                language: Language::Rust,
                content: src.into(),
            },
            x,
        )];
        let analyzed = analyze(repo(), &files);
        let contents = vec![("src/s.rs".to_string(), src.to_string())];
        let (chunks, _) = build_chunks(repo(), &analyzed, &contents, None, 0.82)
            .await
            .unwrap();
        assert_eq!(chunks.len(), 1);
        let c = &chunks[0];
        assert_eq!(c.body, "use std::fmt;\n…\nstatic X: u8 = 1;");
        assert!(!c.header.contains("// part"));
        assert_eq!(
            c.id,
            hash::scoped_id(
                repo(),
                &format!(
                    "{}:file:src/s.rs:0",
                    chunking::content_hash(&c.header, &c.body)
                )
            )
        );
        assert_eq!(c.metadata, json!({"imports": 0, "ranges": 2}));
    }

    /// Every language splits a long head instead of cutting it at the cap.
    #[test]
    fn long_rust_file_head_splits_too() {
        let src: String = (0..200)
            .map(|i| format!("static S{i:03}: u32 = {i};\n"))
            .collect();
        let ranges: Vec<(usize, usize)> = src
            .match_indices("static")
            .map(|(i, _)| (i, i + src[i..].find('\n').unwrap()))
            .collect();
        let parts = file_head_parts(&src, &ranges, FILE_HEAD_MAX_CHARS);
        assert!(parts.len() > 1);
        assert!(parts.last().unwrap().0.contains("static S199: u32 = 199;"));
        assert!(parts.iter().all(|(b, _, _)| b.len() <= FILE_HEAD_MAX_CHARS));
    }
}
