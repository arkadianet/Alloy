//! Micro-benchmark for `diagnostic_fingerprint` (RFC-0010 §11.1) — the
//! dedupe hot path every parsed `rustc` diagnostic runs through (DG6).

use std::hint::black_box;

use alloy_runtime::{diagnostic_fingerprint, DiagnosticLevel, SpanRef};
use criterion::{criterion_group, criterion_main, Criterion};

fn bench_diagnostic_fingerprint(c: &mut Criterion) {
    let span = SpanRef {
        path: "src/scheduler/linear/loop_.rs".into(),
        start_line: 742,
        start_col: 9,
        end_line: 744,
        end_col: 11,
    };
    c.bench_function("diagnostic_fingerprint_with_span", |b| {
        b.iter(|| {
            diagnostic_fingerprint(
                black_box(Some("E0502")),
                black_box(DiagnosticLevel::Error),
                black_box(
                    "cannot borrow `dag` as mutable because it is also borrowed as immutable",
                ),
                black_box(Some(&span)),
            )
        });
    });

    c.bench_function("diagnostic_fingerprint_no_span", |b| {
        b.iter(|| {
            diagnostic_fingerprint(
                black_box(None),
                black_box(DiagnosticLevel::Warning),
                black_box("unused variable: `x`"),
                black_box(None),
            )
        });
    });
}

criterion_group!(benches, bench_diagnostic_fingerprint);
criterion_main!(benches);
