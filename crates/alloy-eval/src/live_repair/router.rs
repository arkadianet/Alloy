//! Pure `router.toml` rendering for the live-repair benchmark workspace.
//!
//! Rendering is a pure string function so it can be unit-tested offline; the
//! thin shell wrapper writes the result into its own throwaway workspace and
//! is the only component that ever contacts the endpoint.

use crate::error::EvalError;
use crate::live_repair::report::LiveRepairEndpoint;

/// Per-request timeout written into the rendered router config, in ms.
pub const LIVE_REPAIR_REQUEST_TIMEOUT_MS: u64 = 600_000;

/// Render the `router.toml` for a live-repair benchmark workspace.
///
/// Ownership: borrows `endpoint`; returns an owned TOML document ending in a
/// newline.
///
/// # Errors
///
/// [`EvalError::Manifest`] when the model id, base URL, or temperature would
/// produce a malformed or injectable TOML document.
pub fn render_router_toml(endpoint: &LiveRepairEndpoint) -> Result<String, EvalError> {
    validate_scalar("model", &endpoint.model)?;
    validate_scalar("base_url", &endpoint.base_url)?;
    if !endpoint.temperature.is_finite() || !(0.0..=2.0).contains(&endpoint.temperature) {
        return Err(EvalError::Manifest(
            "temperature must be finite and within 0.0..=2.0".into(),
        ));
    }

    Ok(format!(
        r#"[policy]
default_tier = "standard"
connect_timeout_ms = 10000
request_timeout_ms = {timeout}
shutdown_grace_ms = 5000
max_in_flight = 1

[[providers]]
id = "local"
kind = "openai_compatible"
base_url = "{base_url}"
api_key_env = "ALLOY_API_KEY"

[[providers.endpoints]]
id = "bench"
display_name = "Bench"
model = "{model}"
tiers = ["standard", "economy", "premium"]
supports_tools = true
supports_structured_output = true
# E2 (c): explicit, never defaulted. Without this flag the router silently
# degrades a schema-carrying request to `json_object`, which puts this arm on
# a different wire contract than the one-shot naive arm and makes the two
# incomparable. `json_schema_strict` stays off: strict mode rejects schemas
# outside OpenAI's supported subset, and local servers grammar-constrain
# regardless.
supports_json_schema = true
max_context = 32768
input_usd_per_mtok = 0.0
output_usd_per_mtok = 0.0
temperature = {temperature:?}

[capability_tiers]
repair = "standard"
edit = "standard"
review = "economy"
planning = "standard"
"#,
        timeout = LIVE_REPAIR_REQUEST_TIMEOUT_MS,
        base_url = endpoint.base_url,
        model = endpoint.model,
        temperature = endpoint.temperature,
    ))
}

fn validate_scalar(field: &str, value: &str) -> Result<(), EvalError> {
    if value.trim().is_empty() {
        return Err(EvalError::Manifest(format!("{field} must be non-empty")));
    }
    if value.len() > 512 {
        return Err(EvalError::Manifest(format!(
            "{field} must be at most 512 bytes"
        )));
    }
    if value.contains(['"', '\\', '\n', '\r']) {
        return Err(EvalError::Manifest(format!(
            "{field} must not contain quotes, backslashes, or newlines"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> LiveRepairEndpoint {
        LiveRepairEndpoint {
            model: "qwen2.5-coder:32b".to_owned(),
            temperature: 0.6,
            base_url: "http://127.0.0.1:11434/v1/".to_owned(),
        }
    }

    #[test]
    fn rendered_router_is_valid_toml_with_expected_values() {
        let rendered = render_router_toml(&endpoint()).unwrap();
        let parsed: toml::Value = toml::from_str(&rendered).unwrap();
        let provider = &parsed["providers"][0];
        assert_eq!(provider["kind"].as_str().unwrap(), "openai_compatible");
        assert_eq!(
            provider["base_url"].as_str().unwrap(),
            "http://127.0.0.1:11434/v1/"
        );
        assert_eq!(provider["api_key_env"].as_str().unwrap(), "ALLOY_API_KEY");
        let model_endpoint = &provider["endpoints"][0];
        assert_eq!(
            model_endpoint["model"].as_str().unwrap(),
            "qwen2.5-coder:32b"
        );
        assert_eq!(model_endpoint["temperature"].as_float().unwrap(), 0.6);
        assert_eq!(
            parsed["policy"]["request_timeout_ms"].as_integer().unwrap() as u64,
            LIVE_REPAIR_REQUEST_TIMEOUT_MS
        );
        assert_eq!(
            parsed["capability_tiers"]["repair"].as_str().unwrap(),
            "standard"
        );
        let tiers = model_endpoint["tiers"].as_array().unwrap();
        assert!(tiers.iter().any(|tier| tier.as_str() == Some("premium")));
        assert!(rendered.ends_with('\n'));
    }

    /// E2 (c) — the Alloy arm's endpoint must declare `supports_json_schema`.
    /// Omitting it serde-defaults to `false`, which makes the router degrade
    /// a schema-carrying repair request to `json_object` behind a log line;
    /// the naive arm always sends `json_schema`, so the two arms would run on
    /// different wire contracts and would not be comparable.
    #[test]
    fn rendered_router_declares_the_json_schema_wire_contract() {
        let rendered = render_router_toml(&endpoint()).unwrap();
        assert!(
            rendered.contains("supports_json_schema = true"),
            "the flag must be explicit in the document, not defaulted:\n{rendered}"
        );
        let parsed: toml::Value = toml::from_str(&rendered).unwrap();
        let model_endpoint = &parsed["providers"][0]["endpoints"][0];
        assert_eq!(
            model_endpoint["supports_json_schema"].as_bool(),
            Some(true),
            "the Alloy arm must accept the caller's JSON Schema"
        );
        assert_eq!(
            model_endpoint["supports_structured_output"].as_bool(),
            Some(true),
            "supports_json_schema requires supports_structured_output"
        );
        // The rendered document must be loadable by the same validator the
        // real binary uses, or the arm never starts.
        let config = alloy_runtime::RouterConfig::from_str("rendered", &rendered).unwrap();
        assert!(config.providers[0].endpoints[0].supports_json_schema);
    }

    #[test]
    fn rendered_router_rejects_injection_and_bad_temperature() {
        for model in ["", "  ", "a\"b", "a\\b", "a\nb", &"x".repeat(513)] {
            let mut broken = endpoint();
            broken.model = model.to_owned();
            assert!(
                render_router_toml(&broken).is_err(),
                "model {model:?} must be rejected"
            );
        }
        let mut broken = endpoint();
        broken.base_url = "http://x\"/v1/".to_owned();
        assert!(render_router_toml(&broken).is_err());

        for temperature in [f64::NAN, f64::INFINITY, -0.1, 2.1] {
            let mut broken = endpoint();
            broken.temperature = temperature;
            assert!(
                render_router_toml(&broken).is_err(),
                "temperature {temperature} must be rejected"
            );
        }
    }

    #[test]
    fn integral_temperature_still_renders_as_float() {
        let mut integral = endpoint();
        integral.temperature = 1.0;
        let rendered = render_router_toml(&integral).unwrap();
        let parsed: toml::Value = toml::from_str(&rendered).unwrap();
        assert_eq!(
            parsed["providers"][0]["endpoints"][0]["temperature"]
                .as_float()
                .unwrap(),
            1.0
        );
    }
}
