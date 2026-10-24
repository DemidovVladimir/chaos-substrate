//! Port implementations: the Obsidian vault on disk, the redb index file and
//! an OpenAI-compatible embeddings endpoint (Ollama by default).

pub mod embedder;
pub mod obsidian;
pub mod redb_index;
