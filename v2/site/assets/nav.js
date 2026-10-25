/* The page list of the chaos-kb field manual: one entry per feature page.
   Order = reading order (drives the rail, the index cards and prev / next).
   A new page = one entry here + one [pages.<slug>] block in ../sources.toml
   (tests/site.rs checks that the two agree and that every page file exists).
   site.js and site.css are shared verbatim with the Tengu field manual. */
window.TUTORIAL = {
  chapters: [
    { id: "start", kanji: "始", name: "Start here", blurb: "What chaos-kb is, and how a folder of notes becomes quick answers." },
    { id: "answer", kanji: "答", name: "Answers", blurb: "Same words by fingerprint, same meaning by cosine, and the line between known and not known." },
    { id: "store", kanji: "蔵", name: "Storage", blurb: "Your notes are the knowledge; the index file is derived from them and safe to delete; the dashboard shows both." },
    { id: "agents", kanji: "繋", name: "Agents", blurb: "The seven tools over MCP, generated from one command-line definition." }
  ],
  pages: [
    { slug: "chaos-kb", chapter: "start", title: "Install and run", blurb: "Build one binary, pull one local model, point it at your notes and connect it to Claude Code or Codex." },
    { slug: "big-picture", chapter: "start", title: "From notes to answers", blurb: "Notes become blocks, blocks become a Merkle tree and chunks, chunks become vectors in one index file." },

    { slug: "merkle", chapter: "answer", title: "Merkle tree and exact check", blurb: "Fingerprints roll up to one root hash: the same words are found in one lookup, and one hash says nothing changed." },
    { slug: "search", chapter: "answer", title: "Chunks, embeddings and search", blurb: "Passages of at most 1200 characters, embedded once, ranked by an exact cosine scan." },
    { slug: "check", chapter: "answer", title: "How check decides", blurb: "Exact words first; otherwise the best of three passages against a line of 0.38." },
    { slug: "topics", chapter: "answer", title: "Topics and read", blurb: "Short ranked lines for a task, a searcher that keeps what matters, and only those passages read back." },

    { slug: "vault", chapter: "store", title: "The vault", blurb: "Obsidian notes parsed into blocks; remember adds one fact under its heading, forget takes one out by its words." },
    { slug: "index-file", chapter: "store", title: "The index file", blurb: "One redb file, replaced in one transaction, shared by the CLI, the MCP server and the dashboard." },
    { slug: "dashboard", chapter: "store", title: "The dashboard", blurb: "One local web page: your notes next to the index as a Merkle tree, with add, replace and remove, re-synced after every change." },

    { slug: "mcp", chapter: "agents", title: "Agents and MCP", blurb: "A stdio server whose tools, schemas and README reference all come from the clap definition." }
  ]
};

/* site.js draws the rail with the Tengu mark; give it chaos-kb's own. Deferred
   scripts run before DOMContentLoaded, so the rail exists by then. */
document.addEventListener("DOMContentLoaded", function () {
  var set = function (sel, text) {
    Array.prototype.forEach.call(document.querySelectorAll(sel), function (e) { e.textContent = text; });
  };
  set(".brand-mark", "知");
  set(".brand-name", "chaos-kb");
  set(".brand-sub", "Field manual");
});
