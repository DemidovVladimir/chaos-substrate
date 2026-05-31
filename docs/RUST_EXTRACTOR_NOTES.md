# Rust Extractor MVP Notes

> **Status: aspirational design notes, not all implemented.** This document records the
> intended capture scope and graph mapping for the Rust extractor. Several items below
> (byte offsets, visibility/generics/where-clause signatures, and the
> `method_of` / `uses_macro` / `returns` / `accepts` / `references_type` edges) describe a
> richer model than the code currently emits. For what the extractor actually produces today,
> see [Actually Emitted Today](#actually-emitted-today) at the bottom of this file. The Rust
> extractor uses the `syn` AST parser.

## MVP Capture Scope

- Crate identity: package name, crate root, edition when available, and source file paths.
- Module structure: `mod` declarations, inline modules, file-backed modules, and parent-child module paths.
- Public API items: structs, enums, unions, traits, type aliases, constants, statics, functions, impl blocks, trait impls, and macro definitions that are visible or referenced from visible items.
- Item signatures: names, visibility, attributes, generics, lifetimes, where clauses, inputs, outputs, receivers, async/unsafe/const/extern qualifiers, and trait bounds.
- Structural fields and variants: struct fields, enum variants, discriminants, tuple fields, and field visibility.
- Relations needed for navigation: contains, defines, imports, re-exports, calls when cheaply available, implements, has method, has field, type references, and macro expansion sites as unresolved references.
- Documentation surface: doc comments and relevant attributes attached to modules and public API items.
- Source spans: byte offsets plus line/column ranges for every emitted node and edge endpoint.

Defer full type inference, borrow semantics, control-flow graphs, MIR-level facts, macro expansion fidelity, and dependency resolution beyond local crate metadata unless an existing parser exposes them cheaply.

## Chunk to Graph Mapping

- Treat each parsed source file as a document chunk with stable file identity and content hash.
- Map module chunks to `Module` nodes. File-backed modules should link to their source file chunk; inline modules should use the enclosing file chunk plus their span.
- Map each top-level or nested item chunk to one graph node keyed by canonical module path plus item name and disambiguator when needed.
- Map impl chunks to `Impl` nodes keyed by target type, optional trait path, module path, and span. Methods inside impls become function nodes linked with `contains` and `method_of`.
- Map function and method bodies as optional body chunks under the signature node. MVP indexing can store the body text/span without creating statement-level nodes.
- Map imports and re-exports to edge records from the owning module node to resolved targets when known, otherwise to unresolved reference nodes carrying the written path.
- Preserve chunk hierarchy with `contains` edges: crate -> module -> item -> member/body.
- Preserve semantic cross-links with typed edges: `implements`, `returns`, `accepts`, `references_type`, `calls`, `uses_macro`, and `documents`.
- Use source spans as edge provenance so graph queries can jump back to the exact chunk that produced a relationship.

Node IDs should be deterministic across runs when paths and item signatures are unchanged. If parsing cannot resolve a name, emit an unresolved reference node rather than dropping the relationship.

## Actually Emitted Today

The notes above are aspirational. What the extractor (`src/extractor.rs`, parsing with `syn`)
emits today is narrower:

- **Nodes** carry only `line_start` and `line_end` (see `KnowledgeNode` in `src/models.rs`).
  There are **no byte offsets** and **no visibility / generics / where-clause / signature**
  fields on the node itself; any extra detail lives in the free-form `metadata` JSON.
- **Node kinds** (`NodeKind`): `Repository`, `File`, `Module`, `Function`, `Struct`, `Enum`,
  `Trait`, `Impl`, `Method`, `Test`, `Dependency`, `Concept`, `Script`, `TypeAlias`,
  `DeploymentResource`. There is no dedicated union / constant / static / field / variant node.
- **Edge kinds** (`EdgeKind`): `Contains`, `Imports`, `Calls`, `UsesType`, `Implements`,
  `Defines`, `Tests`, `Documents`, `Mentions`, `DependsOn`, `Configures`, `Deploys`,
  `SimilarTo`, `PrerequisiteFor`. The aspirational `method_of`, `uses_macro`, `returns`,
  `accepts`, and `references_type` edges are **not** emitted (method-on-type relationships are
  represented with `Contains`). No type-usage edges are produced today either: `UsesType` is
  defined in the `EdgeKind` enum but the extractor never emits it.
- Call edges are heuristic and file-scoped; cross-file call resolution is name-based.
