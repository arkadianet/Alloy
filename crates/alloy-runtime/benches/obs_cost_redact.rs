//! Benchmark CostMeter updates and secret redaction (RFC-0004).

use alloy_runtime::obs::{redact_secrets, CostMeter};
use alloy_runtime::ModelTier;
use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn bench_cost_meter(c: &mut Criterion) {
    c.bench_function("cost_meter_add_model_usage", |b| {
        b.iter(|| {
            let mut m = CostMeter::new();
            for _ in 0..64 {
                m.add_model_usage(
                    ModelTier::Standard,
                    Some(black_box(100)),
                    Some(black_box(50)),
                    Some(black_box(0.01)),
                );
            }
            black_box(m.snapshot())
        })
    });
}

fn bench_redact(c: &mut Criterion) {
    let sample = "hello api_key=sk-abcdefghijklmnop Authorization: Bearer tokensecret99 world";
    c.bench_function("redact_secrets_small", |b| {
        b.iter(|| redact_secrets(black_box(sample)))
    });
}

criterion_group!(benches, bench_cost_meter, bench_redact);
criterion_main!(benches);
