//! [`InProcessMcpHost`] — lifecycle, admission, dispatch, drain (RFC-0006 §5 / §6).
//!
//! The host is the choke point: nothing reaches a builtin without passing the
//! §5.1 pipeline in order (phase → admit → expiry → lookup → parse → derive →
//! grant → dispatch). Cancellation ownership is a host-owned drop guard, so a
//! cloned token is a waiter and never keeps the host alive.
//!
//! Author: arkadianet

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use alloy_runtime::obs::{DecisionLog, ToolCallRecord};
use alloy_runtime::{
    McpServerSpec, PermissionToken, ServerId, ToolCall, ToolName, ToolResult, ToolSelector,
    ToolView,
};
use async_trait::async_trait;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::mcp::authz;
use crate::mcp::builtins::{self, BuiltinCtx};
use crate::mcp::disclose::{disclose, discloses_name};
use crate::mcp::error::McpError;
use crate::mcp::metrics::{McpMetrics, McpMetricsSnapshot};
use crate::mcp::patch::PatchApplyBackend;
use crate::mcp::platform::McpPlatform;
use crate::mcp::registry::Registry;
use crate::sandbox::grant::{trusted_path_dirs, trusted_roots};
use crate::sandbox::{OperatorHomes, PathPolicy, SandboxBroker};

/// Tool server name recorded in the [`DecisionLog`].
const BUILTIN_SERVER: &str = "alloy.builtins";

/// Extra headroom over the profile `exec_timeout` when no timeout is pinned.
const DEFAULT_TIMEOUT_HEADROOM: Duration = Duration::from_secs(60);

/// Additional bound on drain after the cancel token fires.
const DRAIN_CANCEL_GRACE: Duration = Duration::from_secs(5);

/// In-process dependency injection for the host. Not a TOML surface.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct McpHostConfig {
    /// Max concurrent `call` futures. Must be ≥ 1.
    pub max_in_flight: usize,
    /// Wall-clock timeout around **every** dispatch, including non-exec builtins.
    ///
    /// `None` resolves to `broker.profile().exec_timeout + 60s` at
    /// [`InProcessMcpHost::new`]. `Some(d)` is used as-is and construction
    /// fails when `d < exec_timeout`.
    pub call_timeout: Option<Duration>,
    /// Parent cancel token (runtime shutdown).
    pub cancel: CancellationToken,
}

impl McpHostConfig {
    /// Defaults: 64 in flight, derived timeout, fresh cancel token.
    #[must_use]
    pub fn new() -> Self {
        Self {
            max_in_flight: 64,
            call_timeout: None,
            cancel: CancellationToken::new(),
        }
    }

    /// Set the in-flight ceiling.
    #[must_use]
    pub fn with_max_in_flight(mut self, n: usize) -> Self {
        self.max_in_flight = n;
        self
    }

    /// Pin an explicit timeout (must be ≥ the profile `exec_timeout`).
    #[must_use]
    pub fn with_call_timeout(mut self, d: Duration) -> Self {
        self.call_timeout = Some(d);
        self
    }

    /// Adopt a parent cancel token.
    #[must_use]
    pub fn with_cancel(mut self, c: CancellationToken) -> Self {
        self.cancel = c;
        self
    }
}

impl Default for McpHostConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Observable host lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum McpHostPhase {
    /// Serving disclosures and admitting calls.
    Running = 1,
    /// Rejecting new work; in-flight calls finish or are cancelled.
    Draining = 2,
    /// Drain complete.
    Stopped = 3,
}

const PHASE_RUNNING: u8 = McpHostPhase::Running as u8;
const PHASE_DRAINING: u8 = McpHostPhase::Draining as u8;
const PHASE_STOPPED: u8 = McpHostPhase::Stopped as u8;

/// Cancels the host token when the owning [`InProcessMcpHost`] value dies.
///
/// Deliberately outside the shared `Arc<HostState>`: clones of the token handed
/// out by [`InProcessMcpHost::cancellation`] are waiters, so retaining one must
/// not keep the host alive nor suppress cancel-on-drop.
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct HostState {
    broker: Arc<dyn SandboxBroker>,
    path_policy: PathPolicy,
    trusted_path: Vec<PathBuf>,
    patch_backend: Arc<dyn PatchApplyBackend>,
    registry: Registry,
    call_timeout: Duration,
    phase: AtomicU8,
    in_flight: AtomicUsize,
    permits: Arc<Semaphore>,
    drain_notify: Notify,
    metrics: McpMetrics,
    decision_log: OnceLock<Arc<dyn DecisionLog>>,
}

impl HostState {
    fn phase(&self) -> u8 {
        self.phase.load(Ordering::SeqCst)
    }
}

/// Releases the in-flight permit and wakes drain waiters on completion **or**
/// drop, so a cancelled caller cannot wedge `drain`.
struct Admission {
    state: Arc<HostState>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.state.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.state.drain_notify.notify_waiters();
    }
}

/// The in-process MCP host: sole tool bus for Alloy.
///
/// Not `Clone` — share it as `Arc<InProcessMcpHost>` or keep a single owner.
pub struct InProcessMcpHost {
    state: Arc<HostState>,
    /// Unique to this host value; must not live inside `state`.
    cancel_guard: CancelOnDrop,
}

impl std::fmt::Debug for InProcessMcpHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessMcpHost")
            .field("phase", &self.phase())
            .field("call_timeout", &self.state.call_timeout)
            .field("tools", &self.state.registry.names().len())
            .finish_non_exhaustive()
    }
}

impl InProcessMcpHost {
    /// Build the host and register the four builtins.
    ///
    /// `PathPolicy` is constructed here from `broker.profile()` plus
    /// `read_only_roots`; callers cannot inject a divergent policy. `homes` MUST
    /// be the same [`OperatorHomes`] used to build the broker so the exec
    /// pre-check resolves binaries against the same trusted roots.
    ///
    /// # Errors
    ///
    /// [`McpError::Internal`] when `max_in_flight == 0` or an explicit
    /// `call_timeout` is shorter than the profile `exec_timeout`; a mapped
    /// sandbox error when `PathPolicy::from_profile` fails.
    pub fn new(
        broker: Arc<dyn SandboxBroker>,
        homes: OperatorHomes,
        read_only_roots: Vec<PathBuf>,
        patch_backend: Arc<dyn PatchApplyBackend>,
        config: McpHostConfig,
    ) -> Result<Self, McpError> {
        if config.max_in_flight == 0 {
            return Err(McpError::Internal("max_in_flight must be >= 1".into()));
        }
        let exec_timeout = broker.profile().exec_timeout;
        let call_timeout = match config.call_timeout {
            None => exec_timeout.saturating_add(DEFAULT_TIMEOUT_HEADROOM),
            Some(d) if d < exec_timeout => {
                return Err(McpError::Internal("call_timeout < exec_timeout".into()))
            }
            Some(d) => d,
        };

        // Same union the broker builds per exec: PATH search dirs first, then
        // the broader membership roots.
        let mut trusted_path = trusted_path_dirs(Some(&homes.cargo_home), Some(&homes.rustup_home));
        for root in trusted_roots(Some(&homes.cargo_home), Some(&homes.rustup_home)) {
            if !trusted_path.contains(&root) {
                trusted_path.push(root);
            }
        }

        let path_policy = PathPolicy::from_profile(broker.profile(), read_only_roots)
            .map_err(crate::mcp::error::map_sandbox_error)?;

        let state = Arc::new(HostState {
            broker,
            path_policy,
            trusted_path,
            patch_backend,
            registry: Registry::builtins(),
            call_timeout,
            phase: AtomicU8::new(PHASE_RUNNING),
            in_flight: AtomicUsize::new(0),
            permits: Arc::new(Semaphore::new(config.max_in_flight)),
            drain_notify: Notify::new(),
            metrics: McpMetrics::default(),
            decision_log: OnceLock::new(),
        });

        Ok(Self {
            state,
            cancel_guard: CancelOnDrop(config.cancel),
        })
    }

    /// Install an optional [`DecisionLog`]; records are awaited before `call` returns.
    #[must_use]
    pub fn with_decision_log(self, log: Arc<dyn DecisionLog>) -> Self {
        if self.state.decision_log.set(log).is_err() {
            tracing::warn!("mcp host already has a decision log; ignoring replacement");
        }
        self
    }

    /// Begin drain: reject new admissions, wait up to `grace` for in-flight
    /// work, then cancel. Idempotent and safe to call concurrently.
    ///
    /// # Errors
    ///
    /// [`McpError::Internal`] when in-flight work has still not finished five
    /// seconds after the cancel; the phase is set to `Stopped` regardless.
    pub async fn drain(&self, grace: Duration) -> Result<(), McpError> {
        let state = &self.state;
        tracing::info!(
            in_flight = state.in_flight.load(Ordering::SeqCst),
            grace_ms = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX),
            "mcp host drain"
        );

        // 1. Winner/follower election through the phase CAS alone.
        loop {
            match state.phase() {
                PHASE_STOPPED => return Ok(()),
                PHASE_DRAINING => {
                    self.follow_drain(grace).await;
                    return Ok(());
                }
                _ => {
                    if state
                        .phase
                        .compare_exchange(
                            PHASE_RUNNING,
                            PHASE_DRAINING,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_ok()
                    {
                        break;
                    }
                }
            }
        }

        // 2. Winner: let in-flight work finish within `grace`.
        self.wait_for_idle(Some(grace)).await;

        // 3. Still busy — cancel so still-polled calls observe it.
        let mut drained = state.in_flight.load(Ordering::SeqCst) == 0;
        if !drained {
            self.cancel_guard.0.cancel();
            // 4. Bounded wait for the cancelled calls to unwind.
            self.wait_for_idle(Some(DRAIN_CANCEL_GRACE)).await;
            drained = state.in_flight.load(Ordering::SeqCst) == 0;
        }

        // 5. Stopped either way; a stuck call must not leave the host Draining.
        state.phase.store(PHASE_STOPPED, Ordering::SeqCst);
        state.drain_notify.notify_waiters();
        if drained {
            Ok(())
        } else {
            Err(McpError::Internal(
                "drain: in-flight did not reach 0".into(),
            ))
        }
    }

    /// Follower path: wait for the winner to reach `Stopped`, bounded.
    async fn follow_drain(&self, grace: Duration) {
        let deadline = Instant::now() + grace + DRAIN_CANCEL_GRACE;
        loop {
            // Enable-then-check: subscribing before the test avoids a lost wakeup.
            let notified = self.state.drain_notify.notified();
            tokio::pin!(notified);
            if self.state.phase() == PHASE_STOPPED {
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            let _ = tokio::time::timeout(remaining, notified).await;
        }
    }

    /// Wait until `in_flight == 0` or `limit` elapses.
    async fn wait_for_idle(&self, limit: Option<Duration>) {
        let deadline = limit.map(|d| Instant::now() + d);
        loop {
            let notified = self.state.drain_notify.notified();
            tokio::pin!(notified);
            if self.state.in_flight.load(Ordering::SeqCst) == 0 {
                return;
            }
            match deadline {
                None => notified.await,
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return;
                    }
                    let _ = tokio::time::timeout(remaining, notified).await;
                }
            }
        }
    }

    /// Clone of the host cancel token. Clones are waiters only.
    #[must_use]
    pub fn cancellation(&self) -> CancellationToken {
        self.cancel_guard.0.clone()
    }

    /// Current lifecycle phase.
    #[must_use]
    pub fn phase(&self) -> McpHostPhase {
        match self.state.phase() {
            PHASE_RUNNING => McpHostPhase::Running,
            PHASE_DRAINING => McpHostPhase::Draining,
            _ => McpHostPhase::Stopped,
        }
    }

    /// Counter snapshot.
    #[must_use]
    pub fn metrics(&self) -> McpMetricsSnapshot {
        let in_flight = self.state.in_flight.load(Ordering::SeqCst);
        self.state
            .metrics
            .snapshot(u64::try_from(in_flight).unwrap_or(u64::MAX))
    }

    /// Registered tool names, sorted. MVP: exactly the four builtins.
    #[must_use]
    pub fn registered_names(&self) -> Vec<ToolName> {
        self.state.registry.names()
    }

    /// Admission: permit, then **increment then recheck phase** (RFC-0006 §5.1).
    ///
    /// Incrementing before the recheck makes drain's `in_flight == 0` observation
    /// sound: a drain that sees zero must have CAS'd before this call's phase
    /// load, so the call is rejected rather than racing past an "already idle"
    /// drain path that never cancels.
    async fn admit(state: &Arc<HostState>) -> Result<Admission, McpError> {
        if state.phase() != PHASE_RUNNING {
            return Err(McpError::ShuttingDown);
        }
        let permit = Arc::clone(&state.permits)
            .acquire_owned()
            .await
            .map_err(|_| McpError::ShuttingDown)?;
        // Count before the phase recheck so drain cannot observe idle mid-admit.
        state.in_flight.fetch_add(1, Ordering::SeqCst);
        let admission = Admission {
            state: Arc::clone(state),
            _permit: permit,
        };
        if admission.state.phase() != PHASE_RUNNING {
            // Drop decrements in_flight and notifies drain waiters.
            drop(admission);
            return Err(McpError::ShuttingDown);
        }
        Ok(admission)
    }

    async fn run_call(
        &self,
        call: &ToolCall,
        perms: PermissionToken,
    ) -> Result<ToolResult, McpError> {
        let state = &self.state;

        authz::check_expiry(&perms)?;

        let id = state
            .registry
            .lookup(&call.name)
            .ok_or_else(|| McpError::UnknownTool(call.name.to_string()))?;

        let ctx = BuiltinCtx {
            broker: state.broker.as_ref(),
            path_policy: &state.path_policy,
            trusted_path: &state.trusted_path,
            patch_backend: state.patch_backend.as_ref(),
        };

        // Parse + derive + grant, all before any spawn or file open.
        let outcome = match builtins::prepare(id, &ctx, call, &perms) {
            Err(err) => Err(err),
            Ok(prepared) => {
                let dispatch = builtins::execute(&ctx, prepared, perms);
                tokio::select! {
                    () = self.cancel_guard.0.cancelled() => Err(McpError::Cancelled),
                    result = tokio::time::timeout(state.call_timeout, dispatch) => match result {
                        Ok(inner) => inner,
                        Err(_) => Err(McpError::Timeout(state.call_timeout)),
                    },
                }
            }
        };

        // §9.1: warn on every PermissionDenied — prepare-time and broker-mapped.
        match &outcome {
            Err(McpError::PermissionDenied(reason)) => {
                tracing::warn!(tool = %call.name, %reason, "mcp permission denied");
            }
            Err(McpError::Cancelled) => tracing::info!(tool = %call.name, "mcp call cancelled"),
            Err(McpError::Timeout(_)) => tracing::info!(tool = %call.name, "mcp call timed out"),
            _ => {}
        }

        outcome.map(|result| result.with_call_id(call.call_id.clone()))
    }

    /// Await the optional [`DecisionLog`] write. Skipped entirely when the call
    /// carries no session; obs failures warn and never change the return value.
    async fn record_call(&self, call: &ToolCall, latency: Duration, denied: bool) {
        let Some(log) = self.state.decision_log.get() else {
            return;
        };
        let Some(session) = call.session else {
            return;
        };
        let record = ToolCallRecord {
            session,
            run: call.run,
            node: call.node,
            tool_name: call.name.as_str().to_string(),
            tool_server: Some(BUILTIN_SERVER.to_string()),
            latency_ms: Some(u64::try_from(latency.as_millis()).unwrap_or(u64::MAX)),
            denied,
            content_hash: None,
            body: None,
        };
        if let Err(err) = log.record_tool_call(record).await {
            tracing::warn!(%err, "mcp decision log record failed");
        }
    }

    async fn call_inner(
        &self,
        call: ToolCall,
        perms: PermissionToken,
    ) -> Result<ToolResult, McpError> {
        let queued_at = Instant::now();

        // Held for the whole call, including the obs write, so `drain` cannot
        // observe idle while a record is still pending.
        let admitted = Self::admit(&self.state).await;
        // §9.2 measures from admission; time spent waiting for a permit is not
        // tool latency. A rejected admission has no later reference point.
        let started = if admitted.is_ok() {
            Instant::now()
        } else {
            queued_at
        };

        let outcome = match &admitted {
            Err(_) => Err(McpError::ShuttingDown),
            Ok(_) => self.run_call(&call, perms).await,
        };

        let denied = matches!(&outcome, Err(McpError::PermissionDenied(_)));
        match &outcome {
            Ok(result) if result.is_error() => self.state.metrics.call_tool_error(),
            Ok(_) => self.state.metrics.call_ok(),
            Err(_) => {
                self.state.metrics.call_mcp_error();
                if denied {
                    self.state.metrics.denial();
                }
            }
        }

        self.record_call(&call, started.elapsed(), denied).await;
        drop(admitted);
        outcome
    }
}

#[async_trait]
impl McpPlatform for InProcessMcpHost {
    async fn start_server(&self, spec: McpServerSpec) -> Result<ServerId, McpError> {
        Err(McpError::Unsupported(format!(
            "out-of-process MCP servers are not available in MVP: {}",
            spec.name
        )))
    }

    async fn stop_server(&self, id: ServerId) -> Result<(), McpError> {
        let _ = id;
        Err(McpError::Unsupported(
            "out-of-process MCP servers are not available in MVP".into(),
        ))
    }

    async fn tools_for(&self, selectors: &[ToolSelector]) -> Result<Vec<ToolView>, McpError> {
        if self.state.phase() != PHASE_RUNNING {
            return Err(McpError::ShuttingDown);
        }
        let span = tracing::debug_span!("alloy.mcp.disclose", selector_count = selectors.len());
        let _enter = span.enter();

        let (views, truncated) = disclose(self.state.registry.views(), selectors);
        if truncated {
            self.state.metrics.disclose_truncated();
            tracing::warn!(
                truncated = true,
                returned = views.len(),
                "mcp disclosure truncated"
            );
        }
        tracing::debug!(returned = views.len(), truncated, "mcp disclosure");
        Ok(views)
    }

    async fn discloses(
        &self,
        selectors: &[ToolSelector],
        name: &ToolName,
    ) -> Result<bool, McpError> {
        if self.state.phase() != PHASE_RUNNING {
            return Err(McpError::ShuttingDown);
        }
        Ok(discloses_name(self.state.registry.views(), selectors, name))
    }

    async fn call(&self, call: ToolCall, perms: PermissionToken) -> Result<ToolResult, McpError> {
        let span = tracing::info_span!(
            "alloy.mcp.call",
            tool = %call.name,
            run_id = ?call.run,
            call_id = ?call.call_id,
            builtin = true,
        );
        self.call_inner(call, perms).instrument(span).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::patch::StubPatchApplyBackend;
    use crate::sandbox::{RecordingSandboxBroker, SandboxProfile};
    use alloy_runtime::{ProfileId, RunId};

    fn homes(dir: &std::path::Path) -> OperatorHomes {
        OperatorHomes::new(dir.join("cargo"), dir.join("rustup"))
    }

    fn host_in(
        jail: &std::path::Path,
        config: McpHostConfig,
    ) -> Result<InProcessMcpHost, McpError> {
        let profile = SandboxProfile::default_for_jail(jail.to_path_buf()).unwrap();
        let broker: Arc<dyn SandboxBroker> = Arc::new(RecordingSandboxBroker::new(profile));
        InProcessMcpHost::new(
            broker,
            homes(jail),
            Vec::new(),
            Arc::new(StubPatchApplyBackend),
            config,
        )
    }

    fn token() -> PermissionToken {
        PermissionToken {
            profile: ProfileId::new("default").unwrap(),
            grants: Vec::new(),
            expires: None,
            run_id: RunId::new(),
        }
    }

    #[test]
    fn construction_rejects_zero_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let err = host_in(dir.path(), McpHostConfig::new().with_max_in_flight(0)).unwrap_err();
        assert!(matches!(err, McpError::Internal(ref m) if m.contains("max_in_flight")));
    }

    #[test]
    fn construction_explicit_timeout_too_small() {
        let dir = tempfile::tempdir().unwrap();
        let config = McpHostConfig::new().with_call_timeout(Duration::from_secs(1));
        let err = host_in(dir.path(), config).unwrap_err();
        assert!(matches!(err, McpError::Internal(ref m) if m.contains("call_timeout")));
    }

    #[test]
    fn construction_call_timeout_default_ok() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path(), McpHostConfig::new()).unwrap();
        // Profile default exec_timeout is 1800s; the host adds 60s of headroom.
        assert_eq!(host.state.call_timeout, Duration::from_secs(1860));
        assert_eq!(host.phase(), McpHostPhase::Running);
    }

    #[test]
    fn no_graph_query_registered() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path(), McpHostConfig::new()).unwrap();
        let names: Vec<String> = host
            .registered_names()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            names,
            vec!["apply_patch", "cargo_check", "cargo_test", "fs_read"]
        );
    }

    #[tokio::test]
    async fn unknown_tool_err() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path(), McpHostConfig::new()).unwrap();
        let call = ToolCall::new(ToolName::new("bash").unwrap(), serde_json::json!({}));
        let err = host.call(call, token()).await.unwrap_err();
        assert!(matches!(err, McpError::UnknownTool(ref n) if n == "bash"));
        assert_eq!(host.metrics().calls_mcp_error, 1);
    }

    #[tokio::test]
    async fn start_and_stop_server_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path(), McpHostConfig::new()).unwrap();
        let spec = McpServerSpec::new(
            "crates",
            alloy_runtime::McpTransport::Stdio {
                command: "crates-mcp".into(),
                args: vec![],
            },
        );
        assert!(matches!(
            host.start_server(spec).await,
            Err(McpError::Unsupported(_))
        ));
        assert!(matches!(
            host.stop_server(ServerId::new()).await,
            Err(McpError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn drain_rejects_new_calls() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path(), McpHostConfig::new()).unwrap();
        host.drain(Duration::from_millis(50)).await.unwrap();
        assert_eq!(host.phase(), McpHostPhase::Stopped);

        let call = ToolCall::new(ToolName::new("fs_read").unwrap(), serde_json::json!({}));
        assert!(matches!(
            host.call(call, token()).await,
            Err(McpError::ShuttingDown)
        ));
        assert!(matches!(
            host.tools_for(&[ToolSelector::tag("sel.fs")]).await,
            Err(McpError::ShuttingDown)
        ));
    }

    #[tokio::test]
    async fn drain_idempotent_concurrent_followers() {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(host_in(dir.path(), McpHostConfig::new()).unwrap());
        let a = Arc::clone(&host);
        let b = Arc::clone(&host);
        let joined = tokio::time::timeout(
            Duration::from_secs(5),
            futures_join(
                async move { a.drain(Duration::from_millis(20)).await },
                async move { b.drain(Duration::from_millis(20)).await },
            ),
        )
        .await
        .expect("drain must not hang");
        assert!(joined.0.is_ok());
        assert!(joined.1.is_ok());
        assert_eq!(host.phase(), McpHostPhase::Stopped);
        // A third drain after Stopped is still a no-op success.
        assert!(host.drain(Duration::from_millis(1)).await.is_ok());
    }

    /// Minimal two-future join so the crate needs no `futures` dependency.
    async fn futures_join<A, B, TA, TB>(a: A, b: B) -> (TA, TB)
    where
        A: std::future::Future<Output = TA> + Send + 'static,
        B: std::future::Future<Output = TB> + Send + 'static,
        TA: Send + 'static,
        TB: Send + 'static,
    {
        let ha = tokio::spawn(a);
        let hb = tokio::spawn(b);
        (ha.await.unwrap(), hb.await.unwrap())
    }

    #[tokio::test]
    async fn host_drop_cancels_cloned_waiter() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path(), McpHostConfig::new()).unwrap();
        let waiter = host.cancellation();
        let task = tokio::spawn(async move { waiter.cancelled().await });
        drop(host);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("waiter must observe cancel")
            .unwrap();
    }

    #[tokio::test]
    async fn tools_for_empty_selectors_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_in(dir.path(), McpHostConfig::new()).unwrap();
        assert!(host.tools_for(&[]).await.unwrap().is_empty());
        let views = host
            .tools_for(&[ToolSelector::tag("sel.compiler")])
            .await
            .unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].name.as_str(), "cargo_check");
    }
}
