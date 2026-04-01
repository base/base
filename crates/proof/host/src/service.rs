use std::{fmt, time::Duration};

use base_proof_primitives::{ProofRequest, ProofResult, ProverBackend};
use tracing::{Instrument, info, info_span};

use crate::{Host, HostConfig, HostError, Metrics, ProverConfig, metrics::proof_guard};

/// Default timeout for an entire proof request (witness generation + proving).
///
/// Must be shorter than the caller's HTTP request timeout (30 min on the
/// proposer) so the server returns a clean error instead of the client seeing
/// a dropped connection.
const DEFAULT_PROOF_REQUEST_TIMEOUT: Duration = Duration::from_secs(29 * 60);

/// Orchestrates witness generation ([`Host`]) and proving ([`ProverBackend`]).
///
/// Long-lived — holds static config and a backend instance.
/// Receives per-proof [`ProofRequest`]s via [`prove_block`](Self::prove_block).
pub struct ProverService<B> {
    config: ProverConfig,
    backend: B,
    /// Maximum wall-clock time for a single proof request before it is aborted.
    proof_request_timeout: Duration,
}

impl<B: ProverBackend> fmt::Debug for ProverService<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProverService")
            .field("config", &self.config)
            .field("proof_request_timeout", &self.proof_request_timeout)
            .finish_non_exhaustive()
    }
}

impl<B: ProverBackend> ProverService<B> {
    /// Creates a new prover service with the default proof request timeout.
    pub const fn new(config: ProverConfig, backend: B) -> Self {
        Self { config, backend, proof_request_timeout: DEFAULT_PROOF_REQUEST_TIMEOUT }
    }

    /// Sets the proof request timeout.
    pub const fn with_proof_request_timeout(mut self, timeout: Duration) -> Self {
        self.proof_request_timeout = timeout;
        self
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
    ///
    /// The entire operation is bounded by [`Self::proof_request_timeout`].
    /// If witness generation or proving gets stuck (e.g. infinite prefetch
    /// retry loops), the timeout fires and a clean error is returned.
    pub async fn prove_block(&self, request: ProofRequest) -> Result<ProofResult, ProverError<B>> {
        Metrics::requests_total(Metrics::MODE_ONLINE).increment(1);
        let mut guard = proof_guard!();
        let _proof_timer = base_metrics::timed!(Metrics::proof_duration_seconds());

        let l2_block = request.claimed_l2_block_number;
        let timeout = self.proof_request_timeout;

        let result = match tokio::time::timeout(
            timeout,
            Box::pin(
                self.prove_block_inner(request)
                    .instrument(info_span!("proof_request", l2_block)),
            ),
        )
        .await
        {
            Ok(inner) => inner,
            Err(_elapsed) => Err(ProverError::Host(HostError::Custom(format!(
                "proof request timed out after {}s for L2 block {l2_block}",
                timeout.as_secs()
            )))),
        };

        guard.set_outcome(match &result {
            Ok(_) => Metrics::OUTCOME_SUCCESS,
            Err(ProverError::Host(_)) => Metrics::OUTCOME_WITNESS_ERROR,
            Err(ProverError::Backend(_)) => Metrics::OUTCOME_PROVE_ERROR,
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

        let oracle = base_metrics::time!(Metrics::witness_build_duration_seconds(), {
            host.build_witness(oracle).await.map_err(ProverError::Host)?
        });

        let result = base_metrics::time!(Metrics::prover_duration_seconds(), {
            self.backend.prove(oracle).await.map_err(ProverError::Backend)?
        });

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
