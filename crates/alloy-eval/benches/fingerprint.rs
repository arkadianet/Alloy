//! Micro-benchmark for request fingerprinting.

use alloy_eval::RequestFingerprint;
use alloy_runtime::{ChatMessage, ChatRole, CompletionRequest, ResponseFormat, ToolChoice};
use criterion::{criterion_group, criterion_main, Criterion};

fn fingerprint_bench(c: &mut Criterion) {
    let request = CompletionRequest {
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".into(),
        }],
        tools: vec![],
        tool_choice: ToolChoice::None,
        response_format: ResponseFormat::Text,
        temperature: None,
        max_output_tokens: None,
    };
    c.bench_function("request_fingerprint_of", |b| {
        b.iter(|| RequestFingerprint::of(&request))
    });
}

criterion_group!(benches, fingerprint_bench);
criterion_main!(benches);
