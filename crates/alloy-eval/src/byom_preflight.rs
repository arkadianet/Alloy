//! Exact-contract BYOM preflight: prove a bring-your-own-model endpoint can
//! serve **both** arms' wire contracts before any fixture attempt runs.
//!
//! E1 shipped a comparison whose two arms did not share a wire contract. The
//! naive arm hardcoded `response_format: json_schema`; the Alloy arm's
//! rendered `router.toml` omitted `supports_json_schema`, which serde-defaults
//! to `false`, so the router degraded that arm to `json_object` behind a debug
//! log. Nothing in the evidence said so. This module makes that class of
//! asymmetry a hard, machine-readable preflight failure:
//!
//! * both contracts are derived from the code the arms actually run —
//!   [`crate::build_naive_request`] for the naive arm and a real
//!   [`alloy_runtime::RouterConfig`] parse of the rendered `router.toml` for
//!   the Alloy arm, so a config that would degrade is detected as a degrade;
//! * both are exercised against the live endpoint;
//! * everything observed is persisted as JSON — the mode actually put on the
//!   wire, the schema digest, duration, **nullable** usage, HTTP status,
//!   error class, and the timeout stage when a deadline expired;
//! * any failure, on either contract, is fail-closed: the caller gets a
//!   non-zero exit and a stable reason code and can abort the matrix before
//!   scoring.
//!
//! Credentials come from the process environment or an explicit value handed
//! in by the caller. This module never touches dotenv files, and the
//! offline-CI guard enforces that by scanning for the literal name.

use std::time::Duration;

use alloy_runtime::{
    classify_provider_error, repair_response_schema, ChatMessage, ChatRole, CompletionRequest,
    Digest, EndpointId, JsonSchemaSpec, ModelEndpoint, ModelProvider, ModelResponse, ModelTier,
    OpenAiCompatibleProvider, OpenAiCompatibleSpec, ProviderError, ProviderId, ResponseFormat,
    RouterConfig, SecretString, ToolChoice,
};
use serde::{Deserialize, Serialize};

use crate::error::EvalError;
use crate::live_naive::build_naive_request;

/// Schema version of the persisted preflight document.
pub const PREFLIGHT_SCHEMA_VERSION: u32 = 1;

/// Endpoint id used for both probes; never reaches the wire.
const PROBE_ENDPOINT_ID: &str = "byom-preflight";

/// `response_format` shape a request actually carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireMode {
    /// No `response_format` field at all.
    Text,
    /// `{"type":"json_object"}`.
    JsonObject,
    /// `{"type":"json_schema","json_schema":{...}}`.
    JsonSchema,
}

impl WireMode {
    /// Stable lowercase token for report JSON and reason detail strings.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::JsonObject => "json_object",
            Self::JsonSchema => "json_schema",
        }
    }

    fn of(format: &ResponseFormat) -> Self {
        match format {
            ResponseFormat::Text => Self::Text,
            ResponseFormat::JsonObject => Self::JsonObject,
            ResponseFormat::JsonSchema { .. } => Self::JsonSchema,
        }
    }
}

impl std::fmt::Display for WireMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which arm's contract a probe exercises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractArm {
    /// One-shot, tool-free baseline (`alloy-eval-live-naive`).
    Naive,
    /// The real agent, as configured by the rendered `router.toml`.
    Alloy,
}

impl ContractArm {
    /// Stable lowercase token used in reason codes and report JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Naive => "naive",
            Self::Alloy => "alloy",
        }
    }
}

/// Provider failure, flattened for evidence.
///
/// `timeout_stage` is populated only for an attributed timeout, so a report
/// can state which deadline expired instead of guessing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderFailure {
    /// Fine-grained provider failure kind (`timeout`, `auth`, `http_status`, …).
    pub kind: String,
    /// Coarse scheduler-facing class (`timeout`, `model`, `internal`, …).
    pub error_class: String,
    /// `connect`, `request`, or `read` when a deadline expired; else `None`.
    pub timeout_stage: Option<String>,
    /// HTTP status when the provider returned an unmapped one.
    pub http_status: Option<u16>,
    /// Redacted, bounded provider message.
    pub message: String,
}

impl ProviderFailure {
    /// Classify a provider error for evidence without losing its stage.
    #[must_use]
    pub fn classify(error: &ProviderError) -> Self {
        let kind = match error {
            ProviderError::Auth => "auth",
            ProviderError::RateLimit => "rate_limit",
            ProviderError::ContextLength => "context_length",
            ProviderError::Timeout | ProviderError::TimeoutAt { .. } => "timeout",
            ProviderError::MalformedResponse(_) => "malformed_response",
            ProviderError::HttpStatus { .. } => "http_status",
            ProviderError::Tls(_) => "tls",
            ProviderError::Transport(_) => "transport",
            _ => "internal",
        };
        let error_class = serde_json::to_value(classify_provider_error(error).class)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| "internal".to_owned());
        Self {
            kind: kind.to_owned(),
            error_class,
            timeout_stage: error.timeout_stage().map(|stage| stage.as_str().to_owned()),
            http_status: error.http_status(),
            message: bounded(&error.to_string()),
        }
    }

    /// Whether this failure was a deadline expiry.
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        self.kind == "timeout"
    }
}

/// One arm's contract, exercised once against the live endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractProbe {
    /// Arm whose contract this probe reproduces.
    pub arm: ContractArm,
    /// Mode the arm asks for.
    pub requested_mode: WireMode,
    /// Mode actually placed on the wire after config gating.
    ///
    /// Differs from `requested_mode` exactly when the arm's configuration
    /// silently downgraded the contract.
    pub wire_mode: WireMode,
    /// `response_format.json_schema.name`, when a schema was sent.
    pub schema_name: Option<String>,
    /// Lowercase hex SHA-256 of the exact schema bytes sent, when any.
    pub schema_digest: Option<String>,
    /// Wall-clock duration of the single completion.
    pub duration_ms: u64,
    /// Whether the provider returned a usable completion.
    pub ok: bool,
    /// Provider-reported input tokens. `None` means unknown — never zero.
    pub tokens_in: Option<u64>,
    /// Provider-reported output tokens. `None` means unknown — never zero.
    pub tokens_out: Option<u64>,
    /// Whether the reply parsed as a JSON object.
    pub structured_reply: bool,
    /// Redacted, bounded provider finish reason.
    pub finish_reason: Option<String>,
    /// Redacted, bounded provider request id.
    pub provider_request_id: Option<String>,
    /// Populated when the completion failed.
    pub failure: Option<ProviderFailure>,
}

impl ContractProbe {
    /// Whether the arm's configuration downgraded its own contract.
    #[must_use]
    pub fn degraded(&self) -> bool {
        self.requested_mode != self.wire_mode
    }
}

/// Machine-readable preflight verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightFailure {
    /// Stable reason code the caller branches on.
    pub code: String,
    /// Bounded human detail; never load-bearing.
    pub detail: String,
}

impl PreflightFailure {
    fn new(code: &str, detail: String) -> Self {
        Self {
            code: code.to_owned(),
            detail: bounded(&detail),
        }
    }
}

/// Persisted preflight document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightReport {
    /// Document schema version.
    pub schema_version: u32,
    /// Whether every contract passed and the arms agree.
    pub ok: bool,
    /// Reason the preflight failed, when it did.
    pub failure: Option<PreflightFailure>,
    /// Wire model id probed.
    pub model: String,
    /// OpenAI-compatible base URL probed.
    pub base_url: String,
    /// Configured connect deadline.
    pub connect_timeout_ms: u64,
    /// Configured whole-request deadline.
    pub request_timeout_ms: u64,
    /// Whether both arms put the same mode on the wire.
    pub contracts_match: bool,
    /// One entry per arm, in probe order.
    pub probes: Vec<ContractProbe>,
}

/// Inputs for a preflight run.
#[derive(Debug, Clone)]
pub struct PreflightSpec {
    /// Wire model id.
    pub model: String,
    /// OpenAI-compatible base URL.
    pub base_url: String,
    /// Sampling temperature both arms use.
    pub temperature: f64,
    /// Connect deadline.
    pub connect_timeout: Duration,
    /// Whole-request deadline.
    pub request_timeout: Duration,
}

/// The contract one arm will execute.
#[derive(Debug, Clone, PartialEq)]
pub struct ArmContract {
    /// Arm this contract belongs to.
    pub arm: ContractArm,
    /// Mode the arm asks for before any config gating.
    pub requested_mode: WireMode,
    /// Completion request exactly as the arm would send it.
    pub request: CompletionRequest,
}

impl ArmContract {
    /// Mode actually placed on the wire.
    #[must_use]
    pub fn wire_mode(&self) -> WireMode {
        WireMode::of(&self.request.response_format)
    }

    fn schema(&self) -> Option<(&str, &serde_json::Value)> {
        match &self.request.response_format {
            ResponseFormat::JsonSchema { name, schema } => Some((name.as_str(), schema)),
            _ => None,
        }
    }
}

/// Build the naive arm's contract: exactly what `alloy-eval-live-naive`
/// sends, schema included.
///
/// # Errors
///
/// [`EvalError::Manifest`] when the probe request cannot be built (only a
/// non-finite temperature can cause this).
pub fn naive_contract(temperature: f64) -> Result<ArmContract, EvalError> {
    let request = build_naive_request(
        "return the file unchanged",
        "src/lib.rs",
        "pub fn ok() {}\n",
        "no diagnostics",
        temperature,
    )
    .map_err(EvalError::Manifest)?;
    Ok(ArmContract {
        arm: ContractArm::Naive,
        requested_mode: WireMode::JsonSchema,
        request,
    })
}

/// Build the Alloy arm's contract from its **rendered `router.toml`**.
///
/// The mode is derived the same way the router derives it at completion time:
/// a repair capability requests structured output and supplies
/// [`repair_response_schema`], and the schema only reaches the wire when the
/// endpoint declared `supports_json_schema`. A config that would make the
/// router degrade therefore yields `requested_mode = json_schema` with
/// `wire_mode = json_object`, which the preflight reports rather than hides.
///
/// # Errors
///
/// [`EvalError::Manifest`] when the document is not a loadable router config
/// or declares no endpoint.
pub fn alloy_contract(router_toml: &str, temperature: f64) -> Result<ArmContract, EvalError> {
    let config = RouterConfig::from_str("byom-preflight", router_toml)
        .map_err(|error| EvalError::Manifest(format!("rendered router.toml: {error}")))?;
    let endpoint = config
        .providers
        .first()
        .and_then(|provider| provider.endpoints.first())
        .ok_or_else(|| {
            EvalError::Manifest("rendered router.toml declares no endpoint".to_owned())
        })?;

    let schema = repair_response_schema();
    let response_format = if !endpoint.supports_structured_output {
        ResponseFormat::Text
    } else if endpoint.supports_json_schema {
        ResponseFormat::JsonSchema {
            name: schema.name.clone(),
            schema: schema.schema.clone(),
        }
    } else {
        ResponseFormat::JsonObject
    };

    Ok(ArmContract {
        arm: ContractArm::Alloy,
        requested_mode: WireMode::JsonSchema,
        request: CompletionRequest {
            messages: vec![
                ChatMessage {
                    role: ChatRole::System,
                    content: "Reply with only the JSON object the schema requires.".to_owned(),
                },
                ChatMessage {
                    role: ChatRole::User,
                    content: "Produce a minimal repair plan for an empty crate.".to_owned(),
                },
            ],
            tools: vec![],
            tool_choice: ToolChoice::None,
            response_format,
            temperature: Some(temperature as f32),
            max_output_tokens: None,
        },
    })
}

/// Decide the preflight verdict from already-collected probes.
///
/// Pure: the same probes always produce the same verdict, so the fail-closed
/// rule is testable without a network.
#[must_use]
pub fn evaluate(probes: &[ContractProbe]) -> Option<PreflightFailure> {
    if probes.is_empty() {
        return Some(PreflightFailure::new(
            "no_contracts_probed",
            "preflight ran zero contracts".to_owned(),
        ));
    }
    // A degrade is reported before a transport failure: a downgraded contract
    // that happens to succeed is the failure mode E1 actually shipped.
    if let Some(probe) = probes.iter().find(|probe| probe.degraded()) {
        return Some(PreflightFailure::new(
            "wire_contract_degraded",
            format!(
                "{} arm asked for {} but its configuration puts {} on the wire",
                probe.arm.as_str(),
                probe.requested_mode,
                probe.wire_mode
            ),
        ));
    }
    if let Some(probe) = probes.iter().find(|probe| !probe.ok) {
        let failure = probe.failure.as_ref();
        let detail = failure.map_or_else(
            || format!("{} contract failed", probe.arm.as_str()),
            |failure| match &failure.timeout_stage {
                Some(stage) => format!(
                    "{} contract timed out during the {stage} stage after {}ms",
                    probe.arm.as_str(),
                    probe.duration_ms
                ),
                None => format!(
                    "{} contract failed: {} ({})",
                    probe.arm.as_str(),
                    failure.kind,
                    failure.message
                ),
            },
        );
        let code = if failure.is_some_and(ProviderFailure::is_timeout) {
            "contract_timeout"
        } else {
            "contract_failed"
        };
        return Some(PreflightFailure::new(code, detail));
    }
    let mut modes = probes.iter().map(|probe| probe.wire_mode);
    let first = modes.next().unwrap_or(WireMode::Text);
    if let Some(divergent) = modes.find(|mode| *mode != first) {
        return Some(PreflightFailure::new(
            "wire_contract_mismatch",
            format!("arms disagree on the wire contract: {first} vs {divergent}"),
        ));
    }
    None
}

/// Exercise both arms' contracts against the live endpoint and assemble the
/// persisted document.
///
/// Runs even when a probe fails: every contract is attempted so the report
/// says what each one did, and the verdict is computed afterwards by
/// [`evaluate`]. A wholly unresponsive endpoint therefore costs up to
/// `contracts × request_timeout`; give the preflight a tighter deadline than
/// the sweep when that matters.
///
/// # Errors
///
/// [`EvalError::Manifest`] when a contract cannot be built or the provider
/// cannot be constructed (bad base URL, unusable key). Provider *call*
/// failures are not errors here — they are recorded in the report and turn
/// `ok` false.
pub async fn run(
    spec: &PreflightSpec,
    router_toml: &str,
    api_key: SecretString,
) -> Result<PreflightReport, EvalError> {
    let contracts = [
        naive_contract(spec.temperature)?,
        alloy_contract(router_toml, spec.temperature)?,
    ];

    let provider_id = ProviderId::new("byom-preflight")
        .map_err(|error| EvalError::Manifest(format!("provider id: {error:?}")))?;
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleSpec {
        id: provider_id.clone(),
        base_url: spec.base_url.clone(),
        api_key,
        connect_timeout: spec.connect_timeout,
        request_timeout: spec.request_timeout,
    })
    .map_err(|error| EvalError::Manifest(format!("provider: {error}")))?;
    let endpoint = probe_endpoint(&provider_id, &spec.model)?;

    let mut probes = Vec::with_capacity(contracts.len());
    for contract in contracts {
        let started = std::time::Instant::now();
        let outcome = provider.complete(&endpoint, contract.request.clone()).await;
        let duration_ms = duration_ms(started.elapsed());
        probes.push(probe_from(&contract, duration_ms, outcome));
    }

    let failure = evaluate(&probes);
    let contracts_match = probes.first().is_some_and(|first| {
        probes
            .iter()
            .all(|probe| probe.wire_mode == first.wire_mode)
    });
    Ok(PreflightReport {
        schema_version: PREFLIGHT_SCHEMA_VERSION,
        ok: failure.is_none(),
        failure,
        model: spec.model.clone(),
        base_url: spec.base_url.clone(),
        connect_timeout_ms: duration_ms(spec.connect_timeout),
        request_timeout_ms: duration_ms(spec.request_timeout),
        contracts_match,
        probes,
    })
}

fn probe_endpoint(provider: &ProviderId, model: &str) -> Result<ModelEndpoint, EvalError> {
    Ok(ModelEndpoint {
        id: EndpointId::new(PROBE_ENDPOINT_ID)
            .map_err(|error| EvalError::Manifest(format!("endpoint id: {error:?}")))?,
        provider: provider.clone(),
        display_name: "BYOM preflight".to_owned(),
        model: model.to_owned(),
        tiers: vec![ModelTier::Standard],
        supports_tools: false,
        supports_structured_output: true,
        // The probe never gates: each arm's contract is already resolved, so
        // the endpoint must transmit whatever mode that contract carries.
        supports_json_schema: true,
        json_schema_strict: false,
        max_context: 131_072,
        input_usd_per_mtok: None,
        output_usd_per_mtok: None,
        temperature: None,
    })
}

fn probe_from(
    contract: &ArmContract,
    duration_ms: u64,
    outcome: Result<ModelResponse, ProviderError>,
) -> ContractProbe {
    let (schema_name, schema_digest) = match contract.schema() {
        Some((name, schema)) => (
            Some(name.to_owned()),
            Some(schema_digest(schema).as_hex().to_owned()),
        ),
        None => (None, None),
    };
    let mut probe = ContractProbe {
        arm: contract.arm,
        requested_mode: contract.requested_mode,
        wire_mode: contract.wire_mode(),
        schema_name,
        schema_digest,
        duration_ms,
        ok: false,
        tokens_in: None,
        tokens_out: None,
        structured_reply: false,
        finish_reason: None,
        provider_request_id: None,
        failure: None,
    };
    match outcome {
        Ok(response) => {
            probe.ok = true;
            // Absent usage stays absent: a preflight that printed 0 would
            // read as a measured zero-token call.
            probe.tokens_in = response.usage.input_tokens;
            probe.tokens_out = response.usage.output_tokens;
            probe.structured_reply = response.structured.is_some()
                || response
                    .text
                    .as_deref()
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                    .is_some_and(|value| value.is_object());
            probe.finish_reason = response.finish_reason.as_deref().map(bounded);
            probe.provider_request_id = response.provider_request_id.as_deref().map(bounded);
        }
        Err(error) => probe.failure = Some(ProviderFailure::classify(&error)),
    }
    probe
}

/// SHA-256 over the exact schema bytes that go on the wire.
#[must_use]
pub fn schema_digest(schema: &serde_json::Value) -> Digest {
    let bytes = serde_json::to_vec(schema).unwrap_or_else(|_| b"null".to_vec());
    Digest::sha256(&bytes)
}

/// Digest of a [`JsonSchemaSpec`]'s schema body.
#[must_use]
pub fn spec_digest(spec: &JsonSchemaSpec) -> Digest {
    schema_digest(&spec.schema)
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Bound a message on a char boundary so a report line stays small.
fn bounded(value: &str) -> String {
    const MAX: usize = 512;
    if value.len() <= MAX {
        return value.to_owned();
    }
    let mut end = MAX;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_repair::render_router_toml;
    use crate::LiveRepairEndpoint;
    use alloy_runtime::TimeoutStage;

    fn endpoint() -> LiveRepairEndpoint {
        LiveRepairEndpoint {
            model: "qwen2.5-coder:32b".to_owned(),
            temperature: 0.6,
            base_url: "http://127.0.0.1:11434/v1/".to_owned(),
        }
    }

    fn probe(arm: ContractArm, requested: WireMode, wire: WireMode, ok: bool) -> ContractProbe {
        ContractProbe {
            arm,
            requested_mode: requested,
            wire_mode: wire,
            schema_name: None,
            schema_digest: None,
            duration_ms: 5,
            ok,
            tokens_in: None,
            tokens_out: None,
            structured_reply: ok,
            finish_reason: None,
            provider_request_id: None,
            failure: None,
        }
    }

    /// E2 (c) — the two arms must be provably on the same wire contract, and
    /// the Alloy arm's contract must come from the document it actually runs.
    #[test]
    fn both_arms_derive_the_same_wire_contract_from_shipped_config() {
        let rendered = render_router_toml(&endpoint()).unwrap();
        let naive = naive_contract(0.6).unwrap();
        let alloy = alloy_contract(&rendered, 0.6).unwrap();

        assert_eq!(naive.wire_mode(), WireMode::JsonSchema);
        assert_eq!(alloy.wire_mode(), WireMode::JsonSchema);
        assert_eq!(naive.wire_mode(), alloy.wire_mode());
        // The naive arm stays tool-free; the contract under test is the
        // response format, not the tool surface.
        assert!(naive.request.tools.is_empty());
        assert_eq!(naive.request.tool_choice, ToolChoice::None);

        // Neither arm is silently degraded by its own configuration.
        assert_eq!(naive.requested_mode, naive.wire_mode());
        assert_eq!(alloy.requested_mode, alloy.wire_mode());

        // The Alloy probe carries the agent's real repair schema, byte-exact.
        let (name, schema) = alloy.schema().expect("alloy arm sends a schema");
        assert_eq!(name, repair_response_schema().name);
        assert_eq!(
            schema_digest(schema).as_hex(),
            spec_digest(&repair_response_schema()).as_hex()
        );
    }

    /// The exact E1 regression: an endpoint that omits `supports_json_schema`
    /// downgrades the Alloy arm, and the preflight must say so instead of
    /// letting the sweep run on mismatched contracts.
    #[test]
    fn omitted_supports_json_schema_is_reported_as_a_degrade() {
        let rendered = render_router_toml(&endpoint()).unwrap();
        let without = rendered.replace("supports_json_schema = true\n", "");
        assert!(!without.contains("supports_json_schema"));

        let alloy = alloy_contract(&without, 0.6).unwrap();
        assert_eq!(alloy.requested_mode, WireMode::JsonSchema);
        assert_eq!(
            alloy.wire_mode(),
            WireMode::JsonObject,
            "an omitted flag serde-defaults to false and downgrades the arm"
        );

        let degraded = ContractProbe {
            requested_mode: alloy.requested_mode,
            wire_mode: alloy.wire_mode(),
            ..probe(
                ContractArm::Alloy,
                WireMode::JsonSchema,
                WireMode::JsonObject,
                true,
            )
        };
        let failure = evaluate(&[
            probe(
                ContractArm::Naive,
                WireMode::JsonSchema,
                WireMode::JsonSchema,
                true,
            ),
            degraded,
        ])
        .expect("a downgraded arm must fail the preflight");
        assert_eq!(failure.code, "wire_contract_degraded");
        assert!(failure.detail.contains("alloy"), "{}", failure.detail);
        assert!(failure.detail.contains("json_object"), "{}", failure.detail);
    }

    /// Fail closed: any failing contract, and any disagreement between arms,
    /// yields a non-empty machine-readable verdict.
    #[test]
    fn preflight_fails_closed_on_failure_and_on_mismatch() {
        let good = probe(
            ContractArm::Naive,
            WireMode::JsonSchema,
            WireMode::JsonSchema,
            true,
        );
        let good_alloy = probe(
            ContractArm::Alloy,
            WireMode::JsonSchema,
            WireMode::JsonSchema,
            true,
        );
        assert_eq!(evaluate(&[good.clone(), good_alloy.clone()]), None);

        assert_eq!(
            evaluate(&[]).expect("zero contracts is a failure").code,
            "no_contracts_probed"
        );

        let mut failed = good_alloy.clone();
        failed.ok = false;
        failed.failure = Some(ProviderFailure::classify(&ProviderError::Auth));
        let verdict = evaluate(&[good.clone(), failed]).expect("a failed contract fails closed");
        assert_eq!(verdict.code, "contract_failed");
        assert!(verdict.detail.contains("auth"), "{}", verdict.detail);

        // A mismatch that is not a self-degrade (both arms consistent with
        // their own config, but different from each other) is still fatal.
        let mut other = good_alloy;
        other.requested_mode = WireMode::JsonObject;
        other.wire_mode = WireMode::JsonObject;
        let verdict = evaluate(&[good, other]).expect("mismatched arms fail closed");
        assert_eq!(verdict.code, "wire_contract_mismatch");
        assert!(verdict.detail.contains("json_object"), "{}", verdict.detail);
    }

    /// E2 (a)+(d) — a timeout is reported as a timeout, with its stage, and
    /// never collapses into a generic failure.
    #[test]
    fn timeout_verdict_names_the_expiring_stage() {
        for (stage, token) in [
            (TimeoutStage::Connect, "connect"),
            (TimeoutStage::Request, "request"),
            (TimeoutStage::Read, "read"),
        ] {
            let mut timed_out = probe(
                ContractArm::Naive,
                WireMode::JsonSchema,
                WireMode::JsonSchema,
                false,
            );
            timed_out.duration_ms = 10_000;
            timed_out.failure = Some(ProviderFailure::classify(&ProviderError::TimeoutAt {
                stage,
            }));
            let failure = timed_out.failure.clone().unwrap();
            assert_eq!(failure.kind, "timeout");
            assert_eq!(failure.error_class, "timeout");
            assert_eq!(failure.timeout_stage.as_deref(), Some(token));
            assert_eq!(failure.http_status, None);

            let verdict = evaluate(&[timed_out]).expect("a timeout fails closed");
            assert_eq!(verdict.code, "contract_timeout");
            assert!(verdict.detail.contains(token), "{}", verdict.detail);
            assert!(verdict.detail.contains("10000ms"), "{}", verdict.detail);
        }

        // An unattributed timeout reports no stage rather than inventing one.
        let unattributed = ProviderFailure::classify(&ProviderError::Timeout);
        assert_eq!(unattributed.kind, "timeout");
        assert_eq!(unattributed.timeout_stage, None);
    }

    /// HTTP status and error class survive into evidence.
    #[test]
    fn http_failures_retain_status_and_error_class() {
        let failure = ProviderFailure::classify(&ProviderError::HttpStatus {
            status: 502,
            message: "bad gateway".to_owned(),
        });
        assert_eq!(failure.kind, "http_status");
        assert_eq!(failure.http_status, Some(502));
        assert_eq!(failure.error_class, "model");
        assert_eq!(failure.timeout_stage, None);
        assert!(failure.message.contains("502"), "{}", failure.message);
    }

    /// E2 (b) — a successful probe with no provider usage must serialize
    /// `null`, not `0`.
    #[test]
    fn absent_usage_serializes_as_null_never_zero() {
        let contract = naive_contract(0.6).unwrap();
        let probe = probe_from(
            &contract,
            42,
            Ok(ModelResponse {
                text: Some("{\"replacement\":\"x\"}".to_owned()),
                structured: None,
                tool_calls: vec![],
                usage: alloy_runtime::Usage {
                    input_tokens: None,
                    output_tokens: None,
                },
                provider_request_id: None,
                finish_reason: Some("stop".to_owned()),
            }),
        );
        assert!(probe.ok);
        assert_eq!(probe.tokens_in, None);
        assert_eq!(probe.tokens_out, None);
        assert!(probe.structured_reply);
        assert_eq!(probe.schema_name.as_deref(), Some(crate::NAIVE_SCHEMA_NAME));
        assert!(probe.schema_digest.is_some());

        let json = serde_json::to_value(&probe).unwrap();
        assert!(json["tokens_in"].is_null());
        assert!(json["tokens_out"].is_null());
        assert_eq!(json["tokens_in"].as_u64(), None);

        // A provider that reports zero is still reported as zero.
        let measured = probe_from(
            &contract,
            42,
            Ok(ModelResponse {
                text: Some("{}".to_owned()),
                structured: None,
                tool_calls: vec![],
                usage: alloy_runtime::Usage {
                    input_tokens: Some(0),
                    output_tokens: Some(11),
                },
                provider_request_id: None,
                finish_reason: None,
            }),
        );
        assert_eq!(measured.tokens_in, Some(0));
        assert_eq!(measured.tokens_out, Some(11));
    }

    /// The persisted document round-trips and carries every field a caller
    /// needs to abort a matrix.
    #[test]
    fn report_round_trips_with_stage_status_and_digests() {
        let contract = naive_contract(0.6).unwrap();
        let timed_out = probe_from(
            &contract,
            9_999,
            Err(ProviderError::TimeoutAt {
                stage: TimeoutStage::Read,
            }),
        );
        let failure = evaluate(std::slice::from_ref(&timed_out));
        let report = PreflightReport {
            schema_version: PREFLIGHT_SCHEMA_VERSION,
            ok: failure.is_none(),
            failure,
            model: "m".to_owned(),
            base_url: "http://127.0.0.1:1/v1/".to_owned(),
            connect_timeout_ms: 10_000,
            request_timeout_ms: 600_000,
            contracts_match: true,
            probes: vec![timed_out],
        };
        assert!(!report.ok);

        let text = serde_json::to_string(&report).unwrap();
        let parsed: PreflightReport = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, report);
        assert_eq!(
            parsed.probes[0]
                .failure
                .as_ref()
                .unwrap()
                .timeout_stage
                .as_deref(),
            Some("read")
        );
        assert_eq!(parsed.failure.unwrap().code, "contract_timeout");
        assert_eq!(parsed.probes[0].wire_mode, WireMode::JsonSchema);
        assert!(parsed.probes[0].schema_digest.is_some());
    }

    #[test]
    fn alloy_contract_rejects_an_unloadable_router_document() {
        assert!(alloy_contract("not = [toml", 0.6).is_err());
        assert!(alloy_contract("", 0.6).is_err());
    }

    /// End-to-end: both contracts reach a real socket, both bodies carry the
    /// expected `response_format`, and the persisted report says so.
    #[tokio::test]
    async fn live_run_probes_both_contracts_and_records_the_wire_bodies() {
        let server = StubServer::spawn(StubMode::Ok);
        let report = run(
            &spec(server.base_url(), Duration::from_secs(5)),
            &render_router_toml(&endpoint()).unwrap(),
            SecretString::new("k"),
        )
        .await
        .unwrap();

        assert!(report.ok, "{:?}", report.failure);
        assert_eq!(report.failure, None);
        assert!(report.contracts_match);
        assert_eq!(report.schema_version, PREFLIGHT_SCHEMA_VERSION);
        assert_eq!(report.probes.len(), 2);
        assert_eq!(report.probes[0].arm, ContractArm::Naive);
        assert_eq!(report.probes[1].arm, ContractArm::Alloy);
        for probe in &report.probes {
            assert!(probe.ok);
            assert_eq!(probe.wire_mode, WireMode::JsonSchema);
            assert!(!probe.degraded());
            assert!(probe.schema_digest.is_some());
            // The stub reports no usage, so usage stays unknown.
            assert_eq!(probe.tokens_in, None);
            assert_eq!(probe.tokens_out, None);
        }

        // Both arms genuinely put `json_schema` on the wire, with their own
        // schema names.
        let bodies = server.bodies();
        assert_eq!(bodies.len(), 2);
        let names: Vec<String> = bodies
            .iter()
            .map(|body| {
                let value: serde_json::Value = serde_json::from_str(body).unwrap();
                assert_eq!(value["response_format"]["type"], "json_schema");
                value["response_format"]["json_schema"]["name"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(names[0], crate::NAIVE_SCHEMA_NAME);
        assert_eq!(names[1], repair_response_schema().name);
    }

    /// E2 (a)+(d) — a stalled endpoint fails the preflight as a timeout with
    /// its stage, and the report is still complete enough to abort a matrix.
    #[tokio::test]
    async fn live_run_reports_a_stalled_endpoint_as_a_staged_timeout() {
        let server = StubServer::spawn(StubMode::Stall);
        let report = run(
            &spec(server.base_url(), Duration::from_millis(250)),
            &render_router_toml(&endpoint()).unwrap(),
            SecretString::new("k"),
        )
        .await
        .unwrap();

        assert!(!report.ok);
        let failure = report.failure.as_ref().expect("a stall must fail closed");
        assert_eq!(failure.code, "contract_timeout");
        assert!(failure.detail.contains("request"), "{}", failure.detail);
        let probe = &report.probes[0];
        assert!(!probe.ok);
        let probe_failure = probe.failure.as_ref().unwrap();
        assert_eq!(probe_failure.kind, "timeout");
        assert_eq!(probe_failure.timeout_stage.as_deref(), Some("request"));
        assert_eq!(probe_failure.http_status, None);
        assert_eq!(probe.tokens_in, None);
        assert_eq!(report.request_timeout_ms, 250);
    }

    /// An HTTP rejection is fail-closed and keeps its status in evidence.
    #[tokio::test]
    async fn live_run_reports_an_http_rejection_with_its_status() {
        let server = StubServer::spawn(StubMode::Status(422));
        let report = run(
            &spec(server.base_url(), Duration::from_secs(5)),
            &render_router_toml(&endpoint()).unwrap(),
            SecretString::new("k"),
        )
        .await
        .unwrap();

        assert!(!report.ok);
        assert_eq!(report.failure.as_ref().unwrap().code, "contract_failed");
        let probe_failure = report.probes[0].failure.as_ref().unwrap();
        assert_eq!(probe_failure.kind, "http_status");
        assert_eq!(probe_failure.http_status, Some(422));
        assert_eq!(probe_failure.error_class, "model");
        assert_eq!(probe_failure.timeout_stage, None);
    }

    fn spec(base_url: String, request_timeout: Duration) -> PreflightSpec {
        PreflightSpec {
            model: "stub-model".to_owned(),
            base_url,
            temperature: 0.6,
            connect_timeout: Duration::from_millis(200),
            request_timeout,
        }
    }

    #[derive(Clone, Copy)]
    enum StubMode {
        /// Return a valid completion with no `usage` field.
        Ok,
        /// Accept and never answer.
        Stall,
        /// Answer with the given non-success status.
        Status(u16),
    }

    /// Loopback-only OpenAI-compatible stub that records request bodies.
    struct StubServer {
        port: u16,
        bodies: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl StubServer {
        fn spawn(mode: StubMode) -> Self {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let recorded = std::sync::Arc::clone(&bodies);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let recorded = std::sync::Arc::clone(&recorded);
                    std::thread::spawn(move || {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                        let mut buf = Vec::new();
                        let mut chunk = [0u8; 4096];
                        let head_end = loop {
                            let Ok(read) = stream.read(&mut chunk) else {
                                return;
                            };
                            if read == 0 {
                                return;
                            }
                            buf.extend_from_slice(&chunk[..read]);
                            if let Some(pos) = buf
                                .windows(4)
                                .position(|window| window == b"\r\n\r\n")
                                .map(|pos| pos + 4)
                            {
                                break pos;
                            }
                            if buf.len() > 1 << 20 {
                                return;
                            }
                        };
                        let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
                        let length: usize = head
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|value| value.trim().parse().ok())
                            .unwrap_or(0);
                        while buf.len() < head_end + length {
                            let Ok(read) = stream.read(&mut chunk) else {
                                break;
                            };
                            if read == 0 {
                                break;
                            }
                            buf.extend_from_slice(&chunk[..read]);
                        }
                        recorded
                            .lock()
                            .unwrap()
                            .push(String::from_utf8_lossy(&buf[head_end..]).into_owned());

                        match mode {
                            StubMode::Stall => {
                                std::thread::sleep(Duration::from_secs(30));
                            }
                            StubMode::Ok => {
                                // Deliberately no `usage`: unknown must stay
                                // unknown all the way into the report.
                                let doc = r#"{"id":"stub","choices":[{"message":{"content":"{\"ok\":true}"},"finish_reason":"stop"}]}"#;
                                let _ = stream.write_all(
                                    format!(
                                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{doc}",
                                        doc.len()
                                    )
                                    .as_bytes(),
                                );
                            }
                            StubMode::Status(status) => {
                                let doc = r#"{"error":{"message":"nope"}}"#;
                                let _ = stream.write_all(
                                    format!(
                                        "HTTP/1.1 {status} Rejected\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{doc}",
                                        doc.len()
                                    )
                                    .as_bytes(),
                                );
                            }
                        }
                    });
                }
            });
            Self { port, bodies }
        }

        fn base_url(&self) -> String {
            format!("http://127.0.0.1:{}/v1/", self.port)
        }

        fn bodies(&self) -> Vec<String> {
            self.bodies.lock().unwrap().clone()
        }
    }

    #[test]
    fn bounded_messages_stay_on_char_boundaries() {
        let long = "é".repeat(600);
        let cut = bounded(&long);
        assert!(cut.len() <= 512);
        assert!(cut.is_char_boundary(cut.len()));
    }
}
