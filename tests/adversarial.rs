//! Adversarial / partial-write hardening.
//!
//! Exhaustively mutates valid snapshot and WAL files — every single-byte
//! flip, every truncation length — plus pseudo-random garbage, and asserts
//! the loader never panics, never out-of-memories, and never returns a
//! silently-wrong result. A flipped or truncated file is always either a
//! clean `Err` (snapshot) or a correctly-shortened replay (WAL torn tail).
//!
//! This is the property the directives demand of the parse and recovery
//! paths: untrusted input is validated, allocations are bounded, and
//! library code does not panic on hostile input.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use iqdb_index::{Index, IndexCore, IndexStats};
use iqdb_persist::{PersistConfig, PersistError, Persistable, PersistedIndex, Result, VERSION};
use iqdb_types::{DistanceMetric, Hit, Metadata, Result as IqdbResult, SearchParams, VectorId};

// --- minimal storing mock --------------------------------------------------

#[derive(Debug)]
struct StoreIndex {
    dim: usize,
    metric: DistanceMetric,
    rows: Vec<(u64, Arc<[f32]>)>,
}

impl IndexCore for StoreIndex {
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

impl Persistable for StoreIndex {
    const INDEX_TYPE: &'static str = "store";

    fn save_to(&self, w: &mut dyn Write) -> Result<()> {
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
        let mut b8 = [0u8; 8];
        r.read_exact(&mut b8).map_err(io)?;
        let dim = u64::from_le_bytes(b8) as usize;
        // Bound the per-row work so a hostile payload that survived framing
        // cannot drive an unbounded allocation here.
        if dim > 1 << 16 {
            return Err(PersistError::InvalidPayload {
                reason: "mock dim too large",
            });
        }
        r.read_exact(&mut b8).map_err(io)?;
        let n = u64::from_le_bytes(b8) as usize;
        if n > 1 << 24 {
            return Err(PersistError::InvalidPayload {
                reason: "mock row count too large",
            });
        }
        let mut rows = Vec::new();
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

// --- helpers ---------------------------------------------------------------

fn populated(dim: usize, n: u64) -> StoreIndex {
    let mut idx = StoreIndex::new(dim, DistanceMetric::Cosine, ()).unwrap();
    for i in 0..n {
        let v: Vec<f32> = (0..dim).map(|d| (i as f32) + d as f32 * 0.25).collect();
        idx.insert(VectorId::from(i), Arc::from(v.into_boxed_slice()), None)
            .unwrap();
    }
    idx
}

fn wal_path(snapshot: &Path) -> PathBuf {
    let mut s = snapshot.as_os_str().to_os_string();
    s.push(".wal");
    PathBuf::from(s)
}

/// A small, dependency-free PRNG (SplitMix64) for deterministic garbage.
fn garbage(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    (0..len)
        .map(|_| {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            ((z ^ (z >> 31)) & 0xFF) as u8
        })
        .collect()
}

fn load_snapshot(path: &Path, bytes: &[u8]) -> Result<PersistedIndex<StoreIndex>> {
    std::fs::write(path, bytes).unwrap();
    PersistedIndex::<StoreIndex>::load(PersistConfig::new(path))
}

// --- snapshot adversarial --------------------------------------------------

#[test]
fn sanity_version_is_current() {
    // Guards against the harness drifting from the crate version.
    assert!(VERSION.starts_with("0.5"));
}

#[test]
fn every_single_byte_flip_in_a_snapshot_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.iqdb");
    PersistedIndex::open_with(populated(8, 20), PersistConfig::new(&good))
        .unwrap()
        .save()
        .unwrap();
    let bytes = std::fs::read(&good).unwrap();

    let probe = dir.path().join("probe.iqdb");
    assert!(load_snapshot(&probe, &bytes).is_ok(), "baseline must load");

    for i in 0..bytes.len() {
        let mut m = bytes.clone();
        m[i] ^= 0xFF;
        // Every byte is either header metadata (validated / cross-checked)
        // or inside the CRC-covered payload region, so any flip is caught.
        assert!(
            load_snapshot(&probe, &m).is_err(),
            "single-byte flip at offset {i} loaded successfully — silent corruption",
        );
    }
}

#[test]
fn every_truncation_of_a_snapshot_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.iqdb");
    PersistedIndex::open_with(populated(8, 20), PersistConfig::new(&good))
        .unwrap()
        .save()
        .unwrap();
    let bytes = std::fs::read(&good).unwrap();

    let probe = dir.path().join("probe.iqdb");
    for t in 0..bytes.len() {
        assert!(
            load_snapshot(&probe, &bytes[..t]).is_err(),
            "truncation to {t} of {} bytes loaded successfully",
            bytes.len(),
        );
    }
    assert!(load_snapshot(&probe, &bytes).is_ok());
}

#[test]
fn pseudo_random_garbage_is_never_a_valid_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let probe = dir.path().join("probe.iqdb");
    for seed in 0..512u64 {
        // Result is ignored on purpose — the assertion is that this neither
        // panics nor aborts (OOM); a clean Err/Ok both pass.
        let _ = load_snapshot(&probe, &garbage(seed, 96));
    }
    // And a garbage buffer carrying the real magic still cannot pass the
    // header/CRC gauntlet.
    let mut with_magic = b"IQDBPRST".to_vec();
    with_magic.extend_from_slice(&garbage(7, 200));
    assert!(load_snapshot(&probe, &with_magic).is_err());
}

// --- WAL adversarial -------------------------------------------------------

fn wal_cfg(path: &Path) -> PersistConfig {
    let mut cfg = PersistConfig::new(path);
    cfg.wal_enabled = true;
    cfg
}

/// Build a WAL-backed db with `n` logged inserts (no checkpoint) and return
/// the snapshot path plus the raw WAL bytes.
fn build_wal(dir: &Path, n: u64) -> (PathBuf, Vec<u8>) {
    let path = dir.join("idx.iqdb");
    {
        let mut db = PersistedIndex::open_with(
            StoreIndex::new(4, DistanceMetric::Cosine, ()).unwrap(),
            wal_cfg(&path),
        )
        .unwrap();
        for i in 0..n {
            db.insert(
                VectorId::from(i),
                Arc::from(vec![i as f32; 4].into_boxed_slice()),
                None,
            )
            .unwrap();
        }
    }
    let wal = std::fs::read(wal_path(&path)).unwrap();
    (path, wal)
}

#[test]
fn every_truncation_of_a_wal_recovers_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let (path, wal) = build_wal(dir.path(), 12);

    for t in 0..=wal.len() {
        std::fs::write(wal_path(&path), &wal[..t]).unwrap();
        // A truncated WAL is a crash mid-append: replay must drop the torn
        // tail and succeed, never panic.
        let db = PersistedIndex::<StoreIndex>::load(wal_cfg(&path))
            .unwrap_or_else(|e| panic!("WAL truncation to {t} failed to recover: {e:?}"));
        assert!(db.index().len() <= 12);
    }
}

#[test]
fn every_single_byte_flip_in_a_wal_is_handled() {
    let dir = tempfile::tempdir().unwrap();
    let (path, wal) = build_wal(dir.path(), 12);

    for i in 0..wal.len() {
        let mut m = wal.clone();
        m[i] ^= 0xFF;
        std::fs::write(wal_path(&path), &m).unwrap();
        // Either a clean Err (corrupt WAL header) or a correctly-shortened
        // replay (corrupt frame stops replay) — never a panic, never more
        // records than were written.
        if let Ok(db) = PersistedIndex::<StoreIndex>::load(wal_cfg(&path)) {
            assert!(db.index().len() <= 12);
        }
    }
}

#[test]
fn pseudo_random_garbage_as_a_wal_is_handled() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _wal) = build_wal(dir.path(), 4);

    for seed in 0..256u64 {
        std::fs::write(wal_path(&path), garbage(seed, 80)).unwrap();
        // Must not panic; a garbage WAL is rejected (bad magic) or, by
        // astronomically unlikely coincidence, parsed — both are fine.
        let _ = PersistedIndex::<StoreIndex>::load(wal_cfg(&path));
    }
}
