//! Nitro TEE host orchestration.

use std::sync::Arc;

use base_proof_worker::{JobDiscovery, JobDiscoveryConfig, ProofSubmitter};
use base_prover_service_client::ProverWorkerProvider;
use tokio_util::sync::CancellationToken;

use crate::{NitroEnclavePool, ProofGenerator, ProofGeneratorHeartbeatConfig};

/// Runs Nitro TEE proof generation jobs claimed from the prover service.
#[derive(Debug)]
pub struct NitroHost<Client> {
    client: Client,
    pool: Arc<NitroEnclavePool>,
    discovery: JobDiscoveryConfig,
    heartbeat: ProofGeneratorHeartbeatConfig,
}

impl<Client> NitroHost<Client> {
    /// Creates a Nitro host from a prover-service client and enclave pool.
    pub const fn new(
        client: Client,
        pool: Arc<NitroEnclavePool>,
        discovery: JobDiscoveryConfig,
        heartbeat: ProofGeneratorHeartbeatConfig,
    ) -> Self {
        Self { client, pool, discovery, heartbeat }
    }
}

impl<Client> NitroHost<Client>
where
    Client: Clone + ProverWorkerProvider + 'static,
{
    /// Runs the host until cancellation is requested.
    pub async fn run_until_cancelled(self, cancel: CancellationToken) {
        let Self { client, pool, discovery, heartbeat } = self;
        let submitter = ProofSubmitter::new(client.clone());
        let proof_generator = Arc::new(
            ProofGenerator::new(pool, submitter, heartbeat)
                .with_max_pending_submissions(discovery.normalized_max_concurrent_jobs()),
        );
        let discovery = JobDiscovery::new(client, proof_generator, discovery);

        discovery.run_until_cancelled(cancel).await;
    }
}
