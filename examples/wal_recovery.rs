//! Durable mutation with the write-ahead log, and crash recovery.
//!
//! Shows the WAL lifecycle a real consumer follows: enable `wal_enabled`,
//! mutate through the wrapper (each op logged + `fsync`ed before it touches
//! memory), `checkpoint` to fold the log into a snapshot, then recover the
//! last acknowledged state with `load` after a simulated crash.
//!
//! ```sh
//! cargo run --example wal_recovery
//! ```

use std::io::{Read, Write};
use std::sync::Arc;

use iqdb_index::{Index, IndexCore, IndexStats};
use iqdb_persist::{PersistConfig, PersistError, Persistable, PersistedIndex, Result};
use iqdb_types::{DistanceMetric, Hit, Metadata, Result as IqdbResult, SearchParams, VectorId};

/// A toy in-memory index: a `Vec` of `(id, vector)` rows, just enough to be
/// `Persistable`.
struct VecIndex {
    dim: usize,
    metric: DistanceMetric,
    rows: Vec<(u64, Arc<[f32]>)>,
}

impl IndexCore for VecIndex {
    fn insert(&mut self, id: VectorId, v: Arc<[f32]>, _m: Option<Metadata>) -> IqdbResult<()> {
        if let VectorId::U64(n) = id {
            self.rows.push((n, v));
        }
        Ok(())
    }
    fn delete(&mut self, id: &VectorId) -> IqdbResult<()> {
        if let VectorId::U64(n) = id {
            self.rows.retain(|(r, _)| r != n);
        }
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
            index_type: "vec",
            ..IndexStats::default()
        }
    }
}

impl Index for VecIndex {
    type Config = ();
    fn new(dim: usize, metric: DistanceMetric, _c: ()) -> IqdbResult<Self> {
        Ok(Self {
            dim,
            metric,
            rows: Vec::new(),
        })
    }
}

impl Persistable for VecIndex {
    const INDEX_TYPE: &'static str = "vec";

    fn save_to(&self, w: &mut dyn Write) -> Result<()> {
        let io = |s| PersistError::Io {
            path: std::path::PathBuf::new(),
            source: s,
        };
        w.write_all(&(self.dim as u64).to_le_bytes()).map_err(io)?;
        w.write_all(&(self.rows.len() as u64).to_le_bytes())
            .map_err(io)?;
        for (id, v) in &self.rows {
            w.write_all(&id.to_le_bytes()).map_err(io)?;
            for c in v.iter() {
                w.write_all(&c.to_le_bytes()).map_err(io)?;
            }
        }
        Ok(())
    }

    fn load_from(r: &mut dyn Read) -> Result<Self> {
        let io = |s| PersistError::Io {
            path: std::path::PathBuf::new(),
            source: s,
        };
        let mut b8 = [0u8; 8];
        r.read_exact(&mut b8).map_err(io)?;
        let dim = u64::from_le_bytes(b8) as usize;
        r.read_exact(&mut b8).map_err(io)?;
        let n = u64::from_le_bytes(b8) as usize;
        let mut rows = Vec::with_capacity(n);
        for _ in 0..n {
            r.read_exact(&mut b8).map_err(io)?;
            let id = u64::from_le_bytes(b8);
            let mut v = Vec::with_capacity(dim);
            let mut b4 = [0u8; 4];
            for _ in 0..dim {
                r.read_exact(&mut b4).map_err(io)?;
                v.push(f32::from_le_bytes(b4));
            }
            rows.push((id, Arc::from(v.into_boxed_slice())));
        }
        Ok(Self {
            dim,
            metric: DistanceMetric::Cosine,
            rows,
        })
    }
}

fn v(values: &[f32]) -> Arc<[f32]> {
    Arc::from(values.to_vec().into_boxed_slice())
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join("iqdb-persist-wal-example");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("index.iqdb");

    let mut cfg = PersistConfig::new(&path);
    cfg.wal_enabled = true;

    // Session 1: build, mutate, checkpoint, then mutate again — and "crash"
    // (drop) without a trailing checkpoint.
    {
        let index = VecIndex::new(3, DistanceMetric::Cosine, ())?;
        let mut db = PersistedIndex::open_with(index, cfg.clone())?;

        db.insert(VectorId::from(1u64), v(&[0.1, 0.2, 0.3]), None)?;
        db.insert(VectorId::from(2u64), v(&[0.4, 0.5, 0.6]), None)?;
        db.checkpoint()?; // fold {1,2} into a snapshot, reset the WAL

        // These land in the WAL only — no checkpoint follows.
        db.insert(VectorId::from(3u64), v(&[0.7, 0.8, 0.9]), None)?;
        db.delete(&VectorId::from(1u64))?;
        println!("session 1 (in memory): {} vectors", db.index().len());
        // drop -> simulated crash
    }

    // Session 2: recover. load replays the post-checkpoint WAL (insert 3,
    // delete 1) onto the snapshot ({1,2}), leaving {2,3}.
    {
        let db: PersistedIndex<VecIndex> = PersistedIndex::load(cfg)?;
        let mut ids: Vec<u64> = db.index().rows.iter().map(|(id, _)| *id).collect();
        ids.sort_unstable();
        println!(
            "session 2 (recovered): {} vectors, ids = {:?}",
            db.index().len(),
            ids
        );
        assert_eq!(ids, vec![2, 3]);
    }

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}
