# chaos-kb

<!-- tools:start -->
### `kb_sync` · `chaos-kb sync`

Bring the index up to date with the vault: rebuild the Merkle tree, embed only new or edited passages, drop deleted ones. Run after editing notes

| arg | type | default | description |
|---|---|---|---|
| `dry_run` | boolean | `false` | Report what would change without embedding or writing anything |

### `kb_remember` · `chaos-kb remember`

Store one fact in the vault as Markdown and index it. Skipped when the same words are already stored anywhere in the vault

| arg | type | default | description |
|---|---|---|---|
| `text` (required) | string |  | The fact, in plain words or Markdown. One fact per call |
| `heading` | string |  | Section heading inside the note (added when missing) |
| `note` | string | `Inbox.md` | Note to append to, relative to the vault (`.md` optional) |

### `kb_check` · `chaos-kb check`

Is this already in the knowledge base? Exact words are answered from the Merkle leaves without embedding; otherwise the nearest passages by cosine similarity decide. Returns where the knowledge sits

| arg | type | default | description |
|---|---|---|---|
| `query` (required) | string |  | A fact, topic or question |
| `min_score` | number | `0.5` | Similarity from which a passage counts as covering the query |

### `kb_search` · `chaos-kb search`

Passages most similar to a question, best first, with note, section, line range and text

| arg | type | default | description |
|---|---|---|---|
| `query` (required) | string |  | A question or topic |
| `limit` | integer ≥ 0 | `5` | Maximum passages returned |
| `min_score` | number | `0.0` | Drop passages below this similarity |

### Settings

| flag | env var | default | description |
|---|---|---|---|
| `--vault` | `CHAOS_KB_VAULT` |  | Folder of Markdown notes (an Obsidian vault) |
| `--index` | `CHAOS_KB_INDEX` |  | Index file [default: &lt;vault>/.chaos-kb/index.redb] |
| `--embed-url` | `CHAOS_KB_EMBED_URL` | `http://localhost:11434/v1` | OpenAI-compatible embeddings base URL (Ollama, OpenRouter, OpenAI) |
| `--embed-model` | `CHAOS_KB_EMBED_MODEL` | `embeddinggemma` | Embedding model |
<!-- tools:end -->
