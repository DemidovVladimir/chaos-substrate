# chaos-kb

A personal knowledge base for agents. You write down what you know, such as your education,
licences, certificates, how you maintain your car, how you like to communicate, or your medical
treatment, as ordinary Markdown notes in one folder. chaos-kb indexes that folder so an agent can
ask two questions quickly:

1. **Is this already known?** (`check`). If the same words are stored, chaos-kb answers by
   looking up their fingerprint, without calling the embedding model. Otherwise it compares
   meaning: the cosine similarity of embeddings.
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
  a code block, an HTML block, or the note's front matter. Headings are not blocks; they label
  the blocks under them.
- **Fingerprint**: a BLAKE3 hash of a block's words. Before hashing, chaos-kb removes the
  Markdown syntax, keeps a link's label and its destination, and ignores case, spacing, sentence
  punctuation, quotes and brackets. `Allergic to penicillin.` and `- **allergic** to PENICILLIN`
  get the same fingerprint. Every other symbol counts, so `Blood type: A+` and `Blood type: A-`
  stay two different facts, and so do `≤ 140` and `≥ 140`.
- **Merkle tree**: the fingerprints of a note, each labelled by its headings, roll up into one
  hash per note, and the note hashes roll up into one root hash for the whole vault. This gives
  two things:
  - A fact stored with the same words is found by looking up its fingerprint (a leaf of the
    tree), without an embedding call.
  - If the root hash has not changed, nothing in your knowledge has changed. An agent can cache
    what it learned and compare a single hash. Every report includes the root. Formatting-only
    edits, such as bold text or extra spaces, keep every hash.

  A Merkle tree cannot tell that *"how often do I change the oil?"* is about your note *"I change
  the engine oil every 10 000 km"*: the words differ but the meaning is the same. Embeddings
  handle that, so `check` falls back to them.
- **Chunk**: consecutive blocks under one heading, packed up to 1200 characters, and embedded
  together with a short context line (`Note title > Heading`). A chunk is embedded again only when
  its text, its note's title or its headings change. The vectors of deleted text are dropped.
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

Every command prints JSON. These samples come from the benchmark vault described under
[Speed](#speed). A `check` whose answer was found by meaning:

```json
{
  "known": true,
  "matches": [
    {
      "lines": [8, 8],
      "note": "Car.md",
      "score": 0.583015501499176,
      "section": "Maintenance > Oil"
    }
  ],
  "presence": "similar",
  "root": "86a0327e7604fd6dd90baf53f1eb7f46fd39de883bcdb5157f62d71687ad9e5d",
  "score": 0.583015501499176
}
```

The same note, asked in the same words (`"i change the ENGINE oil every 10 000 km"`), is found
by its fingerprint. Nothing is embedded, so there is no score:

```json
{
  "known": true,
  "matches": [
    { "lines": [8, 8], "note": "Car.md", "section": "Maintenance > Oil" }
  ],
  "presence": "exact",
  "root": "86a0327e7604fd6dd90baf53f1eb7f46fd39de883bcdb5157f62d71687ad9e5d"
}
```

`presence` takes one of three values:

- `exact`: every block of the query is stored with the same words, and `matches` lists every
  place they appear.
- `similar`: a passage is at least `min_score` similar.
- `absent`: nothing comes close enough. `score` still shows the best similarity found.

`search` returns a list of `hits`: the same places, each with its `score` and the passage `text`,
so the agent can read the answer without opening the note. `search` always returns the best
passages it has, even weak ones; raise `--min-score` to drop them.

`sync` reports the notes it `added`, `changed` and `removed`, how many chunks the index holds, how
many it `embedded`, and the new `root`. `remember` reports whether it `stored` the fact, or where
the same words already sit (`known_at`).

The index lives at `<vault>/.chaos-kb/index.redb`. Obsidian ignores folders whose names start
with a dot. Deleting the folder is safe, because `sync` rebuilds it from the notes. An index
written by a chaos-kb build with a different table layout reads as empty, and the next `sync`
rebuilds it.

### From an agent (MCP)

`chaos-kb mcp` serves the same four tools over MCP (stdio, newline-delimited JSON-RPC 2.0). For
Claude Code:

```sh
claude mcp add chaos-kb -- chaos-kb --vault ~/Knowledge mcp
```

Other clients take the same command and arguments in their MCP config. When it starts, the
server also tells the agent how to use the four tools together.

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

The embeddings API key, when the endpoint needs one, is read only from the `CHAOS_KB_EMBED_API_KEY` environment variable, never from a flag. It is sent only over `https://`, or over plain `http://` to this machine (`localhost`, `127.0.0.0/8`, `::1`).
<!-- tools:end -->

## How `check` decides

1. **Same words**: if every block of the query, compared word for word, equals a stored block,
   the answer is `exact`. For example, `allergic to PENICILLIN` matches the block
   `Allergic to penicillin.` No embedding is needed, so this works even before the embedding
   model is running.
2. **Same meaning**: otherwise the query is embedded, and the best of the nearest passages
   decides. A score of at least `min_score` means `similar`; anything lower means `absent`.

The default `min_score` is calibrated for `embeddinggemma` on the example vault in
`tests/e2e.rs`:

| questions | cosine score |
|---|---|
| answered by the vault (8, including one in Russian) | 0.42 to 0.62 |
| near misses on the same topics ("do I have a pilot licence?") | 0.09 to 0.35 |
| unrelated ("what is the name of my dog?") | 0.13 to 0.18 |

The gap between answered questions and near misses is narrow, so treat `similar` as "probably
known, read it", and treat `absent` with a score close to the line as "search before you ask".
A different embedding model needs its own `--min-score`.

## Speed

These timings were measured with the release build on an Apple M4, using Ollama 0.24 and
`embeddinggemma` on a vault of 503 notes (1004 chunks). Each time covers the whole process run:

| command | time |
|---|---|
| first `sync` (embeds all 1004 chunks) | 12.6 s |
| `sync` with nothing changed | 0.09 s |
| `sync` after editing one note (one chunk embedded) | 0.14 s |
| `check`, same words (no embedding) | 0.04 s |
| `check`, by meaning (one embedding call) | 0.14 s |
| `search` | 0.09 s |

A fresh index file for that vault is 8.4 MB. The file does not shrink when text is deleted:
redb reuses the freed space for later writes instead.

## Your data

- **The notes are the only copy of your knowledge.** chaos-kb writes to your notes only when you
  call `remember`. `sync`, `check` and `search` only read them.
- **How `remember` writes**: with a heading, the fact goes right under that heading's own text,
  before any subheading, so it reads under exactly that heading. A missing heading is added at
  the end of the note. The note is written through a temporary file and a rename, so it is never
  half-written, and it keeps its file permissions. Existing text is never changed.
- **What `remember` refuses**: writing outside the vault or through a symlinked folder or note,
  changing a read-only note or one that is not valid UTF-8, and text that would change how the
  rest of the note reads (an unclosed code fence or HTML comment, or a heading that would take
  in the sections after it). A refused call writes nothing.
- **Several processes take turns.** A CLI command and a running MCP server can work on the same
  vault: each index operation holds `<index>.lock`, and appends hold
  `<vault>/.chaos-kb/append.lock`. When another process changed the index during a `sync`, the
  `sync` starts over (up to three times).
- **Skipped when reading**: hidden files and folders (such as `.obsidian`), symlinks, files that
  are not Markdown (`.md`), and notes whose path contains `:`, `\` or a control character,
  begins with a space, or is not valid UTF-8. Every note is read whole,
  whatever its size; invalid UTF-8 inside a note is replaced, not fatal.
- **Embeddings are local by default** (Ollama on `localhost`). If you point `--embed-url` at a
  cloud API, your note text is sent there to be embedded. The API key is read only from
  `CHAOS_KB_EMBED_API_KEY` and is never printed. Proxy settings from the environment
  (`ALL_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY`) are used for remote embedders only, never for `localhost`.
- **Changing the embedding model** re-embeds everything on the next `sync`, because vectors from
  different models cannot be compared. Until then, `check` still answers `exact` matches, and
  `check` by meaning and `search` ask you to run `sync`.

## Development

Rules for changing the code, and the commands that validate it, are in [AGENTS.md](AGENTS.md).
A visual tutorial, one page per feature, is in [site/](site/index.html); `cargo test --test
site` fails when it falls out of step with the code. The end-to-end tests in `tests/e2e.rs` run
the real vault, index and Ollama: they skip when nothing listens on `localhost:11434`, and fail
when Ollama runs without `embeddinggemma`.
