//! Client configuration for connecting to the prover service.

use std::time::Duration;

use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use tracing::debug;
use url::Url;

use crate::ProverServiceClientError;

/// Configuration shared by prover-service client roles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProverServiceClientConfig {
    endpoint: String,
    request_timeout: Duration,
    poll_interval: Duration,
    max_wait: Duration,
}

impl ProverServiceClientConfig {
    /// Default per-request timeout for prover-service JSON-RPC calls.
    pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

    /// Default interval used by polling helpers when waiting for proof completion.
    pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);

    /// Default maximum time to wait for proof completion.
    pub const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(30 * 60);

    /// Create a client configuration for the given HTTP endpoint.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            request_timeout: Self::DEFAULT_REQUEST_TIMEOUT,
            poll_interval: Self::DEFAULT_POLL_INTERVAL,
            max_wait: Self::DEFAULT_MAX_WAIT,
        }
    }

    /// Return the prover-service HTTP endpoint.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Return the configured per-request timeout.
    pub const fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    /// Return the interval used by polling helpers.
    pub const fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// Return the maximum time to wait for proof completion.
    pub const fn max_wait(&self) -> Duration {
        self.max_wait
    }

    /// Set the per-request timeout used by the JSON-RPC HTTP client.
    pub const fn with_request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    /// Set the interval used by polling helpers.
    pub const fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    /// Set the maximum time to wait for proof completion.
    pub const fn with_max_wait(mut self, max_wait: Duration) -> Self {
        self.max_wait = max_wait;
        self
    }

    /// Validate the endpoint and duration fields.
    pub fn validate(&self) -> Result<(), ProverServiceClientError> {
        let endpoint = Url::parse(&self.endpoint).map_err(|err| {
            ProverServiceClientError::InvalidConfig(format!("endpoint URL is invalid: {err}"))
        })?;

        if !matches!(endpoint.scheme(), "http" | "https") {
            return Err(ProverServiceClientError::InvalidConfig(
                "endpoint URL scheme must be http or https".to_owned(),
            ));
        }

        if endpoint.host().is_none() {
            return Err(ProverServiceClientError::InvalidConfig(
                "endpoint URL must include a host".to_owned(),
            ));
        }

        if self.request_timeout.is_zero() {
            return Err(ProverServiceClientError::InvalidConfig(
                "request timeout must be greater than zero".to_owned(),
            ));
        }

        if self.poll_interval.is_zero() {
            return Err(ProverServiceClientError::InvalidConfig(
                "poll interval must be greater than zero".to_owned(),
            ));
        }

        if self.max_wait.is_zero() {
            return Err(ProverServiceClientError::InvalidConfig(
                "max wait must be greater than zero".to_owned(),
            ));
        }

        if self.poll_interval > self.max_wait {
            return Err(ProverServiceClientError::InvalidConfig(
                "poll interval must be less than or equal to max wait".to_owned(),
            ));
        }

        Ok(())
    }

    /// Build a JSON-RPC HTTP client from this configuration.
    pub fn build_http_client(&self) -> Result<HttpClient, ProverServiceClientError> {
        self.validate()?;

        let builder = HttpClientBuilder::default().request_timeout(self.request_timeout);

        debug!(endpoint = %self.endpoint, "building prover-service client");
        builder.build(&self.endpoint).map_err(ProverServiceClientError::from)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rstest::rstest;

    use super::*;

    #[test]
    fn config_validation_accepts_valid_config() {
        let config = ProverServiceClientConfig::new("http://localhost:8545")
            .with_request_timeout(Duration::from_secs(1))
            .with_poll_interval(Duration::from_millis(100))
            .with_max_wait(Duration::from_secs(10));

        config.validate().expect("valid config should pass validation");
    }

    #[rstest]
    #[case::url_without_host(
        "http://",
        Duration::from_secs(1),
        Duration::from_millis(100),
        Duration::from_secs(10),
        "host"
    )]
    #[case::non_http_url_scheme(
        "file:///tmp/prover-service.sock",
        Duration::from_secs(1),
        Duration::from_millis(100),
        Duration::from_secs(10),
        "scheme"
    )]
    #[case::zero_request_timeout(
        "http://localhost:8545",
        Duration::ZERO,
        Duration::from_millis(100),
        Duration::from_secs(10),
        "request timeout"
    )]
    #[case::zero_poll_interval(
        "http://localhost:8545",
        Duration::from_secs(1),
        Duration::ZERO,
        Duration::from_secs(10),
        "poll interval"
    )]
    #[case::zero_max_wait(
        "http://localhost:8545",
        Duration::from_secs(1),
        Duration::from_millis(100),
        Duration::ZERO,
        "max wait"
    )]
    #[case::poll_interval_greater_than_max_wait(
        "http://localhost:8545",
        Duration::from_secs(1),
        Duration::from_secs(11),
        Duration::from_secs(10),
        "poll interval"
    )]
    fn config_validation_rejects_invalid_config(
        #[case] endpoint: &str,
        #[case] request_timeout: Duration,
        #[case] poll_interval: Duration,
        #[case] max_wait: Duration,
        #[case] expected_message: &str,
    ) {
        let config = ProverServiceClientConfig::new(endpoint)
            .with_request_timeout(request_timeout)
            .with_poll_interval(poll_interval)
            .with_max_wait(max_wait);

        let err = config.validate().expect_err("invalid config should fail validation");

        assert!(
            matches!(err, ProverServiceClientError::InvalidConfig(message) if message.contains(expected_message))
        );
    }
}
