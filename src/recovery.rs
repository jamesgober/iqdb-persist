//! Crash-recovery support — empty scaffold in v0.2.
//!
//! Recovery beyond "atomic snapshot writes don't corrupt existing files"
//! lands in **v0.3** alongside the WAL. This module will own the
//! startup logic: open the snapshot, detect a partial write, replay any
//! unprocessed WAL entries.
//!
//! v0.2's atomicity story is the temp-file + fsync + atomic-rename +
//! dir-fsync path implemented in the internal `Storage::write_atomic` —
//! sufficient to guarantee an interrupted save never corrupts an
//! existing good file, which is the v0.2 exit criterion.
