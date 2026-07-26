use crate::error::{EvalError, ReportError};
use crate::harness::{FixtureRunOutput, LoadedFixture};
use crate::report::{FixtureOutcome, FixtureStatus};

pub(crate) async fn run(fixture: &LoadedFixture) -> FixtureRunOutput {
    let error = EvalError::Stub("control_plane driver awaits RFCs 0008-0015".to_owned());
    FixtureRunOutput {
        outcome: FixtureOutcome {
            fixture_id: fixture.manifest.id.clone(),
            set: fixture.manifest.set,
            status: FixtureStatus::Error,
            criteria: vec![],
            wall_ms: 0,
            model_calls: 0,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            retry_count: None,
            human_interventions: None,
            unsafe_introduced: None,
            compile_clean: None,
            error: Some(ReportError::from_eval(&error)),
        },
        trajectories: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::tests::loaded_fixture_for_tests;

    #[tokio::test]
    async fn control_plane_stub_is_error_without_trajectories() {
        let fixture = loaded_fixture_for_tests("control", crate::manifest::FixtureDriverKind::ControlPlane);
        let output = run(&fixture).await;
        assert_eq!(output.outcome.status, FixtureStatus::Error);
        assert_eq!(output.outcome.error.unwrap().kind, "stub");
        assert!(output.trajectories.is_empty());
    }
}
