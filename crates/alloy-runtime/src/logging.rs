//! Tracing initialization helpers.

use std::sync::Once;

static INIT: Once = Once::new();

/// Default `EnvFilter` directives used when `RUST_LOG` is unset.
///
/// Targets are **crate** names as the compiler sees them, which for a binary is its
/// `[[bin]] name` — the `alloy` binary's target is `alloy`, not `alloy_cli`. Keep this
/// in sync with the `RUST_LOG` example in `example.env`; `alloy-cli` guards it with a
/// `module_path!()`-derived test.
pub const DEFAULT_FILTER: &str = "alloy_runtime=info,alloy=info";

/// Initialize a default `tracing` subscriber once.
///
/// Safe to call from [`crate::AlloyRuntime::start`] and tests.
pub fn init_tracing() {
    INIT.call_once(|| {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_FILTER));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .try_init();
    });
}
