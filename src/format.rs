//! The on-disk file header and wire format.
//!
//! The header is the first thing in a snapshot file. Its layout is
//! **strict little-endian, fixed-width**:
//!
//! ```text
//! offset  bytes  field
//! 0       8      magic ("IQDBPRST")
//! 8       4      version (u32 LE)
//! 12      8      index_type length (u64 LE)
//! 20      N      index_type (UTF-8, N bytes)
//! 20+N    8      dim (u64 LE)
//! 28+N    1      metric tag (u8)
//! 29+N    8      n_vectors (u64 LE)
//! 37+N    4      crc32 (u32 LE) -- of the payload only
//! 41+N    ...    payload (impl-defined)
//! ```
//!
//! Sizes (`index_type` length, `dim`, `n_vectors`) are always serialized
//! as fixed-width `u64`, never as the host's `usize`. This keeps the
//! format portable across 32- and 64-bit hosts.
//!
//! ## Metric tag values (on-disk contract)
//!
//! - `0` — Cosine
//! - `1` — DotProduct
//! - `2` — Euclidean
//! - `3` — Manhattan
//! - `4` — Hamming
//!
//! These values are part of the on-disk format contract. Once snapshot
//! files exist on disk with a given tag → metric mapping, the mapping
//! cannot change without a format-version bump.

use std::io::{Read, Write};

use iqdb_types::DistanceMetric;

use crate::error::{PersistError, Result};

/// Magic bytes that prefix every iqdb snapshot file.
///
/// # Examples
///
/// ```
/// assert_eq!(&iqdb_persist::MAGIC, b"IQDBPRST");
/// ```
pub const MAGIC: [u8; 8] = *b"IQDBPRST";

/// The on-disk format version this build writes.
///
/// Version `1` (v0.2–v0.3) stored the payload verbatim. Version `2` (v0.4+)
/// prefixes the payload region with a compression preamble; version-1 files
/// are still read (as uncompressed). The format is not frozen until v0.5.
///
/// # Examples
///
/// ```
/// assert_eq!(iqdb_persist::CURRENT_VERSION, 2);
/// ```
pub const CURRENT_VERSION: u32 = 2;

/// The oldest on-disk format version this build can still read.
pub(crate) const MIN_SUPPORTED_VERSION: u32 = 1;

/// The header at the start of every iqdb snapshot file.
///
/// The on-disk representation is fixed-width little-endian — see the
/// module-level docs for the byte-level layout. The Rust struct stores
/// `dim` and `n_vectors` as `usize` for ergonomic in-memory use; the
/// reader and writer convert to/from `u64` at the wire boundary.
///
/// `crc32` is the CRC32 of the **payload bytes only** — it does not
/// cover the header.
///
/// # Examples
///
/// ```
/// use iqdb_persist::{FileHeader, CURRENT_VERSION, MAGIC};
/// use iqdb_types::DistanceMetric;
///
/// let header = FileHeader {
///     magic: MAGIC,
///     version: CURRENT_VERSION,
///     index_type: "flat".to_string(),
///     dim: 128,
///     metric: DistanceMetric::Cosine,
///     n_vectors: 1_000,
///     crc32: 0xDEADBEEF,
/// };
/// assert_eq!(header.index_type, "flat");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHeader {
    /// Magic bytes — always equal to [`MAGIC`].
    pub magic: [u8; 8],
    /// On-disk format version. The reader accepts any version in
    /// `MIN_SUPPORTED_VERSION..=CURRENT_VERSION` and records which one it
    /// read here, so the payload can be decoded per that version.
    pub version: u32,
    /// Stable index-type tag — matched against
    /// [`crate::Persistable::INDEX_TYPE`] on load.
    pub index_type: String,
    /// Dimensionality of the vectors stored in the payload.
    pub dim: usize,
    /// Distance metric the index was built for.
    pub metric: DistanceMetric,
    /// Number of vectors stored in the payload.
    pub n_vectors: usize,
    /// CRC32 of the payload bytes (not of the header).
    pub crc32: u32,
}

/// Convert a [`DistanceMetric`] to its stable on-disk tag byte.
///
/// `DistanceMetric` is `#[non_exhaustive]`; a metric this build of
/// `iqdb-persist` predates has no assigned tag and yields
/// [`PersistError::UnsupportedMetric`] rather than a silently-wrong byte.
pub(crate) fn metric_to_tag(metric: DistanceMetric) -> Result<u8> {
    Ok(match metric {
        DistanceMetric::Cosine => 0,
        DistanceMetric::DotProduct => 1,
        DistanceMetric::Euclidean => 2,
        DistanceMetric::Manhattan => 3,
        DistanceMetric::Hamming => 4,
        _ => return Err(PersistError::UnsupportedMetric { metric }),
    })
}

/// Convert an on-disk tag byte back to a [`DistanceMetric`].
///
/// Returns [`PersistError::InvalidMetric`] for any value not in `0..=4`.
pub(crate) fn tag_to_metric(tag: u8) -> Result<DistanceMetric> {
    match tag {
        0 => Ok(DistanceMetric::Cosine),
        1 => Ok(DistanceMetric::DotProduct),
        2 => Ok(DistanceMetric::Euclidean),
        3 => Ok(DistanceMetric::Manhattan),
        4 => Ok(DistanceMetric::Hamming),
        _ => Err(PersistError::InvalidMetric { tag }),
    }
}

fn usize_to_u64(value: usize, what: &'static str) -> Result<u64> {
    u64::try_from(value).map_err(|_| PersistError::InvalidPayload {
        reason: match what {
            "dim" => "dim does not fit in u64",
            "n_vectors" => "n_vectors does not fit in u64",
            "index_type_len" => "index_type length does not fit in u64",
            _ => "usize value does not fit in u64",
        },
    })
}

fn u64_to_usize(value: u64, what: &'static str) -> Result<usize> {
    usize::try_from(value).map_err(|_| PersistError::InvalidPayload {
        reason: match what {
            "dim" => "dim does not fit in usize on this host",
            "n_vectors" => "n_vectors does not fit in usize on this host",
            "index_type_len" => "index_type length does not fit in usize on this host",
            _ => "u64 value does not fit in usize on this host",
        },
    })
}

/// Write a [`FileHeader`] to `writer` in the fixed-width little-endian
/// wire format.
///
/// # Errors
///
/// Returns [`PersistError::Io`] if a write fails, or
/// [`PersistError::InvalidPayload`] if a `usize` field does not fit in
/// `u64`.
///
/// # Examples
///
/// ```
/// use std::io::Cursor;
///
/// use iqdb_persist::format::{read_header, write_header};
/// use iqdb_persist::{CURRENT_VERSION, FileHeader, MAGIC};
/// use iqdb_types::DistanceMetric;
///
/// let header = FileHeader {
///     magic: MAGIC,
///     version: CURRENT_VERSION,
///     index_type: "flat".to_string(),
///     dim: 8,
///     metric: DistanceMetric::Euclidean,
///     n_vectors: 3,
///     crc32: 0,
/// };
/// let mut buf = Vec::new();
/// write_header(&mut buf, &header).unwrap();
/// let mut cur = Cursor::new(&buf[..]);
/// let parsed = read_header(&mut cur).unwrap();
/// assert_eq!(parsed, header);
/// ```
pub fn write_header(writer: &mut dyn Write, header: &FileHeader) -> Result<()> {
    write_all(writer, &header.magic)?;
    write_all(writer, &header.version.to_le_bytes())?;

    let it_bytes = header.index_type.as_bytes();
    let it_len = usize_to_u64(it_bytes.len(), "index_type_len")?;
    write_all(writer, &it_len.to_le_bytes())?;
    write_all(writer, it_bytes)?;

    let dim_u64 = usize_to_u64(header.dim, "dim")?;
    write_all(writer, &dim_u64.to_le_bytes())?;

    write_all(writer, &[metric_to_tag(header.metric)?])?;

    let n_u64 = usize_to_u64(header.n_vectors, "n_vectors")?;
    write_all(writer, &n_u64.to_le_bytes())?;

    write_all(writer, &header.crc32.to_le_bytes())?;
    Ok(())
}

/// Read a [`FileHeader`] from `reader` and validate it.
///
/// Validation in v0.2:
///
/// - `magic` must equal [`MAGIC`] — otherwise
///   [`PersistError::BadMagic`].
/// - `version` must be in the supported range (up to [`CURRENT_VERSION`])
///   — otherwise [`PersistError::UnsupportedVersion`].
/// - The metric tag must be in the known set — otherwise
///   [`PersistError::InvalidMetric`].
///
/// The `crc32` field is returned as-is — verifying the payload against
/// it is the caller's responsibility (see [`crate::PersistedIndex`]).
///
/// # Errors
///
/// See above + [`PersistError::TruncatedHeader`] for truncated reads.
///
/// # Examples
///
/// See [`write_header`] for a round-trip example.
pub fn read_header(reader: &mut dyn Read) -> Result<FileHeader> {
    let mut magic = [0u8; 8];
    read_exact_or_truncated(reader, &mut magic)?;
    if magic != MAGIC {
        return Err(PersistError::BadMagic { found: magic });
    }

    let mut buf4 = [0u8; 4];
    read_exact_or_truncated(reader, &mut buf4)?;
    let version = u32::from_le_bytes(buf4);
    if !(MIN_SUPPORTED_VERSION..=CURRENT_VERSION).contains(&version) {
        return Err(PersistError::UnsupportedVersion {
            found: version,
            supported: CURRENT_VERSION,
        });
    }

    let mut buf8 = [0u8; 8];
    read_exact_or_truncated(reader, &mut buf8)?;
    let it_len_u64 = u64::from_le_bytes(buf8);
    let it_len = u64_to_usize(it_len_u64, "index_type_len")?;

    // Cap the on-disk length so a malicious or corrupted header can't ask us
    // to allocate gigabytes. 4 KiB is comfortably larger than any plausible
    // tag ("flat", "hnsw", "ivf-pq", ...).
    const MAX_INDEX_TYPE_LEN: usize = 4096;
    if it_len > MAX_INDEX_TYPE_LEN {
        return Err(PersistError::InvalidPayload {
            reason: "index_type length exceeds the 4 KiB cap",
        });
    }
    let mut it_bytes = vec![0u8; it_len];
    read_exact_or_truncated(reader, &mut it_bytes)?;
    let index_type = String::from_utf8(it_bytes).map_err(|_| PersistError::InvalidPayload {
        reason: "index_type is not valid UTF-8",
    })?;

    read_exact_or_truncated(reader, &mut buf8)?;
    let dim = u64_to_usize(u64::from_le_bytes(buf8), "dim")?;

    let mut metric_buf = [0u8; 1];
    read_exact_or_truncated(reader, &mut metric_buf)?;
    let metric = tag_to_metric(metric_buf[0])?;

    read_exact_or_truncated(reader, &mut buf8)?;
    let n_vectors = u64_to_usize(u64::from_le_bytes(buf8), "n_vectors")?;

    read_exact_or_truncated(reader, &mut buf4)?;
    let crc32 = u32::from_le_bytes(buf4);

    Ok(FileHeader {
        magic,
        version,
        index_type,
        dim,
        metric,
        n_vectors,
        crc32,
    })
}

fn write_all(writer: &mut dyn Write, bytes: &[u8]) -> Result<()> {
    // No `path` is available at this layer — callers wrap the writer-
    // bound error with a meaningful path when one exists.
    // (PersistedIndex::save writes into a Vec<u8>, so this never fails
    // in the current flow.)
    writer.write_all(bytes).map_err(|source| PersistError::Io {
        path: std::path::PathBuf::new(),
        source,
    })
}

fn read_exact_or_truncated(reader: &mut dyn Read, buf: &mut [u8]) -> Result<()> {
    match reader.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            Err(PersistError::TruncatedHeader {
                needed: buf.len(),
                found: 0,
            })
        }
        Err(source) => Err(PersistError::Io {
            path: std::path::PathBuf::new(),
            source,
        }),
    }
}
