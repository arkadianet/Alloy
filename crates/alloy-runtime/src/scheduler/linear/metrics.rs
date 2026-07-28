//! [`SchedulerMetrics`] — debug/test counters, not a durability mechanism
//! (RFC-0010 §9.3).

use std::sync::atomic::{AtomicU64, Ordering};

/// Snapshot of the scheduler's internal atomic counters. Debugging aid, not
/// a durability mechanism — durable state lives in the DAG blob and session
/// events.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerMetrics {
    /// `run` invocations started.
    pub runs_started: u64,
    /// `run` invocations that reached `Succeeded`.
    pub runs_succeeded: u64,
    /// `run` invocations that reached `Failed`.
    pub runs_failed: u64,
    /// `run` invocations that reached `Cancelled`.
    pub runs_cancelled: u64,
    /// `run` invocations that reached `ReplanRequired`.
    pub runs_replan_required: u64,
    /// Nodes dispatched (capability, verify, or gate allow-fold).
    pub nodes_dispatched: u64,
    /// Nodes that reached `Succeeded`.
    pub nodes_succeeded: u64,
    /// Nodes durably `Failed`.
    pub nodes_failed: u64,
    /// Nodes marked `Skipped`.
    pub nodes_skipped: u64,
    /// Retries admitted (C8).
    pub retries_admitted: u64,
    /// Retries rejected by admission (A1-A6).
    pub retries_rejected: u64,
    /// Tier escalations applied.
    pub escalations: u64,
    /// Gates opened (first schedule, C9a).
    pub gates_opened: u64,
    /// Gates resolved `allow` / `allow_once`.
    pub gates_allowed: u64,
    /// Gates resolved `deny`.
    pub gates_denied: u64,
    /// Gates resolved `expired`.
    pub gates_expired: u64,
    /// Generation CAS conflicts observed.
    pub cas_conflicts: u64,
    /// RF3/RF6 event repairs appended.
    pub event_repairs: u64,
    /// `cancel` calls observed.
    pub cancels: u64,
    /// Forced C6 writes after `cancel_drain_grace` elapsed.
    pub forced_cancel_writes: u64,
    /// Runs stopped by budget exhaustion.
    pub budget_stops: u64,
    /// Node-level timeouts.
    pub node_timeouts: u64,
    /// Run-level timeouts.
    pub run_timeouts: u64,
}

/// Process-local atomic counters backing [`SchedulerMetrics`].
#[derive(Debug, Default)]
pub(super) struct SchedulerCounters {
    runs_started: AtomicU64,
    runs_succeeded: AtomicU64,
    runs_failed: AtomicU64,
    runs_cancelled: AtomicU64,
    runs_replan_required: AtomicU64,
    nodes_dispatched: AtomicU64,
    nodes_succeeded: AtomicU64,
    nodes_failed: AtomicU64,
    nodes_skipped: AtomicU64,
    retries_admitted: AtomicU64,
    retries_rejected: AtomicU64,
    escalations: AtomicU64,
    gates_opened: AtomicU64,
    gates_allowed: AtomicU64,
    gates_denied: AtomicU64,
    gates_expired: AtomicU64,
    cas_conflicts: AtomicU64,
    event_repairs: AtomicU64,
    cancels: AtomicU64,
    forced_cancel_writes: AtomicU64,
    budget_stops: AtomicU64,
    node_timeouts: AtomicU64,
    run_timeouts: AtomicU64,
}

impl SchedulerCounters {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// A `put_if_generation` CAS lost the generation race (§5.8.4 step 4).
    pub(super) fn inc_cas_conflicts(&self) {
        self.cas_conflicts.fetch_add(1, Ordering::Relaxed);
    }

    /// An RF3/RF6/RF7 crash-repair event was appended (§5.3.3).
    ///
    /// All three repair paths are reachable in production:
    /// `repair_approval_requested` from `gate.rs`'s GR3, and
    /// `repair_gate_terminal` (which calls `repair_node_state`) from R9's
    /// FN2 selection in `loop_.rs`.
    pub(super) fn inc_event_repairs(&self) {
        self.event_repairs.fetch_add(1, Ordering::Relaxed);
    }

    /// §5.12.2 step 1: a `cancel(dag_id)` call was observed (owned or not).
    pub(super) fn inc_cancels(&self) {
        self.cancels.fetch_add(1, Ordering::Relaxed);
    }

    /// A `cancel`-side C6 (or an owned run's C6, observed by `cancel` after
    /// `cancel_drain_grace` had already elapsed) landed past the soft
    /// `cancel_drain_grace` target, inside the `cancel_write_grace` buffer.
    pub(super) fn inc_forced_cancel_writes(&self) {
        self.forced_cancel_writes.fetch_add(1, Ordering::Relaxed);
    }

    /// R4: a `run` invocation acquired ownership and is proceeding.
    pub(super) fn inc_runs_started(&self) {
        self.runs_started.fetch_add(1, Ordering::Relaxed);
    }

    /// R18/R9: a `run` invocation is returning `DagOutcome { state, .. }`.
    pub(super) fn inc_run_terminal(&self, state: crate::scheduler::DagState) {
        match state {
            crate::scheduler::DagState::Succeeded => {
                self.runs_succeeded.fetch_add(1, Ordering::Relaxed);
            }
            crate::scheduler::DagState::Failed => {
                self.runs_failed.fetch_add(1, Ordering::Relaxed);
            }
            crate::scheduler::DagState::Cancelled => {
                self.runs_cancelled.fetch_add(1, Ordering::Relaxed);
            }
            crate::scheduler::DagState::ReplanRequired => {
                self.runs_replan_required.fetch_add(1, Ordering::Relaxed);
            }
            // Loop-internal only (D6); `run` never returns these (fix 24).
            crate::scheduler::DagState::Pending
            | crate::scheduler::DagState::Running
            | crate::scheduler::DagState::WaitingApproval => {}
        }
    }

    /// C3: a node was dispatched (`Ready`/`Pending` → `Running`).
    pub(super) fn inc_nodes_dispatched(&self) {
        self.nodes_dispatched.fetch_add(1, Ordering::Relaxed);
    }

    /// C4: a node reached `Succeeded`.
    pub(super) fn inc_nodes_succeeded(&self) {
        self.nodes_succeeded.fetch_add(1, Ordering::Relaxed);
    }

    /// C7/RC4: a node reached durable `Failed`.
    pub(super) fn inc_nodes_failed(&self) {
        self.nodes_failed.fetch_add(1, Ordering::Relaxed);
    }

    /// C6/C7/C9c/RC4: a node was marked `Skipped`.
    pub(super) fn inc_nodes_skipped_by(&self, n: usize) {
        if n > 0 {
            self.nodes_skipped.fetch_add(n as u64, Ordering::Relaxed);
        }
    }

    /// C8: a retry was admitted.
    pub(super) fn inc_retries_admitted(&self) {
        self.retries_admitted.fetch_add(1, Ordering::Relaxed);
    }

    /// A1-A6: a retry was rejected by admission.
    pub(super) fn inc_retries_rejected(&self) {
        self.retries_rejected.fetch_add(1, Ordering::Relaxed);
    }

    /// ES1: a tier escalation was applied to a dispatch.
    pub(super) fn inc_escalations(&self) {
        self.escalations.fetch_add(1, Ordering::Relaxed);
    }

    /// C9a: a gate was opened (first schedule).
    pub(super) fn inc_gates_opened(&self) {
        self.gates_opened.fetch_add(1, Ordering::Relaxed);
    }

    /// C9b: a gate resolved `allow` / `allow_once`.
    pub(super) fn inc_gates_allowed(&self) {
        self.gates_allowed.fetch_add(1, Ordering::Relaxed);
    }

    /// C9c: a gate resolved `deny`.
    pub(super) fn inc_gates_denied(&self) {
        self.gates_denied.fetch_add(1, Ordering::Relaxed);
    }

    /// C9c (§5.7.8 expiry): a gate resolved `expired`.
    pub(super) fn inc_gates_expired(&self) {
        self.gates_expired.fetch_add(1, Ordering::Relaxed);
    }

    /// BE1: the run stopped on budget exhaustion.
    pub(super) fn inc_budget_stops(&self) {
        self.budget_stops.fetch_add(1, Ordering::Relaxed);
    }

    /// T3/T7: a node-level timeout fired.
    pub(super) fn inc_node_timeouts(&self) {
        self.node_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    /// T7/T8: a run-level timeout fired.
    pub(super) fn inc_run_timeouts(&self) {
        self.run_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn snapshot(&self) -> SchedulerMetrics {
        let l = |c: &AtomicU64| c.load(Ordering::Relaxed);
        SchedulerMetrics {
            runs_started: l(&self.runs_started),
            runs_succeeded: l(&self.runs_succeeded),
            runs_failed: l(&self.runs_failed),
            runs_cancelled: l(&self.runs_cancelled),
            runs_replan_required: l(&self.runs_replan_required),
            nodes_dispatched: l(&self.nodes_dispatched),
            nodes_succeeded: l(&self.nodes_succeeded),
            nodes_failed: l(&self.nodes_failed),
            nodes_skipped: l(&self.nodes_skipped),
            retries_admitted: l(&self.retries_admitted),
            retries_rejected: l(&self.retries_rejected),
            escalations: l(&self.escalations),
            gates_opened: l(&self.gates_opened),
            gates_allowed: l(&self.gates_allowed),
            gates_denied: l(&self.gates_denied),
            gates_expired: l(&self.gates_expired),
            cas_conflicts: l(&self.cas_conflicts),
            event_repairs: l(&self.event_repairs),
            cancels: l(&self.cancels),
            forced_cancel_writes: l(&self.forced_cancel_writes),
            budget_stops: l(&self.budget_stops),
            node_timeouts: l(&self.node_timeouts),
            run_timeouts: l(&self.run_timeouts),
        }
    }
}
