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
- Open the app once so the local server starts.

Windows:

- Download Ollama from `https://ollama.com/download`.
- Open the app once so the local server starts.

Linux:

```bash
curl -fsSL https://ollama.com/install.sh | sh
```

Then start the server if it is not already running:

```bash
ollama serve
```

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
scripts/chaos-agent ollama-setup
CHAOS_CONFIG=chaos-substrate.local.toml scripts/chaos-agent doctor
CHAOS_CONFIG=chaos-substrate.local.toml scripts/chaos-agent init /absolute/path/to/project
CHAOS_CONFIG=chaos-substrate.local.toml scripts/chaos-agent explain /absolute/path/to/project "authorization and RBAC"
```

## Troubleshooting

If `doctor` cannot connect to Ollama:

- Confirm Ollama is running.
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
