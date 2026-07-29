//! The MVP workers (RFC-0013 §9) and their shared attempt scaffolding:
//! turn/tool accounting, the PS6 exchange loop, failure mapping (§12), and
//! the OB2 span / OB3 `worker_attempt` decision record.

mod edit;
mod planning;
mod repair;
mod review;

pub use edit::EditWorker;
pub use planning::PlanningWorker;
pub use repair::RepairWorker;
pub use review::ReviewWorker;

use serde_json::Value;

use crate::adapters::{CapabilityExecError, CapabilityOutcome, ToolCallerError};
use crate::context::{AssembleInputs, AssembleRequest};
use crate::dag::{NodeInputPayload, NodeOutputEnvelope};
use crate::obs::{truncate_utf8_bytes, DecisionKind, DecisionRecord};
use crate::router::{
    classify_router_error, Citation, ModelResponse, PromptPack, RouterError, RoutingRequest,
};
use crate::types::budget::ModelTier;
use crate::types::diagnostic::{DiagnosticEvent, ErrorClass, FailureIr, RetryDisposition};
use crate::types::ids::{Digest, NodeId, ProviderId};
use crate::types::metrics::WorkerMetrics;
use crate::types::tools::{ToolCall, ToolName, ToolResult};

use super::deps::{CapabilityContext, WorkerConfig};
use super::parse::{extract_json, ExtractError, JsonSource};
use super::perms::WorkerToolClass;
use super::prompt::{fence_tool, with_notes, with_system_instruction};
use super::traits::CapabilityDescriptor;

/// FM15: max bytes of a `FailureIr.notes` string.
const MAX_FAILURE_NOTE_BYTES: usize = 2 * 1024;

/// Internal worker failure plumbing (§12).
#[derive(Debug)]
pub(crate) enum WorkerError {
    /// Structured soft failure → `CapabilityOutcome::Failed` (CW2).
    Soft {
        /// Failure class (FM16: the most accurate available class).
        class: ErrorClass,
        /// Admission disposition.
        retry: RetryDisposition,
        /// Redacted, bounded note (FM15).
        notes: String,
        /// Diagnostics the worker was reasoning about (FM14).
        diagnostics: Vec<DiagnosticEvent>,
    },
    /// Host-boundary fault → `Err(CapabilityExecError)` (CW2).
    Host(CapabilityExecError),
}

impl WorkerError {
    pub(crate) fn soft(
        class: ErrorClass,
        retry: RetryDisposition,
        notes: impl Into<String>,
    ) -> Self {
        Self::Soft {
            class,
            retry,
            notes: notes.into(),
            diagnostics: Vec::new(),
        }
    }

    pub(crate) fn cancelled() -> Self {
        Self::Host(CapabilityExecError::Cancelled)
    }
}

/// A worker's successful attempt body: the serialized payload plus the
/// confidence recorded in the decision metadata.
pub(crate) struct WorkerSuccess {
    pub payload: Value,
    pub confidence: f32,
}

/// Per-attempt accounting shared by all workers (BG7, OB3).
pub(crate) struct Attempt {
    pub model_turns: u8,
    pub tool_calls: u32,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub provider: Option<ProviderId>,
    pub tier_override: bool,
    pub structured_fallback: bool,
    pub json_source: Option<JsonSource>,
    pub raw_digest: Option<Digest>,
    pub citations: Vec<Citation>,
    pub system_prompt_digest: Option<Digest>,
}

impl Attempt {
    pub(crate) fn new(preferred: ModelTier, effective: ModelTier) -> Self {
        Self {
            model_turns: 0,
            tool_calls: 0,
            tokens_in: None,
            tokens_out: None,
            provider: None,
            // MR2: escalation is recorded, never overridden.
            tier_override: preferred != effective,
            structured_fallback: false,
            json_source: None,
            raw_digest: None,
            citations: Vec::new(),
            system_prompt_digest: None,
        }
    }

    fn add_tokens(&mut self, input: Option<u64>, output: Option<u64>) {
        if let Some(n) = input {
            self.tokens_in = Some(self.tokens_in.unwrap_or(0).saturating_add(n));
        }
        if let Some(n) = output {
            self.tokens_out = Some(self.tokens_out.unwrap_or(0).saturating_add(n));
        }
    }

    /// BG7/BG8. `WorkerMetrics.confidence` stays `None`: RFC-0007 providers
    /// never report one, and a model's self-reported value is not a provider
    /// value (OC2).
    pub(crate) fn metrics(
        &self,
        ctx: &CapabilityContext<'_>,
        error_class: Option<ErrorClass>,
    ) -> WorkerMetrics {
        WorkerMetrics {
            model_tier_used: ctx.effective_tier,
            provider_id: self
                .provider
                .clone()
                .unwrap_or_else(|| ProviderId::new("unrouted").expect("static id")),
            input_tokens: self.tokens_in,
            output_tokens: self.tokens_out,
            tool_calls: self.tool_calls,
            cache_hits: 0, // no worker-level cache in MVP (BG7).
            duration_ms: u64::try_from(ctx.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            confidence: None,
            error_class,
        }
    }
}

/// OB2 span for one attempt.
pub(crate) fn worker_span(ctx: &CapabilityContext<'_>) -> tracing::Span {
    tracing::info_span!(
        "worker.execute",
        capability = %ctx.capability,
        kind = ?ctx.kind,
        node = %ctx.node,
        attempt = ctx.attempt,
        tier = ?ctx.effective_tier,
        model_turns = tracing::field::Empty,
        tool_calls = tracing::field::Empty,
        outcome = tracing::field::Empty,
        error_class = tracing::field::Empty,
    )
}

/// Close out one attempt: record OB2 span fields, append the single OB3
/// `worker_attempt` decision record, and map the result into the merged
/// outcome shape (CW2).
pub(crate) async fn finish_attempt(
    ctx: &CapabilityContext<'_>,
    descriptor: &CapabilityDescriptor,
    attempt: &Attempt,
    result: Result<WorkerSuccess, WorkerError>,
    span: &tracing::Span,
) -> Result<CapabilityOutcome, CapabilityExecError> {
    span.record("model_turns", attempt.model_turns);
    span.record("tool_calls", attempt.tool_calls);

    // Host-boundary fault (registry, cancellation, invariant break): no
    // decision record — the attempt did not complete as a worker attempt
    // (OB3 covers success and soft failure).
    let result = match result {
        Err(WorkerError::Host(e)) => {
            span.record("outcome", "host_error");
            return Err(e);
        }
        other => other,
    };
    let (outcome_label, error_class, confidence) = match &result {
        Ok(success) => ("succeeded", None, Some(success.confidence)),
        Err(WorkerError::Soft { class, .. }) => ("failed", Some(*class), None),
        Err(WorkerError::Host(_)) => unreachable!("host errors returned above"),
    };
    span.record("outcome", outcome_label);
    if let Some(class) = error_class {
        span.record("error_class", format!("{class:?}"));
    }

    let metadata = serde_json::json!({
        "capability": descriptor.id.as_str(),
        "capability_version": descriptor.version.to_string(),
        "kind": format!("{:?}", ctx.kind),
        "attempt": ctx.attempt,
        "tier": ctx.effective_tier,
        "tier_override": attempt.tier_override,
        "system_prompt_digest": attempt.system_prompt_digest,
        "citations": attempt
            .citations
            .iter()
            .map(|c| serde_json::json!({ "source": c.source, "digest": c.digest }))
            .collect::<Vec<_>>(),
        "json_source": attempt.json_source.map(JsonSource::label),
        "structured_fallback": attempt.structured_fallback,
        "model_turns": attempt.model_turns,
        "tool_calls": attempt.tool_calls,
        "outcome": outcome_label,
        "error_class": error_class.map(|c| format!("{c:?}")),
        "confidence": confidence,
    });
    let record = DecisionRecord {
        session: ctx.session,
        run: Some(ctx.run),
        node: Some(ctx.node),
        kind: DecisionKind::Custom("worker_attempt".into()),
        metadata,
        // OB4: digest of the raw model response body of the final turn.
        content_hash: attempt.raw_digest.clone(),
        // The router owns prompt retention; worker records never carry one.
        prompt_body: None,
    };
    if let Err(e) = ctx.decisions.record(record).await {
        // Observability must not turn a finished attempt into a failure; the
        // event log is the durable channel and its own failures are logged.
        tracing::warn!(error = %e, "worker_attempt decision record failed");
    }

    match result {
        Ok(success) => Ok(CapabilityOutcome::Succeeded {
            payload: success.payload,
        }),
        Err(WorkerError::Soft {
            class,
            retry,
            notes,
            diagnostics,
        }) => Ok(CapabilityOutcome::Failed {
            failure: FailureIr {
                // FM13: placeholder; RFC-0010 CE2 overwrites it.
                node: NodeId::new(),
                error_class: class,
                retry,
                diagnostics,
                notes: truncate_utf8_bytes(
                    &crate::obs::redact_secrets(&notes),
                    MAX_FAILURE_NOTE_BYTES,
                ),
            },
        }),
        Err(WorkerError::Host(_)) => unreachable!("host errors returned above"),
    }
}

/// One routed completion (MR1–MR6, PR9/PR10, BG4/BG5, CW4).
pub(crate) async fn route_and_complete(
    ctx: &CapabilityContext<'_>,
    attempt: &mut Attempt,
    pack: PromptPack,
) -> Result<ModelResponse, WorkerError> {
    // CW4 before each model call.
    if ctx.is_cancelled() {
        return Err(WorkerError::cancelled());
    }
    // BG5 / MR5.
    if ctx.remaining().is_zero() {
        return Err(WorkerError::soft(
            ErrorClass::Timeout,
            RetryDisposition::Retryable,
            "node deadline reached before completion",
        ));
    }

    let request = |structured: bool| RoutingRequest {
        session: ctx.session,
        run: Some(ctx.run),
        node: Some(ctx.node),
        capability: ctx.capability.clone(),
        complexity: None,
        budget_remaining: ctx.cost_meter.to_budget_snapshot(),
        requires_tools: false, // provider-native tool calling is deferred (§1.4).
        requires_structured_output: structured,
    };

    // PR9: structured-output-first; PR10: one fallback on a structured-only
    // NoEndpoint miss.
    let routed = match ctx.router.route(request(true)).await {
        Ok(routed) => routed,
        Err(RouterError::NoEndpoint {
            requires_structured: true,
            ..
        }) => {
            attempt.structured_fallback = true;
            ctx.router
                .route(request(false))
                .await
                .map_err(|e| map_router_error(&e))?
        }
        Err(e) => return Err(map_router_error(&e)),
    };

    let response = ctx
        .router
        .complete(&routed, pack)
        .await
        .map_err(|e| map_router_error(&e))?;

    // MR4: the routed handle is single-use; the next turn routes again.
    attempt.model_turns = attempt.model_turns.saturating_add(1);
    attempt.provider = Some(routed.endpoint().provider.clone());
    attempt.add_tokens(response.usage.input_tokens, response.usage.output_tokens);
    Ok(response)
}

/// FM1: `classify_router_error` is the only mapping; never re-derived.
fn map_router_error(err: &RouterError) -> WorkerError {
    if matches!(err, RouterError::Cancelled) {
        return WorkerError::cancelled();
    }
    let classified = classify_router_error(err);
    let notes = match err {
        // BG4: name the exhausted ceiling.
        RouterError::BudgetDenied(check) => format!("budget denied: {check:?}"),
        other => format!("router: {other}"),
    };
    WorkerError::Soft {
        class: classified.class,
        retry: classified.retry,
        notes,
        diagnostics: Vec::new(),
    }
}

/// One authorized tool call (TL1–TL6, PM1/PM5/PM6, FM2).
///
/// `allowed` is the worker's own `required_tools()` name set; a call outside
/// it is an internal invariant break (TL5, T12).
pub(crate) async fn call_tool(
    ctx: &CapabilityContext<'_>,
    attempt: &mut Attempt,
    config: &WorkerConfig,
    class: WorkerToolClass,
    allowed: &[&str],
    name: &str,
    arguments: Value,
) -> Result<ToolResult, WorkerError> {
    // TL5.
    ensure_tool_allowed(name, allowed)?;
    // CW4 before each tool call.
    if ctx.is_cancelled() {
        return Err(WorkerError::cancelled());
    }
    // BG5.
    if ctx.remaining().is_zero() {
        return Err(WorkerError::soft(
            ErrorClass::Timeout,
            RetryDisposition::Retryable,
            format!("node deadline reached before {name}"),
        ));
    }
    // TL4/CW5 hard stop.
    if attempt.tool_calls >= u32::from(config.max_tool_calls) {
        return Err(WorkerError::soft(
            ErrorClass::Internal,
            RetryDisposition::NonRetryable,
            "max_tool_calls ceiling reached",
        ));
    }

    // PM1/PM5: minted per call, never cached.
    let token = ctx
        .perms
        .token_for(&ctx.exec_ref(), class)
        .await
        .map_err(|e| match e {
            crate::error::AdapterError::PermissionDenied(msg) => WorkerError::soft(
                ErrorClass::Tool,
                RetryDisposition::NonRetryable,
                format!("permission denied: {msg}"),
            ),
            other => WorkerError::Host(CapabilityExecError::Internal(format!(
                "permission minting failed: {other}"
            ))),
        })?;

    let tool_name = ToolName::new(name)
        .map_err(|_| WorkerError::Host(CapabilityExecError::Internal("bad tool name".into())))?;
    // TL2: attribution plus a `{node}:{attempt}:{seq}` call id.
    let seq = attempt.tool_calls;
    let call = ToolCall::new(tool_name, arguments)
        .with_call_id(format!("{}:{}:{seq}", ctx.node, ctx.attempt))
        .with_attribution(Some(ctx.session), Some(ctx.run), Some(ctx.node));

    let result = ctx.tools.call(call, token).await;
    attempt.tool_calls = attempt.tool_calls.saturating_add(1);
    result.map_err(|e| map_tool_caller_error(&e))
}

/// TL5: a worker may only call tools it declared in `required_tools()`; a
/// call outside that set is an internal invariant break (T12).
fn ensure_tool_allowed(name: &str, allowed: &[&str]) -> Result<(), WorkerError> {
    if allowed.contains(&name) {
        Ok(())
    } else {
        Err(WorkerError::Host(CapabilityExecError::Internal(format!(
            "tool {name} outside required_tools"
        ))))
    }
}

/// FM2 mapping for host-boundary tool failures.
fn map_tool_caller_error(err: &ToolCallerError) -> WorkerError {
    match err {
        ToolCallerError::Cancelled => WorkerError::cancelled(),
        ToolCallerError::Timeout => WorkerError::soft(
            ErrorClass::Timeout,
            RetryDisposition::Retryable,
            "tool timeout",
        ),
        ToolCallerError::PermissionDenied(_)
        | ToolCallerError::TokenExpired
        | ToolCallerError::InvalidToken(_) => WorkerError::soft(
            ErrorClass::Tool,
            RetryDisposition::NonRetryable,
            format!("tool denied: {err}"),
        ),
        ToolCallerError::Sandbox(_) => WorkerError::soft(
            ErrorClass::Tool,
            RetryDisposition::Retryable,
            format!("tool sandbox: {err}"),
        ),
        ToolCallerError::UnknownTool(_)
        | ToolCallerError::InvalidArguments(_)
        | ToolCallerError::Unsupported(_)
        | ToolCallerError::ShuttingDown
        | ToolCallerError::Internal(_) => WorkerError::soft(
            ErrorClass::Internal,
            RetryDisposition::NonRetryable,
            format!("tool host: {err}"),
        ),
    }
}

/// FM3 mapping for tool-level (`ToolResult::is_error`) failures.
pub(crate) fn map_tool_result_error(result: &ToolResult) -> WorkerError {
    use crate::types::tools::ToolError;
    let (class, retry) = match result.error() {
        Some(ToolError::Transient { .. } | ToolError::ExecutionFailed { .. }) => {
            (ErrorClass::Tool, RetryDisposition::Retryable)
        }
        Some(ToolError::Permanent { .. } | ToolError::InvalidArgs { .. }) | None => {
            (ErrorClass::Tool, RetryDisposition::NonRetryable)
        }
    };
    let notes = result
        .error()
        .map_or_else(|| "tool error".to_owned(), ToolError::to_string);
    WorkerError::soft(class, retry, format!("{}: {notes}", result.name))
}

/// The PS1–PS6 exchange: assemble → prepend the owned instruction → append
/// fenced notes → route → complete → extract → validate, with at most one
/// in-worker parse-repair turn (PS6, Q2: the repair turn counts against
/// `max_model_turns`).
///
/// `validate` maps the extracted object into the worker's typed proposal; an
/// `Err(reason)` is a PS5 schema violation.
pub(crate) async fn llm_exchange<T>(
    ctx: &CapabilityContext<'_>,
    attempt: &mut Attempt,
    config: &WorkerConfig,
    system_instruction: &'static str,
    inputs: &AssembleInputs,
    feedback: &[String],
    validate: impl Fn(&Value) -> Result<T, String>,
) -> Result<(T, PromptPack), WorkerError> {
    attempt.system_prompt_digest =
        Some(super::prompt::system_instruction_digest(system_instruction));
    let mut repair_notes: Vec<String> = Vec::new();
    let mut repaired = false;

    loop {
        // PR7: never resend a prompt that was not just assembled.
        let req = AssembleRequest {
            session: ctx.session,
            node: ctx.node,
            capability: ctx.capability.clone(),
            // PR2/BG3: the caller's ceiling is the node input budget.
            token_budget: usize::try_from(ctx.budget.max_input).unwrap_or(usize::MAX),
            must_include: vec![],
        };
        let pack = ctx
            .context
            .assemble_with(req, inputs.clone())
            .await
            .map_err(|e| {
                WorkerError::soft(
                    ErrorClass::Internal,
                    RetryDisposition::NonRetryable,
                    format!("context assembly failed: {e}"),
                )
            })?;
        let pack = with_system_instruction(pack, system_instruction);
        // Caller-supplied fenced feedback (a failed dry run, EW6) plus the
        // PS6 validator note, both User-role (PR11).
        let mut notes: Vec<String> = feedback.to_vec();
        notes.extend(repair_notes.iter().cloned());
        let pack = with_notes(pack, &notes);
        // PR4/OC4: the final turn's citations flow through unmodified.
        attempt.citations = pack.citations.clone();

        let response = route_and_complete(ctx, attempt, pack.clone()).await?;

        let violation = match extract_json(&response) {
            Ok(extracted) => {
                attempt.json_source = Some(extracted.source);
                attempt.raw_digest = Some(extracted.raw_digest.clone());
                match validate(&extracted.value) {
                    Ok(value) => return Ok((value, pack)),
                    Err(reason) => reason,
                }
            }
            Err(ExtractError::Refusal) => {
                // PS7/FM5.
                return Err(WorkerError::soft(
                    ErrorClass::Model,
                    RetryDisposition::NonRetryable,
                    "model refused",
                ));
            }
            Err(ExtractError::Truncated) => {
                // PS8/FM6.
                return Err(WorkerError::soft(
                    ErrorClass::Model,
                    RetryDisposition::Retryable,
                    "output truncated",
                ));
            }
            Err(ExtractError::Unparseable(reason)) => reason,
        };

        // PS6: at most one repair turn, then Model/Retryable (FM4).
        if !repaired && attempt.model_turns < config.max_model_turns {
            repaired = true;
            repair_notes = vec![fence_tool(
                "validator",
                &format!(
                    "The previous reply was invalid: {violation}. Reply with exactly one \
                     JSON object matching the schema in the system instruction."
                ),
                config.max_tool_result_bytes,
            )];
            continue;
        }
        return Err(WorkerError::soft(
            ErrorClass::Model,
            RetryDisposition::Retryable,
            format!("invalid model response after repair turn: {violation}"),
        ));
    }
}

/// Decode predecessor output artifacts (RW1/EW2). Returns
/// `(pred kind, decoded payload)` pairs; envelopes are unwrapped to their
/// inner payload. A pred artifact that fails to load or decode is FM10.
pub(crate) async fn load_pred_payloads(
    ctx: &CapabilityContext<'_>,
) -> Result<Vec<(crate::dag::NodeKind, Value)>, WorkerError> {
    let NodeInputPayload::FromPredecessors { preds } = &ctx.input.payload else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(preds.len());
    for pred in preds {
        let blob = ctx.artifacts.get(pred.output_ref).await.map_err(|e| {
            WorkerError::soft(
                ErrorClass::Internal,
                RetryDisposition::NonRetryable,
                format!("predecessor artifact load failed: {e}"),
            )
        })?;
        let value: Value = serde_json::from_slice(&blob.bytes).map_err(|e| {
            WorkerError::soft(
                ErrorClass::Internal,
                RetryDisposition::NonRetryable,
                format!("predecessor artifact is not JSON: {e}"),
            )
        })?;
        // Success outputs are wrapped in a `NodeOutputEnvelope`; synthetic
        // preds (a replanned root carrying a failure body) are bare.
        let payload = match serde_json::from_value::<NodeOutputEnvelope>(value.clone()) {
            Ok(env) if env.is_supported_schema() => env.payload,
            _ => value,
        };
        out.push((pred.kind, payload));
    }
    Ok(out)
}

/// Collect `DiagnosticEvent`s from any predecessor payload that carries a
/// `diagnostics` array (a `FailureIr` body or a synthetic replan pred).
pub(crate) fn diagnostics_from_payloads(
    payloads: &[(crate::dag::NodeKind, Value)],
) -> Vec<DiagnosticEvent> {
    let mut out = Vec::new();
    for (_, payload) in payloads {
        if let Some(list) = payload.get("diagnostics").and_then(Value::as_array) {
            for item in list {
                if let Ok(d) = serde_json::from_value::<DiagnosticEvent>(item.clone()) {
                    out.push(d);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::types::tools::ToolError;

    use super::*;

    fn soft(err: WorkerError) -> (ErrorClass, RetryDisposition) {
        match err {
            WorkerError::Soft { class, retry, .. } => (class, retry),
            WorkerError::Host(e) => panic!("expected soft failure, got host error {e:?}"),
        }
    }

    #[test]
    fn worker_tool_call_outside_required_tools_is_internal() {
        // T12 / TL5.
        let err = ensure_tool_allowed("apply_patch", &["fs_read"]).unwrap_err();
        assert!(matches!(
            err,
            WorkerError::Host(CapabilityExecError::Internal(_))
        ));
        assert!(ensure_tool_allowed("fs_read", &["fs_read"]).is_ok());
    }

    #[test]
    fn failure_mapping_table_is_total() {
        // §12: one case per FM row that maps inside the workers.
        // FM1 via classify_router_error.
        let (class, retry) = soft(map_router_error(&RouterError::Provider(
            crate::router::ProviderError::RateLimit,
        )));
        assert_eq!(
            (class, retry),
            (ErrorClass::Model, RetryDisposition::Retryable)
        );
        // FM8/BG4.
        let (class, retry) = soft(map_router_error(&RouterError::BudgetDenied(
            crate::obs::BudgetCheck::UsdExhausted,
        )));
        assert_eq!(
            (class, retry),
            (ErrorClass::Budget, RetryDisposition::NonRetryable)
        );
        // FM12: router-level cancellation is a host error, not a soft one.
        assert!(matches!(
            map_router_error(&RouterError::Cancelled),
            WorkerError::Host(CapabilityExecError::Cancelled)
        ));

        // FM2.
        let cases: &[(ToolCallerError, ErrorClass, RetryDisposition)] = &[
            (
                ToolCallerError::UnknownTool("x".into()),
                ErrorClass::Internal,
                RetryDisposition::NonRetryable,
            ),
            (
                ToolCallerError::InvalidArguments("x".into()),
                ErrorClass::Internal,
                RetryDisposition::NonRetryable,
            ),
            (
                ToolCallerError::Internal("x".into()),
                ErrorClass::Internal,
                RetryDisposition::NonRetryable,
            ),
            (
                ToolCallerError::PermissionDenied("x".into()),
                ErrorClass::Tool,
                RetryDisposition::NonRetryable,
            ),
            (
                ToolCallerError::TokenExpired,
                ErrorClass::Tool,
                RetryDisposition::NonRetryable,
            ),
            (
                ToolCallerError::InvalidToken("x".into()),
                ErrorClass::Tool,
                RetryDisposition::NonRetryable,
            ),
            (
                ToolCallerError::Unsupported("x".into()),
                ErrorClass::Internal,
                RetryDisposition::NonRetryable,
            ),
            (
                ToolCallerError::ShuttingDown,
                ErrorClass::Internal,
                RetryDisposition::NonRetryable,
            ),
            (
                ToolCallerError::Timeout,
                ErrorClass::Timeout,
                RetryDisposition::Retryable,
            ),
            (
                ToolCallerError::Sandbox("x".into()),
                ErrorClass::Tool,
                RetryDisposition::Retryable,
            ),
        ];
        for (err, want_class, want_retry) in cases {
            let (class, retry) = soft(map_tool_caller_error(err));
            assert_eq!((class, retry), (*want_class, *want_retry), "{err:?}");
        }
        assert!(matches!(
            map_tool_caller_error(&ToolCallerError::Cancelled),
            WorkerError::Host(CapabilityExecError::Cancelled)
        ));

        // FM3.
        let name = ToolName::new("apply_patch").unwrap();
        let result_cases: &[(ToolError, ErrorClass, RetryDisposition)] = &[
            (
                ToolError::Transient {
                    code: "io".into(),
                    message: "x".into(),
                },
                ErrorClass::Tool,
                RetryDisposition::Retryable,
            ),
            (
                ToolError::ExecutionFailed {
                    exit_code: Some(1),
                    signal: None,
                    message: "x".into(),
                },
                ErrorClass::Tool,
                RetryDisposition::Retryable,
            ),
            (
                ToolError::Permanent {
                    code: "conflict".into(),
                    message: "x".into(),
                },
                ErrorClass::Tool,
                RetryDisposition::NonRetryable,
            ),
            (
                ToolError::InvalidArgs {
                    message: "x".into(),
                },
                ErrorClass::Tool,
                RetryDisposition::NonRetryable,
            ),
        ];
        for (tool_err, want_class, want_retry) in result_cases {
            let result =
                crate::types::tools::ToolResult::err(name.clone(), json!({}), tool_err.clone(), 1);
            let (class, retry) = soft(map_tool_result_error(&result));
            assert_eq!((class, retry), (*want_class, *want_retry), "{tool_err:?}");
        }
    }

    #[test]
    fn diagnostics_are_collected_from_failure_shaped_payloads() {
        // RW1 helper: any pred payload with a `diagnostics` array
        // contributes.
        let diag = crate::types::diagnostic::DiagnosticEvent {
            id: crate::types::ids::DiagnosticId::new(),
            code: Some("E0502".into()),
            level: crate::types::diagnostic::DiagnosticLevel::Error,
            message: "borrow".into(),
            spans: vec![],
            children: vec![],
            package: None,
            fingerprint: Digest::sha256(b"d"),
            raw_json: None,
        };
        let payloads = vec![
            (
                crate::dag::NodeKind::Analyze,
                json!({ "diagnostics": [diag], "notes": "n" }),
            ),
            (crate::dag::NodeKind::Analyze, json!({ "other": 1 })),
        ];
        let out = diagnostics_from_payloads(&payloads);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].code.as_deref(), Some("E0502"));
    }
}
