# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Added

### Changed

### Fixed

### Security

---

## [0.4.0] - 2026-06-08

Optional snapshot compression, and the **feature freeze**.

### Added

- **Snapshot compression** — `Compression::Zstd { level }` (Zstandard, via
  the reference C library) and `Compression::Lz4` (pure-Rust `lz4_flex`),
  applied to the snapshot payload. Gated behind the new `zstd` / `lz4`
  cargo features (off by default); selecting a scheme whose feature is not
  compiled in returns `PersistError::Unsupported`.
- **On-disk format version 2** — the payload region gains a 9-byte
  compression preamble (`[scheme tag u8][uncompressed_len u64 LE]`); the
  CRC32 covers the whole region, so corruption is caught before
  decompression. **Version-1 snapshots (v0.2–v0.3) still load** as
  uncompressed.
- **Decompression-bomb guard** — a payload claiming to expand beyond a
  per-file ratio bound is rejected as `InvalidPayload`.
- `PersistError::Compression { reason }` for codec failures (compress-side
  bad parameters, decompress-side errors or length mismatch).
- Compression round-trip / shrink / corruption / legacy-v1-read integration
  tests, codec unit tests, and `benches/compression_bench.rs`.

### Changed

- The persistence **feature set is frozen** as of v0.4: snapshots + CRC32 +
  atomic writes, WAL + crash recovery, and compression are complete.
  Remaining work to 1.0 is `storage-io` integration plus the API/format
  freeze (v0.5), then hardening — no new features.
- `CURRENT_VERSION` is now `2` (writer); the reader accepts `1..=2`.
- Selecting `Compression::Zstd` / `Lz4` is now honored (was rejected with
  `PersistError::Unsupported` through v0.3) when the matching feature is on.

---

## [0.3.0] - 2026-06-07

The write-ahead log: durable, crash-recoverable mutation between snapshots.

### Added

- **Write-ahead log.** `PersistConfig::wal_enabled` turns on a log beside
  the snapshot (`path` + `.wal`). Self-checked frames (per-record length +
  CRC32) over a compact binary encoding of insert/delete mutations,
  including `VectorId` (`U64` / `Bytes`) and `Metadata`.
- **`PersistedIndex::insert` / `delete`** — the durable mutation path: each
  op is logged and `fsync`ed (per `FsyncPolicy`) **before** it is applied in
  memory. A rejected apply rolls the just-logged frame back, so the WAL
  never drifts from the index.
- **`PersistedIndex::checkpoint`** — write a fresh snapshot and truncate the
  WAL, bounding log growth and preventing double-apply on the next load.
- **Crash recovery.** `PersistedIndex::load` replays every committed frame
  onto the snapshot. A torn tail (truncated or mis-checksummed final frame
  from a crash mid-append) is detected and discarded.
- **`FsyncPolicy::Periodic`** now governs WAL appends, `fsync`ing no more
  than once per interval.
- WAL lifecycle + crash-recovery integration tests, a `proptest` round-trip
  over arbitrary records, and `criterion` benches for WAL append and replay
  (`benches/wal_bench.rs`).

### Changed

- `PersistConfig::wal_enabled = true` is now honored (was rejected with
  `PersistError::Unsupported` in v0.2). Compression remains rejected until
  v0.4.
- `PersistedIndex::open_with` writes an initial snapshot and opens a fresh
  WAL when `wal_enabled` (still no I/O in snapshot-only mode).

---

## [0.2.0] - 2026-06-07

The first functional release: atomic snapshot save/load, the versioned
on-disk format, and CRC32 integrity — the hard part of the roadmap, not
deferred.

### Added

- **`Persistable`** trait — the two-method (`save_to` / `load_from`) seam
  plus the stable `INDEX_TYPE` tag an index implements to become
  snapshot-able. The impl writes only the index's self-contained payload;
  framing is added around it.
- **`PersistedIndex<I: Index + Persistable>`** — the snapshot lifecycle
  wrapper. `open_with` wraps an in-memory index for later `save`; `load`
  reconstructs one from disk. `index` / `index_mut` / `config` accessors.
- **`save`** writes atomically: serialize payload → CRC32 → prepend
  `FileHeader` → temp file + `fsync` + atomic rename + directory `fsync`
  (POSIX). An interrupted write never corrupts an existing good file.
- **Versioned wire format** — `FileHeader`, `MAGIC` (`b"IQDBPRST"`),
  `CURRENT_VERSION` (1), and the public `format::{read_header,
  write_header}` for snapshot inspection. All sizes are fixed-width
  little-endian `u64` — portable across 32- and 64-bit hosts.
- **CRC32 integrity** over the payload via the `checksum::{compute,
  verify}` helpers; mismatches surface as `PersistError::ChecksumMismatch`.
- **`PersistConfig` / `FsyncPolicy` / `Compression`** configuration; the
  `wal_enabled` and compression knobs are present but rejected with
  `PersistError::Unsupported` until v0.3 / v0.4.
- **`PersistError`** — `#[non_exhaustive]`, `error_forge::ForgeError`- and
  `std::error::Error`-integrated, with one variant per failure mode.
- **`serde`** feature deriving `Serialize` / `Deserialize` on the config
  types (additive; the on-disk format is unaffected).
- Property-tested header round-trip and targeted error-shape tests; unit
  tests for atomic-save survival, CRC mismatch, index-type mismatch, and
  config validation; a runnable `examples/save_and_load.rs`.

### Changed

- Wired the crate onto the published `iqdb-types` / `iqdb-index` /
  `error-forge` 1.0 crates; added `crc32fast` and a `tracing` span on
  `save`.
- `Matt Callahan` added to the authors.

---

## [0.1.0] - 2026-05-30

Initial scaffold and repository bootstrap. No domain logic yet &mdash; this release establishes the structure, tooling, and quality gates the implementation will be built on.

### Added

- `Cargo.toml` with crate metadata, Rust 2024 edition, MSRV 1.87.
- Dual `Apache-2.0 OR MIT` license files.
- `README.md`, `CHANGELOG.md`, and a documentation skeleton.
- `REPS.md` compliance baseline.
- `.github/workflows/ci.yml` CI matrix; `deny.toml`, `clippy.toml`, `rustfmt.toml`.
- `dev/DIRECTIVES.md` and `dev/ROADMAP.md` (committed engineering standards + plan).
[Unreleased]: https://github.com/jamesgober/iqdb-persist/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/jamesgober/iqdb-persist/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/jamesgober/iqdb-persist/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/jamesgober/iqdb-persist/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jamesgober/iqdb-persist/releases/tag/v0.1.0
