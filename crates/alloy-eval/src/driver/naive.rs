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
    run_scripted(fixture, provider, cancel, ScriptedDriverMode::NaiveBaseline).await
}
