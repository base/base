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
    InstanceDiscovery, ProverClient, ProverInstance, RegistrarError, RegistrarMetrics,
    RegistryClient, Result, registry::ITEEProverRegistry,
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
                metrics::counter!(RegistrarMetrics::PROCESSING_ERRORS_TOTAL).increment(1);
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

    /// Single registration cycle: discover → resolve addresses → register → deregister orphans.
    async fn step(&self) -> Result<()> {
        let instances = self.discovery.discover_instances().await?;
        metrics::counter!(RegistrarMetrics::DISCOVERY_SUCCESS_TOTAL).increment(1);

        if !instances.is_empty() {
            let registerable =
                instances.iter().filter(|i| i.health_status.should_register()).count();
            info!(
                total = instances.len(),
                registerable = registerable,
                "discovered prover instances"
            );
        }

        // Resolve signer addresses for ALL reachable instances (regardless of
        // health status) to build a complete active set. This protects draining
        // instances (still running, usually reachable) from premature
        // deregistration. Truly unreachable instances will fail the RPC and be
        // excluded — the majority guard below is the safeguard for that case.
        // A signer-address cache across cycles would strengthen this but adds
        // state management complexity; deferred for now.
        // Registration is only attempted for instances that pass should_register().
        let mut active_signers = HashSet::new();

        for instance in &instances {
            if self.config.cancel.is_cancelled() {
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
                        "failed to resolve signer address"
                    );
                    metrics::counter!(RegistrarMetrics::PROCESSING_ERRORS_TOTAL).increment(1);
                }
            }
        }

        // Skip orphan cleanup if the loop was interrupted by cancellation,
        // since the active set is incomplete and could cause false deregistrations.
        // CancellationToken is monotonic — once cancelled, it stays cancelled.
        if self.config.cancel.is_cancelled() {
            debug!("shutdown requested, skipping orphan deregistration");
            return Ok(());
        }

        // Guard against mass deregistration from transient failures: require a
        // strict majority (>50%) of discovered instances to be reachable before
        // proceeding with orphan cleanup. When discovery returns zero instances
        // (e.g. after ASG scale-down removes them from the target group),
        // deregistration proceeds normally — scaled-down instances leave the
        // target group entirely, so they don't inflate `instances.len()`.
        if !instances.is_empty() && active_signers.len() * 2 <= instances.len() {
            warn!(
                active = active_signers.len(),
                total = instances.len(),
                "majority of instances unreachable, skipping orphan deregistration"
            );
            return Ok(());
        }

        if let Err(e) = self.deregister_orphans(&active_signers).await {
            warn!(error = %e, "failed to deregister orphan signers");
            metrics::counter!(RegistrarMetrics::PROCESSING_ERRORS_TOTAL).increment(1);
        }

        Ok(())
    }

    /// Resolves a signer address from an instance and attempts registration
    /// if the instance is healthy.
    ///
    /// Returns the derived signer address regardless of whether registration
    /// was needed or succeeded, so the caller can build the active signer set.
    /// Registration failures are logged but do not prevent the address from
    /// being returned.
    async fn process_instance(&self, instance: &ProverInstance) -> Result<Address> {
        let client = ProverClient::new(&instance.endpoint, self.config.prover_timeout)?;

        // Fetch only the public key (cheap RPC) and derive the address.
        let public_key = client.signer_public_key().await?;
        let signer_address = ProverClient::derive_address(&public_key)?;

        // Only attempt registration for instances that pass should_register().
        // Non-registerable instances (Draining, Unhealthy) still contribute
        // their address to the active signer set to prevent premature
        // deregistration.
        if !instance.health_status.should_register() {
            debug!(
                signer = %signer_address,
                status = ?instance.health_status,
                "instance not registerable, skipping registration"
            );
            return Ok(signer_address);
        }

        // Registration is best-effort: failures are logged but the address is
        // still returned to protect the signer from orphan deregistration.
        if let Err(e) = self.try_register(&client, instance, signer_address).await {
            warn!(
                error = %e,
                signer = %signer_address,
                instance = %instance.instance_id,
                "registration attempt failed"
            );
            metrics::counter!(RegistrarMetrics::PROCESSING_ERRORS_TOTAL).increment(1);
        }

        Ok(signer_address)
    }

    /// Attempts to register a signer on-chain if not already registered.
    ///
    /// This is the expensive path: checks on-chain status, fetches the NSM
    /// attestation document, generates a ZK proof, and submits a registration
    /// transaction.
    async fn try_register(
        &self,
        client: &ProverClient,
        instance: &ProverInstance,
        signer_address: Address,
    ) -> Result<()> {
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
        // Bind a random nonce into the attestation to prevent replay attacks.
        let nonce: [u8; 32] = random();
        info!(nonce = %hex::encode(nonce), signer = %signer_address, "requesting attestation with nonce");
        let attestation_bytes = client.signer_attestation(None, Some(nonce.to_vec())).await?;
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
        metrics::counter!(RegistrarMetrics::REGISTRATIONS_TOTAL).increment(1);

        Ok(())
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
                    metrics::counter!(RegistrarMetrics::DEREGISTRATIONS_TOTAL).increment(1);
                    deregistered += 1;
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        signer = %signer,
                        "failed to deregister orphan signer"
                    );
                    metrics::counter!(RegistrarMetrics::PROCESSING_ERRORS_TOTAL).increment(1);
                }
            }
        }

        info!(count = deregistered, "orphan deregistration complete");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        sync::{Arc, Mutex},
    };

    use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
    use alloy_primitives::{Address, B256, Bloom, Bytes, address};
    use alloy_rpc_types_eth::TransactionReceipt;
    use alloy_sol_types::SolCall;
    use async_trait::async_trait;
    use base_tx_manager::{SendHandle, TxCandidate, TxManager};
    use rstest::rstest;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{RegistryClient, Result, registry::ITEEProverRegistry};

    // ── Shared constants ────────────────────────────────────────────────

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

    // ── Test helpers ─────────────────────────────────────────────────────

    /// Builds a minimal `TransactionReceipt` for mock tx managers.
    fn stub_receipt() -> TransactionReceipt {
        let inner = ReceiptEnvelope::Legacy(ReceiptWithBloom {
            receipt: Receipt {
                status: Eip658Value::Eip658(true),
                cumulative_gas_used: 21_000,
                logs: vec![],
            },
            logs_bloom: Bloom::ZERO,
        });
        TransactionReceipt {
            inner,
            transaction_hash: B256::ZERO,
            transaction_index: Some(0),
            block_hash: Some(B256::ZERO),
            block_number: Some(1),
            gas_used: 21_000,
            effective_gas_price: 1_000_000_000,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: Some(Address::ZERO),
            contract_address: None,
        }
    }

    // ── Mock implementations ────────────────────────────────────────────

    /// Mock discovery that is unused by `deregister_orphans` tests.
    #[derive(Debug)]
    struct StubDiscovery;

    #[async_trait]
    impl InstanceDiscovery for StubDiscovery {
        async fn discover_instances(&self) -> Result<Vec<ProverInstance>> {
            Ok(vec![])
        }
    }

    /// Mock proof provider that is unused by `deregister_orphans` tests.
    #[derive(Debug)]
    struct StubProofProvider;

    #[async_trait]
    impl AttestationProofProvider for StubProofProvider {
        async fn generate_proof(
            &self,
            _attestation_bytes: &[u8],
        ) -> base_proof_tee_nitro_attestation_prover::Result<
            base_proof_tee_nitro_attestation_prover::AttestationProof,
        > {
            unimplemented!("not used in deregister_orphans tests")
        }
    }

    /// Mock registry that returns a configured set of registered signers.
    #[derive(Debug)]
    struct MockRegistry {
        signers: Vec<Address>,
    }

    #[async_trait]
    impl RegistryClient for MockRegistry {
        async fn is_registered(&self, _signer: Address) -> Result<bool> {
            Ok(false)
        }

        async fn get_registered_signers(&self) -> Result<Vec<Address>> {
            Ok(self.signers.clone())
        }
    }

    /// Mock tx manager that records submitted calldata for assertion.
    #[derive(Debug, Clone)]
    struct SharedTxManager {
        sent: Arc<Mutex<Vec<Bytes>>>,
    }

    impl SharedTxManager {
        fn new() -> Self {
            Self { sent: Arc::new(Mutex::new(vec![])) }
        }

        fn sent_calldata(&self) -> Vec<Bytes> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl TxManager for SharedTxManager {
        async fn send(&self, candidate: TxCandidate) -> base_tx_manager::SendResponse {
            self.sent.lock().unwrap().push(candidate.tx_data);
            Ok(stub_receipt())
        }

        async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
            unimplemented!("not used in deregister_orphans tests")
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    fn driver_with_shared_tx(
        registered_signers: Vec<Address>,
        tx: SharedTxManager,
    ) -> RegistrationDriver<StubDiscovery, StubProofProvider, MockRegistry, SharedTxManager> {
        let registry = MockRegistry { signers: registered_signers };
        let config = DriverConfig {
            registry_address: Address::repeat_byte(0x01),
            poll_interval: Duration::from_secs(1),
            prover_timeout: Duration::from_secs(1),
            cancel: CancellationToken::new(),
        };
        RegistrationDriver::new(StubDiscovery, StubProofProvider, registry, tx, config)
    }

    // ── Calldata encoding tests ─────────────────────────────────────────

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

    // ── deregister_orphans tests ────────────────────────────────────────

    #[rstest]
    #[case::no_orphans(&[0xAA, 0xBB], &[0xAA, 0xBB], 0)]
    #[case::one_orphan(&[0xAA, 0xBB], &[0xAA], 1)]
    #[case::all_orphans(&[0xAA, 0xBB], &[], 2)]
    #[tokio::test]
    async fn deregister_orphans_tx_count(
        #[case] registered_bytes: &[u8],
        #[case] active_bytes: &[u8],
        #[case] expected_txs: usize,
    ) {
        let registered: Vec<Address> =
            registered_bytes.iter().map(|b| Address::repeat_byte(*b)).collect();
        let active: HashSet<Address> =
            active_bytes.iter().map(|b| Address::repeat_byte(*b)).collect();

        let tx = SharedTxManager::new();
        let driver = driver_with_shared_tx(registered, tx.clone());

        driver.deregister_orphans(&active).await.unwrap();

        assert_eq!(tx.sent_calldata().len(), expected_txs);
    }

    #[tokio::test]
    async fn deregister_orphans_calldata_targets_orphan() {
        let active_signer = Address::repeat_byte(0xAA);
        let orphan = Address::repeat_byte(0xBB);

        let tx = SharedTxManager::new();
        let driver = driver_with_shared_tx(vec![active_signer, orphan], tx.clone());

        driver.deregister_orphans(&HashSet::from([active_signer])).await.unwrap();

        let sent = tx.sent_calldata();
        let expected = ITEEProverRegistry::deregisterSignerCall { signer: orphan }.abi_encode();
        assert_eq!(sent[0], Bytes::from(expected));
    }

    #[tokio::test]
    async fn deregister_orphans_respects_cancellation() {
        let tx = SharedTxManager::new();
        let cancel = CancellationToken::new();
        let registry = MockRegistry { signers: vec![Address::repeat_byte(0xAA)] };
        let config = DriverConfig {
            registry_address: Address::repeat_byte(0x01),
            poll_interval: Duration::from_secs(1),
            prover_timeout: Duration::from_secs(1),
            cancel: cancel.clone(),
        };
        let driver =
            RegistrationDriver::new(StubDiscovery, StubProofProvider, registry, tx.clone(), config);

        cancel.cancel();
        driver.deregister_orphans(&HashSet::new()).await.unwrap();

        assert!(tx.sent_calldata().is_empty(), "no txs should be sent after cancellation");
    }
}
