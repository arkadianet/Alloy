//! Benchmark session event append throughput (RFC-0002).

use std::sync::Arc;

use alloy_runtime::events::{EventSink, NewSessionEvent, SessionEventType};
use alloy_runtime::storage::{AlloyStorage, StorageOpenOptions};
use alloy_runtime::types::ids::SessionId;
use criterion::{criterion_group, criterion_main, Criterion};
use serde_json::json;

fn append_bench(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let storage = rt.block_on(async {
        AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
            .await
            .unwrap()
    });
    let events = storage.events();
    let session = SessionId::new();

    c.bench_function("sqlite_append_session", |b| {
        b.to_async(&rt).iter(|| {
            let events = Arc::clone(&events);
            async move {
                events
                    .append_session(NewSessionEvent {
                        session_id: session,
                        run_id: None,
                        type_: SessionEventType::Decision,
                        payload: json!({}),
                    })
                    .await
                    .unwrap();
            }
        });
    });

    rt.block_on(async {
        storage.close().await.unwrap();
    });
}

criterion_group!(benches, append_bench);
criterion_main!(benches);
