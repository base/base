use std::sync::Arc;

use base_proof_host::{ProverConfig, ProverError, ProverService};
use base_proof_primitives::{ProofRequest, ProofResult};
use base_proof_transport::ProofTransport;

use super::NitroBackend;

/// Host-side TEE prover server wrapping [`ProverService<NitroBackend>`].
#[derive(Debug)]
pub struct NitroServer {
    service: ProverService<NitroBackend>,
}

impl NitroServer {
    /// Create a server with the given prover config and enclave transport.
    pub fn new(config: ProverConfig, transport: Arc<dyn ProofTransport>) -> Self {
        let backend = NitroBackend::new(transport);
        Self { service: ProverService::new(config, backend) }
    }

    /// Run the full proof pipeline for a single request.
    pub async fn prove(
        &self,
        request: ProofRequest,
    ) -> Result<ProofResult, ProverError<NitroBackend>> {
        self.service.prove_block(request).await
    }
}
