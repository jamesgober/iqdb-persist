//! Property tests for the two core invariants from `dev/DIRECTIVES.md` §8.
//!
//! 1. **Atomic snapshot round-trip** — a saved index loads back byte-for-
//!    byte equal in state, for arbitrary contents and any compression
//!    scheme.
//! 2. **WAL replay is the recovery contract** — after an arbitrary sequence
//!    of logged mutations and a simulated crash (drop without a final
//!    checkpoint), `load` reconstructs exactly the state an equivalent
//!    in-memory model holds.
//!
//! Both run against a full-fidelity mock index that serialises ids,
//! vectors, and metadata, so the comparison exercises the whole pipeline.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use iqdb_index::{Index, IndexCore, IndexStats};
use iqdb_persist::{Compression, PersistConfig, PersistError, Persistable, PersistedIndex, Result};
use iqdb_types::{
    DistanceMetric, Hit, Metadata, Result as IqdbResult, SearchParams, Value, VectorId,
};
use proptest::prelude::*;

// --- full-fidelity mock index ----------------------------------------------

#[derive(Debug, Clone)]
struct StoreIndex {
    dim: usize,
    metric: DistanceMetric,
    rows: Vec<(u64, Arc<[f32]>, Option<Metadata>)>,
}

impl StoreIndex {
    /// Live rows as `(id, vector, meta)`, sorted by id — the canonical form
    /// for comparing two indices' state regardless of insertion order.
    fn snapshot_state(&self) -> Vec<(u64, Vec<f32>, Option<Metadata>)> {
        let mut out: Vec<_> = self
            .rows
            .iter()
            .map(|(id, v, m)| (*id, v.to_vec(), m.clone()))
            .collect();
        out.sort_by_key(|(id, _, _)| *id);
        out
    }
}

impl IndexCore for StoreIndex {
    fn insert(&mut self, id: VectorId, v: Arc<[f32]>, m: Option<Metadata>) -> IqdbResult<()> {
        if let VectorId::U64(n) = id {
            self.rows.push((n, v, m));
        }
        Ok(())
    }
    fn delete(&mut self, id: &VectorId) -> IqdbResult<()> {
        if let VectorId::U64(n) = id {
            self.rows.retain(|(r, _, _)| r != n);
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

fn ioerr(source: std::io::Error) -> PersistError {
    PersistError::Io {
        path: PathBuf::new(),
        source,
    }
}

fn put_str(w: &mut dyn Write, s: &str) -> Result<()> {
    w.write_all(&(s.len() as u32).to_le_bytes())
        .map_err(ioerr)?;
    w.write_all(s.as_bytes()).map_err(ioerr)
}

fn put_value(w: &mut dyn Write, v: &Value) -> Result<()> {
    match v {
        Value::String(s) => {
            w.write_all(&[0]).map_err(ioerr)?;
            put_str(w, s)?;
        }
        Value::Int(i) => {
            w.write_all(&[1]).map_err(ioerr)?;
            w.write_all(&i.to_le_bytes()).map_err(ioerr)?;
        }
        Value::Float(f) => {
            w.write_all(&[2]).map_err(ioerr)?;
            w.write_all(&f.to_le_bytes()).map_err(ioerr)?;
        }
        Value::Bool(b) => {
            w.write_all(&[3]).map_err(ioerr)?;
            w.write_all(&[u8::from(*b)]).map_err(ioerr)?;
        }
        Value::Null => w.write_all(&[4]).map_err(ioerr)?,
    }
    Ok(())
}

fn get_u32(r: &mut dyn Read) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).map_err(ioerr)?;
    Ok(u32::from_le_bytes(b))
}

fn get_str(r: &mut dyn Read) -> Result<String> {
    let len = get_u32(r)? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).map_err(ioerr)?;
    String::from_utf8(buf).map_err(|_| PersistError::InvalidPayload {
        reason: "bad utf8 in mock",
    })
}

fn get_value(r: &mut dyn Read) -> Result<Value> {
    let mut t = [0u8; 1];
    r.read_exact(&mut t).map_err(ioerr)?;
    Ok(match t[0] {
        0 => Value::String(get_str(r)?),
        1 => {
            let mut b = [0u8; 8];
            r.read_exact(&mut b).map_err(ioerr)?;
            Value::Int(i64::from_le_bytes(b))
        }
        2 => {
            let mut b = [0u8; 8];
            r.read_exact(&mut b).map_err(ioerr)?;
            Value::Float(f64::from_le_bytes(b))
        }
        3 => {
            let mut b = [0u8; 1];
            r.read_exact(&mut b).map_err(ioerr)?;
            Value::Bool(b[0] != 0)
        }
        4 => Value::Null,
        _ => {
            return Err(PersistError::InvalidPayload {
                reason: "bad value tag in mock",
            });
        }
    })
}

impl Persistable for StoreIndex {
    const INDEX_TYPE: &'static str = "store";

    fn save_to(&self, w: &mut dyn Write) -> Result<()> {
        w.write_all(&(self.dim as u64).to_le_bytes())
            .map_err(ioerr)?;
        w.write_all(&(self.rows.len() as u64).to_le_bytes())
            .map_err(ioerr)?;
        for (id, v, m) in &self.rows {
            w.write_all(&id.to_le_bytes()).map_err(ioerr)?;
            for c in v.iter() {
                w.write_all(&c.to_le_bytes()).map_err(ioerr)?;
            }
            match m {
                Some(meta) => {
                    w.write_all(&[1]).map_err(ioerr)?;
                    w.write_all(&(meta.len() as u32).to_le_bytes())
                        .map_err(ioerr)?;
                    for (k, val) in meta.iter() {
                        put_str(w, k)?;
                        put_value(w, val)?;
                    }
                }
                None => w.write_all(&[0]).map_err(ioerr)?,
            }
        }
        Ok(())
    }

    fn load_from(r: &mut dyn Read) -> Result<Self> {
        let mut b8 = [0u8; 8];
        r.read_exact(&mut b8).map_err(ioerr)?;
        let dim = u64::from_le_bytes(b8) as usize;
        r.read_exact(&mut b8).map_err(ioerr)?;
        let n = u64::from_le_bytes(b8) as usize;
        let mut rows = Vec::with_capacity(n);
        for _ in 0..n {
            r.read_exact(&mut b8).map_err(ioerr)?;
            let id = u64::from_le_bytes(b8);
            let mut v = Vec::with_capacity(dim);
            let mut b4 = [0u8; 4];
            for _ in 0..dim {
                r.read_exact(&mut b4).map_err(ioerr)?;
                v.push(f32::from_le_bytes(b4));
            }
            let mut has = [0u8; 1];
            r.read_exact(&mut has).map_err(ioerr)?;
            let meta = if has[0] == 1 {
                let count = get_u32(r)? as usize;
                let mut map: BTreeMap<String, Value> = BTreeMap::new();
                for _ in 0..count {
                    let k = get_str(r)?;
                    let val = get_value(r)?;
                    let _ = map.insert(k, val);
                }
                Some(map.into_iter().collect())
            } else {
                None
            };
            rows.push((id, Arc::from(v.into_boxed_slice()), meta));
        }
        Ok(Self {
            dim,
            metric: DistanceMetric::Cosine,
            rows,
        })
    }
}

// --- strategies ------------------------------------------------------------

const DIM: usize = 4;

fn vector_strategy() -> impl Strategy<Value = Vec<f32>> {
    // Bounded, always-finite f32 values (exact round-trip, no NaN).
    prop::collection::vec((-1000i32..1000).prop_map(|n| n as f32 / 8.0), DIM..=DIM)
}

fn meta_strategy() -> impl Strategy<Value = Option<Metadata>> {
    let value = prop_oneof![
        "[a-z]{0,8}".prop_map(Value::String),
        any::<i64>().prop_map(Value::Int),
        any::<bool>().prop_map(Value::Bool),
        Just(Value::Null),
    ];
    prop::option::of(
        prop::collection::vec(("[a-z]{1,6}", value), 0..3)
            .prop_map(|kv| kv.into_iter().collect::<Metadata>()),
    )
}

#[derive(Debug, Clone)]
enum Action {
    Insert(Vec<f32>, Option<Metadata>),
    Delete(usize),
    Checkpoint,
}

fn action_strategy() -> impl Strategy<Value = Action> {
    prop_oneof![
        5 => (vector_strategy(), meta_strategy()).prop_map(|(v, m)| Action::Insert(v, m)),
        3 => any::<usize>().prop_map(Action::Delete),
        1 => Just(Action::Checkpoint),
    ]
}

fn vec_arc(v: &[f32]) -> Arc<[f32]> {
    Arc::from(v.to_vec().into_boxed_slice())
}

// --- the invariants --------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Invariant 1: snapshot save → load preserves state exactly, for every
    /// compression scheme available in this build.
    #[test]
    fn snapshot_round_trip_preserves_state(
        rows in prop::collection::vec((any::<u64>(), vector_strategy(), meta_strategy()), 0..40),
    ) {
        // Deduplicate ids so `len`/state are well defined for the mock.
        let mut seen = std::collections::HashSet::new();
        let unique: Vec<_> = rows.into_iter().filter(|(id, _, _)| seen.insert(*id)).collect();

        let mut schemes = vec![Compression::None];
        if cfg!(feature = "zstd") { schemes.push(Compression::Zstd { level: 3 }); }
        if cfg!(feature = "lz4") { schemes.push(Compression::Lz4); }

        for scheme in schemes {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("s.iqdb");
            let mut cfg = PersistConfig::new(&path);
            cfg.compression = scheme;

            let mut idx = StoreIndex::new(DIM, DistanceMetric::Cosine, ()).unwrap();
            for (id, v, m) in &unique {
                idx.insert(VectorId::from(*id), vec_arc(v), m.clone()).unwrap();
            }
            let want = idx.snapshot_state();

            PersistedIndex::open_with(idx, cfg.clone()).unwrap().save().unwrap();
            let restored: PersistedIndex<StoreIndex> = PersistedIndex::load(cfg).unwrap();
            prop_assert_eq!(restored.index().snapshot_state(), want);
        }
    }

    /// Invariant 2: an arbitrary mutation sequence through a WAL-backed
    /// index, followed by a crash (drop without a trailing checkpoint),
    /// recovers to exactly the in-memory model's state.
    #[test]
    fn wal_replay_matches_in_memory_model(actions in prop::collection::vec(action_strategy(), 0..60)) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("idx.iqdb");
        let mut cfg = PersistConfig::new(&path);
        cfg.wal_enabled = true;

        // Reference model: the source of truth for expected state.
        let mut model = StoreIndex::new(DIM, DistanceMetric::Cosine, ()).unwrap();
        let mut live: Vec<u64> = Vec::new();
        let mut next_id: u64 = 0;

        {
            let mut db = PersistedIndex::open_with(
                StoreIndex::new(DIM, DistanceMetric::Cosine, ()).unwrap(),
                cfg.clone(),
            ).unwrap();

            for action in actions {
                match action {
                    Action::Insert(v, m) => {
                        let id = next_id;
                        next_id += 1;
                        live.push(id);
                        model.insert(VectorId::from(id), vec_arc(&v), m.clone()).unwrap();
                        db.insert(VectorId::from(id), vec_arc(&v), m).unwrap();
                    }
                    Action::Delete(k) => {
                        if !live.is_empty() {
                            let id = live.remove(k % live.len());
                            model.delete(&VectorId::from(id)).unwrap();
                            db.delete(&VectorId::from(id)).unwrap();
                        }
                    }
                    Action::Checkpoint => db.checkpoint().unwrap(),
                }
            }
            // drop db -> simulated crash (no trailing checkpoint)
        }

        let recovered: PersistedIndex<StoreIndex> = PersistedIndex::load(cfg).unwrap();
        prop_assert_eq!(recovered.index().snapshot_state(), model.snapshot_state());
    }
}
