//! The index as one redb file (pure-Rust, ACID). The database is opened per
//! operation and closed right after, under a lock file, so a CLI call and a
//! running MCP server take turns on it; vectors are stored as little-endian
//! `f32` bytes.

use crate::domain::changes::NoteHashes;
use crate::domain::chunk::Chunk;
use crate::domain::hash::Digest;
use crate::domain::note::Location;
use crate::domain::path::NotePath;
use crate::domain::vector;
use crate::ports::{Hit, Index, Snapshot, Stale, Update};
use anyhow::{Context, Result, bail, ensure};
use redb::{
    Database, DatabaseError, Key, MultimapTableDefinition, ReadTransaction, ReadableDatabase,
    ReadableMultimapTable, ReadableTable, Table, TableDefinition, TableError, Value,
    WriteTransaction,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::{fs, io};

/// The [`META`] key of the embedder id.
const EMBEDDER_KEY: &str = "embedder";
/// The [`META`] key of the table layout, and this build's layout. Bumped
/// whenever a table changes shape: an index of another layout reads as empty,
/// so the next sync rebuilds it.
const FORMAT_KEY: &str = "format";
const FORMAT: &str = "2";

/// Where a block or chunk sits: note, section, first line, last line.
type LocRow<'a> = (&'a str, &'a str, u32, u32);
/// A chunk: its location, its position in the update (which orders the chunks
/// that share a location: the pieces of a cut block), context and text.
type ChunkRow<'a> = (LocRow<'a>, u32, &'a str, &'a str);

/// `"format"` → the table layout; `"embedder"` → the embedder id of the
/// stored vectors.
const META: TableDefinition<&str, &str> = TableDefinition::new("meta");
/// Note path → note hash. The table keeps its old on-disk name `tree`, so
/// index files written before keep opening.
const NOTES: TableDefinition<&str, &[u8; 32]> = TableDefinition::new("tree");
/// Chunk key → every chunk with that content (the same passage can sit in
/// several notes).
const CHUNKS: MultimapTableDefinition<&[u8; 32], ChunkRow<'static>> =
    MultimapTableDefinition::new("chunks");
/// Chunk key → unit vector as little-endian `f32` bytes.
const VECTORS: TableDefinition<&[u8; 32], &[u8]> = TableDefinition::new("vectors");
/// Block fingerprint → every location of that block.
const BLOCKS: MultimapTableDefinition<&[u8; 32], LocRow<'static>> =
    MultimapTableDefinition::new("blocks");

pub struct RedbIndex {
    path: PathBuf,
}

impl RedbIndex {
    /// An index stored at `path` (created with its parent folder on first write).
    pub fn new(path: &Path) -> RedbIndex {
        RedbIndex {
            path: path.to_path_buf(),
        }
    }

    /// Wait for the lock file; the lock lasts until the file is dropped.
    fn lock(&self) -> Result<fs::File> {
        let path = lock_path(&self.path);
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        file.lock()
            .with_context(|| format!("cannot lock {}", path.display()))?;
        Ok(file)
    }

    /// Run `read` on the stored state; `empty` while the file does not exist
    /// or holds another [`FORMAT`].
    fn read<T>(&self, empty: T, read: impl FnOnce(&ReadTransaction) -> Result<T>) -> Result<T> {
        if !self.path.exists() {
            return Ok(empty);
        }
        let _lock = self.lock()?;
        let opened = open_existing(&self.path)
            .with_context(|| format!("cannot open index {}", self.path.display()))?;
        let Some(db) = opened else {
            return Ok(empty);
        };
        let txn = db.begin_read()?;
        match existing(txn.open_table(META))? {
            Some(meta) if is_current(&meta)? => read(&txn),
            _ => Ok(empty),
        }
    }
}

/// The lock file next to the index: every operation holds it while the
/// database is open, so processes take turns instead of failing.
fn lock_path(index: &Path) -> PathBuf {
    let mut name = index.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// The database at `path`; `None` when there is no file yet. An empty file
/// counts as none: `Database::create` makes the file before it takes the lock.
fn open_existing(path: &Path) -> Result<Option<Database>, DatabaseError> {
    match fs::metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
        Ok(file) if file.len() == 0 => Ok(None),
        Ok(_) => Database::open(path).map(Some),
    }
}

/// A table opened for reading; `None` until a write creates it.
fn existing<T>(opened: Result<T, TableError>) -> Result<Option<T>> {
    match opened {
        Err(TableError::TableDoesNotExist(_)) => Ok(None),
        opened => Ok(Some(opened?)),
    }
}

/// Whether `meta` marks this build's [`FORMAT`]. Every write stores the mark
/// with all the tables, so a marked file has them all.
fn is_current(meta: &impl ReadableTable<&'static str, &'static str>) -> Result<bool> {
    Ok(meta.get(FORMAT_KEY)?.is_some_and(|f| f.value() == FORMAT))
}

/// Delete every table, whatever its type.
fn clear(txn: &WriteTransaction) -> Result<()> {
    for table in txn.list_tables()?.collect::<Vec<_>>() {
        txn.delete_table(table)?;
    }
    for table in txn.list_multimap_tables()?.collect::<Vec<_>>() {
        txn.delete_multimap_table(table)?;
    }
    Ok(())
}

/// `def`, emptied: deleted and created again.
fn fresh<'t, K: Key + 'static, V: Value + 'static>(
    txn: &'t WriteTransaction,
    def: TableDefinition<K, V>,
) -> Result<Table<'t, K, V>> {
    txn.delete_table(def)?;
    Ok(txn.open_table(def)?)
}

/// The snapshot held by `meta` and the note hashes; `None` before the first write.
fn snapshot_in(
    meta: &impl ReadableTable<&'static str, &'static str>,
    hashes: &impl ReadableTable<&'static str, &'static [u8; 32]>,
) -> Result<Option<Snapshot>> {
    let Some(embedder) = meta.get(EMBEDDER_KEY)? else {
        return Ok(None);
    };
    let notes = hashes
        .iter()?
        .map(|row| {
            let (path, hash) = row?;
            Ok((path.value().to_string(), Digest(*hash.value())))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(Snapshot {
        embedder: embedder.value().to_string(),
        notes: NoteHashes::from_notes(notes),
    }))
}

/// Make the tables equal to `update`. Fails before writing anything when the
/// index no longer holds `update.base` ([`Stale`]) or when a chunk has no
/// vector, supplied or stored. A file of another [`FORMAT`] holds nothing,
/// and its tables are dropped.
fn write(txn: &WriteTransaction, update: &Update) -> Result<()> {
    let current = {
        let meta = txn.open_table(META)?;
        if is_current(&meta)? {
            snapshot_in(&meta, &txn.open_table(NOTES)?)?
        } else {
            None
        }
    };
    if current != update.base {
        return Err(Stale.into());
    }
    if current.is_none() {
        clear(txn)?;
    }
    let mut vectors = txn.open_table(VECTORS)?;
    let supplied: HashSet<&Digest> = update.vectors.iter().map(|(key, _)| key).collect();
    for chunk in &update.chunks {
        if !supplied.contains(&chunk.key) && vectors.get(&chunk.key.0)?.is_none() {
            bail!(
                "chunk {} ({} lines {}-{}) has no vector; the index is unchanged",
                chunk.key,
                chunk.at.note,
                chunk.at.lines[0],
                chunk.at.lines[1]
            );
        }
    }

    let mut meta = fresh(txn, META)?;
    meta.insert(FORMAT_KEY, FORMAT)?;
    meta.insert(EMBEDDER_KEY, update.embedder.as_str())?;
    let mut notes = fresh(txn, NOTES)?;
    for (path, hash) in update.notes.notes() {
        notes.insert(path, &hash.0)?;
    }
    txn.delete_multimap_table(CHUNKS)?;
    let mut chunks = txn.open_multimap_table(CHUNKS)?;
    for (position, chunk) in update.chunks.iter().enumerate() {
        let position = u32::try_from(position).context("too many chunks for one index")?;
        chunks.insert(&chunk.key.0, chunk_row(chunk, position))?;
    }
    txn.delete_multimap_table(BLOCKS)?;
    let mut blocks = txn.open_multimap_table(BLOCKS)?;
    for (fingerprint, at) in &update.blocks {
        blocks.insert(&fingerprint.0, loc_row(at))?;
    }

    for (key, v) in &update.vectors {
        vectors.insert(&key.0, vector_bytes(v).as_slice())?;
    }
    let used: HashSet<[u8; 32]> = update.chunks.iter().map(|c| c.key.0).collect();
    vectors.retain(|key, _| used.contains(key))?;
    Ok(())
}

// Row codecs, one pair per stored shape. Decoding never trusts the file: a
// row that does not round-trip is an error.

fn loc_row(at: &Location) -> LocRow<'_> {
    (
        at.note.as_str(),
        at.section.as_str(),
        at.lines[0],
        at.lines[1],
    )
}

fn location((note, section, first, last): LocRow<'_>) -> Result<Location> {
    let parsed =
        NotePath::parse(note).with_context(|| format!("stored note path {note:?} is invalid"))?;
    ensure!(
        parsed.as_str() == note,
        "stored note path {note:?} is not canonical"
    );
    Ok(Location {
        note: parsed,
        section: section.to_string(),
        lines: [first, last],
    })
}

fn chunk_row(chunk: &Chunk, position: u32) -> ChunkRow<'_> {
    (
        loc_row(&chunk.at),
        position,
        chunk.context.as_str(),
        chunk.text.as_str(),
    )
}

fn chunk(key: Digest, (at, _, context, text): ChunkRow<'_>) -> Result<Chunk> {
    let chunk = Chunk::new(location(at)?, context.to_string(), text.to_string());
    ensure!(
        chunk.key == key,
        "stored chunk {key} does not match its content"
    );
    Ok(chunk)
}

fn vector_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn vector(key: Digest, bytes: &[u8]) -> Result<Vec<f32>> {
    ensure!(
        bytes.len().is_multiple_of(4),
        "stored vector {key} has {} bytes, not a whole number of f32s",
        bytes.len()
    );
    Ok(bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

impl Index for RedbIndex {
    fn snapshot(&self) -> Result<Option<Snapshot>> {
        self.read(None, |txn| {
            snapshot_in(&txn.open_table(META)?, &txn.open_table(NOTES)?)
        })
    }

    fn vector_keys(&self) -> Result<HashSet<Digest>> {
        self.read(HashSet::new(), |txn| {
            txn.open_table(VECTORS)?
                .iter()?
                .map(|row| Ok(Digest(*row?.0.value())))
                .collect()
        })
    }

    fn replace(&self, update: Update) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let _lock = self.lock()?;
        let db = Database::create(&self.path)
            .with_context(|| format!("cannot open index {}", self.path.display()))?;
        let txn = db.begin_write()?;
        write(&txn, &update)?;
        txn.commit()?;
        Ok(())
    }

    fn exact(&self, fingerprint: &Digest) -> Result<Vec<Location>> {
        self.read(Vec::new(), |txn| {
            let mut found = txn
                .open_multimap_table(BLOCKS)?
                .get(&fingerprint.0)?
                .map(|row| location(row?.value()))
                .collect::<Result<Vec<_>>>()?;
            found.sort();
            Ok(found)
        })
    }

    fn nearest(&self, query: &[f32], k: usize) -> Result<Vec<Hit>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        self.read(Vec::new(), |txn| {
            let stored = txn
                .open_table(VECTORS)?
                .iter()?
                .map(|row| {
                    let (key, bytes) = row?;
                    let key = Digest(*key.value());
                    Ok((key, vector(key, bytes.value())?))
                })
                .collect::<Result<Vec<_>>>()?;
            if let Some((key, v)) = stored.iter().find(|(_, v)| v.len() != query.len()) {
                bail!(
                    "query has {} dimensions but stored vector {key} has {}; was the index built by another embedder?",
                    query.len(),
                    v.len()
                );
            }
            let best = vector::top_k(query, stored.iter().map(|(key, v)| (*key, v.as_slice())), k);
            let chunks = txn.open_multimap_table(CHUNKS)?;
            let mut hits = Vec::new();
            for (key, score) in best {
                let before = hits.len();
                for row in chunks.get(&key.0)? {
                    let chunk = chunk(key, row?.value())?;
                    hits.push(Hit { chunk, score });
                }
                ensure!(hits.len() > before, "stored vector {key} has no chunk");
            }
            hits.truncate(k);
            Ok(hits)
        })
    }

    fn chunks(&self) -> Result<Vec<Chunk>> {
        self.read(Vec::new(), |txn| {
            let mut out = Vec::new();
            for entry in txn.open_multimap_table(CHUNKS)?.iter()? {
                let (key, rows) = entry?;
                let key = Digest(*key.value());
                for row in rows {
                    let row = row?;
                    let value = row.value();
                    out.push((value.1, chunk(key, value)?));
                }
            }
            out.sort_by(|(p, a), (q, b)| {
                (&a.at.note, a.at.lines, p).cmp(&(&b.at.note, b.at.lines, q))
            });
            Ok(out.into_iter().map(|(_, chunk)| chunk).collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::hash;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};
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

    fn hashes(paths: &[&str]) -> NoteHashes {
        NoteHashes::from_notes(
            paths
                .iter()
                .map(|p| (p.to_string(), hash::bytes(p.as_bytes()))),
        )
    }

    /// Fingerprint of a chunk's only block, as the fixtures store it.
    fn fp(chunk: &Chunk) -> Digest {
        hash::bytes(chunk.text.as_bytes())
    }

    fn update(chunks: Vec<Chunk>, vectors: &[(Digest, &[f32])]) -> Update {
        Update {
            base: None,
            embedder: EMBEDDER.to_string(),
            notes: hashes(&["", "Health/", "Health/Meds.md", "Car.md"]),
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

    /// `up` based on what `index` holds now, as a sync would compute it.
    fn on(index: &RedbIndex, mut up: Update) -> Update {
        up.base = index.snapshot().unwrap();
        up
    }

    fn chunks(hits: &[Hit]) -> Vec<Chunk> {
        hits.iter().map(|h| h.chunk.clone()).collect()
    }

    fn close(x: f32, y: f32) -> bool {
        (x - y).abs() < 1e-6
    }

    /// Snapshot, sorted vector keys, `exact` of each fixture block, and
    /// `nearest` along each fixture vector.
    type Observed = (
        Option<Snapshot>,
        Vec<Digest>,
        Vec<Vec<Location>>,
        Vec<Vec<Hit>>,
    );

    /// Everything the API can tell about the index.
    fn observe(index: &RedbIndex) -> Observed {
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
                notes: full().notes,
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
        assert_eq!(fresh.snapshot().unwrap().unwrap().notes, full().notes);
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
            .replace(on(&index, update(vec![a(), c()], &[(c().key, &VC)])))
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

        index
            .replace(on(&index, update(vec![a()], &[(a().key, &VC)])))
            .unwrap();

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
        let mut bad = on(&index, update(vec![a(), orphan.clone()], &[(a().key, &VB)]));
        bad.embedder = "another-embedder".to_string();
        bad.notes = hashes(&["", "Inbox.md"]);

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
    fn chunks_lists_every_stored_chunk_in_reading_order() {
        let dir = tempdir().unwrap();
        let index = RedbIndex::new(&dir.path().join("index.redb"));
        assert!(
            index.chunks().unwrap().is_empty(),
            "nothing before the first sync"
        );
        index.replace(full()).unwrap();
        assert_eq!(index.chunks().unwrap(), vec![c(), a(), b()]);
    }

    #[test]
    fn chunks_at_one_place_keep_the_order_they_were_given() {
        // The pieces of a cut block share their lines; only the order sync
        // gave them says which comes first. Their keys sort one way, so one
        // of the two orders below would fail if the index went by key.
        let one = chunk("Car.md", "Table", [7, 9], "| part | one |");
        let two = chunk("Car.md", "Table", [7, 9], "| part | two |");
        let dir = tempdir().unwrap();
        let index = RedbIndex::new(&dir.path().join("index.redb"));
        for order in [
            vec![one.clone(), two.clone()],
            vec![two.clone(), one.clone()],
        ] {
            let mut given = vec![c()];
            given.extend(order);
            let vectors: Vec<(Digest, &[f32])> = given.iter().map(|ch| (ch.key, &VA[..])).collect();
            index
                .replace(on(&index, update(given.clone(), &vectors)))
                .unwrap();
            assert_eq!(index.chunks().unwrap(), given);
        }
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
        let fact = hash::bytes(b"vitamin d");
        let other = hash::bytes(b"oil");
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
        assert!(index.exact(&hash::bytes(b"unknown")).unwrap().is_empty());
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
    fn an_empty_file_reads_as_missing_until_replace_fills_it() {
        // What a first write leaves between creating the file and taking its lock.
        let dir = tempdir().unwrap();
        let path = dir.path().join("index.redb");
        fs::write(&path, b"").unwrap();
        let index = RedbIndex::new(&path);

        assert_eq!(index.snapshot().unwrap(), None);
        assert!(index.vector_keys().unwrap().is_empty());
        assert!(index.exact(&fp(&a())).unwrap().is_empty());
        assert!(index.nearest(&VA, 3).unwrap().is_empty());

        index.replace(full()).unwrap();
        assert_eq!(index.snapshot().unwrap().unwrap().notes, full().notes);
    }

    #[test]
    fn replace_refuses_a_stale_base_and_changes_nothing() {
        let dir = tempdir().unwrap();
        let index = RedbIndex::new(&dir.path().join("index.redb"));
        index.replace(full()).unwrap();
        let before = observe(&index);

        // Computed from the empty index, but another writer stored `full` since.
        let late = update(vec![a()], &[(a().key, &VA)]);
        let err = index.replace(late.clone()).unwrap_err();

        assert!(err.is::<Stale>(), "{err:#}");
        assert_eq!(observe(&index), before);
        index.replace(on(&index, late)).unwrap();
        assert_eq!(index.vector_keys().unwrap(), HashSet::from([a().key]));
    }

    #[test]
    fn a_repeated_passage_keeps_every_location() {
        let dir = tempdir().unwrap();
        let index = RedbIndex::new(&dir.path().join("index.redb"));
        let work = chunk("Work/Plan.md", "Plan", [1, 1], "Shared line.");
        let mut home = work.clone();
        home.at = loc("Home/Plan.md", "Plan", [1, 1]);
        assert_eq!(home.key, work.key, "same title, section and text");
        index
            .replace(update(vec![work.clone(), home.clone()], &[(work.key, &VA)]))
            .unwrap();

        let hits = index.nearest(&VA, 5).unwrap();

        assert_eq!(chunks(&hits), vec![home, work]);
        assert!(hits.iter().all(|h| close(h.score, 1.0)));
        assert_eq!(index.nearest(&VA, 1).unwrap().len(), 1, "k still applies");
    }

    #[test]
    fn operations_wait_for_the_lock_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("index.redb");
        let index = RedbIndex::new(&path);
        index.replace(full()).unwrap();
        let (locked, wait_locked) = mpsc::channel();
        let holder = {
            let lock = lock_path(&path);
            thread::spawn(move || {
                let file = fs::File::create(lock).unwrap();
                file.lock().unwrap();
                locked.send(()).unwrap();
                thread::sleep(Duration::from_millis(200));
            })
        };
        wait_locked.recv().unwrap();

        let started = Instant::now();
        let snapshot = index.snapshot().unwrap();

        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "the lock was held, so snapshot had to wait: {:?}",
            started.elapsed()
        );
        assert_eq!(snapshot.map(|s| s.notes), Some(full().notes));
        holder.join().unwrap();
    }

    #[test]
    fn an_index_of_another_format_reads_as_empty_and_is_rebuilt() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("index.redb");
        // An earlier layout: chunks in a plain table, no format mark.
        let old: TableDefinition<&[u8; 32], &str> = TableDefinition::new("chunks");
        let db = Database::create(&path).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_table(META)
            .unwrap()
            .insert(EMBEDDER_KEY, EMBEDDER)
            .unwrap();
        txn.open_table(old)
            .unwrap()
            .insert(&[1u8; 32], "old row")
            .unwrap();
        txn.commit().unwrap();
        drop(db);
        let index = RedbIndex::new(&path);

        assert_eq!(index.snapshot().unwrap(), None);
        assert!(index.vector_keys().unwrap().is_empty());
        assert!(index.nearest(&VA, 3).unwrap().is_empty());

        index.replace(full()).unwrap();
        assert_eq!(index.snapshot().unwrap().unwrap().notes, full().notes);
        assert_eq!(chunks(&index.nearest(&VA, 1).unwrap()), vec![a()]);

        // A layout from a later build is foreign too.
        let later = (FORMAT.parse::<u32>().unwrap() + 1).to_string();
        let db = Database::open(&path).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_table(META)
            .unwrap()
            .insert(FORMAT_KEY, later.as_str())
            .unwrap();
        txn.commit().unwrap();
        drop(db);
        assert_eq!(index.snapshot().unwrap(), None);
        assert!(index.exact(&fp(&a())).unwrap().is_empty());
    }

    /// Mark a hand-written file as this build's format, with all its tables.
    fn mark(txn: &WriteTransaction) {
        txn.open_table(META)
            .unwrap()
            .insert(FORMAT_KEY, FORMAT)
            .unwrap();
        txn.open_table(NOTES).unwrap();
        txn.open_table(VECTORS).unwrap();
        txn.open_multimap_table(CHUNKS).unwrap();
        txn.open_multimap_table(BLOCKS).unwrap();
    }

    #[test]
    fn undecodable_rows_are_errors_not_panics() {
        let dir = tempdir().unwrap();
        let fact = hash::bytes(b"fact");

        // A block outside the vault and a vector of 5 bytes.
        let broken = dir.path().join("broken.redb");
        let db = Database::create(&broken).unwrap();
        let txn = db.begin_write().unwrap();
        mark(&txn);
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
        mark(&txn);
        txn.open_multimap_table(CHUNKS)
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
                    0,
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
