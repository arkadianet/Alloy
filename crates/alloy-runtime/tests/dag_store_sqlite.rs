//! Cross-subsystem SQLite integration for RFC-0009 DAG store + planner.

use alloy_runtime::storage::{
    AlloyStorage, ArtifactStore, DagStore, EventStore, StorageOpenOptions, StoreError,
};
use alloy_runtime::{
    compiler_fingerprint_digest, policy_hash_digest, tool_versions_digest, BudgetPolicy, DagId,
    Goal, PlanContext, PlanProducedPayload, PlanService, ProfileId, RunId, SessionEventType,
    SessionId, TemplatePlanService, ToolchainRecord,
};

fn fixture_toolchain() -> ToolchainRecord {
    ToolchainRecord {
        channel: "1.97.1".into(),
        rustc_version: "rustc 1.97.1 (test)".into(),
        cargo_version: "cargo 1.97.1 (test)".into(),
    }
}

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
        policy_hash: policy_hash_digest(
            &ProfileId::new("default").unwrap(),
            &BudgetPolicy::default(),
        ),
        tool_versions: tool_versions_digest(&fixture_toolchain()),
        compiler_fingerprint: compiler_fingerprint_digest(
            &fixture_toolchain(),
            "x86_64-unknown-linux-gnu",
        ),
        prior_source: None,
        prior_proposal_artifact: None,
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
        let svc = TemplatePlanService::from_storage(&storage);
        let result = svc.plan(plan_ctx(session, run, dag_id)).await.unwrap();
        snapshot_id = result.snapshot_artifact;
        generation = result.dag.generation;
        assert_eq!(result.dag.id, dag_id);

        let got = storage.dags().get(dag_id).await.unwrap().unwrap();
        assert_eq!(got.generation, 1);

        let evs = storage
            .events()
            .list_session_events(session, None, 100)
            .await
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].type_, SessionEventType::PlanProduced);
        assert_eq!(evs[0].run_id, Some(run));
        let payload: PlanProducedPayload = serde_json::from_value(evs[0].payload.clone()).unwrap();
        assert_eq!(payload.snapshot_artifact, snapshot_id);
        assert_eq!(payload.generation, generation);

        storage.close().await.unwrap();
    }

    // Reopen and reload DAG + PlanProduced from durable stores
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

    let evs = storage
        .events()
        .list_session_events(session, None, 100)
        .await
        .unwrap();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].type_, SessionEventType::PlanProduced);
    let payload: PlanProducedPayload = serde_json::from_value(evs[0].payload.clone()).unwrap();
    assert_eq!(payload.snapshot_artifact, snapshot_id);
    assert_eq!(payload.generation, 1);
    assert_eq!(payload.dag_id, dag_id);

    storage.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_put_if_generation_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(dir.path()))
        .await
        .unwrap();
    let svc = TemplatePlanService::from_storage(&storage);
    let dag_id = DagId::new();
    let result = svc
        .plan(plan_ctx(SessionId::new(), RunId::new(), dag_id))
        .await
        .unwrap();

    // Both writers claim expected=1 while advancing generation to 2.
    // First UPDATE … WHERE generation=1 wins; second sees Conflict or Busy.
    let mut a = result.dag.clone();
    let mut b = result.dag.clone();
    a.generation = 2;
    a.state = alloy_runtime::DagState::Failed;
    b.generation = 2;
    b.state = alloy_runtime::DagState::Cancelled;

    let dags = storage.dags();
    let (r1, r2) = tokio::join!(
        dags.put_if_generation(&a, Some(1)),
        dags.put_if_generation(&b, Some(1)),
    );

    let outcomes = [r1, r2];
    let oks = outcomes.iter().filter(|r| r.is_ok()).count();
    let losers = outcomes
        .iter()
        .filter(|r| matches!(r, Err(StoreError::Conflict(_)) | Err(StoreError::Busy)))
        .count();
    assert_eq!(oks, 1, "exactly one CAS writer must win");
    assert_eq!(losers, 1, "the other writer must Conflict or Busy");

    let final_dag = storage.dags().get(dag_id).await.unwrap().unwrap();
    assert_eq!(final_dag.generation, 2);
    assert!(matches!(
        final_dag.state,
        alloy_runtime::DagState::Failed | alloy_runtime::DagState::Cancelled
    ));
    storage.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_cas_across_storage_handles() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let storage_a = AlloyStorage::open(StorageOpenOptions::for_data_dir(&path))
        .await
        .unwrap();
    let svc = TemplatePlanService::from_storage(&storage_a);
    let dag_id = DagId::new();
    let result = svc
        .plan(plan_ctx(SessionId::new(), RunId::new(), dag_id))
        .await
        .unwrap();

    let storage_b = AlloyStorage::open(StorageOpenOptions::for_data_dir(&path))
        .await
        .unwrap();

    let mut a = result.dag.clone();
    let mut b = result.dag.clone();
    a.generation = 2;
    a.state = alloy_runtime::DagState::Failed;
    b.generation = 2;
    b.state = alloy_runtime::DagState::Cancelled;

    let dags_a = storage_a.dags();
    let dags_b = storage_b.dags();
    let (r1, r2) = tokio::join!(
        dags_a.put_if_generation(&a, Some(1)),
        dags_b.put_if_generation(&b, Some(1)),
    );

    let oks = [r1.is_ok(), r2.is_ok()].into_iter().filter(|x| *x).count();
    let losers = [&r1, &r2]
        .into_iter()
        .filter(|r| matches!(r, Err(StoreError::Conflict(_)) | Err(StoreError::Busy)))
        .count();
    assert_eq!(oks, 1);
    assert_eq!(losers, 1);

    storage_a.close().await.unwrap();
    storage_b.close().await.unwrap();
}

#[tokio::test]
async fn replan_persists_prior_gen_via_snapshot_and_durable_event() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let session = SessionId::new();
    let run = RunId::new();
    let dag_id = DagId::new();
    let gen1_snapshot;

    {
        let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(&data))
            .await
            .unwrap();
        let svc = TemplatePlanService::from_storage(&storage);
        let ctx = plan_ctx(session, run, dag_id);
        let first = svc.plan(ctx.clone()).await.unwrap();
        gen1_snapshot = first.snapshot_artifact;

        let mut failed = first.dag.clone();
        failed.state = alloy_runtime::DagState::Failed;
        storage.dags().put(&failed).await.unwrap();

        let mut replan_ctx = ctx;
        replan_ctx.template_override = Some(first.template_id);
        let second = svc
            .replan(alloy_runtime::ReplanReason::UserRequested, replan_ctx)
            .await
            .unwrap();
        assert_eq!(second.dag.generation, 2);

        storage.close().await.unwrap();
    }

    let storage = AlloyStorage::open(StorageOpenOptions::for_data_dir(&data))
        .await
        .unwrap();
    let got = storage.dags().get(dag_id).await.unwrap().unwrap();
    assert_eq!(got.generation, 2);

    let old_snap = storage.artifacts().get(gen1_snapshot).await.unwrap();
    let prior: alloy_runtime::TaskDag = serde_json::from_slice(&old_snap.bytes).unwrap();
    assert_eq!(prior.generation, 1);

    let evs = storage
        .events()
        .list_session_events(session, None, 100)
        .await
        .unwrap();
    assert_eq!(evs.len(), 2);
    assert!(evs
        .iter()
        .all(|e| e.type_ == SessionEventType::PlanProduced));
    let replan_payload: PlanProducedPayload =
        serde_json::from_value(evs[1].payload.clone()).unwrap();
    assert!(replan_payload.replan);
    assert_eq!(replan_payload.generation, 2);
    assert_eq!(replan_payload.dag_id, dag_id);

    storage.close().await.unwrap();
}
