//! WAL hot-path benchmarks.
//!
//! Measures the steady-state cost of [`PersistedIndex::insert`] in WAL mode
//! (encode the record, frame it, write it) and the cost of replaying a
//! populated WAL on [`PersistedIndex::load`]. `FsyncPolicy::Never` isolates
//! the library's own work from the OS `fsync` latency, which is
//! disk-hardware-bound and not what this crate controls.
//!
//! Run with `cargo bench`.

use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use iqdb_index::{Index, IndexCore, IndexStats};
use iqdb_persist::{FsyncPolicy, PersistConfig, PersistError, Persistable, PersistedIndex, Result};
use iqdb_types::{DistanceMetric, Hit, Metadata, Result as IqdbResult, SearchParams, VectorId};

/// A counting mock: insert/delete are O(1) so the benchmark reflects the
/// WAL path, not index work.
struct CountIndex {
    dim: usize,
    metric: DistanceMetric,
    n: usize,
}

impl IndexCore for CountIndex {
    fn insert(&mut self, _: VectorId, _: Arc<[f32]>, _: Option<Metadata>) -> IqdbResult<()> {
        self.n += 1;
        Ok(())
    }
    fn delete(&mut self, _: &VectorId) -> IqdbResult<()> {
        Ok(())
    }
    fn search(&self, _: &[f32], _: &SearchParams) -> IqdbResult<Vec<Hit>> {
        Ok(Vec::new())
    }
    fn len(&self) -> usize {
        self.n
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
            n_vectors: self.n,
            index_type: "count",
            ..IndexStats::default()
        }
    }
}

impl Index for CountIndex {
    type Config = ();
    fn new(dim: usize, metric: DistanceMetric, _: ()) -> IqdbResult<Self> {
        Ok(Self { dim, metric, n: 0 })
    }
}

impl Persistable for CountIndex {
    const INDEX_TYPE: &'static str = "count";
    fn save_to(&self, w: &mut dyn std::io::Write) -> Result<()> {
        let io = |source| PersistError::Io {
            path: std::path::PathBuf::new(),
            source,
        };
        w.write_all(&(self.dim as u64).to_le_bytes()).map_err(io)?;
        w.write_all(&(self.n as u64).to_le_bytes()).map_err(io)?;
        Ok(())
    }
    fn load_from(r: &mut dyn std::io::Read) -> Result<Self> {
        let io = |source| PersistError::Io {
            path: std::path::PathBuf::new(),
            source,
        };
        let mut b = [0u8; 8];
        r.read_exact(&mut b).map_err(io)?;
        let dim = u64::from_le_bytes(b) as usize;
        r.read_exact(&mut b).map_err(io)?;
        let n = u64::from_le_bytes(b) as usize;
        Ok(Self {
            dim,
            metric: DistanceMetric::Cosine,
            n,
        })
    }
}

fn wal_cfg(path: &std::path::Path) -> PersistConfig {
    let mut cfg = PersistConfig::new(path);
    cfg.wal_enabled = true;
    cfg.fsync_policy = FsyncPolicy::Never;
    cfg
}

fn bench_append(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("append.iqdb");
    let index = CountIndex::new(128, DistanceMetric::Cosine, ()).unwrap();
    let mut db = PersistedIndex::open_with(index, wal_cfg(&path)).unwrap();
    let v: Arc<[f32]> = Arc::from(vec![0.1f32; 128].into_boxed_slice());
    let mut id = 0u64;

    let mut group = c.benchmark_group("wal_append");
    group.throughput(criterion::Throughput::Elements(1));
    group.bench_function("insert_d128_never", |b| {
        b.iter(|| {
            id += 1;
            db.insert(VectorId::U64(id), v.clone(), None).unwrap();
        });
    });
    group.finish();
}

fn bench_replay(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("replay.iqdb");
    {
        let index = CountIndex::new(128, DistanceMetric::Cosine, ()).unwrap();
        let mut db = PersistedIndex::open_with(index, wal_cfg(&path)).unwrap();
        let v: Arc<[f32]> = Arc::from(vec![0.1f32; 128].into_boxed_slice());
        for id in 0..10_000u64 {
            db.insert(VectorId::U64(id), v.clone(), None).unwrap();
        }
    }

    c.bench_function("wal_replay_10k_d128", |b| {
        b.iter(|| {
            let db: PersistedIndex<CountIndex> = PersistedIndex::load(wal_cfg(&path)).unwrap();
            assert_eq!(db.index().len(), 10_000);
        });
    });
}

criterion_group!(benches, bench_append, bench_replay);
criterion_main!(benches);
