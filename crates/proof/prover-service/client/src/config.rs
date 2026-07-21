//! Client configuration for connecting to the prover service.

use std::time::Duration;

use base_retry::RetryConfig;
use jsonrpsee::{
    core::client::Error as JsonRpcClientError,
    http_client::{HttpClient, HttpClientBuilder},
};
use thiserror::Error;
use tracing::debug;
use url::Url;

/// Errors that can occur during prover-service client configuration validation.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ProverServiceClientConfigError {
    /// The configured endpoint URL cannot be parsed.
    #[error("endpoint URL is invalid: {0}")]
    InvalidEndpointUrl(String),
    /// The configured endpoint URL uses an unsupported scheme.
    #[error("endpoint URL scheme must be http or https")]
    InvalidEndpointScheme,
    /// The configured endpoint URL does not include a host.
    #[error("endpoint URL must include a host")]
    MissingEndpointHost,
    /// The configured request timeout is zero.
    #[error("request timeout must be greater than zero")]
    ZeroRequestTimeout,
    /// The configured poll interval is zero.
    #[error("poll interval must be greater than zero")]
    ZeroPollInterval,
    /// The configured maximum wait duration is zero.
    #[error("max wait must be greater than zero")]
    ZeroMaxWait,
    /// The configured poll interval is greater than the maximum wait duration.
    #[error("poll interval must be less than or equal to max wait")]
    PollIntervalExceedsMaxWait,
    /// The configured retry initial delay is greater than the retry max delay.
    #[error("retry initial delay must be less than or equal to retry max delay")]
    RetryInitialDelayExceedsMaxDelay,
    /// The configured retry policy is unbounded.
    #[error("unbounded retries are not allowed for prover-service clients")]
    UnboundedRetryNotAllowed,
}

/// Errors that can occur when building a prover-service client.
#[derive(Debug, Error)]
pub enum ProverServiceClientBuildError {
    /// The client configuration is invalid.
    #[error("invalid prover-service client config: {0}")]
    InvalidConfig(#[from] ProverServiceClientConfigError),
    /// A JSON-RPC client, server, or transport error occurred.
    #[error("prover-service RPC/transport failure: {0}")]
    RpcTransport(#[from] JsonRpcClientError),
}

/// Configuration shared by prover-service client roles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProverServiceClientConfig {
    endpoint: String,
    request_timeout: Duration,
    poll_interval: Duration,
    max_wait: Duration,
    retry: RetryConfig,
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
            retry: RetryConfig::default(),
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

    /// Return the retry configuration applied by [`crate::ProofRequesterClient`] and
    /// idempotent [`crate::ProverWorkerClient`] JSON-RPC methods.
    pub const fn retry_config(&self) -> RetryConfig {
        self.retry
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

    /// Set the retry configuration applied by [`crate::ProofRequesterClient`] and
    /// idempotent [`crate::ProverWorkerClient`] JSON-RPC methods.
    pub const fn with_retry_config(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// Validate the endpoint and duration fields.
    pub fn validate(&self) -> Result<(), ProverServiceClientConfigError> {
        let endpoint = Url::parse(&self.endpoint)
            .map_err(|err| ProverServiceClientConfigError::InvalidEndpointUrl(err.to_string()))?;

        if !matches!(endpoint.scheme(), "http" | "https") {
            return Err(ProverServiceClientConfigError::InvalidEndpointScheme);
        }

        if endpoint.host().is_none() {
            return Err(ProverServiceClientConfigError::MissingEndpointHost);
        }

        if self.request_timeout.is_zero() {
            return Err(ProverServiceClientConfigError::ZeroRequestTimeout);
        }

        if self.poll_interval.is_zero() {
            return Err(ProverServiceClientConfigError::ZeroPollInterval);
        }

        if self.max_wait.is_zero() {
            return Err(ProverServiceClientConfigError::ZeroMaxWait);
        }

        if self.poll_interval > self.max_wait {
            return Err(ProverServiceClientConfigError::PollIntervalExceedsMaxWait);
        }

        // `max_attempts == 0` disables retries; `None` is not allowed for this RPC client.
        if self.retry.max_attempts.is_none() {
            return Err(ProverServiceClientConfigError::UnboundedRetryNotAllowed);
        }

        if self.retry.initial_delay > self.retry.max_delay {
            return Err(ProverServiceClientConfigError::RetryInitialDelayExceedsMaxDelay);
        }

        Ok(())
    }

    /// Build a JSON-RPC HTTP client from this configuration.
    pub fn build_http_client(&self) -> Result<HttpClient, ProverServiceClientBuildError> {
        self.validate()?;

        let builder = HttpClientBuilder::default().request_timeout(self.request_timeout);

        debug!(endpoint = %self.endpoint, "building prover-service client");
        builder.build(&self.endpoint).map_err(ProverServiceClientBuildError::from)
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

        assert!(err.to_string().contains(expected_message));
    }

    #[test]
    fn config_validation_rejects_retry_initial_delay_greater_than_max_delay() {
        let config = ProverServiceClientConfig::new("http://localhost:8545").with_retry_config(
            RetryConfig::new(3, Duration::from_secs(60), Duration::from_millis(50)),
        );

        let err = config.validate().expect_err("invalid retry config should fail validation");

        assert_eq!(err, ProverServiceClientConfigError::RetryInitialDelayExceedsMaxDelay);
    }

    #[test]
    fn config_validation_accepts_zero_retry_max_attempts() {
        // `max_attempts == 0` disables retries; validation must not reject it.
        let config = ProverServiceClientConfig::new("http://localhost:8545").with_retry_config(
            RetryConfig::new(0, Duration::from_millis(10), Duration::from_millis(20)),
        );

        config.validate().expect("zero max_attempts retry config should pass validation");
    }

    #[test]
    fn config_validation_rejects_unbounded_retry() {
        let config = ProverServiceClientConfig::new("http://localhost:8545").with_retry_config(
            RetryConfig::unbounded(Duration::from_millis(10), Duration::from_millis(20)),
        );

        let err = config.validate().expect_err("unbounded retry config should fail validation");

        assert_eq!(err, ProverServiceClientConfigError::UnboundedRetryNotAllowed);
    }
}
