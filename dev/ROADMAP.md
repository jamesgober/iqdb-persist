# iqdb-persist -- Roadmap

> Path from scaffold to a stable 1.0. Hard parts are front-loaded; each phase has hard exit criteria.
>
> **Anti-deferral rule:** no listed hard task moves to a later phase unless this file records the move and the reason.

---

## v0.1.0 -- Scaffold (DONE)

Compiles, CI green, structure correct, no domain logic.

- [x] Manifest, README, CHANGELOG, REPS, license, CI, lints in place.
- [x] API surface sketched in `docs/API.md`.

---

## v0.2.0 -- on-disk format + atomic save/load + CRC32 (THE HARD PART, NOT DEFERRED) (DONE)

Exit criteria:
- [x] Every public item has rustdoc + a runnable example.
- [x] Core invariants property-tested.

---

## v0.3.0 -- WAL append + replay + crash recovery (DONE)

Exit criteria:
- [x] New surface tested and benchmarked where it is a hot path.

---

## v0.4.0 -- optional zstd/lz4 compression + feature freeze (DONE)

Exit criteria:
- [x] No `todo!`/`unimplemented!`. Feature freeze declared.

Feature freeze: as of v0.4 the persistence feature set is complete —
snapshots + header + CRC32 (v0.2), WAL + replay + crash recovery (v0.3),
and optional Zstd/LZ4 snapshot compression (v0.4). No new features land
before 1.0; remaining work is the API/format freeze + adversarial hardening
(v0.5) and the alpha/beta/rc series (0.6–0.9). `storage-io` integration is
deferred behind the internal `Storage` seam (see v0.5).

---

## v0.5.0 -- adversarial/partial-write tests + API freeze + format freeze (DONE)

Exit criteria:
- [x] Public API frozen (recorded below). `cargo audit` + `cargo deny` clean.
- [x] Adversarial/partial-write hardening: exhaustive single-byte-flip and
      truncation of snapshot and WAL files, plus pseudo-random garbage —
      the loader never panics, never OOMs, and never returns a
      silently-wrong result.

### Deferred (anti-deferral record)

`storage-io` integration is **deferred past 1.0 with rationale**: the
substrate is the renamed `fsys-rs`, and that rename has not happened
(the crate is still `fsys` 1.1.0). "Out of scope for 1.0" already lists the
substrate itself. The internal `Storage` trait is the swap seam and is in
place; when `storage-io` ships, snapshot I/O moves behind it and the WAL's
`std::fs` handle follows — an internal change, not an API break.

### Frozen public API (SemVer 1.x surface)

- `trait Persistable: Index` — `const INDEX_TYPE`, `save_to`, `load_from`.
- `struct PersistedIndex<I>` — `open_with`, `load`, `index`, `index_mut`,
  `config`, `insert`, `delete`, `save`, `checkpoint`.
- `struct PersistConfig { path, wal_enabled, fsync_policy, compression }`
  — `new`, `Default`.
- `enum FsyncPolicy { Always, Periodic(Duration), Never }`.
- `enum Compression { None, Zstd { level }, Lz4 }`.
- `enum PersistError` (`#[non_exhaustive]`) + `type Result<T>`.
- `struct FileHeader`, `const MAGIC`, `const CURRENT_VERSION`,
  `mod format` (`read_header`, `write_header`), `mod checksum`
  (`compute`, `verify`), `const VERSION`.

### Frozen on-disk format

Snapshot format **version 2** (header layout + compression preamble +
payload CRC32) and the **WAL format** (`IQDBWAL\0` framing, per-record
CRC32) are frozen; the metric-tag mapping (`0..=4`) is fixed. The reader
keeps accepting format v1 for backward compatibility. Any future change
goes through a version bump, never a silent reinterpretation.

---

## v0.6.0 -> v0.9.x -- Alpha / Beta -> RC

- 0.6.x-0.7.x: integrate against real consumers; MINOR-compatible additions only.
- 0.8.x (beta): bug fixes; broader testing; final benchmarks.
- 0.9.x (rc): critical fixes + doc polish.

---

## v1.0.0 -- Stable

- [ ] Definition of Done (DIRECTIVES section 7) satisfied.
- [ ] Public API frozen until 2.0.
- [ ] Release note written; published to crates.io; tag pushed.

---

## Out of scope for 1.0

- The storage substrate itself -- that is `storage-io`.
- Replication/consensus -- distributed phase.
