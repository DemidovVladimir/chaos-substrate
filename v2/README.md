# chaos-kb

A personal knowledge base for agents. You write down what you know, such as your education,
licences, certificates, how you maintain your car, how you like to communicate, or your medical
treatment, as ordinary Markdown notes in one folder. chaos-kb indexes that folder so an agent can
ask three questions quickly:

1. **Is this already known?** (`check`). If the same words are stored, chaos-kb answers by
   looking up their fingerprint, without calling the embedding model. Otherwise it compares
   meaning: the cosine similarity of embeddings.
2. **What do my notes say about it?** (`search`). It returns the closest passages, each with its
   note, section, line range and text.
3. **Which of my notes matter for this task?** (`topics`, then `read`). `topics` lists the
   closest passages as one short line each, without their text; the agent keeps the ones that
   would change how it does the task and fetches only those with `read`. See
   [How `topics` and `read` work together](#how-topics-and-read-work-together).

When the answer is "not known", the agent asks you and stores your answer with `remember`. That
asking behaviour belongs to your agent. chaos-kb is only the memory and the quick lookups. When a
stored fact is wrong or no longer true, `forget` takes it out by its words, and `remember` stores
the correction. To see what is stored, and to add, replace or remove facts yourself, open the
[dashboard](#dashboard).

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
 blocks ─ fingerprint (BLAKE3 of the words) ─▶ exact "is this known?"
   │            └─▶ note hash ─▶ which notes changed since the last sync
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
  stay two different facts, and so do `≤ 140` and `≥ 140`. A colon between digits counts too:
  `1:5` is a ratio, `1.5` a number, while `1,5` and `1.5` are the same number.
- **Note hash**: the fingerprints of a note in order, each labelled by its headings, hashed
  together. `sync` compares each note's hash with the one the index holds, so it knows which
  notes were added, changed or removed. Formatting-only edits, such as bold text or extra spaces,
  keep every hash. A fact stored with the same words is found by looking up its fingerprint,
  without an embedding call. Nothing is hashed above the notes: no tool needs one value for the
  whole vault, and `sync --dry-run` already says what changed.

  A hash cannot tell that *"how often do I change the oil?"* is about your note *"I change
  the engine oil every 10 000 km"*: the words differ but the meaning is the same. Embeddings
  handle that, so `check` falls back to them.
- **Chunk**: consecutive blocks under one heading, packed up to 1200 characters, and embedded
  together with a short context line (`Note title > Heading`). A chunk is embedded again only when
  its text, its note's title or its headings change. The vectors of deleted text are dropped.
- **Search**: the question is embedded and compared with every stored vector. The scan is exact,
  with no approximation, because a personal vault is small enough for that.
- **Passage id**: `note:first-last`, such as `Car.md:8-8`, names one chunk by its note and line
  range. `topics` lists ids and `read` takes them back. A long block cut into several chunks
  gives each piece the same id, and `read` returns all of them in reading order.

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
chaos-kb topics "book the car in for its next service"
chaos-kb read Car.md:8-8
chaos-kb remember "Blood type: A positive." --note Health --heading Blood
chaos-kb forget "Blood type: A positive." --note Health
chaos-kb dashboard                    # the web page: http://127.0.0.1:7341
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
  "presence": "exact"
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

`topics` returns one line per passage, best first: its `id`, `section`, `score` and `gist`, the
passage's first words (at most 100 characters). There is no text, so the list stays short.
`chaos-kb topics "book the car in for its next service" --limit 3`:

```json
{
  "topics": [
    { "gist": "--- car: Toyota Corolla 2018 ---", "id": "Car.md:1-3", "score": 0.2798360586166382, "section": "" },
    { "gist": "I change the engine oil every 10 000 km.", "id": "Car.md:8-8", "score": 0.2677106559276581, "section": "Maintenance > Oil" },
    { "gist": "Driving licence category B since 2015.", "id": "Licences.md:3-3", "score": 0.13712896406650543, "section": "Licences" }
  ]
}
```

`read` returns the `passages` behind the ids, in the order asked for, with their full `text`. It
embeds nothing. Ids the index no longer holds come back under `missing`: the note changed since
`topics` listed them, so list the topics again. `chaos-kb read Car.md:8-8 Gone.md:1-1`:

```json
{
  "missing": ["Gone.md:1-1"],
  "passages": [
    { "id": "Car.md:8-8", "section": "Maintenance > Oil", "text": "I change the engine oil every 10 000 km." }
  ]
}
```

`sync` reports the notes it `added`, `changed` and `removed`, how many chunks the index holds, how
many it `embedded`. `remember` reports whether it `stored` the fact, or where
the same words already sit (`known_at`). `forget` reports where the blocks it `removed` sat,
and the blocks of the text stored nowhere (`not_found`). Both re-sync the index
before they answer. When the index cannot follow (Ollama is not running, say), they put the
notes back and fail with an error that starts with `nothing was changed:`.

The index lives at `<vault>/.chaos-kb/index.redb`. Obsidian ignores folders whose names start
with a dot. Deleting the folder is safe, because `sync` rebuilds it from the notes. An index
written by a chaos-kb build with a different table layout, older or newer, reads as empty, and
the next `sync` rebuilds it. That re-embeds every note.

### From an agent (MCP)

`chaos-kb mcp` serves the same seven tools over MCP (stdio, newline-delimited JSON-RPC 2.0). For
Claude Code:

```sh
claude mcp add chaos-kb -- chaos-kb --vault ~/Knowledge mcp
```

Other clients take the same command and arguments in their MCP config. When it starts, the
server also tells the agent how to use the seven tools together.

### Dashboard

`chaos-kb dashboard` serves one web page on this machine only, at `http://127.0.0.1:7341`
(`--port` picks another port). Open it in a browser to see what the knowledge base holds and to
change it:

- **Notes.** Every note with its hash and its state: *in the index*, *changed* (edited since the
  last sync), *new* (never synced) or *deleted* (gone from disk, still in the index). The bar at
  the top says whether the index holds every note as it is on disk. Open a note to see its
  blocks: each block's section, lines, words and fingerprint, and whether the index holds it.
  Hashes are shown whole; click one to copy it.
- **Add, replace, remove.** *Add a fact* runs `remember`. *Remove* runs `forget` for that fact's
  words in its note. *Replace* stores the new text under the same heading with `remember`, then
  forgets the old text, so a refused new text leaves the old fact in place. Every change syncs
  the index, and the notes it changed light up. *Activity* lists each change with the notes it
  changed.
- **Edits made elsewhere**, in Obsidian or any editor, show up within four seconds as *changed* or
  *new*. While "Sync edits made outside the dashboard" is ticked (the default), the page syncs
  them on its own; untick it to sync with the button instead. After a failed automatic sync
  (Ollama not running, say), the page waits until the notes change again.
- **Try a question** runs `check` and `topics`, as an agent would, and points at the facts they
  found.

The page calls the same tools as an agent (`POST /api/tools/kb_<tool>`, arguments as a JSON
object, read by the same command-line definition), so the CLI, agents and the dashboard share one
knowledge base and take the same locks. The server answers only requests addressed to
`127.0.0.1:<port>` or `localhost:<port>`, and its tools refuse posts from any other web page, so a
site open in the same browser can neither read nor change your notes. The page loads nothing from
the internet.

## Tools

This reference is generated from the command-line definition. `cargo test` fails when it is out
of date.

<!-- tools:start -->
### `kb_sync` · `chaos-kb sync`

Bring the index up to date with the vault: compare each note's hash with the stored one, embed only new or edited passages, drop deleted ones. Run after editing notes

| arg | type | default | description |
|---|---|---|---|
| `dry_run` | boolean | `false` | Report what would change without embedding or writing anything |

### `kb_remember` · `chaos-kb remember`

Store one fact in the vault as Markdown and index it. Skipped when the same words are already stored anywhere in the vault. When the index cannot take it (the embedder is down, say), the note is put back and the error starts with `nothing was changed`

| arg | type | default | description |
|---|---|---|---|
| `text` (required) | string |  | The fact, in plain words or Markdown. One fact per call |
| `heading` | string |  | Section heading inside the note (added when missing) |
| `note` | string | `Inbox.md` | Note to append to, relative to the vault (`.md` optional) |

### `kb_forget` · `chaos-kb forget`

Take a fact out of the vault and the index: every block with the same words, compared as fingerprints (case, spacing and punctuation aside), never a passage that only means the same. Reports where the removed blocks sat; nothing is removed when no block has these words. When the index cannot follow (the embedder is down, say), the notes are put back and the error starts with `nothing was changed`

| arg | type | default | description |
|---|---|---|---|
| `text` (required) | string |  | The fact in its stored words, as `read` or `search` return them. Each block of it is removed wherever it is stored |
| `note` | string |  | Only remove it from this note (relative to the vault, `.md` optional) |

### `kb_check` · `chaos-kb check`

Is this already in the knowledge base? Exact words are answered from the stored block fingerprints without embedding; otherwise the nearest passages by cosine similarity decide. Returns where the knowledge sits

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

### `kb_topics` · `chaos-kb topics`

What the knowledge base holds near a task, as a short list: one line per passage (id, section, similarity, first words), best first, without the text. Keep the ids that would change how the task is done and fetch them with `read`; when none would, the knowledge base has nothing for this task

| arg | type | default | description |
|---|---|---|---|
| `task` (required) | string |  | The task or request, in the user's words |
| `limit` | integer ≥ 0 | `8` | Maximum passages listed |

### `kb_read` · `chaos-kb read`

The passages behind ids from `topics`, with their text, in the order given. Embeds nothing. Ids the index no longer holds come back as `missing` (the note changed: list the topics again)

| arg | type | default | description |
|---|---|---|---|
| `ids` (required) | list of strings |  | Passage ids from `topics` (`note:first-last`, like `Car.md:8-8`) |

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

## How `topics` and `read` work together

An agent about to do a task for you should know what your notes say that changes the result,
and nothing else: every unneeded passage costs tokens and can mislead. Asking "is there anything
relevant?" with a score cut-off does not work. In the end-to-end test (`tests/e2e.rs`) the right
note came first for each of four tasks, yet every relevant passage scored between 0.15 and 0.36,
below `check`'s 0.38 line, and for "Explain to me how a database index works." the right note
(*"I want a concrete example first"*) scored 0.151 against 0.094 for the next, unrelated one.
Requests phrased as tasks score lower than questions, so no fixed number separates what matters
from what does not.

So the work is split in two steps:

1. `topics` ranks the passages near the task and returns one line each (id, section, score,
   first words). The ranking is good; the judgment is left to the caller.
2. The caller, usually a small, fast model acting as a searcher, keeps the ids that would change
   how the task is done, possibly none, and fetches only those with `read`.

What reaches the main agent is then the few passages that matter, as plain text it can put in
front of the task. `read` looks the ids up in the index, so it embeds nothing and works even
while the embedding model is down.

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
| `topics` (one embedding call) | 0.10 s |
| `read` (no embedding) | 0.03 s |

A fresh index file for that vault is 8.4 MB. The file does not shrink when text is deleted:
redb reuses the freed space for later writes instead.

There is no Bloom filter, because it would save nothing. A Bloom filter quickly answers "these
words are certainly not stored", which only speeds up the lookup. Here the lookup is not the slow
part: a same-words `check` takes 0.04 s on a one-note vault and on the 503-note vault alike. And
"not stored word for word" still has to be checked by meaning, which needs the embedding call
either way.

## Your data

- **The notes are the only copy of your knowledge.** chaos-kb writes to your notes only when you
  call `remember` or `forget` (from the CLI, an agent or the dashboard). `sync`, `check`,
  `search`, `topics` and `read` only read them.
- **How `remember` writes**: with a heading, the fact goes right under that heading's own text,
  before any subheading, so it reads under exactly that heading. Headings are compared like
  fingerprints, so `--heading oil` finds `## Oil:`. A missing heading is added at
  the end of the note. The note is written through a temporary file and a rename, so it is never
  half-written, and it keeps its file permissions. Existing text is never changed.
- **How `forget` removes**: it looks the text up by fingerprint, as `remember` does, so only
  blocks with the same words go, never a passage that only means the same. With `--note`, only
  that note is touched. A removed block takes its whole lines with it, and one blank line, so its
  neighbours keep a single blank line between them. Headings stay, even when their last fact is
  gone. The note is rewritten through a temporary file and a rename, and keeps its permissions.
  `forget` refuses, changing nothing, when the note changed since it was read, or when the rest of
  the note would read differently without the block (an indented paragraph that would join the
  list item above it, for example).
- **A failed sync puts the notes back.** `remember` and `forget` write the note first, then sync
  the index. When that sync fails (Ollama not running, a missing model), each note is put back
  byte for byte, and a note or folder the call created is removed again, so the vault and the
  index still agree. The error starts with `nothing was changed:` and gives the cause. A note
  edited by someone else in the meantime is left as it is, and the error names it instead.
- **What `remember` refuses**: writing outside the vault or through a symlinked folder or note,
  changing a read-only note or one that is not valid UTF-8, and text that would change how the
  rest of the note reads (an unclosed code fence or HTML comment, or a heading that would take
  in the sections after it). A refused call changes no note.
- **Several processes take turns.** CLI commands, a running MCP server and the dashboard can work
  on the same vault: each index operation holds `<index>.lock`, and appends and removals hold
  `<vault>/.chaos-kb/append.lock`. When another process changed the index during a `sync`, the
  `sync` starts over, with at most three attempts in all.
- **Skipped when reading**: hidden files and folders (such as `.obsidian`), symlinks, files that
  are not Markdown (`.md`), and notes whose path contains `:`, `\` or a control character,
  begins with a space, or is not valid UTF-8. Every note is read whole,
  whatever its size; invalid UTF-8 inside a note is replaced, not fatal.
- **Embeddings are local by default** (Ollama on `localhost`). If you point `--embed-url` at a
  cloud API, your note text is sent there to be embedded. The API key is read only from
  `CHAOS_KB_EMBED_API_KEY` and is never printed. Proxy settings from the environment
  (`ALL_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY`) are used for remote embedders only, never for `localhost`.
- **Changing the embedding model** re-embeds everything on the next `sync`, because vectors from
  different models cannot be compared. Until then, `check` still answers `exact` matches and
  `read` still returns passages, while `check` by meaning, `search` and `topics` ask you to run
  `sync`.

## Development

Rules for changing the code, and the commands that validate it, are in [AGENTS.md](AGENTS.md).
A visual tutorial, one page per feature, is in [site/](site/index.html); `cargo test --test
site` fails when it falls out of step with the code. The end-to-end tests in `tests/e2e.rs` run
the real vault, index and Ollama: they skip when nothing listens on `localhost:11434`, and fail
when Ollama runs without `embeddinggemma`.
