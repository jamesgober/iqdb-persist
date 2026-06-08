//! WAL lifecycle and crash-recovery integration tests.
//!
//! These exercise the public surface only — `open_with` / `insert` /
//! `delete` / `checkpoint` / `load` on a real storing mock index — plus
//! direct manipulation of the on-disk `.wal` file to simulate a crash.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use iqdb_index::{Index, IndexCore, IndexStats};
use iqdb_persist::{PersistConfig, PersistError, Persistable, PersistedIndex, Result};
use iqdb_types::{
    DistanceMetric, Hit, Metadata, Result as IqdbResult, SearchParams, Value, VectorId,
};

// ---------------------------------------------------------------------------
// A storing mock index: keeps real rows so recovery is observable. The
// snapshot payload serialises ids + vectors (not metadata); metadata is
// carried by the WAL and verified through the replay path.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct StoreIndex {
    dim: usize,
    metric: DistanceMetric,
    rows: Vec<(VectorId, Arc<[f32]>, Option<Metadata>)>,
}

impl StoreIndex {
    fn ids(&self) -> Vec<VectorId> {
        self.rows.iter().map(|(id, _, _)| id.clone()).collect()
    }
    fn meta_of(&self, id: &VectorId) -> Option<&Metadata> {
        self.rows
            .iter()
            .find(|(rid, _, _)| rid == id)
            .and_then(|(_, _, m)| m.as_ref())
    }
}

impl IndexCore for StoreIndex {
    fn insert(&mut self, id: VectorId, v: Arc<[f32]>, m: Option<Metadata>) -> IqdbResult<()> {
        self.rows.push((id, v, m));
        Ok(())
    }
    fn delete(&mut self, id: &VectorId) -> IqdbResult<()> {
        self.rows.retain(|(rid, _, _)| rid != id);
        Ok(())
    }
    fn search(&self, _q: &[f32], _p: &SearchParams) -> IqdbResult<Vec<Hit>> {
        Ok(Vec::new())
    }
    fn len(&self) -> usize {
        self.rows.len()
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn metric(&self) -> DistanceMetric {
        self.metric
    }
    fn flush(&mut self) -> IqdbResult<()> {
        Ok(())
    }
    fn stats(&self) -> IndexStats {
        IndexStats {
            n_vectors: self.rows.len(),
            index_type: "store",
            ..IndexStats::default()
        }
    }
}

impl Index for StoreIndex {
    type Config = ();
    fn new(dim: usize, metric: DistanceMetric, _c: ()) -> IqdbResult<Self> {
        Ok(Self {
            dim,
            metric,
            rows: Vec::new(),
        })
    }
}

fn io(source: std::io::Error) -> PersistError {
    PersistError::Io {
        path: PathBuf::new(),
        source,
    }
}

fn tag(metric: DistanceMetric) -> u8 {
    match metric {
        DistanceMetric::Cosine => 0,
        DistanceMetric::DotProduct => 1,
        DistanceMetric::Euclidean => 2,
        DistanceMetric::Manhattan => 3,
        DistanceMetric::Hamming => 4,
        _ => 255,
    }
}

fn untag(t: u8) -> Result<DistanceMetric> {
    Ok(match t {
        0 => DistanceMetric::Cosine,
        1 => DistanceMetric::DotProduct,
        2 => DistanceMetric::Euclidean,
        3 => DistanceMetric::Manhattan,
        4 => DistanceMetric::Hamming,
        _ => {
            return Err(PersistError::InvalidPayload {
                reason: "bad metric tag in mock payload",
            });
        }
    })
}

impl Persistable for StoreIndex {
    const INDEX_TYPE: &'static str = "store";

    fn save_to(&self, w: &mut dyn Write) -> Result<()> {
        w.write_all(&(self.dim as u64).to_le_bytes()).map_err(io)?;
        w.write_all(&[tag(self.metric)]).map_err(io)?;
        w.write_all(&(self.rows.len() as u64).to_le_bytes())
            .map_err(io)?;
        for (id, v, _) in &self.rows {
            let n = match id {
                VectorId::U64(n) => *n,
                VectorId::Bytes(_) => {
                    return Err(PersistError::InvalidPayload {
                        reason: "mock only serialises U64 ids",
                    });
                }
            };
            w.write_all(&n.to_le_bytes()).map_err(io)?;
            for c in v.iter() {
                w.write_all(&c.to_le_bytes()).map_err(io)?;
            }
        }
        Ok(())
    }

    fn load_from(r: &mut dyn Read) -> Result<Self> {
        let mut b8 = [0u8; 8];
        r.read_exact(&mut b8).map_err(io)?;
        let dim = u64::from_le_bytes(b8) as usize;
        let mut b1 = [0u8; 1];
        r.read_exact(&mut b1).map_err(io)?;
        let metric = untag(b1[0])?;
        r.read_exact(&mut b8).map_err(io)?;
        let n = u64::from_le_bytes(b8) as usize;
        let mut rows = Vec::with_capacity(n);
        for _ in 0..n {
            r.read_exact(&mut b8).map_err(io)?;
            let id = VectorId::U64(u64::from_le_bytes(b8));
            let mut v = Vec::with_capacity(dim);
            let mut b4 = [0u8; 4];
            for _ in 0..dim {
                r.read_exact(&mut b4).map_err(io)?;
                v.push(f32::from_le_bytes(b4));
            }
            rows.push((id, Arc::from(v.into_boxed_slice()), None));
        }
        Ok(Self { dim, metric, rows })
    }
}

fn wal_path(snapshot: &Path) -> PathBuf {
    let mut s = snapshot.as_os_str().to_os_string();
    s.push(".wal");
    PathBuf::from(s)
}

fn vec(values: &[f32]) -> Arc<[f32]> {
    Arc::from(values.to_vec().into_boxed_slice())
}

fn wal_cfg(path: &Path) -> PersistConfig {
    let mut cfg = PersistConfig::new(path);
    cfg.wal_enabled = true;
    cfg
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn open_with_wal_writes_initial_snapshot_and_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idx.iqdb");

    let index = StoreIndex::new(2, DistanceMetric::Euclidean, ()).unwrap();
    let _wrapped = PersistedIndex::open_with(index, wal_cfg(&path)).unwrap();

    assert!(path.exists(), "initial snapshot not written");
    assert!(wal_path(&path).exists(), "WAL file not created");
}

#[test]
fn crash_recovery_replays_unckeckpointed_mutations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idx.iqdb");

    // Open empty, log three inserts (one with metadata) and a delete —
    // no checkpoint — then drop the wrapper to simulate a crash.
    {
        let index = StoreIndex::new(2, DistanceMetric::Cosine, ()).unwrap();
        let mut db = PersistedIndex::open_with(index, wal_cfg(&path)).unwrap();
        db.insert(VectorId::from(1u64), vec(&[0.0, 1.0]), None)
            .unwrap();
        let meta: Metadata = [("tag".to_string(), Value::String("a".to_string()))]
            .into_iter()
            .collect();
        db.insert(VectorId::from(2u64), vec(&[2.0, 3.0]), Some(meta))
            .unwrap();
        db.insert(VectorId::from(3u64), vec(&[4.0, 5.0]), None)
            .unwrap();
        db.delete(&VectorId::from(1u64)).unwrap();
        // drop -> "crash"
    }

    let recovered: PersistedIndex<StoreIndex> = PersistedIndex::load(wal_cfg(&path)).unwrap();
    let idx = recovered.index();
    assert_eq!(idx.len(), 2, "expected ids 2 and 3 after replay");
    let ids = idx.ids();
    assert!(ids.contains(&VectorId::from(2u64)));
    assert!(ids.contains(&VectorId::from(3u64)));
    assert!(!ids.contains(&VectorId::from(1u64)), "delete not replayed");

    // Metadata survived the WAL round-trip.
    let m = idx.meta_of(&VectorId::from(2u64)).expect("metadata lost");
    assert_eq!(m.get("tag"), Some(&Value::String("a".to_string())));
}

#[test]
fn checkpoint_resets_wal_and_avoids_double_apply() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idx.iqdb");

    {
        let index = StoreIndex::new(2, DistanceMetric::Cosine, ()).unwrap();
        let mut db = PersistedIndex::open_with(index, wal_cfg(&path)).unwrap();
        db.insert(VectorId::from(1u64), vec(&[1.0, 1.0]), None)
            .unwrap();
        db.insert(VectorId::from(2u64), vec(&[2.0, 2.0]), None)
            .unwrap();
        db.checkpoint().unwrap(); // snapshot {1,2}, WAL reset
        db.insert(VectorId::from(3u64), vec(&[3.0, 3.0]), None)
            .unwrap();
    }

    // load = snapshot {1,2} + replay {3}. If the WAL had not been reset,
    // replaying {1,2} again would double-count.
    let recovered: PersistedIndex<StoreIndex> = PersistedIndex::load(wal_cfg(&path)).unwrap();
    assert_eq!(recovered.index().len(), 3);
    let ids = recovered.index().ids();
    for n in [1u64, 2, 3] {
        assert!(ids.contains(&VectorId::from(n)));
    }
}

#[test]
fn torn_tail_record_is_discarded_on_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idx.iqdb");

    {
        let index = StoreIndex::new(2, DistanceMetric::Cosine, ()).unwrap();
        let mut db = PersistedIndex::open_with(index, wal_cfg(&path)).unwrap();
        db.insert(VectorId::from(1u64), vec(&[1.0, 1.0]), None)
            .unwrap();
        db.insert(VectorId::from(2u64), vec(&[2.0, 2.0]), None)
            .unwrap();
    }

    // Simulate a crash mid-append: chop the final bytes of the WAL so the
    // last frame is truncated.
    let wp = wal_path(&path);
    let bytes = std::fs::read(&wp).unwrap();
    assert!(bytes.len() > 6);
    std::fs::write(&wp, &bytes[..bytes.len() - 5]).unwrap();

    let recovered: PersistedIndex<StoreIndex> = PersistedIndex::load(wal_cfg(&path)).unwrap();
    // The first insert is intact; the torn second one is dropped.
    assert_eq!(recovered.index().len(), 1);
    assert!(recovered.index().ids().contains(&VectorId::from(1u64)));
}

#[test]
fn continued_appends_after_recovery_persist() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idx.iqdb");

    {
        let index = StoreIndex::new(2, DistanceMetric::Cosine, ()).unwrap();
        let mut db = PersistedIndex::open_with(index, wal_cfg(&path)).unwrap();
        db.insert(VectorId::from(1u64), vec(&[1.0, 1.0]), None)
            .unwrap();
    }
    {
        // Recover, then append more — the new records must land at the end
        // of the existing WAL, not clobber it.
        let mut db: PersistedIndex<StoreIndex> = PersistedIndex::load(wal_cfg(&path)).unwrap();
        assert_eq!(db.index().len(), 1);
        db.insert(VectorId::from(2u64), vec(&[2.0, 2.0]), None)
            .unwrap();
    }

    let again: PersistedIndex<StoreIndex> = PersistedIndex::load(wal_cfg(&path)).unwrap();
    assert_eq!(again.index().len(), 2);
}

#[test]
fn bytes_vector_id_round_trips_through_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idx.iqdb");

    let key = VectorId::Bytes(Box::from(&b"content-hash-key"[..]));
    {
        let index = StoreIndex::new(2, DistanceMetric::Cosine, ()).unwrap();
        let mut db = PersistedIndex::open_with(index, wal_cfg(&path)).unwrap();
        db.insert(key.clone(), vec(&[7.0, 8.0]), None).unwrap();
    }
    let recovered: PersistedIndex<StoreIndex> = PersistedIndex::load(wal_cfg(&path)).unwrap();
    assert!(recovered.index().ids().contains(&key));
}

#[test]
fn snapshot_only_mode_ignores_wal_api_durability() {
    // With WAL disabled, insert applies to memory; durability is the
    // explicit save(), and no .wal file is created.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idx.iqdb");

    let index = StoreIndex::new(2, DistanceMetric::Cosine, ()).unwrap();
    let mut db = PersistedIndex::open_with(index, PersistConfig::new(&path)).unwrap();
    db.insert(VectorId::from(1u64), vec(&[1.0, 2.0]), None)
        .unwrap();
    db.save().unwrap();
    assert!(
        !wal_path(&path).exists(),
        "no WAL expected in snapshot mode"
    );

    let restored: PersistedIndex<StoreIndex> =
        PersistedIndex::load(PersistConfig::new(&path)).unwrap();
    assert_eq!(restored.index().len(), 1);
}
