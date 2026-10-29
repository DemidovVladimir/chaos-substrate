//! End to end on real parts: a Markdown vault on disk, the redb index file and
//! the local Ollama `embeddinggemma`. The vault is a small personal knowledge
//! base (example data). Every test soft-skips when Ollama is not reachable.

use chaos_kb::adapters::embedder::HttpEmbedder;
use chaos_kb::adapters::obsidian::ObsidianVault;
use chaos_kb::adapters::redb_index::RedbIndex;
use chaos_kb::application::Kb;
use chaos_kb::domain::changes::{Changes, NoteHashes};
use chaos_kb::domain::path::NotePath;
use chaos_kb::domain::presence::{DEFAULT_MIN_SCORE, Presence};
use chaos_kb::ports::Index;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

const OLLAMA: &str = "http://localhost:11434/v1";
const MODEL: &str = "embeddinggemma";

/// Example personal notes: path → Markdown.
const VAULT: &[(&str, &str)] = &[
    (
        "Education.md",
        "# Education\n\n## University\n\n- MSc in Applied Physics, 2014.\n- BSc in Physics, 2012.\n\n\
         ## Courses\n\n- Certified Kubernetes Administrator (CKA), passed in March 2023.\n\
         - Rust programming course, finished in 2024.\n",
    ),
    (
        "Licences.md",
        "# Licences\n\nDriving licence category B since 2015.\n\nMotorcycle licence A2 since 2019.\n",
    ),
    (
        "Car/Maintenance.md",
        "---\ncar: Toyota Corolla 2018\n---\n# Maintenance\n\n## Oil\n\n\
         I change the engine oil and the oil filter every 10 000 km, with 5W-30 synthetic oil.\n\n\
         ## Tyres\n\nWinter tyres go on in November and come off in April.\n",
    ),
    (
        "Communication.md",
        "# How I communicate\n\nI prefer short written messages over phone calls.\n\n\
         When someone explains something to me, I want a concrete example first and the general rule after.\n",
    ),
    (
        "Health/Treatment.md",
        "# Treatment\n\n## Allergies\n\nAllergic to penicillin.\n\n\
         ## Supplements\n\nVitamin D, 2000 IU every morning.\n",
    ),
    (
        "Здоровье/Сон.md",
        "# Сон\n\nЯ ложусь спать в 23:00 и встаю в 7:00.\n",
    ),
];

/// Questions the vault answers, with the note that holds the answer.
const KNOWN: &[(&str, &str)] = &[
    (
        "how often do I change the oil in my car?",
        "Car/Maintenance.md",
    ),
    ("when do I put the winter tyres on?", "Car/Maintenance.md"),
    ("what university degree do I have?", "Education.md"),
    ("which certifications have I passed?", "Education.md"),
    ("do I have a driving licence?", "Licences.md"),
    ("how should things be explained to me?", "Communication.md"),
    ("what am I allergic to?", "Health/Treatment.md"),
    ("во сколько я ложусь спать?", "Здоровье/Сон.md"),
];

/// Questions the vault does not answer, on the topics it covers (car, health,
/// licences, education): the hardest to tell apart.
const NEAR_MISS: &[&str] = &[
    "which company insures my car?",
    "what is my blood pressure?",
    "do I have a pilot licence?",
    "which languages do I speak?",
];

/// Questions about things the vault never mentions.
const UNRELATED: &[&str] = &[
    "what is the name of my dog?",
    "which football team do I support?",
    "what is my favourite pizza topping?",
];

/// Tasks an agent does for the user (not questions), with the note that
/// should change how it does them.
const TASKS: &[(&str, &str)] = &[
    (
        "Prescribe an antibiotic for my sore throat.",
        "Health/Treatment.md",
    ),
    (
        "Book the car in at the garage for its next service.",
        "Car/Maintenance.md",
    ),
    (
        "Explain to me how a database index works.",
        "Communication.md",
    ),
    (
        "Fill in my job application: list my qualifications.",
        "Education.md",
    ),
];

fn ollama_up() -> bool {
    let up = std::net::TcpStream::connect("127.0.0.1:11434").is_ok();
    if !up {
        eprintln!("Ollama is not reachable on localhost:11434; skipping the end-to-end test");
    }
    up
}

fn write_vault(root: &Path) {
    for (path, markdown) in VAULT {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, markdown).unwrap();
    }
    // Obsidian's own folder must never be indexed.
    std::fs::create_dir_all(root.join(".obsidian")).unwrap();
    std::fs::write(
        root.join(".obsidian/notes.md"),
        "# Settings\n\nnot knowledge",
    )
    .unwrap();
}

struct Parts {
    vault: ObsidianVault,
    index: RedbIndex,
    embedder: HttpEmbedder,
}

impl Parts {
    fn new(root: &Path) -> Parts {
        Parts {
            vault: ObsidianVault::new(root).unwrap(),
            index: RedbIndex::new(&root.join(".chaos-kb/index.redb")),
            embedder: HttpEmbedder::new(OLLAMA, MODEL, None),
        }
    }

    fn kb(&self) -> Kb<'_> {
        Kb {
            vault: &self.vault,
            index: &self.index,
            embedder: &self.embedder,
        }
    }

    /// Each note's hash as the index holds it.
    fn hashes(&self) -> NoteHashes {
        self.index.snapshot().unwrap().unwrap().notes
    }
}

#[test]
fn personal_vault_answers_from_the_index() {
    if !ollama_up() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    write_vault(dir.path());
    let parts = Parts::new(dir.path());
    let kb = parts.kb();

    let first = kb.sync(false).unwrap();
    assert_eq!(first.notes, VAULT.len(), "the .obsidian folder is skipped");
    assert_eq!(first.embedded, first.chunks);
    assert_eq!(first.changes.added.len(), VAULT.len());

    // Exact words: answered from the stored fingerprints.
    let exact = kb
        .check("allergic to PENICILLIN", DEFAULT_MIN_SCORE)
        .unwrap();
    assert_eq!(exact.presence, Presence::Exact);
    assert_eq!(exact.matches[0].at.note.as_str(), "Health/Treatment.md");
    assert_eq!(exact.matches[0].at.section, "Treatment > Allergies");

    // Paraphrased questions: answered by similarity, from the right note. The
    // scores are printed first so a threshold change can be judged at a glance.
    let known: Vec<_> = KNOWN
        .iter()
        .map(|(question, note)| {
            let check = kb.check(question, DEFAULT_MIN_SCORE).unwrap();
            let top = kb.search(question, 1, 0.0).unwrap().hits.remove(0);
            (question, note, check, top)
        })
        .collect();
    let unknown: Vec<_> = [("near miss", NEAR_MISS), ("unrelated", UNRELATED)]
        .into_iter()
        .flat_map(|(kind, questions)| questions.iter().map(move |q| (kind, q)))
        .map(|(kind, q)| (kind, q, kb.check(q, DEFAULT_MIN_SCORE).unwrap()))
        .collect();
    for (question, _, _, top) in &known {
        eprintln!("known     {:.3} {question} → {}", top.score, top.at.note);
    }
    for (kind, question, check) in &unknown {
        eprintln!("{kind:<9} {:.3} {question}", check.score.unwrap());
    }
    for (question, note, check, top) in &known {
        assert!(check.known, "{question}: {check:?}");
        assert_eq!(top.at.note.as_str(), **note, "{question}");
    }
    for (_, question, check) in &unknown {
        assert_eq!(check.presence, Presence::Absent, "{question}: {check:?}");
    }

    // Nothing changed: nothing embedded, no note changed.
    let again = kb.sync(false).unwrap();
    assert_eq!(again.embedded, 0);
    assert_eq!(again.changes, Changes::default());
}

#[test]
fn tasks_list_their_topics_and_read_them() {
    if !ollama_up() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    write_vault(dir.path());
    let parts = Parts::new(dir.path());
    let kb = parts.kb();
    kb.sync(false).unwrap();

    for (task, note) in TASKS {
        let topics = kb.topics(task, 8).unwrap().topics;
        let listed: Vec<String> = topics
            .iter()
            .map(|t| format!("{:.3} {}", t.score, t.id))
            .collect();
        eprintln!("{task}\n  {}", listed.join("\n  "));
        let rank = topics
            .iter()
            .position(|t| t.id.starts_with(&format!("{note}:")));
        assert!(
            rank.is_some_and(|r| r < 3),
            "{task}: {note} not in the top 3"
        );
        // The id reads back as the passage, without an embedding.
        let id = topics[rank.unwrap()].id.clone();
        let read = kb.read(std::slice::from_ref(&id)).unwrap();
        assert!(read.missing.is_empty());
        assert_eq!(read.passages[0].id, id);
        let gist = topics[rank.unwrap()].gist.trim_end_matches('…');
        assert!(
            read.passages[0]
                .text
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .starts_with(gist)
        );
    }
}

#[test]
fn remember_edit_and_delete_round_trip() {
    if !ollama_up() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    write_vault(dir.path());
    let parts = Parts::new(dir.path());
    let kb = parts.kb();
    kb.sync(false).unwrap();

    // A new fact lands in the vault as Markdown and is found right away.
    let inbox = NotePath::parse("Inbox").unwrap();
    let stored = kb
        .remember("My blood type is A positive.", &inbox, Some("Health"))
        .unwrap();
    assert!(stored.stored);
    let markdown = std::fs::read_to_string(dir.path().join("Inbox.md")).unwrap();
    assert!(markdown.contains("## Health") && markdown.contains("My blood type is A positive."));
    let found = kb
        .check("what is my blood type?", DEFAULT_MIN_SCORE)
        .unwrap();
    assert!(found.known, "{found:?}");
    assert_eq!(found.matches[0].at.note.as_str(), "Inbox.md");

    // The same words again: refused, nothing written.
    let twice = kb
        .remember("my blood type is a positive", &inbox, None)
        .unwrap();
    assert!(!twice.stored);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("Inbox.md")).unwrap(),
        markdown
    );

    // A correction differs by one symbol: it is a new fact, not a repeat.
    let plus = kb.remember("Rhesus factor: A+", &inbox, None).unwrap();
    let after_plus = parts.hashes();
    let minus = kb.remember("Rhesus factor: A-", &inbox, None).unwrap();
    assert!(plus.stored && minus.stored, "{minus:?}");
    assert_ne!(parts.hashes(), after_plus);

    // Markdown is read like a note: a list item with a link is stored once.
    let fact = "- My GP is [Dr. Smith](https://clinic.example/smith).";
    assert!(kb.remember(fact, &inbox, None).unwrap().stored);
    let again = kb.remember(fact, &inbox, None).unwrap();
    assert!(!again.stored, "{again:?}");
    let inbox_text = std::fs::read_to_string(dir.path().join("Inbox.md")).unwrap();
    assert_eq!(inbox_text.matches("Dr. Smith").count(), 1);

    // Formatting-only edit: same knowledge, same note hashes.
    let licences = dir.path().join("Licences.md");
    kb.sync(false).unwrap();
    let before = parts.hashes();
    std::fs::write(
        &licences,
        "# Licences\n\n**Driving licence** category B since 2015.\n\nMotorcycle licence A2 since 2019.\n",
    )
    .unwrap();
    let formatted = kb.sync(false).unwrap();
    assert_eq!(
        formatted.changes,
        Changes::default(),
        "fingerprints ignore Markdown symbols"
    );
    assert_eq!(parts.hashes(), before);
    assert_eq!(formatted.embedded, 1, "the passage text changed");

    // A real edit: one note changed, one chunk embedded.
    std::fs::write(
        &licences,
        "# Licences\n\nDriving licence category B since 2015.\n\nMotorcycle licence A since 2021.\n",
    )
    .unwrap();
    let edited = kb.sync(false).unwrap();
    assert_eq!(edited.changes.changed, ["Licences.md"]);
    assert_eq!(edited.embedded, 1);
    assert_ne!(parts.hashes(), before);

    // Deleting a note removes its knowledge from the index.
    std::fs::remove_file(dir.path().join("Health/Treatment.md")).unwrap();
    let purged = kb.sync(false).unwrap();
    assert_eq!(purged.changes.removed, ["Health/Treatment.md"]);
    let gone = kb
        .check("allergic to penicillin", DEFAULT_MIN_SCORE)
        .unwrap();
    assert_ne!(gone.presence, Presence::Exact);
    assert!(
        gone.matches
            .iter()
            .all(|m| m.at.note.as_str() != "Health/Treatment.md")
    );
}

#[test]
fn forget_takes_a_fact_out_and_the_note_back() {
    if !ollama_up() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    write_vault(dir.path());
    let parts = Parts::new(dir.path());
    let kb = parts.kb();
    kb.sync(false).unwrap();
    let first = parts.hashes();
    let licences = dir.path().join("Licences.md");
    let before = std::fs::read_to_string(&licences).unwrap();

    // Remembered, then forgotten in other formatting: the note reads as it
    // did, byte for byte, so every note hash is the first one again.
    let note = NotePath::parse("Licences").unwrap();
    let added = kb
        .remember("Boat licence since 2022.", &note, None)
        .unwrap();
    assert!(added.stored);
    assert_ne!(parts.hashes(), first);
    let gone = kb.forget("**boat licence** since 2022", None).unwrap();
    assert_eq!(gone.removed.len(), 1, "{gone:?}");
    assert_eq!(gone.removed[0].note.as_str(), "Licences.md");
    assert_eq!(std::fs::read_to_string(&licences).unwrap(), before);
    assert_eq!(parts.hashes(), first);
    let asked = kb
        .check("Boat licence since 2022.", DEFAULT_MIN_SCORE)
        .unwrap();
    assert_ne!(asked.presence, Presence::Exact);

    // Out of a section: the heading and the other facts stay.
    let car = dir.path().join("Car/Maintenance.md");
    let tyres = "Winter tyres go on in November and come off in April.";
    let out = kb
        .forget(tyres, Some(&NotePath::parse("Car/Maintenance").unwrap()))
        .unwrap();
    assert_eq!(out.removed[0].section, "Maintenance > Tyres");
    let markdown = std::fs::read_to_string(&car).unwrap();
    assert!(!markdown.contains("Winter tyres"), "{markdown}");
    assert!(
        markdown.contains("## Tyres") && markdown.contains("10 000 km"),
        "{markdown}"
    );
    let after_out = parts.hashes();
    assert_ne!(after_out, first);
    assert_ne!(
        kb.check(tyres, DEFAULT_MIN_SCORE).unwrap().presence,
        Presence::Exact
    );

    // Words stored nowhere: nothing changes.
    let none = kb.forget("I own a boat.", None).unwrap();
    assert!(none.removed.is_empty());
    assert_eq!(parts.hashes(), after_out);
}

/// With the embedder out of reach, `remember` and `forget` change nothing:
/// each note is put back, byte for byte, and the index still agrees with the
/// vault.
#[test]
fn an_unreachable_embedder_changes_nothing() {
    if !ollama_up() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    write_vault(dir.path());
    let parts = Parts::new(dir.path());
    parts.kb().sync(false).unwrap();
    let first = parts.hashes();
    let licences = dir.path().join("Licences.md");
    let before = std::fs::read_to_string(&licences).unwrap();

    let down = Parts {
        embedder: HttpEmbedder::new("http://127.0.0.1:9/v1", MODEL, None),
        ..Parts::new(dir.path())
    };
    let kb = down.kb();
    let refused = |err: String| {
        let cause = "nothing was changed: embeddings request to http://127.0.0.1:9/v1/embeddings";
        assert!(err.starts_with(cause), "{err}");
    };
    let licence = NotePath::parse("Licences").unwrap();
    refused(format!(
        "{:#}",
        kb.remember("Boat licence since 2022.", &licence, None)
            .unwrap_err()
    ));
    let travel = NotePath::parse("Documents/Travel").unwrap();
    refused(format!(
        "{:#}",
        kb.remember("Passport renewed in 2024.", &travel, None)
            .unwrap_err()
    ));
    refused(format!(
        "{:#}",
        kb.forget("Motorcycle licence A2 since 2019.", None)
            .unwrap_err()
    ));

    assert_eq!(std::fs::read_to_string(&licences).unwrap(), before);
    assert!(
        !dir.path().join("Documents").exists(),
        "no new folder stays"
    );
    assert_eq!(parts.hashes(), first);
    assert_eq!(parts.kb().sync(true).unwrap().changes, Changes::default());
}

/// Drive the real binary as an agent would: CLI sync, then an MCP session.
#[test]
fn binary_serves_cli_and_mcp() {
    if !ollama_up() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    write_vault(dir.path());
    let bin = env!("CARGO_BIN_EXE_chaos-kb");
    let vault = dir.path().to_str().unwrap();

    let out = Command::new(bin)
        .args(["--vault", vault, "sync"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let sync: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(sync["notes"], VAULT.len());

    let mut child = Command::new(bin)
        .args(["--vault", vault, "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    // A request gets one reply line; a notification (no id) gets none.
    let mut send = |message: Value| -> Option<Value> {
        writeln!(stdin, "{message}").unwrap();
        message.get("id")?;
        Some(serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap())
    };

    let init = send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "e2e", "version": "0"}}}))
    .unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "chaos-kb");
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    assert_eq!(send(initialized), None);
    let pong = send(json!({"jsonrpc": "2.0", "id": 9, "method": "ping"})).unwrap();
    assert_eq!(pong, json!({"jsonrpc": "2.0", "id": 9, "result": {}}));

    let list = send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).unwrap();
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "kb_sync",
            "kb_remember",
            "kb_forget",
            "kb_check",
            "kb_search",
            "kb_topics",
            "kb_read",
            "kb_instances"
        ]
    );

    let check = send(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "kb_check", "arguments": {"query": "do I have a driving licence?"}}}))
    .unwrap();
    assert_eq!(check["result"]["isError"], false, "{check}");
    let body: Value =
        serde_json::from_str(check["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["known"], true, "{body}");
    assert_eq!(body["matches"][0]["note"], "Licences.md");

    // topics, then read the ids it listed, given as a JSON array.
    let body = |reply: &Value| -> Value {
        assert_eq!(reply["result"]["isError"], false, "{reply}");
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };
    let topics = body(&send(json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
        "params": {"name": "kb_topics", "arguments": {"task": "Prescribe an antibiotic for me.", "limit": 3}}}))
    .unwrap());
    let ids: Vec<Value> = topics["topics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].clone())
        .collect();
    assert_eq!(ids.len(), 3, "{topics}");
    assert!(
        topics["topics"][0].get("text").is_none(),
        "topics carry no text"
    );
    let read = body(
        &send(json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call",
        "params": {"name": "kb_read", "arguments": {"ids": ids}}}))
        .unwrap(),
    );
    let read_ids: Vec<Value> = read["passages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].clone())
        .collect();
    assert_eq!(read_ids, ids, "in the order asked: {read}");
    assert!(
        read["passages"].to_string().contains("penicillin"),
        "{read}"
    );

    let bad = send(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "kb_search", "arguments": {"query": "x", "limit": "many"}}}))
    .unwrap();
    assert_eq!(bad["result"]["isError"], true, "{bad}");

    drop(stdin);
    assert!(child.wait().unwrap().success());
}

/// Two knowledge bases behind one binary: `--instance` on the CLI and
/// `instance` in an MCP call pick one, and every report names the one that
/// answered.
#[test]
fn binary_serves_several_instances() {
    if !ollama_up() {
        return;
    }
    let (personal, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write_vault(personal.path());
    let office = "# Office\n\nThe office opens at 8:30.\n";
    std::fs::write(work.path().join("Office.md"), office).unwrap();
    let bin = env!("CARGO_BIN_EXE_chaos-kb");
    let vaults = [
        "--vault".to_string(),
        format!("personal={}", personal.path().display()),
        "--vault".to_string(),
        format!("work={}", work.path().display()),
    ];
    let cli = |args: &[&str]| -> Value {
        let out = Command::new(bin).args(&vaults).args(args).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let synced = cli(&["sync"]);
    assert_eq!(synced["instance"], "personal", "{synced}");
    assert_eq!(synced["notes"], VAULT.len());
    let synced = cli(&["--instance", "work", "sync"]);
    assert_eq!(synced["instance"], "work", "{synced}");
    assert_eq!(synced["notes"], 1);
    assert!(work.path().join(".chaos-kb/index.redb").is_file());

    let mut child = Command::new(bin)
        .args(&vaults)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut call = |id: u64, name: &str, arguments: Value| -> Value {
        let message = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}});
        writeln!(stdin, "{message}").unwrap();
        let reply: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
        assert_eq!(reply["result"]["isError"], false, "{reply}");
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };

    let listed = call(1, "kb_instances", json!({}));
    let rows: Vec<(&Value, &Value)> = listed["instances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| (&i["name"], &i["default"]))
        .collect();
    assert_eq!(
        rows,
        [
            (&json!("personal"), &json!(true)),
            (&json!("work"), &json!(false))
        ]
    );

    let fact = "The office opens at 8:30.";
    let at_work = call(2, "kb_check", json!({"query": fact, "instance": "work"}));
    assert_eq!(at_work["instance"], "work", "{at_work}");
    assert_eq!(at_work["presence"], "exact", "{at_work}");
    assert_eq!(at_work["matches"][0]["note"], "Office.md");
    let at_home = call(3, "kb_check", json!({"query": fact}));
    assert_eq!(at_home["instance"], "personal", "{at_home}");
    assert_ne!(at_home["presence"], "exact", "{at_home}");

    drop(stdin);
    assert!(child.wait().unwrap().success());
}
