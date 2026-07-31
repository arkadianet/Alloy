//! Naive baseline driver for RFC-0016 holdout comparison.
//!
//! Default build: scripted ordinal-0 repair turn + golden byte oracle.
//! With `--features stack-driver`: apply golden `full_file_replace` and run a
//! live sandboxed `cargo check` (no control-plane DAG) for a fair thesis
//! comparison against the live ControlPlane stack driver.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

#[cfg(not(feature = "stack-driver"))]
use crate::driver::skeleton::{run_scripted, ScriptedDriverMode};
use crate::harness::{FixtureRunOutput, LoadedFixture};
use crate::scripted::ScriptedProvider;

pub(crate) async fn run(
    fixture: &LoadedFixture,
    provider: Arc<ScriptedProvider>,
    cancel: Option<CancellationToken>,
) -> FixtureRunOutput {
    #[cfg(feature = "stack-driver")]
    {
        let _ = provider;
        return crate::driver::stack::run_naive_live(fixture, cancel).await;
    }
    #[cfg(not(feature = "stack-driver"))]
    {
        run_scripted(fixture, provider, cancel, ScriptedDriverMode::NaiveBaseline).await
    }
}
