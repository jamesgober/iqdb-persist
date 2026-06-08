//! Write-ahead log: append, `fsync`, and reset.
//!
//! The WAL is the durability path *between* snapshots. With
//! [`crate::PersistConfig::wal_enabled`] set, every mutation made through
//! [`crate::PersistedIndex::insert`] / [`crate::PersistedIndex::delete`] is
//! appended here **before** it is applied in memory — the recovery
//! contract from `dev/DIRECTIVES.md`. On restart,
//! [`crate::PersistedIndex::load`] replays the log onto the most recent
//! snapshot (see [`crate::recovery`]).
//!
//! ## File layout
//!
//! A WAL file is a fixed header followed by a sequence of self-checked
//! frames:
//!
//! ```text
//! header (12 bytes):
//!   0   8   magic ("IQDBWAL\0")
//!   8   4   version (u32 LE)
//! then, repeated:
//!   +0  4   record length, L (u32 LE)
//!   +4  4   crc32 of the record bytes (u32 LE)
//!   +8  L   record (op byte + operands)
//! ```
//!
//! Each frame carries its own length and CRC32, so a crash mid-append
//! leaves a **torn tail** that replay detects and discards: the truncated
//! or mis-checksummed final frame was never durably committed, so dropping
//! it is correct. Frames before it are intact and replay in order.
//!
//! ## Record bodies
//!
//! ```text
//! op = 1 (insert): vector_id, vec_len (u32 LE), vec_len × f32 LE,
//!                  has_meta (u8), [n_entries (u32 LE),
//!                  (key_len u32 LE, key utf8, value)…]
//! op = 2 (delete): vector_id
//!
//! vector_id: kind (u8: 0 = U64, 1 = Bytes)
//!            U64   -> u64 LE
//!            Bytes -> len (u64 LE) + bytes
//! value:     tag (u8) 0 String(len u32 + utf8) | 1 Int(i64 LE)
//!                     | 2 Float(f64 LE) | 3 Bool(u8) | 4 Null
//! ```

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use iqdb_types::{Metadata, Value, VectorId};

use crate::checksum;
use crate::config::FsyncPolicy;
use crate::error::{PersistError, Result};

/// Magic bytes that prefix every WAL file (distinct from the snapshot
/// [`crate::MAGIC`]).
pub(crate) const WAL_MAGIC: [u8; 8] = *b"IQDBWAL\0";

/// On-disk WAL format version this build writes and reads.
pub(crate) const WAL_VERSION: u32 = 1;

const OP_INSERT: u8 = 1;
const OP_DELETE: u8 = 2;

const VID_U64: u8 = 0;
const VID_BYTES: u8 = 1;

const VAL_STRING: u8 = 0;
const VAL_INT: u8 = 1;
const VAL_FLOAT: u8 = 2;
const VAL_BOOL: u8 = 3;
const VAL_NULL: u8 = 4;

/// Cap on a single WAL record so a corrupt length field cannot trigger a
/// giant read. 256 MiB is far above any realistic single vector + metadata.
const MAX_RECORD_LEN: usize = 256 * 1024 * 1024;

/// Derive the WAL path that sits beside a snapshot file: the snapshot path
/// with `.wal` appended (for example `index.iqdb` -> `index.iqdb.wal`).
pub(crate) fn wal_path(snapshot: &Path) -> PathBuf {
    let mut s = snapshot.as_os_str().to_os_string();
    s.push(".wal");
    PathBuf::from(s)
}

/// A live, append-positioned write-ahead log.
pub(crate) struct Wal {
    file: File,
    path: PathBuf,
    policy: FsyncPolicy,
    last_fsync: Option<Instant>,
    // Reused across appends so the steady-state insert path makes no
    // per-call framing allocation beyond growing this buffer once.
    scratch: Vec<u8>,
}

impl Wal {
    /// Create (or truncate) a fresh WAL beside `snapshot`, write its
    /// header, and return it positioned for appends.
    pub(crate) fn create(snapshot: &Path, policy: FsyncPolicy) -> Result<Self> {
        let path = wal_path(snapshot);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|source| PersistError::Io {
                path: path.clone(),
                source,
            })?;
        let mut wal = Self {
            file,
            path,
            policy,
            last_fsync: None,
            scratch: Vec::with_capacity(256),
        };
        wal.write_header()?;
        wal.sync_now()?;
        Ok(wal)
    }

    /// Open an existing WAL beside `snapshot` for continued appends,
    /// positioned at the end. Used after [`crate::recovery::replay`] on
    /// load. A header is written if the file did not already exist.
    pub(crate) fn open_for_append(snapshot: &Path, policy: FsyncPolicy) -> Result<Self> {
        let path = wal_path(snapshot);
        let existed = path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| PersistError::Io {
                path: path.clone(),
                source,
            })?;
        let _end = file
            .seek(SeekFrom::End(0))
            .map_err(|source| PersistError::Io {
                path: path.clone(),
                source,
            })?;
        let mut wal = Self {
            file,
            path,
            policy,
            last_fsync: None,
            scratch: Vec::with_capacity(256),
        };
        if !existed {
            wal.write_header()?;
            wal.sync_now()?;
        }
        Ok(wal)
    }

    /// Truncate the WAL back to just its header. Called after a snapshot
    /// checkpoint makes every logged mutation redundant.
    pub(crate) fn reset(&mut self) -> Result<()> {
        self.file.set_len(0).map_err(|source| self.io(source))?;
        let _pos = self
            .file
            .seek(SeekFrom::Start(0))
            .map_err(|source| self.io(source))?;
        self.write_header()?;
        self.sync_now()?;
        Ok(())
    }

    /// Append an insert mutation, then `fsync` per policy.
    pub(crate) fn append_insert(
        &mut self,
        id: &VectorId,
        vector: &[f32],
        meta: Option<&Metadata>,
    ) -> Result<()> {
        self.scratch.clear();
        self.scratch.push(OP_INSERT);
        encode_vector_id(&mut self.scratch, id);
        let vec_len = u32::try_from(vector.len()).map_err(|_| PersistError::InvalidPayload {
            reason: "vector length does not fit in u32",
        })?;
        self.scratch.extend_from_slice(&vec_len.to_le_bytes());
        for &component in vector {
            self.scratch.extend_from_slice(&component.to_le_bytes());
        }
        match meta {
            Some(m) => {
                self.scratch.push(1);
                encode_metadata(&mut self.scratch, m)?;
            }
            None => self.scratch.push(0),
        }
        self.commit_record()
    }

    /// Append a delete mutation, then `fsync` per policy.
    pub(crate) fn append_delete(&mut self, id: &VectorId) -> Result<()> {
        self.scratch.clear();
        self.scratch.push(OP_DELETE);
        encode_vector_id(&mut self.scratch, id);
        self.commit_record()
    }

    /// The current end-of-file position, captured before an append so the
    /// caller can [`rollback`](Self::rollback) to it if applying the
    /// just-logged mutation to memory is rejected — keeping the log exactly
    /// in step with the in-memory index.
    pub(crate) fn mark(&mut self) -> Result<u64> {
        self.file
            .stream_position()
            .map_err(|source| self.io(source))
    }

    /// Truncate the WAL back to `mark`, discarding a record that was logged
    /// but whose in-memory apply failed.
    pub(crate) fn rollback(&mut self, mark: u64) -> Result<()> {
        self.file.set_len(mark).map_err(|source| self.io(source))?;
        let _pos = self
            .file
            .seek(SeekFrom::Start(mark))
            .map_err(|source| self.io(source))?;
        self.sync_now()
    }

    fn write_header(&mut self) -> Result<()> {
        let mut header = [0u8; 12];
        header[..8].copy_from_slice(&WAL_MAGIC);
        header[8..].copy_from_slice(&WAL_VERSION.to_le_bytes());
        self.file
            .write_all(&header)
            .map_err(|source| PersistError::Io {
                path: self.path.clone(),
                source,
            })
    }

    /// Frame and write `self.scratch` (the record body): a `len(u32) +
    /// crc32(u32)` head followed by the body, then `fsync` per policy.
    fn commit_record(&mut self) -> Result<()> {
        let body_len =
            u32::try_from(self.scratch.len()).map_err(|_| PersistError::InvalidPayload {
                reason: "WAL record length does not fit in u32",
            })?;
        let crc = checksum::compute(&self.scratch);
        let mut frame_head = [0u8; 8];
        frame_head[..4].copy_from_slice(&body_len.to_le_bytes());
        frame_head[4..].copy_from_slice(&crc.to_le_bytes());
        self.file
            .write_all(&frame_head)
            .map_err(|source| PersistError::Io {
                path: self.path.clone(),
                source,
            })?;
        self.file
            .write_all(&self.scratch)
            .map_err(|source| PersistError::Io {
                path: self.path.clone(),
                source,
            })?;
        self.maybe_sync()
    }

    fn maybe_sync(&mut self) -> Result<()> {
        match self.policy {
            FsyncPolicy::Always => self.sync_now(),
            FsyncPolicy::Never => Ok(()),
            FsyncPolicy::Periodic(interval) => {
                let now = Instant::now();
                let due = match self.last_fsync {
                    Some(last) => now.duration_since(last) >= interval,
                    None => true,
                };
                if due { self.sync_now() } else { Ok(()) }
            }
        }
    }

    fn sync_now(&mut self) -> Result<()> {
        self.file.sync_all().map_err(|source| PersistError::Io {
            path: self.path.clone(),
            source,
        })?;
        self.last_fsync = Some(Instant::now());
        Ok(())
    }

    fn io(&self, source: std::io::Error) -> PersistError {
        PersistError::Io {
            path: self.path.clone(),
            source,
        }
    }
}

fn encode_vector_id(buf: &mut Vec<u8>, id: &VectorId) {
    match id {
        VectorId::U64(n) => {
            buf.push(VID_U64);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        VectorId::Bytes(b) => {
            buf.push(VID_BYTES);
            let len = b.len() as u64;
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(b);
        }
    }
}

fn encode_metadata(buf: &mut Vec<u8>, meta: &Metadata) -> Result<()> {
    let n = u32::try_from(meta.len()).map_err(|_| PersistError::InvalidPayload {
        reason: "metadata entry count does not fit in u32",
    })?;
    buf.extend_from_slice(&n.to_le_bytes());
    for (key, value) in meta.iter() {
        let key_len = u32::try_from(key.len()).map_err(|_| PersistError::InvalidPayload {
            reason: "metadata key length does not fit in u32",
        })?;
        buf.extend_from_slice(&key_len.to_le_bytes());
        buf.extend_from_slice(key.as_bytes());
        encode_value(buf, value)?;
    }
    Ok(())
}

fn encode_value(buf: &mut Vec<u8>, value: &Value) -> Result<()> {
    match value {
        Value::String(s) => {
            buf.push(VAL_STRING);
            let len = u32::try_from(s.len()).map_err(|_| PersistError::InvalidPayload {
                reason: "metadata string length does not fit in u32",
            })?;
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Int(i) => {
            buf.push(VAL_INT);
            buf.extend_from_slice(&i.to_le_bytes());
        }
        Value::Float(f) => {
            buf.push(VAL_FLOAT);
            buf.extend_from_slice(&f.to_le_bytes());
        }
        Value::Bool(b) => {
            buf.push(VAL_BOOL);
            buf.push(u8::from(*b));
        }
        Value::Null => buf.push(VAL_NULL),
    }
    Ok(())
}

/// A decoded WAL mutation, produced by [`parse_records`] for replay.
#[derive(Debug)]
pub(crate) enum WalRecord {
    /// An insert: id, vector, optional metadata.
    Insert {
        /// The vector id.
        id: VectorId,
        /// The vector payload.
        vector: Arc<[f32]>,
        /// Optional metadata.
        meta: Option<Metadata>,
    },
    /// A delete by id.
    Delete {
        /// The vector id to remove.
        id: VectorId,
    },
}

/// Parse every intact frame in a WAL byte image, in append order.
///
/// A torn tail (a final frame that is truncated or fails its CRC32) is
/// silently dropped — it was never durably committed. Returns
/// [`PersistError::BadMagic`] / [`PersistError::UnsupportedVersion`] for a
/// non-WAL or wrong-version file, and [`PersistError::InvalidPayload`] for
/// a frame that passes its CRC32 but does not decode (a corrupt-yet-
/// checksum-valid record — a real defect, not a crash artifact).
pub(crate) fn parse_records(bytes: &[u8]) -> Result<Vec<WalRecord>> {
    if bytes.len() < 12 {
        // Missing or half-written header: nothing was ever committed.
        return Ok(Vec::new());
    }
    if bytes[..8] != WAL_MAGIC {
        return Err(PersistError::BadMagic {
            found: clone_first8(bytes),
        });
    }
    let version = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    if version != WAL_VERSION {
        return Err(PersistError::UnsupportedVersion {
            found: version,
            supported: WAL_VERSION,
        });
    }

    let mut records = Vec::new();
    let mut pos = 12usize;
    while pos < bytes.len() {
        if bytes.len() - pos < 8 {
            break; // torn tail: incomplete frame head
        }
        let len = u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
            as usize;
        let crc = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]);
        if len > MAX_RECORD_LEN {
            break; // corrupt length on the final frame: treat as torn
        }
        let body_start = pos + 8;
        let body_end = match body_start.checked_add(len) {
            Some(e) => e,
            None => break,
        };
        if body_end > bytes.len() {
            break; // torn tail: body truncated
        }
        let body = &bytes[body_start..body_end];
        if checksum::compute(body) != crc {
            break; // torn / corrupt tail
        }
        records.push(decode_record(body)?);
        pos = body_end;
    }
    Ok(records)
}

fn clone_first8(bytes: &[u8]) -> [u8; 8] {
    let mut out = [0u8; 8];
    let n = bytes.len().min(8);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

fn decode_record(body: &[u8]) -> Result<WalRecord> {
    let mut r = Cur::new(body);
    match r.u8()? {
        OP_INSERT => {
            let id = decode_vector_id(&mut r)?;
            let vec_len = r.u32()? as usize;
            let mut v = Vec::with_capacity(vec_len.min(4096));
            for _ in 0..vec_len {
                v.push(r.f32()?);
            }
            let meta = if r.u8()? == 1 {
                Some(decode_metadata(&mut r)?)
            } else {
                None
            };
            Ok(WalRecord::Insert {
                id,
                vector: Arc::from(v.into_boxed_slice()),
                meta,
            })
        }
        OP_DELETE => {
            let id = decode_vector_id(&mut r)?;
            Ok(WalRecord::Delete { id })
        }
        _ => Err(PersistError::InvalidPayload {
            reason: "unknown WAL op code",
        }),
    }
}

fn decode_vector_id(r: &mut Cur<'_>) -> Result<VectorId> {
    match r.u8()? {
        VID_U64 => Ok(VectorId::U64(r.u64()?)),
        VID_BYTES => {
            let len = usize::try_from(r.u64()?).map_err(|_| PersistError::InvalidPayload {
                reason: "WAL vector-id length does not fit in usize",
            })?;
            Ok(VectorId::Bytes(r.bytes(len)?.to_vec().into_boxed_slice()))
        }
        _ => Err(PersistError::InvalidPayload {
            reason: "unknown WAL vector-id kind",
        }),
    }
}

fn decode_metadata(r: &mut Cur<'_>) -> Result<Metadata> {
    let n = r.u32()? as usize;
    let mut entries: Vec<(String, Value)> = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        let key_len = r.u32()? as usize;
        let key = String::from_utf8(r.bytes(key_len)?.to_vec()).map_err(|_| {
            PersistError::InvalidPayload {
                reason: "WAL metadata key is not valid UTF-8",
            }
        })?;
        entries.push((key, decode_value(r)?));
    }
    Ok(entries.into_iter().collect())
}

fn decode_value(r: &mut Cur<'_>) -> Result<Value> {
    match r.u8()? {
        VAL_STRING => {
            let len = r.u32()? as usize;
            let s = String::from_utf8(r.bytes(len)?.to_vec()).map_err(|_| {
                PersistError::InvalidPayload {
                    reason: "WAL metadata string is not valid UTF-8",
                }
            })?;
            Ok(Value::String(s))
        }
        VAL_INT => Ok(Value::Int(r.i64()?)),
        VAL_FLOAT => Ok(Value::Float(r.f64()?)),
        VAL_BOOL => Ok(Value::Bool(r.u8()? != 0)),
        VAL_NULL => Ok(Value::Null),
        _ => Err(PersistError::InvalidPayload {
            reason: "unknown WAL metadata value tag",
        }),
    }
}

/// A bounds-checked cursor over a record body. Every read returns
/// [`PersistError::InvalidPayload`] rather than panicking on a short slice,
/// so a checksum-valid-but-structurally-short record is reported cleanly.
struct Cur<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cur<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(PersistError::InvalidPayload {
                reason: "WAL record offset overflow",
            })?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(PersistError::InvalidPayload {
                reason: "WAL record ended mid-field",
            })?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }

    fn f32(&mut self) -> Result<f32> {
        let b = self.take(4)?;
        Ok(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.u64()?))
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.take(n)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use iqdb_types::{Metadata, Value, VectorId};
    use proptest::prelude::*;

    use super::*;

    /// Build a WAL on a temp path, append `ops`, then read the raw bytes
    /// back so they can be fed to [`parse_records`].
    fn write_and_read(ops: &[WalRecord]) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let snap = dir.path().join("x.iqdb");
        let mut wal = Wal::create(&snap, FsyncPolicy::Never).unwrap();
        for op in ops {
            match op {
                WalRecord::Insert { id, vector, meta } => {
                    wal.append_insert(id, vector, meta.as_ref()).unwrap();
                }
                WalRecord::Delete { id } => wal.append_delete(id).unwrap(),
            }
        }
        std::fs::read(wal_path(&snap)).unwrap()
    }

    fn records_eq(a: &WalRecord, b: &WalRecord) -> bool {
        match (a, b) {
            (
                WalRecord::Insert {
                    id: ia,
                    vector: va,
                    meta: ma,
                },
                WalRecord::Insert {
                    id: ib,
                    vector: vb,
                    meta: mb,
                },
            ) => {
                ia == ib
                    && va.len() == vb.len()
                    && va
                        .iter()
                        .zip(vb.iter())
                        .all(|(x, y)| x.to_bits() == y.to_bits())
                    && meta_eq(ma.as_ref(), mb.as_ref())
            }
            (WalRecord::Delete { id: ia }, WalRecord::Delete { id: ib }) => ia == ib,
            _ => false,
        }
    }

    fn meta_eq(a: Option<&Metadata>, b: Option<&Metadata>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(x), Some(y)) => {
                if x.len() != y.len() {
                    return false;
                }
                x.iter()
                    .zip(y.iter())
                    .all(|((ka, va), (kb, vb))| ka == kb && value_eq(va, vb))
            }
            _ => false,
        }
    }

    fn value_eq(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
            _ => a == b,
        }
    }

    #[test]
    fn empty_wal_parses_to_no_records() {
        let bytes = write_and_read(&[]);
        let parsed = parse_records(&bytes).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut bytes = vec![0u8; 12];
        bytes[..8].copy_from_slice(b"NOTAWAL!");
        assert!(matches!(
            parse_records(&bytes),
            Err(PersistError::BadMagic { .. })
        ));
    }

    #[test]
    fn wrong_version_is_rejected() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&WAL_MAGIC);
        bytes.extend_from_slice(&999u32.to_le_bytes());
        assert!(matches!(
            parse_records(&bytes),
            Err(PersistError::UnsupportedVersion { found: 999, .. })
        ));
    }

    #[test]
    fn torn_final_frame_is_dropped() {
        let ops = vec![
            WalRecord::Insert {
                id: VectorId::U64(1),
                vector: Arc::from(vec![1.0f32, 2.0].into_boxed_slice()),
                meta: None,
            },
            WalRecord::Insert {
                id: VectorId::U64(2),
                vector: Arc::from(vec![3.0f32, 4.0].into_boxed_slice()),
                meta: None,
            },
        ];
        let bytes = write_and_read(&ops);
        let truncated = &bytes[..bytes.len() - 3];
        let parsed = parse_records(truncated).unwrap();
        assert_eq!(parsed.len(), 1);
        assert!(records_eq(&parsed[0], &ops[0]));
    }

    fn value_strategy() -> impl Strategy<Value = Value> {
        prop_oneof![
            ".*".prop_map(Value::String),
            any::<i64>().prop_map(Value::Int),
            any::<f64>().prop_map(Value::Float),
            any::<bool>().prop_map(Value::Bool),
            Just(Value::Null),
        ]
    }

    fn metadata_strategy() -> impl Strategy<Value = Metadata> {
        prop::collection::vec((".*", value_strategy()), 0..4)
            .prop_map(|entries| entries.into_iter().collect())
    }

    fn id_strategy() -> impl Strategy<Value = VectorId> {
        prop_oneof![
            any::<u64>().prop_map(VectorId::U64),
            prop::collection::vec(any::<u8>(), 1..16)
                .prop_map(|b| VectorId::Bytes(b.into_boxed_slice())),
        ]
    }

    fn op_strategy() -> impl Strategy<Value = WalRecord> {
        prop_oneof![
            (
                id_strategy(),
                prop::collection::vec(any::<f32>(), 0..8),
                prop::option::of(metadata_strategy()),
            )
                .prop_map(|(id, v, meta)| WalRecord::Insert {
                    id,
                    vector: Arc::from(v.into_boxed_slice()),
                    meta,
                }),
            id_strategy().prop_map(|id| WalRecord::Delete { id }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(96))]

        #[test]
        fn records_round_trip(ops in prop::collection::vec(op_strategy(), 0..12)) {
            let bytes = write_and_read(&ops);
            let parsed = parse_records(&bytes).unwrap();
            prop_assert_eq!(parsed.len(), ops.len());
            for (got, want) in parsed.iter().zip(ops.iter()) {
                prop_assert!(records_eq(got, want));
            }
        }
    }
}
