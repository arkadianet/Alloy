//! Validated HTTP client policy for model providers.

use std::time::Duration;

use super::error::{map_reqwest_error, ProviderError};

pub(crate) struct ValidatedHttpClient {
    inner: reqwest::Client,
    connect_timeout: Duration,
    request_timeout: Duration,
}

impl ValidatedHttpClient {
    /// Build a client whose two deadlines are separately attributable.
    ///
    /// `connect_timeout` bounds the connect/TLS phase and `request_timeout`
    /// bounds the whole exchange. A connect deadline longer than the
    /// whole-request deadline can never fire: the request timer wins, reqwest
    /// reports a non-connect timeout, and a genuinely unreachable endpoint is
    /// mis-attributed to the REQUEST stage. The effective connect deadline is
    /// therefore clamped to the request deadline so CONNECT stays reportable
    /// (E2 §a); the operator's configured values are retained verbatim for
    /// evidence.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Internal`] when either deadline is zero, or
    /// the mapped transport error when the client cannot be built.
    pub(crate) fn build(
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, ProviderError> {
        if connect_timeout.is_zero() || request_timeout.is_zero() {
            return Err(ProviderError::Internal(
                "connect_timeout and request_timeout must both be non-zero".into(),
            ));
        }
        let inner = reqwest::Client::builder()
            .use_rustls_tls()
            .redirect(reqwest::redirect::Policy::none())
            .danger_accept_invalid_certs(false)
            .connect_timeout(connect_timeout.min(request_timeout))
            .timeout(request_timeout)
            .build()
            .map_err(map_reqwest_error)?;
        Ok(Self {
            inner,
            connect_timeout,
            request_timeout,
        })
    }

    /// Connect deadline actually installed on the client.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn effective_connect_timeout(&self) -> Duration {
        self.connect_timeout.min(self.request_timeout)
    }

    pub(crate) fn inner(&self) -> &reqwest::Client {
        &self.inner
    }

    /// Configured connect deadline, so a report can state what expired
    /// against which ceiling.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// Configured whole-request deadline.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) const fn request_timeout(&self) -> Duration {
        self.request_timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_deadlines_are_rejected() {
        assert!(matches!(
            ValidatedHttpClient::build(Duration::ZERO, Duration::from_secs(5)),
            Err(ProviderError::Internal(_))
        ));
        assert!(matches!(
            ValidatedHttpClient::build(Duration::from_secs(5), Duration::ZERO),
            Err(ProviderError::Internal(_))
        ));
    }

    /// E2 (a) — CONNECT is only reportable while the connect deadline can
    /// still fire. A connect deadline above the whole-request deadline is
    /// clamped so an unreachable endpoint is not mis-attributed to REQUEST,
    /// while the configured pair stays readable for evidence.
    #[test]
    fn connect_deadline_is_clamped_to_the_request_deadline() {
        let inverted =
            ValidatedHttpClient::build(Duration::from_secs(10), Duration::from_millis(20)).unwrap();
        assert_eq!(inverted.connect_timeout(), Duration::from_secs(10));
        assert_eq!(inverted.request_timeout(), Duration::from_millis(20));
        assert_eq!(
            inverted.effective_connect_timeout(),
            Duration::from_millis(20)
        );

        let ordered =
            ValidatedHttpClient::build(Duration::from_secs(10), Duration::from_secs(600)).unwrap();
        assert_eq!(
            ordered.effective_connect_timeout(),
            Duration::from_secs(10),
            "an orderable pair is installed verbatim"
        );
    }
}
