# Feature Context Tutorial

`chaos feature-context` prepares focused implementation context for agents. Use it before changing a
subfeature that may touch an existing generated feature map.

The command combines:

- live Postgres retrieval from the indexed repository
- graph context paths around matched chunks
- generated feature-page manifests from `docs/features_memory`

It does not read the whole `docs/` tree. By default it scans only direct `*.html` files in
`docs/features_memory` and ignores pages without:

```html
<script type="application/json" id="chaos-feature-manifest">...</script>
```

Pages land in `docs/features_memory` from two producers:

- `chaos add` / `chaos_add` writes a deterministic feature/bug page (no LLM round-trip) every time it
  incrementally indexes a change. It builds the manifest directly from the changed symbols and their
  graph neighbors, satisfying the same interactive contract as the LLM write path.
- `chaos_write_feature_website` writes an LLM-composed page from `chaos_feature_context` evidence.

Both embed the same `chaos-feature-manifest` block, so `feature-context` can match either kind.

## Sibling: `chaos impact`

`chaos_impact` / `chaos impact <repo> <feature>` is the sibling tool. Where `feature-context` returns
evidence as JSON and only writes HTML when `--output-html` is passed, `chaos impact` ALWAYS writes an
interactive impact HTML (an impact summary plus the evidence dashboard) to
`docs/features_memory/<slug>-impact.html`, framed as feature-vs-existing-code — how this feature maps
onto the codebase as it is TODAY (the before). It returns a compact summary (counts plus the existing
files/symbols the feature touches, warnings, and the HTML path), keeping the full evidence in the HTML
only. Reach for it when you want the guaranteed HTML artifact rather than a feature_context JSON dump.

## Usage

```bash
cargo run -- feature-context /absolute/path/to/repo "implement secure upload icon"
```

With explicit paths and limits:

```bash
cargo run -- feature-context /absolute/path/to/repo "implement secure upload icon" \
  --features-dir /absolute/path/to/repo/docs/features_memory \
  --limit 10 \
  --feature-limit 3 \
  --nodes-per-feature 8
```

To write a standalone explanation website (light editorial theme):

```bash
cargo run -- feature-context /absolute/path/to/repo "authorization and RBAC" \
  --output-html /absolute/path/to/repo/docs/features_memory/authorization-rbac-explanation.html
```

The generated website separates code hits from supplemental documentation evidence. Markdown/MDX and
text PDF docs remain lower priority than source code, but matching docs are kept visible when they
appear in the retrieval candidates.

## Output

The JSON response contains:

- `postgres.hits`: semantic and keyword retrieval results
- `postgres.context_paths`: graph paths between matched nodes
- `postgres.hits[].metadata.source_priority`: `supplemental` for Markdown/MDX and PDF documentation hits
- `feature_matches`: relevant generated feature pages
- `feature_matches[].feature`: generic feature metadata such as id, title, domain, and summary
- `feature_matches[].claims`: important claims with confidence and related nodes
- `feature_matches[].modes`: named graph views such as architecture, upload flow, or security boundary
- `feature_matches[].matched_nodes`: source-linked feature nodes with code snippets
- `feature_matches[].related_edges`: relationships around those nodes

## Manifest Schema

Generated feature pages use a generic schema. The same contract should support Authorization/RBAC,
billing, search, uploads, infrastructure, or other feature areas.

Shape:

```json
{
  "schema_version": "1",
  "feature": {
    "id": "authorization-rbac",
    "title": "Authorization and RBAC",
    "domain": "security",
    "summary": "How identity, roles, permissions, and enforcement points connect."
  },
  "title": "Authorization and RBAC Map",
  "subtitle": "Focused feature map regenerated from indexed knowledge.",
  "claims": [
    {
      "id": "backend-enforces-permission",
      "title": "Backend enforces the permission decision",
      "body": "Client-side gating is not the security boundary.",
      "confidence": 0.9,
      "node_ids": ["auth-middleware", "permission-check"]
    }
  ],
  "modes": [
    {
      "id": "request-enforcement-flow",
      "title": "Request enforcement flow",
      "node_ids": ["auth-middleware", "role-lookup", "permission-check"]
    }
  ],
  "nodes": [],
  "edges": [],
  "story": [
    {
      "id": "request-enters-api",
      "title": "Request enters the API boundary",
      "body": "The request reaches the backend enforcement point.",
      "node_ids": ["auth-middleware", "permission-check"],
      "edge_ids": ["auth-middleware->permission-check"]
    }
  ]
}
```

Every node and edge should include:

- `evidence`: source, extraction or curation method, and notes
- `confidence`: a numeric confidence score from `0.0` to `1.0`

Every story step should include explicit `node_ids` and, when useful, `edge_ids`. Do not infer a
story-step highlight by expanding the graph from the first node. Step highlighting should show the
curated subflow for that step only; broader views belong in `modes`.

For manually curated built-in maps, evidence currently records `manual-feature-map`. Future
query-generated maps should record extractor/query provenance more precisely.

## Recommended Agent Workflow

1. Use MCP `chaos_feature_context` with the user task when MCP is available. Use
   `chaos feature-context` only for direct CLI debugging. When you want a guaranteed interactive
   impact HTML instead of feature_context JSON, use `chaos_impact` (or `chaos impact <repo>
   <feature>`), which always writes the impact page and returns a compact summary.
2. Read the highest scoring `postgres.hits` and `feature_matches`.
3. Use feature manifests as the stable machine contract; do not scrape the visual DOM.
4. Open source files from the returned paths and line ranges before editing.
5. After implementation, run project tests, then re-index. Use `chaos add` (or MCP `chaos_add`) to
   incrementally index just the changed files, refresh the Obsidian vault, and write a deterministic
   feature/bug page into `docs/features_memory` in one shot; run a full `chaos analyze` (or
   `chaos refresh`) when you need a complete graph rebuild. For an LLM-composed feature page, compose
   the page and manifest from `chaos_feature_context` evidence, then write it with
   `chaos_write_feature_website`.

## Why Manifests Instead Of DOM Scraping

The visual website is optimized for humans. The manifest is optimized for agents:

- stable JSON schema
- no dependence on CSS, layout, or text rendering
- exact source file and line-range anchors
- claim cards, graph modes, evidence, and confidence fields
- explicit nodes and edges
- compact scan scope in `docs/features_memory`

This keeps generated websites useful for humans while keeping agent context retrieval predictable.
