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
    /// Connect or request timeout elapsed.
    #[error("timeout")]
    Timeout,
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
        ProviderError::Timeout => (ErrorClass::Timeout, RetryDisposition::Retryable),
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

#[cfg(feature = "http-provider")]
pub(crate) fn map_reqwest_error(err: reqwest::Error) -> ProviderError {
    if err.is_timeout() {
        return ProviderError::Timeout;
    }

    let message = err.to_string();
    if error_chain_contains_tls(&err) {
        return ProviderError::Tls(redact_and_truncate(&message, 512));
    }

    ProviderError::Transport(redact_and_truncate(&message, 512))
}

/// Walk `Error::source` **and** nested `io::Error::get_ref` payloads.
///
/// Hyper/tokio-rustls wrap `rustls::Error` in `io::Error`. `io::Error::source`
/// returns the *inner* error's source, not the wrapped value itself, so a plain
/// `source()` walk misses the certificate error (RFC-0007 §8.3.2).
#[cfg(feature = "http-provider")]
pub(crate) fn error_chain_contains_tls(err: &(dyn std::error::Error + 'static)) -> bool {
    const MAX_DEPTH: usize = 16;
    let mut stack: Vec<&(dyn std::error::Error + 'static)> = vec![err];
    let mut depth = 0usize;
    while let Some(current) = stack.pop() {
        depth = depth.saturating_add(1);
        if depth > MAX_DEPTH {
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
