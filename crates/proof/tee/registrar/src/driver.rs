//! Registration driver — core orchestration loop.
//!
//! Discovers prover instances, checks on-chain registration status, generates
//! ZK proofs for unregistered signers, and submits registration transactions
//! to L1 via the [`TxManager`].

use std::{fmt, time::Duration};

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolCall;
use base_proof_tee_nitro_attestation_prover::AttestationProofProvider;
use base_tx_manager::{TxCandidate, TxManager};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    InstanceDiscovery, ProverClient, ProverInstance, RegistrarError, RegistryClient, Result,
    registry::ITEEProverRegistry,
};

/// Runtime parameters for the [`RegistrationDriver`] that are not
/// trait-based dependencies.
#[derive(Debug, Clone)]
pub struct DriverConfig {
    /// `TEEProverRegistry` contract address on L1.
    pub registry_address: Address,
    /// Interval between discovery and registration poll cycles.
    pub poll_interval: Duration,
    /// Timeout for JSON-RPC calls to prover instances.
    pub prover_timeout: Duration,
    /// Cancellation token for graceful shutdown.
    pub cancel: CancellationToken,
}

/// Core registration loop tying together discovery, attestation polling,
/// ZK proof generation, and on-chain submission.
///
/// Generic over the discovery, proof generation, registry, and transaction
/// manager backends so each can be mocked independently in tests.
pub struct RegistrationDriver<D, P, R, T> {
    discovery: D,
    proof_provider: P,
    registry: R,
    tx_manager: T,
    config: DriverConfig,
}

impl<D, P, R, T> fmt::Debug for RegistrationDriver<D, P, R, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistrationDriver").field("config", &self.config).finish_non_exhaustive()
    }
}

impl<D, P, R, T> RegistrationDriver<D, P, R, T>
where
    D: InstanceDiscovery,
    P: AttestationProofProvider,
    R: RegistryClient,
    T: TxManager,
{
    /// Creates a new registration driver.
    pub const fn new(
        discovery: D,
        proof_provider: P,
        registry: R,
        tx_manager: T,
        config: DriverConfig,
    ) -> Self {
        Self { discovery, proof_provider, registry, tx_manager, config }
    }

    /// Runs the registration loop until cancelled.
    ///
    /// Runs `step()` immediately on startup, then waits `poll_interval` between
    /// subsequent ticks. Individual instance failures are logged and skipped —
    /// the loop continues with the next instance.
    pub async fn run(&self) -> Result<()> {
        info!(
            poll_interval = ?self.config.poll_interval,
            registry = %self.config.registry_address,
            "starting registration driver"
        );

        loop {
            if let Err(e) = self.step().await {
                warn!(error = %e, "registration step failed");
            }

            tokio::select! {
                () = self.config.cancel.cancelled() => {
                    info!("registration driver received shutdown signal");
                    break;
                }
                () = tokio::time::sleep(self.config.poll_interval) => {}
            }
        }

        info!("registration driver stopped");
        Ok(())
    }

    /// Single registration cycle: discover → filter → register.
    async fn step(&self) -> Result<()> {
        let instances = self.discovery.discover_instances().await?;
        let registerable: Vec<_> =
            instances.iter().filter(|i| i.health_status.should_register()).collect();

        if registerable.is_empty() {
            return Ok(());
        }

        info!(
            total = instances.len(),
            registerable = registerable.len(),
            "discovered prover instances"
        );

        for instance in registerable {
            if self.config.cancel.is_cancelled() {
                break;
            }

            if let Err(e) = self.process_instance(instance).await {
                warn!(
                    error = %e,
                    instance = %instance.instance_id,
                    endpoint = %instance.endpoint,
                    "failed to process instance"
                );
            }
        }

        Ok(())
    }

    /// Processes a single instance: check registration first (cheap), then
    /// fetch attestation and generate proof only if needed.
    async fn process_instance(&self, instance: &ProverInstance) -> Result<()> {
        let client = ProverClient::new(&instance.endpoint, self.config.prover_timeout)?;

        // Fetch only the public key (cheap RPC) and derive the address to
        // check registration before triggering the expensive NSM attestation call.
        let public_key = client.signer_public_key().await?;
        let signer_address = ProverClient::derive_address(&public_key)?;

        if self.registry.is_registered(signer_address).await? {
            debug!(signer = %signer_address, "already registered, skipping");
            return Ok(());
        }

        // Check cancellation before the most expensive operation (proof generation
        // can take minutes via Boundless).
        if self.config.cancel.is_cancelled() {
            debug!("shutdown requested, skipping proof generation");
            return Ok(());
        }

        info!(
            signer = %signer_address,
            instance = %instance.instance_id,
            "generating proof for unregistered signer"
        );

        // Only fetch the full NSM attestation document when registration is needed.
        let attestation_bytes = client.signer_attestation().await?;
        let proof = self.proof_provider.generate_proof(&attestation_bytes).await?;

        // Check cancellation before submitting the transaction — avoid starting
        // new on-chain work if shutdown is in progress.
        if self.config.cancel.is_cancelled() {
            debug!("shutdown requested, skipping transaction submission");
            return Ok(());
        }

        let calldata = ITEEProverRegistry::registerSignerCall {
            output: proof.output,
            proofBytes: proof.proof_bytes,
        }
        .abi_encode();

        let candidate = TxCandidate {
            tx_data: Bytes::from(calldata),
            to: Some(self.config.registry_address),
            ..Default::default()
        };

        let receipt = self.tx_manager.send(candidate).await.map_err(RegistrarError::from)?;

        info!(
            signer = %signer_address,
            tx_hash = %receipt.transaction_hash,
            "signer registered successfully"
        );

        Ok(())
    }
}
