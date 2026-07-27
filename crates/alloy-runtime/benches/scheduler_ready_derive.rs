//! Micro-benchmarks for the scheduler's hot pure functions (RFC-0010 §11.1):
//! `ready_nodes` (every L8/L9 loop iteration) and `derive_dag_state` (every
//! R17/§5.17 terminal derivation). Same day-1 repair-template fixture as
//! `dag_validate.rs`, for the same reason: representative node/edge count,
//! not an invented shape.

use std::collections::BTreeMap;
use std::hint::black_box;

use alloy_runtime::{
    allocate_ids, build_topology, derive_dag_state, ready_nodes, ArtifactId, BuildTopology, DagId,
    DeriveFlags, SessionId, TemplateCatalog, TemplateId,
};
use criterion::{criterion_group, criterion_main, Criterion};

fn repair_template_dag() -> alloy_runtime::TaskDag {
    let m = TemplateCatalog::get(TemplateId::RepairLocalDiagnostic);
    let ids = allocate_ids(m);
    let mut input_refs = BTreeMap::new();
    for nid in ids.nodes.values() {
        input_refs.insert(*nid, ArtifactId::new());
    }
    build_topology(BuildTopology {
        manifest: m,
        dag_id: DagId::new(),
        session_id: SessionId::new(),
        generation: 1,
        ids: &ids,
        input_refs: &input_refs,
    })
}

fn bench_ready_nodes(c: &mut Criterion) {
    let dag = repair_template_dag();
    c.bench_function("ready_nodes_repair_local_diagnostic", |b| {
        b.iter(|| ready_nodes(black_box(&dag)));
    });
}

fn bench_derive_dag_state(c: &mut Criterion) {
    let dag = repair_template_dag();
    c.bench_function("derive_dag_state_repair_local_diagnostic", |b| {
        b.iter(|| derive_dag_state(black_box(&dag), DeriveFlags::default()).unwrap());
    });
}

criterion_group!(benches, bench_ready_nodes, bench_derive_dag_state);
criterion_main!(benches);
