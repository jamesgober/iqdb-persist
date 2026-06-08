# iqdb-persist &mdash; API Reference

> Complete reference for every public item in `iqdb-persist` v0.2.0, with
> descriptions, parameters, errors, and runnable examples.

`iqdb-persist` is the on-disk persistence layer of the iQDB vector
database. It adds durable snapshot **save** and **load** to any index that
implements [`iqdb_index::Index`], wrapped behind a small framing layer
(versioned file header + CRC32 integrity check) and an atomic write
(temp file + `fsync` + rename + directory `fsync`). An interrupted write
never corrupts an existing good file.

---

## Table of Contents

- **[Installation](#installation)**
- **[Tiered API](#tiered-api)**
- **[Quick Start](#quick-start)**
- **[Public APIs](#public-apis)**
  - [`Persistable`](#persistable)
  - [`PersistedIndex` — construction](#persistedindex--construction)
  - [`PersistedIndex` — accessors](#persistedindex--accessors)
  - [`PersistedIndex` — save](#persistedindex--save)
  - [`PersistConfig`](#persistconfig)
  - [`FsyncPolicy`](#fsyncpolicy)
  - [`Compression`](#compression)
  - [`FileHeader`](#fileheader)
  - [`MAGIC` and `CURRENT_VERSION`](#magic-and-current_version)
  - [`format` — header read/write](#format--header-readwrite)
  - [`checksum` — CRC32 helpers](#checksum--crc32-helpers)
  - [`VERSION`](#version)
- **[Errors](#errors)**
- **[On-disk format](#on-disk-format)**
- **[Durability and atomicity](#durability-and-atomicity)**
- **[Feature flags](#feature-flags)**
- **[Notes](#notes)**

---

## Installation

```toml
[dependencies]
iqdb-persist = "0.2"
```

`iqdb-persist` takes its core vocabulary — `DistanceMetric`, `IqdbError` —
from `iqdb-types`, and the `Index` / `IndexCore` traits the persisted index
is generic over from `iqdb-index`. A typical consumer depends on all three:

```toml
[dependencies]
iqdb-persist = "0.2"
iqdb-index   = "1.0"
iqdb-types   = "1.0"
```

MSRV is Rust **1.87** (edition 2024). The crate is `std`-only; the `serde`
feature is opt-in (see [Feature flags](#feature-flags)).

---

## Tiered API

- **Tier 1 — the lazy path.** [`PersistConfig::new`](#persistconfig) plus
  [`PersistedIndex::open_with`](#persistedindex--construction),
  [`PersistedIndex::save`](#persistedindex--save), and
  [`PersistedIndex::load`](#persistedindex--construction) cover the whole
  common case: wrap an index, save it, load it back.
- **Tier 2 — the configured path.** The [`PersistConfig`](#persistconfig)
  fields — [`fsync_policy`](#fsyncpolicy), [`compression`](#compression),
  `wal_enabled` — tune durability and on-disk size.
- **Tier 3 — the trait seam.** An index opts into persistence by
  implementing [`Persistable`](#persistable); everything in Tier 1 and
  Tier 2 then works against it unchanged. The lower-level
  [`format`](#format--header-readwrite) and [`checksum`](#checksum--crc32-helpers)
  helpers are exposed for tools that inspect a snapshot file without
  loading the index it carries.

---

## Quick Start

```rust
use std::io::{Read, Write};
use std::sync::Arc;

use iqdb_index::{Index, IndexCore, IndexStats};
use iqdb_persist::{PersistConfig, PersistError, PersistedIndex, Persistable, Result};
use iqdb_types::{DistanceMetric, Hit, Metadata, Result as IqdbResult, SearchParams, VectorId};

// A minimal index that knows how to serialize its own payload.
struct VecIndex { dim: usize, metric: DistanceMetric, n: usize }

impl IndexCore for VecIndex {
    fn insert(&mut self, _: VectorId, _: Arc<[f32]>, _: Option<Metadata>) -> IqdbResult<()> { self.n += 1; Ok(()) }
    fn delete(&mut self, _: &VectorId) -> IqdbResult<()> { Ok(()) }
    fn search(&self, _: &[f32], _: &SearchParams) -> IqdbResult<Vec<Hit>> { Ok(Vec::new()) }
    fn len(&self) -> usize { self.n }
    fn dim(&self) -> usize { self.dim }
    fn metric(&self) -> DistanceMetric { self.metric }
    fn flush(&mut self) -> IqdbResult<()> { Ok(()) }
    fn stats(&self) -> IndexStats { IndexStats { index_type: "vec", ..IndexStats::default() } }
}
impl Index for VecIndex {
    type Config = ();
    fn new(dim: usize, metric: DistanceMetric, _: ()) -> IqdbResult<Self> { Ok(Self { dim, metric, n: 0 }) }
}
impl Persistable for VecIndex {
    const INDEX_TYPE: &'static str = "vec";
    fn save_to(&self, w: &mut dyn Write) -> Result<()> {
        let io = |source| PersistError::Io { path: std::path::PathBuf::new(), source };
        // Self-describing payload: dim and count restate the constructor inputs.
        w.write_all(&(self.dim as u64).to_le_bytes()).map_err(io)?;
        w.write_all(&(self.n as u64).to_le_bytes()).map_err(io)?;
        Ok(())
    }
    fn load_from(r: &mut dyn Read) -> Result<Self> {
        let io = |source| PersistError::Io { path: std::path::PathBuf::new(), source };
        let mut b = [0u8; 8];
        r.read_exact(&mut b).map_err(io)?;
        let dim = u64::from_le_bytes(b) as usize;
        r.read_exact(&mut b).map_err(io)?;
        let n = u64::from_le_bytes(b) as usize;
        Ok(Self { dim, metric: DistanceMetric::Cosine, n })
    }
}

# fn main() -> Result<()> {
let path = std::env::temp_dir().join("quickstart.iqdb");
let cfg = PersistConfig::new(&path);

// 1. Wrap an index and save it atomically.
let mut index = VecIndex::new(8, DistanceMetric::Cosine, ()).unwrap();
let _ = index.insert(VectorId::from(1u64), Arc::<[f32]>::from(&[0.0; 8][..]), None);
PersistedIndex::open_with(index, cfg.clone())?.save()?;

// 2. Load it back; the framing verifies magic, version, type, and CRC32.
let restored: PersistedIndex<VecIndex> = PersistedIndex::load(cfg)?;
assert_eq!(restored.index().len(), 1);
assert_eq!(restored.index().dim(), 8);

std::fs::remove_file(&path).ok();
# Ok(())
# }
```

A complete, runnable version (with vector payloads) lives in
[`examples/save_and_load.rs`](../examples/save_and_load.rs):

```sh
cargo run --example save_and_load
```

---

## Public APIs

### `Persistable`

```rust
pub trait Persistable: Index {
    const INDEX_TYPE: &'static str;
    fn save_to(&self, writer: &mut dyn Write) -> Result<()>;
    fn load_from(reader: &mut dyn Read) -> Result<Self> where Self: Sized;
}
```

The trait an index implements to become snapshot-able. It is a supertrait
of [`iqdb_index::Index`], so any `Persistable` is also a full index.

**`const INDEX_TYPE: &'static str`** — a stable, short identity tag (for
example `"flat"`, `"hnsw"`). It is stamped into the [`FileHeader`](#fileheader)
on save and matched against the caller's `I::INDEX_TYPE` on load: asking for
`PersistedIndex::<FlatIndex>::load` on an HNSW file fails with
[`PersistError::InvalidIndexType`](#errors). This string is part of the
on-disk contract — once snapshot files exist with a tag, renaming it is a
breaking format change, exactly like changing the magic bytes.

**`fn save_to(&self, writer: &mut dyn Write) -> Result<()>`** — write
**only** the index's self-contained payload (vectors, ids, metadata). Do
**not** write magic bytes, the header, or a checksum from inside the impl:
that framing is added by [`PersistedIndex`](#persistedindex--save) around
the payload.

- `writer`: the byte sink the payload is written to. In the current flow
  this is an in-memory `Vec<u8>` that `PersistedIndex` then frames and
  writes atomically.
- **Errors:** any [`PersistError`](#errors) the impl chooses — typically
  [`PersistError::Io`](#errors) on a write failure or
  [`PersistError::InvalidPayload`](#errors) if a `usize` field does not fit
  in `u64`.

**`fn load_from(reader: &mut dyn Read) -> Result<Self>`** — reconstruct
`Self` from the payload alone. No header is passed in, so the payload
**must be self-describing**: restate any state the constructor needs
(typically `dim` and `metric`) at the start of the payload.
[`PersistedIndex::load`](#persistedindex--construction) cross-checks the
reconstructed index's `dim` / `metric` / `len` against the header and
errors with [`PersistError::InvalidPayload`](#errors) on any disagreement —
catching a corrupted file the same way CRC32 catches bit flips.

See the [Quick Start](#quick-start) for a full `impl`.

---

### `PersistedIndex` — construction

```rust
pub struct PersistedIndex<I: Index + Persistable> { /* private */ }

impl<I: Index + Persistable> PersistedIndex<I> {
    pub fn open_with(inner: I, config: PersistConfig) -> Result<Self>;
    pub fn load(config: PersistConfig) -> Result<Self>;
}
```

The snapshot lifecycle wrapper around an in-memory index. There is no
magic "create-or-load" constructor by design — that ergonomic one-call
lives in the umbrella `iqdb` crate, layered on these two honest
primitives.

**`open_with(inner, config)`** — wrap an already-constructed index for a
later [`save`](#persistedindex--save). Performs **no disk I/O** at
construction.

- `inner`: the index to wrap.
- `config`: the [`PersistConfig`](#persistconfig). Validated here.
- **Errors:** [`PersistError::Unsupported`](#errors) if `config` requests a
  feature this build does not implement (`wal_enabled = true`, or any
  non-`None` compression in v0.2).

**`load(config)`** — read `config.path` and reconstruct the wrapped index.

- `config`: the [`PersistConfig`](#persistconfig); `config.path` is the
  file to read.
- **Errors:** [`PersistError::Io`](#errors) (including a missing file),
  [`BadMagic`](#errors), [`UnsupportedVersion`](#errors),
  [`TruncatedHeader`](#errors), [`InvalidMetric`](#errors),
  [`InvalidIndexType`](#errors), [`ChecksumMismatch`](#errors),
  [`InvalidPayload`](#errors), or [`IndexBuild`](#errors) — see
  [Errors](#errors).

```rust
# use iqdb_persist::{PersistConfig, PersistError, PersistedIndex, Persistable, Result};
# use iqdb_index::{Index, IndexCore, IndexStats};
# use iqdb_types::{DistanceMetric, Hit, Metadata, Result as IqdbResult, SearchParams, VectorId};
# use std::io::{Read, Write};
# use std::sync::Arc;
# struct Idx { dim: usize, metric: DistanceMetric }
# impl IndexCore for Idx {
# fn insert(&mut self,_:VectorId,_:Arc<[f32]>,_:Option<Metadata>)->IqdbResult<()>{Ok(())}
# fn delete(&mut self,_:&VectorId)->IqdbResult<()>{Ok(())}
# fn search(&self,_:&[f32],_:&SearchParams)->IqdbResult<Vec<Hit>>{Ok(Vec::new())}
# fn len(&self)->usize{0} fn dim(&self)->usize{self.dim} fn metric(&self)->DistanceMetric{self.metric}
# fn flush(&mut self)->IqdbResult<()>{Ok(())}
# fn stats(&self)->IndexStats{IndexStats{index_type:"idx",..IndexStats::default()}}
# }
# impl Index for Idx { type Config=(); fn new(dim:usize,metric:DistanceMetric,_:())->IqdbResult<Self>{Ok(Self{dim,metric})} }
# impl Persistable for Idx {
# const INDEX_TYPE:&'static str="idx";
# fn save_to(&self,w:&mut dyn Write)->Result<()>{
#   let io=|s|PersistError::Io{path:std::path::PathBuf::new(),source:s};
#   w.write_all(&(self.dim as u64).to_le_bytes()).map_err(io)?; w.write_all(&[2]).map_err(io)?; Ok(()) }
# fn load_from(r:&mut dyn Read)->Result<Self>{
#   let io=|s|PersistError::Io{path:std::path::PathBuf::new(),source:s};
#   let mut b=[0u8;8]; r.read_exact(&mut b).map_err(io)?; let dim=u64::from_le_bytes(b) as usize;
#   let mut t=[0u8;1]; r.read_exact(&mut t).map_err(io)?; Ok(Self{dim,metric:DistanceMetric::Euclidean}) }
# }
# fn main() -> Result<()> {
let path = std::env::temp_dir().join("ctor.iqdb");
let cfg = PersistConfig::new(&path);
let idx = Idx::new(4, DistanceMetric::Euclidean, ()).unwrap();
PersistedIndex::open_with(idx, cfg.clone())?.save()?;
let restored: PersistedIndex<Idx> = PersistedIndex::load(cfg)?;
assert_eq!(restored.index().dim(), 4);
# std::fs::remove_file(&path).ok();
# Ok(())
# }
```

---

### `PersistedIndex` — accessors

```rust
impl<I: Index + Persistable> PersistedIndex<I> {
    pub fn index(&self) -> &I;
    pub fn index_mut(&mut self) -> &mut I;
    pub fn config(&self) -> &PersistConfig;
}
```

- **`index()`** — borrow the wrapped index for queries.
- **`index_mut()`** — borrow it mutably for inserts / deletes / flush. The
  on-disk file is **not** updated until you call
  [`save`](#persistedindex--save) again.
- **`config()`** — the [`PersistConfig`](#persistconfig) this wrapper was
  constructed with.

```rust
# use iqdb_persist::{PersistConfig, PersistError, PersistedIndex, Persistable, Result};
# use iqdb_index::{Index, IndexCore, IndexStats};
# use iqdb_types::{DistanceMetric, Hit, Metadata, Result as IqdbResult, SearchParams, VectorId};
# use std::io::{Read, Write}; use std::sync::Arc;
# struct Idx { dim: usize }
# impl IndexCore for Idx {
# fn insert(&mut self,_:VectorId,_:Arc<[f32]>,_:Option<Metadata>)->IqdbResult<()>{Ok(())}
# fn delete(&mut self,_:&VectorId)->IqdbResult<()>{Ok(())}
# fn search(&self,_:&[f32],_:&SearchParams)->IqdbResult<Vec<Hit>>{Ok(Vec::new())}
# fn len(&self)->usize{0} fn dim(&self)->usize{self.dim} fn metric(&self)->DistanceMetric{DistanceMetric::Cosine}
# fn flush(&mut self)->IqdbResult<()>{Ok(())}
# fn stats(&self)->IndexStats{IndexStats{index_type:"idx",..IndexStats::default()}}
# }
# impl Index for Idx { type Config=(); fn new(dim:usize,_:DistanceMetric,_:())->IqdbResult<Self>{Ok(Self{dim})} }
# impl Persistable for Idx { const INDEX_TYPE:&'static str="idx";
# fn save_to(&self,_:&mut dyn Write)->Result<()>{Ok(())} fn load_from(_:&mut dyn Read)->Result<Self>{Ok(Self{dim:1})} }
# fn main() -> Result<()> {
let cfg = PersistConfig::new("snap.iqdb");
let wrapped = PersistedIndex::open_with(Idx::new(4, DistanceMetric::Cosine, ()).unwrap(), cfg)?;
assert_eq!(wrapped.index().dim(), 4);
assert_eq!(wrapped.config().path.file_name().unwrap(), "snap.iqdb");
# Ok(())
# }
```

---

### `PersistedIndex` — save

```rust
impl<I: Index + Persistable> PersistedIndex<I> {
    pub fn save(&self) -> Result<()>;
}
```

Write the current state of the wrapped index to `self.config.path`
**atomically**. The sequence is: serialize the payload via
[`Persistable::save_to`](#persistable) into a buffer, compute its CRC32,
prepend the [`FileHeader`](#fileheader), then hand the framed bytes to the
storage substrate, which writes a temp file, `fsync`s it, renames it over
the target, and (on POSIX) `fsync`s the parent directory. See
[Durability and atomicity](#durability-and-atomicity).

- **Errors:** [`PersistError::Io`](#errors) if the temp write, rename, or
  directory `fsync` fails; any error from
  [`Persistable::save_to`](#persistable); or
  [`PersistError::InvalidPayload`](#errors) if a `usize` field does not fit
  in `u64`.

The method is instrumented with a `tracing` span at `debug` level carrying
the path, index type, and vector count.

---

### `PersistConfig`

```rust
pub struct PersistConfig {
    pub path: PathBuf,
    pub wal_enabled: bool,
    pub fsync_policy: FsyncPolicy,
    pub compression: Compression,
}

impl PersistConfig {
    pub fn new(path: impl Into<PathBuf>) -> Self;
}
impl Default for PersistConfig { /* ... */ }
```

Configuration handed to [`PersistedIndex`](#persistedindex--construction).

- **`path`** — the snapshot file on disk.
- **`wal_enabled`** — reserved for v0.3. `true` is rejected at construction
  with [`PersistError::Unsupported`](#errors) in v0.2.
- **`fsync_policy`** — how aggressively to flush; see
  [`FsyncPolicy`](#fsyncpolicy).
- **`compression`** — payload compression; see
  [`Compression`](#compression). Non-`None` is rejected in v0.2.

**`new(path)`** builds a config with v0.2 defaults: `wal_enabled = false`,
`fsync_policy = Always`, `compression = None`. **`default()`** is the same
with an empty placeholder path — set `path` before saving or loading.

```rust
use iqdb_persist::{Compression, FsyncPolicy, PersistConfig};

let cfg = PersistConfig::new("/var/lib/app/index.iqdb");
assert_eq!(cfg.fsync_policy, FsyncPolicy::Always);
assert_eq!(cfg.compression, Compression::None);
assert!(!cfg.wal_enabled);

// Override individual fields with struct-update syntax.
let fast = PersistConfig { fsync_policy: FsyncPolicy::Never, ..PersistConfig::new("scratch.iqdb") };
assert_eq!(fast.fsync_policy, FsyncPolicy::Never);
```

---

### `FsyncPolicy`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsyncPolicy { Always, Periodic(Duration), Never }
```

How aggressively the layer flushes to durable storage.

- **`Always`** — `fsync` every write. The default; strongest durability.
- **`Periodic(Duration)`** — WAL-specific (v0.3+); treated as `Always` for
  snapshot writes in v0.2.
- **`Never`** — never `fsync`. Fastest, weakest durability — appropriate
  for tests and tmpfs-backed paths only.

```rust
use iqdb_persist::FsyncPolicy;
use std::time::Duration;

let _ = FsyncPolicy::Always;
let _ = FsyncPolicy::Periodic(Duration::from_secs(1));
let _ = FsyncPolicy::Never;
```

---

### `Compression`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression { None, Zstd { level: i32 }, Lz4 }
```

Compression applied to the payload bytes on save. v0.2 ships `None`; the
other variants exist so the `PersistConfig` shape does not change when
compression lands in v0.4. Selecting them in v0.2 returns
[`PersistError::Unsupported`](#errors) at construction.

```rust
use iqdb_persist::Compression;

let _ = Compression::None;          // honored in v0.2
let _ = Compression::Zstd { level: 3 }; // lands in v0.4
let _ = Compression::Lz4;           // lands in v0.4
```

---

### `FileHeader`

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHeader {
    pub magic: [u8; 8],
    pub version: u32,
    pub index_type: String,
    pub dim: usize,
    pub metric: DistanceMetric,
    pub n_vectors: usize,
    pub crc32: u32,
}
```

The header at the start of every snapshot file. Exposed so a tool can
inspect a file's metadata without loading the index it carries. The
on-disk representation is fixed-width little-endian (see
[On-disk format](#on-disk-format)); `crc32` covers the **payload bytes
only**, not the header.

```rust
use iqdb_persist::{FileHeader, CURRENT_VERSION, MAGIC};
use iqdb_types::DistanceMetric;

let header = FileHeader {
    magic: MAGIC,
    version: CURRENT_VERSION,
    index_type: "flat".to_string(),
    dim: 128,
    metric: DistanceMetric::Cosine,
    n_vectors: 1_000,
    crc32: 0xDEAD_BEEF,
};
assert_eq!(header.index_type, "flat");
```

---

### `MAGIC` and `CURRENT_VERSION`

```rust
pub const MAGIC: [u8; 8] = *b"IQDBPRST";
pub const CURRENT_VERSION: u32 = 1;
```

- **`MAGIC`** — the eight bytes that prefix every snapshot file. A
  mismatch on load surfaces as [`PersistError::BadMagic`](#errors).
- **`CURRENT_VERSION`** — the on-disk format version this build writes and
  reads. A different version on load surfaces as
  [`PersistError::UnsupportedVersion`](#errors).

```rust
assert_eq!(&iqdb_persist::MAGIC, b"IQDBPRST");
assert_eq!(iqdb_persist::CURRENT_VERSION, 1);
```

---

### `format` — header read/write

```rust
pub fn write_header(writer: &mut dyn Write, header: &FileHeader) -> Result<()>;
pub fn read_header(reader: &mut dyn Read) -> Result<FileHeader>;
```

The `iqdb_persist::format` module exposes the two halves of the wire
format so external tooling can parse or emit a header directly.

- **`write_header(writer, header)`** — write `header` in fixed-width
  little-endian. **Errors:** [`PersistError::Io`](#errors) on a write
  failure, [`PersistError::InvalidPayload`](#errors) if a `usize` field
  does not fit in `u64`, or [`PersistError::UnsupportedMetric`](#errors) if
  the metric has no on-disk tag in this build.
- **`read_header(reader)`** — read and validate a header. Validates magic,
  version, and the metric tag. **Errors:** [`BadMagic`](#errors),
  [`UnsupportedVersion`](#errors), [`InvalidMetric`](#errors),
  [`TruncatedHeader`](#errors), or [`InvalidPayload`](#errors). The
  `crc32` field is returned as-is; verifying the payload against it is the
  caller's job (see [`checksum`](#checksum--crc32-helpers)).

```rust
use std::io::Cursor;
use iqdb_persist::format::{read_header, write_header};
use iqdb_persist::{CURRENT_VERSION, FileHeader, MAGIC};
use iqdb_types::DistanceMetric;

let header = FileHeader {
    magic: MAGIC, version: CURRENT_VERSION, index_type: "flat".to_string(),
    dim: 8, metric: DistanceMetric::Euclidean, n_vectors: 3, crc32: 0,
};
let mut buf = Vec::new();
write_header(&mut buf, &header).unwrap();
let parsed = read_header(&mut Cursor::new(&buf[..])).unwrap();
assert_eq!(parsed, header);
```

---

### `checksum` — CRC32 helpers

```rust
pub fn compute(bytes: &[u8]) -> u32;
pub fn verify(bytes: &[u8], expected: u32) -> Result<()>;
```

The `iqdb_persist::checksum` module wraps a SIMD-accelerated CRC32 (IEEE
polynomial) behind a tiny surface.

- **`compute(bytes)`** — the CRC32 of `bytes`.
- **`verify(bytes, expected)`** — `Ok(())` if `compute(bytes) == expected`,
  otherwise [`PersistError::ChecksumMismatch`](#errors). Never panics,
  never returns silently-wrong data.

```rust
use iqdb_persist::checksum;

let payload = b"hello";
let crc = checksum::compute(payload);
assert!(checksum::verify(payload, crc).is_ok());
assert!(checksum::verify(payload, crc.wrapping_add(1)).is_err());
```

---

### `VERSION`

```rust
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
```

The crate version, baked in at compile time.

```rust
assert_eq!(iqdb_persist::VERSION.split('.').count(), 3);
```

---

## Errors

Every fallible function returns `iqdb_persist::Result<T>` =
`Result<T, PersistError>`. `PersistError` is `#[non_exhaustive]`, so a
`match` on it must include a wildcard arm. It implements
`error_forge::ForgeError` (`kind()` / `caption()`) so it composes into the
same structured error events the rest of the iQDB spine emits, and
`std::error::Error` (with `source()` set for the wrapping variants).

| Variant | When |
|---------|------|
| `Io { path, source }` | an OS-level read/write/rename/`fsync` failed; `source` is the `std::io::Error`. A missing file on `load` is reported here. |
| `BadMagic { found }` | the first eight bytes are not [`MAGIC`](#magic-and-current_version) — the file is not an iQDB snapshot. |
| `UnsupportedVersion { found, supported }` | the header's format version is not the one this build supports. |
| `ChecksumMismatch { expected, computed }` | the payload CRC32 does not match the header — corruption or tampering. |
| `TruncatedHeader { needed, found }` | the file ended before the full header could be read. |
| `TruncatedPayload { needed, found }` | the file ended before the full payload could be read. |
| `InvalidMetric { tag }` | the header's metric tag is not in the known set (`0..=4`). |
| `UnsupportedMetric { metric }` | on save, a `DistanceMetric` this build has no on-disk tag for (`DistanceMetric` is `#[non_exhaustive]`). |
| `InvalidIndexType { found, expected }` | the header's index-type tag does not equal the caller's `I::INDEX_TYPE`. |
| `InvalidPayload { reason }` | the payload decoded structurally wrong — including a `dim` / `metric` / `n_vectors` disagreement between header and reconstructed index. |
| `IndexBuild(IqdbError)` | a downstream `Index::new` / `insert` (called from inside a `load_from` impl) returned an `IqdbError`. |
| `Unsupported { feature, available_in }` | the config requested a feature this build does not implement yet (`wal_enabled`, compression). |

```rust
use iqdb_persist::PersistError;

let err = PersistError::ChecksumMismatch { expected: 0xDEAD_BEEF, computed: 0 };
assert!(err.to_string().contains("checksum mismatch"));

let unsup = PersistError::Unsupported { feature: "wal_enabled", available_in: "v0.3" };
assert!(unsup.to_string().contains("v0.3"));
```

---

## On-disk format

A snapshot file is a header followed by the impl-defined payload. The
header is **strict little-endian, fixed-width**; all sizes are `u64`
regardless of host word size, so a file written on a 64-bit host reads
correctly on a 32-bit one.

```text
offset  bytes  field
0       8      magic ("IQDBPRST")
8       4      version (u32 LE)
12      8      index_type length, N (u64 LE)
20      N      index_type (UTF-8)
20+N    8      dim (u64 LE)
28+N    1      metric tag (u8)
29+N    8      n_vectors (u64 LE)
37+N    4      crc32 (u32 LE) — of the payload only
41+N    ...    payload (impl-defined, self-describing)
```

**Metric tag values** (on-disk contract — stable across the format
version): `0` Cosine, `1` DotProduct, `2` Euclidean, `3` Manhattan,
`4` Hamming.

The `index_type` length is capped at 4 KiB on read so a corrupt or hostile
header cannot trigger a giant allocation.

---

## Durability and atomicity

[`PersistedIndex::save`](#persistedindex--save) never writes the target
file in place. The internal storage substrate implements:

1. **Write a temp file** next to the target (`create_new`, so a stale temp
   never silently wins).
2. **`fsync` the temp file** (unless `FsyncPolicy::Never`).
3. **Atomically rename** the temp file over the target. On the same
   filesystem this is a single `rename(2)`; readers see either the old file
   or the new one, never a half-written mix.
4. **`fsync` the parent directory** so the rename itself is durable
   (POSIX only — see below).

If any step fails, the temp file is removed and the original target is left
**byte-for-byte intact**. This is the v0.2 recovery contract; full WAL
replay lands in v0.3.

**Platform note.** The directory-`fsync` step is POSIX-specific. On
Linux/macOS it flushes the parent directory inode so the rename survives a
crash. Windows exposes no portable directory `fsync` through `std`
(`File::open` on a directory handle fails with `ERROR_ACCESS_DENIED`) and
NTFS journals directory metadata separately, so the step is a deliberate
no-op there — not a durability regression. The temp-write + file-`fsync` +
atomic-rename byte sequence is identical on both platforms.

---

## Feature flags

| Feature | Default | Description |
|---------|---------|-------------|
| `std`   | yes | Standard library. Persistence is effectively `std`-only; the marker exists so a future `no_std` header-parsing subset does not break the surface. |
| `serde` | no  | Derive `Serialize` / `Deserialize` on [`PersistConfig`](#persistconfig), [`FsyncPolicy`](#fsyncpolicy), and [`Compression`](#compression) so a config can round-trip through a config file. Additive; the on-disk snapshot format is hand-rolled and unaffected. |

---

## Notes

- **Generic, never concrete.** This crate never names a concrete index.
  The `index_type` → concrete-type registry that a "create-or-load" call
  needs lives in the umbrella `iqdb` crate.
- **Payload-only impls.** A `Persistable` impl writes only the index's own
  bytes; framing (header + CRC32 + atomic write) is centralized here so it
  stays uniform across every index implementation.
- **Out of scope for v0.2:** WAL append/replay (v0.3), crash recovery
  beyond atomic-snapshot integrity (v0.3), Zstd/LZ4 compression (v0.4),
  and the external `storage-io` substrate (v0.5+). The `PersistConfig`
  knobs for these already exist and reject cleanly until then.

---

<sub>Copyright &copy; 2026 <strong>James Gober</strong>.</sub>
