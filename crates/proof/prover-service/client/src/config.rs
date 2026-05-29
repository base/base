//! Client configuration for connecting to the prover service.

use std::time::Duration;

use jsonrpsee::{
    core::client::Error,
    http_client::{HttpClient, HttpClientBuilder},
};
use tracing::debug;

/// Configuration shared by prover-service client roles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProverServiceClientConfig {
    endpoint: String,
    request_timeout: Option<Duration>,
}

impl ProverServiceClientConfig {
    /// Create a client configuration for the given HTTP endpoint.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self { endpoint: endpoint.into(), request_timeout: None }
    }

    /// Return the prover-service HTTP endpoint.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Return the configured per-request timeout.
    pub const fn request_timeout(&self) -> Option<Duration> {
        self.request_timeout
    }

    /// Set the per-request timeout used by the JSON-RPC HTTP client.
    pub const fn with_request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = Some(request_timeout);
        self
    }

    /// Build a JSON-RPC HTTP client from this configuration.
    pub fn build_http_client(&self) -> Result<HttpClient, Error> {
        let mut builder = HttpClientBuilder::default();
        if let Some(request_timeout) = self.request_timeout {
            builder = builder.request_timeout(request_timeout);
        }

        debug!(endpoint = %self.endpoint, "building prover-service client");
        builder.build(&self.endpoint)
    }
}
