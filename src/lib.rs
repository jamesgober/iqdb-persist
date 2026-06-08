//! # iqdb-persist
//!
//! On-disk persistence for the **iQDB** vector database. Provides atomic
//! snapshot save / load with a versioned file header and a CRC32 integrity
//! check, generic over any type implementing [`iqdb_index::Index`].
//!
//! ## Tiered API
//!
//! - **Tier 1 — the lazy path.** [`PersistConfig::new`] plus
//!   [`PersistedIndex::open_with`] / [`PersistedIndex::save`] /
//!   [`PersistedIndex::load`] cover the whole common case — wrap an index,
//!   save it, load it back — with no builder and no generics to name
//!   beyond the index type itself. With the WAL on,
//!   [`PersistedIndex::insert`] / [`PersistedIndex::delete`] /
//!   [`PersistedIndex::checkpoint`] add durable, crash-recoverable
//!   mutation.
//! - **Tier 2 — the configured path.** The [`PersistConfig`] fields
//!   ([`fsync_policy`](PersistConfig::fsync_policy),
//!   [`compression`](PersistConfig::compression),
//!   [`wal_enabled`](PersistConfig::wal_enabled)) tune durability and
//!   on-disk size.
//! - **Tier 3 — the trait seam.** An index opts into persistence by
//!   implementing [`Persistable`]; everything in Tier 1 and Tier 2 then
//!   works against it unchanged.
//!
//! ## Surface
//!
//! - [`Persistable`] — the trait an index implements. Two methods,
//!   `save_to(&mut dyn Write)` and `load_from(&mut dyn Read) -> Result<Self>`,
//!   plus a stable [`INDEX_TYPE`](Persistable::INDEX_TYPE) tag. The impl
//!   serializes **only** the index's self-contained payload; the file
//!   header, CRC32, and atomic write are added by [`PersistedIndex`]
//!   around it.
//! - [`PersistedIndex`] — wraps an `I: Index + Persistable`. Two honest
//!   constructors: [`open_with`](PersistedIndex::open_with) wraps an
//!   in-memory index for later [`save`](PersistedIndex::save);
//!   [`load`](PersistedIndex::load) reconstructs an index from disk and
//!   errors if the file does not exist.
//! - [`FileHeader`] + [`MAGIC`] + [`CURRENT_VERSION`] — the wire format.
//! - [`PersistConfig`] / [`FsyncPolicy`] / [`Compression`] — configuration.
//!   `wal_enabled = true` turns on the write-ahead log (v0.3);
//!   `Compression::Zstd|Lz4` is still rejected with
//!   [`PersistError::Unsupported`] until v0.4.
//! - [`PersistError`] — `#[non_exhaustive]` and `error_forge::ForgeError`-
//!   integrated.
//!
//! ## Three guards
//!
//! 1. The trait impl writes / reads **only** the index's self-contained
//!    payload. Framing (header + CRC32) lives in [`PersistedIndex`].
//! 2. This crate stays generic over `I` — it never names a concrete
//!    index. The `index_type` → concrete-type registry that
//!    `Database::open` needs lives in the umbrella `iqdb` crate.
//! 3. Tests use a tiny in-crate mock `Persistable`; `iqdb-persist` never
//!    dev-deps a concrete index crate.
//!
//! ## Scope
//!
//! v0.2 shipped atomic snapshot save/load + header + CRC32; v0.3 adds the
//! write-ahead log, replay, and crash recovery (this is the durability
//! path between snapshots). Compression is scaffolded and lands in v0.4;
//! the external `storage-io` substrate in v0.5. See `CHANGELOG.md` and
//! `dev/ROADMAP.md`.
//!
//! ## Example
//!
//! ```
//! use iqdb_persist::{FileHeader, CURRENT_VERSION, MAGIC};
//! use iqdb_types::DistanceMetric;
//!
//! // A header is just data; tools can inspect a snapshot file without
//! // loading the index it carries.
//! let header = FileHeader {
//!     magic: MAGIC,
//!     version: CURRENT_VERSION,
//!     index_type: "flat".to_string(),
//!     dim: 128,
//!     metric: DistanceMetric::Cosine,
//!     n_vectors: 1_000,
//!     crc32: 0,
//! };
//! assert_eq!(header.version, 1);
//! assert_eq!(&header.magic, b"IQDBPRST");
//! ```

#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(warnings)]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(unused_must_use)]
#![deny(unused_results)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
#![deny(clippy::unreachable)]
#![deny(clippy::undocumented_unsafe_blocks)]
#![forbid(unsafe_code)]

pub mod checksum;
mod compression;
mod config;
mod error;
pub mod format;
mod persisted;
mod recovery;
mod storage;
mod wal;

use std::io::{Read, Write};

use iqdb_index::Index;

pub use crate::config::{Compression, FsyncPolicy, PersistConfig};
pub use crate::error::{PersistError, Result};
pub use crate::format::{CURRENT_VERSION, FileHeader, MAGIC};
pub use crate::persisted::PersistedIndex;

// `Storage` stays internal to the `storage` module — it is the
// substrate seam for the future `storage-io` swap, not a v0.2 public
// extension point.

/// The version of this crate, taken from `Cargo.toml` at compile time.
///
/// # Examples
///
/// ```
/// let v = iqdb_persist::VERSION;
/// assert_eq!(v.split('.').count(), 3);
/// ```
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// An index that can be written to and read from a byte stream.
///
/// The two methods serialize the index's **self-contained payload**: the
/// vectors, the ids, and the metadata. They do **not** write the file
/// header or the CRC32 — that framing is added by [`PersistedIndex`]
/// around the payload. Keeping the impl payload-only is what lets the
/// wire format stay centralized in this crate and uniform across every
/// future index implementation.
///
/// ## On-disk format contract
///
/// [`INDEX_TYPE`](Persistable::INDEX_TYPE) is stamped into the file
/// header on save and matched on load. Once snapshot files exist on
/// real users' disks with a given tag, **renaming the tag is a breaking
/// format change** — treat it with the same care as the magic bytes.
/// [`CURRENT_VERSION`] is for evolving the wire format; the type tag is
/// identity, not version.
///
/// ## Self-describing payload
///
/// [`load_from`](Persistable::load_from) reconstructs `Self` from the
/// payload alone — no header is passed in. The payload MUST therefore be
/// self-describing: the impl should re-state any state the constructor
/// needs (typically `dim` and `metric` for a vector index) at the start
/// of the payload. [`PersistedIndex::load`] cross-checks the
/// payload-reconstructed `Self`'s [`dim`](iqdb_index::IndexCore::dim) /
/// [`metric`](iqdb_index::IndexCore::metric) /
/// [`len`](iqdb_index::IndexCore::len) against the header values and
/// errors loudly on mismatch — a header claiming `dim = 128` over a
/// payload that says `96` is a corrupted file we catch the same way
/// CRC32 catches bit flips.
///
/// # Examples
///
/// ```no_run
/// use std::io::{Read, Write};
///
/// use iqdb_index::{Index, IndexCore, IndexStats};
/// use iqdb_persist::{Persistable, Result};
/// use iqdb_types::{
///     DistanceMetric, Hit, Metadata, Result as IqdbResult, SearchParams, VectorId,
/// };
///
/// # struct DummyIndex { dim: usize, metric: DistanceMetric }
/// # impl IndexCore for DummyIndex {
/// #     fn insert(&mut self, _: VectorId, _: std::sync::Arc<[f32]>, _: Option<Metadata>) -> IqdbResult<()> { Ok(()) }
/// #     fn delete(&mut self, _: &VectorId) -> IqdbResult<()> { Ok(()) }
/// #     fn search(&self, _: &[f32], _: &SearchParams) -> IqdbResult<Vec<Hit>> { Ok(Vec::new()) }
/// #     fn len(&self) -> usize { 0 }
/// #     fn dim(&self) -> usize { self.dim }
/// #     fn metric(&self) -> DistanceMetric { self.metric }
/// #     fn flush(&mut self) -> IqdbResult<()> { Ok(()) }
/// #     fn stats(&self) -> IndexStats { IndexStats { index_type: "dummy", ..IndexStats::default() } }
/// # }
/// # impl Index for DummyIndex {
/// #     type Config = ();
/// #     fn new(dim: usize, metric: DistanceMetric, _: ()) -> IqdbResult<Self> { Ok(Self { dim, metric }) }
/// # }
/// impl Persistable for DummyIndex {
///     const INDEX_TYPE: &'static str = "dummy";
///     fn save_to(&self, _w: &mut dyn Write) -> Result<()> { Ok(()) }
///     fn load_from(_r: &mut dyn Read) -> Result<Self> {
///         Ok(DummyIndex { dim: 1, metric: DistanceMetric::Cosine })
///     }
/// }
/// ```
pub trait Persistable: Index {
    /// Stable, short identifier written into the file header.
    ///
    /// Examples: `"flat"`, `"hnsw"`.
    ///
    /// IMPORTANT: this string is part of the on-disk format contract.
    /// Once snapshot files exist on real users' disks with this tag,
    /// renaming it is a breaking format change — treat it with the
    /// same care as the magic bytes. [`CURRENT_VERSION`] is for
    /// evolving the format; this tag is identity, not version.
    const INDEX_TYPE: &'static str;

    /// Write ONLY the index's self-contained payload.
    ///
    /// The framing ([`FileHeader`] + CRC32) is added by
    /// [`PersistedIndex`] around this. Do not write magic bytes or a
    /// checksum from inside the impl.
    ///
    /// # Errors
    ///
    /// Returns a [`PersistError`] if a write to `writer` fails or if a
    /// `usize` field of the index does not fit in `u64`.
    fn save_to(&self, writer: &mut dyn Write) -> Result<()>;

    /// Reconstruct `Self` from the payload alone (no framing).
    ///
    /// The payload must be self-describing — see the trait-level
    /// "Self-describing payload" note.
    ///
    /// # Errors
    ///
    /// Returns a [`PersistError`] if a read from `reader` fails or if
    /// the payload bytes do not decode to a valid `Self`.
    fn load_from(reader: &mut dyn Read) -> Result<Self>
    where
        Self: Sized;
}
