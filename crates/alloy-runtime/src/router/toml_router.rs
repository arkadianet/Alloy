//! TOML-backed router orchestration, metering, and lifecycle.

#[cfg(feature = "http-provider")]
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use scopeguard::guard;
use tokio::sync::{oneshot, watch, Notify, Semaphore, SemaphorePermit};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::obs::{
    BudgetCheck, DecisionLog, DecisionRecord, ModelCallRecord, ObsError, SharedCostMeter,
};
use crate::types::budget::BudgetPolicy;
use crate::types::ids::{EventSeq, RunId};

use super::config::RouterConfig;
use super::decision_bridge::{
    budget_decision_for_complete, budget_decision_for_route, route_decision, BudgetCounters,
};
use super::error::{normalize_provider_error, RouterError};
use super::meter_bridge::build_model_call_record;
use super::metrics::{RouterMetrics, RouterMetricsSnapshot};
#[cfg(feature = "http-provider")]
use super::openai::{OpenAiCompatibleProvider, OpenAiCompatibleSpec};
#[cfg(feature = "http-provider")]
use super::secret::SecretString;
use super::select::{
    apply_usd_ceiling_overlay, check_budget_snapshot, resolve_tier, select_endpoint, TierSource,
};
use super::traits::{ModelProvider, ModelRouter};
use super::types::{
    redact_and_truncate, CompletionRequest, ModelResponse, PromptPack, ResponseFormat, RoutedModel,
    RoutingRequest, ToolChoice,
};

const PHASE_READY: u8 = 0;
const PHASE_DRAINING: u8 = 1;
const PHASE_STOPPED: u8 = 2;
const POST_CANCEL_MAX: Duration = Duration::from_secs(1);
static NEXT_ROUTER_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

/// Dependencies and validated inputs for [`TomlModelRouter::from_parts`].
#[non_exhaustive]
pub struct TomlModelRouterParts {
    /// Router configuration; all invariants are revalidated.
    pub config: RouterConfig,
    /// Provider implementation matching the sole configured provider id.
    pub provider: Arc<dyn ModelProvider>,
    /// Run-level budget policy.
    pub budget_policy: BudgetPolicy,
    /// Decision log required for production routers.
    pub decision_log: Option<Arc<dyn DecisionLog>>,
    /// Cost meter required for production routers.
    pub cost_meter: Option<SharedCostMeter>,
    /// Run to which this router is bound.
    pub bound_run: Option<RunId>,
    /// Test-only escape hatch for isolated router unit tests.
    #[cfg(test)]
    pub allow_unmetered: bool,
    /// Optional host cancellation token.
    pub shutdown_token: Option<CancellationToken>,
}

impl TomlModelRouterParts {
    /// Construct router dependencies with host cancellation disabled.
    pub fn new(
        config: RouterConfig,
        provider: Arc<dyn ModelProvider>,
        budget_policy: BudgetPolicy,
        decision_log: Option<Arc<dyn DecisionLog>>,
        cost_meter: Option<SharedCostMeter>,
        bound_run: Option<RunId>,
    ) -> Self {
        Self {
            config,
            provider,
            budget_policy,
            decision_log,
            cost_meter,
            bound_run,
            #[cfg(test)]
            allow_unmetered: false,
            shutdown_token: None,
        }
    }

    /// Attach a host cancellation token.
    #[must_use]
    pub fn shutdown_token(mut self, token: CancellationToken) -> Self {
        self.shutdown_token = Some(token);
        self
    }

    /// Permit missing observability dependencies in isolated unit tests.
    #[cfg(test)]
    #[must_use]
    pub fn allow_unmetered(mut self) -> Self {
        self.allow_unmetered = true;
        self
    }
}

/// Final, shared result of router shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouterShutdownReport {
    /// Whether shutdown grace elapsed and cancellation was signalled.
    pub cancelled_in_flight: bool,
    /// Provider or route calls still admitted after post-cancel waiting.
    pub remaining_in_flight: usize,
    /// Durable model-call appends still pending after aggregate drain.
    pub remaining_appends: usize,
}

/// Run-scoped router loaded from strict TOML or injected validated parts.
pub struct TomlModelRouter {
    config: RouterConfig,
    provider: Arc<dyn ModelProvider>,
    budget_policy: BudgetPolicy,
    decision_log: Option<Arc<dyn DecisionLog>>,
    cost_meter: Option<SharedCostMeter>,
    bound_run: Option<RunId>,
    router_instance_id: u64,
    phase: Arc<AtomicU8>,
    semaphore: Semaphore,
    admission_notify: Notify,
    in_flight_notify: Arc<Notify>,
    shutdown_token: CancellationToken,
    report_tx: watch::Sender<Option<RouterShutdownReport>>,
    metrics: Arc<RouterMetrics>,
    append_supervisor: Arc<DurableAppendSupervisor>,
}

impl TomlModelRouter {
    /// Load configuration, resolve the API key, and build the HTTP provider.
    ///
    /// Missing or empty key variables fail closed and mention `example_env_hint`;
    /// this constructor never reads or writes a `.env` file.
    #[cfg(feature = "http-provider")]
    pub fn from_paths(
        router_path: &Path,
        budget_policy: BudgetPolicy,
        example_env_hint: &Path,
        decision_log: Arc<dyn DecisionLog>,
        cost_meter: SharedCostMeter,
        bound_run: RunId,
    ) -> Result<Self, RouterError> {
        let config = RouterConfig::load(router_path)?;
        validate_price_completeness(&config, &budget_policy)?;
        let provider_config = config
            .providers
            .first()
            .ok_or_else(|| RouterError::Config("at least one provider is required".into()))?;
        let api_key = std::env::var(&provider_config.api_key_env)
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                tracing::error!(
                    env_key = %provider_config.api_key_env,
                    hint = %example_env_hint.display(),
                    "model provider API key is missing"
                );
                RouterError::Config(format!(
                    "environment variable {} is unset or empty (see {})",
                    provider_config.api_key_env,
                    example_env_hint.display()
                ))
            })?;
        let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleSpec {
            id: provider_config.id.clone(),
            base_url: provider_config.base_url.clone(),
            api_key: SecretString::new(api_key),
            connect_timeout: config.policy.connect_timeout,
            request_timeout: config.policy.request_timeout,
        })
        .map_err(|error| RouterError::Config(redact_and_truncate(&error.to_string(), 512)))?;
        Self::from_parts(TomlModelRouterParts::new(
            config,
            Arc::new(provider),
            budget_policy,
            Some(decision_log),
            Some(cost_meter),
            Some(bound_run),
        ))
    }

    /// Construct from injected parts after revalidating all configuration invariants.
    pub fn from_parts(mut parts: TomlModelRouterParts) -> Result<Self, RouterError> {
        parts.config.validate_and_normalize()?;
        validate_price_completeness(&parts.config, &parts.budget_policy)?;

        let configured_provider = parts
            .config
            .providers
            .first()
            .ok_or_else(|| RouterError::Config("at least one provider is required".into()))?;
        if parts.provider.id() != configured_provider.id {
            return Err(RouterError::Config(
                "injected provider id does not match router config".into(),
            ));
        }

        #[cfg(test)]
        let allow_unmetered = parts.allow_unmetered;
        #[cfg(not(test))]
        let allow_unmetered = false;

        if !allow_unmetered {
            if parts.decision_log.is_none() {
                return Err(RouterError::Config(
                    "production router requires a decision log".into(),
                ));
            }
            if parts.cost_meter.is_none() {
                return Err(RouterError::Config(
                    "production router requires a cost meter".into(),
                ));
            }
            if parts.bound_run.is_none() {
                return Err(RouterError::Config(
                    "metered router requires bound_run".into(),
                ));
            }
        }

        let metrics = Arc::new(RouterMetrics::default());
        let append_supervisor = Arc::new(DurableAppendSupervisor {
            pending: AtomicUsize::new(0),
            done_notify: Notify::new(),
            obs_record_errors: Arc::clone(&metrics.obs_record_errors),
        });
        let (report_tx, _) = watch::channel(None);
        Ok(Self {
            semaphore: Semaphore::new(parts.config.policy.max_in_flight as usize),
            config: parts.config,
            provider: parts.provider,
            budget_policy: parts.budget_policy,
            decision_log: parts.decision_log,
            cost_meter: parts.cost_meter,
            bound_run: parts.bound_run,
            router_instance_id: NEXT_ROUTER_INSTANCE_ID.fetch_add(1, Ordering::SeqCst),
            phase: Arc::new(AtomicU8::new(PHASE_READY)),
            admission_notify: Notify::new(),
            in_flight_notify: Arc::new(Notify::new()),
            shutdown_token: parts.shutdown_token.unwrap_or_default(),
            report_tx,
            metrics,
            append_supervisor,
        })
    }

    /// Return a point-in-time metrics snapshot.
    #[must_use]
    pub fn metrics(&self) -> RouterMetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Begin draining and return the final report shared by every shutdown caller.
    ///
    /// The CAS winner spawns a runtime-owned drain task (requires a Tokio runtime)
    /// so an aborted first caller cannot orphan drain leadership; every caller
    /// then waits on the shared `watch` report (§6.6).
    pub async fn shutdown(&self) -> RouterShutdownReport {
        let mut report_rx = self.report_tx.subscribe();
        if let Some(report) = *report_rx.borrow() {
            return report;
        }

        if self
            .phase
            .compare_exchange(
                PHASE_READY,
                PHASE_DRAINING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            self.semaphore.close();
            self.admission_notify.notify_waiters();

            let grace = self.config.policy.shutdown_grace;
            let metrics = Arc::clone(&self.metrics);
            let in_flight_notify = Arc::clone(&self.in_flight_notify);
            let shutdown_token = self.shutdown_token.clone();
            let append_supervisor = Arc::clone(&self.append_supervisor);
            let phase = Arc::clone(&self.phase);
            let report_tx = self.report_tx.clone();
            tokio::spawn(async move {
                let report = drain_shutdown(
                    grace,
                    &metrics,
                    &in_flight_notify,
                    &shutdown_token,
                    &append_supervisor,
                )
                .await;
                phase.store(PHASE_STOPPED, Ordering::SeqCst);
                report_tx.send_replace(Some(report));
                if report.remaining_in_flight > 0 || report.remaining_appends > 0 {
                    tracing::warn!(
                        remaining_in_flight = report.remaining_in_flight,
                        remaining_appends = report.remaining_appends,
                        "router stopped with unfinished work"
                    );
                }
            });
        }

        loop {
            if let Some(report) = *report_rx.borrow() {
                return report;
            }
            if report_rx.changed().await.is_err() {
                // `report_tx` is owned by `self`; the sender cannot drop while
                // `shutdown` is being polled.
                unreachable!("router shutdown report sender dropped while shutdown awaited");
            }
        }
    }

    async fn admit(&self) -> Result<AdmissionGuard<'_>, RouterError> {
        let shutdown = self.admission_notify.notified();
        tokio::pin!(shutdown);
        shutdown.as_mut().enable();
        if self.phase.load(Ordering::SeqCst) != PHASE_READY {
            return Err(RouterError::ShuttingDown);
        }
        let permit = tokio::select! {
            result = self.semaphore.acquire() => {
                result.map_err(|_| RouterError::ShuttingDown)?
            }
            () = &mut shutdown => return Err(RouterError::ShuttingDown),
        };
        self.metrics.in_flight.fetch_add(1, Ordering::SeqCst);
        let guard = AdmissionGuard {
            router: self,
            _permit: permit,
        };
        if self.phase.load(Ordering::SeqCst) != PHASE_READY {
            drop(guard);
            return Err(RouterError::ShuttingDown);
        }
        Ok(guard)
    }

    async fn route_inner(
        &self,
        request: RoutingRequest,
        span: &tracing::Span,
    ) -> Result<RoutedModel, RouterError> {
        let _admission = self.admit().await?;
        if self
            .bound_run
            .is_some_and(|bound| request.run != Some(bound))
        {
            return Err(RouterError::Config("run mismatch".into()));
        }

        let (tier, tier_source) = resolve_tier(&self.config, &request.capability);
        span.record("tier", super::decision_bridge::tier_name(tier));
        if tier_source == TierSource::Default {
            self.metrics
                .routes_default_tier
                .fetch_add(1, Ordering::Relaxed);
        }
        let in_flight = self.metrics.in_flight.load(Ordering::SeqCst);
        let Some(endpoint) = select_endpoint(
            &self.config,
            tier,
            request.requires_tools,
            request.requires_structured_output,
        ) else {
            let decision = route_decision(
                &request,
                tier,
                tier_source,
                &self
                    .config
                    .providers
                    .first()
                    .expect("validated single provider")
                    .id,
                None,
                in_flight,
            );
            self.record_decision(decision).await;
            self.metrics
                .routes_no_endpoint
                .fetch_add(1, Ordering::Relaxed);
            return Err(RouterError::NoEndpoint {
                tier,
                requires_tools: request.requires_tools,
                requires_structured: request.requires_structured_output,
            });
        };
        span.record("endpoint_id", endpoint.id.as_str());

        let (check, counters, budget_source) = self.route_budget(&request);
        if check.is_exhausted() {
            tracing::warn!(
                budget_check = super::decision_bridge::budget_check_name(check),
                capability = %request.capability,
                "model route denied by budget"
            );
            let decision = budget_decision_for_route(
                &request,
                tier,
                tier_source,
                check,
                counters,
                budget_source,
                in_flight,
            );
            self.record_decision(decision).await;
            self.metrics
                .routes_budget_denied
                .fetch_add(1, Ordering::Relaxed);
            return Err(RouterError::BudgetDenied(check));
        }

        let decision = route_decision(
            &request,
            tier,
            tier_source,
            &endpoint.provider,
            Some(&endpoint),
            in_flight,
        );
        let route_event_seq = self.record_decision(decision).await;
        self.metrics.routes_ok.fetch_add(1, Ordering::Relaxed);
        Ok(RoutedModel::mint(
            endpoint,
            tier,
            &request,
            tier_source == TierSource::CapabilityMap,
            route_event_seq,
            self.router_instance_id,
        ))
    }

    async fn complete_inner(
        &self,
        routed: &RoutedModel,
        prompt: PromptPack,
        span: &tracing::Span,
    ) -> Result<ModelResponse, RouterError> {
        let (normalized, receiver) = {
            let _admission = self.admit().await?;
            if routed.router_instance_id() != self.router_instance_id {
                return Err(RouterError::WrongRouter);
            }
            if self.shutdown_token.is_cancelled() {
                return Err(RouterError::Cancelled);
            }
            if !routed.try_consume() {
                return Err(RouterError::AlreadyCompleted);
            }

            if let Some(meter) = &self.cost_meter {
                let (meter_check, snapshot) = meter.check_and_snapshot(&self.budget_policy);
                let check = apply_usd_ceiling_overlay(meter_check, &self.budget_policy);
                if check.is_exhausted() {
                    tracing::warn!(
                        budget_check = super::decision_bridge::budget_check_name(check),
                        capability = %routed.capability(),
                        "model completion denied by budget recheck"
                    );
                    let decision = budget_decision_for_complete(
                        routed,
                        check,
                        BudgetCounters::from(&snapshot),
                        self.metrics.in_flight.load(Ordering::SeqCst),
                    );
                    self.record_decision(decision).await;
                    return Err(RouterError::BudgetDenied(check));
                }
            }

            let request = CompletionRequest {
                messages: prompt.messages.clone(),
                tools: vec![],
                tool_choice: ToolChoice::None,
                response_format: if routed.requires_structured_output() {
                    ResponseFormat::JsonObject
                } else {
                    ResponseFormat::Text
                },
                temperature: None,
                max_output_tokens: None,
            };
            let started = tokio::time::Instant::now();
            let provider_call = self.provider.complete(routed.endpoint(), request);
            tokio::pin!(provider_call);
            let provider_result = tokio::select! {
                biased;
                result = &mut provider_call => result,
                () = self.shutdown_token.cancelled() => return Err(RouterError::Cancelled),
            };
            let duration = started.elapsed();
            span.record(
                "duration_ms",
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            );

            let normalized = match provider_result {
                Ok(mut response) => {
                    response.finish_reason = response
                        .finish_reason
                        .as_deref()
                        .map(|value| redact_and_truncate(value, 128));
                    response.provider_request_id = response
                        .provider_request_id
                        .as_deref()
                        .map(|value| redact_and_truncate(value, 256));
                    Ok(response)
                }
                Err(error) => Err(normalize_provider_error(error)),
            };
            let built =
                build_model_call_record(routed, self.provider.id(), &prompt, duration, &normalized);
            if built.prompt_body_oversize {
                self.metrics
                    .model_call_prompt_body_oversize
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    session = %routed.session(),
                    bytes = built.canonical_len,
                    limit = crate::obs::MODEL_PROMPT_BODY_MAX_BYTES,
                    "prompt body omitted from model-call record"
                );
            }
            if let Some(meter) = &self.cost_meter {
                meter.add_model_usage(
                    routed.tier(),
                    built.input_tokens,
                    built.output_tokens,
                    built.usd,
                );
            }
            let receiver = self
                .decision_log
                .as_ref()
                .map(|log| self.append_supervisor.spawn(Arc::clone(log), built.record));
            (normalized, receiver)
        };

        if let Some(receiver) = receiver {
            match receiver.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(err = %error, "model-call record append failed");
                }
                Err(error) => {
                    self.metrics
                        .obs_record_errors
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(err = %error, "model-call append task ended without result");
                }
            }
        }

        normalized.map_err(RouterError::Provider)
    }

    fn route_budget(
        &self,
        request: &RoutingRequest,
    ) -> (BudgetCheck, BudgetCounters, &'static str) {
        if let Some(meter) = &self.cost_meter {
            let (meter_check, snapshot) = meter.check_and_snapshot(&self.budget_policy);
            (
                apply_usd_ceiling_overlay(meter_check, &self.budget_policy),
                BudgetCounters::from(&snapshot),
                "meter",
            )
        } else {
            (
                apply_usd_ceiling_overlay(
                    check_budget_snapshot(&request.budget_remaining, &self.budget_policy),
                    &self.budget_policy,
                ),
                BudgetCounters::from(&request.budget_remaining),
                "snapshot",
            )
        }
    }

    async fn record_decision(&self, record: DecisionRecord) -> Option<EventSeq> {
        let log = self.decision_log.as_ref()?;
        match log.record(record).await {
            Ok(sequence) => Some(sequence),
            Err(error) => {
                self.metrics
                    .obs_record_errors
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(err = %error, "route decision append failed");
                None
            }
        }
    }
}

#[async_trait]
impl ModelRouter for TomlModelRouter {
    async fn route(&self, request: RoutingRequest) -> Result<RoutedModel, RouterError> {
        let span = tracing::info_span!(
            "alloy.router.route",
            session = %request.session,
            run = ?request.run,
            capability = %request.capability,
            tier = tracing::field::Empty,
            endpoint_id = tracing::field::Empty
        );
        self.route_inner(request, &span)
            .instrument(span.clone())
            .await
    }

    async fn complete(
        &self,
        routed: &RoutedModel,
        prompt: PromptPack,
    ) -> Result<ModelResponse, RouterError> {
        let span = tracing::info_span!(
            "alloy.router.complete",
            session = %routed.session(),
            provider_id = %routed.endpoint().provider,
            endpoint_id = %routed.endpoint().id,
            tier = super::decision_bridge::tier_name(routed.tier()),
            duration_ms = tracing::field::Empty
        );
        let result = self
            .complete_inner(routed, prompt, &span)
            .instrument(span.clone())
            .await;
        match &result {
            Ok(_) => {
                self.metrics.completes_ok.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                // §9.3: every returned complete Err increments completes_err
                // (not future drops — those never reach this match).
                self.metrics.completes_err.fetch_add(1, Ordering::Relaxed);
            }
        }
        result
    }
}

struct AdmissionGuard<'a> {
    router: &'a TomlModelRouter,
    _permit: SemaphorePermit<'a>,
}

impl Drop for AdmissionGuard<'_> {
    fn drop(&mut self) {
        let previous = self.router.metrics.in_flight.fetch_sub(1, Ordering::SeqCst);
        if previous == 1 {
            self.router.in_flight_notify.notify_waiters();
        }
    }
}

struct DurableAppendSupervisor {
    pending: AtomicUsize,
    done_notify: Notify,
    obs_record_errors: Arc<AtomicU64>,
}

impl DurableAppendSupervisor {
    fn spawn(
        self: &Arc<Self>,
        log: Arc<dyn DecisionLog>,
        record: ModelCallRecord,
    ) -> oneshot::Receiver<Result<(), ObsError>> {
        let (sender, receiver) = oneshot::channel();
        self.pending.fetch_add(1, Ordering::SeqCst);
        let supervisor = Arc::clone(self);
        let pending_guard = guard((), {
            let cleanup_supervisor = Arc::clone(&supervisor);
            move |()| cleanup_supervisor.finish_one()
        });
        tokio::spawn(async move {
            let result = log.record_model_call(record).await.map(|_| ());
            if result.is_err() {
                supervisor.obs_record_errors.fetch_add(1, Ordering::Relaxed);
            }
            let _ = sender.send(result);
            drop(pending_guard);
        });
        receiver
    }

    fn finish_one(&self) {
        self.pending.fetch_sub(1, Ordering::SeqCst);
        self.done_notify.notify_waiters();
    }

    async fn drain_aggregate(&self, budget: Duration) -> usize {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let wait = self.done_notify.notified();
            tokio::pin!(wait);
            wait.as_mut().enable();
            let left = self.pending.load(Ordering::SeqCst);
            if left == 0 {
                return 0;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                tracing::warn!(
                    remaining_appends = left,
                    "model-call append drain timed out"
                );
                return left;
            }
            if tokio::time::timeout(remaining, &mut wait).await.is_err() {
                let pending = self.pending.load(Ordering::SeqCst);
                if pending > 0 {
                    tracing::warn!(
                        remaining_appends = pending,
                        "model-call append drain timed out"
                    );
                }
                return pending;
            }
        }
    }
}

async fn drain_shutdown(
    grace: Duration,
    metrics: &RouterMetrics,
    in_flight_notify: &Notify,
    shutdown_token: &CancellationToken,
    append_supervisor: &DurableAppendSupervisor,
) -> RouterShutdownReport {
    wait_for_zero(&metrics.in_flight, in_flight_notify, grace).await;

    let mut cancelled = false;
    let mut append_budget = grace;
    if metrics.in_flight.load(Ordering::SeqCst) > 0 {
        shutdown_token.cancel();
        cancelled = true;
        let post_cancel = grace.min(POST_CANCEL_MAX);
        wait_for_zero(&metrics.in_flight, in_flight_notify, post_cancel).await;
        append_budget = post_cancel;
    }

    RouterShutdownReport {
        cancelled_in_flight: cancelled,
        remaining_in_flight: metrics.in_flight.load(Ordering::SeqCst),
        remaining_appends: append_supervisor.drain_aggregate(append_budget).await,
    }
}

async fn wait_for_zero(counter: &AtomicUsize, notify: &Notify, duration: Duration) {
    let wait = notify.notified();
    tokio::pin!(wait);
    wait.as_mut().enable();
    if counter.load(Ordering::SeqCst) != 0 {
        let _ = tokio::time::timeout(duration, wait).await;
    }
}

fn validate_price_completeness(
    config: &RouterConfig,
    budget_policy: &BudgetPolicy,
) -> Result<(), RouterError> {
    if budget_policy.max_usd_per_run.is_finite() && budget_policy.max_usd_per_run > 0.0 {
        for endpoint in config
            .providers
            .iter()
            .flat_map(|provider| &provider.endpoints)
        {
            if endpoint.input_usd_per_mtok.is_none() || endpoint.output_usd_per_mtok.is_none() {
                return Err(RouterError::Config(format!(
                    "endpoint {} requires input and output prices under a finite USD budget",
                    endpoint.id
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::{RecordingDecisionLog, RetentionPolicy};
    use crate::router::{RecordingModelProvider, Usage};
    use crate::types::budget::BudgetSnapshot;
    use crate::types::ids::{CapabilityId, ProviderId, SessionId};

    fn config() -> RouterConfig {
        RouterConfig::from_str(
            "test",
            r#"
[policy]
default_tier = "standard"
shutdown_grace_ms = 10

[[providers]]
id = "provider"
kind = "openai_compatible"
base_url = "https://example.com"
api_key_env = "KEY"

[[providers.endpoints]]
id = "endpoint"
display_name = "Endpoint"
model = "configured"
tiers = ["standard"]
max_context = 1
input_usd_per_mtok = 1.0
output_usd_per_mtok = 1.0
"#,
        )
        .unwrap()
    }

    fn request(run: RunId) -> RoutingRequest {
        RoutingRequest {
            session: SessionId::new(),
            run: Some(run),
            node: None,
            capability: CapabilityId::new("repair").unwrap(),
            complexity: None,
            budget_remaining: BudgetSnapshot {
                usd_spent: 0.0,
                tokens_in: 0,
                tokens_out: 0,
            },
            requires_tools: false,
            requires_structured_output: false,
        }
    }

    #[tokio::test]
    async fn route_complete_records_and_meters_once() {
        let id = ProviderId::new("provider").unwrap();
        let provider = Arc::new(RecordingModelProvider::new(id));
        provider.push(Ok(ModelResponse {
            text: Some("done".into()),
            structured: None,
            tool_calls: vec![],
            usage: Usage {
                input_tokens: Some(10),
                output_tokens: Some(5),
            },
            provider_request_id: None,
            finish_reason: Some("stop".into()),
        }));
        let log = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
        let meter = SharedCostMeter::new();
        let run = RunId::new();
        let router = TomlModelRouter::from_parts(TomlModelRouterParts::new(
            config(),
            provider.clone(),
            BudgetPolicy::default(),
            Some(log.clone()),
            Some(meter.clone()),
            Some(run),
        ))
        .unwrap();
        let routed = router.route(request(run)).await.unwrap();
        let clone = routed.clone();
        router
            .complete(
                &routed,
                PromptPack {
                    messages: vec![],
                    citations: vec![],
                    domains: None,
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            router
                .complete(
                    &clone,
                    PromptPack {
                        messages: vec![],
                        citations: vec![],
                        domains: None,
                    }
                )
                .await,
            Err(RouterError::AlreadyCompleted)
        ));
        assert_eq!(meter.snapshot().model_calls, 1);
        assert_eq!(log.recorded_model_calls().len(), 1);
        assert_eq!(provider.recorded().len(), 1);
    }

    #[tokio::test]
    async fn zero_usd_overlay_and_shutdown_are_fail_closed() {
        let id = ProviderId::new("provider").unwrap();
        let provider = Arc::new(RecordingModelProvider::new(id));
        let run = RunId::new();
        let router = TomlModelRouter::from_parts(
            TomlModelRouterParts::new(
                config(),
                provider,
                BudgetPolicy {
                    max_usd_per_run: 0.0,
                    ..BudgetPolicy::default()
                },
                None,
                None,
                Some(run),
            )
            .allow_unmetered(),
        )
        .unwrap();
        assert!(matches!(
            router.route(request(run)).await,
            Err(RouterError::BudgetDenied(BudgetCheck::UsdExhausted))
        ));
        let first = router.shutdown().await;
        let second = router.shutdown().await;
        assert_eq!(first, second);
        assert!(matches!(
            router.route(request(run)).await,
            Err(RouterError::ShuttingDown)
        ));
    }

    #[test]
    fn production_dependencies_and_prices_are_required() {
        let provider = Arc::new(RecordingModelProvider::new(
            ProviderId::new("provider").unwrap(),
        ));
        let result = TomlModelRouter::from_parts(TomlModelRouterParts::new(
            config(),
            provider,
            BudgetPolicy::default(),
            None,
            None,
            None,
        ));
        assert!(matches!(result, Err(RouterError::Config(_))));

        let mut without_prices = config();
        without_prices.providers[0].endpoints[0].input_usd_per_mtok = None;
        assert!(validate_price_completeness(&without_prices, &BudgetPolicy::default()).is_err());
    }
}
