//! Exact-contract BYOM preflight.
//!
//! Exercises both holdout arms' wire contracts against a live
//! OpenAI-compatible endpoint before any fixture runs, writes what it observed
//! as JSON, and fails closed with a stable reason code so a matrix wrapper can
//! abort before it scores anything.
//!
//! The Alloy arm's contract is derived from the same rendered `router.toml`
//! the arm executes, so a config that would silently degrade `json_schema` to
//! `json_object` is caught here rather than discovered after a sweep.
//!
//! The API key comes from the process environment (default `ALLOY_API_KEY`,
//! overridable with `--api-key-env`) or from an explicit `--api-key` value.
//! This binary never reads or writes a dotenv file.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use alloy_eval::{
    render_router_toml, run_byom_preflight, LiveRepairEndpoint, PreflightReport, PreflightSpec,
    DEFAULT_CONNECT_TIMEOUT_MS, DEFAULT_REQUEST_TIMEOUT_MS,
};
use alloy_runtime::SecretString;

const USAGE: &str = "\
alloy-eval-byom-preflight — prove one endpoint serves BOTH arms' wire contracts

USAGE:
  alloy-eval-byom-preflight --model <id> --base-url <url> --temperature <f64> \\
    --result <json-path> [--router-toml <path>] \\
    [--request-timeout-ms <u64>] [--connect-timeout-ms <u64>] \\
    [--api-key-env <NAME> | --api-key <value>]

Without --router-toml the Alloy arm's contract is derived from the router
document this repository renders for the given endpoint.

EXIT CODES:
  0  both contracts passed and the arms agree
  2  the preflight could not run (bad usage, missing credential, bad config)
  3  a contract failed or the arms disagree; --result holds the reason

Never reads or writes a dotenv file.";

/// A contract failed or the arms disagree. The report was written.
const EXIT_CONTRACT_FAILED: u8 = 3;

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(EXIT_CONTRACT_FAILED),
        Err(error) => {
            eprintln!("alloy-eval-byom-preflight: {error}");
            ExitCode::from(2)
        }
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

/// Default environment variable holding the endpoint credential.
const DEFAULT_API_KEY_ENV: &str = "ALLOY_API_KEY";

/// Resolve the credential from an explicit flag or the named environment
/// variable. Both paths reject an empty value; neither consults a file.
///
/// `lookup` is the environment reader, injected so the rule is testable
/// without mutating process state.
fn resolve_api_key_with(
    options: &BTreeMap<String, String>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<SecretString, String> {
    if options.contains_key("api-key") && options.contains_key("api-key-env") {
        return Err("--api-key and --api-key-env are mutually exclusive".to_owned());
    }
    if let Some(explicit) = options.get("api-key") {
        let trimmed = explicit.trim();
        if trimmed.is_empty() {
            return Err("--api-key must not be empty".to_owned());
        }
        return Ok(SecretString::new(trimmed));
    }
    let name = options
        .get("api-key-env")
        .map_or(DEFAULT_API_KEY_ENV, String::as_str);
    lookup(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(SecretString::new)
        .ok_or_else(|| {
            format!(
                "environment variable {name} is unset or empty (export it in the calling shell, \
                 or pass --api-key)"
            )
        })
}

fn resolve_api_key(options: &BTreeMap<String, String>) -> Result<SecretString, String> {
    resolve_api_key_with(options, |name| std::env::var(name).ok())
}

/// Returns `Ok(true)` when both contracts passed.
fn run() -> Result<bool, String> {
    let options = parse_options(std::env::args().skip(1).collect())?;
    let model = required(&options, "model")?.to_owned();
    let base_url = required(&options, "base-url")?.to_owned();
    let temperature: f64 = required(&options, "temperature")?
        .parse()
        .map_err(|_| "--temperature must be a number".to_owned())?;
    let result_path = PathBuf::from(required(&options, "result")?);
    let request_timeout =
        optional_millis(&options, "request-timeout-ms", DEFAULT_REQUEST_TIMEOUT_MS)?;
    let connect_timeout =
        optional_millis(&options, "connect-timeout-ms", DEFAULT_CONNECT_TIMEOUT_MS)?;

    let endpoint = LiveRepairEndpoint {
        model: model.clone(),
        temperature,
        base_url: base_url.clone(),
    };
    let router_toml = match options.get("router-toml") {
        Some(path) => fs::read_to_string(Path::new(path))
            .map_err(|error| format!("read router.toml {path}: {error}"))?,
        None => render_router_toml(&endpoint).map_err(|error| format!("render router: {error}"))?,
    };

    // Resolve the credential only after every non-network argument validates,
    // so a usage mistake never touches a secret.
    let api_key = resolve_api_key(&options)?;

    let spec = PreflightSpec {
        model,
        base_url,
        temperature,
        connect_timeout,
        request_timeout,
    };
    let report = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("build async runtime: {error}"))?
        .block_on(run_byom_preflight(&spec, &router_toml, api_key))
        .map_err(|error| format!("preflight: {error}"))?;

    write_report(&result_path, &report)?;
    describe(&report);
    Ok(report.ok)
}

fn write_report(path: &Path, report: &PreflightReport) -> Result<(), String> {
    let json = serde_json::to_string_pretty(report)
        .map_err(|error| format!("serialize preflight report: {error}"))?;
    fs::write(path, format!("{json}\n"))
        .map_err(|error| format!("write {}: {error}", path.display()))
}

fn describe(report: &PreflightReport) {
    for probe in &report.probes {
        let usage = match (probe.tokens_in, probe.tokens_out) {
            (Some(input), Some(output)) => format!("{input}/{output}"),
            _ => "unknown".to_owned(),
        };
        println!(
            "alloy-eval-byom-preflight: {arm} wire_mode={mode} ok={ok} {duration}ms tokens={usage}{degraded}",
            arm = probe.arm.as_str(),
            mode = probe.wire_mode.as_str(),
            ok = probe.ok,
            duration = probe.duration_ms,
            degraded = if probe.degraded() { " DEGRADED" } else { "" },
        );
        if let Some(failure) = &probe.failure {
            eprintln!(
                "alloy-eval-byom-preflight: {arm} failed kind={kind} class={class} status={status} timeout_stage={stage}",
                arm = probe.arm.as_str(),
                kind = failure.kind,
                class = failure.error_class,
                status = failure
                    .http_status
                    .map_or_else(|| "none".to_owned(), |status| status.to_string()),
                stage = failure.timeout_stage.as_deref().unwrap_or("none"),
            );
        }
    }
    match &report.failure {
        Some(failure) => eprintln!(
            "alloy-eval-byom-preflight: FAILED reason={} detail={}",
            failure.code, failure.detail
        ),
        None => println!("alloy-eval-byom-preflight: both wire contracts served"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// Credentials come from the environment or an explicit flag only, and an
    /// empty value is never accepted as "configured".
    #[test]
    fn credentials_come_from_env_or_flag_and_reject_empty_values() {
        let none = |_: &str| None;
        let key = resolve_api_key_with(&options(&[("api-key", " secret ")]), none).unwrap();
        assert_eq!(key.expose(), "secret");

        let from_env = resolve_api_key_with(&options(&[]), |name| {
            assert_eq!(name, DEFAULT_API_KEY_ENV);
            Some("env-secret".to_owned())
        })
        .unwrap();
        assert_eq!(from_env.expose(), "env-secret");

        let renamed = resolve_api_key_with(&options(&[("api-key-env", "OTHER_KEY")]), |name| {
            (name == "OTHER_KEY").then(|| "other".to_owned())
        })
        .unwrap();
        assert_eq!(renamed.expose(), "other");

        assert!(resolve_api_key_with(&options(&[("api-key", "   ")]), none).is_err());
        assert!(resolve_api_key_with(&options(&[]), |_| Some("  ".to_owned())).is_err());
        assert!(resolve_api_key_with(&options(&[]), none).is_err());
        assert!(
            resolve_api_key_with(&options(&[("api-key", "a"), ("api-key-env", "B")]), none)
                .is_err()
        );
    }

    /// E2 (d) — the deadline is configurable and defaults to the Alloy arm's,
    /// so the preflight measures the same latency budget the sweep will use.
    #[test]
    fn deadlines_are_configurable_and_default_to_the_arm_budget() {
        let empty = options(&[]);
        assert_eq!(
            optional_millis(&empty, "request-timeout-ms", DEFAULT_REQUEST_TIMEOUT_MS).unwrap(),
            Duration::from_millis(DEFAULT_REQUEST_TIMEOUT_MS)
        );
        assert_eq!(
            optional_millis(&empty, "connect-timeout-ms", DEFAULT_CONNECT_TIMEOUT_MS).unwrap(),
            Duration::from_millis(DEFAULT_CONNECT_TIMEOUT_MS)
        );

        let set = options(&[("request-timeout-ms", "1500")]);
        assert_eq!(
            optional_millis(&set, "request-timeout-ms", DEFAULT_REQUEST_TIMEOUT_MS).unwrap(),
            Duration::from_millis(1500)
        );
        assert!(optional_millis(
            &options(&[("request-timeout-ms", "0")]),
            "request-timeout-ms",
            1
        )
        .is_err());
        assert!(optional_millis(
            &options(&[("request-timeout-ms", "soon")]),
            "request-timeout-ms",
            1
        )
        .is_err());
    }

    #[test]
    fn option_parsing_rejects_bare_words_and_repeats() {
        let parsed = parse_options(vec!["--model".into(), "m".into()]).unwrap();
        assert_eq!(parsed.get("model").map(String::as_str), Some("m"));
        assert!(parse_options(vec!["model".into(), "m".into()]).is_err());
        assert!(parse_options(vec!["--model".into()]).is_err());
        assert!(parse_options(vec![
            "--model".into(),
            "a".into(),
            "--model".into(),
            "b".into()
        ])
        .is_err());
        assert!(required(&parsed, "base-url").is_err());
    }
}
