# Real parsers for JS/TS, Python, and Solidity

**Date:** 2026-05-29
**Status:** Approved design, pending implementation plan
**Scope:** Replace the regex-based symbol/import extraction for JavaScript, TypeScript,
Python, and Solidity with real pure-Rust AST parsers. Rust already uses `syn`; it is
untouched. Markdown, PDF, and JSON-config extraction are untouched.

## Background and motivation

`src/extractor.rs` extracts a source-grounded knowledge graph. Today only **Rust** is
parsed with a real AST (`syn::parse_file`, confidence ~1.0). JavaScript, TypeScript,
Python, Solidity, and AWS-CDK detection are **regex-based** (`regex::Regex`, confidence
0.55–0.95). The regex approach misses class methods, arrow/const functions assigned in
non-trivial ways, multi-line signatures, and computes line ranges via fragile text
searches (`find_line` / `find_block_end`), which mis-locate overloaded or duplicate
names.

There is no Tree-sitter anywhere in the repository; the project's "Hard Rules" require
runtime extraction to stay Rust-side. The chosen parsers are all pure-Rust crates, so no
C toolchain or external service is introduced, honoring those rules.

**Verified prerequisites (2026-05-29):**

- `cargo check` on `main` + working tree passes (exit 0, `chaos-substrate v0.5.0`).
- `src/weights.rs` already centralizes every `EdgeWeight`; `extractor.rs` consumes it via
  `weights::*`. The "scattered weight literals" cleanup is already complete.
- Candidate crates compile together on the local toolchain (Rust 1.91.1, edition 2021) in
  25.7s, exit 0: `oxc_parser`/`oxc_allocator`/`oxc_ast`/`oxc_span` 0.116,
  `rustpython-parser` 0.4.0, `solang-parser` 0.3.5.

## Guiding principle: the output contract is frozen

`storage`, `query`, `graph_export`, `obsidian_export`, and the 11 extractor unit tests all
consume `ExtractionResult { nodes, edges, chunks, files }`. The parser swap must keep:

- the same `NodeKind` mapping per construct,
- the same `stable_id` string formats,
- the same chunk text layout (`"Language: …\nFile: …\nSymbol: …\n…\n\n{code}"`),
- the same `EdgeKind`s, and
- the same `symbol_names` population semantics (used by call-edge resolution).

Parsers change **how** symbols are discovered and **improve** line-range fidelity. They do
**not** change the shape of what is emitted. All 11 existing tests must stay green; new
tests cover constructs regex could not (class methods, arrow/const functions, multi-line
signatures, nested classes).

## Decisions (ratified with the user on 2026-05-29)

1. **Parser strategy:** pure-Rust per language — `oxc` (JS/TS), `rustpython-parser`
   (Python), `solang-parser` (Solidity). `syn` stays for Rust. No C deps.
2. **Sequencing:** cleanups land first as their own commit, then the parser swap.
3. **Parse-failure policy:** graceful per-file degradation. On a parse error, emit the
   file node + a file-level chunk, log a warning, and skip that file's symbols. Never
   abort the whole `analyze`; never emit fabricated symbols.
4. **Confidence weights:** raise `confidence` toward 1.0 for now-parser-certain edges
   (imports, Solidity members, inheritance); keep `cost` unchanged so routing is identical
   and only ranking trust improves. Update `weights.rs` doc comments accordingly.
5. **Call edges (backlog item 3):** included in the parser phase. Replace the
   `content.contains("name(")` global heuristic with real AST call sites resolved
   file-scoped first, then a global fallback.
6. **AWS-CDK detection:** convert to the oxc AST (reuse the JS/TS parse) — detect
   `class … extends …Stack` and `new …Stack(this, "id", …)` from AST nodes. Removes the
   last JS-side regexes.

## Phase 1 — Cleanups (one commit, no new dependencies)

### C1. DRY the per-language file prelude

Every code/doc extractor repeats ~15 lines: read file → build `SourceFile` → push file →
build `file_node` → push `Contains` edge → push `file_node`. Extract a helper:

```rust
fn begin_file(
    root: &Path, path: &Path, repo_id: Uuid, commit_sha: Option<String>,
    repo_node_id: Uuid, language: Language,
    contains: EdgeWeight, contains_meta: serde_json::Value,
    result: &mut ExtractionResult,
) -> Result<(SourceFile, Uuid /* file_node_id */)>
```

It returns the `SourceFile` and `file_node` id; each caller then runs its own
symbol/chunk logic. The `Contains` weight differs per caller (`CONTAINS_CODE`,
`CONTAINS_DOC`, `CONTAINS_PDF`), so it is a parameter. Expected reduction: ~80 lines.

### C2. Documentation fixes

- `docs/CLAUDE_VALIDATION_BRIEF.md` — "Implemented language support" lists only
  Rust/TS/JS, while Solidity + Python appear lower in the same file. Add Solidity and
  Python to the list (factual fix).
- De-duplicate near-verbatim content: setup steps repeated across `PLUGIN_INSTALL.md`,
  `PLUGIN_WORKFLOW.md`, `CLAUDE_CODE_COWORK.md`, `CLAUDE_MCP_INSTALL.md`, and `README.md`;
  the manifest schema in 3 places; the hard rules in 4. Choose one canonical home for
  each and cross-reference from the others. No new claims; only consolidation.

### Superseded backlog item

The original "hoist remaining inline regexes to `LazyLock`" item is **superseded**: Phase 2
deletes the Python/Solidity/JS-TS import & symbol regexes outright, and the AWS-CDK regexes
become AST queries. There are no surviving inline regexes worth hoisting.

## Phase 2 — Parser swap (one commit, adds dependencies)

### New module layout

`extractor.rs` is 2628 lines. Move language extraction into a focused module tree:

```
src/extractor.rs        orchestration: walk + dispatch, shared helpers,
                        Rust(syn), markdown/pdf/json/cargo, call edges
src/lang/mod.rs         FileExtraction context + LineIndex + pub(crate) glue
src/lang/javascript.rs  oxc            -> JS/TS symbols, imports, AWS-CDK
src/lang/python.rs      rustpython     -> functions, classes, imports
src/lang/solidity.rs    solang-parser  -> contracts/interfaces/libraries,
                                          members, imports, inheritance
```

Shared helpers (`file_node`, `edge`, `chunk_for_node`, hashing) become `pub(crate)`. A
`FileExtraction<'a>` context struct bundles `repo_id`, `&SourceFile`, `file_node_id`,
`&str content`, `&mut symbol_names`, and `&mut ExtractionResult` so each language module
exposes a single `extract(ctx) -> Result<()>`.

### LineIndex

A small helper that precomputes line-start byte offsets once per file and converts an AST
byte offset → 1-based line number. This replaces `find_line` / `find_block_end` /
`find_python_block_end` for the parsed languages: AST spans give exact `line_start` /
`line_end`, including correct ranges for overloaded or duplicate names. The Rust (`syn`)
path keeps its current line logic — out of scope.

### AST → existing node shapes

The construct→`NodeKind`/`stable_id`/edge mapping must match the current regex output
exactly. Summary (full mapping to be enumerated in the implementation plan):

- **oxc (JS/TS):** `FunctionDeclaration`, arrow/const function initializers, and
  **class methods** → `Function`; `Class` → `Struct`; `TSInterfaceDeclaration` → `Trait`;
  `TSEnumDeclaration` → `Enum`; `TSTypeAliasDeclaration` → `TypeAlias`;
  `ImportDeclaration` / `require(...)` / `export … from` → `Dependency`
  (`stable_id` via existing `import_stable_id`, bare vs relative preserved). Test-file /
  test-symbol detection (`is_js_ts_test_file`, `is_test_symbol`) preserved.
- **rustpython (Python):** `FunctionDef` / `AsyncFunctionDef` → `Function`; `ClassDef` →
  `Struct`; `Import` / `ImportFrom` (relative dots preserved) → `Dependency`. Test
  detection preserved.
- **solang (Solidity):** `ContractDefinition` (contract→`Struct`, interface→`Trait`,
  library→`Module`); functions / constructor / fallback / receive → member `Function`s;
  `event` / `modifier` → `Concept`; `ImportDirective` → `Dependency`; base contracts →
  `Implements` edges (replacing `add_solidity_inheritance_edges`' string parsing).

### AWS-CDK via AST

In `javascript.rs`, after parsing, walk the oxc tree for `class X extends …Stack` →
`DeploymentResource` (resource_kind "stack", `Defines` edge / `DEFINES_SYMBOL`) and
`new …Construct(this, "id", …)` → `DeploymentResource` (resource_kind "construct",
`Configures` edge / `CONFIGURES`, with `construct_type`/`logical_id`/`service` metadata).
Output must match the current `extract_aws_cdk_knowledge` exactly — the
`extracts_aws_cdk_stacks_and_resources` test pins this contract.

### Call edges (item 3)

Replace `add_call_edges`' `content.contains("name(")` over a global symbol map with real
AST call sites:

- Each language module collects call-expression callee names with their enclosing symbol.
- Resolution prefers a **file-scoped** symbol map (name → node id within the same file),
  falling back to a global map only when no local match exists. This eliminates
  cross-file same-name collisions and comment/string false hits.
- Edge weight stays `CALLS_HEURISTIC`'s `cost`; `confidence` may rise since callee
  extraction is now AST-exact while cross-file resolution remains heuristic — final value
  set in the implementation plan, but cost unchanged.

### Confidence weight updates

In `weights.rs`, raise `confidence` (not `cost`) for edges now produced by real parsers:
`IMPORTS_MODULE`, `IMPORTS_SOLIDITY`, `CONTAINS_MEMBER`, and Solidity `IMPLEMENTS`
inheritance. Update the module doc comment that currently frames these as regex
heuristics. Exact target values enumerated in the plan; `cost` values are frozen.

## Error handling

- **Parse failure (per file):** log a warning (file path + parser diagnostic), emit the
  file node and a single file-level chunk so the file is still searchable, skip symbol
  extraction for that file. No abort, no fabricated symbols.
- **Read failure:** unchanged — propagate with the existing `with_context` message.
- **Walk/dispatch:** unchanged.

## Testing strategy

- **Regression:** all 11 existing extractor tests stay green unmodified — they are the
  output contract. The repo-wide suite (`cargo test`, 27 tests) must pass.
- **New fidelity tests** (things regex missed, now expected to be captured):
  - JS/TS: class methods; arrow/const functions; multi-line function signatures; nested
    classes; `export … from` re-exports.
  - Python: methods inside classes; `async def`; `from . import x` and `from .pkg import y`.
  - Solidity: functions inside a contract resolved to the right contract; multiple bases
    in `is A, B`.
  - Call edges: same-named functions in two files resolve to the local definition, not the
    first-seen global one; a name appearing only in a comment/string produces no edge.
  - Parse failure: a syntactically invalid file yields a file node + file chunk and a
    warning, and does not abort the run or emit symbols.
- **Validation gate (from CLAUDE.md):** `cargo fmt --check`, `cargo test`,
  `cargo clippy --all-targets --all-features -- -D warnings`.

## Out of scope

- Changing the Rust (`syn`) extractor.
- Markdown/PDF/JSON-config extraction behavior.
- Replacing Postgres/pgvector or the embedding pipeline.
- Type inference, cross-crate resolution, control-flow graphs, or macro expansion.
- Any change to `cost` (routing) weights.

## Risks and mitigations

- **oxc arena/lifetime API complexity.** Mitigation: keep all oxc AST access inside
  `javascript.rs`, collect plain owned data (names, spans, kinds) before touching
  `ExtractionResult`; the allocator/parser live in a tight local scope.
- **Output-contract drift.** Mitigation: the 11 contract tests run unmodified; any drift
  fails CI.
- **Solidity grammar coverage (solang 0.3).** Mitigation: graceful per-file degrade
  ensures an unsupported construct downgrades that file rather than breaking the run.
- **Confidence bump altering query rankings.** Mitigation: `cost` frozen (routing
  identical); ranking-sensitive query tests reviewed when values are finalized.

## Correction (2026-05-29, Task 11)

> **Superseded by the shipped implementation.** The `regex` crate was ultimately
> **removed** — it is no longer a dependency in `Cargo.toml` and there are zero `regex::`
> uses in `src/`. `find_item_line` was rewritten without regex: it now uses a hand-rolled
> word-boundary matcher (`line_has_keyword_name` in `src/extractor.rs`, "the equivalent of
> the regex `\b{keyword}\s+{name}\b` but without compiling a regex per call"). The historical
> note below is retained for context but no longer reflects the codebase.

The `regex` crate dependency is **retained**: the Rust (`syn`) extraction path's
`find_item_line` function uses `regex::Regex::new` + `regex::escape` to locate symbol lines
within source text (syn spans lack absolute line numbers in non-proc-macro builds). Removing
this function was out of scope from the start, and the `regex` crate therefore stays in
`Cargo.toml`. All non-Rust language extraction (JS/TS via oxc, Python via
rustpython-parser, Solidity via solang-parser) is regex-free; the old per-language regex
helpers and `JS_TS_SYMBOL_PATTERNS` were deleted as planned. The earlier plan note suggesting
the `regex` dependency could be fully removed was incorrect.
