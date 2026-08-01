//! One-shot, tool-free naive replacement driver (E1 three-arm holdout, arm B).
//!
//! Reads the target file and diagnostics from disk, sends exactly one
//! OpenAI-compatible completion with no tools, records bounded telemetry for
//! that call, then writes the model's replacement back through a sibling temp
//! file + rename. `ALLOY_API_KEY` is read from the process environment and
//! never logged or serialized.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use alloy_eval::{
    build_naive_request, parse_replacement, resolve_target, write_resolved_replacement,
    NaiveRunTelemetry, DEFAULT_CONNECT_TIMEOUT_MS, DEFAULT_REQUEST_TIMEOUT_MS,
};
use alloy_runtime::{
    EndpointId, ModelEndpoint, ModelProvider, ModelTier, OpenAiCompatibleProvider,
    OpenAiCompatibleSpec, ProviderId, SecretString,
};

const USAGE: &str = "\
alloy-eval-live-naive — one-shot, tool-free naive replacement driver

USAGE:
  alloy-eval-live-naive --workspace <dir> --target <relative-path> \\
    --diagnostics <path> --goal <text> --model <id> --temperature <f64> \\
    --base-url <url> --result <json-path> \\
    [--request-timeout-ms <u64>] [--connect-timeout-ms <u64>]

Deadlines default to the Alloy arm's rendered router.toml values so the two
arms tolerate the same response latency. A provider failure still writes the
result file — the call was spent — and exits 3; usage/exit 2 problems exit 2.

Reads ALLOY_API_KEY from the process environment; never logs it.";

/// The single call reached the provider and failed. Telemetry was written.
const EXIT_PROVIDER_FAILED: u8 = 3;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Harness(error)) => {
            eprintln!("alloy-eval-live-naive: {error}");
            ExitCode::from(2)
        }
        Err(Failure::Provider(error)) => {
            eprintln!("alloy-eval-live-naive: {error}");
            ExitCode::from(EXIT_PROVIDER_FAILED)
        }
    }
}

/// Separates "the harness could not run the attempt" from "the attempt ran
/// and the provider failed". Only the latter has spent telemetry to report.
enum Failure {
    Harness(String),
    Provider(String),
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Harness(message)
    }
}

fn parse_options(args: Vec<String>) -> Result<BTreeMap<String, String>, String> {
    let mut options = BTreeMap::new();
    let mut iter = args.into_iter();
    while let Some(flag) = iter.next() {
        let key = flag
            .strip_prefix("--")
            .ok_or_else(|| format!("expected an option, got {flag}\n{USAGE}"))?
            .to_owned();
        let value = iter
            .next()
            .ok_or_else(|| format!("option --{key} requires a value"))?;
        if options.insert(key.clone(), value).is_some() {
            return Err(format!("option --{key} may only be provided once"));
        }
    }
    Ok(options)
}

fn required<'a>(options: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str, String> {
    options
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| format!("missing required option --{key}\n{USAGE}"))
}

fn write_telemetry(path: &PathBuf, telemetry: &NaiveRunTelemetry) -> Result<(), String> {
    let json = serde_json::to_string_pretty(telemetry)
        .map_err(|error| format!("serialize telemetry: {error}"))?;
    fs::write(path, format!("{json}\n"))
        .map_err(|error| format!("write {}: {error}", path.display()))
}

/// Parse an optional millisecond deadline, defaulting when absent.
fn optional_millis(
    options: &BTreeMap<String, String>,
    key: &str,
    default_ms: u64,
) -> Result<Duration, String> {
    let Some(raw) = options.get(key) else {
        return Ok(Duration::from_millis(default_ms));
    };
    let millis: u64 = raw
        .parse()
        .map_err(|_| format!("--{key} must be a non-negative integer of milliseconds"))?;
    if millis == 0 {
        return Err(format!("--{key} must be greater than zero"));
    }
    Ok(Duration::from_millis(millis))
}

fn run() -> Result<(), Failure> {
    let options = parse_options(std::env::args().skip(1).collect())?;
    let workspace = PathBuf::from(required(&options, "workspace")?);
    let target = required(&options, "target")?.to_owned();
    let diagnostics_path = PathBuf::from(required(&options, "diagnostics")?);
    let goal = required(&options, "goal")?.to_owned();
    let model = required(&options, "model")?.to_owned();
    let temperature: f64 = required(&options, "temperature")?
        .parse()
        .map_err(|_| "--temperature must be a number".to_owned())?;
    let base_url = required(&options, "base-url")?.to_owned();
    let result_path = PathBuf::from(required(&options, "result")?);
    let request_timeout =
        optional_millis(&options, "request-timeout-ms", DEFAULT_REQUEST_TIMEOUT_MS)?;
    let connect_timeout =
        optional_millis(&options, "connect-timeout-ms", DEFAULT_CONNECT_TIMEOUT_MS)?;

    // Validate the target before it is read: an absolute or traversal path
    // must never reach the network, even embedded read-only in the prompt.
    let target_path =
        resolve_target(&workspace, &target).map_err(|error| format!("target: {error}"))?;
    let target_source = fs::read_to_string(&target_path)
        .map_err(|error| format!("read target {}: {error}", target_path.display()))?;
    let diagnostics = fs::read_to_string(&diagnostics_path)
        .map_err(|error| format!("read diagnostics {}: {error}", diagnostics_path.display()))?;

    let request = build_naive_request(&goal, &target, &target_source, &diagnostics, temperature)?;

    let api_key = std::env::var("ALLOY_API_KEY")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "environment variable ALLOY_API_KEY is unset or empty (export it in the calling shell)"
                .to_owned()
        })?;

    let endpoint = ModelEndpoint {
        id: EndpointId::new("naive").map_err(|error| format!("endpoint id: {error:?}"))?,
        provider: ProviderId::new("naive").map_err(|error| format!("provider id: {error:?}"))?,
        display_name: "Naive".to_owned(),
        model,
        tiers: vec![ModelTier::Standard],
        supports_tools: false,
        supports_structured_output: true,
        supports_json_schema: true,
        json_schema_strict: false,
        max_context: 131_072,
        input_usd_per_mtok: None,
        output_usd_per_mtok: None,
        temperature: None,
    };
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleSpec {
        id: endpoint.provider.clone(),
        base_url,
        api_key: SecretString::new(api_key),
        connect_timeout,
        request_timeout,
    })
    .map_err(|error| format!("provider: {error}"))?;

    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("build async runtime: {error}"))?
        .block_on(provider.complete(&endpoint, request));

    // The call is spent — and billed — the moment it returns, so record it
    // before anything that can still fail. A malformed reply, and a deadline
    // that expired mid-flight, must not make a real model call disappear from
    // the arm's telemetry: a timeout is reported as a timeout with its stage,
    // never as "telemetry incomplete".
    let response = match outcome {
        Ok(response) => {
            write_telemetry(
                &result_path,
                &NaiveRunTelemetry::from_response(&response, request_timeout),
            )?;
            response
        }
        Err(error) => {
            let telemetry = NaiveRunTelemetry::from_failure(&error, request_timeout);
            let stage = telemetry
                .failure
                .as_ref()
                .and_then(|failure| failure.timeout_stage.clone())
                .unwrap_or_else(|| "n/a".to_owned());
            write_telemetry(&result_path, &telemetry)?;
            return Err(Failure::Provider(format!(
                "completion failed after {}ms budget: {error} (timeout_stage={stage})",
                request_timeout.as_millis()
            )));
        }
    };

    let text = response
        .text
        .ok_or_else(|| "provider returned no message content".to_owned())?;
    let replacement = parse_replacement(&text)?;
    write_resolved_replacement(&target_path, &replacement.replacement)?;

    println!(
        "alloy-eval-live-naive: replaced {} ({} bytes)",
        target,
        replacement.replacement.len()
    );
    Ok(())
}
