//! Registration driver — core orchestration loop.
//!
//! Discovers prover instances, checks on-chain registration status, generates
//! ZK proofs for unregistered signers, and submits registration transactions
//! to L1 via the [`TxManager`]. Also detects orphaned on-chain signers (those
//! no longer backed by a healthy instance) and deregisters them.

use std::{collections::HashSet, fmt, time::Duration};

use alloy_primitives::{Address, Bytes, hex};
use alloy_sol_types::SolCall;
use base_proof_tee_nitro_attestation_prover::AttestationProofProvider;
use base_tx_manager::{TxCandidate, TxManager};
use rand::random;
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

    /// Single registration cycle: discover → filter → register → deregister orphans.
    async fn step(&self) -> Result<()> {
        let instances = self.discovery.discover_instances().await?;
        let registerable: Vec<_> =
            instances.iter().filter(|i| i.health_status.should_register()).collect();

        if !registerable.is_empty() {
            info!(
                total = instances.len(),
                registerable = registerable.len(),
                "discovered prover instances"
            );
        }

        let mut active_signers = HashSet::new();
        let mut cancelled = false;

        for instance in registerable {
            if self.config.cancel.is_cancelled() {
                cancelled = true;
                break;
            }

            match self.process_instance(instance).await {
                Ok(address) => {
                    active_signers.insert(address);
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        instance = %instance.instance_id,
                        endpoint = %instance.endpoint,
                        "failed to process instance"
                    );
                }
            }
        }

        // Skip orphan cleanup if the loop was interrupted by cancellation,
        // since the active set is incomplete and could cause false deregistrations.
        if cancelled {
            debug!("shutdown requested, skipping orphan deregistration");
            return Ok(());
        }

        // Guard against mass deregistration from transient failures: if
        // discovery found instances but no active signers were resolved (all
        // processing failed, or all instances are unhealthy/draining and may
        // recover), our view of the active set is unreliable. Only proceed
        // when discovery itself returns zero instances (truly no instances
        // exist) or we successfully resolved at least one active signer.
        if active_signers.is_empty() && !instances.is_empty() {
            warn!("no active signers resolved, skipping orphan deregistration");
            return Ok(());
        }

        if let Err(e) = self.deregister_orphans(&active_signers).await {
            warn!(error = %e, "failed to deregister orphan signers");
        }

        Ok(())
    }

    /// Processes a single instance: check registration first (cheap), then
    /// fetch attestation and generate proof only if needed.
    ///
    /// Returns the derived signer address regardless of whether registration
    /// was needed, so the caller can build the active signer set.
    async fn process_instance(&self, instance: &ProverInstance) -> Result<Address> {
        let client = ProverClient::new(&instance.endpoint, self.config.prover_timeout)?;

        // Fetch only the public key (cheap RPC) and derive the address to
        // check registration before triggering the expensive NSM attestation call.
        let public_key = client.signer_public_key().await?;
        let signer_address = ProverClient::derive_address(&public_key)?;

        if self.registry.is_registered(signer_address).await? {
            debug!(signer = %signer_address, "already registered, skipping");
            return Ok(signer_address);
        }

        // Check cancellation before the most expensive operation (proof generation
        // can take minutes via Boundless).
        if self.config.cancel.is_cancelled() {
            debug!("shutdown requested, skipping proof generation");
            return Ok(signer_address);
        }

        info!(
            signer = %signer_address,
            instance = %instance.instance_id,
            "generating proof for unregistered signer"
        );

        // Only fetch the full NSM attestation document when registration is needed.
        // Bind a random nonce into the attestation to prevent replay attacks.
        let nonce: [u8; 32] = random();
        info!(nonce = %hex::encode(nonce), signer = %signer_address, "requesting attestation with nonce");
        let attestation_bytes = client.signer_attestation(None, Some(nonce.to_vec())).await?;
        let proof = self.proof_provider.generate_proof(&attestation_bytes).await?;

        // Check cancellation before submitting the transaction — avoid starting
        // new on-chain work if shutdown is in progress.
        if self.config.cancel.is_cancelled() {
            debug!("shutdown requested, skipping transaction submission");
            return Ok(signer_address);
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

        Ok(signer_address)
    }

    /// Deregisters any on-chain signer that is not in the `active_signers` set.
    ///
    /// These orphans arise when a prover instance is terminated (e.g. ASG
    /// scale-down) without first deregistering its signer on-chain.
    ///
    /// # Assumptions
    ///
    /// - **Single registrar**: This method queries *all* on-chain signers and
    ///   treats any signer not in `active_signers` as an orphan. If multiple
    ///   registrar instances manage disjoint prover fleets, one registrar would
    ///   incorrectly deregister another's signers. The current deployment model
    ///   assumes a single registrar per registry contract.
    ///
    /// - **Draining instances are expendable**: Instances in the `Draining`
    ///   state are excluded from `active_signers` (they don't pass
    ///   `should_register()`), so their on-chain signers will be deregistered.
    ///   This is correct as long as the ALB drain timeout completes before the
    ///   next deregistration cycle, ensuring no in-flight signed operations are
    ///   disrupted.
    async fn deregister_orphans(&self, active_signers: &HashSet<Address>) -> Result<()> {
        let orphans: Vec<_> = self
            .registry
            .get_registered_signers()
            .await?
            .into_iter()
            .filter(|addr| !active_signers.contains(addr))
            .collect();

        if orphans.is_empty() {
            return Ok(());
        }

        info!(count = orphans.len(), "deregistering orphan signers");

        let mut deregistered = 0usize;
        for signer in orphans {
            if self.config.cancel.is_cancelled() {
                debug!("shutdown requested, stopping orphan deregistration");
                break;
            }

            info!(signer = %signer, "deregistering orphan signer");

            let calldata = ITEEProverRegistry::deregisterSignerCall { signer }.abi_encode();

            let candidate = TxCandidate {
                tx_data: Bytes::from(calldata),
                to: Some(self.config.registry_address),
                ..Default::default()
            };

            match self.tx_manager.send(candidate).await {
                Ok(receipt) => {
                    info!(
                        signer = %signer,
                        tx_hash = %receipt.transaction_hash,
                        "orphan signer deregistered"
                    );
                    deregistered += 1;
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        signer = %signer,
                        "failed to deregister orphan signer"
                    );
                }
            }
        }

        info!(count = deregistered, "orphan deregistration complete");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, address};
    use alloy_sol_types::SolCall;
    use rstest::rstest;

    use crate::registry::ITEEProverRegistry;

    /// Expected byte length of ABI-encoded `deregisterSigner(address)` calldata:
    /// 4-byte selector + 32-byte left-padded address word.
    const DEREGISTER_CALLDATA_LEN: usize = 36;

    /// Number of zero-padding bytes before the 20-byte address in the ABI word.
    const ABI_ADDRESS_PAD: usize = 12;

    /// Byte offset where the raw 20-byte address starts in the encoded calldata
    /// (after the 4-byte selector and 12 bytes of zero-padding).
    const ABI_ADDRESS_OFFSET: usize = 4 + ABI_ADDRESS_PAD;

    /// Well-known Hardhat / Anvil account #0 address.
    const HARDHAT_ACCOUNT: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    #[rstest]
    #[case::zero_address(Address::ZERO)]
    #[case::hardhat_account(HARDHAT_ACCOUNT)]
    #[case::all_ones(Address::repeat_byte(0xFF))]
    fn deregister_calldata_encodes_correctly(#[case] signer: Address) {
        let calldata = ITEEProverRegistry::deregisterSignerCall { signer }.abi_encode();

        assert_eq!(calldata.len(), DEREGISTER_CALLDATA_LEN);
        assert_eq!(&calldata[..4], &ITEEProverRegistry::deregisterSignerCall::SELECTOR);
        // The 12 bytes between the selector and the address must be zero-padding.
        assert_eq!(&calldata[4..ABI_ADDRESS_OFFSET], &[0u8; ABI_ADDRESS_PAD]);
        // The last 20 bytes must be the raw signer address.
        assert_eq!(&calldata[ABI_ADDRESS_OFFSET..], signer.as_slice());
    }
}
