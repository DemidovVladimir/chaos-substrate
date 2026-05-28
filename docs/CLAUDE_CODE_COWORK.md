# Claude Code And Cowork Setup

Use this guide when a target repository should be usable from Claude Code or a Claude Cowork-style
agent workflow.

## 1. Prepare Chaos Substrate

From the Chaos Substrate checkout:

```sh
scripts/chaos-agent bootstrap
export PATH="$HOME/.local/bin:$PATH"
```

Use `chaos-substrate.local.toml` for Ollama. `chaos-agent ollama-setup` tries to start Ollama and
pull `nomic-embed-text` automatically when Ollama is installed.

## 2. Install Chaos Substrate In Claude Cowork

Cowork installs custom plugins from a file upload in the Claude Desktop UI.

From the Chaos Substrate checkout:

```sh
scripts/package-cowork-plugin
```

If the Claude CLI is installed, this validates the plugin package before creating the zip.
The package also builds and includes `target/release/chaos`, so Cowork does not depend on a stale
or missing release binary when exposing MCP tools.

Upload this file in Claude Desktop -> Cowork -> Customize -> Plugins:

```text
dist/chaos-substrate-cowork-plugin.zip
```

After installation, type `/` or use the `+` button in Cowork and verify the Chaos Substrate skill is
available.

## 3. Add Chaos Substrate To Claude Code

Chaos Substrate can be loaded as a Claude Code plugin:

```sh
claude --plugin-dir /absolute/path/to/chaos-substrate
```

The plugin exposes the namespaced skill:

```text
/chaos-substrate:chaos-substrate
```

The plugin also includes root `.mcp.json` and `bin/chaos-agent`. For an explicit project MCP entry,
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

## 4. Manual `.mcp.json`

If you prefer to manage the file directly, copy `docs/claude_code_mcp.example.json` into the target
project as `.mcp.json` and replace the fallback paths.

Claude Code supports environment-variable expansion in `.mcp.json`, so teams can keep this file in
git while each developer sets local paths:

```sh
export CHAOS_BIN=/absolute/path/to/chaos-substrate/target/release/chaos
export CHAOS_CONFIG=/absolute/path/to/chaos-substrate/chaos-substrate.toml
export DATABASE_URL=postgres://chaos:chaos@localhost:54329/chaos_substrate
```

## 5. Add Project Instructions

Claude Code and Cowork-style agents should read project instructions from `CLAUDE.md`. In the target
project, add a short section like this:

```md
## Chaos Substrate

Use Chaos Substrate before non-trivial architecture, security, or feature work.

- Index/update: ask for `chaos_analyze` on this repository.
- Query: use `chaos_query` with a focused question before editing.
- Feature context: use `chaos_feature_context`.
- Feature website generation: read `chaos_feature_context`, compose the feature-specific page and
  manifest, then call `chaos_write_feature_website`.
- Do not treat generated docs as source of truth when source code disagrees.
- Feature-map story steps must use explicit `node_ids`/`edge_ids`; do not infer step scope by
  graph expansion.
```

## 6. Use From Claude

After configuring MCP, ask Claude Code:

```text
Use chaos_analyze on this repository, then explain the authorization flow.
```

Or:

```text
Use chaos_query on this repository. Question: where is request authorization enforced?
```

For feature explanations and website generation, use the MCP two-step workflow:

```text
1. Use chaos_feature_context on this repository.
   Task: explain OCL across the project.
2. Read the returned evidence.
3. Compose the feature-specific website and chaos-feature-manifest.
4. Use chaos_write_feature_website to write docs/features_memory/ocl-explanation.html.
```

If Claude only uses `chaos_query` for a feature explanation, the plugin is stale or MCP is exposing
the old tool surface. Rebuild and re-upload `dist/chaos-substrate-cowork-plugin.zip`, then verify the
MCP tool list contains `chaos_feature_context` and `chaos_write_feature_website`.

Claude Cowork-style sandboxes may not be able to reach host Postgres or write project files
directly. In that case, the agent should use the host MCP tools instead of claiming only the CLI can
do the work. If `chaos_write_feature_website` cannot write the file, it should still return the
feature context and state that filesystem writing was blocked.

For large output, Claude Code supports MCP output limit environment variables. If responses are
truncated, start Claude Code with a higher limit, for example:

```sh
MAX_MCP_OUTPUT_TOKENS=50000 claude
```

## 7. What Not To Do

- Do not configure Chaos Substrate through `cargo run` in MCP settings; use the release binary.
- Do not copy the skill into every project; load the Claude plugin or use `chaos-agent onboard`.
- Do not reduce feature explanation to `chaos_query` only when `chaos_feature_context` is available.
- Do not use a static script as a substitute for feature understanding; compose the feature website
  after reading evidence, then write it with `chaos_write_feature_website`.
- Do not expose Chaos Substrate as HTTP just for Claude Code. Keep MCP on stdio.
- Do not commit personal absolute paths, database dumps, or API keys.
- Do not bypass embedder failures with fake vectors.
