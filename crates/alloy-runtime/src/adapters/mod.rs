//! Runtime node adapters (Verify*/GateHuman) — stubs until RFC-0010/0006.

mod capability;
mod diagnostics;
mod gate;
mod perms;
mod seed;
mod tool_caller;
// `pub(crate)` so `lang::runner` can reuse the single cargo argv path and
// exit-code classification (RFC-0014 LB9) without a second implementation.
pub(crate) mod verify;

pub use capability::{
    CapabilityExecContext, CapabilityExecError, CapabilityExecutor, CapabilityOutcome,
    UnavailableCapabilityExecutor,
};
pub use diagnostics::{diagnostic_fingerprint, parse_rustc_diagnostics};
pub use gate::SessionGateHumanAdapter;
pub use perms::{SessionVerifyPermissions, VerifyClass, VerifyPermissions};
pub use seed::{seed_graph_diagnostics, SeedReport};
pub use tool_caller::{ToolCaller, ToolCallerError};
pub use verify::{McpVerifyCompileAdapter, McpVerifyTestAdapter};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::error::AdapterError;
use crate::types::diagnostic::DiagnosticEvent;
use crate::types::ids::{ArtifactId, DagId, GateId, NodeId, RunId, SessionId};

/// One verifier trait for every "did it work" question (research §7.11
/// item 5; RFC-0019 grows this). Compile, test, and any future verifier
/// (clippy, `NoNewUnsafe`) answer through the same [`Verdict`] type so two
/// implementations of the same question can never silently disagree again.
#[async_trait]
pub trait Verifier: Send + Sync {
    /// Run the verification for a node.
    async fn verify(&self, ctx: &NodeExecContext) -> Result<Verdict, AdapterError>;
}

/// Human gate adapter.
#[async_trait]
pub trait GateHumanAdapter: Send + Sync {
    /// Wait for approval (RunController::approve resumes).
    async fn wait_approval(
        &self,
        ctx: &NodeExecContext,
        gate: GateId,
    ) -> Result<Approval, AdapterError>;
}

/// Serde-safe node execution identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeExecRef {
    /// Session id.
    pub session_id: SessionId,
    /// Run id.
    pub run_id: RunId,
    /// DAG id.
    pub dag_id: DagId,
    /// Node id.
    pub node_id: NodeId,
    /// Workspace root.
    pub workspace_root: std::path::PathBuf,
    /// 1-based attempt index for this dispatch (RFC-0010 §3.1.1 NX1).
    ///
    /// MUST be `>= 1` whenever a node is dispatched (checkpoint C3). Gate
    /// **wait** contexts (unresolved, no C3 yet) use `0`.
    pub attempt: u32,
}

/// Runtime execution context (not serde; holds cancellation).
#[derive(Debug, Clone)]
pub struct NodeExecContext {
    /// Persistable identity.
    pub meta: NodeExecRef,
    /// Cancellation token.
    pub cancellation: CancellationToken,
}

/// Three-valued verification outcome (research §7.11 item 6).
///
/// The old `ok: bool` mislabelled infrastructure failures as agent
/// failures; `Inconclusive` is the honest label for "the verifier could not
/// produce an answer" — a signal-killed cargo, truncated output, or a
/// missing exit code. Training labels and retry policy both depend on the
/// distinction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictOutcome {
    /// The property verified holds.
    Pass,
    /// The property verified does not hold — an agent-attributable failure.
    Fail,
    /// The verifier ran but could not answer; retryable, never an agent
    /// failure label.
    Inconclusive {
        /// Why no answer exists.
        reason: String,
    },
}

/// Verifier result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    /// Three-valued outcome.
    pub outcome: VerdictOutcome,
    /// Diagnostics produced.
    pub diagnostics: Vec<DiagnosticEvent>,
    /// Optional raw log artifact.
    pub raw_artifact: Option<ArtifactId>,
}

impl Verdict {
    /// A bare passing verdict.
    #[must_use]
    pub fn pass() -> Self {
        Self {
            outcome: VerdictOutcome::Pass,
            diagnostics: vec![],
            raw_artifact: None,
        }
    }

    /// A bare failing verdict.
    #[must_use]
    pub fn fail() -> Self {
        Self {
            outcome: VerdictOutcome::Fail,
            diagnostics: vec![],
            raw_artifact: None,
        }
    }

    /// `true` iff the outcome is [`VerdictOutcome::Pass`].
    #[must_use]
    pub fn passed(&self) -> bool {
        matches!(self.outcome, VerdictOutcome::Pass)
    }
}

/// The one shared decision for "did cargo's answer mean pass, fail, or
/// no-answer" (research §7.11 items 5/6). Both the runtime verify adapters
/// and `alloy-eval`'s `compile_clean` MUST route through this function, so
/// a disagreement is a compile error rather than a silent divergence.
///
/// - exit 0 with no error-level diagnostics → `Pass`
/// - exit 0 **with** error-level diagnostics → `Fail` (diagnostics win)
/// - exit 101 → `Fail` (the normal compile/test failure signal) — **unless**
///   `fail_requires_diagnostics` is set and none were parsed, in which case
///   cargo died before compiling anything (config load failure, internal
///   error) and the verdict is `Inconclusive`. Check/compile callers pass
///   `true` (a failing build always yields rustc error diagnostics); test
///   callers pass `false` (test failures produce none by design, DG7).
/// - any other exit, or no exit code → `Inconclusive` (cargo itself failed;
///   nothing about the agent's patch was decided)
#[must_use]
pub fn cargo_exit_verdict(
    exit_code: Option<i64>,
    has_error_diagnostics: bool,
    fail_requires_diagnostics: bool,
) -> VerdictOutcome {
    match exit_code {
        Some(0) => {
            if has_error_diagnostics {
                VerdictOutcome::Fail
            } else {
                VerdictOutcome::Pass
            }
        }
        Some(101) if fail_requires_diagnostics && !has_error_diagnostics => {
            VerdictOutcome::Inconclusive {
                reason: "cargo exited 101 with no error diagnostics (cargo itself failed \
                         before compiling; environment, not a compile verdict)"
                    .into(),
            }
        }
        Some(101) => VerdictOutcome::Fail,
        Some(other) => VerdictOutcome::Inconclusive {
            reason: format!("cargo exited {other}"),
        },
        None => VerdictOutcome::Inconclusive {
            reason: "no exit code reported".into(),
        },
    }
}

/// Human approval decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    /// Allow ongoing.
    Allow,
    /// Deny.
    Deny,
    /// Allow once.
    AllowOnce,
}

/// Unavailable verifier stub (compile slot).
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableVerifyCompile;

/// Unavailable verifier stub (test slot).
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableVerifyTest;

/// Unavailable gate adapter stub.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableGateHuman;

#[async_trait]
impl Verifier for UnavailableVerifyCompile {
    async fn verify(&self, _ctx: &NodeExecContext) -> Result<Verdict, AdapterError> {
        Err(AdapterError::Unavailable)
    }
}

#[async_trait]
impl Verifier for UnavailableVerifyTest {
    async fn verify(&self, _ctx: &NodeExecContext) -> Result<Verdict, AdapterError> {
        Err(AdapterError::Unavailable)
    }
}

#[async_trait]
impl GateHumanAdapter for UnavailableGateHuman {
    async fn wait_approval(
        &self,
        _ctx: &NodeExecContext,
        _gate: GateId,
    ) -> Result<Approval, AdapterError> {
        Err(AdapterError::Unavailable)
    }
}
