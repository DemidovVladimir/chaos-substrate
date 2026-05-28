# Graph Webpage Tutorial

Chaos Substrate can export an indexed repository to a standalone `graph.html` page. Use it to inspect
the persisted knowledge graph after indexing.

The graph page is a static file. It does not start an HTTP server, use Node.js, or call OpenAI/Ollama.
It reads graph data from Postgres during export and embeds that data into the generated HTML.

## 1. Start Storage

```sh
docker compose up -d
```

## 2. Configure A Real Embedder

OpenAI:

```sh
cp chaos-substrate.example.toml chaos-substrate.toml
export OPENAI_API_KEY="..."
```

Ollama:

```sh
cp chaos-substrate.local.toml chaos-substrate.toml
```

See `docs/OLLAMA_SETUP.md` if Ollama is not installed or the model is missing. For agent use,
`chaos-agent bootstrap` performs the Ollama readiness step automatically.

## 3. Prepare The Database

```sh
cargo run -- migrate
cargo run -- doctor
```

`doctor` should report Postgres, pgvector, provider, model, and embedding dimensions.

## 4. Index A Repository

```sh
cargo run -- analyze /absolute/path/to/repo
```

Expected output includes counts for files, nodes, edges, chunks, and embedded chunks.

## 5. Export The Webpage

```sh
cargo run -- graph /absolute/path/to/repo --output graph.html
```

You can also use the repository name if it is unique in the database:

```sh
cargo run -- graph repo-name --output graph.html
```

## 6. Inspect The Graph

Open `graph.html` in a browser.

Validate these areas:

- repository and file nodes exist for the expected indexed files
- symbol nodes exist for important functions, classes, structs, traits, tests, and deployment resources
- dependency nodes exist for Cargo or npm packages
- `contains` edges connect repository, files, and symbols
- `imports`, `depends_on`, `calls`, `defines`, `configures`, and `deploys` edges appear where expected
- clicked nodes show stable IDs, file paths, line ranges, chunk counts, and metadata

Useful interactions:

- search by node name, file path, stable ID, or kind
- filter visible nodes by kind
- zoom and pan around dense graphs
- drag a node to pin it while checking nearby relationships

## 7. Re-Index And Compare

After changing the target repository, run:

```sh
cargo run -- analyze /absolute/path/to/repo
cargo run -- graph /absolute/path/to/repo --output graph.html
```

The index replacement is repository-scoped. The export should reflect the latest persisted nodes and
edges for that repository.

## Troubleshooting

If the graph command says the repository is not indexed, run `analyze` with the same absolute path
you pass to `graph`.

If the page has nodes but no useful relationships, check the `edges` count in the `analyze` output
and inspect Postgres:

```sql
select kind, count(*) from edges group by kind order by kind;
```

If the graph is visually dense, use search and kind filters to isolate one subsystem at a time.

For very large repositories, you can also export the same graph as an Obsidian vault:

```sh
cargo run -- obsidian /absolute/path/to/repo --output chaos-obsidian-vault
```

See [OBSIDIAN_EXPORT.md](OBSIDIAN_EXPORT.md) for that workflow.
