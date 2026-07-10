//! Content-hashing microbenchmark.
//!
//! `basemind::hashing::hash_bytes` (blake3) runs once per file on the scanner hot
//! path, so its throughput gates the I/O-bound portion of a scan. Bench across a
//! few buffer sizes representative of real source files (1 KiB → 256 KiB) plus the
//! zero-alloc hex round-trip used to key the content-addressed blob store.

use basemind::hashing::{from_hex, hash_bytes, hex_buf, hex_str};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

/// Buffer sizes in bytes: small file, typical module, large generated file.
const SIZES: &[usize] = &[1 << 10, 1 << 12, 1 << 14, 1 << 16, 1 << 18];

fn bench_hash(c: &mut Criterion) {
    let mut group = c.benchmark_group("hashing/hash_bytes");
    for &size in SIZES {
        let buf: Vec<u8> = (0..size).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &buf, |b, buf| {
            b.iter(|| hash_bytes(black_box(buf)));
        });
    }
    group.finish();

    let hash = hash_bytes(b"basemind blob key sample");
    c.bench_function("hashing/hex_roundtrip", |b| {
        b.iter(|| {
            let buf = hex_buf(black_box(&hash));
            from_hex(black_box(hex_str(&buf))).unwrap()
        });
    });
}

criterion_group!(benches, bench_hash);
criterion_main!(benches);
