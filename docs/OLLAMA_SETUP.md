# Ollama Setup

Use Ollama when you want Chaos Substrate to create embeddings locally instead of using OpenAI.

Chaos Substrate expects Ollama's `/api/embed` endpoint and defaults to:

```toml
[embedding]
provider = "ollama"
model = "nomic-embed-text"
dimensions = 768
base_url = "http://localhost:11434"
```

The ready-to-use local config is:

```text
chaos-substrate.local.toml
```

## 1. Install Ollama

Official install page:

```text
https://ollama.com/download
```

macOS:

- Download Ollama from `https://ollama.com/download`.
- The wrapper will try to open the Ollama app automatically when `chaos ollama-setup` runs.

Windows:

- Download Ollama from `https://ollama.com/download`.
- Open the app once if `chaos ollama-setup` cannot reach the local server.

Linux:

```bash
curl -fsSL https://ollama.com/install.sh | sh
```

Then start the server if it is not already running:

```bash
ollama serve
```

`chaos ollama-setup` will try `systemctl` first and then `ollama serve` in the background on
Linux, so this manual command is usually only needed when the local install blocks background start.

## 2. Pull The Embedding Model

```bash
ollama pull nomic-embed-text
```

This model returns 768-dimensional embeddings, so `dimensions = 768` must stay in the config.

## 3. Check Ollama Is Serving

```bash
curl http://localhost:11434/api/tags
```

You should get a JSON response listing local models.

## 4. Run Chaos Substrate With Ollama

From the Chaos Substrate directory:

```bash
docker compose up -d
cargo build --release
./target/release/chaos --config chaos-substrate.local.toml migrate
./target/release/chaos --config chaos-substrate.local.toml doctor
./target/release/chaos --config chaos-substrate.local.toml analyze /absolute/path/to/project
./target/release/chaos --config chaos-substrate.local.toml refresh /absolute/path/to/project
```

For normal agent use, prefer the wrapper:

```bash
CHAOS_CONFIG=chaos-substrate.local.toml scripts/chaos bootstrap
export PATH="$HOME/.local/bin:$PATH"
```

After bootstrap, use the plugin from the target project with natural requests such as:

```text
Use Chaos Substrate on this project and create an index plus explanation.
Generate a feature explanation website for authorization and RBAC.
```

`bootstrap`, `doctor`, `onboard`, `init`, and `update` all enforce Ollama readiness when the active
config uses `provider = "ollama"`. `doctor` also performs a real embedding probe, so it fails before
printing success if the Ollama server cannot answer.

## Troubleshooting

If `doctor` cannot connect to Ollama:

- Run `chaos ollama-setup`; it tries to start Ollama and pull `nomic-embed-text`.
- Confirm `curl http://localhost:11434/api/tags` returns JSON.
- Confirm `nomic-embed-text` is installed with `ollama list`.
- Confirm `chaos-substrate.local.toml` has `provider = "ollama"` and `dimensions = 768`.

If analysis fails with an embedding dimension mismatch:

- Keep `nomic-embed-text` paired with `dimensions = 768`.
- If you switch to another embedding model, update `dimensions` to exactly match that model.
- Do not bypass this check with fake vectors; Chaos Substrate intentionally fails when dimensions do
  not match.

If `ollama pull nomic-embed-text` fails:

- Check network access.
- Run `ollama --version`.
- Restart Ollama and try the pull again.
