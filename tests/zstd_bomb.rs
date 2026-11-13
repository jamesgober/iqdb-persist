//! A Zstd frame that expands far past its recorded length must be stopped
//! while it decodes, not after. A counting allocator tracks the peak heap
//! use of the load.

#![cfg(feature = "zstd")]
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

use iqdb_index::{Index, IndexCore, IndexStats};
use iqdb_persist::{PersistConfig, PersistError, Persistable, PersistedIndex, Result};
use iqdb_types::{DistanceMetric, Hit, Metadata, Result as IqdbResult, SearchParams, VectorId};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call forwards to `System` unchanged; the counters are
// bookkeeping only.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: same contract as the caller's.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            let _ = PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) };
        let _ = LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: same contract as the caller's.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                let grow = new_size - layout.size();
                let now = LIVE.fetch_add(grow, Ordering::Relaxed) + grow;
                let _ = PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                let _ = LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[derive(Debug)]
struct Empty;

impl IndexCore for Empty {
    fn insert(
        &mut self,
        _: VectorId,
        _: std::sync::Arc<[f32]>,
        _: Option<Metadata>,
    ) -> IqdbResult<()> {
        Ok(())
    }
    fn delete(&mut self, _: &VectorId) -> IqdbResult<()> {
        Ok(())
    }
    fn search(&self, _: &[f32], _: &SearchParams) -> IqdbResult<Vec<Hit>> {
        Ok(Vec::new())
    }
    fn len(&self) -> usize {
        0
    }
    fn dim(&self) -> usize {
        2
    }
    fn metric(&self) -> DistanceMetric {
        DistanceMetric::Euclidean
    }
    fn flush(&mut self) -> IqdbResult<()> {
        Ok(())
    }
    fn stats(&self) -> IndexStats {
        IndexStats::default()
    }
}

impl Index for Empty {
    type Config = ();
    fn new(_: usize, _: DistanceMetric, _: ()) -> IqdbResult<Self> {
        Ok(Self)
    }
}

impl Persistable for Empty {
    const INDEX_TYPE: &'static str = "empty";
    fn save_to(&self, _: &mut dyn Write) -> Result<()> {
        Ok(())
    }
    fn load_from(_: &mut dyn Read) -> Result<Self> {
        Ok(Self)
    }
}

#[test]
fn oversized_zstd_frame_is_stopped_while_decoding() {
    const EXPANDED: usize = 128 << 20;

    // 128 MiB of zeros, streamed through the encoder in 1 MiB chunks.
    let mut enc = zstd::Encoder::new(Vec::new(), 3).unwrap();
    let chunk = vec![0u8; 1 << 20];
    for _ in 0..(EXPANDED >> 20) {
        enc.write_all(&chunk).unwrap();
    }
    let frame = enc.finish().unwrap();
    drop(chunk);

    // A format-v2 file whose payload claims 16 bytes, inside the ratio guard.
    let mut region = vec![1u8];
    region.extend_from_slice(&16u64.to_le_bytes());
    region.extend_from_slice(&frame);
    let crc = iqdb_persist::checksum::compute(&region);
    let mut file = Vec::new();
    file.extend_from_slice(&iqdb_persist::MAGIC);
    file.extend_from_slice(&2u32.to_le_bytes());
    file.extend_from_slice(&(Empty::INDEX_TYPE.len() as u64).to_le_bytes());
    file.extend_from_slice(Empty::INDEX_TYPE.as_bytes());
    file.extend_from_slice(&2u64.to_le_bytes());
    file.push(2);
    file.extend_from_slice(&0u64.to_le_bytes());
    file.extend_from_slice(&crc.to_le_bytes());
    file.extend_from_slice(&region);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bomb.iqdb");
    std::fs::write(&path, &file).unwrap();
    drop(file);

    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let err = PersistedIndex::<Empty>::load(PersistConfig::new(&path)).unwrap_err();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);

    assert!(matches!(err, PersistError::Compression { .. }), "{err:?}");
    assert!(
        peak < 16 << 20,
        "load reserved {peak} bytes for a frame recorded as 16 bytes"
    );
}
