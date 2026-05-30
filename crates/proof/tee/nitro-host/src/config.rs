//! Worker configuration for the nitro prover host.

use base_prover_service_client::{ProverServiceClientConfig, ProverServiceClientError};

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
    pub fn validate(&self) -> Result<(), ProverServiceClientError> {
        self.prover_service.validate()
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

        assert!(
            matches!(err, ProverServiceClientError::InvalidConfig(message) if message.contains("scheme"))
        );
    }
}
