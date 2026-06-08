//! Snapshot-payload compression codecs.
//!
//! Compression wraps the [`crate::Persistable`] payload before it is framed
//! and written, transparently to the trait impl. It applies only to
//! snapshots, not the WAL (per-record compression of tiny mutations is not
//! worthwhile). The chosen scheme is recorded in a one-byte tag at the
//! front of the format-v2 payload region (see [`crate::format`]), so
//! [`crate::PersistedIndex::load`] knows how to decode it.
//!
//! Both codecs are behind cargo features (`zstd`, `lz4`), off by default.
//! Selecting a [`crate::Compression`] scheme whose feature is not compiled
//! in is reported as [`crate::PersistError::Unsupported`] rather than a
//! panic or a silent fallback.

use std::borrow::Cow;

use crate::config::Compression;
use crate::error::{PersistError, Result};

/// On-disk scheme tag: no compression (payload stored verbatim).
pub(crate) const SCHEME_NONE: u8 = 0;
/// On-disk scheme tag: Zstandard.
pub(crate) const SCHEME_ZSTD: u8 = 1;
/// On-disk scheme tag: LZ4 (block format).
pub(crate) const SCHEME_LZ4: u8 = 2;

/// Upper bound on the decompression expansion ratio. A payload region that
/// claims to decode to more than `compressed_len * MAX_DECOMPRESS_RATIO`
/// bytes is rejected as a decompression bomb. The bound scales with the
/// file, so it never limits a legitimately large snapshot (whose ratio is
/// small) while capping an absurd claim from a crafted header.
const MAX_DECOMPRESS_RATIO: usize = 4096;

/// The on-disk scheme tag for a [`Compression`] setting.
pub(crate) fn scheme_tag(scheme: Compression) -> u8 {
    match scheme {
        Compression::None => SCHEME_NONE,
        Compression::Zstd { .. } => SCHEME_ZSTD,
        Compression::Lz4 => SCHEME_LZ4,
    }
}

/// Compress `raw` per `scheme`. `None` borrows `raw` unchanged; the codecs
/// return owned buffers.
///
/// # Errors
///
/// [`PersistError::Unsupported`] if the scheme's cargo feature is not
/// compiled in; [`PersistError::Compression`] if the codec rejects the
/// input (for example a Zstd level outside `1..=22`).
pub(crate) fn encode(scheme: Compression, raw: &[u8]) -> Result<Cow<'_, [u8]>> {
    match scheme {
        Compression::None => Ok(Cow::Borrowed(raw)),
        Compression::Zstd { level } => encode_zstd(raw, level),
        Compression::Lz4 => encode_lz4(raw),
    }
}

/// Decompress the `data` of a payload region whose scheme tag is `tag` and
/// whose recorded uncompressed length is `uncompressed_len`.
///
/// # Errors
///
/// [`PersistError::InvalidPayload`] for an unknown tag or a
/// decompression-bomb-sized claim; [`PersistError::Unsupported`] if the
/// scheme's feature is not compiled in; [`PersistError::Compression`] if
/// the codec fails or the decoded length does not match.
pub(crate) fn decode(tag: u8, data: &[u8], uncompressed_len: usize) -> Result<Vec<u8>> {
    if tag != SCHEME_NONE && uncompressed_len > data.len().saturating_mul(MAX_DECOMPRESS_RATIO) {
        return Err(PersistError::InvalidPayload {
            reason: "declared uncompressed size exceeds the decompression-ratio guard",
        });
    }
    match tag {
        SCHEME_NONE => Ok(data.to_vec()),
        SCHEME_ZSTD => decode_zstd(data, uncompressed_len),
        SCHEME_LZ4 => decode_lz4(data, uncompressed_len),
        _ => Err(PersistError::InvalidPayload {
            reason: "unknown compression scheme tag",
        }),
    }
}

// -- Zstd --------------------------------------------------------------------

#[cfg(feature = "zstd")]
fn encode_zstd(raw: &[u8], level: i32) -> Result<Cow<'_, [u8]>> {
    if !(1..=22).contains(&level) {
        return Err(PersistError::Compression {
            reason: "zstd level must be in 1..=22",
        });
    }
    zstd::encode_all(raw, level)
        .map(Cow::Owned)
        .map_err(|_| PersistError::Compression {
            reason: "zstd compression failed",
        })
}

#[cfg(not(feature = "zstd"))]
fn encode_zstd(_raw: &[u8], _level: i32) -> Result<Cow<'_, [u8]>> {
    Err(PersistError::Unsupported {
        feature: "Zstd compression",
        available_in: "the `zstd` cargo feature",
    })
}

#[cfg(feature = "zstd")]
fn decode_zstd(data: &[u8], uncompressed_len: usize) -> Result<Vec<u8>> {
    let out = zstd::decode_all(data).map_err(|_| PersistError::Compression {
        reason: "zstd decompression failed",
    })?;
    if out.len() != uncompressed_len {
        return Err(PersistError::Compression {
            reason: "zstd decompressed length does not match the recorded length",
        });
    }
    Ok(out)
}

#[cfg(not(feature = "zstd"))]
fn decode_zstd(_data: &[u8], _uncompressed_len: usize) -> Result<Vec<u8>> {
    Err(PersistError::Unsupported {
        feature: "Zstd decompression",
        available_in: "the `zstd` cargo feature",
    })
}

// -- LZ4 ---------------------------------------------------------------------

#[cfg(feature = "lz4")]
fn encode_lz4(raw: &[u8]) -> Result<Cow<'_, [u8]>> {
    Ok(Cow::Owned(lz4_flex::block::compress(raw)))
}

#[cfg(not(feature = "lz4"))]
fn encode_lz4(_raw: &[u8]) -> Result<Cow<'_, [u8]>> {
    Err(PersistError::Unsupported {
        feature: "LZ4 compression",
        available_in: "the `lz4` cargo feature",
    })
}

#[cfg(feature = "lz4")]
fn decode_lz4(data: &[u8], uncompressed_len: usize) -> Result<Vec<u8>> {
    let out = lz4_flex::block::decompress(data, uncompressed_len).map_err(|_| {
        PersistError::Compression {
            reason: "lz4 decompression failed",
        }
    })?;
    if out.len() != uncompressed_len {
        return Err(PersistError::Compression {
            reason: "lz4 decompressed length does not match the recorded length",
        });
    }
    Ok(out)
}

#[cfg(not(feature = "lz4"))]
fn decode_lz4(_data: &[u8], _uncompressed_len: usize) -> Result<Vec<u8>> {
    Err(PersistError::Unsupported {
        feature: "LZ4 decompression",
        available_in: "the `lz4` cargo feature",
    })
}

#[cfg(all(test, any(feature = "zstd", feature = "lz4")))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn round_trip(scheme: Compression) {
        let raw: Vec<u8> = (0..4096u32).flat_map(|n| n.to_le_bytes()).collect();
        let encoded = encode(scheme, &raw).unwrap();
        let decoded = decode(scheme_tag(scheme), &encoded, raw.len()).unwrap();
        assert_eq!(decoded, raw);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_round_trips() {
        round_trip(Compression::Zstd { level: 3 });
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_rejects_bad_level() {
        assert!(matches!(
            encode(Compression::Zstd { level: 99 }, b"data"),
            Err(PersistError::Compression { .. })
        ));
    }

    #[cfg(feature = "lz4")]
    #[test]
    fn lz4_round_trips() {
        round_trip(Compression::Lz4);
    }

    #[test]
    fn none_is_identity() {
        let raw = b"verbatim";
        let encoded = encode(Compression::None, raw).unwrap();
        assert_eq!(&*encoded, raw);
        assert_eq!(decode(SCHEME_NONE, raw, raw.len()).unwrap(), raw);
    }

    #[cfg(feature = "lz4")]
    #[test]
    fn decompression_bomb_claim_is_rejected() {
        let encoded = encode(Compression::Lz4, b"small").unwrap();
        let huge = encoded.len() * MAX_DECOMPRESS_RATIO + 1;
        assert!(matches!(
            decode(SCHEME_LZ4, &encoded, huge),
            Err(PersistError::InvalidPayload { .. })
        ));
    }
}
