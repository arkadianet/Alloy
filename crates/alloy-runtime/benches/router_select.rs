//! Benchmark deterministic RFC-0007 endpoint selection.

use std::sync::Arc;

use alloy_runtime::{
    BudgetPolicy, BudgetSnapshot, CapabilityId, ModelRouter, ProviderId, RecordingDecisionLog,
    RecordingModelProvider, RetentionPolicy, RouterConfig, RoutingRequest, RunId, SessionId,
    SharedCostMeter, TomlModelRouter, TomlModelRouterParts,
};
use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn router() -> (TomlModelRouter, RunId) {
    let config = RouterConfig::from_str(
        "benchmark",
        r#"
[policy]
default_tier = "standard"

[[providers]]
id = "provider"
kind = "openai_compatible"
base_url = "https://example.com"
api_key_env = "KEY"

[[providers.endpoints]]
id = "endpoint"
display_name = "Endpoint"
model = "configured"
tiers = ["standard"]
max_context = 1
input_usd_per_mtok = 0.0
output_usd_per_mtok = 0.0

[capability_tiers]
repair = "standard"
"#,
    )
    .expect("benchmark config");
    let run = RunId::new();
    let provider = Arc::new(RecordingModelProvider::new(
        ProviderId::new("provider").expect("provider id"),
    ));
    let log = Arc::new(RecordingDecisionLog::new(RetentionPolicy::defaults()));
    let router = TomlModelRouter::from_parts(TomlModelRouterParts::new(
        config,
        provider,
        BudgetPolicy::default(),
        Some(log),
        Some(SharedCostMeter::new()),
        Some(run),
    ))
    .expect("benchmark router");
    (router, run)
}

fn request(run: RunId) -> RoutingRequest {
    RoutingRequest {
        session: SessionId::new(),
        run: Some(run),
        node: None,
        capability: CapabilityId::new("repair").expect("capability id"),
        complexity: None,
        budget_remaining: BudgetSnapshot {
            usd_spent: 0.0,
            tokens_in: 0,
            tokens_out: 0,
        },
        requires_tools: false,
        requires_structured_output: false,
    }
}

fn bench_router_select(criterion: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().expect("Tokio runtime");
    let (router, run) = router();
    criterion.bench_function("router_select_first_match", |bencher| {
        bencher
            .to_async(&runtime)
            .iter(|| async { black_box(router.route(request(run)).await.expect("route")) });
    });
}

criterion_group!(benches, bench_router_select);
criterion_main!(benches);
