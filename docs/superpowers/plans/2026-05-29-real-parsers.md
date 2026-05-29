# Real-Parser Extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the regex symbol/import extraction for JavaScript, TypeScript, Python, and Solidity with pure-Rust AST parsers (oxc, rustpython-parser, solang-parser), keeping the `ExtractionResult` output contract identical.

**Architecture:** `src/extractor.rs` keeps orchestration, the Rust (`syn`) path, markdown/PDF/JSON/Cargo extraction, and call-edge assembly. Per-language AST extraction moves into a new `src/lang/` module (`mod.rs`, `javascript.rs`, `python.rs`, `solidity.rs`). A shared `LineIndex` converts AST byte spans to 1-based line numbers, replacing the `find_line`/`find_block_end` text heuristics for the parsed languages. The Rust `syn` path is untouched.

**Tech Stack:** Rust, `oxc_parser`/`oxc_ast`/`oxc_ast_visit`/`oxc_allocator`/`oxc_span`/`oxc_syntax` 0.116, `rustpython-parser` 0.4, `solang-parser` 0.3, existing `syn` 2.

**Design spec:** `docs/superpowers/specs/2026-05-29-real-parsers-design.md`

**Verified prerequisites (probe-compiled on Rust 1.91.1, edition 2021):**
- The five oxc crates + rustpython-parser + solang-parser compile together (exit 0).
- All API snippets in this plan were run in a probe binary and produced the expected symbols/lines.

---

## Conventions used in every task

- After each code change run `cargo build` before tests to catch type errors fast.
- The 11 extractor tests in `src/extractor.rs` (module `#[cfg(test)] mod tests`) are the **output contract**. They MUST stay green **unmodified** unless a task explicitly says to add/adjust a test.
- Validation gate before any commit that finishes a task: `cargo fmt`, then `cargo test`, then `cargo clippy --all-targets --all-features -- -D warnings`.
- Commit messages end with the trailer:
  `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`
- Work happens on the existing `real-parsers` branch.

---

# Phase 1 — Cleanups (no new dependencies)

## Task 1: Extract a `begin_file` helper (DRY the per-language file prelude)

**Files:**
- Modify: `src/extractor.rs` (free function near `file_node`, ~line 1682; call sites in `extract_markdown_file`, `extract_pdf_file`, `extract_rust_file`, `extract_js_ts_file`, `extract_solidity_file`, `extract_python_file`)

The repeated prelude is: read file → build `SourceFile` → `result.files.push` → build `file_node` → push `Contains` edge → `result.nodes.push(file_node)`. The `Contains` weight and metadata differ per caller (`CONTAINS_CODE {}`, `CONTAINS_DOC {"source_priority":"supplemental"}`, `CONTAINS_PDF {...}`).

- [ ] **Step 1: Add the helper**

Add this free function (place it directly above `fn file_node(`):

```rust
/// Read a source file, register it, and emit its `File` node + `Contains`
/// edge. Returns the `SourceFile` and the file node's id so callers can attach
/// symbols. Centralizes the prelude every language extractor used to repeat.
#[allow(clippy::too_many_arguments)]
fn begin_file(
    root: &Path,
    path: &Path,
    repo_id: Uuid,
    commit_sha: Option<String>,
    repo_node_id: Uuid,
    language: Language,
    contains: EdgeWeight,
    contains_meta: serde_json::Value,
    result: &mut ExtractionResult,
) -> Result<(SourceFile, Uuid)> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let rel = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();
    let file = SourceFile {
        id: Uuid::new_v4(),
        repo_id,
        commit_sha,
        path: rel.clone(),
        language,
        content: content.clone(),
        content_hash: hash(&content),
        line_count: content.lines().count() as i32,
    };
    result.files.push(file.clone());

    let file_node = file_node(repo_id, &file, &rel);
    let file_node_id = file_node.id;
    result.edges.push(edge(
        repo_id,
        repo_node_id,
        file_node_id,
        EdgeKind::Contains,
        contains,
        contains_meta,
    ));
    result.nodes.push(file_node);
    Ok((file, file_node_id))
}
```

- [ ] **Step 2: Refactor `extract_rust_file` to use it**

Replace its prelude (the `let content = …` through `result.nodes.push(file_node.clone());`) with:

```rust
let (file, file_node_id) = begin_file(
    root, path, repo_id, commit_sha, repo_node_id,
    Language::Rust, weights::CONTAINS_CODE, json!({}), result,
)?;
let content = file.content.clone();
let rel = file.path.clone();
```

Then update later references: `file_node.id` → `file_node_id`. Keep everything else (the `syn::parse_file` loop) identical.

- [ ] **Step 3: Refactor the other five extractors the same way**

Apply the identical transformation to `extract_js_ts_file` (`Language` param it already receives, `CONTAINS_CODE`, `json!({})`), `extract_solidity_file` (`Language::Solidity`, `CONTAINS_CODE`, `json!({})`), `extract_python_file` (`Language::Python`, `CONTAINS_CODE`, `json!({})`), `extract_markdown_file` (`Language::Markdown`, `CONTAINS_DOC`, `json!({"source_priority": "supplemental"})`), and `extract_pdf_file` (`Language::Pdf` — confirm the variant name in `models.rs`; `CONTAINS_PDF`, copy its existing metadata object). For markdown/PDF, keep their post-prelude chunk-building code unchanged.

- [ ] **Step 4: Build and test**

Run: `cargo build && cargo test`
Expected: compiles; all 27 tests PASS (no behavior change — same nodes/edges/chunks).

- [ ] **Step 5: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
git add src/extractor.rs
git commit -m "Refactor: extract begin_file helper for the per-language prelude

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

## Task 2: Documentation fixes

**Files:**
- Modify: `docs/CLAUDE_VALIDATION_BRIEF.md`
- Modify: `docs/PLUGIN_INSTALL.md`, `docs/PLUGIN_WORKFLOW.md`, `docs/CLAUDE_CODE_COWORK.md`, `docs/CLAUDE_MCP_INSTALL.md`, `README.md` (consolidation only)

- [ ] **Step 1: Fix the language-support list**

In `docs/CLAUDE_VALIDATION_BRIEF.md`, find the "Implemented language support" list (currently Rust/TypeScript/JavaScript only). Add two bullets so it matches the rest of the file:
- `Solidity contracts, interfaces, libraries, functions, constructors, events, modifiers, imports, and inheritance`
- `Python functions, classes, and imports (def/class/import, indentation-aware)`

- [ ] **Step 2: De-duplicate setup/manifest/hard-rules**

Pick canonical homes and cross-reference (do not delete unique content):
- Local MCP setup steps → canonical in `docs/CLAUDE_MCP_INSTALL.md`; other files link to it with a one-line pointer instead of repeating the commands.
- Feature-website manifest schema → canonical in the doc that defines it most fully (search `rg -l "manifest"`); others link.
- "Hard Rules" → canonical in `CLAUDE.md`; other copies replaced with "See Hard Rules in `CLAUDE.md`."

- [ ] **Step 3: Verify and commit**

Run: `rg -n "Implemented language support" -A 8 docs/CLAUDE_VALIDATION_BRIEF.md`
Expected: Solidity and Python bullets present.

```bash
git add docs/ README.md
git commit -m "Docs: list Solidity/Python support and de-duplicate setup guides

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

# Phase 2 — Parser swap

## Task 3: Add dependencies and the `lang` module skeleton

**Files:**
- Modify: `Cargo.toml`
- Create: `src/lang/mod.rs`
- Modify: `src/main.rs` (add `mod lang;`)
- Modify: `src/extractor.rs` (make shared helpers `pub(crate)`)

- [ ] **Step 1: Add dependencies**

In `Cargo.toml` `[dependencies]`, add:

```toml
oxc_allocator = "0.116"
oxc_ast = "0.116"
oxc_ast_visit = "0.116"
oxc_parser = "0.116"
oxc_span = "0.116"
oxc_syntax = "0.116"
rustpython-parser = "0.4"
solang-parser = "0.3"
```

Run: `cargo build`
Expected: dependencies resolve and the crate still compiles (no usage yet).

- [ ] **Step 2: Expose shared helpers to the new module**

In `src/extractor.rs`, change these free functions from private to `pub(crate)`: `fn edge(` → `pub(crate) fn edge(`, `fn chunk_for_node(` → `pub(crate) fn chunk_for_node(`, `fn file_node(` (not needed by lang, leave), `fn hash(` → `pub(crate) fn hash(`, `fn is_bare_module_specifier(` → `pub(crate)`, `fn import_stable_id(` → `pub(crate)`, `fn is_test_symbol(` → `pub(crate)`, `fn is_python_test_file(` → `pub(crate)`, `fn is_js_ts_test_file(` → `pub(crate)`, `fn slice_lines(` → `pub(crate)`. Also make `const MAX_CHUNK_CHARS` stay private (unused by lang).

- [ ] **Step 3: Write the LineIndex unit test (TDD)**

Create `src/lang/mod.rs` with the test first:

```rust
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
    /// Byte offset of the start of each line.
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
    pub(crate) fn line(&self, byte_offset: usize) -> usize {
        match self.line_starts.binary_search(&byte_offset) {
            Ok(idx) => idx + 1,
            Err(idx) => idx, // idx = number of line starts <= offset
        }
    }
}

/// Per-file extraction context shared by the language submodules.
pub(crate) struct FileExtraction<'a> {
    pub repo_id: Uuid,
    pub file: &'a SourceFile,
    pub file_node_id: Uuid,
    pub lines: LineIndex,
    pub symbol_names: &'a mut HashMap<String, Uuid>,
    pub result: &'a mut ExtractionResult,
}

#[cfg(test)]
mod tests {
    use super::LineIndex;

    #[test]
    fn line_index_maps_offsets_to_one_based_lines() {
        let src = "alpha\nbeta\ngamma\n";
        let idx = LineIndex::new(src);
        assert_eq!(idx.line(0), 1); // 'a' of alpha
        assert_eq!(idx.line(6), 2); // 'b' of beta
        assert_eq!(idx.line(11), 3); // 'g' of gamma
        assert_eq!(idx.line(src.len()), 4); // trailing position
    }
}
```

Add empty submodule files so the `mod` lines resolve:
- `src/lang/javascript.rs`: `//! oxc-based JS/TS extraction.` (plus `use` lines added in Task 6)
- `src/lang/python.rs`: `//! rustpython-parser-based Python extraction.`
- `src/lang/solidity.rs`: `//! solang-parser-based Solidity extraction.`

- [ ] **Step 4: Wire the module in**

In `src/main.rs`, add `mod lang;` next to the other `mod` declarations.

- [ ] **Step 5: Run the LineIndex test**

Run: `cargo test lang::tests::line_index_maps_offsets_to_one_based_lines`
Expected: PASS. Also `cargo build` clean.

- [ ] **Step 6: Commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
git add Cargo.toml Cargo.lock src/main.rs src/lang/ src/extractor.rs
git commit -m "Add lang module skeleton, LineIndex, and parser dependencies

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

## Task 4: Python extraction via rustpython-parser

**Files:**
- Modify: `src/lang/python.rs`
- Modify: `src/extractor.rs` (`extract_python_file` calls `lang::python::extract`; delete `extract_python_imports`/`extract_python_symbols` and `find_python_block_end`)

Contract to preserve (from current `extract_python_symbols`/`extract_python_imports`):
- Function/method → `NodeKind::Function`, stable_id `"{path}:function:{name}"`, `Contains`/`CONTAINS_CODE`, chunk metadata `{"language":"python","python_kind":"function"}`.
- Class → `NodeKind::Struct`, python_kind `"class"`.
- Test detection: if `is_python_test_file(path) || is_test_symbol(name)` → `NodeKind::Test`.
- Import → `NodeKind::Dependency`, stable_id via `import_stable_id`, `Imports`/`IMPORTS_MODULE`, metadata `{"module","language":"python","scope": bare|relative}`; relative form keeps leading dots; `file_id`/`line` are `None` for bare specifiers.
- Chunk text format: `"Language: python\nFile: {path}\nSymbol: {name}\nKind: {python_kind}\nLines: {start}-{end}\n\n{code}"`.

- [ ] **Step 1: Write the new fidelity test (TDD)**

Add to `src/extractor.rs` `mod tests` (it already has `extracts_python_functions_classes_and_imports`):

```rust
#[test]
fn python_captures_methods_and_relative_imports() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("svc.py"),
        "from .util import helper\nclass Service:\n    def run(self):\n        return helper()\n",
    )
    .unwrap();
    let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
    let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

    assert!(result.nodes.iter().any(|n| n.name == "Service" && n.kind == NodeKind::Struct));
    assert!(result.nodes.iter().any(|n| n.name == "run" && n.kind == NodeKind::Function));
    assert!(result
        .nodes
        .iter()
        .any(|n| n.kind == NodeKind::Dependency && n.name == ".util"));
}
```

- [ ] **Step 2: Run it — verify it fails for the right reason**

Run: `cargo test python_captures_methods_and_relative_imports`
Expected: this may already PASS on the current regex path (its `^[ \t]*def` matches nested methods and it captures `.util`). That is fine — it serves as a regression guard that the new AST path preserves the same behavior. The genuinely new coverage is the fidelity gained elsewhere (arrow functions in JS, accurate spans); for Python the AST path's value is correctness on edge cases the regex mishandles (e.g. `def` inside strings/decorated defs).

- [ ] **Step 3: Implement the Python extractor**

Replace the contents of `src/lang/python.rs` with (verified against rustpython-parser 0.4):

```rust
//! rustpython-parser-based Python extraction.

use crate::extractor::{chunk_for_node, edge, import_stable_id, is_bare_module_specifier,
    is_python_test_file, is_test_symbol, slice_lines};
use crate::lang::FileExtraction;
use crate::models::{EdgeKind, KnowledgeNode, NodeKind};
use crate::weights;
use anyhow::Result;
use rustpython_parser::{ast, Parse};
use serde_json::json;
use uuid::Uuid;

pub(crate) fn extract(ctx: &mut FileExtraction) -> Result<()> {
    let content = ctx.file.content.clone();
    let suite = match ast::Suite::parse(&content, &ctx.file.path) {
        Ok(suite) => suite,
        Err(err) => {
            tracing_warn(ctx, &format!("python parse failed: {err}"));
            return Ok(()); // graceful per-file degrade (file node already emitted)
        }
    };
    let mut imports = Vec::new();
    let mut symbols = Vec::new();
    walk(&suite, &mut imports, &mut symbols);

    for imp in imports {
        emit_import(ctx, &imp);
    }
    for sym in symbols {
        emit_symbol(ctx, &sym);
    }
    Ok(())
}

struct PyImport {
    module: String,
    offset: usize,
}
struct PySymbol {
    name: String,
    python_kind: &'static str,
    base_kind: NodeKind,
    start: usize,
    end: usize,
}

fn walk(stmts: &[ast::Stmt], imports: &mut Vec<PyImport>, symbols: &mut Vec<PySymbol>) {
    for stmt in stmts {
        match stmt {
            ast::Stmt::FunctionDef(f) => {
                push_symbol(symbols, f.name.as_str(), "function", NodeKind::Function, &f.range);
                walk(&f.body, imports, symbols);
            }
            ast::Stmt::AsyncFunctionDef(f) => {
                push_symbol(symbols, f.name.as_str(), "function", NodeKind::Function, &f.range);
                walk(&f.body, imports, symbols);
            }
            ast::Stmt::ClassDef(c) => {
                push_symbol(symbols, c.name.as_str(), "class", NodeKind::Struct, &c.range);
                walk(&c.body, imports, symbols);
            }
            ast::Stmt::Import(i) => {
                for alias in &i.names {
                    imports.push(PyImport {
                        module: alias.name.to_string(),
                        offset: i.range.start().to_usize(),
                    });
                }
            }
            ast::Stmt::ImportFrom(i) => {
                let dots = i.level.map(|l| ".".repeat(l.to_usize())).unwrap_or_default();
                let module = format!("{dots}{}", i.module.as_ref().map(|m| m.as_str()).unwrap_or(""));
                imports.push(PyImport { module, offset: i.range.start().to_usize() });
            }
            _ => {}
        }
    }
}

fn push_symbol(
    out: &mut Vec<PySymbol>,
    name: &str,
    python_kind: &'static str,
    base_kind: NodeKind,
    range: &ast::text_size::TextRange,
) {
    out.push(PySymbol {
        name: name.to_string(),
        python_kind,
        base_kind,
        start: range.start().to_usize(),
        end: range.end().to_usize(),
    });
}
```

> NOTE on `TextRange` path: the probe used `f.range.start().to_usize()` directly with no extra import; if the `range:` parameter type above does not resolve, take `start`/`end` in `walk` (where the concrete node is in scope) and store the offsets, dropping the `range` parameter. The offsets are what matter.

Now the two emit helpers (copy field-for-field from the current `extract_python_symbols`/`extract_python_imports`, sourcing line from `ctx.lines`):

```rust
fn emit_symbol(ctx: &mut FileExtraction, sym: &PySymbol) {
    let line = ctx.lines.line(sym.start);
    let end = ctx.lines.line(sym.end);
    let code = slice_lines(&ctx.file.content, line, end);
    let kind = if is_python_test_file(&ctx.file.path) || is_test_symbol(&sym.name) {
        NodeKind::Test
    } else {
        sym.base_kind.clone()
    };
    let node = KnowledgeNode {
        id: Uuid::new_v4(),
        repo_id: ctx.repo_id,
        file_id: Some(ctx.file.id),
        kind: kind.clone(),
        stable_id: format!("{}:{}:{}", ctx.file.path, kind.as_str(), sym.name),
        name: sym.name.clone(),
        line_start: Some(line as i32),
        line_end: Some(end as i32),
        metadata: json!({"language":"python","file":ctx.file.path,"python_kind":sym.python_kind}),
    };
    ctx.symbol_names.entry(sym.name.clone()).or_insert(node.id);
    ctx.result.edges.push(edge(
        ctx.repo_id, ctx.file_node_id, node.id, EdgeKind::Contains,
        weights::CONTAINS_CODE, json!({"language":"python","kind":sym.python_kind}),
    ));
    ctx.result.chunks.push(chunk_for_node(
        ctx.repo_id, Some(ctx.file.id), Some(node.id), kind.as_str(),
        &format!("Language: python\nFile: {}\nSymbol: {}\nKind: {}\nLines: {}-{}\n\n{}",
            ctx.file.path, sym.name, sym.python_kind, line, end, code),
        Some(line as i32), Some(end as i32),
        json!({"symbol":sym.name,"kind":kind.as_str(),"python_kind":sym.python_kind,"file":ctx.file.path}),
    ));
    ctx.result.nodes.push(node);
}

fn emit_import(ctx: &mut FileExtraction, imp: &PyImport) {
    let module = imp.module.trim();
    if module.is_empty() { return; }
    let line = ctx.lines.line(imp.offset) as i32;
    let is_bare = is_bare_module_specifier(module);
    let node = KnowledgeNode {
        id: Uuid::new_v4(),
        repo_id: ctx.repo_id,
        file_id: if is_bare { None } else { Some(ctx.file.id) },
        kind: NodeKind::Dependency,
        stable_id: import_stable_id(ctx.file, module, is_bare),
        name: module.to_string(),
        line_start: if is_bare { None } else { Some(line) },
        line_end: if is_bare { None } else { Some(line) },
        metadata: json!({"module":module,"language":"python","scope": if is_bare {"bare"} else {"relative"}}),
    };
    ctx.result.edges.push(edge(
        ctx.repo_id, ctx.file_node_id, node.id, EdgeKind::Imports,
        weights::IMPORTS_MODULE,
        json!({"file":ctx.file.path,"module":module,"line":line}),
    ));
    ctx.result.nodes.push(node);
}

fn tracing_warn(ctx: &FileExtraction, msg: &str) {
    eprintln!("[chaos-substrate] {}: {msg}", ctx.file.path);
}
```

> The `tracing_warn` shim keeps Task 4 self-contained; Task 10 replaces all per-file warnings with the project's chosen logging once the degrade policy lands. If the repo already uses `eprintln!`/a logger for warnings, match that instead.

- [ ] **Step 4: Rewire `extract_python_file`**

In `src/extractor.rs`, after `begin_file` produces `(file, file_node_id)`, replace the two `self.extract_python_*` calls with:

```rust
let mut ctx = crate::lang::FileExtraction {
    repo_id,
    file: &file,
    file_node_id,
    lines: crate::lang::LineIndex::new(&file.content),
    symbol_names,
    result,
};
crate::lang::python::extract(&mut ctx)?;
```

Delete `fn extract_python_imports`, `fn extract_python_symbols`, and `fn find_python_block_end` (now unused). Make `LineIndex`, `FileExtraction` `pub(crate)` (already are per Task 3).

- [ ] **Step 5: Run the full suite**

Run: `cargo test`
Expected: all existing Python tests (`extracts_python_functions_classes_and_imports`, `detects_python_extensions`) PASS plus the new `python_captures_methods_and_relative_imports`. If a stable_id/metadata field drifts, diff against the deleted regex code and align.

- [ ] **Step 6: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
git add src/lang/python.rs src/extractor.rs
git commit -m "Replace Python regex extraction with rustpython-parser AST

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

## Task 5: Solidity extraction via solang-parser

**Files:**
- Modify: `src/lang/solidity.rs`
- Modify: `src/extractor.rs` (`extract_solidity_file` calls `lang::solidity::extract`; delete `extract_solidity_imports`/`extract_solidity_symbols`/`add_solidity_inheritance_edges`)

Contract to preserve (from current `extract_solidity_symbols`/`_imports`):
- contract→`Struct`, interface→`Trait`, library→`Module`; stable_id `"{path}:solidity:{kind_text}:{name}"`; `Defines`/`DEFINES_SYMBOL`; metadata `{"language":"solidity","file","solidity_kind":kind_text}`; chunk header `"Language: solidity\nFile: …\nSymbol: …\nSolidity kind: {kind_text}\nLines: …\n\n{code}"`.
- members: function/constructor/fallback/receive → `Function`; event/modifier → `Concept`; stable_id `"{path}:solidity:{solidity_kind}:{name}"`; `Contains`/`CONTAINS_MEMBER`.
- inheritance: each base → `Implements`/`IMPLEMENTS`, target node stable_id `"solidity:inheritance:{base}"`, `NodeKind::Concept`.
- imports: `Dependency`, stable_id via `import_stable_id`, `Imports`/`IMPORTS_SOLIDITY`.

- [ ] **Step 1: Add a fidelity test (TDD)**

Add to `mod tests` (alongside `extracts_solidity_contracts_members_imports_and_inheritance`):

```rust
#[test]
fn solidity_members_resolve_within_contract() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Vault.sol"),
        "interface IVault { function deposit() external; }\ncontract Vault is IVault, Pausable {\n    function deposit() public {}\n}\n",
    )
    .unwrap();
    let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
    let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();

    assert!(result.nodes.iter().any(|n| n.name == "Vault" && n.kind == NodeKind::Struct));
    assert!(result.nodes.iter().any(|n| n.name == "IVault" && n.kind == NodeKind::Trait));
    let bases: Vec<&str> = result.nodes.iter()
        .filter(|n| n.metadata.get("relationship").and_then(|v| v.as_str()) == Some("inheritance"))
        .map(|n| n.name.as_str()).collect();
    assert!(bases.contains(&"IVault") && bases.contains(&"Pausable"));
}
```

- [ ] **Step 2: Run it**

Run: `cargo test solidity_members_resolve_within_contract`
Expected: PASS or FAIL depending on current regex behavior — either way it becomes a regression guard for the AST path.

- [ ] **Step 3: Implement the Solidity extractor**

Replace `src/lang/solidity.rs` with (verified against solang-parser 0.3.5):

```rust
//! solang-parser-based Solidity extraction.

use crate::extractor::{chunk_for_node, edge, import_stable_id, is_bare_module_specifier, slice_lines};
use crate::lang::FileExtraction;
use crate::models::{EdgeKind, KnowledgeNode, NodeKind};
use crate::weights;
use anyhow::Result;
use serde_json::json;
use solang_parser::pt::{ContractPart, ContractTy, FunctionTy, Import, ImportPath, SourceUnitPart};
use uuid::Uuid;

pub(crate) fn extract(ctx: &mut FileExtraction) -> Result<()> {
    let content = ctx.file.content.clone();
    let unit = match solang_parser::parse(&content, 0) {
        Ok((unit, _comments)) => unit,
        Err(diags) => {
            eprintln!("[chaos-substrate] {}: solidity parse failed ({} diagnostics)",
                ctx.file.path, diags.len());
            return Ok(()); // graceful per-file degrade
        }
    };

    for part in &unit.0 {
        match part {
            SourceUnitPart::ImportDirective(import) => emit_import(ctx, import),
            SourceUnitPart::ContractDefinition(contract) => {
                let kind_text = match contract.ty {
                    ContractTy::Interface(_) => "interface",
                    ContractTy::Library(_) => "library",
                    ContractTy::Abstract(_) | ContractTy::Contract(_) => "contract",
                };
                let node_kind = match kind_text {
                    "interface" => NodeKind::Trait,
                    "library" => NodeKind::Module,
                    _ => NodeKind::Struct,
                };
                let Some(name) = contract.name.as_ref().map(|n| n.name.clone()) else { continue };
                let start = ctx.lines.line(contract.loc.start());
                let end = ctx.lines.line(contract.loc.end());
                let code = slice_lines(&ctx.file.content, start, end);
                let node = KnowledgeNode {
                    id: Uuid::new_v4(),
                    repo_id: ctx.repo_id,
                    file_id: Some(ctx.file.id),
                    kind: node_kind.clone(),
                    stable_id: format!("{}:solidity:{}:{}", ctx.file.path, kind_text, name),
                    name: name.clone(),
                    line_start: Some(start as i32),
                    line_end: Some(end as i32),
                    metadata: json!({"language":"solidity","file":ctx.file.path,"solidity_kind":kind_text}),
                };
                let contract_node_id = node.id;
                ctx.symbol_names.entry(name.clone()).or_insert(node.id);
                ctx.result.edges.push(edge(
                    ctx.repo_id, ctx.file_node_id, node.id, EdgeKind::Defines,
                    weights::DEFINES_SYMBOL, json!({"language":"solidity","kind":kind_text}),
                ));
                ctx.result.chunks.push(chunk_for_node(
                    ctx.repo_id, Some(ctx.file.id), Some(node.id), node_kind.as_str(),
                    &format!("Language: solidity\nFile: {}\nSymbol: {}\nSolidity kind: {}\nLines: {}-{}\n\n{}",
                        ctx.file.path, name, kind_text, start, end, code),
                    Some(start as i32), Some(end as i32),
                    json!({"symbol":name,"kind":node_kind.as_str(),"solidity_kind":kind_text,"file":ctx.file.path}),
                ));
                ctx.result.nodes.push(node);

                for base in &contract.base {
                    emit_inheritance(ctx, contract_node_id, &base.name.to_string());
                }
                for cp in &contract.parts {
                    emit_member(ctx, cp);
                }
            }
            _ => {}
        }
    }
    Ok(())
}
```

Then the member/import/inheritance helpers (member kinds map to the current `member_patterns`):

```rust
fn emit_member(ctx: &mut FileExtraction, part: &ContractPart) {
    let (solidity_kind, base_kind, name, loc) = match part {
        ContractPart::FunctionDefinition(f) => {
            let kind = match f.ty {
                FunctionTy::Constructor => "constructor",
                FunctionTy::Fallback => "fallback",
                FunctionTy::Receive => "fallback", // current code groups receive under fallback pattern
                FunctionTy::Modifier => "modifier",
                FunctionTy::Function => "function",
            };
            let node_kind = if matches!(f.ty, FunctionTy::Modifier) { NodeKind::Concept } else { NodeKind::Function };
            let nm = match f.ty {
                FunctionTy::Constructor => "constructor".to_string(),
                _ => f.name.as_ref().map(|n| n.name.clone()).unwrap_or_default(),
            };
            (kind, node_kind, nm, f.loc)
        }
        ContractPart::EventDefinition(e) => {
            ("event", NodeKind::Concept, e.name.as_ref().map(|n| n.name.clone()).unwrap_or_default(), e.loc)
        }
        _ => return,
    };
    if name.is_empty() { return; }
    let start = ctx.lines.line(loc.start());
    let end = ctx.lines.line(loc.end());
    let code = slice_lines(&ctx.file.content, start, end);
    let node = KnowledgeNode {
        id: Uuid::new_v4(),
        repo_id: ctx.repo_id,
        file_id: Some(ctx.file.id),
        kind: base_kind.clone(),
        stable_id: format!("{}:solidity:{}:{}", ctx.file.path, solidity_kind, name),
        name: name.clone(),
        line_start: Some(start as i32),
        line_end: Some(end as i32),
        metadata: json!({"language":"solidity","file":ctx.file.path,"solidity_kind":solidity_kind}),
    };
    ctx.symbol_names.entry(name.clone()).or_insert(node.id);
    ctx.result.edges.push(edge(
        ctx.repo_id, ctx.file_node_id, node.id, EdgeKind::Contains,
        weights::CONTAINS_MEMBER, json!({"language":"solidity","kind":solidity_kind}),
    ));
    ctx.result.chunks.push(chunk_for_node(
        ctx.repo_id, Some(ctx.file.id), Some(node.id), base_kind.as_str(),
        &format!("Language: solidity\nFile: {}\nSymbol: {}\nSolidity kind: {}\nLines: {}-{}\n\n{}",
            ctx.file.path, name, solidity_kind, start, end, code),
        Some(start as i32), Some(end as i32),
        json!({"symbol":name,"kind":base_kind.as_str(),"solidity_kind":solidity_kind,"file":ctx.file.path}),
    ));
    ctx.result.nodes.push(node);
}

fn emit_inheritance(ctx: &mut FileExtraction, contract_node_id: Uuid, base: &str) {
    if base.is_empty() { return; }
    let node = KnowledgeNode {
        id: Uuid::new_v4(),
        repo_id: ctx.repo_id,
        file_id: None,
        kind: NodeKind::Concept,
        stable_id: format!("solidity:inheritance:{base}"),
        name: base.to_string(),
        line_start: None,
        line_end: None,
        metadata: json!({"language":"solidity","relationship":"inheritance"}),
    };
    ctx.result.edges.push(edge(
        ctx.repo_id, contract_node_id, node.id, EdgeKind::Implements,
        weights::IMPLEMENTS, json!({"language":"solidity","base":base}),
    ));
    ctx.result.nodes.push(node);
}

fn emit_import(ctx: &mut FileExtraction, import: &Import) {
    let (path, loc) = match import {
        Import::Plain(p, loc) => (p, loc),
        Import::Rename(p, _, loc) => (p, loc),
        Import::GlobalSymbol(p, _, loc) => (p, loc),
    };
    let module = match path {
        ImportPath::Filename(s) => s.string.clone(),
        ImportPath::Path(p) => p.to_string(),
    };
    if module.is_empty() { return; }
    let line = ctx.lines.line(loc.start()) as i32;
    let is_bare = is_bare_module_specifier(&module);
    let node = KnowledgeNode {
        id: Uuid::new_v4(),
        repo_id: ctx.repo_id,
        file_id: if is_bare { None } else { Some(ctx.file.id) },
        kind: NodeKind::Dependency,
        stable_id: import_stable_id(ctx.file, &module, is_bare),
        name: module.clone(),
        line_start: if is_bare { None } else { Some(line) },
        line_end: if is_bare { None } else { Some(line) },
        metadata: json!({"module":module,"language":"solidity","scope": if is_bare {"bare"} else {"relative"}}),
    };
    ctx.result.edges.push(edge(
        ctx.repo_id, ctx.file_node_id, node.id, EdgeKind::Imports,
        weights::IMPORTS_SOLIDITY, json!({"file":ctx.file.path,"module":module,"line":line}),
    ));
    ctx.result.nodes.push(node);
}
```

> CONTRACT CHECK: the current code emits `fallback`/`receive` via one regex labeled `"fallback"`, and uses a separate `"constructor"` label. The mapping above matches that. If the existing Solidity test asserts a specific `solidity_kind` for `receive`, adjust the `FunctionTy::Receive` arm to `"receive"` to match — verify by reading the test before finalizing.

- [ ] **Step 4: Rewire `extract_solidity_file`** (same `FileExtraction` construction as Task 4 Step 4, calling `crate::lang::solidity::extract`). Delete `extract_solidity_imports`, `extract_solidity_symbols`, `add_solidity_inheritance_edges`.

- [ ] **Step 5: Test**

Run: `cargo test`
Expected: `extracts_solidity_contracts_members_imports_and_inheritance` + the new test PASS. Reconcile any field drift against the deleted code.

- [ ] **Step 6: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
git add src/lang/solidity.rs src/extractor.rs
git commit -m "Replace Solidity regex extraction with solang-parser AST

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

## Task 6: JS/TS extraction via oxc

**Files:**
- Modify: `src/lang/javascript.rs`
- Modify: `src/extractor.rs` (`extract_js_ts_file` calls `lang::javascript::extract`; delete `extract_js_ts_imports`/`extract_js_ts_symbols` and the `JS_TS_SYMBOL_PATTERNS` static)

Contract to preserve (from current `extract_js_ts_symbols`/`_imports`): function (incl. arrow/const)→`Function`; class→`Struct`; interface→`Trait`; enum→`Enum`; type alias→`TypeAlias`; stable_id `"{path}:{kind}:{name}"`; `Contains`/`CONTAINS_CODE`; test detection via `is_js_ts_test_file`/`is_test_symbol`; chunk header `"Language: {lang}\nFile: …\nSymbol: …\nKind: …\nLines: …\n\n{code}"`; imports → `Dependency`/`IMPORTS_MODULE`. **Improvement:** class methods are now captured (the spec's new behavior).

- [ ] **Step 1: Add fidelity tests (TDD)**

```rust
#[test]
fn javascript_captures_methods_and_arrow_functions() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("widget.ts"),
        "import { dep } from \"./dep\";\nexport const make = () => 1;\nexport class Widget {\n  render() { return make(); }\n}\n",
    )
    .unwrap();
    let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
    let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();
    assert!(result.nodes.iter().any(|n| n.name == "make" && n.kind == NodeKind::Function));
    assert!(result.nodes.iter().any(|n| n.name == "Widget" && n.kind == NodeKind::Struct));
    assert!(result.nodes.iter().any(|n| n.name == "render" && n.kind == NodeKind::Function));
    assert!(result.nodes.iter().any(|n| n.kind == NodeKind::Dependency && n.name == "./dep"));
}
```

- [ ] **Step 2: Run it**

Run: `cargo test javascript_captures_methods_and_arrow_functions`
Expected: FAIL on the `render` method assertion (the regex never captured methods).

- [ ] **Step 3: Implement the JS/TS extractor**

Replace `src/lang/javascript.rs` with the verified visitor (compiled in the probe). The collector gathers symbols, imports, and call sites; the `Visit` trait auto-descends into exports, class bodies, and nested scopes:

```rust
//! oxc-based JS/TS extraction.

use crate::extractor::{chunk_for_node, edge, import_stable_id, is_bare_module_specifier,
    is_js_ts_test_file, is_test_symbol, slice_lines};
use crate::lang::FileExtraction;
use crate::models::{EdgeKind, KnowledgeNode, NodeKind};
use crate::weights;
use anyhow::Result;
use oxc_allocator::Allocator;
use oxc_ast::ast::{BindingPattern, CallExpression, Class, Expression, Function, ImportDeclaration,
    MethodDefinition, Statement, TSEnumDeclaration, TSInterfaceDeclaration, TSTypeAliasDeclaration,
    VariableDeclarator};
use oxc_ast_visit::{walk, Visit};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};
use oxc_syntax::scope::ScopeFlags;
use serde_json::json;
use uuid::Uuid;

struct Symbol { name: String, kind: NodeKind, start: u32 }
struct Import { module: String, start: u32 }

#[derive(Default)]
struct Collector {
    symbols: Vec<Symbol>,
    imports: Vec<Import>,
    calls: Vec<String>,
}

impl<'a> Visit<'a> for Collector {
    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        if let Some(id) = &it.id {
            self.symbols.push(Symbol { name: id.name.to_string(), kind: NodeKind::Function, start: it.span.start });
        }
        walk::walk_function(self, it, flags);
    }
    fn visit_variable_declarator(&mut self, it: &VariableDeclarator<'a>) {
        let is_fn = matches!(it.init.as_ref(),
            Some(Expression::ArrowFunctionExpression(_)) | Some(Expression::FunctionExpression(_)));
        if is_fn {
            if let BindingPattern::BindingIdentifier(id) = &it.id {
                self.symbols.push(Symbol { name: id.name.to_string(), kind: NodeKind::Function, start: it.span.start });
            }
        }
        walk::walk_variable_declarator(self, it);
    }
    fn visit_class(&mut self, it: &Class<'a>) {
        if let Some(id) = &it.id {
            self.symbols.push(Symbol { name: id.name.to_string(), kind: NodeKind::Struct, start: it.span.start });
        }
        walk::walk_class(self, it);
    }
    fn visit_method_definition(&mut self, it: &MethodDefinition<'a>) {
        if let Some(name) = it.key.static_name() {
            self.symbols.push(Symbol { name: name.to_string(), kind: NodeKind::Function, start: it.span().start });
        }
        walk::walk_method_definition(self, it);
    }
    fn visit_ts_interface_declaration(&mut self, it: &TSInterfaceDeclaration<'a>) {
        self.symbols.push(Symbol { name: it.id.name.to_string(), kind: NodeKind::Trait, start: it.span.start });
        walk::walk_ts_interface_declaration(self, it);
    }
    fn visit_ts_enum_declaration(&mut self, it: &TSEnumDeclaration<'a>) {
        self.symbols.push(Symbol { name: it.id.name.to_string(), kind: NodeKind::Enum, start: it.span.start });
        walk::walk_ts_enum_declaration(self, it);
    }
    fn visit_ts_type_alias_declaration(&mut self, it: &TSTypeAliasDeclaration<'a>) {
        self.symbols.push(Symbol { name: it.id.name.to_string(), kind: NodeKind::TypeAlias, start: it.span.start });
        walk::walk_ts_type_alias_declaration(self, it);
    }
    fn visit_import_declaration(&mut self, it: &ImportDeclaration<'a>) {
        self.imports.push(Import { module: it.source.value.to_string(), start: it.span.start });
        walk::walk_import_declaration(self, it);
    }
    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        if let Some(ident) = it.callee.get_identifier_reference() {
            self.calls.push(ident.name.to_string());
        }
        walk::walk_call_expression(self, it);
    }
}

pub(crate) fn extract(ctx: &mut FileExtraction) -> Result<()> {
    let content = ctx.file.content.clone();
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(&ctx.file.path).unwrap_or_default();
    let ret = Parser::new(&allocator, &content, source_type).parse();
    if !ret.errors.is_empty() && ret.program.body.is_empty() {
        eprintln!("[chaos-substrate] {}: js/ts parse failed ({} errors)", ctx.file.path, ret.errors.len());
        return Ok(()); // graceful per-file degrade
    }
    let mut collector = Collector::default();
    collector.visit_program(&ret.program);

    for sym in &collector.symbols { emit_symbol(ctx, sym); }
    for imp in &collector.imports { emit_import(ctx, imp); }
    super::javascript_cdk::extract(ctx, &ret.program); // Task 7 wires this; remove if Task 7 not yet done
    Ok(())
}
```

> Until Task 7 lands, drop the `super::javascript_cdk::extract` line (keep CDK on the old regex path by leaving `extract_aws_cdk_knowledge` called from `extract_js_ts_file`). Re-add CDK via AST in Task 7. Pick ONE: either remove that line now, or do Tasks 6+7 together.

The two emit helpers mirror the current code (note `start: u32` → `usize`):

```rust
fn emit_symbol(ctx: &mut FileExtraction, sym: &Symbol) {
    let line = ctx.lines.line(sym.start as usize);
    // Symbol end: oxc spans cover the whole node; for parity with the old
    // find_block_end heuristic, reuse slice_lines from line to the node end.
    // Use the function/class span end when available; fall back to `line`.
    let end = line; // refined below if span end is threaded through (see NOTE)
    let kind = if is_js_ts_test_file(&ctx.file.path) || is_test_symbol(&sym.name) {
        NodeKind::Test
    } else {
        sym.kind.clone()
    };
    let code = slice_lines(&ctx.file.content, line, end);
    let node = KnowledgeNode {
        id: Uuid::new_v4(),
        repo_id: ctx.repo_id,
        file_id: Some(ctx.file.id),
        kind: kind.clone(),
        stable_id: format!("{}:{}:{}", ctx.file.path, kind.as_str(), sym.name),
        name: sym.name.clone(),
        line_start: Some(line as i32),
        line_end: Some(end as i32),
        metadata: json!({"language":ctx.file.language.as_str(),"file":ctx.file.path}),
    };
    ctx.symbol_names.entry(sym.name.clone()).or_insert(node.id);
    ctx.result.edges.push(edge(
        ctx.repo_id, ctx.file_node_id, node.id, EdgeKind::Contains, weights::CONTAINS_CODE, json!({}),
    ));
    ctx.result.chunks.push(chunk_for_node(
        ctx.repo_id, Some(ctx.file.id), Some(node.id), kind.as_str(),
        &format!("Language: {}\nFile: {}\nSymbol: {}\nKind: {}\nLines: {}-{}\n\n{}",
            ctx.file.language.as_str(), ctx.file.path, sym.name, kind.as_str(), line, end, code),
        Some(line as i32), Some(end as i32),
        json!({"symbol":sym.name,"kind":kind.as_str(),"file":ctx.file.path}),
    ));
    ctx.result.nodes.push(node);
}

fn emit_import(ctx: &mut FileExtraction, imp: &Import) {
    let module = imp.module.trim();
    if module.is_empty() { return; }
    let line = ctx.lines.line(imp.start as usize) as i32;
    let is_bare = is_bare_module_specifier(module);
    let node = KnowledgeNode {
        id: Uuid::new_v4(),
        repo_id: ctx.repo_id,
        file_id: if is_bare { None } else { Some(ctx.file.id) },
        kind: NodeKind::Dependency,
        stable_id: import_stable_id(ctx.file, module, is_bare),
        name: module.to_string(),
        line_start: if is_bare { None } else { Some(line) },
        line_end: if is_bare { None } else { Some(line) },
        metadata: json!({"module":module,"language":ctx.file.language.as_str(),"scope": if is_bare {"bare"} else {"relative"}}),
    };
    ctx.result.edges.push(edge(
        ctx.repo_id, ctx.file_node_id, node.id, EdgeKind::Imports, weights::IMPORTS_MODULE,
        json!({"file":ctx.file.path,"module":module,"line":line}),
    ));
    ctx.result.nodes.push(node);
}
```

> NOTE (line_end fidelity): the old regex used `find_block_end` from the matched line. To preserve `line_end`, store the node's span END alongside `start` in `Symbol` (`end: u32`, from `it.span.end` / `it.span().end`), then `let end = ctx.lines.line(sym.end as usize);`. This is the recommended approach — add the `end` field and drop the `end = line` placeholder. (Methods/classes have accurate end spans in oxc.) Verify the `extracts_typescript_symbols_and_package_metadata` test still passes either way; if it asserts exact `line_end`, the span-end approach is required.

- [ ] **Step 4: Rewire `extract_js_ts_file`** (build `FileExtraction`, call `crate::lang::javascript::extract`). Delete `extract_js_ts_imports`, `extract_js_ts_symbols`, and the `JS_TS_SYMBOL_PATTERNS` static.

- [ ] **Step 5: Test**

Run: `cargo test`
Expected: `extracts_typescript_symbols_and_package_metadata`, `detects_javascript_extensions` + the new test PASS.

- [ ] **Step 6: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
git add src/lang/javascript.rs src/extractor.rs
git commit -m "Replace JS/TS regex extraction with oxc AST (captures methods)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

## Task 7: AWS-CDK detection via the oxc AST

**Files:**
- Modify: `src/lang/javascript.rs` (add CDK detection reusing the parsed program)
- Modify: `src/extractor.rs` (delete `extract_aws_cdk_knowledge`, `looks_like_cdk_file` regex pieces, `cdk_service` stays)

Contract to preserve (from current `extract_aws_cdk_knowledge`): stack = `class X extends *.Stack` → `DeploymentResource`, stable_id `"{path}:aws-cdk:stack:{name}"`, `Defines`/`DEFINES_SYMBOL`, chunk_type `"aws_cdk_stack"`. construct = `new *.Construct(this, "id", …)` → `DeploymentResource`, stable_id `"{path}:aws-cdk:resource:{construct_type}:{logical_id}"`, name `"{construct_type} {logical_id}"`, `Configures`/`CONFIGURES`, metadata with `construct_type`/`logical_id`/`service` (via existing `cdk_service`), chunk_type `"aws_cdk_resource"`.

- [ ] **Step 1: Keep the existing CDK test as the contract**

`extracts_aws_cdk_stacks_and_resources` must pass unchanged. Read it first to confirm exact asserted fields (chunk_type, metadata keys).

- [ ] **Step 2: Implement CDK detection over the oxc tree**

Extend the `Collector` to also record:
- class declarations whose `super_class` is an identifier/member ending in `Stack` → stack candidates (name + span).
- `NewExpression` whose callee name matches `[A-Z]…` and whose first arg is `this` and second arg is a string literal → construct candidates (`construct_type`, `logical_id`, span).

Add to `Collector`: `stacks: Vec<(String, u32)>` and `constructs: Vec<(String, String, u32)>`. Add visitor methods:

```rust
fn visit_class(&mut self, it: &Class<'a>) {
    if let Some(id) = &it.id {
        self.symbols.push(Symbol { name: id.name.to_string(), kind: NodeKind::Struct, start: it.span.start });
        if let Some(super_class) = &it.super_class {
            if ends_with_stack(super_class) {
                self.stacks.push((id.name.to_string(), it.span.start));
            }
        }
    }
    walk::walk_class(self, it);
}
fn visit_new_expression(&mut self, it: &oxc_ast::ast::NewExpression<'a>) {
    if let Some(ty) = callee_name(&it.callee) {
        if ty.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
            if let Some(logical_id) = cdk_logical_id(&it.arguments) {
                self.constructs.push((ty, logical_id, it.span.start));
            }
        }
    }
    walk::walk_new_expression(self, it);
}
```

With free helpers `ends_with_stack(expr) -> bool` (true when the expression text/last member ident ends with `Stack`), `callee_name(expr) -> Option<String>` (identifier or `obj.Member` → `Member`), and `cdk_logical_id(args) -> Option<String>` (first arg is `this`/ThisExpression, second is a `StringLiteral` → its value). Implement them by matching on `Expression`/`Argument` variants (mirror `get_identifier_reference` usage from Task 6). Then in `extract`, after symbols/imports, emit a `DeploymentResource` node per stack/construct using the exact field formats listed above (copy from the deleted `extract_aws_cdk_knowledge`, replacing `find_line`/`find_block_end` with `ctx.lines`).

- [ ] **Step 3: Remove the regex CDK path**

Delete `extract_aws_cdk_knowledge`; remove its call from `extract_js_ts_file` (CDK now runs inside `lang::javascript::extract`). Keep `cdk_service` (move it to `javascript.rs` or make `pub(crate)`).

- [ ] **Step 4: Test**

Run: `cargo test extracts_aws_cdk_stacks_and_resources && cargo test`
Expected: the CDK test + full suite PASS.

- [ ] **Step 5: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
git add src/lang/javascript.rs src/extractor.rs
git commit -m "Detect AWS-CDK stacks/constructs from the oxc AST

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

## Task 8: Call edges from AST call sites with file-scoped resolution

**Files:**
- Modify: `src/extractor.rs` (`add_call_edges`, `extract` to thread file-scoped call data)
- Modify: `src/lang/*` (collect callee names per file)

Current `add_call_edges` scans every chunk's text for `"{name}("` against the **global** `symbol_names`, so same-named symbols across files collide (first-inserted wins) and comments/strings produce false edges. Replace with real call sites resolved file-scoped first.

- [ ] **Step 1: Add tests (TDD)**

```rust
#[test]
fn call_edges_resolve_to_local_definition() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.py"), "def helper():\n    return 1\ndef run():\n    return helper()\n").unwrap();
    fs::write(dir.path().join("b.py"), "def helper():\n    return 2\n").unwrap();
    let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
    let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();
    // The call from a.py::run must target a.py::helper, not b.py::helper.
    let a_helper = result.nodes.iter().find(|n|
        n.name == "helper" && n.metadata.get("file").and_then(|v| v.as_str()) == Some("a.py")).unwrap();
    let run = result.nodes.iter().find(|n| n.name == "run").unwrap();
    assert!(result.edges.iter().any(|e|
        e.kind == EdgeKind::Calls && e.source_node_id == run.id && e.target_node_id == a_helper.id));
}

#[test]
fn call_edges_ignore_names_only_in_comments() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("c.py"),
        "def helper():\n    return 1\ndef run():\n    # helper() in a comment\n    return 2\n").unwrap();
    let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
    let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap();
    let run = result.nodes.iter().find(|n| n.name == "run").unwrap();
    let helper = result.nodes.iter().find(|n| n.name == "helper").unwrap();
    assert!(!result.edges.iter().any(|e|
        e.kind == EdgeKind::Calls && e.source_node_id == run.id && e.target_node_id == helper.id));
}
```

- [ ] **Step 2: Run — expect failure**

Run: `cargo test call_edges_resolve_to_local_definition call_edges_ignore_names_only_in_comments`
Expected: both FAIL against the current `contains("name(")` global resolver.

- [ ] **Step 3: Collect real call sites**

Each language collector already (Task 6) or newly (Tasks 4/5) gathers callee names with their enclosing symbol node id. Define in `extractor.rs`:

```rust
/// A resolved-or-pending call: (caller node id, callee name, caller file path).
pub(crate) struct CallSite {
    pub caller: Uuid,
    pub callee: String,
    pub file: String,
}
```

Add `pub calls: Vec<CallSite>` to `FileExtraction`-adjacent collection (thread a `&mut Vec<CallSite>` through `extract`, or store on `ExtractionResult` behind a `#[serde(skip)]` field). For Python (rustpython `ast::Expr::Call`), Solidity (solang `Expression::FunctionCall`), and JS/TS (oxc `CallExpression`, already collected), record `(enclosing_symbol_id, callee_name, file_path)` while walking each symbol's body. For the enclosing symbol, track the "current symbol id" during traversal (push/pop as you enter function/method/contract-member bodies).

- [ ] **Step 4: Resolve file-scoped first**

Rewrite `add_call_edges` to take the collected `CallSite`s plus two maps — a per-file `HashMap<(String /*file*/, String /*name*/), Uuid>` and the existing global `HashMap<String, Uuid>` — and emit one `Calls` edge per call site, preferring the file-local target:

```rust
fn add_call_edges(
    repo_id: Uuid,
    result: &mut ExtractionResult,
    calls: &[CallSite],
    by_file: &HashMap<(String, String), Uuid>,
    global: &HashMap<String, Uuid>,
) {
    for call in calls {
        let target = by_file
            .get(&(call.file.clone(), call.callee.clone()))
            .or_else(|| global.get(&call.callee));
        if let Some(&target) = target {
            if target != call.caller {
                result.edges.push(edge(
                    repo_id, call.caller, target, EdgeKind::Calls,
                    weights::CALLS_HEURISTIC,
                    json!({"detector":"ast_call_site","callee":call.callee}),
                ));
            }
        }
    }
}
```

`repo_id` is already a parameter of `extract`; thread it through. Populate `by_file` when emitting each symbol (`(file.path, name) -> node.id`). Keep the global map as the cross-file fallback. Update the call site in `extract` from `add_call_edges(&mut result, &symbol_names)` to `add_call_edges(repo_id, &mut result, &calls, &by_file, &symbol_names)`.

- [ ] **Step 5: Test**

Run: `cargo test`
Expected: the two new call-edge tests PASS and all prior tests stay green. The `detector` value changed from `"name_call_heuristic"` to `"ast_call_site"`; if any test asserts the old detector string, update that assertion (this is the one allowed test edit, and only the metadata string).

- [ ] **Step 6: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
git add src/extractor.rs src/lang/
git commit -m "Resolve call edges from AST call sites, file-scoped first

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

## Task 9: Raise confidence for parser-backed edges

**Files:**
- Modify: `src/weights.rs`

- [ ] **Step 1: Bump confidence, keep cost**

Edit these constants (change only the second `EdgeWeight::new` argument — the `confidence`; leave `cost` untouched):
- `IMPORTS_MODULE`: `EdgeWeight::new(0.35, 0.90)` → `EdgeWeight::new(0.35, 1.00)`
- `IMPORTS_SOLIDITY`: `EdgeWeight::new(0.30, 0.90)` → `EdgeWeight::new(0.30, 1.00)`
- `CONTAINS_MEMBER`: `EdgeWeight::new(0.10, 0.95)` → `EdgeWeight::new(0.10, 1.00)`
- `IMPLEMENTS`: `EdgeWeight::new(0.20, 0.75)` → `EdgeWeight::new(0.20, 0.95)` (inheritance is now AST-exact; cross-language trait/impl heuristics keep a small discount)

Leave `CALLS_HEURISTIC` cost unchanged; optionally raise its confidence `0.55` → `0.70` (callee is AST-exact; cross-file resolution still heuristic).

- [ ] **Step 2: Update the module doc comment**

In the `//!` header of `weights.rs`, change the sentence framing imports/members/inheritance as "regex- or name-based heuristics" to note they are now produced by real per-language parsers (oxc/rustpython/solang) and only cross-file call resolution remains heuristic.

- [ ] **Step 3: Test**

Run: `cargo test`
Expected: PASS. If any `query`/ranking test asserts a specific score that shifts, update it to the new expected value (confidence feeds ranking, not routing cost).

- [ ] **Step 4: Commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
git add src/weights.rs src/query.rs
git commit -m "Raise extraction confidence for parser-backed edges (cost unchanged)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

## Task 10: Graceful parse-failure handling + logging

**Files:**
- Modify: `src/lang/{python,solidity,javascript}.rs` (already return `Ok(())` on parse failure from Tasks 4–7; standardize the warning)
- Modify: `src/extractor.rs` if a shared warn helper is preferred

- [ ] **Step 1: Add the parse-failure test**

```rust
#[test]
fn invalid_source_file_degrades_to_file_node_without_aborting() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("ok.py"), "def fine():\n    return 1\n").unwrap();
    fs::write(dir.path().join("broken.py"), "def (:\n    this is not python\n").unwrap();
    let extractor = RustRepositoryExtractor::new(IndexingConfig::default());
    let result = extractor.extract(dir.path(), Uuid::new_v4(), None).unwrap(); // must NOT error
    // The good file's symbol is present.
    assert!(result.nodes.iter().any(|n| n.name == "fine" && n.kind == NodeKind::Function));
    // The broken file still produced a File node (degrade, not drop).
    assert!(result.nodes.iter().any(|n| n.kind == NodeKind::File && n.name == "broken.py"));
    // No symbols were fabricated from the broken file.
    assert!(!result.nodes.iter().any(|n|
        n.metadata.get("file").and_then(|v| v.as_str()) == Some("broken.py")
            && n.kind == NodeKind::Function));
}
```

- [ ] **Step 2: Run it**

Run: `cargo test invalid_source_file_degrades_to_file_node_without_aborting`
Expected: PASS (Tasks 4–7 already emit the file node before parsing and return `Ok(())` on parse error). If it FAILS because a parser panics instead of returning `Err`, wrap the parse call in `std::panic::catch_unwind` or guard the specific input; prefer the parser's `Result` where available (rustpython/solang return `Result`; oxc returns `errors` in `ParserReturn`).

- [ ] **Step 3: Standardize the warning**

Ensure all three modules emit a single consistent warning line on degrade (file path + parser + diagnostic count). If the project has a logger, use it; otherwise keep `eprintln!` with the `[chaos-substrate]` prefix. Remove the temporary `tracing_warn` shim from Task 4 in favor of the standard form.

- [ ] **Step 4: Test and commit**

```bash
cargo test
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
git add src/lang/ src/extractor.rs
git commit -m "Degrade gracefully on per-file parse failure with a warning

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

## Task 11: Remove the `regex` dependency and final validation

**Files:**
- Modify: `src/extractor.rs` (remove `use regex::Regex;`, the `fn regex(` helper)
- Modify: `Cargo.toml` (remove `regex = "1"`)

- [ ] **Step 1: Confirm no remaining regex uses**

Run: `rg -n "Regex|regex::" src/`
Expected: no matches (all moved to AST in Tasks 4–7). If any remain, they belong to a not-yet-migrated path — resolve before removing the dep.

- [ ] **Step 2: Remove the helper, import, and dependency**

Delete `use regex::Regex;`, `fn regex(pattern: &str) -> Regex { … }`, and any leftover `LazyLock` regex statics from `src/extractor.rs`. Remove the `regex = "1"` line from `Cargo.toml`.

- [ ] **Step 3: Full validation gate**

Run, in order:
```bash
cargo build
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```
Expected: build clean, fmt clean, all tests PASS (original 27 + the new fidelity/call/degrade tests), clippy clean with no warnings.

- [ ] **Step 4: Smoke-test a real analyze (no DB writes needed for parse)**

Run: `cargo run -- analyze .` against a small mixed-language fixture, or rely on the test suite if no embedder/DB is configured. Confirm no panics and that JS/Python/Solidity symbols appear. (Per CLAUDE.md, real indexing needs an embedder; the parse path is exercised by tests regardless.)

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/extractor.rs
git commit -m "Drop the regex dependency now that all extraction is AST-based

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final self-check before opening a PR

- [ ] All four languages: Rust (`syn`, unchanged), Python (`rustpython-parser`), Solidity (`solang-parser`), JS/TS + CDK (`oxc`) extract via real parsers.
- [ ] `rg -i "tree.?sitter" .` still returns nothing (we never claimed Tree-sitter; docs stay accurate).
- [ ] No `regex` dependency; no `find_line`/`find_block_end`/`find_python_block_end` used by the parsed languages (delete them if fully unused; keep any the Rust path still needs).
- [ ] `cargo fmt --check`, `cargo test`, `cargo clippy --all-targets --all-features -- -D warnings` all clean.
- [ ] Hard Rules honored: pure-Rust extraction, no mock embedders, Postgres/pgvector untouched, MCP stdio untouched.
