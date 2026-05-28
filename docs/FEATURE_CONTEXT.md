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

To write a dark standalone explanation website:

```bash
cargo run -- feature-context /absolute/path/to/repo "authorization and RBAC" \
  --output-html /absolute/path/to/repo/docs/features_memory/authorization-rbac-explanation.html
```

The generated website separates code hits from supplemental documentation evidence. Markdown/MDX
docs remain lower priority than source code, but matching docs are kept visible when they appear in
the retrieval candidates.

## Output

The JSON response contains:

- `postgres.hits`: semantic and keyword retrieval results
- `postgres.context_paths`: graph paths between matched nodes
- `postgres.hits[].metadata.source_priority`: `supplemental` for Markdown/MDX documentation hits
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

1. Run `chaos feature-context` with the user task.
2. Read the highest scoring `postgres.hits` and `feature_matches`.
3. Use feature manifests as the stable machine contract; do not scrape the visual DOM.
4. Open source files from the returned paths and line ranges before editing.
5. After implementation, run project tests, then re-run `chaos analyze` and `chaos refresh` to
   update the index and Obsidian vault. Regenerate focused feature pages with
   `chaos feature-context --output-html` when the feature explanation should change.

## Why Manifests Instead Of DOM Scraping

The visual website is optimized for humans. The manifest is optimized for agents:

- stable JSON schema
- no dependence on CSS, layout, or text rendering
- exact source file and line-range anchors
- claim cards, graph modes, evidence, and confidence fields
- explicit nodes and edges
- compact scan scope in `docs/features_memory`

This keeps generated websites useful for humans while keeping agent context retrieval predictable.
