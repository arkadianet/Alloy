use std::sync::Arc;
use std::time::Instant;

use alloy_runtime::{ModelProvider, ProviderError};
use regex::Regex;
use tokio_util::sync::CancellationToken;

use crate::cost_claim::derive_eval_usd;
use crate::error::{EvalError, ReportError};
use crate::fingerprint::RequestFingerprint;
use crate::harness::{FixtureRunOutput, LoadedFixture};
use crate::manifest::{ScriptTurn, SuccessCriterion};
use crate::report::{CriterionResult, FixtureOutcome, FixtureStatus};
use crate::scripted::{ScriptOutcome, ScriptedProvider};
use crate::trajectory::EvalTrajectoryRecord;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScriptedDriverMode {
    SkeletonReplay,
    NaiveBaseline,
}

struct CriteriaState {
    results: Vec<CriterionResult>,
    carrier: usize,
}

struct RunObservations {
    tokens_in: Option<u64>,
    tokens_out: Option<u64>,
    cost_usd: Option<f64>,
    compile_clean: Option<bool>,
    unsafe_introduced: Option<bool>,
    patch_passed: bool,
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
    let mut observations = RunObservations {
        tokens_in: Some(0),
        tokens_out: Some(0),
        cost_usd: Some(0.0),
        compile_clean: None,
        unsafe_introduced: None,
        patch_passed: false,
    };

    for turn in &plan.turns {
        if is_cancelled(&cancel) {
            return cancelled_output(fixture, started, trajectories);
        }

        let request = turn.request.clone();
        if request != turn.request {
            criteria.set_carrier_failure("script miss");
            break;
        }

        if is_cancelled(&cancel) {
            return cancelled_output(fixture, started, trajectories);
        }

        let request_fingerprint = RequestFingerprint::of(&request);
        let attempt_started = Instant::now();
        let complete = provider.complete(&fixture.endpoint, request);
        let complete_result = if let Some(token) = &cancel {
            tokio::select! {
                biased;
                _ = token.cancelled() => {
                    let duration_ms = elapsed_ms(attempt_started);
                    trajectories.push(EvalTrajectoryRecord::cancelled(
                        fixture.manifest.id.clone(),
                        fixture.manifest.set,
                        turn.turn_id.clone(),
                        request_fingerprint,
                        &fixture.endpoint,
                        Some(duration_ms),
                        FixtureStatus::Error,
                        None,
                    ));
                    return cancelled_output(fixture, started, trajectories);
                }
                result = complete => result,
            }
        } else {
            complete.await
        };
        let duration_ms = elapsed_ms(attempt_started);

        match complete_result {
            Ok(response) => {
                trajectories.push(EvalTrajectoryRecord::from_response(
                    fixture.manifest.id.clone(),
                    fixture.manifest.set,
                    turn.turn_id.clone(),
                    request_fingerprint,
                    &fixture.endpoint,
                    &response,
                    Some(duration_ms),
                    FixtureStatus::Error,
                    None,
                ));
                accumulate_usage(&mut observations, &response.usage);
                accumulate_cost(
                    &mut observations,
                    derive_eval_usd(&fixture.endpoint, &response.usage),
                );
                if let Some(text) = response.text {
                    candidate = Some(text);
                }
            }
            Err(error) => {
                trajectories.push(EvalTrajectoryRecord::from_provider_error(
                    fixture.manifest.id.clone(),
                    fixture.manifest.set,
                    turn.turn_id.clone(),
                    request_fingerprint,
                    &fixture.endpoint,
                    &error,
                    Some(duration_ms),
                    FixtureStatus::Error,
                    None,
                ));
                if let Some(error) = provider_map_error(&error) {
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

    if candidate.is_none() {
        observations.compile_clean = Some(false);
        criteria.set_carrier_failure("missing repair text");
    }

    if is_cancelled(&cancel) {
        return cancelled_output(fixture, started, trajectories);
    }

    if let Some(candidate) = candidate.as_deref() {
        match std::fs::read(&fixture.paths.golden) {
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
            Err(error) => {
                return error_output(fixture, started, EvalError::Io(error), trajectories)
            }
        }
    }

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
                match evaluate_no_new_unsafe(fixture, candidate.as_deref()) {
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

struct ExecutionPlan {
    turns: Vec<ScriptTurn>,
    entries: Vec<(RequestFingerprint, ScriptOutcome)>,
}

fn execution_plan(
    fixture: &LoadedFixture,
    mode: ScriptedDriverMode,
) -> Result<ExecutionPlan, EvalError> {
    match mode {
        ScriptedDriverMode::SkeletonReplay => Ok(ExecutionPlan {
            turns: fixture.manifest.turns.clone(),
            entries: fixture.script_entries.clone(),
        }),
        ScriptedDriverMode::NaiveBaseline => {
            let mut matches = fixture.manifest.turns.iter().filter(|turn| {
                turn.turn_id.capability.as_str() == "repair" && turn.turn_id.ordinal == 0
            });
            let Some(turn) = matches.next() else {
                return Err(EvalError::Manifest(
                    "exactly one repair ordinal 0 turn is required".to_owned(),
                ));
            };
            if matches.next().is_some() {
                return Err(EvalError::Manifest(
                    "exactly one repair ordinal 0 turn is required".to_owned(),
                ));
            }
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

fn evaluate_no_new_unsafe(
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
    let pre_source = std::fs::read_to_string(&fixture.paths.target)?;
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
    let regex = Regex::new(r"(?m)(^|\s)unsafe(\s|!|\()").expect("unsafe regex is valid");
    source.lines().filter(|line| regex.is_match(line)).count()
}

fn accumulate_usage(observations: &mut RunObservations, usage: &alloy_runtime::Usage) {
    observations.tokens_in = match (observations.tokens_in, usage.input_tokens) {
        (Some(total), Some(value)) => Some(total.saturating_add(value)),
        _ => None,
    };
    observations.tokens_out = match (observations.tokens_out, usage.output_tokens) {
        (Some(total), Some(value)) => Some(total.saturating_add(value)),
        _ => None,
    };
}

fn accumulate_cost(observations: &mut RunObservations, usd: Option<f64>) {
    observations.cost_usd = match (observations.cost_usd, usd) {
        (Some(total), Some(value)) if value.is_finite() => Some(total + value),
        _ => None,
    };
}

fn provider_map_error(error: &ProviderError) -> Option<EvalError> {
    match error {
        ProviderError::Internal(message) if message.starts_with("scripted miss:") => {
            Some(EvalError::Internal(message.clone()))
        }
        ProviderError::Internal(message) if message == "scripted wrong endpoint" => {
            Some(EvalError::Internal(message.clone()))
        }
        _ => None,
    }
}

fn is_cancelled(cancel: &Option<CancellationToken>) -> bool {
    cancel
        .as_ref()
        .map(CancellationToken::is_cancelled)
        .unwrap_or(false)
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
mod tests {
    use super::*;
    use crate::harness::tests::loaded_fixture_for_tests;
    use crate::manifest::FixtureDriverKind;

    #[test]
    fn carrier_detail_is_sticky() {
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
    }

    #[test]
    fn unsafe_count_is_line_based() {
        assert_eq!(unsafe_line_count("unsafe\n"), 0);
        assert_eq!(unsafe_line_count("unsafe ()\nunsafe!();\nmyunsafe();\n"), 2);
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
}
