<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br>
    <b>iqdb-persist</b>
    <br>
    <sub><sup>iQDB DISK PERSISTENCE</sup></sub>
</h1>

<div align="center">
    <a href="https://crates.io/crates/iqdb-persist"><img alt="Crates.io" src="https://img.shields.io/crates/v/iqdb-persist"></a>
    <a href="https://crates.io/crates/iqdb-persist"><img alt="Downloads" src="https://img.shields.io/crates/d/iqdb-persist?color=%230099ff"></a>
    <a href="https://docs.rs/iqdb-persist"><img alt="docs.rs" src="https://img.shields.io/docsrs/iqdb-persist"></a>
    <a href="https://github.com/jamesgober/iqdb-persist/actions"><img alt="CI" src="https://github.com/jamesgober/iqdb-persist/actions/workflows/ci.yml/badge.svg"></a>
    <a href="https://github.com/rust-lang/rfcs/blob/master/text/2495-min-rust-version.md"><img alt="MSRV" src="https://img.shields.io/badge/MSRV-1.87%2B-blue"></a>
</div>

<br>

<div align="left">
    <p>
        <strong>iqdb-persist</strong> is what moves iQDB from demo-only to actually usable: it adds durable snapshot <strong>save</strong> and <strong>load</strong> to any index, behind a versioned file header, a CRC32 integrity check, and an atomic write that never corrupts an existing good file.
    </p>
    <p>
        It is the embedded persistence layer, generic over any type that implements <code>iqdb_index::Index</code>, and designed to sit on the <code>storage-io</code> substrate (the renamed <code>fsys-rs</code>) rather than touching files directly.
    </p>
    <br>
    <hr>
    <p>
        <strong>MSRV is 1.87+</strong> (Rust 2024 edition). Atomic saves. Versioned, portable on-disk format. CRC32 integrity.
    </p>
    <blockquote>
        <strong>Status: pre-1.0, in active development.</strong> The public API is being designed across the 0.x series and frozen at <code>1.0.0</code>. See <a href="./CHANGELOG.md"><code>CHANGELOG.md</code></a> and the <a href="./dev/ROADMAP.md"><code>ROADMAP</code></a>.
    </blockquote>
</div>

<hr>
<br>

<h2>What it does</h2>

- **Atomic snapshot save/load** &mdash; write-to-temp + `fsync` + atomic rename + directory `fsync`; an interrupted write never corrupts an existing good file.
- **Versioned header** &mdash; magic bytes, format version, index-type tag, dim, metric, vector count. All sizes are fixed-width little-endian `u64`, so a file is portable across 32- and 64-bit hosts.
- **CRC32 integrity** &mdash; computed over the payload; a single-bit flip surfaces as `ChecksumMismatch` on load, never a panic or a silently-wrong result.
- **Write-ahead log & crash recovery** &mdash; with `wal_enabled`, every `insert` / `delete` is logged and `fsync`ed *before* it touches memory, then replayed onto the snapshot on `load`. A crash mid-append leaves a torn tail that recovery detects and discards.
- **Optional compression** &mdash; Zstd or LZ4 on the snapshot payload, behind the `zstd` / `lz4` cargo features. The CRC32 covers the compressed bytes, so corruption is caught before decompression.
- **Generic over the index** &mdash; `PersistedIndex<I: Index + Persistable>` wraps any concrete index; the framing lives here, the payload bytes live in the index's `Persistable` impl.

<br>

## Installation

```toml
[dependencies]
iqdb-persist = "0.4"
iqdb-index   = "1.0"
iqdb-types   = "1.0"
```

Snapshot compression is opt-in via cargo features (off by default):

```toml
iqdb-persist = { version = "0.4", features = ["zstd", "lz4"] }
```

<br>

## Quick start

```rust
use iqdb_persist::{PersistConfig, PersistedIndex};

// `MyIndex: iqdb_index::Index + iqdb_persist::Persistable`
let cfg = PersistConfig::new("/var/lib/app/index.iqdb");

// Wrap an in-memory index and save it atomically.
let wrapped = PersistedIndex::open_with(my_index, cfg.clone())?;
wrapped.save()?;

// Later — reconstruct it from disk (verifies magic, version, type, CRC32).
let restored: PersistedIndex<MyIndex> = PersistedIndex::load(cfg)?;
let index = restored.index();
```

For durable, crash-recoverable mutation, set `cfg.wal_enabled = true` and
mutate through the wrapper — each op is logged before it is applied, and
`checkpoint()` folds the log back into a fresh snapshot:

```rust
let mut db = PersistedIndex::open_with(my_index, cfg.clone())?; // writes base snapshot + opens WAL
db.insert(id, vector, metadata)?;   // logged + fsynced, then applied
db.delete(&other_id)?;
db.checkpoint()?;                    // snapshot the state, truncate the WAL
// after a crash: PersistedIndex::load(cfg) replays the WAL onto the snapshot
```

An index opts in by implementing the two-method `Persistable` trait. A
complete, runnable version lives in
[`examples/save_and_load.rs`](./examples/save_and_load.rs) &mdash; run it
with `cargo run --example save_and_load`. Full reference:
[`docs/API.md`](./docs/API.md).

<br>

## Status

This is <code>v0.4.0</code>: atomic snapshot save/load + versioned header + CRC32 (v0.2), the write-ahead log with replay and crash recovery (v0.3), and optional Zstd/LZ4 snapshot compression (v0.4) are implemented and tested. The <strong>feature set is now frozen</strong>; the external `storage-io` substrate integrates and the public API + on-disk format freeze at v0.5 per the <a href="./dev/ROADMAP.md"><code>ROADMAP</code></a>.

<hr>
<br>

## Where It Fits

`iqdb-persist` is the embedded persistence crate of the iQDB family. It builds on:

- `iqdb-types` &mdash; core vocabulary (`DistanceMetric`, `IqdbError`)
- `iqdb-index` &mdash; the `Index` / `IndexCore` traits it wraps as persistable

Snapshot file I/O goes through a tiny internal `Storage` seam so the future `storage-io` substrate (the `fsys-rs` rename) can drop in unchanged; v0.3 ships one impl over `std::fs`, and the WAL appends through its own `std::fs` handle until that substrate lands in v0.5.

<br>

## Contributing

See <a href="./dev/DIRECTIVES.md"><code>dev/DIRECTIVES.md</code></a> for engineering standards and the definition of done. Before a PR: `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all-features` must be clean.

<br>

<div id="license">
    <h2>License</h2>
    <p>Licensed under either of</p>
    <ul>
        <li><b>Apache License, Version 2.0</b> &mdash; <a href="./LICENSE-APACHE">LICENSE-APACHE</a></li>
        <li><b>MIT License</b> &mdash; <a href="./LICENSE-MIT">LICENSE-MIT</a></li>
    </ul>
    <p>at your option.</p>
</div>

<div align="center">
  <h2></h2>
  <sup>COPYRIGHT <small>&copy;</small> 2026 <strong>JAMES GOBER.</strong></sup>
</div>
