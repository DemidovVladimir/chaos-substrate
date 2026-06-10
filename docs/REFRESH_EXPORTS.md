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

To sanity-check what the index now contains, run the read-only, embedder-free `chaos stats`
command, which reports totals and per-kind breakdowns from Postgres:

```bash
cargo run -- stats /absolute/path/to/repo
```

## 2. Refresh Generated Views

```bash
cargo run -- refresh /absolute/path/to/repo
```

Default outputs:

- `/absolute/path/to/repo/chaos-obsidian-vault`

Feature context pages generated with `feature-context --output-html` use the shared light editorial theme.
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

For plugin/MCP workflows, generate feature pages by first calling `chaos_feature_context`, then
having the LLM compose the page and manifest, then calling `chaos_write_feature_website`.

The MCP server exposes fifteen tools: `chaos_analyze`, `chaos_add`, `chaos_stats`, `chaos_query`,
`chaos_feature_context`, `chaos_impact`, `chaos_write_feature_website`, `chaos_obsidian`,
`chaos_refresh`, `chaos_write_storyboard`, `chaos_change_plan`, `chaos_components`, `chaos_features`, `chaos_project`, and `chaos_help`.

Both `chaos refresh` and `chaos obsidian` are now also available as MCP tools, so an agent can
regenerate the Obsidian vault (`chaos_obsidian`) or refresh the vault and feature pages
(`chaos_refresh`) directly over MCP, not only from the CLI.

`chaos_refresh` and `chaos_obsidian` also regenerate the god-node community notes
(`vault/Communities/*.md` plus `vault/Feature Map.md`) and an interactive
`docs/features_memory/feature-map.html` from the persisted hierarchy layers — with no re-index and
no embedder.

For direct CLI debugging, generate a focused feature explanation page from the current index:

```bash
cargo run -- feature-context /absolute/path/to/repo "authorization and RBAC" \
  --output-html /absolute/path/to/repo/docs/features_memory/authorization-rbac-explanation.html
```

Before implementing a subfeature, generate focused agent context:

```bash
cargo run -- feature-context /absolute/path/to/repo "implement secure upload icon"
```

To see how a planned feature maps onto the codebase as it is today (the before), use `chaos impact`.
It always writes an interactive HTML impact report into `docs/features_memory/<slug>-impact.html`:

```bash
cargo run -- impact /absolute/path/to/repo "implement secure upload icon"
```

## 4. Use Custom Output Paths

```bash
cargo run -- refresh /absolute/path/to/repo \
  --obsidian-output /absolute/path/to/repo/docs/chaos-vault \
  --features-dir /absolute/path/to/repo/docs/features_memory
```

`refresh` flags:

- `--obsidian-output <path>` overrides the regenerated Obsidian vault location.
- `--features-dir <path>` overrides the directory scanned for generated feature pages.
- `--all-features` also regenerates every feature website already present in the features directory,
  not just the Obsidian vault. Use this after re-indexing when existing feature pages should be
  refreshed in bulk from the current Postgres index.

```bash
cargo run -- refresh /absolute/path/to/repo --all-features
```

## 5. Incremental Add (One-Shot)

`chaos add` is the incremental counterpart to the analyze-then-refresh cycle. In one command it:

1. detects changed files from git — the working tree (staged, unstaged, and untracked) by default,
   `--since <ref>` for a committed range, or `--path <file>` for explicit files including
   Markdown/Notion exports and PDFs;
2. incrementally indexes only those files into Postgres/pgvector, re-embedding just the changed
   chunks instead of the whole repository;
3. refreshes the Obsidian vault (the same regeneration `refresh` performs);
4. writes an interactive feature/bug HTML page into `docs/features_memory`.

```bash
cargo run -- add /absolute/path/to/repo
```

Feature vs bug is auto-detected from the branch name and the latest commit subject; override with
`--kind feature|bug` and annotate with `-m/--message <text>`. Other flags:

- `--path <file>` indexes specific files (repeatable); may be combined with detected changes.
- `--since <ref>` indexes the files changed in a committed range instead of the working tree.
- `--obsidian-output <dir>` overrides the regenerated Obsidian vault location.
- `--no-obsidian` skips the vault refresh.
- `--no-page` skips writing the feature/bug page.

Like `analyze`, `add` requires a real embedder. Generated artifact directories (the vault,
`docs/features_memory`, and `indexing.skip_dirs`) are excluded so it never re-indexes its own output.
Cross-file call edges into unchanged files are not rebuilt incrementally; run a full `chaos analyze`
(or `chaos refresh`) when you need a complete graph rebuild.

The MCP `chaos_add` tool exposes the same one-shot incremental flow.

## 6. Suggested Workflow

```bash
cargo run -- analyze /absolute/path/to/repo
cargo run -- refresh /absolute/path/to/repo
```

For an iterative loop after editing a few files, use the incremental path instead:

```bash
cargo run -- add /absolute/path/to/repo
```

Open the regenerated Obsidian vault for broad graph exploration. Open the generated feature website
when you need a guided, code-linked view of one feature flow.

This merges live Postgres search results with relevant generated feature-page manifests.

## Notes

- `refresh` depends on the repository already being indexed.
- `chaos add` incrementally indexes the changed files first, then refreshes the same Obsidian vault.
- Obsidian output is always regenerated (by `refresh`, and by `add` unless `--no-obsidian` is set).
- Feature websites are generated from focused queries and current source snippets.
- The default feature-memory directory is `docs/features_memory`, separated from normal prose docs
  so agents can scan it without reading unrelated documentation.
- Feature maps such as Authorization/RBAC, upload flows, infrastructure correlation, and failure
  modes should use the same manifest schema.
