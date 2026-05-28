# Claude Code And Cowork Setup

Use this guide when a target repository should be usable from Claude Code or a Claude Cowork-style
agent workflow.

## 1. Prepare Chaos Substrate

From the Chaos Substrate checkout:

```sh
scripts/chaos-agent bootstrap
export PATH="$HOME/.local/bin:$PATH"
```

Use `chaos-substrate.local.toml` for Ollama, and make sure `ollama serve` plus
`ollama pull nomic-embed-text` have been run.

## 2. Add Chaos Substrate To Claude Code

Claude Code supports local stdio MCP servers through `claude mcp add`. The wrapper can register the
server for you:

```sh
chaos-agent claude-code-add local /absolute/path/to/target-repo
```

Scopes:

- `local`: private to the current Claude Code project.
- `project`: writes a shareable `.mcp.json` in the project root.
- `user`: available across your local projects.

For a team-shared project config, run:

```sh
chaos-agent claude-code-add project /absolute/path/to/target-repo
```

For example:

```sh
chaos-agent claude-code-add project /absolute/path/to/infra-repo
```

The second argument is the Claude Code project directory where `.mcp.json` should be written. If you
omit it, the wrapper uses the current working directory.

Claude Code will ask for approval before using project-scoped MCP servers from `.mcp.json`.

## 3. Manual `.mcp.json`

If you prefer to manage the file directly, copy `docs/claude_code_mcp.example.json` into the target
project as `.mcp.json` and replace the fallback paths.

Claude Code supports environment-variable expansion in `.mcp.json`, so teams can keep this file in
git while each developer sets local paths:

```sh
export CHAOS_BIN=/absolute/path/to/chaos-substrate/target/release/chaos
export CHAOS_CONFIG=/absolute/path/to/chaos-substrate/chaos-substrate.toml
export DATABASE_URL=postgres://chaos:chaos@localhost:54329/chaos_substrate
```

## 4. Add Project Instructions

Claude Code and Cowork-style agents should read project instructions from `CLAUDE.md`. In the target
project, add a short section like this:

```md
## Chaos Substrate

Use Chaos Substrate before non-trivial architecture, security, or feature work.

- Index/update: ask for `chaos_analyze` on this repository.
- Query: use `chaos_query` with a focused question before editing.
- Feature context: prefer `chaos-agent context <repo> "<task>"` or generated
  `docs/features_memory/*.html` manifests when available.
- Do not treat generated docs as source of truth when source code disagrees.
- Feature-map story steps must use explicit `node_ids`/`edge_ids`; do not infer step scope by
  graph expansion.
```

## 5. Use From Claude

After configuring MCP, ask Claude Code:

```text
Use chaos_analyze on this repository, then explain the authorization flow.
```

Or:

```text
Use chaos_query on this repository. Question: where is request authorization enforced?
```

For large output, Claude Code supports MCP output limit environment variables. If responses are
truncated, start Claude Code with a higher limit, for example:

```sh
MAX_MCP_OUTPUT_TOKENS=50000 claude
```

## 6. What Not To Do

- Do not configure Chaos Substrate through `cargo run` in MCP settings; use the release binary.
- Do not expose Chaos Substrate as HTTP just for Claude Code. Keep MCP on stdio.
- Do not commit personal absolute paths, database dumps, or API keys.
- Do not bypass embedder failures with fake vectors.
