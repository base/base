//! Registration driver — core orchestration loop.
//!
//! Discovers prover instances, checks on-chain registration status, generates
//! ZK proofs for unregistered signers, and submits registration transactions
//! to L1 via [`SimpleTxManager`].

use std::{fmt, time::Duration};

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolCall;
use base_proof_tee_nitro_attestation_prover::AttestationProofProvider;
use base_tx_manager::{SimpleTxManager, TxCandidate, TxManager};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    InstanceDiscovery, ProverClient, ProverInstance, RegistrarError, RegistryClient, Result,
    registry::ITEEProverRegistry,
};

/// Core registration loop tying together discovery, attestation polling,
/// ZK proof generation, and on-chain submission.
///
/// Generic over the discovery, proof generation, and registry backends so
/// each can be mocked independently in tests.
pub struct RegistrationDriver<D, P, R> {
    discovery: D,
    proof_provider: P,
    registry: R,
    tx_manager: SimpleTxManager,
    registry_address: Address,
    poll_interval: Duration,
    prover_timeout: Duration,
    cancel: CancellationToken,
}

impl<D, P, R> fmt::Debug for RegistrationDriver<D, P, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistrationDriver")
            .field("registry_address", &self.registry_address)
            .field("poll_interval", &self.poll_interval)
            .field("prover_timeout", &self.prover_timeout)
            .finish_non_exhaustive()
    }
}

impl<D, P, R> RegistrationDriver<D, P, R>
where
    D: InstanceDiscovery,
    P: AttestationProofProvider,
    R: RegistryClient,
{
    /// Creates a new registration driver.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        discovery: D,
        proof_provider: P,
        registry: R,
        tx_manager: SimpleTxManager,
        registry_address: Address,
        poll_interval: Duration,
        prover_timeout: Duration,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            discovery,
            proof_provider,
            registry,
            tx_manager,
            registry_address,
            poll_interval,
            prover_timeout,
            cancel,
        }
    }

    /// Runs the registration loop until cancelled.
    ///
    /// Runs `step()` immediately on startup, then waits `poll_interval` between
    /// subsequent ticks. Individual instance failures are logged and skipped —
    /// the loop continues with the next instance.
    pub async fn run(&self) -> Result<()> {
        info!(
            poll_interval = ?self.poll_interval,
            registry = %self.registry_address,
            "starting registration driver"
        );

        loop {
            if let Err(e) = self.step().await {
                warn!(error = %e, "registration step failed");
            }

            tokio::select! {
                () = self.cancel.cancelled() => {
                    info!("registration driver received shutdown signal");
                    break;
                }
                () = tokio::time::sleep(self.poll_interval) => {}
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
            if self.cancel.is_cancelled() {
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

    /// Processes a single instance: fetch attestation, check registration,
    /// generate proof if needed, submit transaction.
    async fn process_instance(&self, instance: &ProverInstance) -> Result<()> {
        let client = ProverClient::new(&instance.endpoint, self.prover_timeout)?;
        let response = client.get_attestation_response().await?;

        if self.registry.is_registered(response.signer_address).await? {
            debug!(signer = %response.signer_address, "already registered, skipping");
            return Ok(());
        }

        // Check cancellation before the most expensive operation (proof generation
        // can take minutes via Boundless).
        if self.cancel.is_cancelled() {
            debug!("shutdown requested, skipping proof generation");
            return Ok(());
        }

        info!(
            signer = %response.signer_address,
            instance = %instance.instance_id,
            "generating proof for unregistered signer"
        );

        let proof = self.proof_provider.generate_proof(&response.attestation_bytes).await?;

        // Check cancellation before submitting the transaction — avoid starting
        // new on-chain work if shutdown is in progress.
        if self.cancel.is_cancelled() {
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
            to: Some(self.registry_address),
            ..Default::default()
        };

        let receipt = self.tx_manager.send(candidate).await.map_err(RegistrarError::from)?;

        info!(
            signer = %response.signer_address,
            tx_hash = %receipt.transaction_hash,
            "signer registered successfully"
        );

        Ok(())
    }
}
