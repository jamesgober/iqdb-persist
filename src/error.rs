//! The iqdb-persist domain error.
//!
//! [`PersistError`] names every failure mode the persistence layer can
//! surface. It mirrors [`iqdb_types::IqdbError`]'s shape (non-exhaustive
//! enum, one variant per failure, [`error_forge::ForgeError`]
//! integration) so the two errors compose into the same operator-facing
//! structured-error events.
//!
//! Unlike `IqdbError`, this type is **not** `Copy` or `Clone`: the `Io`
//! variant wraps a `std::io::Error` (which implements neither) and the
//! `InvalidIndexType` variant carries an owned `String` for the tag the
//! header surfaced.

use std::path::PathBuf;

use error_forge::ForgeError;
use iqdb_types::{DistanceMetric, IqdbError};

/// An error from an `iqdb-persist` save, load, or format operation.
///
/// Each variant identifies one specific failure. The enum is
/// `#[non_exhaustive]`: future releases may add variants without it
/// being a breaking change, so a `match` on it must include a wildcard
/// arm.
///
/// # Examples
///
/// ```
/// use iqdb_persist::PersistError;
///
/// let err = PersistError::ChecksumMismatch { expected: 0xDEADBEEF, computed: 0x00000000 };
/// assert!(err.to_string().contains("checksum mismatch"));
///
/// let unsup = PersistError::Unsupported { feature: "wal_enabled", available_in: "v0.3" };
/// assert!(unsup.to_string().contains("v0.3"));
/// ```
#[non_exhaustive]
#[derive(Debug)]
pub enum PersistError {
    /// An OS-level I/O failure occurred while reading or writing a
    /// snapshot file. `path` is the file whose operation failed;
    /// `source` is the underlying `std::io::Error` and is reachable via
    /// [`std::error::Error::source`].
    Io {
        /// The path whose I/O operation failed.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The first eight bytes of the file did not match
    /// [`crate::MAGIC`] — the file is not an iqdb snapshot.
    BadMagic {
        /// The eight magic bytes actually read from the file.
        found: [u8; 8],
    },
    /// The header's format-version field is not one this build supports.
    /// `found` is what the file declared; `supported` is the version this
    /// build writes.
    UnsupportedVersion {
        /// The version the file declared.
        found: u32,
        /// The format version this build supports.
        supported: u32,
    },
    /// The CRC32 of the payload bytes did not match the header's stored
    /// value — the payload is corrupted or has been tampered with.
    ChecksumMismatch {
        /// The CRC32 the header claimed.
        expected: u32,
        /// The CRC32 actually computed over the payload bytes.
        computed: u32,
    },
    /// The file ended before the full header could be read. `needed` is
    /// the number of bytes the parser still wanted; `found` is how many
    /// were available.
    TruncatedHeader {
        /// Bytes the parser still needed.
        needed: usize,
        /// Bytes that were available.
        found: usize,
    },
    /// The file ended before the full payload could be read.
    TruncatedPayload {
        /// Payload bytes the parser still needed.
        needed: u64,
        /// Payload bytes that were available.
        found: u64,
    },
    /// The header's metric tag does not correspond to any
    /// [`iqdb_types::DistanceMetric`] variant this build knows about.
    InvalidMetric {
        /// The on-disk metric tag byte.
        tag: u8,
    },
    /// A [`DistanceMetric`] this build has no on-disk tag for. Only
    /// occurs on save if a newer `iqdb-types` introduced a metric
    /// variant that this build of `iqdb-persist` predates —
    /// `DistanceMetric` is `#[non_exhaustive]`.
    UnsupportedMetric {
        /// The metric that could not be encoded.
        metric: DistanceMetric,
    },
    /// The header's index-type tag does not match the concrete `I`'s
    /// [`crate::Persistable::INDEX_TYPE`].
    InvalidIndexType {
        /// The index-type tag the file declared.
        found: String,
        /// The index-type tag the caller's `I` requires.
        expected: &'static str,
    },
    /// The payload bytes decoded successfully at the byte level but
    /// produced a structurally invalid index.
    InvalidPayload {
        /// Short, stable identifier for the structural check that failed.
        reason: &'static str,
    },
    /// A nested [`IqdbError`] surfaced from a downstream construction
    /// step — typically [`iqdb_index::Index::new`] or
    /// [`iqdb_index::IndexCore::insert`] called from inside a
    /// [`crate::Persistable::load_from`] impl.
    IndexBuild(IqdbError),
    /// A configuration value asked for a feature that this build does
    /// not implement yet.
    Unsupported {
        /// Short, stable identifier for the unsupported feature.
        feature: &'static str,
        /// The version where the feature lands.
        available_in: &'static str,
    },
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "I/O error on {}: {source}", path.display())
            }
            Self::BadMagic { found } => {
                write!(f, "bad magic: not an iqdb snapshot (found {found:?})")
            }
            Self::UnsupportedVersion { found, supported } => {
                write!(
                    f,
                    "unsupported format version: found {found}, supported {supported}",
                )
            }
            Self::ChecksumMismatch { expected, computed } => {
                write!(
                    f,
                    "checksum mismatch: header expected {expected:#010x}, computed {computed:#010x}",
                )
            }
            Self::TruncatedHeader { needed, found } => {
                write!(f, "truncated header: needed {needed} bytes, found {found}")
            }
            Self::TruncatedPayload { needed, found } => {
                write!(f, "truncated payload: needed {needed} bytes, found {found}")
            }
            Self::InvalidMetric { tag } => {
                write!(f, "invalid metric tag: {tag}")
            }
            Self::UnsupportedMetric { metric } => {
                write!(f, "unsupported metric for this build: {metric:?}")
            }
            Self::InvalidIndexType { found, expected } => {
                write!(
                    f,
                    "index type mismatch: file declared {found:?}, caller expected {expected:?}",
                )
            }
            Self::InvalidPayload { reason } => {
                write!(f, "invalid payload: {reason}")
            }
            Self::IndexBuild(e) => write!(f, "index construction failed: {e}"),
            Self::Unsupported {
                feature,
                available_in,
            } => {
                write!(
                    f,
                    "feature not supported in this build: {feature} (available in {available_in})",
                )
            }
        }
    }
}

impl std::error::Error for PersistError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::IndexBuild(e) => Some(e),
            _ => None,
        }
    }
}

impl ForgeError for PersistError {
    fn kind(&self) -> &'static str {
        match self {
            Self::Io { .. } => "Io",
            Self::BadMagic { .. } => "BadMagic",
            Self::UnsupportedVersion { .. } => "UnsupportedVersion",
            Self::ChecksumMismatch { .. } => "ChecksumMismatch",
            Self::TruncatedHeader { .. } => "TruncatedHeader",
            Self::TruncatedPayload { .. } => "TruncatedPayload",
            Self::InvalidMetric { .. } => "InvalidMetric",
            Self::UnsupportedMetric { .. } => "UnsupportedMetric",
            Self::InvalidIndexType { .. } => "InvalidIndexType",
            Self::InvalidPayload { .. } => "InvalidPayload",
            Self::IndexBuild(_) => "IndexBuild",
            Self::Unsupported { .. } => "Unsupported",
        }
    }

    fn caption(&self) -> &'static str {
        match self {
            Self::Io { .. } => "OS-level I/O failure on a snapshot file",
            Self::BadMagic { .. } => "file is not an iqdb snapshot",
            Self::UnsupportedVersion { .. } => {
                "snapshot format version is not supported by this build"
            }
            Self::ChecksumMismatch { .. } => "payload CRC32 does not match the header",
            Self::TruncatedHeader { .. } => "file ended before the full header could be read",
            Self::TruncatedPayload { .. } => "file ended before the full payload could be read",
            Self::InvalidMetric { .. } => {
                "metric tag does not correspond to any known distance metric"
            }
            Self::UnsupportedMetric { .. } => "distance metric has no on-disk tag in this build",
            Self::InvalidIndexType { .. } => {
                "header's index-type tag does not match the caller's I"
            }
            Self::InvalidPayload { .. } => "payload bytes decoded to a structurally invalid index",
            Self::IndexBuild(_) => "a downstream Index::new or insert returned an error",
            Self::Unsupported { .. } => {
                "the requested feature lands in a later version of iqdb-persist"
            }
        }
    }
}

impl From<IqdbError> for PersistError {
    fn from(value: IqdbError) -> Self {
        Self::IndexBuild(value)
    }
}

/// A specialized [`Result`](core::result::Result) whose error is
/// [`PersistError`].
///
/// # Examples
///
/// ```
/// use iqdb_persist::{PersistError, Result};
///
/// fn need_wal_off(wal_enabled: bool) -> Result<()> {
///     if wal_enabled {
///         return Err(PersistError::Unsupported {
///             feature: "wal_enabled",
///             available_in: "v0.3",
///         });
///     }
///     Ok(())
/// }
///
/// assert!(need_wal_off(true).is_err());
/// assert!(need_wal_off(false).is_ok());
/// ```
pub type Result<T> = core::result::Result<T, PersistError>;
