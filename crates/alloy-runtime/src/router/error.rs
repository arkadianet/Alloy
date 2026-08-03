//! Router and provider error taxonomy and classification.

use crate::error::RuntimeError;
use crate::obs::BudgetCheck;
use crate::types::budget::ModelTier;
use crate::types::diagnostic::{ErrorClass, RetryDisposition};

use super::types::redact_and_truncate;

/// Failure produced while routing or completing a model request.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RouterError {
    /// Configuration could not be loaded or violated an invariant.
    #[error("config: {0}")]
    Config(String),
    /// No endpoint satisfied the resolved tier and capability requirements.
    #[error(
        "no endpoint for tier {tier:?} (tools={requires_tools}, structured={requires_structured})"
    )]
    NoEndpoint {
        /// Resolved tier.
        tier: ModelTier,
        /// Whether tool support was required.
        requires_tools: bool,
        /// Whether structured-output support was required.
        requires_structured: bool,
    },
    /// A run budget ceiling was exhausted.
    #[error("budget denied: {0:?}")]
    BudgetDenied(BudgetCheck),
    /// The routed handle's one-shot completion ticket was already consumed.
    #[error("routed model already completed")]
    AlreadyCompleted,
    /// The routed handle belongs to another router instance.
    #[error("routed model was issued by a different router instance")]
    WrongRouter,
    /// The selected provider failed.
    #[error("provider: {0}")]
    Provider(#[from] ProviderError),
    /// Host-level cancellation won before the provider returned.
    #[error("cancelled")]
    Cancelled,
    /// The router is draining or stopped.
    #[error("shutting down")]
    ShuttingDown,
    /// An internal invariant failed.
    #[error("internal: {0}")]
    Internal(String),
}

/// Stage of a provider attempt whose deadline expired.
///
/// A bare "timeout" cannot be acted on: a CONNECT expiry means the endpoint
/// was unreachable, while a REQUEST or READ expiry means the model was
/// reached and was simply slower than the configured ceiling. Reports must be
/// able to say which one happened (E2 §a).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TimeoutStage {
    /// TCP connect or TLS handshake did not complete within `connect_timeout`.
    Connect,
    /// The request deadline expired before response headers arrived.
    Request,
    /// The request deadline expired while the response body was being read.
    Read,
}

impl TimeoutStage {
    /// Stable lowercase token for logs, metrics, and report JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Request => "request",
            Self::Read => "read",
        }
    }
}

impl std::fmt::Display for TimeoutStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Failure produced by a model provider.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProviderError {
    /// Authentication or authorization was rejected.
    #[error("auth failed")]
    Auth,
    /// Provider rate limit was reached.
    #[error("rate limited")]
    RateLimit,
    /// Prompt exceeded the provider context window.
    #[error("context length exceeded")]
    ContextLength,
    /// A deadline elapsed but the expiring stage is not knowable.
    ///
    /// Only providers that cannot observe transport stages (scripted and
    /// offline providers) produce this. Every HTTP attempt reports
    /// [`ProviderError::TimeoutAt`] instead, so a real run never has to
    /// guess.
    #[error("timeout")]
    Timeout,
    /// A deadline elapsed, attributed to the stage that expired.
    #[error("timeout during {stage}")]
    TimeoutAt {
        /// Stage whose deadline expired.
        stage: TimeoutStage,
    },
    /// A successful HTTP response had unusable JSON or shape.
    #[error("malformed response: {0}")]
    MalformedResponse(String),
    /// Provider returned an otherwise unmapped HTTP status.
    #[error("http status {status}: {message}")]
    HttpStatus {
        /// Numeric HTTP status.
        status: u16,
        /// Redacted and bounded provider message.
        message: String,
    },
    /// TLS handshake, certificate, or protocol failure.
    #[error("tls: {0}")]
    Tls(String),
    /// DNS, connection, or non-TLS I/O failure.
    #[error("transport: {0}")]
    Transport(String),
    /// Provider implementation invariant failure.
    #[error("internal: {0}")]
    Internal(String),
}

impl ProviderError {
    /// Whether this failure is a deadline expiry, attributed or not.
    #[must_use]
    pub const fn is_timeout(&self) -> bool {
        matches!(self, Self::Timeout | Self::TimeoutAt { .. })
    }

    /// Stage whose deadline expired, when the provider could attribute one.
    ///
    /// `None` for non-timeouts **and** for an unattributed
    /// [`ProviderError::Timeout`]: a report must never invent a stage.
    #[must_use]
    pub const fn timeout_stage(&self) -> Option<TimeoutStage> {
        match self {
            Self::TimeoutAt { stage } => Some(*stage),
            _ => None,
        }
    }

    /// Numeric HTTP status when the provider returned an unmapped one.
    #[must_use]
    pub const fn http_status(&self) -> Option<u16> {
        match self {
            Self::HttpStatus { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// Scheduler-facing classification that preserves retry disposition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedRouterFailure {
    /// Coarse failure class.
    pub class: ErrorClass,
    /// Whether a later scheduler may consider retrying.
    pub retry: RetryDisposition,
}

/// Classify a provider error without implementing a retry.
#[must_use]
pub fn classify_provider_error(err: &ProviderError) -> ClassifiedRouterFailure {
    let (class, retry) = match err {
        ProviderError::RateLimit | ProviderError::Transport(_) => {
            (ErrorClass::Model, RetryDisposition::Retryable)
        }
        ProviderError::Timeout | ProviderError::TimeoutAt { .. } => {
            (ErrorClass::Timeout, RetryDisposition::Retryable)
        }
        ProviderError::HttpStatus { status, .. } if *status >= 500 => {
            (ErrorClass::Model, RetryDisposition::Retryable)
        }
        ProviderError::Auth
        | ProviderError::ContextLength
        | ProviderError::MalformedResponse(_)
        | ProviderError::HttpStatus { .. }
        | ProviderError::Tls(_)
        | ProviderError::Internal(_) => (ErrorClass::Model, RetryDisposition::NonRetryable),
    };
    ClassifiedRouterFailure { class, retry }
}

/// Classify a router error without implementing a retry.
#[must_use]
pub fn classify_router_error(err: &RouterError) -> ClassifiedRouterFailure {
    match err {
        RouterError::Provider(provider) => classify_provider_error(provider),
        RouterError::BudgetDenied(_) => ClassifiedRouterFailure {
            class: ErrorClass::Budget,
            retry: RetryDisposition::NonRetryable,
        },
        RouterError::Cancelled => ClassifiedRouterFailure {
            class: ErrorClass::Cancelled,
            retry: RetryDisposition::NonRetryable,
        },
        RouterError::Config(_)
        | RouterError::NoEndpoint { .. }
        | RouterError::AlreadyCompleted
        | RouterError::WrongRouter
        | RouterError::ShuttingDown
        | RouterError::Internal(_) => ClassifiedRouterFailure {
            class: ErrorClass::Internal,
            retry: RetryDisposition::NonRetryable,
        },
    }
}

pub(crate) fn normalize_provider_error(err: ProviderError) -> ProviderError {
    match err {
        ProviderError::MalformedResponse(message) => {
            ProviderError::MalformedResponse(redact_and_truncate(&message, 512))
        }
        ProviderError::HttpStatus { status, message } => ProviderError::HttpStatus {
            status,
            message: redact_and_truncate(&message, 512),
        },
        ProviderError::Tls(message) => ProviderError::Tls(redact_and_truncate(&message, 512)),
        ProviderError::Transport(message) => {
            ProviderError::Transport(redact_and_truncate(&message, 512))
        }
        ProviderError::Internal(message) => {
            ProviderError::Internal(redact_and_truncate(&message, 512))
        }
        other => other,
    }
}

/// Pick the stage to report for a timeout.
///
/// A connect-phase failure is authoritative regardless of where the caller
/// was in the exchange; otherwise the caller's position (`Request` before
/// headers, `Read` while draining the body) is the honest answer.
pub(crate) const fn resolve_timeout_stage(
    is_connect: bool,
    default_stage: TimeoutStage,
) -> TimeoutStage {
    if is_connect {
        TimeoutStage::Connect
    } else {
        default_stage
    }
}

/// Map a transport failure, attributing a timeout to `default_stage` unless
/// reqwest reports it as a connect-phase failure.
#[cfg(feature = "http-provider")]
pub(crate) fn map_reqwest_error_at(
    err: reqwest::Error,
    default_stage: TimeoutStage,
) -> ProviderError {
    if err.is_timeout() {
        return ProviderError::TimeoutAt {
            stage: resolve_timeout_stage(err.is_connect(), default_stage),
        };
    }

    let message = err.to_string();
    if error_chain_contains_tls(&err) {
        return ProviderError::Tls(redact_and_truncate(&message, 512));
    }

    ProviderError::Transport(redact_and_truncate(&message, 512))
}

/// Map a transport failure raised before the response body is being drained.
#[cfg(feature = "http-provider")]
pub(crate) fn map_reqwest_error(err: reqwest::Error) -> ProviderError {
    map_reqwest_error_at(err, TimeoutStage::Request)
}

/// Walk `Error::source` **and** nested `io::Error::get_ref` payloads.
///
/// Hyper/tokio-rustls wrap `rustls::Error` in `io::Error`. `io::Error::source`
/// returns the *inner* error's source, not the wrapped value itself, so a plain
/// `source()` walk misses the certificate error (RFC-0007 §8.3.2).
#[cfg(feature = "http-provider")]
pub(crate) fn error_chain_contains_tls(err: &(dyn std::error::Error + 'static)) -> bool {
    const MAX_VISITED: usize = 16;
    let mut stack: Vec<&(dyn std::error::Error + 'static)> = vec![err];
    let mut visited = 0usize;
    while let Some(current) = stack.pop() {
        visited = visited.saturating_add(1);
        if visited > MAX_VISITED {
            break;
        }
        if current.downcast_ref::<rustls::Error>().is_some() {
            return true;
        }
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            if let Some(inner) = io.get_ref() {
                stack.push(inner);
            }
        }
        if let Some(src) = current.source() {
            stack.push(src);
        }
    }
    false
}

impl From<RouterError> for RuntimeError {
    fn from(err: RouterError) -> Self {
        match err {
            RouterError::Config(message) => Self::Config(message),
            other => Self::Internal(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_preserves_retryability() {
        assert_eq!(
            classify_provider_error(&ProviderError::RateLimit),
            ClassifiedRouterFailure {
                class: ErrorClass::Model,
                retry: RetryDisposition::Retryable,
            }
        );
        assert_eq!(
            classify_provider_error(&ProviderError::Tls("certificate".into())).retry,
            RetryDisposition::NonRetryable
        );
        assert_eq!(
            classify_provider_error(&ProviderError::HttpStatus {
                status: 503,
                message: String::new(),
            })
            .retry,
            RetryDisposition::Retryable
        );
        assert_eq!(
            classify_router_error(&RouterError::Cancelled).class,
            ErrorClass::Cancelled
        );
    }

    /// E2 (a) — a timeout must say *which* stage expired. A retained error
    /// that only says "timeout" cannot be attributed to CONNECT vs
    /// REQUEST/READ in a report.
    #[test]
    fn timeout_carries_and_classifies_its_stage() {
        for (stage, text) in [
            (TimeoutStage::Connect, "connect"),
            (TimeoutStage::Request, "request"),
            (TimeoutStage::Read, "read"),
        ] {
            let error = ProviderError::TimeoutAt { stage };
            assert_eq!(error.timeout_stage(), Some(stage));
            assert!(error.is_timeout());
            assert_eq!(stage.as_str(), text);
            assert!(error.to_string().contains(text), "{error}");
            assert_eq!(
                classify_provider_error(&error),
                ClassifiedRouterFailure {
                    class: ErrorClass::Timeout,
                    retry: RetryDisposition::Retryable,
                }
            );
            // Normalization must not erase the stage.
            assert_eq!(
                normalize_provider_error(ProviderError::TimeoutAt { stage }).timeout_stage(),
                Some(stage)
            );
        }

        // The stage-less variant stays a timeout but reports no stage, so a
        // report can never invent one.
        let unattributed = ProviderError::Timeout;
        assert!(unattributed.is_timeout());
        assert_eq!(unattributed.timeout_stage(), None);
        assert_eq!(
            classify_provider_error(&unattributed).class,
            ErrorClass::Timeout
        );

        // Non-timeouts never claim a stage, and HTTP status is readable.
        let status = ProviderError::HttpStatus {
            status: 503,
            message: String::new(),
        };
        assert!(!status.is_timeout());
        assert_eq!(status.timeout_stage(), None);
        assert_eq!(status.http_status(), Some(503));
        assert_eq!(ProviderError::Auth.http_status(), None);
    }

    /// A connect-phase reqwest timeout is attributed to CONNECT even when the
    /// caller's default stage is REQUEST or READ; anything else keeps the
    /// caller's stage.
    #[test]
    fn connect_phase_timeouts_override_the_caller_stage() {
        for default_stage in [TimeoutStage::Request, TimeoutStage::Read] {
            assert_eq!(
                resolve_timeout_stage(true, default_stage),
                TimeoutStage::Connect
            );
            assert_eq!(resolve_timeout_stage(false, default_stage), default_stage);
        }
    }

    #[test]
    fn timeout_stage_names_are_stable_report_tokens() {
        assert_eq!(TimeoutStage::Connect.to_string(), "connect");
        assert_eq!(TimeoutStage::Request.to_string(), "request");
        assert_eq!(TimeoutStage::Read.to_string(), "read");
    }

    #[test]
    fn normalization_redacts_and_bounds_messages() {
        let error = normalize_provider_error(ProviderError::Internal(format!(
            "api_key=sk-abcdefgh {}",
            "é".repeat(600)
        )));
        let ProviderError::Internal(message) = error else {
            panic!("unexpected variant");
        };
        assert!(message.len() <= 512);
        assert!(!message.contains("sk-"));
        assert!(message.is_char_boundary(message.len()));
    }

    #[cfg(feature = "http-provider")]
    #[test]
    fn tls_nested_io_error_classified() {
        let rustls = rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer);
        let inner = std::io::Error::new(std::io::ErrorKind::InvalidData, rustls);
        let outer = std::io::Error::other(inner);
        assert!(error_chain_contains_tls(&outer));
    }
}
