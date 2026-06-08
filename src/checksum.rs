//! CRC32 helpers for snapshot integrity.
//!
//! Wraps [`crc32fast`] behind a tiny module-local surface so the rest of
//! `iqdb-persist` does not name the crate directly. CRC32 is computed
//! over the **payload bytes only** — not the file header — so a header
//! field can be rewritten without recomputing the checksum and so a
//! single-bit flip in the payload always surfaces as
//! [`crate::PersistError::ChecksumMismatch`].

use crate::error::{PersistError, Result};

/// Compute the CRC32 of `bytes` using the IEEE polynomial.
///
/// # Examples
///
/// ```
/// use iqdb_persist::checksum;
///
/// let empty = checksum::compute(&[]);
/// let one_byte = checksum::compute(&[0x00]);
/// assert_ne!(empty, one_byte);
/// ```
#[must_use]
pub fn compute(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

/// Verify that the CRC32 of `bytes` matches `expected`.
///
/// # Errors
///
/// Returns [`PersistError::ChecksumMismatch`] when the computed and
/// expected values differ. Never panics, never returns silently-wrong
/// data.
///
/// # Examples
///
/// ```
/// use iqdb_persist::checksum;
///
/// let payload = b"hello";
/// let crc = checksum::compute(payload);
/// assert!(checksum::verify(payload, crc).is_ok());
/// assert!(checksum::verify(payload, crc.wrapping_add(1)).is_err());
/// ```
pub fn verify(bytes: &[u8], expected: u32) -> Result<()> {
    let computed = compute(bytes);
    if computed == expected {
        Ok(())
    } else {
        Err(PersistError::ChecksumMismatch { expected, computed })
    }
}
