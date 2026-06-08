//! Snapshot-compression codec benchmarks.
//!
//! Measures compress + decompress throughput for Zstd and LZ4 on a
//! representative vector-index payload (interleaved `f32` coordinates plus
//! `u64` ids — the kind of bytes a `Persistable::save_to` emits). Requires
//! both the `zstd` and `lz4` features:
//!
//! ```sh
//! cargo bench --bench compression_bench --features zstd,lz4
//! ```

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use iqdb_persist::Compression;

/// Build a payload that resembles a flat index snapshot: `n` rows of a
/// `u64` id followed by a `dim`-length `f32` vector, little-endian. Values
/// are deterministic (no RNG) but varied enough to be realistically
/// semi-compressible.
fn sample_payload(n: usize, dim: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(n * (8 + dim * 4));
    for i in 0..n {
        buf.extend_from_slice(&(i as u64).to_le_bytes());
        for d in 0..dim {
            let v = ((i * 31 + d * 7) % 97) as f32 / 97.0;
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
    buf
}

fn compress(scheme: Compression, raw: &[u8]) -> Vec<u8> {
    // Round-trip through a snapshot file via the public API would also pull
    // in an index; here we exercise the codec the same way the crate does
    // internally, through a tiny re-encode using the public Compression
    // contract surfaced by save/load. For a focused codec micro-benchmark
    // we call the underlying crates directly.
    match scheme {
        Compression::Zstd { level } => zstd::encode_all(raw, level).expect("zstd encode"),
        Compression::Lz4 => lz4_flex::block::compress(raw),
        Compression::None => raw.to_vec(),
    }
}

fn bench_codecs(c: &mut Criterion) {
    let payload = sample_payload(10_000, 128);
    let mut group = c.benchmark_group("compress");
    group.throughput(Throughput::Bytes(payload.len() as u64));

    for scheme in [Compression::Zstd { level: 3 }, Compression::Lz4] {
        let name = match scheme {
            Compression::Zstd { .. } => "zstd_l3",
            Compression::Lz4 => "lz4",
            Compression::None => "none",
        };
        group.bench_with_input(BenchmarkId::from_parameter(name), &payload, |b, p| {
            b.iter(|| compress(scheme, p));
        });
    }
    group.finish();

    // Decompression throughput (throughput counted in uncompressed bytes).
    let mut dgroup = c.benchmark_group("decompress");
    dgroup.throughput(Throughput::Bytes(payload.len() as u64));

    let zstd_blob = zstd::encode_all(&payload[..], 3).expect("zstd encode");
    dgroup.bench_function("zstd_l3", |b| {
        b.iter(|| zstd::decode_all(&zstd_blob[..]).expect("zstd decode"));
    });

    let lz4_blob = lz4_flex::block::compress(&payload);
    let ulen = payload.len();
    dgroup.bench_function("lz4", |b| {
        b.iter(|| lz4_flex::block::decompress(&lz4_blob, ulen).expect("lz4 decode"));
    });
    dgroup.finish();
}

criterion_group!(benches, bench_codecs);
criterion_main!(benches);
