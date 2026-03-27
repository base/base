use std::fmt;

use base_proof_primitives::{ProofRequest, ProofResult, ProverBackend};
use tracing::{Instrument, info, info_span};

use crate::{
    Host, HostConfig, HostError, ProverConfig,
    metrics::{proof_guard, timed},
};

/// Orchestrates witness generation ([`Host`]) and proving ([`ProverBackend`]).
///
/// Long-lived — holds static config and a backend instance.
/// Receives per-proof [`ProofRequest`]s via [`prove_block`](Self::prove_block).
pub struct ProverService<B> {
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
        base_metrics::inc!(counter, crate::Metrics::REQUESTS_TOTAL, crate::Metrics::LABEL_MODE => crate::Metrics::MODE_ONLINE);
        let mut guard =
            proof_guard!(crate::Metrics::IN_FLIGHT_PROOFS, crate::Metrics::REQUESTS_RESULT_TOTAL);
        let _proof_timer = timed!(crate::Metrics::PROOF_DURATION_SECONDS);

        let l2_block = request.claimed_l2_block_number;
        let result = Box::pin(
            self.prove_block_inner(request).instrument(info_span!("proof_request", l2_block)),
        )
        .await;

        guard.set_outcome(match &result {
            Ok(_) => crate::Metrics::OUTCOME_SUCCESS,
            Err(ProverError::Host(_)) => crate::Metrics::OUTCOME_WITNESS_ERROR,
            Err(ProverError::Backend(_)) => crate::Metrics::OUTCOME_PROVE_ERROR,
        });

        result
    }

    async fn prove_block_inner(
        &self,
        request: ProofRequest,
    ) -> Result<ProofResult, ProverError<B>> {
        info!(l2_block = request.claimed_l2_block_number, "starting proof generation");

        let host = Host::new(HostConfig { request, prover: self.config.clone(), data_dir: None });
        let oracle = self.backend.create_oracle();

        let mut witness_timer = timed!(crate::Metrics::WITNESS_BUILD_DURATION_SECONDS);
        let oracle = host.build_witness(oracle).await.map_err(ProverError::Host)?;
        witness_timer.stop();

        let mut prover_timer = timed!(crate::Metrics::PROVER_DURATION_SECONDS);
        let result = self.backend.prove(oracle).await.map_err(ProverError::Backend)?;
        prover_timer.stop();

        Ok(result)
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
        fn insert_preimage(
            &self,
            key: PreimageKey,
            value: &[u8],
        ) -> base_proof_preimage::WitnessOracleResult<()> {
            self.preimages.write().unwrap().insert(key, value.to_vec());
            Ok(())
        }

        fn finalize(&self) -> base_proof_preimage::WitnessOracleResult<()> {
            Ok(())
        }

        fn preimage_count(&self) -> base_proof_preimage::WitnessOracleResult<usize> {
            Ok(self.preimages.read().unwrap().len())
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
}
