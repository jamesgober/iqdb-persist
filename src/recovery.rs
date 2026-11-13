//! Crash recovery: replay the WAL onto a loaded snapshot.
//!
//! [`crate::PersistedIndex::load`] reconstructs the index from the most
//! recent snapshot, then calls [`replay`] to re-apply every mutation the
//! WAL recorded *after* that snapshot. Together they realise the recovery
//! contract from `dev/DIRECTIVES.md`: a mutation acknowledged to the caller
//! was logged before it was applied, so it survives a crash and is restored
//! here.
//!
//! A torn tail in the WAL (the signature of a crash mid-append) is
//! discarded by [`crate::wal::parse_log`]; the mutation it represents was
//! never acknowledged, so dropping it is correct. The intact length it
//! reports lets the caller cut the torn bytes off before appending again.
//!
//! ## Replaying onto a newer snapshot
//!
//! A crash between a checkpoint's snapshot rename and its log reset, or a
//! plain [`crate::PersistedIndex::save`] in WAL mode, leaves a snapshot that
//! already holds some or all of the logged mutations. Replay is written to
//! be safe in that case: an insert the index rejects as
//! [`IqdbError::Duplicate`] and a delete it rejects as
//! [`IqdbError::NotFound`] are already reflected in the snapshot and are
//! skipped. The log only ever holds mutations that applied cleanly when
//! they were made, so for each id it alternates between inserts and
//! deletes, and skipping those two rejections reaches the same final state
//! whichever prefix of the log the snapshot already covers.

use std::path::Path;

use iqdb_index::IndexCore;
use iqdb_types::IqdbError;

use crate::error::{PersistError, Result};
use crate::wal::{self, WalRecord};

/// What [`replay`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Replayed {
    /// Records applied to the index.
    pub(crate) applied: usize,
    /// Records skipped because the snapshot already reflected them.
    pub(crate) skipped: usize,
    /// Byte length of the intact log prefix, or `None` when there was no
    /// log file. Everything past it is a torn tail.
    pub(crate) valid_len: Option<u64>,
}

/// Replay every committed WAL frame beside `snapshot` onto `index`.
///
/// A missing WAL file replays nothing. Each insert's vector dimension is
/// cross-checked against the index; a mismatch is a corrupt log and
/// surfaces as [`PersistError::InvalidPayload`]. A duplicate insert or an
/// absent-id delete is skipped (see the module docs); any other downstream
/// `insert` / `delete` error surfaces as [`PersistError::IndexBuild`].
pub(crate) fn replay<I: IndexCore>(snapshot: &Path, index: &mut I) -> Result<Replayed> {
    let path = wal::wal_path(snapshot);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Replayed {
                applied: 0,
                skipped: 0,
                valid_len: None,
            });
        }
        Err(source) => return Err(PersistError::Io { path, source }),
    };

    let (records, valid_len) = wal::parse_log(&bytes)?;
    let mut applied = 0usize;
    let mut skipped = 0usize;
    for record in records {
        let outcome = match record {
            WalRecord::Insert { id, vector, meta } => {
                if vector.len() != index.dim() {
                    return Err(PersistError::InvalidPayload {
                        reason: "WAL insert vector dimension disagrees with the index",
                    });
                }
                match index.insert(id, vector, meta) {
                    Err(IqdbError::Duplicate) => None,
                    other => Some(other),
                }
            }
            WalRecord::Delete { id } => match index.delete(&id) {
                Err(IqdbError::NotFound) => None,
                other => Some(other),
            },
        };
        match outcome {
            Some(result) => {
                result?;
                applied += 1;
            }
            None => skipped += 1,
        }
    }
    Ok(Replayed {
        applied,
        skipped,
        valid_len: Some(valid_len as u64),
    })
}
