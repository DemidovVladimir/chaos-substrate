//! The index as one redb file (pure-Rust, ACID). The database is opened per
//! operation and closed right after, so a CLI call and a running MCP server
//! can share the file; vectors are stored as little-endian `f32` bytes.

use crate::domain::hash::Digest;
use crate::domain::note::Location;
use crate::ports::{Hit, Index, Snapshot, Update};
use anyhow::Result;
use redb::{MultimapTableDefinition, TableDefinition};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long an operation waits for another handle to close the file.
const LOCK_PATIENCE: Duration = Duration::from_secs(3);

/// Where a block or chunk sits: note, section, first line, last line.
type LocRow<'a> = (&'a str, &'a str, u32, u32);
/// A chunk: its location, context and text.
type ChunkRow<'a> = (LocRow<'a>, &'a str, &'a str);

/// `"embedder"` → the embedder id of the stored vectors.
const META: TableDefinition<&str, &str> = TableDefinition::new("meta");
/// Merkle tree: vault path → hash.
const TREE: TableDefinition<&str, &[u8; 32]> = TableDefinition::new("tree");
/// Chunk key → chunk.
const CHUNKS: TableDefinition<&[u8; 32], ChunkRow<'static>> = TableDefinition::new("chunks");
/// Chunk key → unit vector as little-endian `f32` bytes.
const VECTORS: TableDefinition<&[u8; 32], &[u8]> = TableDefinition::new("vectors");
/// Block fingerprint → every location of that block.
const BLOCKS: MultimapTableDefinition<&[u8; 32], LocRow<'static>> =
    MultimapTableDefinition::new("blocks");

pub struct RedbIndex {
    path: PathBuf,
    /// How long an operation waits for another handle to close the file.
    patience: Duration,
}

impl RedbIndex {
    /// An index stored at `path` (created with its parent folder on first write).
    pub fn new(path: &Path) -> RedbIndex {
        RedbIndex {
            path: path.to_path_buf(),
            patience: LOCK_PATIENCE,
        }
    }
}

impl Index for RedbIndex {
    fn snapshot(&self) -> Result<Option<Snapshot>> {
        let _ = (&self.path, self.patience);
        todo!()
    }

    fn vector_keys(&self) -> Result<HashSet<Digest>> {
        todo!()
    }

    fn replace(&self, update: Update) -> Result<()> {
        let _ = update;
        todo!()
    }

    fn exact(&self, fingerprint: &Digest) -> Result<Vec<Location>> {
        let _ = fingerprint;
        todo!()
    }

    fn nearest(&self, query: &[f32], k: usize) -> Result<Vec<Hit>> {
        let _ = (query, k);
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::chunk::Chunk;
    use crate::domain::hash;
    use crate::domain::merkle::Tree;
    use crate::domain::path::NotePath;
    use redb::Database;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;
    use tempfile::tempdir;

    const EMBEDDER: &str = "test-embedder@3d";
    const VA: [f32; 3] = [1.0, 0.0, 0.0];
    const VB: [f32; 3] = [0.8, 0.6, 0.0];
    const VC: [f32; 3] = [0.0, 0.0, 1.0];

    fn loc(note: &str, section: &str, lines: [u32; 2]) -> Location {
        Location {
            note: NotePath::parse(note).unwrap(),
            section: section.to_string(),
            lines,
        }
    }

    fn chunk(note: &str, section: &str, lines: [u32; 2], text: &str) -> Chunk {
        let at = loc(note, section, lines);
        let context = format!("{} > {section}", at.note.title());
        Chunk::new(at, context, text.to_string())
    }

    fn a() -> Chunk {
        chunk(
            "Health/Meds.md",
            "Morning",
            [3, 4],
            "Take vitamin D with breakfast.",
        )
    }

    fn b() -> Chunk {
        chunk(
            "Health/Meds.md",
            "Evening",
            [8, 8],
            "Magnesium before sleep.",
        )
    }

    fn c() -> Chunk {
        chunk("Car.md", "Oil", [2, 5], "Change the oil every 10000 km.")
    }

    fn tree(paths: &[&str]) -> Tree {
        Tree::from_nodes(
            paths
                .iter()
                .map(|p| (p.to_string(), hash::leaf(p.as_bytes()))),
        )
    }

    /// Fingerprint of a chunk's only block, as the fixtures store it.
    fn fp(chunk: &Chunk) -> Digest {
        hash::leaf(chunk.text.as_bytes())
    }

    fn update(chunks: Vec<Chunk>, vectors: &[(Digest, &[f32])]) -> Update {
        Update {
            embedder: EMBEDDER.to_string(),
            tree: tree(&["", "Health/", "Health/Meds.md", "Car.md"]),
            blocks: chunks.iter().map(|c| (fp(c), c.at.clone())).collect(),
            chunks,
            vectors: vectors.iter().map(|(k, v)| (*k, v.to_vec())).collect(),
        }
    }

    /// Chunks a, b, c with vectors along VA, VB, VC.
    fn full() -> Update {
        update(
            vec![a(), b(), c()],
            &[(a().key, &VA), (b().key, &VB), (c().key, &VC)],
        )
    }

    fn chunks(hits: &[Hit]) -> Vec<Chunk> {
        hits.iter().map(|h| h.chunk.clone()).collect()
    }

    fn close(x: f32, y: f32) -> bool {
        (x - y).abs() < 1e-6
    }

    /// Everything the API can tell about the index.
    fn observe(
        index: &RedbIndex,
    ) -> (
        Option<Snapshot>,
        Vec<Digest>,
        Vec<Vec<Location>>,
        Vec<Vec<Hit>>,
    ) {
        let mut keys: Vec<Digest> = index.vector_keys().unwrap().into_iter().collect();
        keys.sort();
        let exact = [a(), b(), c()]
            .iter()
            .map(|c| index.exact(&fp(c)).unwrap())
            .collect();
        let nearest = [VA, VB, VC]
            .iter()
            .map(|q| index.nearest(q, 10).unwrap())
            .collect();
        (index.snapshot().unwrap(), keys, exact, nearest)
    }

    #[test]
    fn round_trip_returns_what_replace_stored() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("deeper").join("index.redb");
        let index = RedbIndex::new(&path);

        index.replace(full()).unwrap();

        assert!(
            path.is_file(),
            "replace creates the parent folders and the file"
        );
        assert_eq!(
            index.snapshot().unwrap(),
            Some(Snapshot {
                embedder: EMBEDDER.to_string(),
                tree: full().tree,
            })
        );
        assert_eq!(
            index.vector_keys().unwrap(),
            HashSet::from([a().key, b().key, c().key])
        );
        assert_eq!(index.exact(&fp(&a())).unwrap(), vec![a().at]);
        assert_eq!(index.exact(&fp(&c())).unwrap(), vec![c().at]);
        let hits = index.nearest(&VA, 2).unwrap();
        assert_eq!(chunks(&hits), vec![a(), b()]);
        assert!(close(hits[0].score, 1.0), "{hits:?}");
        assert!(close(hits[1].score, 0.8), "{hits:?}");
    }

    #[test]
    fn state_persists_across_instances_and_each_operation_releases_the_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("index.redb");
        RedbIndex::new(&path).replace(full()).unwrap();
        // The operation closed the file: another handle opens it at once.
        drop(Database::open(&path).unwrap());

        let fresh = RedbIndex::new(&path);
        assert_eq!(fresh.snapshot().unwrap().unwrap().tree, full().tree);
        drop(Database::open(&path).unwrap());
        assert_eq!(fresh.vector_keys().unwrap().len(), 3);
        assert_eq!(fresh.exact(&fp(&b())).unwrap(), vec![b().at]);
        assert_eq!(chunks(&fresh.nearest(&VC, 1).unwrap()), vec![c()]);
        drop(Database::open(&path).unwrap());
    }

    #[test]
    fn replace_drops_unused_vectors_and_keeps_reused_ones() {
        let dir = tempdir().unwrap();
        let index = RedbIndex::new(&dir.path().join("index.redb"));
        index
            .replace(update(vec![a(), b()], &[(a().key, &VA), (b().key, &VB)]))
            .unwrap();

        // `a` is reused without resupplying its vector; `b` is gone.
        index
            .replace(update(vec![a(), c()], &[(c().key, &VC)]))
            .unwrap();

        assert_eq!(
            index.vector_keys().unwrap(),
            HashSet::from([a().key, c().key])
        );
        let hits = index.nearest(&VA, 3).unwrap();
        assert_eq!(chunks(&hits), vec![a(), c()], "b's chunk is gone too");
        assert!(close(hits[0].score, 1.0), "a keeps its first vector");
        assert!(
            index.exact(&fp(&b())).unwrap().is_empty(),
            "blocks are rewritten"
        );
    }

    #[test]
    fn resupplied_vector_overwrites_the_stored_one() {
        let dir = tempdir().unwrap();
        let index = RedbIndex::new(&dir.path().join("index.redb"));
        index.replace(update(vec![a()], &[(a().key, &VA)])).unwrap();

        index.replace(update(vec![a()], &[(a().key, &VC)])).unwrap();

        let hits = index.nearest(&VC, 1).unwrap();
        assert_eq!(chunks(&hits), vec![a()]);
        assert!(close(hits[0].score, 1.0), "{hits:?}");
    }

    #[test]
    fn replace_with_a_missing_vector_fails_and_changes_nothing() {
        let dir = tempdir().unwrap();
        let index = RedbIndex::new(&dir.path().join("index.redb"));
        let orphan = chunk("Inbox.md", "", [1, 1], "A fact nobody embedded.");

        // On a fresh file: an error, and still nothing stored.
        assert!(index.replace(update(vec![orphan.clone()], &[])).is_err());
        assert_eq!(index.snapshot().unwrap(), None);
        assert!(index.vector_keys().unwrap().is_empty());

        index.replace(full()).unwrap();
        let before = observe(&index);
        let mut bad = update(vec![a(), orphan.clone()], &[(a().key, &VB)]);
        bad.embedder = "another-embedder".to_string();
        bad.tree = tree(&["", "Inbox.md"]);

        let err = index.replace(bad).unwrap_err();

        assert!(err.to_string().contains(&orphan.key.to_hex()), "{err:#}");
        assert_eq!(observe(&index), before);
    }

    #[test]
    fn nearest_ranks_best_first_and_respects_k() {
        let dir = tempdir().unwrap();
        let index = RedbIndex::new(&dir.path().join("index.redb"));
        index.replace(full()).unwrap();

        let all = index.nearest(&VB, 10).unwrap();
        assert_eq!(chunks(&all), vec![b(), a(), c()], "k above n returns all n");
        assert!(close(all[0].score, 1.0) && close(all[1].score, 0.8) && close(all[2].score, 0.0));
        assert_eq!(chunks(&index.nearest(&VB, 1).unwrap()), vec![b()]);
        assert!(index.nearest(&VB, 0).unwrap().is_empty());
    }

    #[test]
    fn nearest_rejects_a_query_of_another_dimension() {
        let dir = tempdir().unwrap();
        let index = RedbIndex::new(&dir.path().join("index.redb"));
        index.replace(full()).unwrap();

        let err = index.nearest(&[1.0, 0.0], 2).unwrap_err();

        assert!(err.to_string().contains("dimension"), "{err:#}");
    }

    #[test]
    fn nearest_on_an_empty_index_is_empty() {
        let dir = tempdir().unwrap();
        let index = RedbIndex::new(&dir.path().join("index.redb"));
        index.replace(update(Vec::new(), &[])).unwrap();

        assert!(index.snapshot().unwrap().is_some());
        assert!(index.nearest(&VA, 5).unwrap().is_empty());
        assert!(
            index.nearest(&[1.0], 5).unwrap().is_empty(),
            "no stored length to mismatch"
        );
    }

    #[test]
    fn exact_returns_every_location_sorted() {
        let dir = tempdir().unwrap();
        let index = RedbIndex::new(&dir.path().join("index.redb"));
        let fact = hash::leaf(b"vitamin d");
        let other = hash::leaf(b"oil");
        let mut up = full();
        up.blocks = vec![
            (fact, loc("Health/Meds.md", "Morning", [10, 10])),
            (other, loc("Car.md", "Oil", [2, 2])),
            (fact, loc("Archive/2025.md", "", [4, 4])),
            (fact, loc("Health/Meds.md", "Morning", [9, 9])),
            (fact, loc("Health/Meds.md", "Evening", [20, 20])),
        ];
        index.replace(up).unwrap();

        assert_eq!(
            index.exact(&fact).unwrap(),
            vec![
                loc("Archive/2025.md", "", [4, 4]),
                loc("Health/Meds.md", "Evening", [20, 20]),
                loc("Health/Meds.md", "Morning", [9, 9]),
                loc("Health/Meds.md", "Morning", [10, 10]),
            ]
        );
        assert_eq!(
            index.exact(&other).unwrap(),
            vec![loc("Car.md", "Oil", [2, 2])]
        );
        assert!(index.exact(&hash::leaf(b"unknown")).unwrap().is_empty());
    }

    #[test]
    fn reads_of_a_missing_file_are_empty_and_create_nothing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("absent").join("index.redb");
        let index = RedbIndex::new(&path);

        assert_eq!(index.snapshot().unwrap(), None);
        assert!(index.vector_keys().unwrap().is_empty());
        assert!(index.exact(&fp(&a())).unwrap().is_empty());
        assert!(index.nearest(&VA, 3).unwrap().is_empty());
        assert!(!path.parent().unwrap().exists(), "no folder, no file");
    }

    #[test]
    fn snapshot_waits_for_another_handle_to_close_the_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("index.redb");
        let index = RedbIndex::new(&path);
        index.replace(full()).unwrap();
        let (opened, wait_opened) = mpsc::channel();
        let holder = {
            let path = path.clone();
            thread::spawn(move || {
                let db = Database::open(&path).unwrap();
                opened.send(()).unwrap();
                thread::sleep(Duration::from_millis(200));
                drop(db);
            })
        };
        wait_opened.recv().unwrap();

        let started = Instant::now();
        let snapshot = index.snapshot().unwrap();

        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "the file was held, so snapshot had to wait: {:?}",
            started.elapsed()
        );
        assert_eq!(snapshot.map(|s| s.tree), Some(full().tree));
        holder.join().unwrap();
    }

    #[test]
    fn gives_up_on_a_held_file_with_a_message_naming_it() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("index.redb");
        RedbIndex::new(&path).replace(full()).unwrap();
        let impatient = RedbIndex {
            path: path.clone(),
            patience: Duration::from_millis(50),
        };
        let held = Database::open(&path).unwrap();

        let read = impatient.snapshot().unwrap_err();
        let write = impatient.replace(full()).unwrap_err();

        let name = path.display().to_string();
        assert!(read.to_string().contains(&name), "{read:#}");
        assert!(write.to_string().contains(&name), "{write:#}");
        drop(held);
        assert!(impatient.snapshot().unwrap().is_some());
    }

    #[test]
    fn undecodable_rows_are_errors_not_panics() {
        let dir = tempdir().unwrap();
        let fact = hash::leaf(b"fact");

        // A block outside the vault and a vector of 5 bytes.
        let broken = dir.path().join("broken.redb");
        let db = Database::create(&broken).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_multimap_table(BLOCKS)
            .unwrap()
            .insert(&fact.0, ("../escape.md", "", 1, 1))
            .unwrap();
        txn.open_table(VECTORS)
            .unwrap()
            .insert(&[7u8; 32], &[0u8; 5][..])
            .unwrap();
        txn.commit().unwrap();
        drop(db);
        let index = RedbIndex::new(&broken);
        assert!(index.exact(&fact).is_err());
        assert!(index.nearest(&[1.0], 1).is_err());

        // A chunk row stored under a key that is not its content's.
        let swapped = dir.path().join("swapped.redb");
        let (a, c) = (a(), c());
        let bytes: Vec<u8> = VA.iter().flat_map(|x| x.to_le_bytes()).collect();
        let db = Database::create(&swapped).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_table(CHUNKS)
            .unwrap()
            .insert(
                &a.key.0,
                (
                    (
                        c.at.note.as_str(),
                        c.at.section.as_str(),
                        c.at.lines[0],
                        c.at.lines[1],
                    ),
                    c.context.as_str(),
                    c.text.as_str(),
                ),
            )
            .unwrap();
        txn.open_table(VECTORS)
            .unwrap()
            .insert(&a.key.0, bytes.as_slice())
            .unwrap();
        txn.commit().unwrap();
        drop(db);
        assert!(RedbIndex::new(&swapped).nearest(&VA, 1).is_err());
    }
}
