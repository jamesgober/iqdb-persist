//! Payload compression — empty scaffold in v0.2.
//!
//! Compression support lands in **v0.4**. This module will own the
//! Zstd and LZ4 codecs that wrap the [`crate::Persistable`] payload
//! before it is handed to the storage substrate, transparently to the
//! trait impl.
//!
//! v0.2 ships [`crate::Compression::None`] only. Setting
//! [`crate::PersistConfig::compression`] to [`crate::Compression::Zstd`]
//! or [`crate::Compression::Lz4`] in v0.2 surfaces as
//! [`crate::PersistError::Unsupported`] at
//! [`crate::PersistedIndex::open_with`] / [`crate::PersistedIndex::load`]
//! construction — never as a panic, never as a silent no-op.
