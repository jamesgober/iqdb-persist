//! Snapshot-compression integration tests.
//!
//! Round-trips a populated index through a compressed snapshot for each
//! feature-gated scheme, and verifies that a legacy (format v1, no
//! preamble) snapshot still loads.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use iqdb_index::{Index, IndexCore, IndexStats};
#[cfg(any(feature = "zstd", feature = "lz4"))]
use iqdb_persist::Compression;
use iqdb_persist::{
    FileHeader, MAGIC, PersistConfig, PersistError, Persistable, PersistedIndex, Result, checksum,
};
use iqdb_types::{DistanceMetric, Hit, Metadata, Result as IqdbResult, SearchParams, VectorId};

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
    fn delete(&mut self, _id: &VectorId) -> IqdbResult<()> {
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
        let tag: u8 = match self.metric {
            DistanceMetric::Cosine => 0,
            DistanceMetric::DotProduct => 1,
            DistanceMetric::Euclidean => 2,
            DistanceMetric::Manhattan => 3,
            DistanceMetric::Hamming => 4,
            _ => 255,
        };
        w.write_all(&[tag]).map_err(io)?;
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
        let mut b1 = [0u8; 1];
        r.read_exact(&mut b1).map_err(io)?;
        let metric = match b1[0] {
            0 => DistanceMetric::Cosine,
            1 => DistanceMetric::DotProduct,
            2 => DistanceMetric::Euclidean,
            3 => DistanceMetric::Manhattan,
            4 => DistanceMetric::Hamming,
            _ => {
                return Err(PersistError::InvalidPayload {
                    reason: "bad metric tag",
                });
            }
        };
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
        Ok(Self { dim, metric, rows })
    }
}

fn populated(dim: usize, n: u64) -> StoreIndex {
    let mut idx = StoreIndex::new(dim, DistanceMetric::Cosine, ()).unwrap();
    for i in 0..n {
        // Zero-ish vectors compress well, exercising a real ratio.
        idx.insert(
            VectorId::from(i),
            Arc::from(vec![0.0f32; dim].into_boxed_slice()),
            None,
        )
        .unwrap();
    }
    idx
}

#[cfg(any(feature = "zstd", feature = "lz4"))]
fn round_trip_with(scheme: Compression) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.iqdb");
    let mut cfg = PersistConfig::new(&path);
    cfg.compression = scheme;

    PersistedIndex::open_with(populated(16, 200), cfg.clone())
        .unwrap()
        .save()
        .unwrap();

    let restored: PersistedIndex<StoreIndex> = PersistedIndex::load(cfg).unwrap();
    assert_eq!(restored.index().len(), 200);
    assert_eq!(restored.index().dim(), 16);
}

#[cfg(feature = "zstd")]
#[test]
fn zstd_snapshot_round_trips() {
    round_trip_with(Compression::Zstd { level: 3 });
}

#[cfg(feature = "lz4")]
#[test]
fn lz4_snapshot_round_trips() {
    round_trip_with(Compression::Lz4);
}

#[cfg(feature = "zstd")]
#[test]
fn compression_shrinks_a_compressible_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let raw_path = dir.path().join("raw.iqdb");
    let zstd_path = dir.path().join("zstd.iqdb");

    PersistedIndex::open_with(populated(64, 500), PersistConfig::new(&raw_path))
        .unwrap()
        .save()
        .unwrap();

    let mut cfg = PersistConfig::new(&zstd_path);
    cfg.compression = Compression::Zstd { level: 9 };
    PersistedIndex::open_with(populated(64, 500), cfg)
        .unwrap()
        .save()
        .unwrap();

    let raw = std::fs::metadata(&raw_path).unwrap().len();
    let zstd = std::fs::metadata(&zstd_path).unwrap().len();
    assert!(zstd < raw, "compressed {zstd} not smaller than raw {raw}");
}

#[cfg(feature = "lz4")]
#[test]
fn corrupt_compressed_payload_is_caught_by_crc() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.iqdb");
    let mut cfg = PersistConfig::new(&path);
    cfg.compression = Compression::Lz4;

    PersistedIndex::open_with(populated(8, 50), cfg.clone())
        .unwrap()
        .save()
        .unwrap();

    // Flip a byte in the payload region (well past the header).
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    std::fs::write(&path, &bytes).unwrap();

    let err = PersistedIndex::<StoreIndex>::load(cfg).unwrap_err();
    assert!(matches!(err, PersistError::ChecksumMismatch { .. }));
}

/// A format-v1 snapshot (no compression preamble) — what v0.2 / v0.3 wrote
/// — must still load under the v0.4 reader.
#[test]
fn legacy_v1_snapshot_still_loads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.iqdb");

    let index = populated(4, 10);

    // Serialize exactly what v1 stored: header (version 1) + raw payload,
    // crc32 over the raw payload.
    let mut raw = Vec::new();
    index.save_to(&mut raw).unwrap();
    let header = FileHeader {
        magic: MAGIC,
        version: 1,
        index_type: StoreIndex::INDEX_TYPE.to_string(),
        dim: index.dim(),
        metric: index.metric(),
        n_vectors: index.len(),
        crc32: checksum::compute(&raw),
    };
    let mut file = Vec::new();
    iqdb_persist::format::write_header(&mut file, &header).unwrap();
    file.extend_from_slice(&raw);
    std::fs::write(&path, &file).unwrap();

    let restored: PersistedIndex<StoreIndex> =
        PersistedIndex::load(PersistConfig::new(&path)).unwrap();
    assert_eq!(restored.index().len(), 10);
    assert_eq!(restored.index().dim(), 4);
}
