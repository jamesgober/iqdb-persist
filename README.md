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
        <strong>iqdb-persist</strong> is what moves iQDB from demo-only to actually usable: it serializes indexes and vectors to disk, logs mutations to a WAL, and recovers cleanly after a crash.
    </p>
    <p>
        It is the embedded persistence layer and is designed to sit on the <code>storage-io</code> substrate (the renamed <code>fsys-rs</code>) rather than touching files directly.
    </p>
    <br>
    <hr>
    <p>
        <strong>MSRV is 1.87+</strong> (Rust 2024 edition). Atomic saves. WAL recovery. Versioned on-disk format.
    </p>
    <blockquote>
        <strong>Status: pre-1.0, in active development.</strong> The public API is being designed across the 0.x series and frozen at <code>1.0.0</code>. See <a href="./CHANGELOG.md"><code>CHANGELOG.md</code></a>.
    </blockquote>
</div>

<hr>
<br>

<h2>What it does</h2>

- **On-disk format** &mdash; versioned file header, magic, CRC32 integrity per index type
- **Atomic save/load** &mdash; write-to-temp-then-rename; a partial write never corrupts data
- **WAL** &mdash; write-ahead log: every mutation logged before memory, replayed on startup
- **Crash recovery** &mdash; detect partial writes, replay the WAL
- **Optional compression** &mdash; zstd or lz4


<br>

## Installation

```toml
[dependencies]
iqdb-persist = "0.1"
```

<br>

## Status

This is the <code>v0.1.0</code> scaffold: structure, tooling, and quality gates are in place; the implementation lands across the 0.x series per the <a href="./dev/ROADMAP.md"><code>ROADMAP</code></a> and <a href="./docs/API.md"><code>docs/API.md</code></a>.

<hr>
<br>

## Where It Fits

`iqdb-persist` is a Phase-5 embedded crate. It builds on:

- `iqdb-types` &mdash; core types
- `iqdb-index` &mdash; wraps any index as persistable
- `storage-io` &mdash; disk I/O substrate (integration pending the fsys-rs rename)

Storage substrate (`storage-io`) wiring is pending the `fsys-rs` -> `storage-io` rename; the scaffold builds without it.

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
