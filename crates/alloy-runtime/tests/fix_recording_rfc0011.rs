//! RFC-0011 IN1/IN14 (amendment A-0011-5): the host verify path records a
//! `FixEvent` when a verification passes after an edit.
//!
//! The seam under test is `FixRecordingVerifier` — a `Verifier` decorator
//! owned by the composition root, not by any capability worker (SEC4).
//!
//! Author: arkadianet

use std::path::Path;
use std::sync::{Arc, Mutex};

use alloy_runtime::adapters::{
    AppliedEdit, AppliedEditSource, FixRecordingVerifier, NodeExecContext, NodeExecRef, Verdict,
    VerdictOutcome, Verifier, MAX_PENDING_RUNS,
};
use alloy_runtime::graph::{FileChange, FixEvent, GraphError, GraphQuery, GraphView, ProjectGraph};
use alloy_runtime::types::ids::{GraphSnapshotId, GraphVersion};
use alloy_runtime::{
    AdapterError, ArtifactId, DagId, DiagnosticEvent, DiagnosticId, DiagnosticLevel, Digest,
    EventSeq, NodeId, RunId, SessionId, SpanRef, TransactionId,
};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

// --- doubles ------------------------------------------------------------

/// Records every `record_fix`; reads are empty; other writes are refused.
#[derive(Default)]
struct RecordingGraph {
    fixes: Mutex<Vec<FixEvent>>,
    fail_writes: bool,
}

impl RecordingGraph {
    fn recorded(&self) -> Vec<FixEvent> {
        self.fixes.lock().unwrap().clone()
    }
}

#[async_trait]
impl ProjectGraph for RecordingGraph {
    async fn rebuild(&self, _root: &Path) -> Result<GraphVersion, GraphError> {
        Err(GraphError::Disabled)
    }
    async fn apply_incremental(&self, _c: &[FileChange]) -> Result<GraphVersion, GraphError> {
        Err(GraphError::Disabled)
    }
    async fn query(&self, _q: GraphQuery) -> Result<GraphView, GraphError> {
        Ok(GraphView::empty(GraphVersion(1)))
    }
    async fn record_diagnostic(&self, _d: DiagnosticEvent) -> Result<(), GraphError> {
        Err(GraphError::Disabled)
    }
    async fn record_fix(&self, f: FixEvent) -> Result<(), GraphError> {
        if self.fail_writes {
            return Err(GraphError::Busy);
        }
        self.fixes.lock().unwrap().push(f);
        Ok(())
    }
    async fn snapshot(&self) -> Result<GraphSnapshotId, GraphError> {
        Err(GraphError::Disabled)
    }
}

/// FIFO verdict script.
struct ScriptedVerifier {
    verdicts: Mutex<Vec<Verdict>>,
}

impl ScriptedVerifier {
    fn new(verdicts: Vec<Verdict>) -> Self {
        Self {
            verdicts: Mutex::new(verdicts.into_iter().rev().collect()),
        }
    }
}

#[async_trait]
impl Verifier for ScriptedVerifier {
    async fn verify(&self, _ctx: &NodeExecContext) -> Result<Verdict, AdapterError> {
        Ok(self
            .verdicts
            .lock()
            .unwrap()
            .pop()
            .expect("verdict script exhausted"))
    }
}

/// Scripted view of the edit log: one answer per call.
struct ScriptedEdits {
    answers: Mutex<Vec<Option<AppliedEdit>>>,
}

impl ScriptedEdits {
    fn new(answers: Vec<Option<AppliedEdit>>) -> Self {
        Self {
            answers: Mutex::new(answers.into_iter().rev().collect()),
        }
    }
}

#[async_trait]
impl AppliedEditSource for ScriptedEdits {
    async fn latest_applied_edit(&self, _session: SessionId, _run: RunId) -> Option<AppliedEdit> {
        self.answers.lock().unwrap().pop().flatten()
    }
}

/// A single mutable "latest edit", the shape the real event log has: every
/// run sees whatever was applied last, and a test moves it forward by hand.
#[derive(Default)]
struct LatestEdit {
    current: Mutex<Option<AppliedEdit>>,
}

impl LatestEdit {
    fn set(&self, edit: AppliedEdit) {
        *self.current.lock().unwrap() = Some(edit);
    }
}

#[async_trait]
impl AppliedEditSource for LatestEdit {
    async fn latest_applied_edit(&self, _session: SessionId, _run: RunId) -> Option<AppliedEdit> {
        self.current.lock().unwrap().clone()
    }
}

// --- helpers ------------------------------------------------------------

fn ctx(run: RunId) -> NodeExecContext {
    NodeExecContext {
        meta: NodeExecRef {
            session_id: SessionId::new(),
            run_id: run,
            dag_id: DagId::new(),
            node_id: NodeId::new(),
            workspace_root: std::path::PathBuf::from("/tmp/ws"),
            attempt: 1,
        },
        cancellation: CancellationToken::new(),
    }
}

fn failing(code: &str) -> Verdict {
    Verdict {
        outcome: VerdictOutcome::Fail,
        diagnostics: vec![DiagnosticEvent {
            id: DiagnosticId::new(),
            code: Some(code.into()),
            level: DiagnosticLevel::Error,
            message: "mismatched types".into(),
            spans: vec![SpanRef {
                path: "src/main.rs".into(),
                start_line: 2,
                start_col: 1,
                end_line: 2,
                end_col: 9,
            }],
            children: vec![],
            package: Some("toy-core".into()),
            fingerprint: Digest::sha256(code.as_bytes()),
            raw_json: None,
        }],
        raw_artifact: None,
    }
}

fn inconclusive() -> Verdict {
    Verdict {
        outcome: VerdictOutcome::Inconclusive {
            reason: "cargo died before compiling".into(),
        },
        diagnostics: vec![],
        raw_artifact: None,
    }
}

/// A `Fail` that named nothing: the build is broken but no code was parsed.
fn failing_without_codes() -> Verdict {
    Verdict::fail()
}

fn an_edit(seq: u64) -> AppliedEdit {
    AppliedEdit {
        seq: EventSeq(seq),
        transaction: Some(TransactionId::new()),
        patch_artifact: Some(ArtifactId::new()),
    }
}

// --- tests --------------------------------------------------------------

#[tokio::test]
async fn pass_after_a_failure_and_an_edit_records_one_verified_fix_per_code() {
    // IN14/A-0011-5: the failing verify names the codes, the edit names the
    // patch, and the passing verify closes the loop.
    let graph = Arc::new(RecordingGraph::default());
    let edit = an_edit(9);
    let inner = Arc::new(ScriptedVerifier::new(vec![
        failing("E0308"),
        Verdict::pass(),
    ]));
    let edits = Arc::new(ScriptedEdits::new(vec![None, Some(edit.clone())]));
    let verifier = FixRecordingVerifier::new(
        inner as Arc<dyn Verifier>,
        Arc::clone(&graph) as Arc<dyn ProjectGraph>,
        edits as Arc<dyn AppliedEditSource>,
    );

    let c = ctx(RunId::new());
    assert!(!verifier.verify(&c).await.unwrap().passed());
    assert!(
        graph.recorded().is_empty(),
        "nothing to record on a failure"
    );
    assert!(verifier.verify(&c).await.unwrap().passed());

    let recorded = graph.recorded();
    assert_eq!(recorded.len(), 1);
    let f = &recorded[0];
    assert_eq!(f.diagnostic_code.as_deref(), Some("E0308"));
    assert!(f.verified);
    assert_eq!(f.patch_artifact, edit.patch_artifact);
    assert_eq!(f.transaction, edit.transaction);
    assert_eq!(f.crate_id.as_ref().map(|c| c.as_str()), Some("toy-core"));
    assert!(f.diagnostic.is_some(), "the fixed diagnostic is named");
}

#[tokio::test]
async fn a_pass_without_a_preceding_failure_or_edit_records_nothing() {
    // No failure ⇒ nothing was fixed; no *new* edit ⇒ nothing this run did.
    let graph = Arc::new(RecordingGraph::default());
    let inner = Arc::new(ScriptedVerifier::new(vec![
        Verdict::pass(),
        failing("E0502"),
        Verdict::pass(),
    ]));
    let stale = an_edit(4);
    let edits = Arc::new(ScriptedEdits::new(vec![
        Some(stale.clone()),
        Some(stale.clone()),
        Some(stale),
    ]));
    let verifier = FixRecordingVerifier::new(
        inner as Arc<dyn Verifier>,
        Arc::clone(&graph) as Arc<dyn ProjectGraph>,
        edits as Arc<dyn AppliedEditSource>,
    );
    let c = ctx(RunId::new());
    verifier.verify(&c).await.unwrap(); // pass, no prior failure
    verifier.verify(&c).await.unwrap(); // fail
    verifier.verify(&c).await.unwrap(); // pass, but the edit is unchanged
    assert!(graph.recorded().is_empty());
}

#[tokio::test]
async fn a_graph_write_failure_never_changes_the_verdict() {
    // Ingest is bookkeeping: a Busy graph must not fail a passing verify.
    let graph = Arc::new(RecordingGraph {
        fixes: Mutex::new(Vec::new()),
        fail_writes: true,
    });
    let inner = Arc::new(ScriptedVerifier::new(vec![
        failing("E0308"),
        Verdict::pass(),
    ]));
    let edits = Arc::new(ScriptedEdits::new(vec![None, Some(an_edit(3))]));
    let verifier = FixRecordingVerifier::new(
        inner as Arc<dyn Verifier>,
        Arc::clone(&graph) as Arc<dyn ProjectGraph>,
        edits as Arc<dyn AppliedEditSource>,
    );
    let c = ctx(RunId::new());
    verifier.verify(&c).await.unwrap();
    assert!(verifier.verify(&c).await.unwrap().passed());
    assert!(graph.recorded().is_empty());
}

#[tokio::test]
async fn each_run_keeps_its_own_pending_codes() {
    // Two runs through one decorator must not cross-contaminate.
    let graph = Arc::new(RecordingGraph::default());
    let inner = Arc::new(ScriptedVerifier::new(vec![
        failing("E0308"),
        Verdict::pass(),
    ]));
    let edits = Arc::new(ScriptedEdits::new(vec![None, Some(an_edit(2))]));
    let verifier = FixRecordingVerifier::new(
        inner as Arc<dyn Verifier>,
        Arc::clone(&graph) as Arc<dyn ProjectGraph>,
        edits as Arc<dyn AppliedEditSource>,
    );
    let first = ctx(RunId::new());
    let second = ctx(RunId::new());
    verifier.verify(&first).await.unwrap(); // fail in run one
    verifier.verify(&second).await.unwrap(); // pass in run two
    assert!(
        graph.recorded().is_empty(),
        "run two never failed; run one's codes are not its own"
    );
}

#[tokio::test]
async fn a_failure_that_names_no_code_drops_the_earlier_pending_codes() {
    // A `Fail` is a fresh statement of what is broken. If it names nothing,
    // the honest pending set is empty: a later pass must not be attributed
    // to codes that a *different*, earlier failure reported.
    let graph = Arc::new(RecordingGraph::default());
    let edits = Arc::new(LatestEdit::default());
    let inner = Arc::new(ScriptedVerifier::new(vec![
        failing("E0308"),
        failing_without_codes(),
        Verdict::pass(),
    ]));
    let verifier = FixRecordingVerifier::new(
        inner as Arc<dyn Verifier>,
        Arc::clone(&graph) as Arc<dyn ProjectGraph>,
        Arc::clone(&edits) as Arc<dyn AppliedEditSource>,
    );

    let c = ctx(RunId::new());
    edits.set(an_edit(1));
    verifier.verify(&c).await.unwrap(); // fail: E0308 pending
    edits.set(an_edit(2));
    verifier.verify(&c).await.unwrap(); // fail again, nothing nameable
    edits.set(an_edit(3));
    assert!(verifier.verify(&c).await.unwrap().passed());

    assert!(
        graph.recorded().is_empty(),
        "the uncoded failure replaced E0308; nothing may be claimed"
    );
}

#[tokio::test]
async fn a_later_failure_replaces_the_codes_of_the_earlier_one() {
    // Pending is "what the last failure said", not a union over the run.
    let graph = Arc::new(RecordingGraph::default());
    let edits = Arc::new(LatestEdit::default());
    let inner = Arc::new(ScriptedVerifier::new(vec![
        failing("E0308"),
        failing("E0502"),
        Verdict::pass(),
    ]));
    let verifier = FixRecordingVerifier::new(
        inner as Arc<dyn Verifier>,
        Arc::clone(&graph) as Arc<dyn ProjectGraph>,
        Arc::clone(&edits) as Arc<dyn AppliedEditSource>,
    );

    let c = ctx(RunId::new());
    edits.set(an_edit(1));
    verifier.verify(&c).await.unwrap();
    edits.set(an_edit(2));
    verifier.verify(&c).await.unwrap();
    edits.set(an_edit(3));
    verifier.verify(&c).await.unwrap();

    let codes: Vec<_> = graph
        .recorded()
        .into_iter()
        .filter_map(|f| f.diagnostic_code)
        .collect();
    assert_eq!(codes, vec!["E0502".to_string()]);
}

#[tokio::test]
async fn an_inconclusive_verdict_leaves_the_pending_codes_untouched() {
    // Inconclusive decides nothing about the patch (`cargo_exit_verdict`),
    // so it must neither record nor forget.
    let graph = Arc::new(RecordingGraph::default());
    let edits = Arc::new(LatestEdit::default());
    let inner = Arc::new(ScriptedVerifier::new(vec![
        failing("E0308"),
        inconclusive(),
        Verdict::pass(),
    ]));
    let verifier = FixRecordingVerifier::new(
        inner as Arc<dyn Verifier>,
        Arc::clone(&graph) as Arc<dyn ProjectGraph>,
        Arc::clone(&edits) as Arc<dyn AppliedEditSource>,
    );

    let c = ctx(RunId::new());
    edits.set(an_edit(1));
    verifier.verify(&c).await.unwrap();
    edits.set(an_edit(2));
    verifier.verify(&c).await.unwrap(); // inconclusive: no state change
    verifier.verify(&c).await.unwrap(); // pass, on the edit that came after

    let codes: Vec<_> = graph
        .recorded()
        .into_iter()
        .filter_map(|f| f.diagnostic_code)
        .collect();
    assert_eq!(codes, vec!["E0308".to_string()]);
}

#[tokio::test]
async fn a_pass_on_an_edit_no_newer_than_the_failure_records_nothing() {
    // The recording condition is strictly "a new edit since the failure".
    let graph = Arc::new(RecordingGraph::default());
    let edits = Arc::new(LatestEdit::default());
    let inner = Arc::new(ScriptedVerifier::new(vec![
        failing("E0308"),
        Verdict::pass(),
    ]));
    let verifier = FixRecordingVerifier::new(
        inner as Arc<dyn Verifier>,
        Arc::clone(&graph) as Arc<dyn ProjectGraph>,
        Arc::clone(&edits) as Arc<dyn AppliedEditSource>,
    );
    let c = ctx(RunId::new());
    edits.set(an_edit(7));
    verifier.verify(&c).await.unwrap();
    verifier.verify(&c).await.unwrap();
    assert!(graph.recorded().is_empty());
}

#[tokio::test]
async fn abandoned_runs_are_evicted_oldest_first_and_never_the_newest() {
    // Failed/cancelled runs never come back to pass, so their entries would
    // leak. The map is capped: the oldest failure is forgotten, and the
    // freshest one still records when it passes.
    let graph = Arc::new(RecordingGraph::default());
    let edits = Arc::new(LatestEdit::default());
    let n = MAX_PENDING_RUNS + 1;
    let mut verdicts: Vec<Verdict> = (0..n).map(|_| failing("E0308")).collect();
    verdicts.push(Verdict::pass()); // the oldest run comes back
    verdicts.push(Verdict::pass()); // so does the newest
    let inner = Arc::new(ScriptedVerifier::new(verdicts));
    let verifier = FixRecordingVerifier::new(
        inner as Arc<dyn Verifier>,
        Arc::clone(&graph) as Arc<dyn ProjectGraph>,
        Arc::clone(&edits) as Arc<dyn AppliedEditSource>,
    );

    let runs: Vec<_> = (0..n).map(|_| ctx(RunId::new())).collect();
    for (i, c) in runs.iter().enumerate() {
        edits.set(an_edit(i as u64 + 1));
        verifier.verify(c).await.unwrap();
    }
    edits.set(an_edit(1000));

    verifier.verify(&runs[0]).await.unwrap(); // evicted: nothing to claim
    assert!(
        graph.recorded().is_empty(),
        "the oldest run's pending codes were evicted, so nothing is claimed"
    );
    verifier.verify(runs.last().unwrap()).await.unwrap();
    assert_eq!(
        graph.recorded().len(),
        1,
        "the newest run must never be the one evicted"
    );
}
