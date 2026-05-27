# Refresh Export Tutorial

Use `chaos refresh` after re-indexing a repository when you want the generated human-facing views to
match the current Postgres index.

The command reads persisted graph data from Postgres. It does not run extraction, does not call the
embedding provider, and does not replace the canonical database state. It only regenerates derived
files.

## 1. Re-Index The Repository

```bash
cargo run -- analyze /absolute/path/to/repo
```

## 2. Refresh Generated Views

```bash
cargo run -- refresh /absolute/path/to/repo
```

Default outputs:

- `/absolute/path/to/repo/chaos-obsidian-vault`

Feature context pages generated with `feature-context --output-html` use dark, neon static websites.
They are standalone HTML files and can be opened directly in a browser.

Each generated feature page also embeds a machine-readable manifest:

```html
<script type="application/json" id="chaos-feature-manifest">...</script>
```

Agents should use this manifest, not the visual DOM, when they need feature-page context. The
manifest schema is generic and contains feature metadata, claims, modes, nodes, edges, evidence, and
confidence fields.

Story steps in feature maps should carry explicit `node_ids` and optional `edge_ids`. Renderers
should highlight exactly that curated step scope. They should not expand a story step through the
graph automatically, because that turns a local flow step into an unreadable neighborhood. Use
`modes` for intentionally broader graph views.

## 3. Generate Feature Pages

Generate a focused feature explanation page from the current index:

```bash
cargo run -- feature-context /absolute/path/to/repo "authorization and RBAC" \
  --output-html /absolute/path/to/repo/docs/features_memory/authorization-rbac-explanation.html
```

Before implementing a subfeature, generate focused agent context:

```bash
cargo run -- feature-context /absolute/path/to/repo "implement secure upload icon"
```

## 4. Use Custom Output Paths

```bash
cargo run -- refresh /absolute/path/to/repo \
  --obsidian-output /absolute/path/to/repo/docs/chaos-vault \
  --features-dir /absolute/path/to/repo/docs/features_memory
```

## 5. Suggested Workflow

```bash
cargo run -- analyze /absolute/path/to/repo
cargo run -- refresh /absolute/path/to/repo
```

Open the regenerated Obsidian vault for broad graph exploration. Open the generated feature website
when you need a guided, code-linked view of one feature flow.

This merges live Postgres search results with relevant generated feature-page manifests.

## Notes

- `refresh` depends on the repository already being indexed.
- Obsidian output is always regenerated.
- Feature websites are generated from focused queries and current source snippets.
- The default feature-memory directory is `docs/features_memory`, separated from normal prose docs
  so agents can scan it without reading unrelated documentation.
- Feature maps such as Authorization/RBAC, upload flows, infrastructure correlation, and failure
  modes should use the same manifest schema.
