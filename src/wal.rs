//! Write-ahead log support — empty scaffold in v0.2.
//!
//! WAL support lands in **v0.3**. This module owns the append-only
//! mutation log that pairs with snapshot writes for crash recovery:
//! every mutation hits the WAL first, then memory; on startup the WAL
//! is replayed onto the most recent snapshot.
//!
//! v0.2 deliberately ships **no live API** here. Setting
//! [`crate::PersistConfig::wal_enabled`] to `true` in v0.2 surfaces as
//! [`crate::PersistError::Unsupported`] at
//! [`crate::PersistedIndex::open_with`] / [`crate::PersistedIndex::load`]
//! construction — never as a panic, never as a silent no-op.
//!
//! See `CHANGELOG.md` for the WAL surface that will land in v0.3.
