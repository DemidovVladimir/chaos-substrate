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
- existing known feature pages in `/absolute/path/to/repo/docs/features_memory`

For the encrypted data-room feature, the known pages are:

- `e2e-encryption-feature-map.html`
- `e2e-encryption-infra-map.html`

The feature pages are generated as dark, neon, Blade Runner-inspired static websites. They are
standalone HTML files and can be opened directly in a browser.

Each generated feature page also embeds a machine-readable manifest:

```html
<script type="application/json" id="chaos-feature-manifest">...</script>
```

Agents should use this manifest, not the visual DOM, when they need feature-page context. The
manifest schema is generic and contains feature metadata, claims, modes, nodes, edges, evidence, and
confidence fields. E2E encryption is the first built-in feature map, not a special-case schema.

## 3. Create Missing Built-In Pages

By default, `refresh` updates known feature pages only when they already exist. This keeps the command
from creating unrelated feature pages in every indexed repository.

To create every built-in feature page template:

```bash
cargo run -- refresh /absolute/path/to/repo --all-features
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

Before implementing a subfeature, generate focused agent context:

```bash
cargo run -- feature-context /absolute/path/to/repo "implement store nft icon"
```

This merges live Postgres search results with relevant generated feature-page manifests.

## Notes

- `refresh` depends on the repository already being indexed.
- Obsidian output is always regenerated.
- Feature websites are regenerated from built-in templates plus current source snippets.
- The default feature-memory directory is `docs/features_memory`, separated from normal prose docs
  so agents can scan it without reading unrelated documentation.
- The current built-in feature website set covers the encrypted data-room upload/read flow and its
  AWS/CDK/AppSync/React infrastructure correlation. Future maps such as Authorization/RBAC should use
  the same manifest schema.
