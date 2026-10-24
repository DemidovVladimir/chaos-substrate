# chaos-kb

A personal knowledge base for agents. You write down what you know, such as your education,
licences, certificates, how you maintain your car, how you like to communicate, or your medical
treatment, as ordinary Markdown notes in one folder. chaos-kb indexes that folder so an agent can
ask two questions quickly:

1. **Is this already known?** (`check`). If the same words are stored, a fingerprint lookup
   answers without calling the embedding model. Otherwise chaos-kb compares meaning: the cosine
   similarity of embeddings.
2. **What do my notes say about it?** (`search`). It returns the closest passages, each with its
   note, section, line range and text.

When the answer is "not known", the agent asks you and stores your answer with `remember`. That
asking behaviour belongs to your agent. chaos-kb is only the memory and the quick lookups.

Everything runs on your machine. The notes are files you own, the index is one file next to
them, and embeddings come from a local [Ollama](https://ollama.com) model. Nothing is sent
anywhere unless you point chaos-kb at a cloud embeddings API yourself.

This is version 2 of Chaos Substrate, in the `v2/` folder. It shares no code with the 1.0 crate
at the repository root, which indexes code repositories into Postgres.

## How it works

```text
 your notes (Markdown, an Obsidian vault)
        │  sync
        ▼
 blocks ─ fingerprint (BLAKE3 of the words) ─▶ Merkle tree ─▶ root hash
   │                                            exact "is this known?"
   │
   └─ chunks (≤ 1200 chars, with "Title > Heading") ─▶ embeddings (Ollama) ─▶ index file (redb)
                                                       "what do I know about it?", by meaning
```

- **Block**: the smallest piece of knowledge. It is a paragraph, one list item, a table, a quote,
  a code block, or the note's front matter.
- **Fingerprint**: a BLAKE3 hash of a block's words that ignores case, punctuation, spacing and
  Markdown symbols. `Allergic to penicillin.` and `- **allergic** to PENICILLIN` get the same
  fingerprint.
- **Merkle tree**: fingerprints roll up into one hash per note, one per folder, and one root
  hash for the whole vault. This gives two things:
  - A fact stored with the same words is found with one lookup, without an embedding call.
  - If the root hash has not changed, nothing in your knowledge has changed. An agent can cache
    what it learned and compare a single hash. Every report includes the root.

  A Merkle tree cannot tell that *"how often do I change the oil?"* is about your note *"I change
  the engine oil every 10 000 km"*: the words differ but the meaning is the same. Embeddings
  handle that, so `check` falls back to them.
- **Chunk**: consecutive blocks under one heading, packed up to 1200 characters, and embedded
  together with a short context line (`Note title > Heading`). A chunk is embedded again only when
  its content changes, and the vectors of deleted text are dropped.
- **Search**: the question is embedded and compared with every stored vector. The scan is exact,
  with no approximation, because a personal vault is small enough for that.

## Install

You need Rust 1.91 or later, and Ollama with the embedding model:

```sh
ollama pull embeddinggemma
cargo install --path v2          # installs the `chaos-kb` binary
```

## Use

```sh
export CHAOS_KB_VAULT=~/Knowledge     # any folder of .md files; open it in Obsidian if you like
chaos-kb sync                         # run after you edit notes
chaos-kb check "how often do I change the oil?"
chaos-kb search "how should things be explained to me?"
chaos-kb remember "Blood type: A positive." --note Health --heading Blood
```

Every command prints JSON. Here is a `check` whose answer was found by meaning:

```json
{
  "known": true,
  "presence": "similar",
  "score": 0.583015501499176,
  "matches": [
    { "note": "Car.md", "section": "Maintenance > Oil", "lines": [8, 8], "score": 0.583015501499176 }
  ],
  "root": "f667e5f87e7edfd0635d630b9d85d09b2a058f1e9a8afe5879a985f266bddc9e"
}
```

`presence` takes one of three values:

- `exact`: the same words are stored, and `matches` lists every place they appear.
- `similar`: a passage is at least `min_score` similar.
- `absent`: nothing comes close enough.

`search` returns the same places with the passage `text`, so the agent can read the answer
without opening the note.

The index lives at `<vault>/.chaos-kb/index.redb`. Obsidian ignores folders whose names start
with a dot. Deleting the folder is safe, because `sync` rebuilds it from the notes.

### From an agent (MCP)

`chaos-kb mcp` serves the same four tools over MCP (stdio, newline-delimited JSON-RPC 2.0). For
Claude Code:

```sh
claude mcp add chaos-kb -- chaos-kb --vault ~/Knowledge mcp
```

Other clients take the same command and arguments in their MCP config. The server also gives the
agent a short workflow guide: check before asking, search to read, remember one fact per call,
and sync after editing notes.

## Tools

This reference is generated from the command-line definition. `cargo test` fails when it is out
of date.

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
| `min_score` | number | `0.38` | Similarity from which a passage counts as covering the query |

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

The embeddings API key, when the endpoint needs one, is read only from the `CHAOS_KB_EMBED_API_KEY` environment variable, never from a flag.
<!-- tools:end -->

## How `check` decides

1. **Same words**: if the query, compared word for word, equals a stored block, the answer is
   `exact`. For example, `allergic to PENICILLIN` matches the block `Allergic to penicillin.`
   No embedding is needed.
2. **Same meaning**: otherwise the query is embedded, and the best of the three nearest passages
   decides. A score of at least `min_score` (default 0.38) means `similar`; anything lower means
   `absent`.

The default is calibrated for `embeddinggemma` on the example vault in `tests/e2e.rs`:

| questions | cosine score |
|---|---|
| answered by the vault (8, including one in Russian) | 0.42 to 0.62 |
| near misses on the same topics ("do I have a pilot licence?") | 0.09 to 0.35 |
| unrelated ("what is the name of my dog?") | 0.13 to 0.18 |

The gap between the two groups is narrow, so treat `similar` as "probably known, read it". Some
paraphrases fall below the line. For example, *"can I drive a car legally?"* scores 0.30 against
*"Driving licence category B since 2015."* An agent that gets `absent` with a score close to
the line should `search` before it asks you. A different embedding model needs its own
`--min-score`.

## Speed

These timings were measured with the release build on an Apple M4, using Ollama 0.24 and
`embeddinggemma` on a vault of 502 notes (1003 chunks). Each time covers the whole process run:

| command | time |
|---|---|
| first `sync` (embeds all 1003 chunks) | 13.2 s |
| `sync` with nothing changed | 0.10 s |
| `check`, same words (no embedding) | 0.04 s |
| `check`, by meaning (one embedding call) | 0.15 s |
| `search` | 0.10 s |

The index file for that vault is 8.4 MB.

## Your data

- **The notes are the only copy of your knowledge.** chaos-kb writes to the vault only when you
  call `remember`. It appends to one note through a temporary file and a rename, so the note is
  never half-written. It refuses to write outside the vault or through symlinked folders.
- **Skipped**: hidden files and folders (such as `.obsidian`), symlinks, files that are not
  Markdown, and notes larger than 4 MiB.
- **Embeddings are local by default** (Ollama on `localhost`). If you point `--embed-url` at a
  cloud API, your note text is sent there to be embedded. The API key is read only from
  `CHAOS_KB_EMBED_API_KEY` and is never printed.
- **Changing the embedding model** re-embeds everything on the next `sync`, because vectors from
  different models cannot be compared. `check` and `search` ask you to run `sync` first.

## Development

Rules for changing the code are in [AGENTS.md](AGENTS.md). To validate:

```sh
cd v2
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test        # the end-to-end tests need Ollama with embeddinggemma; they skip without it
```
