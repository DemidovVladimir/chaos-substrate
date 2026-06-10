# Quickstart: Chaos Substrate in Claude Code

The shortest path from a clean machine to a generated feature page in Claude Code:
**install Rust → bootstrap services → install the plugin → index a repo → generate
feature knowledge for end-to-end encryption.**

Every step has a copy-paste block and a one-line note on what it does. For depth, each
step links to the canonical doc. This guide uses **Claude Code (CLI)**, the **full plugin**
(skills + MCP tools + hooks), and **local Ollama** embeddings (no API key).

## 0. Prerequisites

You install everything else below; you only need these two first:

- **Docker** — runs the bundled Postgres + pgvector. Install Docker Desktop (macOS/Windows)
  or Docker Engine (Linux), and confirm `docker compose version` works.
- **Claude Code CLI** — the `claude` command. See the Claude Code install docs; confirm
  `claude --version` works.

## 1. Install Rust

The runtime is a single Rust binary, so you need the Rust toolchain (`cargo`).

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # macOS / Linux
source "$HOME/.cargo/env"
cargo --version                                                  # verify
```

> Windows: install from <https://rustup.rs>. Then use WSL or a Unix-like shell for the
> shell commands below.

## 2. Get the code

```bash
git clone https://github.com/chaos-substrate/chaos-substrate.git
cd chaos-substrate
```

## 3. Bootstrap services (one command)

Copy the example config (it already defaults to local Ollama), then run `bootstrap`:

```bash
cp chaos-substrate.example.toml chaos-substrate.toml
scripts/chaos-agent bootstrap
export PATH="$HOME/.local/bin:$PATH"          # so `chaos-agent` is on PATH
```

`bootstrap` does the whole setup in order: installs the `chaos-agent` wrapper, builds the
release binary (`cargo build --release`), starts Postgres (pgvector on host port `54329`),
starts Ollama and pulls `nomic-embed-text`, runs `chaos migrate`, then `chaos doctor`.

A healthy `doctor` reports Postgres, pgvector, provider (`ollama`), model
(`nomic-embed-text`), and dimensions (`768`). If it fails, the embedder or database is not
reachable — analysis is **fail-closed** and will not fabricate vectors.

- Embedder install/troubleshooting (and the dimension check): [OLLAMA_SETUP.md](OLLAMA_SETUP.md).
- Prefer OpenAI? Uncomment the `open_ai` block in `chaos-substrate.toml`, comment out the
  `ollama` block, and `export OPENAI_API_KEY=...` before bootstrap.
- No Docker / external Postgres? Set `CHAOS_NO_DOCKER=1` and point `DATABASE_URL` at your
  own instance. See [RUNBOOK.md](../RUNBOOK.md).

## 4. Install the plugin in Claude Code

The plugin bundles the skill, the nine MCP tools, and the tool-use hooks together.

For local testing, launch Claude Code pointed at this checkout:

```bash
claude --plugin-dir /absolute/path/to/chaos-substrate
```

For a real install, add the bundled marketplace (`.claude-plugin/marketplace.json`) and
install `chaos-substrate` from the `/plugin` UI. Full marketplace and Cowork-zip flow:
[PLUGIN_INSTALL.md](PLUGIN_INSTALL.md).

Inside Claude Code, verify the plugin loaded:

- The skill is available as `/chaos-substrate:chaos-substrate`.
- The fourteen MCP tools are listed: `chaos_analyze`, `chaos_add`, `chaos_stats`, `chaos_query`,
  `chaos_feature_context`, `chaos_impact`, `chaos_write_feature_website`, `chaos_obsidian`, `chaos_refresh`, `chaos_write_storyboard`, `chaos_change_plan`, `chaos_components`, `chaos_features`, `chaos_project`.
- The hooks inject code-memory context on `Grep` / `Glob` / `Bash` (safe no-op if the DB or
  index is unavailable).

> **MCP server only?** If you want the tools without the plugin, skip this step and run
> `scripts/chaos-agent claude-code-add local` (or `claude-code-add project /abs/path/to/repo`
> for a shareable `.mcp.json`). Per-editor details: [EDITOR_SETUP.md](EDITOR_SETUP.md).

## 5. Index your repository

Open your target repository (Rust, TypeScript, JavaScript, Python, or Solidity) in Claude
Code and ask it to index:

```text
Index this project with Chaos Substrate.
```

This calls `chaos_analyze`, building the persistent graph + embeddings in Postgres. Re-run
it (or "Update the Chaos Substrate index") whenever the code changes.

CLI mirror (on PATH after step 3):

```bash
chaos-agent update /absolute/path/to/your-repo
```

**Incremental indexing after edits.** Once a repo is analyzed, you don't have to re-index the
whole tree for small changes. After editing a few files, ask Claude Code to run `chaos_add`:

```text
Run chaos_add to index my changes and write a feature page.
```

`chaos_add` detects the changed files from git (working-tree staged + unstaged + untracked by
default; pass explicit paths or a `--since REF` range), re-embeds only those changed chunks
into Postgres/pgvector, refreshes the Obsidian vault, and writes a feature/bug HTML page into
`docs/features_memory/` — feature vs. bug is auto-detected from the branch name and latest
commit subject. CLI mirror:

```bash
chaos add /absolute/path/to/your-repo
chaos add /absolute/path/to/your-repo --since main --kind feature -m "describe the change"
```

**Sanity-check the index.** After an `analyze` or `add`, ask Claude Code to run `chaos_stats`
(or run the CLI mirror) to see index totals and breakdowns read from Postgres — files, nodes,
edges, chunks, embedded vs. missing, plus per-kind/per-language counts. It is read-only and
embedder-free. CLI mirror:

```bash
chaos stats /absolute/path/to/your-repo
```

> `chaos_add` does **not** rebuild cross-file call edges into unchanged files; run a full
> `chaos analyze` (or `chaos refresh`) for a complete graph rebuild. Like `analyze`, it
> requires a real embedder.

## 6. Generate feature knowledge for end-to-end encryption

With the repo indexed, ask Claude Code to build the feature page:

```text
Generate a feature explanation website for end-to-end encryption with Chaos Substrate.
```

Claude first calls `chaos_feature_context` to gather evidence (semantic + keyword hits,
graph context, existing manifests), then composes the page and calls
`chaos_write_feature_website`. The result is a self-contained dark HTML page with an
interactive graph, story flow, code context, and a machine-readable manifest, written under
`docs/features_memory/` in your repo.

> "end-to-end encryption" is just an example — swap in any feature your repo actually has.
> The writer **rejects README-like pages**: a feature page must include the interactive
> graph/story/code navigation, architecture/flow sections, evidence, and a populated
> manifest. If `chaos_feature_context` warns about missing indexed subtrees or missing doc
> hits, re-index (step 5) or re-target before writing.

CLI mirror — generate the page without the agent:

```bash
chaos-agent explain /absolute/path/to/your-repo "end-to-end encryption"
# writes docs/features_memory/end-to-end-encryption-explanation.html
```

Feature-context workflow and the manifest contract: [FEATURE_CONTEXT.md](FEATURE_CONTEXT.md).

## 7. Verify & troubleshoot

```bash
# Services + embedder healthy:
./target/release/chaos --config chaos-substrate.toml doctor

# MCP framing (newline-delimited JSON-RPC, no Content-Length) — expect one JSON line:
printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}\n' \
  | ./target/release/chaos --config chaos-substrate.toml mcp
```

If Claude Code doesn't see the tools: restart Claude Code, confirm `docker compose ps` shows
Postgres, and confirm `doctor` succeeds. If indexing fails, check Ollama is running and the
model is pulled — do not bypass embedder failures with mock vectors.

Deeper references:

- [EDITOR_SETUP.md](EDITOR_SETUP.md) — canonical per-editor MCP registration.
- [CLAUDE_MCP_INSTALL.md](CLAUDE_MCP_INSTALL.md) — Claude Desktop config and a longer MCP walkthrough.
- [PLUGIN_INSTALL.md](PLUGIN_INSTALL.md) — plugin package, marketplace, and Cowork zip.
- README → [MCP Tools](../README.md#mcp-tools) — the twelve-tool reference.
