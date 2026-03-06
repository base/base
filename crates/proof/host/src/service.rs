use std::{fmt, sync::Arc};

use base_proof_preimage::WitnessOracle;
use base_proof_primitives::{ProofRequest, ProofResult, ProverBackend};
use tracing::info;

use crate::{Host, HostConfig, HostError, ProverConfig};

/// Orchestrates witness generation ([`Host`]) and proving ([`ProverBackend`]).
///
/// Long-lived — holds static config and a backend instance.
/// Receives per-proof [`ProofRequest`]s via [`prove_block`](Self::prove_block).
pub struct ProverService<B: ProverBackend> {
    config: ProverConfig,
    backend: B,
}

impl<B: ProverBackend> fmt::Debug for ProverService<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProverService").field("config", &self.config).finish_non_exhaustive()
    }
}

impl<B: ProverBackend> ProverService<B> {
    /// Creates a new prover service.
    pub const fn new(config: ProverConfig, backend: B) -> Self {
        Self { config, backend }
    }

    /// Returns a reference to the prover configuration.
    pub const fn config(&self) -> &ProverConfig {
        &self.config
    }

    /// Prove a single block.
    ///
    /// 1. Assembles [`HostConfig`] from static config + per-proof request
    /// 2. Creates a fresh [`Host`] (connects RPCs internally)
    /// 3. Creates a backend-specific oracle via [`ProverBackend::create_oracle`]
    /// 4. Runs witness generation via [`Host::build_witness`]
    /// 5. Hands the populated oracle to [`ProverBackend::prove`]
    pub async fn prove_block(&self, request: ProofRequest) -> Result<ProofResult, ProverError<B>> {
        info!(l2_block = request.claimed_l2_block_number, "starting proof generation");

        let host = Host::new(HostConfig { request, prover: self.config.clone(), data_dir: None });
        let oracle = self.backend.create_oracle();
        let oracle = Arc::new(oracle);

        host.build_witness(Arc::clone(&oracle)).await.map_err(ProverError::Host)?;

        // Extract oracle from Arc. Safe because Host drops its clone after
        // build_witness returns — no background tasks retain a reference.
        let oracle = Arc::try_unwrap(oracle).map_err(|_| ProverError::WitnessNotRecovered)?;

        info!(
            preimage_count = oracle.preimage_count(),
            "witness generation complete, starting proving"
        );

        self.backend.prove(oracle).await.map_err(ProverError::Backend)
    }
}

/// Error type for [`ProverService`] operations.
///
/// Generic over the backend to carry its specific error type without boxing.
#[derive(Debug, thiserror::Error)]
pub enum ProverError<B: ProverBackend> {
    /// Witness generation failed.
    #[error("witness generation failed: {0}")]
    Host(HostError),

    /// Backend proving failed.
    #[error("proving failed: {0}")]
    Backend(B::Error),

    /// Witness oracle could not be recovered after witness generation.
    /// This indicates a bug — Host should drop all references before returning.
    #[error("failed to recover witness oracle after witness generation")]
    WitnessNotRecovered,
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::RwLock};

    use base_proof_preimage::{PreimageKey, WitnessOracle};

    use super::*;

    /// Minimal backend for testing trait bounds and `ProverService` compilation.
    struct NoopBackend;

    #[derive(Debug, Default)]
    struct NoopOracle {
        preimages: RwLock<HashMap<PreimageKey, Vec<u8>>>,
    }

    impl WitnessOracle for NoopOracle {
        fn insert_preimage(&self, key: PreimageKey, value: &[u8]) {
            self.preimages.write().unwrap().insert(key, value.to_vec());
        }
        fn finalize(&self) {}
        fn preimage_count(&self) -> usize {
            self.preimages.read().unwrap().len()
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("noop")]
    struct NoopError;

    #[async_trait::async_trait]
    impl ProverBackend for NoopBackend {
        type Oracle = NoopOracle;
        type Error = NoopError;

        fn create_oracle(&self) -> Self::Oracle {
            NoopOracle::default()
        }

        async fn prove(&self, _witness: Self::Oracle) -> Result<ProofResult, Self::Error> {
            Err(NoopError)
        }
    }

    #[test]
    fn prover_error_host_display() {
        let err: ProverError<NoopBackend> =
            ProverError::Host(HostError::Custom("rpc timeout".into()));
        assert!(err.to_string().contains("witness generation failed"));
        assert!(err.to_string().contains("rpc timeout"));
    }

    #[test]
    fn prover_error_backend_display() {
        let err: ProverError<NoopBackend> = ProverError::Backend(NoopError);
        assert!(err.to_string().contains("proving failed"));
        assert!(err.to_string().contains("noop"));
    }

    #[test]
    fn prover_error_witness_not_recovered_display() {
        let err: ProverError<NoopBackend> = ProverError::WitnessNotRecovered;
        assert!(err.to_string().contains("failed to recover witness oracle"));
    }
}
