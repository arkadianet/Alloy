use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use alloy_runtime::{
    CompletionRequest, ModelEndpoint, ModelProvider, ModelResponse, ProviderError, Usage,
};
use regex::Regex;
use tokio_util::sync::CancellationToken;

use crate::cost_claim::derive_eval_usd;
use crate::error::{bound_message, EvalError, ReportError};
use crate::fingerprint::RequestFingerprint;
use crate::harness::{FixtureRunOutput, LoadedFixture};
use crate::manifest::{FixtureTurnId, ScriptTurn, SuccessCriterion};
use crate::report::{CriterionResult, FixtureOutcome, FixtureStatus};
use crate::scripted::{
    ScriptOutcome, ScriptedProvider, SCRIPTED_MISS_PREFIX, SCRIPTED_WRONG_ENDPOINT,
};
use crate::trajectory::EvalTrajectoryRecord;

/// §5.3.1 carrier detail; conformance tests match it byte-for-byte.
const SCRIPT_MISS_DETAIL: &str = "script miss";
const CANCEL_BEFORE_COMPLETE: &str = "before_complete";
const CANCEL_BEFORE_PATCH: &str = "before_patch";
const CANCEL_BEFORE_CRITERIA: &str = "before_criteria";

/// One `unsafe` occurrence per source line; compiled once for the process.
static UNSAFE_LINE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|\s)unsafe(\s|!|\(|\{)").expect("unsafe line regex is valid"));

// Test-only fault injection for the §5.3.2 built-request comparison. The
// production path never sets it, so `build_turn_request` stays an identity.
#[cfg(test)]
thread_local! {
    static FORCE_SCRIPT_MISS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScriptedDriverMode {
    SkeletonReplay,
    /// Offline scripted control-plane path: all manifest turns, same oracle as
    /// skeleton replay. Under `--features stack-driver` the live path is used
    /// instead, so this variant is constructed only in the offline build.
    #[cfg_attr(feature = "stack-driver", allow(dead_code))]
    ControlPlane,
    /// Offline scripted naive baseline. Under `--features stack-driver` the
    /// live naive path is used instead (lib build); unit tests still construct
    /// this variant under `cfg(test)`.
    #[cfg_attr(all(feature = "stack-driver", not(test)), allow(dead_code))]
    NaiveBaseline,
}

struct CriteriaState {
    results: Vec<CriterionResult>,
    carrier: usize,
}

struct RunObservations {
    tokens_in: Option<u64>,
    tokens_out: Option<u64>,
    tokens_in_incomplete: bool,
    tokens_out_incomplete: bool,
    successful_responses: u32,
    cost_usd: Option<f64>,
    compile_clean: Option<bool>,
    unsafe_introduced: Option<bool>,
    patch_passed: bool,
}

impl RunObservations {
    /// Token and cost fields start absent: they only become `Some` once a
    /// successful response actually reports them (§3.7.1).
    fn new() -> Self {
        Self {
            tokens_in: None,
            tokens_out: None,
            tokens_in_incomplete: false,
            tokens_out_incomplete: false,
            successful_responses: 0,
            cost_usd: None,
            compile_clean: None,
            unsafe_introduced: None,
            patch_passed: false,
        }
    }
}

pub(crate) async fn run(
    fixture: &LoadedFixture,
    provider: Arc<ScriptedProvider>,
    cancel: Option<CancellationToken>,
) -> FixtureRunOutput {
    run_scripted(
        fixture,
        provider,
        cancel,
        ScriptedDriverMode::SkeletonReplay,
    )
    .await
}

pub(crate) async fn run_scripted(
    fixture: &LoadedFixture,
    provider: Arc<ScriptedProvider>,
    cancel: Option<CancellationToken>,
    mode: ScriptedDriverMode,
) -> FixtureRunOutput {
    let started = Instant::now();
    let mut trajectories = Vec::new();
    let mut criteria = CriteriaState::new(&fixture.manifest.success_criteria);

    let plan = match execution_plan(fixture, mode) {
        Ok(plan) => plan,
        Err(error) => return error_output(fixture, started, error, trajectories),
    };

    for (fingerprint, outcome) in &plan.entries {
        if is_cancelled(&cancel) {
            return cancelled_output(fixture, started, trajectories);
        }
        provider.insert(fingerprint.clone(), outcome.clone());
    }

    let mut candidate = None::<String>;
    let mut observations = RunObservations::new();

    for turn in &plan.turns {
        if is_cancelled(&cancel) {
            return cancelled_output(fixture, started, trajectories);
        }

        let request = build_turn_request(turn);
        if request != turn.request {
            tracing::warn!(
                fixture_id = %fixture.manifest.id,
                turn_id = %turn.turn_id.render(),
                "eval driver built a request that misses the script"
            );
            criteria.set_carrier_failure(SCRIPT_MISS_DETAIL);
            break;
        }

        if is_cancelled(&cancel) {
            return cancelled_output(fixture, started, trajectories);
        }

        let dispatched = dispatch_turn(
            &provider,
            fixture,
            &turn.turn_id,
            request,
            &cancel,
            &mut trajectories,
        )
        .await;

        match dispatched {
            None => return cancelled_output(fixture, started, trajectories),
            Some(Ok(response)) => {
                accumulate_usage(&mut observations, &response.usage);
                if let Some(text) = response.text {
                    candidate = Some(text);
                }
            }
            Some(Err(error)) => {
                if let Some(error) = provider_map_error(&error) {
                    if matches!(
                        &error,
                        EvalError::Internal(message)
                            if message.starts_with(SCRIPTED_MISS_PREFIX)
                    ) {
                        tracing::warn!(
                            fixture_id = %fixture.manifest.id,
                            turn_id = %turn.turn_id.render(),
                            "eval scripted provider map desynchronized"
                        );
                    }
                    return error_output(fixture, started, error, trajectories);
                }
                if candidate.is_none() {
                    observations.compile_clean = Some(false);
                    criteria.set_carrier_failure("provider error before repair text");
                    break;
                }
            }
        }
    }

    finalize_cost(&mut observations, &fixture.endpoint);

    if candidate.is_none() {
        observations.compile_clean = Some(false);
        criteria.set_carrier_failure("missing repair text");
    }

    maybe_cancel_at_checkpoint(fixture, CANCEL_BEFORE_PATCH, &cancel);
    if is_cancelled(&cancel) {
        return cancelled_output(fixture, started, trajectories);
    }

    if let Some(candidate) = candidate.as_deref() {
        match read_bytes(fixture.paths.golden.clone()).await {
            Ok(golden) if candidate.as_bytes() == golden.as_slice() => {
                observations.patch_passed = true;
                match fixture.post_repair.compile_clean() {
                    Ok(clean) => observations.compile_clean = Some(clean),
                    Err(error) => return error_output(fixture, started, error, trajectories),
                }
                if observations.compile_clean == Some(false) {
                    criteria.set_carrier_failure("compile not clean");
                }
            }
            Ok(_) => {
                observations.compile_clean = Some(false);
                criteria.set_carrier_failure("patch oracle failed");
            }
            Err(error) => return error_output(fixture, started, error, trajectories),
        }
    }

    maybe_cancel_at_checkpoint(fixture, CANCEL_BEFORE_CRITERIA, &cancel);
    if is_cancelled(&cancel) {
        return cancelled_output(fixture, started, trajectories);
    }

    for index in 0..fixture.manifest.success_criteria.len() {
        let criterion = fixture.manifest.success_criteria[index];
        let result = match criterion {
            SuccessCriterion::CompileClean => {
                let passed = observations.compile_clean == Some(true);
                CriterionResult {
                    name: criterion,
                    passed,
                    detail: if passed {
                        String::new()
                    } else {
                        "compile not clean".to_owned()
                    },
                }
            }
            SuccessCriterion::NoNewUnsafe => {
                match evaluate_no_new_unsafe(fixture, candidate.as_deref()).await {
                    Ok((introduced, result)) => {
                        observations.unsafe_introduced = Some(introduced);
                        result
                    }
                    Err(error) => return error_output(fixture, started, error, trajectories),
                }
            }
            SuccessCriterion::ExpectedDiagnosticsCleared => {
                match evaluate_expected_diagnostics_cleared(fixture, observations.patch_passed) {
                    Ok(result) => result,
                    Err(error) => return error_output(fixture, started, error, trajectories),
                }
            }
            SuccessCriterion::ScriptTurnsConsumed => {
                let remaining = provider.remaining_keys().len();
                let passed = !fixture.manifest.require_consume_all || remaining == 0;
                CriterionResult {
                    name: criterion,
                    passed,
                    detail: if passed {
                        String::new()
                    } else {
                        format!("unconsumed script keys: {remaining}")
                    },
                }
            }
        };
        criteria.set_result(index, result);
    }

    let status = if criteria.results.iter().all(|criterion| criterion.passed) {
        FixtureStatus::Pass
    } else {
        FixtureStatus::Fail
    };
    stamp_trajectories(&mut trajectories, status, observations.compile_clean);
    FixtureRunOutput {
        outcome: FixtureOutcome {
            fixture_id: fixture.manifest.id.clone(),
            set: fixture.manifest.set,
            status,
            criteria: criteria.results,
            wall_ms: elapsed_ms(started),
            model_calls: saturating_len_u32(trajectories.len()),
            tokens_in: observations.tokens_in,
            tokens_out: observations.tokens_out,
            cost_usd: observations.cost_usd,
            retry_count: None,
            human_interventions: None,
            unsafe_introduced: observations.unsafe_introduced,
            compile_clean: observations.compile_clean.or(Some(false)),
            error: None,
        },
        trajectories,
    }
}

/// Build the request the driver sends for `turn`.
///
/// Day-1 replays `turn.request` byte-for-byte (§5.3.2 step 1); the seam exists
/// so a future driver that synthesizes requests still gets compared against
/// the manifest before the provider is called.
fn build_turn_request(turn: &ScriptTurn) -> CompletionRequest {
    #[cfg(test)]
    if FORCE_SCRIPT_MISS.with(std::cell::Cell::get) {
        let mut request = turn.request.clone();
        request.max_output_tokens = Some(
            request
                .max_output_tokens
                .unwrap_or_default()
                .saturating_add(1),
        );
        return request;
    }
    turn.request.clone()
}

/// Race one `complete` against cancellation and append exactly one trajectory
/// row for the dispatch (§3.16 / §5.3.2 step 3).
///
/// Returns `None` when cancellation won; the row is still recorded because the
/// invocation was attempted.
async fn dispatch_turn(
    provider: &ScriptedProvider,
    fixture: &LoadedFixture,
    turn_id: &FixtureTurnId,
    request: CompletionRequest,
    cancel: &Option<CancellationToken>,
    trajectories: &mut Vec<EvalTrajectoryRecord>,
) -> Option<Result<ModelResponse, ProviderError>> {
    let request_fingerprint = RequestFingerprint::of(&request);
    let attempt_started = Instant::now();
    let complete = provider.complete(&fixture.endpoint, request);
    maybe_cancel_at_checkpoint(fixture, CANCEL_BEFORE_COMPLETE, cancel);
    let result = if let Some(token) = cancel {
        tokio::select! {
            biased;
            () = token.cancelled() => {
                trajectories.push(EvalTrajectoryRecord::cancelled(
                    fixture.manifest.id.clone(),
                    fixture.manifest.set,
                    turn_id.clone(),
                    request_fingerprint,
                    &fixture.endpoint,
                    Some(elapsed_ms(attempt_started)),
                    FixtureStatus::Error,
                    None,
                ));
                #[cfg(test)]
                if fixture.panic_after_dispatch {
                    panic!("eval test panic");
                }
                return None;
            }
            result = complete => result,
        }
    } else {
        complete.await
    };
    let duration_ms = Some(elapsed_ms(attempt_started));

    trajectories.push(match &result {
        Ok(response) => EvalTrajectoryRecord::from_response(
            fixture.manifest.id.clone(),
            fixture.manifest.set,
            turn_id.clone(),
            request_fingerprint,
            &fixture.endpoint,
            response,
            duration_ms,
            FixtureStatus::Error,
            None,
        ),
        Err(error) => EvalTrajectoryRecord::from_provider_error(
            fixture.manifest.id.clone(),
            fixture.manifest.set,
            turn_id.clone(),
            request_fingerprint,
            &fixture.endpoint,
            error,
            duration_ms,
            FixtureStatus::Error,
            None,
        ),
    });
    #[cfg(test)]
    if fixture.panic_after_dispatch {
        panic!("eval test panic");
    }
    Some(result)
}

struct ExecutionPlan {
    turns: Vec<ScriptTurn>,
    entries: Vec<(RequestFingerprint, ScriptOutcome)>,
}

fn execution_plan(
    fixture: &LoadedFixture,
    mode: ScriptedDriverMode,
) -> Result<ExecutionPlan, EvalError> {
    match mode {
        ScriptedDriverMode::SkeletonReplay | ScriptedDriverMode::ControlPlane => {
            Ok(ExecutionPlan {
                turns: fixture.manifest.turns.clone(),
                entries: fixture.script_entries.clone(),
            })
        }
        ScriptedDriverMode::NaiveBaseline => {
            let turn =
                crate::manifest::require_single_repair_ordinal_zero(&fixture.manifest.turns)?;
            Ok(ExecutionPlan {
                turns: vec![turn.clone()],
                entries: vec![(
                    RequestFingerprint::of(&turn.request),
                    ScriptOutcome::from(turn.outcome.clone()),
                )],
            })
        }
    }
}

impl CriteriaState {
    fn new(criteria: &[SuccessCriterion]) -> Self {
        let carrier = criteria
            .iter()
            .position(|criterion| *criterion == SuccessCriterion::CompileClean)
            .unwrap_or(0);
        let results = criteria
            .iter()
            .copied()
            .map(|name| CriterionResult {
                name,
                passed: true,
                detail: String::new(),
            })
            .collect();
        Self { results, carrier }
    }

    fn set_carrier_failure(&mut self, detail: &str) {
        self.set_failure_if_not_sticky(self.carrier, detail);
    }

    fn set_result(&mut self, index: usize, result: CriterionResult) {
        if self.is_sticky(index) {
            return;
        }
        self.results[index] = result;
    }

    fn set_failure_if_not_sticky(&mut self, index: usize, detail: &str) {
        if self.is_sticky(index) {
            return;
        }
        self.results[index].passed = false;
        self.results[index].detail = detail.to_owned();
    }

    fn is_sticky(&self, index: usize) -> bool {
        !self.results[index].passed && !self.results[index].detail.is_empty()
    }
}

async fn evaluate_no_new_unsafe(
    fixture: &LoadedFixture,
    candidate: Option<&str>,
) -> Result<(bool, CriterionResult), EvalError> {
    let Some(candidate) = candidate else {
        return Ok((
            false,
            CriterionResult {
                name: SuccessCriterion::NoNewUnsafe,
                passed: false,
                detail: "missing repair text".to_owned(),
            },
        ));
    };
    let pre_source = read_to_string(fixture.paths.target.clone()).await?;
    let pre_count = unsafe_line_count(&pre_source);
    let post_count = unsafe_line_count(candidate);
    let introduced = post_count > pre_count;
    Ok((
        introduced,
        CriterionResult {
            name: SuccessCriterion::NoNewUnsafe,
            passed: !introduced,
            detail: if introduced {
                "unsafe introduced".to_owned()
            } else {
                String::new()
            },
        },
    ))
}

fn evaluate_expected_diagnostics_cleared(
    fixture: &LoadedFixture,
    patch_passed: bool,
) -> Result<CriterionResult, EvalError> {
    if !patch_passed {
        return Ok(CriterionResult {
            name: SuccessCriterion::ExpectedDiagnosticsCleared,
            passed: false,
            detail: "patch oracle failed; diagnostics not attributable".to_owned(),
        });
    }
    let diagnostics = fixture.post_repair.diagnostics()?;
    for expected in &fixture.manifest.expected_diagnostics {
        if diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code.as_deref() == Some(expected.code.as_str()))
        {
            return Ok(CriterionResult {
                name: SuccessCriterion::ExpectedDiagnosticsCleared,
                passed: false,
                detail: format!("expected diagnostic remains: {}", expected.code),
            });
        }
    }
    Ok(CriterionResult {
        name: SuccessCriterion::ExpectedDiagnosticsCleared,
        passed: true,
        detail: String::new(),
    })
}

fn unsafe_line_count(source: &str) -> usize {
    source
        .lines()
        .filter(|line| UNSAFE_LINE_PATTERN.is_match(line))
        .count()
}

/// Accumulate one successful response's usage (§3.7.1).
///
/// A side that a successful response left absent is incomplete for the whole
/// fixture and stays `None`; later responses cannot revive it.
fn accumulate_usage(observations: &mut RunObservations, usage: &Usage) {
    observations.successful_responses = observations.successful_responses.saturating_add(1);
    accumulate_usage_side(
        &mut observations.tokens_in,
        &mut observations.tokens_in_incomplete,
        usage.input_tokens,
    );
    accumulate_usage_side(
        &mut observations.tokens_out,
        &mut observations.tokens_out_incomplete,
        usage.output_tokens,
    );
}

fn accumulate_usage_side(total: &mut Option<u64>, incomplete: &mut bool, value: Option<u64>) {
    match value {
        None => {
            *incomplete = true;
            *total = None;
        }
        Some(value) if !*incomplete => {
            *total = Some(total.unwrap_or(0).saturating_add(value));
        }
        Some(_) => {}
    }
}

/// Derive fixture USD exactly once from the saturated totals (§3.7.1).
///
/// Per-response USD is never summed: repeated rounding would drift from the
/// RFC-0007 formula applied to the totals.
fn finalize_cost(observations: &mut RunObservations, endpoint: &ModelEndpoint) {
    observations.cost_usd = match (
        observations.successful_responses,
        observations.tokens_in,
        observations.tokens_out,
    ) {
        (0, _, _) => None,
        (_, Some(input_tokens), Some(output_tokens)) => derive_eval_usd(
            endpoint,
            &Usage {
                input_tokens: Some(input_tokens),
                output_tokens: Some(output_tokens),
            },
        ),
        _ => None,
    };
}

fn provider_map_error(error: &ProviderError) -> Option<EvalError> {
    match error {
        ProviderError::Internal(message) if message.starts_with(SCRIPTED_MISS_PREFIX) => {
            Some(EvalError::Internal(message.clone()))
        }
        ProviderError::Internal(message) if message == SCRIPTED_WRONG_ENDPOINT => {
            Some(EvalError::Internal(message.clone()))
        }
        _ => None,
    }
}

async fn read_bytes(path: PathBuf) -> Result<Vec<u8>, EvalError> {
    join_blocking(tokio::task::spawn_blocking(move || std::fs::read(path)).await)
}

async fn read_to_string(path: PathBuf) -> Result<String, EvalError> {
    join_blocking(tokio::task::spawn_blocking(move || std::fs::read_to_string(path)).await)
}

fn join_blocking<T>(
    joined: Result<std::io::Result<T>, tokio::task::JoinError>,
) -> Result<T, EvalError> {
    match joined {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(EvalError::Io(error)),
        Err(error) => Err(EvalError::Internal(bound_message(format!(
            "join_failed: {error:?}"
        )))),
    }
}

fn is_cancelled(cancel: &Option<CancellationToken>) -> bool {
    cancel
        .as_ref()
        .map(CancellationToken::is_cancelled)
        .unwrap_or(false)
}

fn maybe_cancel_at_checkpoint(
    _fixture: &LoadedFixture,
    _checkpoint: &'static str,
    _cancel: &Option<CancellationToken>,
) {
    #[cfg(test)]
    if _fixture.cancel_at_checkpoint == Some(_checkpoint) {
        if let Some(token) = _cancel {
            token.cancel();
        }
    }
}

fn cancelled_output(
    fixture: &LoadedFixture,
    started: Instant,
    mut trajectories: Vec<EvalTrajectoryRecord>,
) -> FixtureRunOutput {
    stamp_trajectories(&mut trajectories, FixtureStatus::Error, None);
    FixtureRunOutput {
        outcome: FixtureOutcome {
            fixture_id: fixture.manifest.id.clone(),
            set: fixture.manifest.set,
            status: FixtureStatus::Error,
            criteria: vec![],
            wall_ms: elapsed_ms(started),
            model_calls: saturating_len_u32(trajectories.len()),
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            retry_count: None,
            human_interventions: None,
            unsafe_introduced: None,
            compile_clean: None,
            error: Some(ReportError::cancelled()),
        },
        trajectories,
    }
}

fn error_output(
    fixture: &LoadedFixture,
    started: Instant,
    error: EvalError,
    mut trajectories: Vec<EvalTrajectoryRecord>,
) -> FixtureRunOutput {
    stamp_trajectories(&mut trajectories, FixtureStatus::Error, None);
    FixtureRunOutput {
        outcome: FixtureOutcome {
            fixture_id: fixture.manifest.id.clone(),
            set: fixture.manifest.set,
            status: FixtureStatus::Error,
            criteria: vec![],
            wall_ms: elapsed_ms(started),
            model_calls: saturating_len_u32(trajectories.len()),
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            retry_count: None,
            human_interventions: None,
            unsafe_introduced: None,
            compile_clean: None,
            error: Some(ReportError::from_eval(&error)),
        },
        trajectories,
    }
}

fn stamp_trajectories(
    trajectories: &mut [EvalTrajectoryRecord],
    status: FixtureStatus,
    compile_clean: Option<bool>,
) {
    for row in trajectories {
        row.fixture_status = status;
        row.compile_clean = compile_clean;
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

fn saturating_len_u32(len: usize) -> u32 {
    len.try_into().unwrap_or(u32::MAX)
}

#[cfg(test)]
struct ForceScriptMiss;

#[cfg(test)]
impl ForceScriptMiss {
    fn enable() -> Self {
        FORCE_SCRIPT_MISS.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for ForceScriptMiss {
    fn drop(&mut self) {
        FORCE_SCRIPT_MISS.with(|flag| flag.set(false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::tests::{
        loaded_fixture_for_tests, loaded_fixture_with_outcome, response_outcome,
    };
    use crate::manifest::{ExpectedDiagnostic, FixtureDriverKind, ScriptTurnOutcome};
    use crate::scripted::ScriptedProviderError;
    use alloy_runtime::ErrorClass;

    #[test]
    fn carrier_detail_is_sticky_against_later_criterion() {
        let mut state = CriteriaState::new(&[
            SuccessCriterion::NoNewUnsafe,
            SuccessCriterion::ExpectedDiagnosticsCleared,
        ]);
        state.set_carrier_failure("missing repair text");
        state.set_result(
            0,
            CriterionResult {
                name: SuccessCriterion::NoNewUnsafe,
                passed: true,
                detail: String::new(),
            },
        );
        assert!(!state.results[0].passed);
        assert_eq!(state.results[0].detail, "missing repair text");

        // A non-carrier criterion still records its own result.
        state.set_result(
            1,
            CriterionResult {
                name: SuccessCriterion::ExpectedDiagnosticsCleared,
                passed: true,
                detail: String::new(),
            },
        );
        assert!(state.results[1].passed);
    }

    #[test]
    fn no_new_unsafe_is_line_scoped() {
        assert_eq!(unsafe_line_count("unsafe\n"), 0);
        assert_eq!(unsafe_line_count("unsafe ()\nunsafe!();\nmyunsafe();\n"), 2);
        // Only the line carrying `unsafe` counts, not the whole source.
        assert_eq!(
            unsafe_line_count("fn a() {}\nfn b() { unsafe { } }\nfn c() {}\n"),
            1
        );
        assert_eq!(unsafe_line_count(""), 0);
    }

    #[tokio::test]
    async fn cancel_before_install_returns_cancelled_without_rows() {
        let fixture = loaded_fixture_for_tests("cancel", FixtureDriverKind::SkeletonReplay);
        let provider = fixture.scripts.as_ref().unwrap().clone();
        let token = CancellationToken::new();
        token.cancel();
        let output = run(&fixture, provider, Some(token)).await;
        assert_eq!(output.outcome.error.unwrap().kind, "cancelled");
        assert_eq!(output.outcome.model_calls, 0);
        assert!(output.trajectories.is_empty());
    }

    #[tokio::test]
    async fn cancel_after_dispatch_retains_exact_row() {
        let fixture = loaded_fixture_for_tests("dispatch", FixtureDriverKind::SkeletonReplay);
        let provider = fixture.scripts.as_ref().unwrap().clone();
        let token = CancellationToken::new();
        token.cancel();
        let mut trajectories = Vec::new();
        let turn = &fixture.manifest.turns[0];

        let dispatched = dispatch_turn(
            &provider,
            &fixture,
            &turn.turn_id,
            turn.request.clone(),
            &Some(token),
            &mut trajectories,
        )
        .await;

        assert!(dispatched.is_none());
        assert_eq!(trajectories.len(), 1);
        assert_eq!(trajectories[0].error_class, Some(ErrorClass::Cancelled));
        assert!(!trajectories[0].complete_ok);
    }

    fn assert_cancelled_output(output: &FixtureRunOutput, model_calls: u32, trajectories: usize) {
        assert_eq!(output.outcome.status, FixtureStatus::Error);
        assert_eq!(output.outcome.error.as_ref().unwrap().kind, "cancelled");
        assert_eq!(output.outcome.model_calls, model_calls);
        assert_eq!(output.outcome.compile_clean, None);
        assert!(output.outcome.criteria.is_empty());
        assert_eq!(output.trajectories.len(), trajectories);
    }

    #[tokio::test]
    async fn cancel_checkpoints_cover_driver() {
        let fixture =
            loaded_fixture_for_tests("cancel-checkpoints", FixtureDriverKind::SkeletonReplay);
        let token = CancellationToken::new();
        token.cancel();
        let output = run(
            &fixture,
            fixture.scripts.as_ref().unwrap().clone(),
            Some(token),
        )
        .await;
        assert_cancelled_output(&output, 0, 0);

        let mut fixture =
            loaded_fixture_for_tests("cancel-complete", FixtureDriverKind::SkeletonReplay);
        fixture.cancel_at_checkpoint = Some(CANCEL_BEFORE_COMPLETE);
        let provider = fixture.scripts.as_ref().unwrap().clone();
        let token = CancellationToken::new();
        let output = run(&fixture, provider, Some(token)).await;
        assert_cancelled_output(&output, 1, 1);
        assert_eq!(
            output.trajectories[0].error_class,
            Some(ErrorClass::Cancelled)
        );
        assert!(!output.trajectories[0].complete_ok);

        let mut fixture =
            loaded_fixture_for_tests("cancel-before-patch", FixtureDriverKind::SkeletonReplay);
        fixture.cancel_at_checkpoint = Some(CANCEL_BEFORE_PATCH);
        let provider = fixture.scripts.as_ref().unwrap().clone();
        let output = run(&fixture, provider, Some(CancellationToken::new())).await;
        assert_cancelled_output(&output, 1, 1);
        assert!(output.trajectories[0].complete_ok);

        let dir = tempfile::tempdir().unwrap();
        let golden = dir.path().join("lib.rs.post");
        std::fs::write(&golden, "fixed").unwrap();
        let mut fixture =
            loaded_fixture_for_tests("cancel-before-criteria", FixtureDriverKind::SkeletonReplay);
        fixture.paths.golden = golden;
        fixture.cancel_at_checkpoint = Some(CANCEL_BEFORE_CRITERIA);
        let provider = fixture.scripts.as_ref().unwrap().clone();
        let output = run(&fixture, provider, Some(CancellationToken::new())).await;
        assert_cancelled_output(&output, 1, 1);
        assert!(output.trajectories[0].complete_ok);
    }

    #[tokio::test]
    async fn wrong_driver_request_is_script_miss_fail() {
        let fixture = loaded_fixture_for_tests("script-miss", FixtureDriverKind::SkeletonReplay);
        let provider = fixture.scripts.as_ref().unwrap().clone();
        let _guard = ForceScriptMiss::enable();

        let output = run(&fixture, provider, None).await;

        assert_eq!(output.outcome.status, FixtureStatus::Fail);
        assert_eq!(output.outcome.criteria[0].detail, SCRIPT_MISS_DETAIL);
        assert!(!output.outcome.criteria[0].passed);
        // The provider was never called for the mismatched turn.
        assert_eq!(output.outcome.model_calls, 0);
        assert!(output.trajectories.is_empty());
        assert!(output.outcome.error.is_none());
    }

    #[tokio::test]
    async fn correct_request_missing_map_is_error() {
        let mut fixture = loaded_fixture_for_tests("map-desync", FixtureDriverKind::SkeletonReplay);
        fixture.script_entries.clear();
        let provider = fixture.scripts.as_ref().unwrap().clone();

        let output = run(&fixture, provider, None).await;

        assert_eq!(output.outcome.status, FixtureStatus::Error);
        assert!(output.outcome.criteria.is_empty());
        assert_eq!(output.outcome.model_calls, 1);
        assert_eq!(output.outcome.compile_clean, None);
        let error = output.outcome.error.unwrap();
        assert_eq!(error.kind, "internal");
        assert!(error.message.contains(SCRIPTED_MISS_PREFIX), "{error:?}");
    }

    #[tokio::test]
    async fn script_miss_carrier_prefers_compile_clean() {
        let mut with_compile =
            loaded_fixture_for_tests("carrier-compile", FixtureDriverKind::SkeletonReplay);
        with_compile.manifest.success_criteria = vec![
            SuccessCriterion::ScriptTurnsConsumed,
            SuccessCriterion::CompileClean,
        ];
        let provider = with_compile.scripts.as_ref().unwrap().clone();
        let _guard = ForceScriptMiss::enable();
        let output = run(&with_compile, provider, None).await;
        assert_eq!(
            output.outcome.criteria[1].name,
            SuccessCriterion::CompileClean
        );
        assert_eq!(output.outcome.criteria[1].detail, SCRIPT_MISS_DETAIL);
        assert_eq!(
            output.outcome.criteria[0].detail,
            "unconsumed script keys: 1"
        );
        drop(_guard);

        let mut without_compile =
            loaded_fixture_for_tests("carrier-first", FixtureDriverKind::SkeletonReplay);
        without_compile.manifest.success_criteria =
            vec![SuccessCriterion::ExpectedDiagnosticsCleared];
        let provider = without_compile.scripts.as_ref().unwrap().clone();
        let _guard = ForceScriptMiss::enable();
        let output = run(&without_compile, provider, None).await;
        assert_eq!(
            output.outcome.criteria[0].name,
            SuccessCriterion::ExpectedDiagnosticsCleared
        );
        assert_eq!(output.outcome.criteria[0].detail, SCRIPT_MISS_DETAIL);
    }

    #[tokio::test]
    async fn provider_error_before_repair_fails() {
        let fixture = loaded_fixture_with_outcome(
            "provider-error",
            FixtureDriverKind::SkeletonReplay,
            ScriptTurnOutcome::Error {
                error: ScriptedProviderError::RateLimit,
            },
        );
        let provider = fixture.scripts.as_ref().unwrap().clone();

        let output = run(&fixture, provider, None).await;

        assert_eq!(output.outcome.status, FixtureStatus::Fail);
        assert_eq!(
            output.outcome.criteria[0].detail,
            "provider error before repair text"
        );
        assert_eq!(output.outcome.compile_clean, Some(false));
        assert_eq!(output.outcome.model_calls, 1);
        assert!(output.outcome.error.is_none());
    }

    #[tokio::test]
    async fn provider_error_before_repair_sets_compile_false() {
        let fixture = loaded_fixture_with_outcome(
            "ordinary-provider-error",
            FixtureDriverKind::SkeletonReplay,
            ScriptTurnOutcome::Error {
                error: ScriptedProviderError::Timeout,
            },
        );
        let output = run(&fixture, fixture.scripts.as_ref().unwrap().clone(), None).await;
        assert_eq!(output.outcome.status, FixtureStatus::Fail);
        assert_eq!(output.outcome.compile_clean, Some(false));
        assert_eq!(
            output.outcome.criteria[0].detail,
            "provider error before repair text"
        );

        let fixture = loaded_fixture_for_tests("wrong-endpoint", FixtureDriverKind::SkeletonReplay);
        let mut other_endpoint = fixture.endpoint.clone();
        other_endpoint.id = alloy_runtime::EndpointId::new("other").unwrap();
        let provider = Arc::new(
            ScriptedProvider::new(other_endpoint.provider.clone(), other_endpoint).unwrap(),
        );
        let output = run(&fixture, provider, None).await;
        assert_eq!(output.outcome.status, FixtureStatus::Error);
        assert_eq!(output.outcome.compile_clean, None);
        assert!(output.outcome.criteria.is_empty());
    }

    #[tokio::test]
    async fn missing_repair_text_fails() {
        let fixture = loaded_fixture_with_outcome(
            "missing-text",
            FixtureDriverKind::SkeletonReplay,
            response_outcome(None),
        );
        let provider = fixture.scripts.as_ref().unwrap().clone();

        let output = run(&fixture, provider, None).await;

        assert_eq!(output.outcome.status, FixtureStatus::Fail);
        assert_eq!(output.outcome.criteria[0].detail, "missing repair text");
        assert_eq!(output.outcome.compile_clean, Some(false));
        assert!(output.outcome.error.is_none());
    }

    #[tokio::test]
    async fn trailing_provider_error_keeps_repair_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let golden = dir.path().join("lib.rs.post");
        std::fs::write(&golden, "fixed").unwrap();
        let mut fixture =
            loaded_fixture_for_tests("trailing-error", FixtureDriverKind::SkeletonReplay);
        fixture.paths.golden = golden;
        let mut trailing = fixture.manifest.turns[0].clone();
        trailing.turn_id.ordinal = 1;
        trailing.request.max_output_tokens = Some(1);
        trailing.outcome = ScriptTurnOutcome::Error {
            error: ScriptedProviderError::RateLimit,
        };
        fixture.manifest.turns.push(trailing.clone());
        fixture.script_entries.push((
            RequestFingerprint::of(&trailing.request),
            ScriptOutcome::from(trailing.outcome),
        ));
        let provider = fixture.scripts.as_ref().unwrap().clone();

        let output = run(&fixture, provider, None).await;

        assert_eq!(
            output.outcome.status,
            FixtureStatus::Pass,
            "{:?}",
            output.outcome
        );
        assert_eq!(output.outcome.compile_clean, Some(true));
        assert_eq!(output.outcome.model_calls, 2);
        assert!(output.outcome.criteria[0].passed);
    }

    #[test]
    fn expected_diagnostics_cleared() {
        let mut fixture =
            loaded_fixture_for_tests("diagnostics", FixtureDriverKind::SkeletonReplay);

        let passed = evaluate_expected_diagnostics_cleared(&fixture, true).unwrap();
        assert!(passed.passed);
        assert_eq!(passed.detail, "");

        let patch_failed = evaluate_expected_diagnostics_cleared(&fixture, false).unwrap();
        assert!(!patch_failed.passed);
        assert_eq!(
            patch_failed.detail,
            "patch oracle failed; diagnostics not attributable"
        );

        fixture.post_repair.stdout_lines = vec![serde_json::json!({
            "reason": "compiler-message",
            "message": {
                "level": "error",
                "message": "still broken",
                "code": { "code": "E0001" }
            }
        })
        .to_string()];
        let remains = evaluate_expected_diagnostics_cleared(&fixture, true).unwrap();
        assert!(!remains.passed);
        assert_eq!(remains.detail, "expected diagnostic remains: E0001");
    }

    #[tokio::test]
    async fn criteria_exactly_manifest_list() {
        let dir = tempfile::tempdir().unwrap();
        let golden = dir.path().join("lib.rs.post");
        std::fs::write(&golden, "fixed").unwrap();
        let mut fixture =
            loaded_fixture_for_tests("criteria-list", FixtureDriverKind::SkeletonReplay);
        fixture.paths.golden = golden;
        fixture.manifest.success_criteria = vec![
            SuccessCriterion::CompileClean,
            SuccessCriterion::ScriptTurnsConsumed,
        ];
        let expected = fixture.manifest.success_criteria.clone();
        let provider = fixture.scripts.as_ref().unwrap().clone();

        let output = run(&fixture, provider, None).await;

        let actual: Vec<_> = output
            .outcome
            .criteria
            .iter()
            .map(|criterion| criterion.name)
            .collect();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn script_turns_consumed_skeleton() {
        let dir = tempfile::tempdir().unwrap();
        let golden = dir.path().join("lib.rs.post");
        std::fs::write(&golden, "fixed").unwrap();
        let mut fixture =
            loaded_fixture_for_tests("turns-consumed", FixtureDriverKind::SkeletonReplay);
        fixture.paths.golden = golden;
        fixture.manifest.success_criteria = vec![SuccessCriterion::ScriptTurnsConsumed];
        let provider = fixture.scripts.as_ref().unwrap().clone();

        let output = run(&fixture, Arc::clone(&provider), None).await;

        assert_eq!(output.outcome.status, FixtureStatus::Pass);
        assert_eq!(output.outcome.criteria.len(), 1);
        assert_eq!(
            output.outcome.criteria[0].name,
            SuccessCriterion::ScriptTurnsConsumed
        );
        assert!(output.outcome.criteria[0].passed);
        assert!(provider.is_exhausted());
    }

    #[tokio::test]
    async fn script_turns_consumed_naive_installed_only() {
        let dir = tempfile::tempdir().unwrap();
        let golden = dir.path().join("lib.rs.post");
        std::fs::write(&golden, "fixed").unwrap();
        let mut fixture =
            loaded_fixture_for_tests("naive-consumption", FixtureDriverKind::NaiveBaseline);
        fixture.paths.golden = golden;
        fixture.manifest.success_criteria = vec![SuccessCriterion::ScriptTurnsConsumed];
        let mut control_only = fixture.manifest.turns[0].clone();
        control_only.turn_id.capability = alloy_runtime::CapabilityId::new("review").unwrap();
        control_only.turn_id.ordinal = 1;
        control_only.request.max_output_tokens = Some(99);
        control_only.outcome = response_outcome(Some("control-only"));
        fixture.manifest.turns.push(control_only.clone());
        fixture.script_entries.push((
            RequestFingerprint::of(&control_only.request),
            ScriptOutcome::from(control_only.outcome),
        ));
        let provider = fixture.scripts.as_ref().unwrap().clone();

        let output = run_scripted(
            &fixture,
            Arc::clone(&provider),
            None,
            ScriptedDriverMode::NaiveBaseline,
        )
        .await;

        assert_eq!(output.outcome.status, FixtureStatus::Pass);
        assert!(output.outcome.criteria[0].passed);
        assert_eq!(output.outcome.criteria[0].detail, "");
        assert!(provider.is_exhausted());
        assert_eq!(provider.recorded().len(), 1);
    }

    #[tokio::test]
    async fn narrow_driver_still_records_compile() {
        let dir = tempfile::tempdir().unwrap();
        let golden = dir.path().join("lib.rs.post");
        std::fs::write(&golden, "fixed").unwrap();

        let mut skeleton =
            loaded_fixture_for_tests("narrow-skeleton", FixtureDriverKind::SkeletonReplay);
        skeleton.paths.golden = golden.clone();
        skeleton.manifest.success_criteria = vec![SuccessCriterion::ScriptTurnsConsumed];
        let output = run(&skeleton, skeleton.scripts.as_ref().unwrap().clone(), None).await;
        assert_eq!(output.outcome.compile_clean, Some(true));
        assert_eq!(
            output.outcome.criteria[0].name,
            SuccessCriterion::ScriptTurnsConsumed
        );

        let mut naive = loaded_fixture_for_tests("narrow-naive", FixtureDriverKind::NaiveBaseline);
        naive.paths.golden = golden;
        naive.manifest.success_criteria = vec![SuccessCriterion::ScriptTurnsConsumed];
        let output = run_scripted(
            &naive,
            naive.scripts.as_ref().unwrap().clone(),
            None,
            ScriptedDriverMode::NaiveBaseline,
        )
        .await;
        assert_eq!(output.outcome.compile_clean, Some(true));
        assert_eq!(
            output.outcome.criteria[0].name,
            SuccessCriterion::ScriptTurnsConsumed
        );
    }

    #[test]
    fn naive_selects_unique_repair_zero() {
        let mut fixture = loaded_fixture_for_tests("naive-plan", FixtureDriverKind::NaiveBaseline);
        let selected = fixture.manifest.turns[0].clone();
        let mut ignored = selected.clone();
        ignored.turn_id.capability = alloy_runtime::CapabilityId::new("review").unwrap();
        ignored.turn_id.ordinal = 1;
        fixture.manifest.turns.push(ignored);

        let plan = execution_plan(&fixture, ScriptedDriverMode::NaiveBaseline).unwrap();
        assert_eq!(plan.turns, vec![selected.clone()]);
        assert_eq!(plan.entries.len(), 1);

        fixture.manifest.turns.push(selected);
        assert!(matches!(
            execution_plan(&fixture, ScriptedDriverMode::NaiveBaseline),
            Err(EvalError::Manifest(message))
                if message == "exactly one repair ordinal 0 turn is required"
        ));
    }

    #[tokio::test]
    async fn patch_mismatch_fails_compile() {
        let dir = tempfile::tempdir().unwrap();
        let golden = dir.path().join("lib.rs.post");
        std::fs::write(&golden, "not the candidate\n").unwrap();
        let mut fixture =
            loaded_fixture_for_tests("patch-mismatch", FixtureDriverKind::SkeletonReplay);
        fixture.paths.golden = golden;
        let provider = fixture.scripts.as_ref().unwrap().clone();

        let output = run(&fixture, provider, None).await;

        assert_eq!(output.outcome.status, FixtureStatus::Fail);
        assert_eq!(output.outcome.criteria[0].detail, "patch oracle failed");
        assert_eq!(output.outcome.compile_clean, Some(false));
        assert!(output.outcome.error.is_none());
    }

    #[test]
    fn no_new_unsafe_exact_regex() {
        assert_eq!(UNSAFE_LINE_PATTERN.as_str(), r"(^|\s)unsafe(\s|!|\(|\{)");
        assert_eq!(unsafe_line_count("unsafe \n"), 1);
        assert_eq!(unsafe_line_count(" unsafe! {}\n"), 1);
        assert_eq!(unsafe_line_count("\tunsafe(foo)\n"), 1);
        assert_eq!(unsafe_line_count("unsafe{\n"), 1);
        assert_eq!(unsafe_line_count("myunsafe();\n"), 0);
        assert_eq!(unsafe_line_count("unsafe\n"), 0);
        assert_eq!(unsafe_line_count("safe\nunsafe \nunsafe!(); unsafe()"), 2);
        assert_eq!(unsafe_line_count("// unsafe block\n\" unsafe! \"\n"), 2);
        assert_eq!(
            unsafe_line_count("unsafe \r\n"),
            unsafe_line_count("unsafe \n")
        );
        assert!(unsafe_line_count("unsafe!()") > unsafe_line_count("safe()"));
    }

    #[tokio::test]
    async fn no_new_unsafe_uses_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("lib.rs");
        let golden = dir.path().join("lib.rs.post");
        std::fs::write(&target, "fn safe() {}\n").unwrap();
        std::fs::write(&golden, "fn golden_is_safe() {}\n").unwrap();
        let mut fixture =
            loaded_fixture_for_tests("unsafe-candidate", FixtureDriverKind::SkeletonReplay);
        fixture.paths.target = target;
        fixture.paths.golden = golden;

        let (introduced, result) =
            evaluate_no_new_unsafe(&fixture, Some("fn changed() { unsafe { work(); } }\n"))
                .await
                .unwrap();

        assert!(introduced);
        assert!(!result.passed);
        assert_eq!(result.detail, "unsafe introduced");
    }

    #[tokio::test]
    async fn criterion_detail_strings_are_exact() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("lib.rs");
        let golden = dir.path().join("lib.rs.post");
        std::fs::write(&target, "fn safe() {}\n").unwrap();
        std::fs::write(&golden, "fixed").unwrap();
        let mut fixture =
            loaded_fixture_for_tests("criterion-details", FixtureDriverKind::SkeletonReplay);
        fixture.paths.target = target;
        fixture.paths.golden = golden;

        let (_, safe) = evaluate_no_new_unsafe(&fixture, Some("fn safe() {}\n"))
            .await
            .unwrap();
        assert!(safe.passed);
        assert_eq!(safe.detail, "");
        let (_, unsafe_result) =
            evaluate_no_new_unsafe(&fixture, Some("fn f() { unsafe { f(); } }\n"))
                .await
                .unwrap();
        assert!(!unsafe_result.passed);
        assert_eq!(unsafe_result.detail, "unsafe introduced");

        fixture
            .manifest
            .expected_diagnostics
            .push(ExpectedDiagnostic {
                code: "E0002".to_owned(),
                message_contains: "second".to_owned(),
            });
        fixture.post_repair.stdout_lines = vec![
            serde_json::json!({
                "reason": "compiler-message",
                "message": {
                    "level": "error",
                    "message": "second remains",
                    "code": { "code": "E0002" }
                }
            })
            .to_string(),
            serde_json::json!({
                "reason": "compiler-message",
                "message": {
                    "level": "error",
                    "message": "first remains",
                    "code": { "code": "E0001" }
                }
            })
            .to_string(),
        ];
        let diagnostics = evaluate_expected_diagnostics_cleared(&fixture, true).unwrap();
        assert_eq!(diagnostics.detail, "expected diagnostic remains: E0001");
        let unattributable = evaluate_expected_diagnostics_cleared(&fixture, false).unwrap();
        assert_eq!(
            unattributable.detail,
            "patch oracle failed; diagnostics not attributable"
        );

        fixture.post_repair.stdout_lines.clear();
        fixture.manifest.success_criteria = vec![SuccessCriterion::ScriptTurnsConsumed];
        let mut extra_request = fixture.manifest.turns[0].request.clone();
        extra_request.max_output_tokens = Some(42);
        fixture.script_entries.push((
            RequestFingerprint::of(&extra_request),
            ScriptOutcome::from(response_outcome(Some("unused"))),
        ));
        let output = run(&fixture, fixture.scripts.as_ref().unwrap().clone(), None).await;
        assert!(!output.outcome.criteria[0].passed);
        assert_eq!(
            output.outcome.criteria[0].detail,
            "unconsumed script keys: 1"
        );
    }

    #[tokio::test]
    async fn outcome_usage_accounting() {
        // No successful response: both token sides and cost stay absent.
        let fixture = loaded_fixture_with_outcome(
            "no-success",
            FixtureDriverKind::SkeletonReplay,
            ScriptTurnOutcome::Error {
                error: ScriptedProviderError::RateLimit,
            },
        );
        let provider = fixture.scripts.as_ref().unwrap().clone();
        let output = run(&fixture, provider, None).await;
        assert_eq!(output.outcome.tokens_in, None);
        assert_eq!(output.outcome.tokens_out, None);
        assert_eq!(output.outcome.cost_usd, None);

        // One success with complete usage: totals present, cost derived once
        // from the totals when the endpoint carries both prices.
        let dir = tempfile::tempdir().unwrap();
        let golden = dir.path().join("lib.rs.post");
        std::fs::write(&golden, "fixed").unwrap();
        let mut fixture = loaded_fixture_for_tests("usage", FixtureDriverKind::SkeletonReplay);
        fixture.paths.golden = golden;
        fixture.endpoint.input_usd_per_mtok = Some(2.0);
        fixture.endpoint.output_usd_per_mtok = Some(4.0);
        let provider = fixture.scripts.as_ref().unwrap().clone();
        let output = run(&fixture, provider, None).await;
        assert_eq!(output.outcome.tokens_in, Some(1));
        assert_eq!(output.outcome.tokens_out, Some(2));
        assert_eq!(
            output.outcome.cost_usd,
            Some(2.0 / 1_000_000.0 + 8.0 / 1_000_000.0)
        );

        // A successful response missing one side voids that side, and with it
        // the cost, even though the other side is complete.
        let mut fixture =
            loaded_fixture_with_outcome("half-usage", FixtureDriverKind::SkeletonReplay, {
                ScriptTurnOutcome::Response {
                    text: Some("fixed".to_owned()),
                    structured: None,
                    usage: Usage {
                        input_tokens: Some(7),
                        output_tokens: None,
                    },
                    provider_request_id: None,
                    finish_reason: None,
                }
            });
        fixture.paths.golden = dir.path().join("lib.rs.post");
        fixture.endpoint.input_usd_per_mtok = Some(2.0);
        fixture.endpoint.output_usd_per_mtok = Some(4.0);
        let provider = fixture.scripts.as_ref().unwrap().clone();
        let output = run(&fixture, provider, None).await;
        assert_eq!(output.outcome.tokens_in, Some(7));
        assert_eq!(output.outcome.tokens_out, None);
        assert_eq!(output.outcome.cost_usd, None);
    }
}
