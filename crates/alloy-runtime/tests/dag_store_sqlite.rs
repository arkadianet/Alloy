//! Cross-subsystem SQLite integration for RFC-0009 DAG store + planner.

use std::sync::Arc;

use alloy_runtime::storage::{
    AlloyStorage, ArtifactStore, DagStore, StorageOpenOptions, StoreError,
};
use alloy_runtime::{
    mvp_compiler_fingerprint_digest, mvp_policy_hash_digest, mvp_tool_versions_digest, DagId,
    EventSink, Goal, InMemoryEventSink, PlanContext, PlanProducedPayload, PlanService, RunId,
    SessionEventType, SessionId, TemplatePlanService,
};

fn plan_ctx(session: SessionId, run: RunId, dag: DagId) -> PlanContext {
    PlanContext {
        session_id: session,
        run_id: run,
        dag_id: dag,
        goal: Goal {
            text: "integration repair".into(),
            constraints: vec![],
            attachments: vec![],
        },
        template_override: None,
        policy_hash: mvp_policy_hash_digest(),
        tool_versions: mvp_tool_versions_digest(),
        compiler_fingerprint: mvp_compiler_fingerprint_digest(),
    }
}

#[tokio::test]
async fn plan_persist_reopen_snapshot_and_event() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let session = SessionId::new();
    let run = RunId::new();
    let dag_id = DagId::new();
    let snapshot_id;
    let generation;

    {
        let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(&data))
            .await
            .unwrap();
        let events = Arc::new(InMemoryEventSink::new());
        let svc = TemplatePlanService::new(
            storage.dags() as Arc<dyn DagStore>,
            storage.artifacts() as Arc<dyn ArtifactStore>,
            events.clone() as Arc<dyn EventSink>,
        );
        let result = svc.plan(plan_ctx(session, run, dag_id)).await.unwrap();
        snapshot_id = result.snapshot_artifact;
        generation = result.dag.generation;
        assert_eq!(result.dag.id, dag_id);

        let got = storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(got.generation, 1);

        let evs = events.session_events(session);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].type_, SessionEventType::PlanProduced);
        assert_eq!(evs[0].run_id, Some(run));
        let payload: PlanProducedPayload = serde_json::from_value(evs[0].payload.clone()).unwrap();
        assert_eq!(payload.snapshot_artifact, snapshot_id);
        assert_eq!(payload.generation, generation);

        storage.close().await.unwrap();
    }

    // Reopen and reload
    let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(&data))
        .await
        .unwrap();
    let got = storage.dags().get(dag_id).await.unwrap().unwrap();
    assert_eq!(got.id, dag_id);
    assert_eq!(got.generation, 1);

    let snap = storage.artifacts().get(snapshot_id).await.unwrap();
    let from_snap: alloy_runtime::TaskDag = serde_json::from_slice(&snap.bytes).unwrap();
    assert_eq!(from_snap, got);

    for node in got.nodes.values() {
        let _ = storage.artifacts().get(node.input_ref).await.unwrap();
    }

    storage.close().await.unwrap();
}

#[tokio::test]
async fn concurrent_put_if_generation_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
        .await
        .unwrap();
    let events = Arc::new(InMemoryEventSink::new());
    let svc = TemplatePlanService::new(
        storage.dags() as Arc<dyn DagStore>,
        storage.artifacts() as Arc<dyn ArtifactStore>,
        events as Arc<dyn EventSink>,
    );
    let dag_id = DagId::new();
    let result = svc
        .plan(plan_ctx(SessionId::new(), RunId::new(), dag_id))
        .await
        .unwrap();

    // Stale expected generation must Conflict even under concurrent load.
    let mut a = result.dag.clone();
    a.state = alloy_runtime::DagState::Failed;
    let dags = storage.dags();
    let (r1, r2) = tokio::join!(
        dags.put_if_generation(&a, Some(1)),
        dags.put_if_generation(&a, Some(0)),
    );
    assert!(r1.is_ok());
    assert!(matches!(r2, Err(StoreError::Conflict(_))));
}
