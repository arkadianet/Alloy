//! Benchmark deterministic RFC-0007 endpoint selection.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use alloy_runtime::{
    BudgetPolicy, BudgetSnapshot, CapabilityId, DecisionLog, DecisionRecord, EventSeq,
    ModelCallRecord, ModelRouter, ObsError, ProviderId, RecordingModelProvider, RouterConfig,
    RoutingRequest, RunId, SessionId, SharedCostMeter, TomlModelRouter, TomlModelRouterParts,
    ToolCallRecord,
};
use async_trait::async_trait;
use criterion::{black_box, criterion_group, criterion_main, Criterion};

/// Decision log that assigns sequences without retaining records.
struct DiscardDecisionLog {
    next_seq: AtomicU64,
}

#[async_trait]
impl DecisionLog for DiscardDecisionLog {
    async fn record(&self, _rec: DecisionRecord) -> Result<EventSeq, ObsError> {
        Ok(EventSeq(self.next_seq.fetch_add(1, Ordering::Relaxed)))
    }

    async fn record_model_call(&self, _rec: ModelCallRecord) -> Result<EventSeq, ObsError> {
        Ok(EventSeq(self.next_seq.fetch_add(1, Ordering::Relaxed)))
    }

    async fn record_tool_call(&self, _rec: ToolCallRecord) -> Result<EventSeq, ObsError> {
        Ok(EventSeq(self.next_seq.fetch_add(1, Ordering::Relaxed)))
    }
}

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
    let log = Arc::new(DiscardDecisionLog {
        next_seq: AtomicU64::new(0),
    });
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
    let request = request(run);
    criterion.bench_function("router_select_first_match", |bencher| {
        bencher.to_async(&runtime).iter(|| {
            let request = request.clone();
            async {
                // Drop the handle after selection; admission is released on
                // route return and discard log avoids DecisionRecord growth.
                black_box(router.route(request).await.expect("route"))
            }
        });
    });
}

criterion_group!(benches, bench_router_select);
criterion_main!(benches);
