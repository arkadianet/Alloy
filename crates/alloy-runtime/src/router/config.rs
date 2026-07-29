//! Strict `router.toml` parsing, normalization, and validation.

use std::collections::{BTreeMap, HashSet};
use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use url::Url;

use crate::types::budget::ModelTier;
use crate::types::ids::{EndpointId, ProviderId};

use super::error::RouterError;
use super::types::{redact_and_truncate, ModelEndpoint};

/// Fully validated router configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct RouterConfig {
    /// Routing and lifecycle policy.
    pub policy: RouterPolicy,
    /// Provider catalog. RFC-0007 requires exactly one entry.
    pub providers: Vec<ProviderConfig>,
    /// ASCII-lowercase capability-to-tier map.
    pub capability_tiers: BTreeMap<String, ModelTier>,
}

/// Router timeout, concurrency, and default-tier policy.
#[derive(Debug, Clone, PartialEq)]
pub struct RouterPolicy {
    /// Tier used when a capability has no explicit mapping.
    pub default_tier: ModelTier,
    /// Provider connection timeout.
    pub connect_timeout: Duration,
    /// Total provider request timeout.
    pub request_timeout: Duration,
    /// Router shutdown drain grace.
    pub shutdown_grace: Duration,
    /// Maximum concurrent route and completion calls.
    pub max_in_flight: u32,
    /// Parsed but intentionally unused scoring stub.
    pub scoring: ScoringWeights,
}

/// Parsed endpoint-scoring weights reserved for future routing.
///
/// RFC-0007 selection never reads these values.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScoringWeights {
    /// Reserved complexity weight.
    pub complexity_weight: Option<f64>,
    /// Reserved budget weight.
    pub budget_weight: Option<f64>,
    /// Reserved latency weight.
    pub latency_weight: Option<f64>,
}

/// One validated provider and its endpoints.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderConfig {
    /// Provider catalog identifier.
    pub id: ProviderId,
    /// Provider protocol kind.
    pub kind: ProviderKind,
    /// Valid HTTPS or loopback-HTTP API base URL.
    pub base_url: String,
    /// Process environment variable containing the API key.
    pub api_key_env: String,
    /// Endpoints in declaration order.
    pub endpoints: Vec<EndpointConfig>,
}

/// Supported provider protocol kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// OpenAI-compatible chat-completions HTTP protocol.
    OpenaiCompatible,
}

/// One validated endpoint row.
#[derive(Debug, Clone, PartialEq)]
pub struct EndpointConfig {
    /// Endpoint catalog identifier.
    pub id: EndpointId,
    /// Human-readable endpoint label.
    pub display_name: String,
    /// Operator-configured wire model identifier.
    pub model: String,
    /// Eligible model tiers.
    pub tiers: Vec<ModelTier>,
    /// Whether tool-enabled work may select this endpoint.
    pub supports_tools: bool,
    /// Whether JSON-object output may select this endpoint.
    pub supports_structured_output: bool,
    /// Advisory context-window size.
    pub max_context: u32,
    /// Operator price per million input tokens.
    pub input_usd_per_mtok: Option<f64>,
    /// Operator price per million output tokens.
    pub output_usd_per_mtok: Option<f64>,
    /// Optional sampling temperature sent with every completion on this
    /// endpoint (issue #53). `None` leaves the provider default in force.
    pub temperature: Option<f32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RouterFile {
    policy: RouterPolicyFile,
    providers: Vec<ProviderFile>,
    #[serde(default)]
    capability_tiers: BTreeMap<String, ModelTier>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RouterPolicyFile {
    default_tier: ModelTier,
    #[serde(default = "default_connect_timeout_ms")]
    connect_timeout_ms: u64,
    #[serde(default = "default_request_timeout_ms")]
    request_timeout_ms: u64,
    #[serde(default = "default_shutdown_grace_ms")]
    shutdown_grace_ms: u64,
    #[serde(default = "default_max_in_flight")]
    max_in_flight: u32,
    #[serde(default)]
    scoring: ScoringWeightsFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScoringWeightsFile {
    complexity_weight: Option<f64>,
    budget_weight: Option<f64>,
    latency_weight: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderFile {
    id: String,
    kind: String,
    base_url: String,
    api_key_env: String,
    endpoints: Vec<EndpointFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointFile {
    id: String,
    display_name: String,
    model: String,
    tiers: Vec<ModelTier>,
    #[serde(default)]
    supports_tools: bool,
    #[serde(default)]
    supports_structured_output: bool,
    max_context: u32,
    input_usd_per_mtok: Option<f64>,
    output_usd_per_mtok: Option<f64>,
    #[serde(default)]
    temperature: Option<f32>,
}

const fn default_connect_timeout_ms() -> u64 {
    10_000
}

const fn default_request_timeout_ms() -> u64 {
    120_000
}

const fn default_shutdown_grace_ms() -> u64 {
    5_000
}

const fn default_max_in_flight() -> u32 {
    32
}

impl RouterConfig {
    /// Load, parse, normalize, and validate a `router.toml` file.
    ///
    /// API keys are not resolved by this operation.
    pub fn load(path: &Path) -> Result<Self, RouterError> {
        let raw = std::fs::read_to_string(path).map_err(|error| {
            RouterError::Config(format!("read router {}: {error}", path.display()))
        })?;
        Self::from_str(&path.display().to_string(), &raw)
    }

    /// Parse, normalize, and validate TOML from an in-memory string.
    ///
    /// `source_name` is used only in a bounded, redacted error message.
    pub fn from_str(source_name: &str, toml: &str) -> Result<Self, RouterError> {
        let source = redact_and_truncate(source_name, 512);
        let file: RouterFile = toml::from_str(toml).map_err(|error| {
            RouterError::Config(format!(
                "parse router {source}: {}",
                redact_and_truncate(&error.to_string(), 512)
            ))
        })?;
        let mut config = Self::try_from(file)?;
        config.validate_and_normalize()?;
        Ok(config)
    }

    pub(crate) fn validate_and_normalize(&mut self) -> Result<(), RouterError> {
        validate_policy(&self.policy)?;

        if self.providers.is_empty() {
            return Err(config_error("at least one provider is required"));
        }

        let mut provider_ids = HashSet::new();
        for provider in &self.providers {
            if !provider_ids.insert(provider.id.as_str()) {
                return Err(config_error("duplicate provider id"));
            }
        }
        if self.providers.len() != 1 {
            return Err(config_error("MVP allows exactly one provider"));
        }

        let mut endpoint_ids = HashSet::new();
        for provider in &self.providers {
            validate_provider(provider)?;
            for endpoint in &provider.endpoints {
                if !endpoint_ids.insert(endpoint.id.as_str()) {
                    return Err(config_error("duplicate endpoint id"));
                }
                validate_endpoint(endpoint)?;
            }
        }

        let source = std::mem::take(&mut self.capability_tiers);
        let mut normalized = BTreeMap::new();
        for (raw_key, tier) in source {
            let key = raw_key.trim();
            if key.is_empty() || key.len() > 128 {
                return Err(config_error(
                    "capability key must contain 1..=128 UTF-8 bytes after trim",
                ));
            }
            let canonical = key.to_ascii_lowercase();
            if normalized.insert(canonical, tier).is_some() {
                return Err(config_error(
                    "capability keys collide after ASCII lowercase normalization",
                ));
            }
        }
        self.capability_tiers = normalized;
        Ok(())
    }
}

impl TryFrom<RouterFile> for RouterConfig {
    type Error = RouterError;

    fn try_from(file: RouterFile) -> Result<Self, Self::Error> {
        let providers = file
            .providers
            .into_iter()
            .map(ProviderConfig::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            policy: RouterPolicy {
                default_tier: file.policy.default_tier,
                connect_timeout: Duration::from_millis(file.policy.connect_timeout_ms),
                request_timeout: Duration::from_millis(file.policy.request_timeout_ms),
                shutdown_grace: Duration::from_millis(file.policy.shutdown_grace_ms),
                max_in_flight: file.policy.max_in_flight,
                scoring: ScoringWeights {
                    complexity_weight: file.policy.scoring.complexity_weight,
                    budget_weight: file.policy.scoring.budget_weight,
                    latency_weight: file.policy.scoring.latency_weight,
                },
            },
            providers,
            capability_tiers: file.capability_tiers,
        })
    }
}

impl TryFrom<ProviderFile> for ProviderConfig {
    type Error = RouterError;

    fn try_from(file: ProviderFile) -> Result<Self, Self::Error> {
        let id = ProviderId::new(file.id)
            .map_err(|_| config_error("provider id must contain 1..=128 bytes"))?;
        let kind = match file.kind.as_str() {
            "openai_compatible" => ProviderKind::OpenaiCompatible,
            _ => return Err(config_error("unsupported provider kind")),
        };
        let endpoints = file
            .endpoints
            .into_iter()
            .map(EndpointConfig::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            id,
            kind,
            base_url: file.base_url,
            api_key_env: file.api_key_env.trim().to_owned(),
            endpoints,
        })
    }
}

impl TryFrom<EndpointFile> for EndpointConfig {
    type Error = RouterError;

    fn try_from(file: EndpointFile) -> Result<Self, Self::Error> {
        let id = EndpointId::new(file.id)
            .map_err(|_| config_error("endpoint id must contain 1..=128 bytes"))?;
        if let Some(t) = file.temperature {
            if !t.is_finite() || !(0.0..=2.0).contains(&t) {
                return Err(config_error(
                    "endpoint temperature must be within 0.0..=2.0",
                ));
            }
        }
        Ok(Self {
            id,
            display_name: file.display_name,
            model: file.model,
            tiers: file.tiers,
            supports_tools: file.supports_tools,
            supports_structured_output: file.supports_structured_output,
            max_context: file.max_context,
            input_usd_per_mtok: file.input_usd_per_mtok,
            output_usd_per_mtok: file.output_usd_per_mtok,
            temperature: file.temperature,
        })
    }
}

impl EndpointConfig {
    pub(crate) fn to_endpoint(&self, provider: ProviderId) -> ModelEndpoint {
        ModelEndpoint {
            id: self.id.clone(),
            provider,
            display_name: self.display_name.clone(),
            model: self.model.clone(),
            tiers: self.tiers.clone(),
            supports_tools: self.supports_tools,
            supports_structured_output: self.supports_structured_output,
            max_context: self.max_context,
            input_usd_per_mtok: self.input_usd_per_mtok,
            output_usd_per_mtok: self.output_usd_per_mtok,
            temperature: self.temperature,
        }
    }
}

fn validate_policy(policy: &RouterPolicy) -> Result<(), RouterError> {
    if policy.connect_timeout.is_zero()
        || policy.request_timeout.is_zero()
        || policy.shutdown_grace.is_zero()
    {
        return Err(config_error("router timeouts must be greater than zero"));
    }
    if policy.max_in_flight == 0 || policy.max_in_flight > 1024 {
        return Err(config_error("max_in_flight must be in 1..=1024"));
    }
    Ok(())
}

fn validate_provider(provider: &ProviderConfig) -> Result<(), RouterError> {
    if provider.kind != ProviderKind::OpenaiCompatible {
        return Err(config_error("unsupported provider kind"));
    }
    validate_base_url(&provider.base_url).map_err(config_error)?;
    if provider.api_key_env.trim().is_empty() {
        return Err(config_error("api_key_env must not be empty"));
    }
    if !is_valid_env_var_name(provider.api_key_env.trim()) {
        return Err(config_error(
            "api_key_env must not contain '=' or NUL (std::env::var would panic)",
        ));
    }
    if provider.endpoints.is_empty() {
        return Err(config_error("provider must declare at least one endpoint"));
    }
    Ok(())
}

/// Reject names that panic inside `std::env::var` (`=` or NUL bytes).
fn is_valid_env_var_name(name: &str) -> bool {
    !name.contains('=') && !name.contains('\0')
}

fn validate_endpoint(endpoint: &EndpointConfig) -> Result<(), RouterError> {
    if endpoint.display_name.is_empty() || endpoint.display_name.len() > 256 {
        return Err(config_error(
            "endpoint display_name must contain 1..=256 UTF-8 bytes",
        ));
    }
    if endpoint.model.is_empty() || endpoint.model.len() > 512 {
        return Err(config_error(
            "endpoint model must contain 1..=512 UTF-8 bytes",
        ));
    }
    if endpoint.tiers.is_empty() {
        return Err(config_error("endpoint tiers must not be empty"));
    }
    if endpoint.max_context == 0 {
        return Err(config_error(
            "endpoint max_context must be greater than zero",
        ));
    }
    for price in [endpoint.input_usd_per_mtok, endpoint.output_usd_per_mtok]
        .into_iter()
        .flatten()
    {
        if !price.is_finite() || price < 0.0 {
            return Err(config_error(
                "endpoint prices must be finite and non-negative",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_base_url(raw: &str) -> Result<Url, String> {
    if raw.trim().is_empty() {
        return Err("base_url must not be empty".into());
    }
    let url = Url::parse(raw).map_err(|_| "base_url is not a valid URL".to_owned())?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("base_url must not contain userinfo".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("base_url must not contain a query or fragment".into());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "base_url must contain a host".to_owned())?;
    match url.scheme() {
        "https" => {}
        "http" if is_loopback_host(host) => {}
        "http" => return Err("plaintext base_url must use a loopback host".into()),
        _ => return Err("base_url scheme must be https or loopback http".into()),
    }
    Ok(url)
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn config_error(message: impl Into<String>) -> RouterError {
    RouterError::Config(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(base_url: &str) -> String {
        format!(
            r#"
[policy]
default_tier = "standard"

[[providers]]
id = "provider"
kind = "openai_compatible"
base_url = "{base_url}"
api_key_env = "MODEL_KEY"

[[providers.endpoints]]
id = "endpoint"
display_name = "Endpoint"
model = "configured-model"
tiers = ["standard"]
max_context = 1024

[capability_tiers]
Repair = "standard"
"#
        )
    }

    #[test]
    fn parses_normalizes_and_defaults() {
        let config = RouterConfig::from_str("test", &sample("https://example.com/v1")).unwrap();
        assert_eq!(config.policy.max_in_flight, 32);
        assert_eq!(
            config.capability_tiers.get("repair"),
            Some(&ModelTier::Standard)
        );
    }

    /// Issue #53 — optional endpoint sampling temperature. Workers do
    /// mechanical repair; provider defaults (Ollama ≈0.8) are too hot, and
    /// the knob is operator-owned like every other endpoint field.
    #[test]
    fn endpoint_temperature_parses_defaults_and_validates() {
        let with = sample("https://example.com").replace(
            "max_context = 1024",
            "max_context = 1024\ntemperature = 0.2",
        );
        let config = RouterConfig::from_str("test", &with).unwrap();
        assert_eq!(config.providers[0].endpoints[0].temperature, Some(0.2));

        let config = RouterConfig::from_str("test", &sample("https://example.com")).unwrap();
        assert_eq!(config.providers[0].endpoints[0].temperature, None);

        for bad in [
            "temperature = 2.5",
            "temperature = -0.1",
            "temperature = nan",
        ] {
            let body = sample("https://example.com")
                .replace("max_context = 1024", &format!("max_context = 1024\n{bad}"));
            assert!(
                RouterConfig::from_str("test", &body).is_err(),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn enforces_url_security() {
        assert!(RouterConfig::from_str("test", &sample("http://example.com")).is_err());
        assert!(RouterConfig::from_str("test", &sample("http://127.0.0.1")).is_ok());
        assert!(RouterConfig::from_str("test", &sample("https://user@example.com")).is_err());
        assert!(RouterConfig::from_str("test", &sample("https://example.com?x=1")).is_err());
    }

    #[test]
    fn rejects_unknown_fields_and_normalized_collisions() {
        let unknown = sample("https://example.com").replace(
            "default_tier = \"standard\"",
            "default_tier = \"standard\"\nmisspelled = 1",
        );
        assert!(RouterConfig::from_str("test", &unknown).is_err());

        let collision =
            sample("https://example.com").replace("Repair = ", "Repair = \"standard\"\nrepair = ");
        assert!(RouterConfig::from_str("test", &collision).is_err());
    }

    #[test]
    fn rejects_invalid_structural_values() {
        let two = format!(
            "{}\n[[providers]]\nid='other'\nkind='openai_compatible'\nbase_url='https://example.com'\napi_key_env='K'\nendpoints=[]\n",
            sample("https://example.com")
        );
        assert!(RouterConfig::from_str("test", &two).is_err());

        let bad_key = sample("https://example.com")
            .replace("api_key_env = \"MODEL_KEY\"", "api_key_env = \"BAD=NAME\"");
        assert!(RouterConfig::from_str("test", &bad_key).is_err());
        assert!(!is_valid_env_var_name("BAD\0NAME"));

        let zero = sample("https://example.com").replace(
            "default_tier = \"standard\"",
            "default_tier = \"standard\"\nmax_in_flight = 0",
        );
        assert!(RouterConfig::from_str("test", &zero).is_err());
    }

    #[test]
    fn toml_rejects_duplicate_provider_endpoint_ids() {
        let duplicate = sample("https://example.com").replace(
            "id = \"endpoint\"\ndisplay_name = \"Endpoint\"\nmodel = \"configured-model\"\ntiers = [\"standard\"]\nmax_context = 1024",
            "id = \"endpoint\"\ndisplay_name = \"Endpoint\"\nmodel = \"configured-model\"\ntiers = [\"standard\"]\nmax_context = 1024\n\n[[providers.endpoints]]\nid = \"endpoint\"\ndisplay_name = \"Dup\"\nmodel = \"other\"\ntiers = [\"economy\"]\nmax_context = 1024",
        );
        assert!(RouterConfig::from_str("test", &duplicate).is_err());
    }

    #[test]
    fn toml_rejects_empty_model() {
        let empty_model =
            sample("https://example.com").replace("model = \"configured-model\"", "model = \"\"");
        assert!(RouterConfig::from_str("test", &empty_model).is_err());
    }
}
