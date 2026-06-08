//! Public configuration types for the persistence layer.
//!
//! [`PersistConfig`] is what a caller hands to [`crate::PersistedIndex`].
//! v0.2 implements the **snapshot** subset of the surface: file path,
//! fsync policy, and `Compression::None`. The WAL knob and the
//! compression variants are present so the v0.3 / v0.4 wiring lands
//! without an API break; v0.2 rejects them at construction with a clear
//! [`crate::PersistError::Unsupported`] rather than panicking or
//! silently no-oping.

use std::path::PathBuf;
use std::time::Duration;

/// How aggressively the persistence layer fsyncs to durable storage.
///
/// v0.2 honors `Always` and `Never` on snapshot save. `Periodic` is
/// WAL-specific and is treated as `Always` for snapshot writes in v0.2.
///
/// # Examples
///
/// ```
/// use iqdb_persist::FsyncPolicy;
/// use std::time::Duration;
///
/// let _ = FsyncPolicy::Always;
/// let _ = FsyncPolicy::Periodic(Duration::from_secs(1));
/// let _ = FsyncPolicy::Never;
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum FsyncPolicy {
    /// fsync every write.
    Always,
    /// fsync no more often than this interval (WAL-specific in v0.3+).
    Periodic(Duration),
    /// Never fsync. Fastest, weakest durability — appropriate for tests
    /// and tmpfs-backed paths only.
    Never,
}

/// Compression applied to the payload bytes on save.
///
/// v0.2 ships `None`. The other variants are present so the
/// `PersistConfig` shape does not change when compression lands; in
/// v0.2 they return [`crate::PersistError::Unsupported`] at
/// [`crate::PersistedIndex`] construction.
///
/// # Examples
///
/// ```
/// use iqdb_persist::Compression;
///
/// let none = Compression::None;
/// let zstd = Compression::Zstd { level: 3 };
/// let lz4 = Compression::Lz4;
/// let _ = (none, zstd, lz4);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Compression {
    /// No compression. The only variant honored in v0.2.
    None,
    /// Zstd compression at the given level. Lands in v0.4.
    Zstd {
        /// The Zstd compression level (1–22).
        level: i32,
    },
    /// LZ4 compression. Lands in v0.4.
    Lz4,
}

/// Configuration for [`crate::PersistedIndex`].
///
/// Construct with [`PersistConfig::new`] (recommended) and override the
/// fields you want, or start from [`PersistConfig::default`] for a
/// no-path placeholder + sensible knobs.
///
/// # Examples
///
/// ```
/// use iqdb_persist::{Compression, FsyncPolicy, PersistConfig};
/// use std::path::PathBuf;
///
/// let cfg = PersistConfig::new("/tmp/my-snapshot.iqdb");
/// assert_eq!(cfg.path, PathBuf::from("/tmp/my-snapshot.iqdb"));
/// assert_eq!(cfg.fsync_policy, FsyncPolicy::Always);
/// assert_eq!(cfg.compression, Compression::None);
/// assert!(!cfg.wal_enabled);
/// ```
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PersistConfig {
    /// Path to the snapshot file on disk.
    pub path: PathBuf,
    /// Whether the WAL is enabled. v0.2 rejects `true` with
    /// [`crate::PersistError::Unsupported`]; the API is in place for
    /// v0.3.
    pub wal_enabled: bool,
    /// How aggressively to fsync.
    pub fsync_policy: FsyncPolicy,
    /// Payload compression. v0.2 rejects non-`None` with
    /// [`crate::PersistError::Unsupported`]; the API is in place for
    /// v0.4.
    pub compression: Compression,
}

impl PersistConfig {
    /// Build a config with `path` and v0.2 defaults
    /// (`wal_enabled = false`, `fsync_policy = Always`,
    /// `compression = None`).
    ///
    /// # Examples
    ///
    /// ```
    /// use iqdb_persist::PersistConfig;
    ///
    /// let cfg = PersistConfig::new("snapshot.iqdb");
    /// assert_eq!(cfg.path.file_name().unwrap(), "snapshot.iqdb");
    /// ```
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            wal_enabled: false,
            fsync_policy: FsyncPolicy::Always,
            compression: Compression::None,
        }
    }
}

impl Default for PersistConfig {
    /// Returns a config with an empty path and v0.2 defaults. The empty
    /// path is a placeholder — callers MUST set
    /// [`path`](PersistConfig::path) before passing the config to
    /// [`crate::PersistedIndex::save`] or
    /// [`crate::PersistedIndex::load`].
    ///
    /// # Examples
    ///
    /// ```
    /// use iqdb_persist::{Compression, FsyncPolicy, PersistConfig};
    ///
    /// let cfg = PersistConfig {
    ///     path: "snapshot.iqdb".into(),
    ///     ..PersistConfig::default()
    /// };
    /// assert_eq!(cfg.fsync_policy, FsyncPolicy::Always);
    /// assert_eq!(cfg.compression, Compression::None);
    /// ```
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            wal_enabled: false,
            fsync_policy: FsyncPolicy::Always,
            compression: Compression::None,
        }
    }
}
