//! The internal storage substrate.
//!
//! Every byte of file I/O in `iqdb-persist` goes through the
//! [`Storage`] trait, so when the `storage-io` crate (v0.5+) lands it
//! can drop in unchanged. v0.2 ships exactly one impl: [`StdFsStorage`]
//! over [`std::fs`].
//!
//! ## Why path-based, not stream-based
//!
//! v0.2 indexes fit in RAM (a `FlatIndex`'s state is already a `Vec`),
//! so buffering the serialized form into a `Vec<u8>` before the atomic
//! move is fine and keeps the trait tiny.
//!
//! ## Atomic write contract
//!
//! [`Storage::write_atomic`] MUST be implemented as
//! **temp file + fsync + rename + dir fsync**, so an interrupted write
//! never corrupts an existing good file.
//!
//! The directory-fsync step is POSIX-specific: it makes the rename
//! itself durable on Linux/macOS by flushing the parent directory
//! inode. Windows exposes no portable directory fsync through `std` —
//! `File::open` on a directory handle fails with
//! `ERROR_ACCESS_DENIED` — and NTFS journals directory metadata
//! through a separate mechanism, so skipping the step on non-unix
//! targets is correct, not a durability regression.

#[cfg(unix)]
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use crate::config::FsyncPolicy;
use crate::error::{PersistError, Result};

/// The minimal file-I/O surface `iqdb-persist` needs.
///
/// `pub(crate)` — this trait is the substrate seam for the future
/// `storage-io` swap, not a v0.2 public extension point.
pub(crate) trait Storage: Send + Sync {
    /// Read an entire file into memory.
    fn read_all(&self, path: &Path) -> Result<Vec<u8>>;

    /// Write `payload` atomically to `target` (temp + fsync + rename +
    /// dir fsync).
    fn write_atomic(&self, target: &Path, payload: &[u8], policy: FsyncPolicy) -> Result<()>;
}

/// The `std::fs`-backed [`Storage`] impl shipped in v0.2.
pub(crate) struct StdFsStorage;

impl Storage for StdFsStorage {
    fn read_all(&self, path: &Path) -> Result<Vec<u8>> {
        std::fs::read(path).map_err(|source| PersistError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    fn write_atomic(&self, target: &Path, payload: &[u8], policy: FsyncPolicy) -> Result<()> {
        let target_dir = target.parent().unwrap_or_else(|| Path::new("."));
        let file_name = target.file_name().ok_or_else(|| PersistError::Io {
            path: target.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "target has no file name",
            ),
        })?;

        // Build a temp name that is unique enough across concurrent saves
        // and pid recycling. `SystemTime::now()` is non-monotonic, but
        // collisions only need to be improbable enough that `create_new`
        // catches them at file-creation time.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let temp_name = format!("{}.tmp.{pid}.{nanos}", file_name.to_string_lossy());
        let temp_path = target_dir.join(&temp_name);

        // Phase 1: write the temp file.
        {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp_path)
                .map_err(|source| PersistError::Io {
                    path: temp_path.clone(),
                    source,
                })?;
            if let Err(source) = file.write_all(payload) {
                drop(file);
                let _cleanup = std::fs::remove_file(&temp_path);
                return Err(PersistError::Io {
                    path: temp_path,
                    source,
                });
            }
            if policy != FsyncPolicy::Never {
                if let Err(source) = file.sync_all() {
                    drop(file);
                    let _cleanup = std::fs::remove_file(&temp_path);
                    return Err(PersistError::Io {
                        path: temp_path,
                        source,
                    });
                }
            }
            // file dropped here -> closed before the rename
        }

        // Phase 2: atomic rename. POSIX rename(2) replaces the target
        // atomically when source and destination are on the same
        // filesystem (which they are, since the temp file lives next to
        // the target).
        if let Err(source) = std::fs::rename(&temp_path, target) {
            let _cleanup = std::fs::remove_file(&temp_path);
            return Err(PersistError::Io {
                path: temp_path,
                source,
            });
        }

        // Phase 3: fsync the directory so the rename itself is durable.
        //
        // POSIX-only. On Linux/macOS this opens the parent directory and
        // flushes its inode so the just-completed rename survives a
        // crash. On Windows `File::open` against a directory returns
        // `ERROR_ACCESS_DENIED` (std exposes no portable directory
        // fsync), and NTFS journals directory metadata separately, so
        // the step is deliberately a no-op there. `FsyncPolicy::Always`
        // therefore means "fsync everything `std` lets us fsync" on
        // both platforms; the POSIX byte sequence is unchanged.
        #[cfg(unix)]
        {
            if policy != FsyncPolicy::Never {
                let dir = File::open(target_dir).map_err(|source| PersistError::Io {
                    path: target_dir.to_path_buf(),
                    source,
                })?;
                dir.sync_all().map_err(|source| PersistError::Io {
                    path: target_dir.to_path_buf(),
                    source,
                })?;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = policy;
            let _ = target_dir;
        }

        Ok(())
    }
}
