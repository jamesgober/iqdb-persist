# iqdb-persist &mdash; API Reference

> Complete reference for every public item in `iqdb-persist` v1.0.0, with
> descriptions, parameters, errors, and runnable examples.
>
> **Stable (v1.0.0).** The surface below is the committed SemVer 1.x
> contract — no breaking changes before 2.0. The on-disk format is frozen
> and the parse/recovery paths are adversarially hardened (exhaustive
> byte-flip / truncation / garbage testing).

`iqdb-persist` is the on-disk persistence layer of the iQDB vector
database. It adds durable snapshot **save** and **load** to any index that
implements [`iqdb_index::Index`], wrapped behind a small framing layer
(versioned file header + CRC32 integrity check) and an atomic write
(temp file + `fsync` + rename + directory `fsync`). An interrupted write
never corrupts an existing good file. With the optional **write-ahead log**
enabled, mutations are logged and `fsync`ed before they are applied in
memory and replayed onto the snapshot on load, so an acknowledged
`insert` / `delete` survives a crash.

---

## Table of Contents

- **[Installation](#installation)**
- **[Tiered API](#tiered-api)**
- **[Quick Start](#quick-start)**
- **[Public APIs](#public-apis)**
  - [`Persistable`](#persistable)
  - [`PersistedIndex` — construction](#persistedindex--construction)
  - [`PersistedIndex` — accessors](#persistedindex--accessors)
  - [`PersistedIndex` — mutation (WAL)](#persistedindex--mutation-wal)
  - [`PersistedIndex` — save and checkpoint](#persistedindex--save-and-checkpoint)
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
- **[Write-ahead log](#write-ahead-log)**
- **[Durability and atomicity](#durability-and-atomicity)**
- **[Feature flags](#feature-flags)**
- **[Notes](#notes)**

---

## Installation

```toml
[dependencies]
iqdb-persist = "1.0"
```

`iqdb-persist` takes its core vocabulary — `DistanceMetric`, `IqdbError`,
`VectorId`, `Metadata` — from `iqdb-types`, and the `Index` / `IndexCore`
traits the persisted index is generic over from `iqdb-index`. A typical
consumer depends on all three:

```toml
[dependencies]
iqdb-persist = "1.0"
iqdb-index   = "1.0"
iqdb-types   = "1.0"
```

MSRV is Rust **1.87** (edition 2024). The crate is `std`-only; the `serde`,
`zstd`, and `lz4` features are opt-in (see [Feature flags](#feature-flags)).

---

## Tiered API

- **Tier 1 — the lazy path.** [`PersistConfig::new`](#persistconfig) plus
  [`PersistedIndex::open_with`](#persistedindex--construction),
  [`PersistedIndex::save`](#persistedindex--save-and-checkpoint), and
  [`PersistedIndex::load`](#persistedindex--construction) cover the whole
  common case: wrap an index, save it, load it back. With the WAL on,
  [`insert`](#persistedindex--mutation-wal) /
  [`delete`](#persistedindex--mutation-wal) /
  [`checkpoint`](#persistedindex--save-and-checkpoint) add durable,
  crash-recoverable mutation.
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
- **`index_mut()`** — borrow it mutably for direct inserts / deletes /
  flush. **WAL note:** mutations through this borrow **bypass the WAL** —
  they are not logged and will not survive a crash until the next
  [`checkpoint`](#persistedindex--save-and-checkpoint). In WAL mode prefer
  [`insert`](#persistedindex--mutation-wal) /
  [`delete`](#persistedindex--mutation-wal). The on-disk snapshot is **not**
  updated until you call [`save`](#persistedindex--save-and-checkpoint) or
  [`checkpoint`](#persistedindex--save-and-checkpoint).
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

### `PersistedIndex` — mutation (WAL)

```rust
impl<I: Index + Persistable> PersistedIndex<I> {
    pub fn insert(&mut self, id: VectorId, vector: Arc<[f32]>, meta: Option<Metadata>) -> Result<()>;
    pub fn delete(&mut self, id: &VectorId) -> Result<()>;
}
```

The durable mutation path. With the WAL enabled
([`PersistConfig::wal_enabled`](#persistconfig)), each call **logs and
`fsync`s the operation before applying it** in memory, so an acknowledged
mutation survives a crash and is restored on the next
[`load`](#persistedindex--construction). In snapshot-only mode they apply
to memory directly (durability then comes from the next
[`save`](#persistedindex--save-and-checkpoint)).

If the in-memory apply is rejected (for example a duplicate id, or an
absent id on delete), the just-logged record is rolled back, so the WAL
never drifts out of step with the index.

- `id` / `vector` / `meta`: the mutation, the same triple
  [`IndexCore::insert`](https://docs.rs/iqdb-index) takes.
- **Errors:** [`PersistError::Io`](#errors) if the WAL append or `fsync`
  fails; [`PersistError::IndexBuild`](#errors) if the index rejects the
  mutation; [`PersistError::InvalidPayload`](#errors) if a vector length
  does not fit in `u32`.

```rust
# use std::sync::Arc;
# use iqdb_persist::{PersistConfig, PersistError, PersistedIndex, Persistable, Result};
# use iqdb_index::{Index, IndexCore, IndexStats};
# use iqdb_types::{DistanceMetric, Hit, Metadata, Result as IqdbResult, SearchParams, VectorId};
# use std::io::{Read, Write};
# struct Idx { dim: usize, n: usize }
# impl IndexCore for Idx {
# fn insert(&mut self,_:VectorId,_:Arc<[f32]>,_:Option<Metadata>)->IqdbResult<()>{self.n+=1;Ok(())}
# fn delete(&mut self,_:&VectorId)->IqdbResult<()>{Ok(())}
# fn search(&self,_:&[f32],_:&SearchParams)->IqdbResult<Vec<Hit>>{Ok(Vec::new())}
# fn len(&self)->usize{self.n} fn dim(&self)->usize{self.dim} fn metric(&self)->DistanceMetric{DistanceMetric::Cosine}
# fn flush(&mut self)->IqdbResult<()>{Ok(())}
# fn stats(&self)->IndexStats{IndexStats{index_type:"idx",..IndexStats::default()}}
# }
# impl Index for Idx { type Config=(); fn new(dim:usize,_:DistanceMetric,_:())->IqdbResult<Self>{Ok(Self{dim,n:0})} }
# impl Persistable for Idx { const INDEX_TYPE:&'static str="idx";
# fn save_to(&self,w:&mut dyn Write)->Result<()>{let io=|s|PersistError::Io{path:std::path::PathBuf::new(),source:s};
#   w.write_all(&(self.dim as u64).to_le_bytes()).map_err(io)?; w.write_all(&(self.n as u64).to_le_bytes()).map_err(io)?; Ok(())}
# fn load_from(r:&mut dyn Read)->Result<Self>{let io=|s|PersistError::Io{path:std::path::PathBuf::new(),source:s};
#   let mut b=[0u8;8]; r.read_exact(&mut b).map_err(io)?; let dim=u64::from_le_bytes(b) as usize;
#   r.read_exact(&mut b).map_err(io)?; let n=u64::from_le_bytes(b) as usize; Ok(Self{dim,n})} }
# fn main() -> Result<()> {
let path = std::env::temp_dir().join("wal-doc.iqdb");
let mut cfg = PersistConfig::new(&path);
cfg.wal_enabled = true;

let mut db = PersistedIndex::open_with(Idx::new(4, DistanceMetric::Cosine, ()).unwrap(), cfg.clone())?;
db.insert(VectorId::from(1u64), Arc::<[f32]>::from(&[0.0; 4][..]), None)?; // logged, then applied
db.checkpoint()?;                                                          // fold WAL into a snapshot

# std::fs::remove_file(&path).ok();
# let mut wp = path.clone().into_os_string(); wp.push(".wal"); std::fs::remove_file(wp).ok();
# Ok(())
# }
```

---

### `PersistedIndex` — save and checkpoint

```rust
impl<I: Index + Persistable> PersistedIndex<I> {
    pub fn save(&self) -> Result<()>;
    pub fn checkpoint(&mut self) -> Result<()>;
}
```

**`save()`** writes the current state to `self.config.path` **atomically**:
serialize the payload via [`Persistable::save_to`](#persistable), compute
its CRC32, prepend the [`FileHeader`](#fileheader), then hand the framed
bytes to the storage substrate, which writes a temp file, `fsync`s it,
renames it over the target, and (on POSIX) `fsync`s the parent directory.
See [Durability and atomicity](#durability-and-atomicity).

**`checkpoint()`** writes a fresh snapshot **and** truncates the WAL back
to empty — the WAL-mode compaction operation. After a checkpoint the
snapshot alone captures the full state, so the log can be reset; this
bounds WAL growth and prevents a later [`load`](#persistedindex--construction)
from double-counting mutations. In snapshot-only mode it is equivalent to
`save`. **In WAL mode use `checkpoint`, not bare `save`:** `save` alone
leaves the WAL in place, so the next `load` would replay logged mutations
on top of a snapshot that already contains them.

- **Errors:** [`PersistError::Io`](#errors) if the temp write, rename, or
  `fsync` fails (and, for `checkpoint`, if truncating the WAL fails); any
  error from [`Persistable::save_to`](#persistable);
  [`PersistError::InvalidPayload`](#errors) if a `usize` field does not fit
  in `u64`.

Both go through a snapshot writer instrumented with a `tracing` span at
`debug` level carrying the path, index type, and vector count.

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

- **`path`** — the snapshot file on disk. The WAL lives beside it at
  `path` + `.wal`.
- **`wal_enabled`** — turn on the write-ahead log (v0.3+). When `true`,
  [`open_with`](#persistedindex--construction) writes an initial snapshot
  and opens a fresh WAL; [`insert`](#persistedindex--mutation-wal) /
  [`delete`](#persistedindex--mutation-wal) log before applying;
  [`load`](#persistedindex--construction) replays the log;
  [`checkpoint`](#persistedindex--save-and-checkpoint) compacts it.
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

Compression applied to the **snapshot payload** on save (the WAL is always
uncompressed). `None` is always available. `Zstd` (best ratio) and `Lz4`
(fastest) are gated behind the `zstd` / `lz4` cargo features; selecting one
whose feature is not compiled in returns
[`PersistError::Unsupported`](#errors) at construction. `Zstd { level }`
requires `level` in `1..=22`. The chosen scheme is recorded on disk, so a
file written with one scheme loads correctly as long as that scheme's
feature is enabled at load time.

```rust
use iqdb_persist::Compression;

let _ = Compression::None;              // always available
let _ = Compression::Zstd { level: 3 }; // requires the `zstd` feature
let _ = Compression::Lz4;               // requires the `lz4` feature
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
| `InvalidPayload { reason }` | the payload decoded structurally wrong — including a `dim` / `metric` / `n_vectors` disagreement between header and reconstructed index, or a decompression-bomb-sized length claim. |
| `Compression { reason }` | a compress step rejected its input (e.g. a Zstd level outside `1..=22`) or a decompress step failed / produced the wrong length. Bulk corruption is caught earlier by the CRC32 as `ChecksumMismatch`. |
| `IndexBuild(IqdbError)` | a downstream `Index::new` / `insert` (called from inside a `load_from` impl) returned an `IqdbError`. |
| `Unsupported { feature, available_in }` | the config selected a scheme whose cargo feature is not compiled in (`Compression::Zstd` without `zstd`, `Compression::Lz4` without `lz4`). |

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

**Format versions.** Version `1` (v0.2–v0.3) stored the payload verbatim.
Version `2` (v0.4+) prefixes the payload region with a 9-byte compression
preamble — `[scheme tag u8][uncompressed_len u64 LE]` — followed by the
(optionally compressed) bytes; the CRC32 covers the whole region, so
corruption is caught before decompression. The reader accepts both versions
(a version-1 file loads as uncompressed), so older snapshots remain
readable. The format is not frozen until v0.5.

---

## Write-ahead log

When [`PersistConfig::wal_enabled`](#persistconfig) is set, mutations are
recorded in a log beside the snapshot (`path` + `.wal`) so they survive a
crash between checkpoints. The log is a 12-byte header followed by
self-checked frames:

```text
header:  8  magic ("IQDBWAL\0")  +  4  version (u32 LE)
frame:   4  record length L (u32 LE)
         4  crc32 of the record (u32 LE)
         L  record body
record:  op (u8)  1 = insert | 2 = delete
  insert: vector_id, vec_len (u32 LE), vec_len × f32 LE, has_meta (u8),
          [n_entries (u32 LE), (key_len u32, key utf8, value)…]
  delete: vector_id
vector_id: kind (u8) 0 U64(u64 LE) | 1 Bytes(len u64 LE + bytes)
value:     tag (u8) 0 String | 1 Int(i64) | 2 Float(f64 bits) | 3 Bool | 4 Null
```

**Lifecycle.** [`open_with`](#persistedindex--construction) (WAL mode)
writes a base snapshot and a fresh empty log.
[`insert`](#persistedindex--mutation-wal) /
[`delete`](#persistedindex--mutation-wal) append a frame and `fsync` per
[`FsyncPolicy`](#fsyncpolicy) **before** touching memory.
[`checkpoint`](#persistedindex--save-and-checkpoint) writes a new snapshot
and truncates the log. [`load`](#persistedindex--construction) reconstructs
the snapshot, then replays every committed frame onto it.

**Crash safety.** Each frame carries its own length and CRC32. A crash
mid-append leaves a **torn tail** — a truncated or mis-checksummed final
frame — which replay detects and discards: that mutation was never
acknowledged, so dropping it is correct. A frame that fails its CRC32 but
sits before intact frames stops replay there. A frame that passes its CRC32
yet does not decode is a genuine corruption and surfaces as
[`PersistError::InvalidPayload`](#errors).

**Rollback.** `insert` / `delete` log before applying; if the in-memory
apply is then rejected, the just-written frame is truncated away, so the
log never contains a mutation the index does not.

---

## Durability and atomicity

[`PersistedIndex::save`](#persistedindex--save-and-checkpoint) never writes
the target file in place. The internal storage substrate implements:

1. **Write a temp file** next to the target (`create_new`, so a stale temp
   never silently wins).
2. **`fsync` the temp file** (unless `FsyncPolicy::Never`).
3. **Atomically rename** the temp file over the target. On the same
   filesystem this is a single `rename(2)`; readers see either the old file
   or the new one, never a half-written mix.
4. **`fsync` the parent directory** so the rename itself is durable
   (POSIX only — see below).

If any step fails, the temp file is removed and the original target is left
**byte-for-byte intact**. This protects the snapshot; the
[write-ahead log](#write-ahead-log) covers the mutations made between
snapshots.

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
| `zstd`  | no  | Enable `Compression::Zstd` (Zstandard, via the reference C library). Selecting it without this feature returns [`PersistError::Unsupported`](#errors). |
| `lz4`   | no  | Enable `Compression::Lz4` (LZ4 block format, pure-Rust `lz4_flex`). Selecting it without this feature returns [`PersistError::Unsupported`](#errors). |

---

## Notes

- **Generic, never concrete.** This crate never names a concrete index.
  The `index_type` → concrete-type registry that a "create-or-load" call
  needs lives in the umbrella `iqdb` crate.
- **Payload-only impls.** A `Persistable` impl writes only the index's own
  bytes; framing (header + CRC32 + atomic write) is centralized here so it
  stays uniform across every index implementation.
- **Frozen at v0.5.** The feature set (snapshots + CRC32 + atomic writes,
  WAL + crash recovery, Zstd/LZ4 compression), the public API above, and
  the on-disk format are all committed. Remaining work to 1.0 is the
  alpha/beta/rc hardening series — no new surface.
- **Out of scope:** the external `storage-io` substrate. Snapshot I/O
  already goes through an internal `Storage` seam; the WAL appends through
  its own `std::fs` handle. `storage-io` is the renamed `fsys-rs`; until
  that rename lands the integration is deferred, and when it does it is an
  internal swap behind the seam, not an API change.

---

<sub>Copyright &copy; 2026 <strong>James Gober</strong>.</sub>
