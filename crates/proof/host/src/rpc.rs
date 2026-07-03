use std::time::Duration;

use base_proof_primitives::{ProofRequest, ProofResult, ProverApiServer, ProverBackend};
use jsonrpsee::core::{RpcResult, async_trait};
use jsonrpsee::types::ErrorObjectOwned;
use tracing::warn;

use crate::ProverService;

/// Shared JSON-RPC handler for `prover_*` methods.
#[derive(Debug)]
pub struct ProverRpc<B: ProverBackend> {
    service: ProverService<B>,
    proof_request_timeout: Duration,
}

impl<B: ProverBackend> ProverRpc<B> {
    /// Creates a new proving RPC handler.
    pub const fn new(service: ProverService<B>, proof_request_timeout: Duration) -> Self {
        Self { service, proof_request_timeout }
    }
}

#[async_trait]
impl<B> ProverApiServer for ProverRpc<B>
where
    B: ProverBackend + 'static,
{
    async fn prove(&self, request: ProofRequest) -> RpcResult<ProofResult> {
        let l2_block = request.claimed_l2_block_number;
        let timeout = self.proof_request_timeout;

        match tokio::time::timeout(timeout, self.service.prove_block(request)).await {
            Ok(result) => result
                .map_err(|error| ErrorObjectOwned::owned(-32000, error.to_string(), None::<()>)),
            Err(_elapsed) => {
                let timeout_secs = timeout.as_secs();

                warn!(l2_block = l2_block, timeout_secs = timeout_secs, "proof request timed out");
                Err(ErrorObjectOwned::owned(
                    -32000,
                    format!(
                        "proof request timed out after {timeout_secs}s for L2 block {l2_block}"
                    ),
                    None::<()>,
                ))
            }
        }
    }
}
