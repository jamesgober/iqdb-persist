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
[Unreleased]: https://github.com/jamesgober/iqdb-persist/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/jamesgober/iqdb-persist/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jamesgober/iqdb-persist/releases/tag/v0.1.0
