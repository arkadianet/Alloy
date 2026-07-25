//! Benchmark Digest::sha256 (RFC-0001 shared IR).

use alloy_runtime::Digest;
use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn bench_digest(c: &mut Criterion) {
    let payload = vec![0u8; 4096];
    c.bench_function("digest_sha256_4k", |b| {
        b.iter(|| Digest::sha256(black_box(&payload)))
    });
}

criterion_group!(benches, bench_digest);
criterion_main!(benches);
