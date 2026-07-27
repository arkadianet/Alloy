//! Micro-benchmark for DagValidator on the day-1 repair template.

use std::collections::BTreeMap;
use std::hint::black_box;

use alloy_runtime::{
    allocate_ids, build_topology, ArtifactId, BuildTopology, DagId, DagValidator, SessionId,
    TemplateCatalog, TemplateId, ValidateOpts,
};
use criterion::{criterion_group, criterion_main, Criterion};

fn bench_validate_repair_template(c: &mut Criterion) {
    let m = TemplateCatalog::get(TemplateId::RepairLocalDiagnostic);
    let ids = allocate_ids(m);
    let mut input_refs = BTreeMap::new();
    for nid in ids.nodes.values() {
        input_refs.insert(*nid, ArtifactId::new());
    }
    let dag = build_topology(BuildTopology {
        manifest: m,
        dag_id: DagId::new(),
        session_id: SessionId::new(),
        generation: 1,
        ids: &ids,
        input_refs: &input_refs,
    });

    c.bench_function("dag_validate_repair_local_diagnostic", |b| {
        b.iter(|| {
            DagValidator::validate(black_box(&dag), ValidateOpts::default()).unwrap();
        });
    });
}

criterion_group!(benches, bench_validate_repair_template);
criterion_main!(benches);
