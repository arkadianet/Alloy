//! Validated HTTP client policy for model providers.

use std::time::Duration;

use super::error::{map_reqwest_error, ProviderError};

pub(crate) struct ValidatedHttpClient {
    inner: reqwest::Client,
}

impl ValidatedHttpClient {
    pub(crate) fn build(
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, ProviderError> {
        let inner = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .danger_accept_invalid_certs(false)
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .build()
            .map_err(map_reqwest_error)?;
        Ok(Self { inner })
    }

    pub(crate) fn inner(&self) -> &reqwest::Client {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_with_finite_policy_timeouts() {
        let client =
            ValidatedHttpClient::build(Duration::from_secs(1), Duration::from_secs(2)).unwrap();
        let _ = client.inner();
    }
}
