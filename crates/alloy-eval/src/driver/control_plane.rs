//! Offline scripted control-plane driver for RFC-0016 holdout control runs.
//!
//! M7 wires this path through the scheduler/CLI vertical slice (RFCs
//! 0008–0015). Until that stack is available inside `alloy-eval`, the driver
//! replays every manifest turn via [`ScriptedProvider`] with the same golden
//! byte oracle as [`super::skeleton`]. Live DAG execution, sandbox apply, and
//! `TomlModelRouter` integration remain blocked on those RFCs.

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
    run_scripted(fixture, provider, cancel, ScriptedDriverMode::ControlPlane).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::tests::loaded_fixture_for_tests;
    use crate::manifest::FixtureDriverKind;
    use crate::report::FixtureStatus;

    #[tokio::test]
    async fn control_plane_replays_all_turns_offline() {
        let dir = tempfile::tempdir().unwrap();
        let golden = dir.path().join("lib.rs.post");
        std::fs::write(&golden, "fixed").unwrap();
        let mut fixture =
            loaded_fixture_for_tests("control-plane", FixtureDriverKind::ControlPlane);
        fixture.paths.golden = golden;
        let provider = fixture.scripts.as_ref().unwrap().clone();

        let output = run(&fixture, provider, None).await;

        assert_eq!(output.outcome.status, FixtureStatus::Pass, "{:?}", output.outcome);
        assert_eq!(output.outcome.model_calls, 1);
        assert!(output.outcome.error.is_none());
        assert_eq!(output.trajectories.len(), 1);
        assert!(output.trajectories[0].complete_ok);
    }
}
