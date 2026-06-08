//! [`PersistedIndex`] — the snapshot lifecycle wrapper.
//!
//! Wraps an `I: Index + Persistable` with the framing (file header,
//! CRC32, atomic write) the [`crate::Persistable`] impl does not write
//! itself.

use std::io::Cursor;
use std::sync::Arc;

use iqdb_index::Index;
use iqdb_types::{Metadata, VectorId};

use crate::Persistable;
use crate::config::{Compression, PersistConfig};
use crate::error::{PersistError, Result};
use crate::format::{self, CURRENT_VERSION, FileHeader, MAGIC};
use crate::storage::{StdFsStorage, Storage};
use crate::wal::Wal;
use crate::{checksum, recovery};

/// A snapshot-persistent wrapper around an in-memory index.
///
/// Borrow the wrapped index with [`index`](PersistedIndex::index) /
/// [`index_mut`](PersistedIndex::index_mut) for queries and mutations;
/// call [`save`](PersistedIndex::save) to write the current state to
/// disk; call [`PersistedIndex::load`] later to recover it.
///
/// # Examples
///
/// ```no_run
/// # use iqdb_persist::{PersistConfig, PersistedIndex, Persistable};
/// # use iqdb_index::Index;
/// # fn demo<I: Index + Persistable>(inner: I) -> iqdb_persist::Result<()> {
/// let cfg = PersistConfig::new("snapshot.iqdb");
/// let wrapped = PersistedIndex::open_with(inner, cfg.clone())?;
/// wrapped.save()?;
///
/// // Later:
/// let restored: PersistedIndex<I> = PersistedIndex::load(cfg)?;
/// let _idx = restored.index();
/// # Ok(())
/// # }
/// ```
pub struct PersistedIndex<I: Index + Persistable> {
    inner: I,
    config: PersistConfig,
    storage: Box<dyn Storage>,
    // `Some` exactly when `config.wal_enabled`. Holds the live, append-
    // positioned write-ahead log; `None` in snapshot-only mode.
    wal: Option<Wal>,
}

// `Box<dyn Storage>` is not `Debug`, so we cannot `#[derive(Debug)]`.
// A manual impl that delegates to `I: Debug` is enough to let
// `Result<PersistedIndex<I>, _>::unwrap_err` work in tests, without
// requiring the trait to be `Debug` (which is an internal substrate).
impl<I: Index + Persistable + core::fmt::Debug> core::fmt::Debug for PersistedIndex<I> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PersistedIndex")
            .field("inner", &self.inner)
            .field("config", &self.config)
            .field("storage", &"<dyn Storage>")
            .field("wal", &self.wal.is_some())
            .finish()
    }
}

impl<I: Index + Persistable> PersistedIndex<I> {
    /// Wrap an already-constructed `inner`.
    ///
    /// In snapshot-only mode (`config.wal_enabled = false`) this performs
    /// no disk I/O — call [`save`](Self::save) when you want to write.
    /// With the WAL enabled it establishes the on-disk state immediately:
    /// it writes an initial snapshot (the base every later replay starts
    /// from) and opens a fresh write-ahead log beside it. Use this to
    /// start from an in-memory index; use [`load`](Self::load) to recover
    /// an existing one.
    ///
    /// # Errors
    ///
    /// - [`PersistError::Unsupported`] if `config` requests a feature this
    ///   build does not implement (any non-`None` compression).
    /// - With the WAL enabled, any [`save`](Self::save) error from writing
    ///   the initial snapshot, or [`PersistError::Io`] opening the WAL.
    pub fn open_with(inner: I, config: PersistConfig) -> Result<Self> {
        Self::open_with_storage(inner, config, Box::new(StdFsStorage))
    }

    /// Read `config.path` from disk and reconstruct the wrapped index. With
    /// the WAL enabled, every mutation logged after the snapshot is then
    /// replayed onto it, restoring the last acknowledged state.
    ///
    /// # Errors
    ///
    /// - [`PersistError::Io`] if the file cannot be read.
    /// - [`PersistError::BadMagic`] / [`PersistError::UnsupportedVersion`]
    ///   / [`PersistError::TruncatedHeader`] / [`PersistError::InvalidMetric`]
    ///   for header-level corruption.
    /// - [`PersistError::InvalidIndexType`] if `header.index_type` does
    ///   not equal `I::INDEX_TYPE` — asking for
    ///   `PersistedIndex::<FlatIndex>::load` on an HNSW file fails here.
    /// - [`PersistError::ChecksumMismatch`] if the payload CRC32 does
    ///   not match the header.
    /// - [`PersistError::InvalidPayload`] if the impl-reconstructed
    ///   index's `dim` / `metric` / `len` disagrees with the header, or a
    ///   WAL record disagrees with the index dimension.
    /// - [`PersistError::IndexBuild`] if the
    ///   [`Persistable::load_from`] impl, or a replayed mutation, returned
    ///   a downstream [`iqdb_types::IqdbError`].
    /// - [`PersistError::Unsupported`] if `config` requests an
    ///   unsupported feature (see [`open_with`](Self::open_with)).
    pub fn load(config: PersistConfig) -> Result<Self> {
        Self::load_with_storage(config, Box::new(StdFsStorage))
    }

    /// Build a `PersistedIndex` against a non-default [`Storage`]
    /// substrate. The snapshot path goes through `storage`; the WAL (when
    /// enabled) uses `std::fs` directly until the `storage-io` substrate
    /// lands in v0.5.
    pub(crate) fn open_with_storage(
        inner: I,
        config: PersistConfig,
        storage: Box<dyn Storage>,
    ) -> Result<Self> {
        validate_config(&config)?;
        let mut this = Self {
            inner,
            config,
            storage,
            wal: None,
        };
        if this.config.wal_enabled {
            // Establish a base snapshot so `load` always reconstructs from
            // real state, then start a fresh, empty WAL beside it.
            this.write_snapshot()?;
            let wal = Wal::create(&this.config.path, this.config.fsync_policy)?;
            this.wal = Some(wal);
        }
        Ok(this)
    }

    /// `load`, but reading the snapshot through `storage`.
    pub(crate) fn load_with_storage(
        config: PersistConfig,
        storage: Box<dyn Storage>,
    ) -> Result<Self> {
        validate_config(&config)?;
        let bytes = storage.read_all(&config.path)?;
        let mut cursor = Cursor::new(&bytes[..]);
        let header = format::read_header(&mut cursor)?;

        let header_end =
            usize::try_from(cursor.position()).map_err(|_| PersistError::InvalidPayload {
                reason: "header position does not fit in usize",
            })?;
        if header_end > bytes.len() {
            return Err(PersistError::TruncatedHeader {
                needed: header_end,
                found: bytes.len(),
            });
        }

        // Guard #1 cross-check: caller's I must match the file's tag.
        if header.index_type != I::INDEX_TYPE {
            return Err(PersistError::InvalidIndexType {
                found: header.index_type,
                expected: I::INDEX_TYPE,
            });
        }

        let payload = &bytes[header_end..];
        checksum::verify(payload, header.crc32)?;

        let mut payload_cursor = Cursor::new(payload);
        let inner = <I as Persistable>::load_from(&mut payload_cursor)?;

        if inner.dim() != header.dim {
            return Err(PersistError::InvalidPayload {
                reason: "header dim disagrees with payload-reconstructed index",
            });
        }
        if inner.metric() != header.metric {
            return Err(PersistError::InvalidPayload {
                reason: "header metric disagrees with payload-reconstructed index",
            });
        }
        if inner.len() != header.n_vectors {
            return Err(PersistError::InvalidPayload {
                reason: "header n_vectors disagrees with payload-reconstructed index",
            });
        }

        let mut this = Self {
            inner,
            config,
            storage,
            wal: None,
        };
        if this.config.wal_enabled {
            // Replay the deltas the WAL recorded after this snapshot, then
            // re-open the same WAL for continued appends at its end.
            let _applied = recovery::replay(&this.config.path, &mut this.inner)?;
            let wal = Wal::open_for_append(&this.config.path, this.config.fsync_policy)?;
            this.wal = Some(wal);
        }
        Ok(this)
    }

    /// Borrow the wrapped index for queries.
    #[must_use]
    pub fn index(&self) -> &I {
        &self.inner
    }

    /// Borrow the wrapped index mutably for direct inserts / deletes /
    /// flush.
    ///
    /// **WAL note:** mutations made through this borrow **bypass the
    /// write-ahead log** — they are not logged and will not survive a crash
    /// until the next [`checkpoint`](Self::checkpoint). In WAL mode prefer
    /// [`insert`](Self::insert) / [`delete`](Self::delete), which log
    /// before applying.
    pub fn index_mut(&mut self) -> &mut I {
        &mut self.inner
    }

    /// The [`PersistConfig`] this wrapper was constructed with.
    #[must_use]
    pub fn config(&self) -> &PersistConfig {
        &self.config
    }

    /// Apply an insert durably: with the WAL enabled, the mutation is
    /// **logged and `fsync`ed before** it is applied in memory, so an
    /// acknowledged insert survives a crash and is restored on the next
    /// [`load`](Self::load). In snapshot-only mode it applies to memory
    /// directly (durability comes from the next [`save`](Self::save)).
    ///
    /// If the in-memory apply is rejected (for example a duplicate id), the
    /// just-logged record is rolled back so the WAL stays exactly in step
    /// with the index.
    ///
    /// # Errors
    ///
    /// - [`PersistError::Io`] if the WAL append or `fsync` fails.
    /// - [`PersistError::IndexBuild`] if the index rejects the insert.
    /// - [`PersistError::InvalidPayload`] if the vector length does not fit
    ///   in `u32`.
    pub fn insert(
        &mut self,
        id: VectorId,
        vector: Arc<[f32]>,
        meta: Option<Metadata>,
    ) -> Result<()> {
        let Self { inner, wal, .. } = self;
        match wal {
            Some(w) => {
                let mark = w.mark()?;
                w.append_insert(&id, &vector, meta.as_ref())?;
                match inner.insert(id, vector, meta) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        w.rollback(mark)?;
                        Err(PersistError::from(e))
                    }
                }
            }
            None => {
                inner.insert(id, vector, meta)?;
                Ok(())
            }
        }
    }

    /// Apply a delete durably — the [`insert`](Self::insert) contract, for
    /// removals.
    ///
    /// # Errors
    ///
    /// - [`PersistError::Io`] if the WAL append or `fsync` fails.
    /// - [`PersistError::IndexBuild`] if the index rejects the delete (for
    ///   example an absent id).
    pub fn delete(&mut self, id: &VectorId) -> Result<()> {
        let Self { inner, wal, .. } = self;
        match wal {
            Some(w) => {
                let mark = w.mark()?;
                w.append_delete(id)?;
                match inner.delete(id) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        w.rollback(mark)?;
                        Err(PersistError::from(e))
                    }
                }
            }
            None => {
                inner.delete(id)?;
                Ok(())
            }
        }
    }

    /// Write a snapshot of the current state to `self.config.path`
    /// atomically.
    ///
    /// In WAL mode this does **not** truncate the log — prefer
    /// [`checkpoint`](Self::checkpoint), which writes a snapshot *and*
    /// resets the WAL so the two never double-count a mutation on the next
    /// [`load`](Self::load).
    ///
    /// # Errors
    ///
    /// - [`PersistError::Io`] if the temp write, rename, or directory
    ///   fsync fails.
    /// - Any error returned by [`Persistable::save_to`].
    /// - [`PersistError::InvalidPayload`] if a `usize` field of the
    ///   index does not fit in `u64`.
    pub fn save(&self) -> Result<()> {
        self.write_snapshot()
    }

    /// Write a fresh snapshot and reset the WAL to empty — the WAL-mode
    /// durability-compaction operation.
    ///
    /// After a checkpoint the snapshot alone captures the full state, so
    /// the log can be truncated; this bounds WAL growth. In snapshot-only
    /// mode it is equivalent to [`save`](Self::save).
    ///
    /// # Errors
    ///
    /// The [`save`](Self::save) errors, plus [`PersistError::Io`] if
    /// truncating or re-initialising the WAL fails.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.write_snapshot()?;
        if let Some(wal) = &mut self.wal {
            wal.reset()?;
        }
        Ok(())
    }

    /// The shared snapshot writer behind [`save`](Self::save) /
    /// [`checkpoint`](Self::checkpoint) and the WAL-mode initial snapshot.
    #[tracing::instrument(level = "debug", skip_all, fields(
        path = %self.config.path.display(),
        index_type = I::INDEX_TYPE,
        n = self.inner.len(),
    ))]
    fn write_snapshot(&self) -> Result<()> {
        let mut payload_buf: Vec<u8> = Vec::new();
        <I as Persistable>::save_to(&self.inner, &mut payload_buf)?;

        let crc32 = checksum::compute(&payload_buf);
        let header = FileHeader {
            magic: MAGIC,
            version: CURRENT_VERSION,
            index_type: I::INDEX_TYPE.to_string(),
            dim: self.inner.dim(),
            metric: self.inner.metric(),
            n_vectors: self.inner.len(),
            crc32,
        };

        let mut full: Vec<u8> = Vec::with_capacity(payload_buf.len() + 64);
        format::write_header(&mut full, &header)?;
        full.extend_from_slice(&payload_buf);

        self.storage
            .write_atomic(&self.config.path, &full, self.config.fsync_policy)
    }
}

fn validate_config(config: &PersistConfig) -> Result<()> {
    if !matches!(config.compression, Compression::None) {
        return Err(PersistError::Unsupported {
            feature: "compression",
            available_in: "v0.4",
        });
    }
    Ok(())
}

// ----------------------------------------------------------------------
// Unit tests — exercise the framing logic against a tiny in-crate
// `MockIndex` so iqdb-persist never dev-deps iqdb-flat. Atomicity uses
// a FailingStorage substrate (the pub(crate) test seam exposed above).
// ----------------------------------------------------------------------
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::io::{Read, Write};
    use std::sync::Arc;

    use iqdb_index::{Index, IndexCore, IndexStats};
    use iqdb_types::{DistanceMetric, Hit, Metadata, Result as IqdbResult, SearchParams, VectorId};

    use super::*;
    use crate::format::{metric_to_tag, tag_to_metric};

    // -- mock index ------------------------------------------------------

    #[derive(Debug)]
    struct MockIndex {
        dim: usize,
        metric: DistanceMetric,
        n: usize,
    }

    impl IndexCore for MockIndex {
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
                index_type: "mock",
                ..IndexStats::default()
            }
        }
    }

    impl Index for MockIndex {
        type Config = ();
        fn new(dim: usize, metric: DistanceMetric, _: ()) -> IqdbResult<Self> {
            Ok(Self { dim, metric, n: 0 })
        }
    }

    impl Persistable for MockIndex {
        const INDEX_TYPE: &'static str = "mock";

        fn save_to(&self, writer: &mut dyn Write) -> Result<()> {
            // Self-describing prefix: metric_tag u8, dim u64 LE, n u64 LE.
            writer
                .write_all(&[metric_to_tag(self.metric)?])
                .map_err(io_err)?;
            let dim_u64 = u64::try_from(self.dim).map_err(|_| PersistError::InvalidPayload {
                reason: "mock dim does not fit in u64",
            })?;
            writer.write_all(&dim_u64.to_le_bytes()).map_err(io_err)?;
            let n_u64 = u64::try_from(self.n).map_err(|_| PersistError::InvalidPayload {
                reason: "mock n does not fit in u64",
            })?;
            writer.write_all(&n_u64.to_le_bytes()).map_err(io_err)?;
            Ok(())
        }

        fn load_from(reader: &mut dyn Read) -> Result<Self> {
            let mut tag = [0u8; 1];
            reader.read_exact(&mut tag).map_err(io_err)?;
            let metric = tag_to_metric(tag[0])?;
            let mut buf = [0u8; 8];
            reader.read_exact(&mut buf).map_err(io_err)?;
            let dim = usize::try_from(u64::from_le_bytes(buf)).map_err(|_| {
                PersistError::InvalidPayload {
                    reason: "mock dim does not fit in usize",
                }
            })?;
            reader.read_exact(&mut buf).map_err(io_err)?;
            let n = usize::try_from(u64::from_le_bytes(buf)).map_err(|_| {
                PersistError::InvalidPayload {
                    reason: "mock n does not fit in usize",
                }
            })?;
            Ok(Self { dim, metric, n })
        }
    }

    fn io_err(source: std::io::Error) -> PersistError {
        PersistError::Io {
            path: std::path::PathBuf::new(),
            source,
        }
    }

    // -- failing storage seam -------------------------------------------

    /// A `Storage` that succeeds on read but always fails the rename
    /// leg of `write_atomic`, while doing the preceding temp-write +
    /// fsync correctly. Used to prove atomicity: the target on disk
    /// must be left untouched.
    struct FailingRenameStorage;

    impl Storage for FailingRenameStorage {
        fn read_all(&self, path: &std::path::Path) -> Result<Vec<u8>> {
            StdFsStorage.read_all(path)
        }

        fn write_atomic(
            &self,
            target: &std::path::Path,
            payload: &[u8],
            _policy: crate::config::FsyncPolicy,
        ) -> Result<()> {
            use std::fs::OpenOptions;

            let target_dir = target.parent().unwrap_or_else(|| std::path::Path::new("."));
            let file_name = target.file_name().unwrap();
            let temp_path = target_dir.join(format!(
                "{}.tmp.failtest.{}",
                file_name.to_string_lossy(),
                std::process::id(),
            ));
            {
                let mut f = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&temp_path)
                    .map_err(|source| PersistError::Io {
                        path: temp_path.clone(),
                        source,
                    })?;
                f.write_all(payload).map_err(|source| PersistError::Io {
                    path: temp_path.clone(),
                    source,
                })?;
                f.sync_all().map_err(|source| PersistError::Io {
                    path: temp_path.clone(),
                    source,
                })?;
            }
            let _cleanup = std::fs::remove_file(&temp_path);
            Err(PersistError::Io {
                path: target.to_path_buf(),
                source: std::io::Error::other("simulated rename failure"),
            })
        }
    }

    // -- the atomicity test ---------------------------------------------

    #[test]
    fn save_failure_leaves_original_file_intact() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("idx.iqdb");

        // 1) Save a "good" snapshot with the real storage.
        let inner = MockIndex {
            dim: 16,
            metric: DistanceMetric::Cosine,
            n: 7,
        };
        let cfg = PersistConfig::new(&snapshot);
        let wrap = PersistedIndex::open_with(inner, cfg.clone()).unwrap();
        wrap.save().unwrap();

        let good_bytes = std::fs::read(&snapshot).unwrap();
        assert!(!good_bytes.is_empty(), "good save produced empty file");

        // 2) Try to save a *different* index using the failing-rename
        //    storage. The save MUST error.
        let other = MockIndex {
            dim: 16,
            metric: DistanceMetric::Cosine,
            n: 99,
        };
        let wrap2 =
            PersistedIndex::open_with_storage(other, cfg.clone(), Box::new(FailingRenameStorage))
                .unwrap();
        let err = wrap2.save().unwrap_err();
        assert!(matches!(err, PersistError::Io { .. }));

        // 3) The on-disk bytes MUST equal the original good save.
        let after_bytes = std::fs::read(&snapshot).unwrap();
        assert_eq!(
            after_bytes, good_bytes,
            "rename failure corrupted the snapshot"
        );

        // 4) The original must still load and report n = 7, not 99.
        let restored: PersistedIndex<MockIndex> = PersistedIndex::load(cfg).unwrap();
        assert_eq!(restored.index().len(), 7);
    }

    #[test]
    fn validate_config_rejects_compression() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("idx.iqdb");

        // WAL is supported as of v0.3 — enabling it must succeed.
        let mut cfg = PersistConfig::new(&snapshot);
        cfg.wal_enabled = true;
        let inner = MockIndex {
            dim: 4,
            metric: DistanceMetric::Euclidean,
            n: 0,
        };
        assert!(PersistedIndex::open_with(inner, cfg).is_ok());

        // Compression still lands later and is rejected at construction.
        let mut cfg2 = PersistConfig::new(&snapshot);
        cfg2.compression = Compression::Lz4;
        let inner2 = MockIndex {
            dim: 4,
            metric: DistanceMetric::Euclidean,
            n: 0,
        };
        let err = PersistedIndex::open_with(inner2, cfg2).unwrap_err();
        assert!(matches!(
            err,
            PersistError::Unsupported {
                feature: "compression",
                ..
            }
        ));
    }

    #[test]
    fn crc_mismatch_after_byte_flip_in_payload() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("idx.iqdb");

        // 1) Save a good snapshot.
        let inner = MockIndex {
            dim: 8,
            metric: DistanceMetric::Cosine,
            n: 11,
        };
        let cfg = PersistConfig::new(&snapshot);
        PersistedIndex::open_with(inner, cfg.clone())
            .unwrap()
            .save()
            .unwrap();

        // 2) Read the bytes, flip a bit in the LAST byte (well inside
        //    the payload region — the mock payload is 17 bytes long,
        //    and the header is much larger than that).
        let mut bytes = std::fs::read(&snapshot).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        std::fs::write(&snapshot, &bytes).unwrap();

        // 3) Load MUST surface ChecksumMismatch — not a panic, not a
        //    silently-wrong load.
        let err: PersistError = PersistedIndex::<MockIndex>::load(cfg).unwrap_err();
        assert!(
            matches!(err, PersistError::ChecksumMismatch { .. }),
            "expected ChecksumMismatch, got {err:?}",
        );
    }

    #[test]
    fn invalid_index_type_on_wrong_i_surfaces_loudly() {
        // Save a file with INDEX_TYPE = "mock", then try to load it
        // as if its tag were "other".
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("idx.iqdb");
        let cfg = PersistConfig::new(&snapshot);

        let inner = MockIndex {
            dim: 4,
            metric: DistanceMetric::Euclidean,
            n: 3,
        };
        PersistedIndex::open_with(inner, cfg.clone())
            .unwrap()
            .save()
            .unwrap();

        // A second mock with a different INDEX_TYPE.
        #[derive(Debug)]
        struct OtherMock;
        impl IndexCore for OtherMock {
            fn insert(
                &mut self,
                _: VectorId,
                _: Arc<[f32]>,
                _: Option<Metadata>,
            ) -> IqdbResult<()> {
                Ok(())
            }
            fn delete(&mut self, _: &VectorId) -> IqdbResult<()> {
                Ok(())
            }
            fn search(&self, _: &[f32], _: &SearchParams) -> IqdbResult<Vec<Hit>> {
                Ok(Vec::new())
            }
            fn len(&self) -> usize {
                0
            }
            fn dim(&self) -> usize {
                4
            }
            fn metric(&self) -> DistanceMetric {
                DistanceMetric::Euclidean
            }
            fn flush(&mut self) -> IqdbResult<()> {
                Ok(())
            }
            fn stats(&self) -> IndexStats {
                IndexStats {
                    index_type: "other",
                    ..IndexStats::default()
                }
            }
        }
        impl Index for OtherMock {
            type Config = ();
            fn new(_: usize, _: DistanceMetric, _: ()) -> IqdbResult<Self> {
                Ok(Self)
            }
        }
        impl Persistable for OtherMock {
            const INDEX_TYPE: &'static str = "other";
            fn save_to(&self, _w: &mut dyn Write) -> Result<()> {
                Ok(())
            }
            fn load_from(_r: &mut dyn Read) -> Result<Self> {
                Ok(Self)
            }
        }

        let err = PersistedIndex::<OtherMock>::load(cfg).unwrap_err();
        assert!(
            matches!(
                err,
                PersistError::InvalidIndexType {
                    expected: "other",
                    ..
                }
            ),
            "expected InvalidIndexType, got {err:?}",
        );
    }

    #[test]
    fn roundtrip_through_storage_recovers_state() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("idx.iqdb");
        let cfg = PersistConfig::new(&snapshot);

        let inner = MockIndex {
            dim: 32,
            metric: DistanceMetric::Manhattan,
            n: 42,
        };
        let wrap = PersistedIndex::open_with(inner, cfg.clone()).unwrap();
        wrap.save().unwrap();

        let restored: PersistedIndex<MockIndex> = PersistedIndex::load(cfg).unwrap();
        assert_eq!(restored.index().dim(), 32);
        assert_eq!(restored.index().metric(), DistanceMetric::Manhattan);
        assert_eq!(restored.index().len(), 42);
    }
}
