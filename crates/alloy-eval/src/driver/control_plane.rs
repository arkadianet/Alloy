//! Control-plane driver for RFC-0016 holdout control runs.
//!
//! Default: scripted replay of every manifest turn via [`ScriptedProvider`]
//! with the golden byte oracle ([`super::skeleton`]).
//!
//! Live stack path (scheduler / Landlock / MCP / EditEngine /
//! GenerationDriver) requires **both** `--features stack-driver` and
//! `ALLOY_EVAL_LIVE_STACK=1`. Without the env flag, the feature only compiles
//! the live module so clippy/CI can check it; holdout goldens stay scripted.
//! Golden-derived live runs are integration smoke, not thesis evidence
//! (RFC-0016 §5.9 / Appendix B).
//!
//! Author: arkadianet

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::driver::skeleton::{run_scripted, ScriptedDriverMode};
use crate::harness::{FixtureRunOutput, LoadedFixture};
use crate::scripted::ScriptedProvider;

pub(crate) async fn run(
    fixture: &LoadedFixture,
    provider: Arc<ScriptedProvider>,
    cancel: Option<CancellationToken>,
) -> FixtureRunOutput {
    #[cfg(feature = "stack-driver")]
    if crate::driver::stack::live_stack_requested() {
        let _ = provider;
        return crate::driver::stack::run_live(fixture, cancel).await;
    }
    run_scripted(fixture, provider, cancel, ScriptedDriverMode::ControlPlane).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::skeleton::{run_scripted, ScriptedDriverMode};
    use crate::fingerprint::RequestFingerprint;
    use crate::harness::tests::{loaded_fixture_for_tests, response_outcome};
    use crate::manifest::{FixtureDriverKind, ScriptTurnOutcome};
    use crate::report::FixtureStatus;
    use crate::scripted::ScriptOutcome;
    use alloy_runtime::Usage;

    /// Offline scripted semantics — exercised directly so a live-stack env
    /// cannot rewrite these unit tests onto the Landlock path.
    async fn run_offline(
        fixture: &LoadedFixture,
        provider: Arc<ScriptedProvider>,
    ) -> FixtureRunOutput {
        run_scripted(fixture, provider, None, ScriptedDriverMode::ControlPlane).await
    }

    #[tokio::test]
    async fn control_plane_replays_all_turns_offline() {
        let dir = tempfile::tempdir().unwrap();
        let golden = dir.path().join("lib.rs.post");
        std::fs::write(&golden, "fixed").unwrap();
        let mut fixture =
            loaded_fixture_for_tests("control-plane", FixtureDriverKind::ControlPlane);
        fixture.paths.golden = golden;
        let provider = fixture.scripts.as_ref().unwrap().clone();

        let output = run_offline(&fixture, provider).await;

        assert_eq!(
            output.outcome.status,
            FixtureStatus::Pass,
            "{:?}",
            output.outcome
        );
        assert_eq!(output.outcome.model_calls, 1);
        assert!(output.outcome.error.is_none());
        assert_eq!(output.trajectories.len(), 1);
        assert!(output.trajectories[0].complete_ok);
    }

    /// A review turn with no repair text must still be dispatched and consumed:
    /// the control plane installs and replays every manifest turn, unlike the
    /// naive baseline, and a `text = None` success does not replace the repair
    /// candidate.
    #[tokio::test]
    async fn control_plane_consumes_every_manifest_turn() {
        let dir = tempfile::tempdir().unwrap();
        let golden = dir.path().join("lib.rs.post");
        std::fs::write(&golden, "fixed").unwrap();
        let mut fixture = loaded_fixture_for_tests("multi-turn", FixtureDriverKind::ControlPlane);
        fixture.paths.golden = golden;
        let mut review = fixture.manifest.turns[0].clone();
        review.turn_id.capability = alloy_runtime::CapabilityId::new("review").unwrap();
        review.request.max_output_tokens = Some(8);
        review.outcome = ScriptTurnOutcome::Response {
            text: None,
            structured: Some(serde_json::json!({ "verdict": "clean" })),
            usage: Usage {
                input_tokens: Some(4),
                output_tokens: Some(1),
            },
            provider_request_id: None,
            finish_reason: None,
        };
        fixture.manifest.turns.push(review.clone());
        fixture.script_entries.push((
            RequestFingerprint::of(&review.request),
            ScriptOutcome::from(review.outcome),
        ));
        let provider = fixture.scripts.as_ref().unwrap().clone();

        let output = run_offline(&fixture, Arc::clone(&provider)).await;

        assert_eq!(
            output.outcome.status,
            FixtureStatus::Pass,
            "{:?}",
            output.outcome
        );
        assert_eq!(output.outcome.model_calls, 2);
        assert_eq!(output.outcome.tokens_in, Some(5));
        assert_eq!(output.outcome.tokens_out, Some(3));
        assert_eq!(output.outcome.compile_clean, Some(true));
        assert!(output
            .outcome
            .criteria
            .iter()
            .all(|criterion| criterion.passed));
        assert_eq!(output.trajectories.len(), 2);
        assert_eq!(output.trajectories[1].turn_id.render(), "review/-/0");
        assert!(provider.is_exhausted());
    }

    /// Two turns sharing one request fingerprint consume their FIFO outcomes
    /// in declaration order; the last successful text is the repair candidate.
    #[tokio::test]
    async fn control_plane_retries_same_fingerprint_fifo() {
        let dir = tempfile::tempdir().unwrap();
        let golden = dir.path().join("lib.rs.post");
        std::fs::write(&golden, "fixed").unwrap();
        let mut fixture = loaded_fixture_for_tests("fifo-retry", FixtureDriverKind::ControlPlane);
        fixture.paths.golden = golden;
        let mut retry = fixture.manifest.turns[0].clone();
        retry.turn_id.ordinal = 1;
        retry.outcome = response_outcome(Some("fixed"));
        fixture.manifest.turns[0].outcome = response_outcome(Some("wrong bytes"));
        fixture.script_entries[0].1 =
            ScriptOutcome::from(fixture.manifest.turns[0].outcome.clone());
        fixture.manifest.turns.push(retry.clone());
        fixture.script_entries.push((
            RequestFingerprint::of(&retry.request),
            ScriptOutcome::from(retry.outcome),
        ));
        let provider = fixture.scripts.as_ref().unwrap().clone();

        let output = run_offline(&fixture, Arc::clone(&provider)).await;

        assert_eq!(
            output.outcome.status,
            FixtureStatus::Pass,
            "{:?}",
            output.outcome
        );
        assert_eq!(output.outcome.model_calls, 2);
        assert_eq!(output.outcome.compile_clean, Some(true));
        assert_eq!(output.trajectories.len(), 2);
        assert_eq!(output.trajectories[0].turn_id.render(), "repair/-/0");
        assert_eq!(output.trajectories[1].turn_id.render(), "repair/-/1");
        assert!(provider.is_exhausted());
    }
}
