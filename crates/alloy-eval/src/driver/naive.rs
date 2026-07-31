//! Naive baseline driver for RFC-0016 holdout comparison.
//!
//! Default: scripted ordinal-0 repair turn + golden byte oracle.
//! Live path (golden `full_file_replace` + sandboxed `cargo check`) requires
//! `--features stack-driver` and `ALLOY_EVAL_LIVE_STACK=1`, matching the
//! ControlPlane live gate. Golden apply is plumbing smoke, not a thesis arm
//! with independent model outputs.

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
        return crate::driver::stack::run_naive_live(fixture, cancel).await;
    }
    run_scripted(fixture, provider, cancel, ScriptedDriverMode::NaiveBaseline).await
}
