//! Worker configuration for the nitro prover host.

use base_prover_service_client::{ProverServiceClientConfig, ProverServiceClientConfigError};
use thiserror::Error;

/// Errors that can occur during nitro worker configuration validation.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum ConfigError {
    /// Invalid prover-service client configuration.
    #[error("invalid prover-service client config: {0}")]
    InvalidProverService(#[from] ProverServiceClientConfigError),
}

/// Configuration for a nitro prover worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NitroWorkerConfig {
    /// Prover-service client configuration.
    pub prover_service: ProverServiceClientConfig,
}

impl NitroWorkerConfig {
    /// Create a nitro worker configuration.
    pub const fn new(prover_service: ProverServiceClientConfig) -> Self {
        Self { prover_service }
    }

    /// Validate the worker configuration.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.prover_service.validate().map_err(ConfigError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_accepts_valid_prover_service_endpoint() {
        let config =
            NitroWorkerConfig::new(ProverServiceClientConfig::new("http://localhost:8545"));

        config.validate().expect("valid endpoint should pass validation");
    }

    #[test]
    fn validation_rejects_invalid_prover_service_endpoint() {
        let config = NitroWorkerConfig::new(ProverServiceClientConfig::new(
            "file:///tmp/prover-service.sock",
        ));

        let err = config.validate().expect_err("invalid endpoint should fail validation");

        assert!(matches!(
            err,
            ConfigError::InvalidProverService(
                ProverServiceClientConfigError::InvalidEndpointScheme
            )
        ));
    }
}
