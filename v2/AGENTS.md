# chaos-kb — rules for changing the code

chaos-kb (Chaos Substrate v2) is a personal knowledge base for agents: Markdown notes in, instant
"is this known?" checks and similarity search out. What it does and how to use it is in
[README.md](README.md); this file holds only the rules for working on it.

## Rules

- **Layers** (enforced by the `layering` test in `src/lib.rs`): `domain` is pure and depends on
  nothing in the crate; `ports` on `domain`; `application` on `domain` + `ports`; `adapters`
  implement ports and never use `application` or `entry`; `entry` wires adapters into the
  application.
- **Test first.** Write the failing test, run it and see it fail for the right reason, then
  implement until it passes. Unit tests sit next to the code; `tests/e2e.rs` runs the real vault,
  index and local Ollama.
- **No fake vectors outside `#[cfg(test)]`.** When the embedder fails, `sync` fails.
- **One source per fact.** The clap definitions in `src/entry/cli.rs` are the only definition of
  the tools, their arguments, defaults and descriptions. MCP schemas and the README tool block are
  generated from them, and `cargo test` fails when the README block drifts (the failure prints the
  expected block). User docs live in README.md only.
- **The vault is the only copy of the knowledge.** The index is derived and safe to delete.
- No `unsafe` (forbidden in `Cargo.toml`). Rust only. Never print, log or commit secrets; the
  embeddings API key is read only from `CHAOS_KB_EMBED_API_KEY`.

## When behaviour changes

In the same commit: update README.md, and every site page whose `sources` in `site/sources.toml`
list a changed file (bump its footer date).

## Validate

```sh
cd v2
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
