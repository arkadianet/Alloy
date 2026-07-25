//! Tracing initialization helpers.

use std::sync::Once;

static INIT: Once = Once::new();

/// Initialize a default `tracing` subscriber once.
///
/// Safe to call from [`crate::AlloyRuntime::start`] and tests.
pub fn init_tracing() {
    INIT.call_once(|| {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new("alloy_runtime=info,alloy_cli=info")
        });
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .try_init();
    });
}
