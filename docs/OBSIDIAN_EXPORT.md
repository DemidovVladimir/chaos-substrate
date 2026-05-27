# Obsidian Export Tutorial

Chaos Substrate can export an indexed repository into a local Obsidian vault. Use this when the
browser graph is too dense or when you want to inspect relationships through backlinks, search, and
Obsidian's graph view.

The export reads persisted graph data from Postgres. It does not re-index source files and does not
call OpenAI or Ollama.

## 1. Index A Repository

```sh
cargo run -- analyze /absolute/path/to/repo
```

The output should include non-zero counts for files, nodes, edges, chunks, and embedded chunks.

## 2. Export The Vault

```sh
cargo run -- obsidian /absolute/path/to/repo --output chaos-obsidian-vault
```

You can also use the repository name if it is unique in the database:

```sh
cargo run -- obsidian repo-name --output chaos-obsidian-vault
```

The command prints a JSON summary with the output folder, repository id, topic count, node note
count, and edge count.

## 3. Open In Obsidian

In Obsidian, choose "Open folder as vault" and select the generated output directory.

The vault contains:

- `README.md` with repository counts and starting links
- `Topics/` notes for inferred subsystems and feature areas
- `Nodes/` notes for files, symbols, dependencies, scripts, tests, and deployment resources
- `Edges.md` with a compact relationship manifest
- `.obsidian/graph.json` with graph view defaults

## 4. Validate The Graph

Start with `README.md`, then open topic notes. Each topic links to representative nodes and each
node links back to its topic when a topic can be inferred.

Node notes include:

- source file and line range when available
- node kind and stable id
- chunk count
- outgoing relationships
- incoming relationships
- raw metadata as JSON

Use Obsidian's backlinks panel to inspect reverse relationships. Use the graph view to spot isolated
topics, heavily connected dependencies, and unexpected clusters.

## Troubleshooting

If the export says the repository is not indexed, run `analyze` with the same absolute path you pass
to `obsidian`.

If the vault is very large, start from `Topics/` rather than opening the global graph immediately.
Large repositories can produce thousands of node notes, and Obsidian will be faster when you inspect
one topic or search result at a time.
