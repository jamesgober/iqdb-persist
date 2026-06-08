//! Save an index to disk and load it back.
//!
//! `iqdb-persist` is generic over `I: Index + Persistable`, so this
//! example ships its own minimal in-memory index — a thin `Vec` of
//! `(VectorId, Arc<[f32]>)` rows — rather than pulling in a concrete
//! index crate. It shows the whole Tier-1 lifecycle: wrap, save, load,
//! and confirm the restored state matches.
//!
//! Run with:
//!
//! ```sh
//! cargo run --example save_and_load
//! ```

use std::io::{Read, Write};
use std::sync::Arc;

use iqdb_index::{Index, IndexCore, IndexStats};
use iqdb_persist::{PersistConfig, PersistError, Persistable, PersistedIndex, Result};
use iqdb_types::{DistanceMetric, Hit, Metadata, Result as IqdbResult, SearchParams, VectorId};

/// A toy in-memory index: just enough surface to be `Persistable`.
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
        // Self-describing payload: dim, metric tag, row count, then rows.
        let io = |source| PersistError::Io {
            path: std::path::PathBuf::new(),
            source,
        };
        w.write_all(&(self.dim as u64).to_le_bytes()).map_err(io)?;
        let tag: u8 = match self.metric {
            DistanceMetric::Cosine => 0,
            DistanceMetric::DotProduct => 1,
            DistanceMetric::Euclidean => 2,
            DistanceMetric::Manhattan => 3,
            DistanceMetric::Hamming => 4,
            _ => {
                return Err(PersistError::InvalidPayload {
                    reason: "example does not encode this metric",
                });
            }
        };
        w.write_all(&[tag]).map_err(io)?;
        w.write_all(&(self.rows.len() as u64).to_le_bytes())
            .map_err(io)?;
        for (id, v) in &self.rows {
            w.write_all(&id.to_le_bytes()).map_err(io)?;
            for component in v.iter() {
                w.write_all(&component.to_le_bytes()).map_err(io)?;
            }
        }
        Ok(())
    }

    fn load_from(r: &mut dyn Read) -> Result<Self> {
        let io = |source| PersistError::Io {
            path: std::path::PathBuf::new(),
            source,
        };
        let mut u64_buf = [0u8; 8];
        r.read_exact(&mut u64_buf).map_err(io)?;
        let dim = u64::from_le_bytes(u64_buf) as usize;

        let mut tag = [0u8; 1];
        r.read_exact(&mut tag).map_err(io)?;
        let metric = match tag[0] {
            0 => DistanceMetric::Cosine,
            1 => DistanceMetric::DotProduct,
            2 => DistanceMetric::Euclidean,
            3 => DistanceMetric::Manhattan,
            4 => DistanceMetric::Hamming,
            _ => {
                return Err(PersistError::InvalidPayload {
                    reason: "unknown metric tag in payload",
                });
            }
        };

        r.read_exact(&mut u64_buf).map_err(io)?;
        let n = u64::from_le_bytes(u64_buf) as usize;

        let mut rows = Vec::with_capacity(n);
        for _ in 0..n {
            r.read_exact(&mut u64_buf).map_err(io)?;
            let id = u64::from_le_bytes(u64_buf);
            let mut v = Vec::with_capacity(dim);
            let mut f_buf = [0u8; 4];
            for _ in 0..dim {
                r.read_exact(&mut f_buf).map_err(io)?;
                v.push(f32::from_le_bytes(f_buf));
            }
            rows.push((id, Arc::from(v.into_boxed_slice())));
        }
        Ok(Self { dim, metric, rows })
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join("iqdb-persist-example");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("vec.iqdb");

    // 1. Build and populate an index.
    let mut index = VecIndex::new(3, DistanceMetric::Cosine, ())?;
    index.insert(
        VectorId::from(1u64),
        Arc::<[f32]>::from(&[0.1, 0.2, 0.3][..]),
        None,
    )?;
    index.insert(
        VectorId::from(2u64),
        Arc::<[f32]>::from(&[0.4, 0.5, 0.6][..]),
        None,
    )?;

    // 2. Wrap and save atomically.
    let cfg = PersistConfig::new(&path);
    let wrapped = PersistedIndex::open_with(index, cfg.clone())?;
    wrapped.save()?;
    println!(
        "saved {} vectors to {}",
        wrapped.index().len(),
        path.display()
    );

    // 3. Load it back and confirm the state survived the round-trip.
    let restored: PersistedIndex<VecIndex> = PersistedIndex::load(cfg)?;
    println!(
        "loaded {} vectors (dim={}, metric={:?})",
        restored.index().len(),
        restored.index().dim(),
        restored.index().metric(),
    );
    assert_eq!(restored.index().len(), 2);

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}
