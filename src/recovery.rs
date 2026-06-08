//! Crash recovery: replay the WAL onto a loaded snapshot.
//!
//! [`crate::PersistedIndex::load`] reconstructs the index from the most
//! recent snapshot, then calls [`replay`] to re-apply every mutation the
//! WAL recorded *after* that snapshot. Together they realise the recovery
//! contract from `dev/DIRECTIVES.md`: a mutation acknowledged to the caller
//! was logged before it was applied, so it survives a crash and is restored
//! here.
//!
//! A torn tail in the WAL — the signature of a crash mid-append — is
//! discarded by [`crate::wal::parse_records`]; the mutation it represents
//! was never acknowledged, so dropping it is correct.

use std::path::Path;

use iqdb_index::IndexCore;

use crate::error::{PersistError, Result};
use crate::wal::{self, WalRecord};

/// Replay every committed WAL frame beside `snapshot` onto `index`,
/// returning the number of records applied.
///
/// A missing WAL file replays nothing (`Ok(0)`). Each insert's vector
/// dimension is cross-checked against the index; a mismatch is a corrupt
/// log and surfaces as [`PersistError::InvalidPayload`]. A downstream
/// `insert` / `delete` error surfaces as [`PersistError::IndexBuild`].
pub(crate) fn replay<I: IndexCore>(snapshot: &Path, index: &mut I) -> Result<usize> {
    let path = wal::wal_path(snapshot);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => return Err(PersistError::Io { path, source }),
    };

    let records = wal::parse_records(&bytes)?;
    let mut applied = 0usize;
    for record in records {
        match record {
            WalRecord::Insert { id, vector, meta } => {
                if vector.len() != index.dim() {
                    return Err(PersistError::InvalidPayload {
                        reason: "WAL insert vector dimension disagrees with the index",
                    });
                }
                index.insert(id, vector, meta)?;
            }
            WalRecord::Delete { id } => {
                index.delete(&id)?;
            }
        }
        applied += 1;
    }
    Ok(applied)
}
