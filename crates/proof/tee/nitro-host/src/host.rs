//! Nitro TEE host orchestration.

use std::sync::Arc;

use base_proof_worker::{JobClaimFilter, JobDiscovery, JobDiscoveryConfig, ProofSubmitter};
use base_prover_service_client::ProverWorkerProvider;
use base_prover_service_protocol::TeeKind;
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
    ///
    /// # Panics
    ///
    /// Panics unless `discovery` claims only AWS Nitro TEE jobs.
    pub fn new(
        client: Client,
        pool: Arc<NitroEnclavePool>,
        discovery: JobDiscoveryConfig,
        heartbeat: ProofGeneratorHeartbeatConfig,
    ) -> Self {
        assert!(
            matches!(
                discovery.claim_filter(),
                JobClaimFilter::Tee { tee_kinds }
                    if tee_kinds.as_slice() == [TeeKind::AwsNitro]
            ),
            "NitroHost requires JobDiscoveryConfig::tee with TeeKind::AwsNitro"
        );
        Self { client, pool, discovery, heartbeat }
    }

    /// Returns the job discovery config.
    pub const fn discovery_config(&self) -> &JobDiscoveryConfig {
        &self.discovery
    }

    /// Returns the heartbeat settings used while proofs are generated.
    pub const fn heartbeat_config(&self) -> ProofGeneratorHeartbeatConfig {
        self.heartbeat
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
        let proof_generator = Arc::new(ProofGenerator::new(pool, submitter, heartbeat));
        let discovery = JobDiscovery::new(client, proof_generator, discovery);

        discovery.run_until_cancelled(cancel).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_genesis::ChainConfig;
    use base_common_genesis::RollupConfig;
    use base_proof_host::ProverConfig;
    use base_proof_tee_nitro_enclave::Server as EnclaveServer;
    use base_proof_worker::JobDiscoveryConfig;
    use base_prover_service_protocol::{ZkBackend, ZkVm};

    use super::NitroHost;
    use crate::{NitroEnclavePool, NitroTransport, ProofGeneratorHeartbeatConfig};

    #[test]
    #[should_panic(expected = "NitroHost requires JobDiscoveryConfig::tee with TeeKind::AwsNitro")]
    fn new_rejects_zk_discovery() {
        let server = Arc::new(EnclaveServer::new_local().unwrap());
        let transport = Arc::new(NitroTransport::local(server));
        let pool = Arc::new(NitroEnclavePool::new(
            ProverConfig {
                l1_eth_url: "http://127.0.0.1:1".to_string(),
                l2_eth_url: "http://127.0.0.1:1".to_string(),
                l2_node_url: "http://127.0.0.1:1".to_string(),
                l1_beacon_url: "http://127.0.0.1:1".to_string(),
                l2_chain_id: 0,
                rollup_config: RollupConfig::default(),
                l1_config: ChainConfig::default(),
                enable_experimental_witness_endpoint: false,
            },
            transport,
        ));
        let discovery =
            JobDiscoveryConfig::zk("worker-a", vec![ZkVm::Sp1], vec![ZkBackend::Cluster]);
        let _host = NitroHost::new((), pool, discovery, ProofGeneratorHeartbeatConfig::default());
    }
}
