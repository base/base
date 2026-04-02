//! Registration driver — core orchestration loop.
//!
//! Discovers prover instances, checks on-chain registration status, generates
//! ZK proofs for unregistered signers, and submits registration transactions
//! to L1 via the [`TxManager`]. Also detects orphaned on-chain signers (those
//! no longer backed by a healthy instance) and deregisters them.

use std::{collections::HashSet, error::Error, fmt, time::Duration};

use alloy_primitives::{Address, Bytes, hex};
use alloy_sol_types::SolCall;
use base_proof_contracts::ITEEProverRegistry;
use base_proof_tee_nitro_attestation_prover::AttestationProofProvider;
use base_tx_manager::{TxCandidate, TxManager};
use futures::stream::StreamExt;
use rand::random;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, info_span, warn};

use crate::{
    InstanceDiscovery, InstanceHealthStatus, ProverClient, ProverInstance, RegistrarError,
    RegistrarMetrics, RegistryClient, Result, SignerClient,
};

/// Default maximum number of instances processed concurrently.
///
/// Each instance may trigger a ~20-minute Boundless proof generation, so
/// limiting concurrency prevents overwhelming the proof service and keeps
/// nonce management tractable. The default allows moderate parallelism
/// while keeping resource usage bounded.
pub const DEFAULT_MAX_CONCURRENCY: usize = 4;

/// Default maximum number of transaction submission retries for transient
/// errors before giving up.
pub const DEFAULT_MAX_TX_RETRIES: u32 = 3;

/// Default delay between transaction submission retries.
pub const DEFAULT_TX_RETRY_DELAY_SECS: u64 = 5;

/// Default duration (in seconds) after launch during which unhealthy
/// instances are still eligible for registration.
///
/// New EC2 instances may fail ALB health checks while the application is
/// still initializing. This window allows the registrar to attempt
/// registration during that warm-up period rather than waiting for the
/// instance to become healthy. Set to 0 to disable.
pub const DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS: u64 = 3600;

/// Runtime parameters for the [`RegistrationDriver`] that are not
/// trait-based dependencies.
#[derive(Debug, Clone)]
pub struct DriverConfig {
    /// `TEEProverRegistry` contract address on L1.
    pub registry_address: Address,
    /// Interval between discovery and registration poll cycles.
    pub poll_interval: Duration,
    /// Cancellation token for graceful shutdown.
    pub cancel: CancellationToken,
    /// Maximum number of instances to process concurrently. Each instance
    /// may trigger proof generation, so this bounds concurrent proof work
    /// and nonce acquisition. Defaults to [`DEFAULT_MAX_CONCURRENCY`].
    pub max_concurrency: usize,
    /// Maximum number of transaction submission retries for transient errors.
    /// Defaults to [`DEFAULT_MAX_TX_RETRIES`].
    pub max_tx_retries: u32,
    /// Delay between transaction submission retries.
    /// Defaults to [`DEFAULT_TX_RETRY_DELAY_SECS`] seconds.
    pub tx_retry_delay: Duration,
    /// Duration after launch during which unhealthy instances are still
    /// eligible for registration. New instances may fail ALB health checks
    /// while the application is still initializing. Set to zero to disable.
    /// Defaults to [`DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS`] seconds.
    pub unhealthy_registration_window: Duration,
}

/// Core registration loop tying together discovery, attestation polling,
/// ZK proof generation, and on-chain submission.
///
/// Generic over the discovery, proof generation, registry, transaction
/// manager, and signer client backends so each can be mocked independently
/// in tests.
pub struct RegistrationDriver<D, P, R, T, S> {
    discovery: D,
    proof_provider: P,
    registry: R,
    tx_manager: T,
    signer_client: S,
    config: DriverConfig,
}

impl<D, P, R, T, S> fmt::Debug for RegistrationDriver<D, P, R, T, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistrationDriver").field("config", &self.config).finish_non_exhaustive()
    }
}

impl<D, P, R, T, S> RegistrationDriver<D, P, R, T, S>
where
    D: InstanceDiscovery,
    P: AttestationProofProvider,
    R: RegistryClient,
    T: TxManager,
    S: SignerClient,
{
    /// Creates a new registration driver.
    pub const fn new(
        discovery: D,
        proof_provider: P,
        registry: R,
        tx_manager: T,
        signer_client: S,
        config: DriverConfig,
    ) -> Self {
        Self { discovery, proof_provider, registry, tx_manager, signer_client, config }
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
                RegistrarMetrics::processing_errors_total().increment(1);
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

    /// Single registration cycle: discover → resolve addresses → register →
    /// deregister orphans.
    async fn step(&self) -> Result<()> {
        let instances = self.discovery.discover_instances().await?;
        RegistrarMetrics::discovery_success_total().increment(1);

        if !instances.is_empty() {
            let registerable =
                instances.iter().filter(|i| i.health_status.should_register()).count();
            info!(
                total = instances.len(),
                registerable = registerable,
                "discovered prover instances"
            );
        }

        // Process all instances concurrently. Proof generation (~20 min via
        // Boundless) is the bottleneck — running instances in parallel overlaps
        // these waits and dramatically reduces total cycle time.
        //
        // Resolve signer addresses for ALL reachable instances (regardless of
        // health status) to build a complete active set. This protects draining
        // instances (still running, usually reachable) from premature
        // deregistration. Truly unreachable instances will fail the RPC and be
        // excluded — the majority guard below is the safeguard for that case.
        // A signer-address cache across cycles would strengthen this but adds
        // state management complexity; deferred for now.
        // Registration is only attempted for instances that pass should_register().
        let mut active_signers = HashSet::new();
        let mut reachable_instances = 0usize;

        let concurrency = self.config.max_concurrency.max(1);
        let mut futs = futures::stream::iter(instances.iter().map(|instance| {
            let span = info_span!(
                "process_instance",
                instance_id = %instance.instance_id,
                endpoint = %instance.endpoint,
                health = ?instance.health_status,
            );
            async move { (instance, self.process_instance(instance).await) }.instrument(span)
        }))
        .buffer_unordered(concurrency);

        // Use `tokio::select!` so cancellation is observed immediately, even
        // when all futures are blocked on long-running proof generation (~20 min).
        // Without this, shutdown would hang until at least one future completes.
        //
        // NOTE: When the cancellation branch fires, `futs` is dropped, which
        // cancels any in-flight futures — including those awaiting
        // `tx_manager.send()` inside `try_register`. Dropping `send()` after
        // nonce acquisition but before signing can leave a nonce gap. This is
        // benign during shutdown because the next startup fetches a fresh nonce
        // from chain. If the service ever needs cancel-and-restart within the
        // same process (e.g. hot reconfiguration), a `NonceManager::reset()`
        // would be needed.
        loop {
            tokio::select! {
                biased;
                () = self.config.cancel.cancelled() => {
                    debug!("shutdown requested during instance processing");
                    break;
                }
                maybe_result = futs.next() => {
                    match maybe_result {
                        None => break, // all futures completed
                        Some((instance, result)) => {
                            match result {
                                Ok(addresses) => {
                                    reachable_instances += 1;
                                    for addr in addresses {
                                        active_signers.insert(addr);
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        error = %e,
                                        instance = %instance.instance_id,
                                        endpoint = %instance.endpoint,
                                        "failed to resolve signer addresses"
                                    );
                                    RegistrarMetrics::processing_errors_total().increment(1);
                                }
                            }
                        }
                    }
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
        // proceeding with orphan cleanup. The comparison uses instance counts
        // (not signer counts) so multi-enclave instances don't inflate the ratio.
        // When discovery returns zero instances (e.g. after ASG scale-down removes
        // them from the target group), deregistration proceeds normally — scaled-down
        // instances leave the target group entirely, so they don't inflate
        // `instances.len()`.
        if !instances.is_empty() && reachable_instances.saturating_mul(2) <= instances.len() {
            warn!(
                reachable = reachable_instances,
                total = instances.len(),
                "majority of instances unreachable, skipping orphan deregistration"
            );
            return Ok(());
        }

        let registered_signers = self.registry.get_registered_signers().await?;

        if let Err(e) = self.deregister_orphans(&active_signers, &registered_signers).await {
            warn!(error = %e, "failed to deregister orphan signers");
            RegistrarMetrics::processing_errors_total().increment(1);
        }

        Ok(())
    }

    /// Returns `true` if the instance is [`InstanceHealthStatus::Unhealthy`]
    /// and was launched within the configured
    /// [`DriverConfig::unhealthy_registration_window`].
    ///
    /// New EC2 instances may fail ALB health checks while the application is
    /// still initializing. This predicate lets the registrar attempt
    /// registration during that warm-up period rather than waiting for the
    /// instance to become healthy.
    ///
    /// Returns `false` if:
    /// - The instance is not `Unhealthy` (other statuses have their own rules).
    /// - The window is zero (feature disabled).
    /// - The instance has no launch time (e.g. discovery didn't return one).
    /// - The launch time is in the future (clock skew — treated as unknown).
    fn is_recently_launched_unhealthy(&self, instance: &ProverInstance) -> bool {
        if instance.health_status != InstanceHealthStatus::Unhealthy {
            return false;
        }
        if self.config.unhealthy_registration_window.is_zero() {
            return false;
        }
        instance.launch_time.is_some_and(|lt| {
            lt.elapsed().is_ok_and(|elapsed| elapsed < self.config.unhealthy_registration_window)
        })
    }

    /// Resolves signer addresses from an instance and attempts registration.
    ///
    /// Returns the derived signer addresses regardless of whether registration
    /// was needed or succeeded, so the caller can build the active signer set.
    /// Registration failures are logged but do not prevent the addresses from
    /// being returned.
    async fn process_instance(&self, instance: &ProverInstance) -> Result<Vec<Address>> {
        let public_keys = self.signer_client.signer_public_key(&instance.endpoint).await?;
        let mut addresses = Vec::with_capacity(public_keys.len());

        for public_key in &public_keys {
            addresses.push(ProverClient::derive_address(public_key)?);
        }

        // Only attempt registration for instances that pass should_register().
        // Non-registerable instances (Draining, Unhealthy) still contribute
        // their addresses to the active signer set to prevent premature
        // deregistration.
        //
        // Exception: recently-launched Unhealthy instances are allowed through
        // when they fall within the configured unhealthy_registration_window.
        // New instances may fail ALB health checks during startup while the
        // application is still initializing.
        if !instance.health_status.should_register() {
            if !self.is_recently_launched_unhealthy(instance) {
                debug!(
                    status = ?instance.health_status,
                    instance = %instance.instance_id,
                    "instance not registerable, skipping registration"
                );
                return Ok(addresses);
            }
            info!(
                instance = %instance.instance_id,
                launch_time = ?instance.launch_time,
                window = ?self.config.unhealthy_registration_window,
                "unhealthy instance recently launched, attempting registration"
            );
        }

        // Fetch attestations once for all enclaves before the registration
        // loop. Each signer_attestation RPC hits NSM hardware on the enclave
        // side, so fetching per-enclave would generate N×N attestation documents
        // for N enclaves. A single nonce binds the entire batch for freshness.
        let nonce: [u8; 32] = random();
        info!(
            nonce = %hex::encode(nonce),
            instance = %instance.instance_id,
            "requesting attestations with nonce"
        );
        let all_attestations = self
            .signer_client
            .signer_attestation(&instance.endpoint, None, Some(nonce.to_vec()))
            .await?;

        if all_attestations.len() < addresses.len() {
            return Err(RegistrarError::ProverClient {
                instance: instance.endpoint.to_string(),
                source: format!(
                    "expected {} attestations but got {}",
                    addresses.len(),
                    all_attestations.len()
                )
                .into(),
            });
        }

        for (idx, &signer_address) in addresses.iter().enumerate() {
            if let Err(e) =
                self.try_register(instance, signer_address, idx, &all_attestations[idx]).await
            {
                error!(
                    error = %e,
                    error_source = e.source().map(|s| s.to_string()).unwrap_or_default(),
                    error_debug = ?e,
                    signer = %signer_address,
                    enclave_index = idx,
                    instance = %instance.instance_id,
                    "registration attempt failed"
                );
                RegistrarMetrics::processing_errors_total().increment(1);
            }
        }

        Ok(addresses)
    }

    /// Attempts to register a signer on-chain if not already registered.
    ///
    /// This is the expensive path: checks on-chain status, generates a ZK
    /// proof from the pre-fetched attestation, and submits a registration
    /// transaction.
    ///
    /// Registration is PCR0-agnostic: all legitimate enclaves are registered
    /// regardless of their PCR0 measurement. This enables pre-registration of
    /// new-PCR0 enclaves before a hardfork, eliminating the proof-generation
    /// delay when the on-chain `TEE_IMAGE_HASH` rotates. The on-chain
    /// `TEEVerifier` gates proof acceptance on `TEE_IMAGE_HASH` at submission
    /// time, so pre-registered enclaves cannot produce accepted proposals
    /// until the hardfork activates.
    async fn try_register(
        &self,
        instance: &ProverInstance,
        signer_address: Address,
        enclave_index: usize,
        attestation_bytes: &[u8],
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
            enclave_index,
            instance = %instance.instance_id,
            "generating proof for unregistered signer"
        );

        let proof = self
            .proof_provider
            .generate_proof_for_signer(attestation_bytes, signer_address)
            .await?;

        // Check cancellation before submitting the transaction — avoid starting
        // new on-chain work if shutdown is in progress.
        if self.config.cancel.is_cancelled() {
            debug!("shutdown requested, skipping transaction submission");
            return Ok(());
        }

        let calldata = Bytes::from(
            ITEEProverRegistry::registerSignerCall {
                output: proof.output,
                proofBytes: proof.proof_bytes,
            }
            .abi_encode(),
        );

        info!(
            signer = %signer_address,
            registry = %self.config.registry_address,
            calldata_len = calldata.len(),
            "Registering signer"
        );

        let candidate = TxCandidate {
            tx_data: calldata,
            to: Some(self.config.registry_address),
            ..Default::default()
        };

        info!(
            tx = ?candidate,
            "Sending tx candidate",
        );

        // Retry tx submission on transient errors to avoid discarding an
        // expensive proof (~20 min Boundless generation) on a nonce race
        // or brief network blip.
        //
        // Only errors that `TxManagerError::is_retryable()` considers
        // transient are retried.  Deterministic failures (execution
        // reverted, insufficient funds, config errors, fee limits, etc.)
        // abort immediately since retrying with the same calldata and
        // state cannot succeed.
        let max_tx_retries = self.config.max_tx_retries;
        let tx_retry_delay = self.config.tx_retry_delay;
        let mut tx_retries = 0;

        let receipt = loop {
            // Check cancellation at the top of each iteration to avoid
            // starting new on-chain work after shutdown is requested.
            if self.config.cancel.is_cancelled() {
                debug!("shutdown requested, aborting tx submission");
                return Ok(());
            }

            match self.tx_manager.send(candidate.clone()).await {
                Ok(receipt) => break receipt,
                Err(e) => {
                    // The signer may already be registered despite the error
                    // (e.g. the tx was mined but the tx manager reported a
                    // nonce race during fee bumping). Check on-chain state.
                    match self.registry.is_registered(signer_address).await {
                        Ok(true) => {
                            info!(
                                signer = %signer_address,
                                error = %e,
                                "tx error but signer is registered on-chain, treating as success"
                            );
                            RegistrarMetrics::registrations_total().increment(1);
                            return Ok(());
                        }
                        Err(registry_err) => {
                            warn!(
                                error = %registry_err,
                                signer = %signer_address,
                                "failed to query is_registered after tx error"
                            );
                        }
                        Ok(false) => {}
                    }

                    // Non-retryable errors (execution reverts, insufficient
                    // funds, config errors, fee limits, etc.) cannot be
                    // resolved by retrying with the same calldata.
                    if !e.is_retryable() {
                        return Err(RegistrarError::from(e));
                    }

                    tx_retries += 1;
                    if tx_retries > max_tx_retries {
                        return Err(RegistrarError::from(e));
                    }

                    warn!(
                        error = %e,
                        signer = %signer_address,
                        retry = tx_retries,
                        max_retries = max_tx_retries,
                        "tx submission failed, retrying with same proof"
                    );

                    // Cancellation-aware delay: abort immediately if
                    // shutdown is requested during the retry wait.
                    tokio::select! {
                        () = self.config.cancel.cancelled() => {
                            debug!("shutdown requested during retry delay");
                            return Err(RegistrarError::from(e));
                        }
                        () = tokio::time::sleep(tx_retry_delay) => {}
                    }
                }
            }
        };

        if !receipt.inner.status() {
            warn!(
                signer = %signer_address,
                tx_hash = %receipt.transaction_hash,
                "registration transaction reverted onchain",
            );
            return Err(RegistrarError::Transaction(
                format!("registration transaction {} reverted", receipt.transaction_hash,).into(),
            ));
        }

        info!(
            signer = %signer_address,
            tx_hash = %receipt.transaction_hash,
            "signer registered successfully"
        );
        RegistrarMetrics::registrations_total().increment(1);

        Ok(())
    }

    /// Submits a `deregisterSigner` transaction and returns whether it succeeded.
    async fn submit_deregistration(&self, signer: Address) -> bool {
        let calldata =
            Bytes::from(ITEEProverRegistry::deregisterSignerCall { signer }.abi_encode());

        info!(
            signer = %signer,
            registry = %self.config.registry_address,
            calldata_len = calldata.len(),
            "Deregistering signer"
        );

        let candidate = TxCandidate {
            tx_data: calldata,
            to: Some(self.config.registry_address),
            ..Default::default()
        };

        info!(
            tx = ?candidate,
            "Sending tx candidate",
        );

        match self.tx_manager.send(candidate).await {
            Ok(receipt) => {
                if !receipt.inner.status() {
                    warn!(
                        signer = %signer,
                        tx_hash = %receipt.transaction_hash,
                        "deregistration transaction reverted onchain",
                    );
                    RegistrarMetrics::processing_errors_total().increment(1);
                    return false;
                }
                info!(
                    signer = %signer,
                    tx_hash = %receipt.transaction_hash,
                    "signer deregistered"
                );
                true
            }
            Err(e) => {
                warn!(error = %e, signer = %signer, "failed to deregister signer");
                RegistrarMetrics::processing_errors_total().increment(1);
                false
            }
        }
    }

    /// Deregisters any on-chain signer that is not in the `active_signers` set.
    ///
    /// These orphans arise when a prover instance is terminated (e.g. ASG
    /// scale-down) without first deregistering its signer on-chain.
    ///
    /// # Defense in depth
    ///
    /// Before submitting a deregistration transaction, each orphan candidate is
    /// verified via [`RegistryClient::is_registered`] (backed by the
    /// `isRegisteredSigner` mapping). This guards against ghost entries in the
    /// on-chain `EnumerableSetLib.AddressSet` that can appear after certain
    /// add/remove sequences due to a bug in Solady v0.0.245. Without this
    /// check, ghost addresses would be deregistered every cycle in an infinite
    /// loop, burning gas without effect.
    ///
    /// # Assumptions
    ///
    /// - **Single registrar**: This method queries *all* on-chain signers and
    ///   treats any signer not in `active_signers` as an orphan. If multiple
    ///   registrar instances manage disjoint prover fleets, one registrar would
    ///   incorrectly deregister another's signers. The current deployment model
    ///   assumes a single registrar per registry contract.
    async fn deregister_orphans(
        &self,
        active_signers: &HashSet<Address>,
        registered_signers: &[Address],
    ) -> Result<()> {
        let orphans: Vec<_> = registered_signers
            .iter()
            .copied()
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

            // Verify the signer is truly registered on-chain before spending
            // gas on a deregistration tx. The `getRegisteredSigners()` view
            // reads from an `EnumerableSetLib.AddressSet` which can contain
            // ghost entries (addresses that appear in `values()` but have
            // `isRegisteredSigner == false`) due to a storage corruption bug
            // in Solady v0.0.245. Skipping ghosts prevents an infinite
            // deregistration loop.
            match self.registry.is_registered(signer).await {
                Ok(false) => {
                    warn!(
                        signer = %signer,
                        "signer appears in getRegisteredSigners but isRegisteredSigner is false, \
                         skipping (possible EnumerableSet ghost entry)"
                    );
                    continue;
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        signer = %signer,
                        "failed to verify signer registration status, skipping deregistration"
                    );
                    continue;
                }
                Ok(true) => {}
            }

            if self.submit_deregistration(signer).await {
                RegistrarMetrics::deregistrations_total().increment(1);
                deregistered += 1;
            }
        }

        info!(count = deregistered, "orphan deregistration complete");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        sync::{
            Arc, Mutex,
            atomic::{AtomicU32, AtomicUsize, Ordering},
        },
        time::SystemTime,
    };

    use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
    use alloy_primitives::{Address, B256, Bloom, Bytes, address};
    use alloy_rpc_types_eth::TransactionReceipt;
    use alloy_sol_types::SolCall;
    use async_trait::async_trait;
    use base_proof_tee_nitro_attestation_prover::AttestationProof;
    use base_tx_manager::{SendHandle, TxCandidate, TxManager, TxManagerError};
    use hex_literal::hex;
    use k256::ecdsa::SigningKey;
    use rstest::rstest;
    use tokio_util::sync::CancellationToken;
    use url::Url;

    use super::*;
    use crate::{InstanceHealthStatus, RegistryClient, Result, SignerClient};

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

    /// Well-known Hardhat / Anvil account #0 private key.
    const HARDHAT_KEY_0: [u8; 32] =
        hex!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

    /// Hardhat / Anvil account #1 private key.
    const HARDHAT_KEY_1: [u8; 32] =
        hex!("59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d");

    /// Hardhat / Anvil account #2 private key.
    const HARDHAT_KEY_2: [u8; 32] =
        hex!("5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a");

    /// Hardhat / Anvil account #3 private key.
    const HARDHAT_KEY_3: [u8; 32] =
        hex!("7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6");

    /// Prover instance endpoints for tests. Each simulates a distinct
    /// EC2 instance at a private IP.
    const EP1: &str = "10.0.0.1:8000";
    const EP2: &str = "10.0.0.2:8000";
    const EP3: &str = "10.0.0.3:8000";
    const EP4: &str = "10.0.0.4:8000";

    /// Synthetic orphan addresses for deregistration tests.
    /// Each uses `Address::repeat_byte` for deterministic, readable values.
    const ORPHAN_A: Address = Address::repeat_byte(0xAA);
    const ORPHAN_B: Address = Address::repeat_byte(0xBB);
    const ORPHAN_C: Address = Address::repeat_byte(0xCC);
    const ORPHAN_D: Address = Address::repeat_byte(0xDD);
    const ORPHAN_E: Address = Address::repeat_byte(0xEE);

    /// Placeholder registry contract address used in `DriverConfig`.
    const TEST_REGISTRY_ADDRESS: Address = Address::repeat_byte(0x01);

    // ── Test helpers ─────────────────────────────────────────────────────

    /// Derives the uncompressed 65-byte public key from a private key.
    fn public_key_from_private(private_key: &[u8; 32]) -> Vec<u8> {
        let signing_key = SigningKey::from_slice(private_key).unwrap();
        signing_key.verifying_key().to_encoded_point(false).as_bytes().to_vec()
    }

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

    /// Builds a [`ProverInstance`] with the given host:port and health status.
    ///
    /// Prepends `http://` to form a valid URL automatically. The `launch_time`
    /// defaults to `None` — use [`instance_with_launch_time`] for tests that
    /// need a specific launch time.
    fn instance(host_port: &str, status: InstanceHealthStatus) -> ProverInstance {
        let endpoint = Url::parse(&format!("http://{host_port}")).unwrap();
        ProverInstance {
            instance_id: format!("i-{host_port}"),
            endpoint,
            health_status: status,
            launch_time: None,
        }
    }

    /// Builds a [`ProverInstance`] with an explicit `launch_time`.
    fn instance_with_launch_time(
        host_port: &str,
        status: InstanceHealthStatus,
        launch_time: Option<SystemTime>,
    ) -> ProverInstance {
        let endpoint = Url::parse(&format!("http://{host_port}")).unwrap();
        ProverInstance {
            instance_id: format!("i-{host_port}"),
            endpoint,
            health_status: status,
            launch_time,
        }
    }

    // ── Mock implementations ────────────────────────────────────────────

    /// Configurable mock discovery that returns a pre-set list of instances.
    #[derive(Debug)]
    struct MockDiscovery {
        instances: Vec<ProverInstance>,
    }

    #[async_trait]
    impl InstanceDiscovery for MockDiscovery {
        async fn discover_instances(&self) -> Result<Vec<ProverInstance>> {
            Ok(self.instances.clone())
        }
    }

    /// Mock proof provider that returns a dummy proof.
    #[derive(Debug)]
    struct StubProofProvider;

    #[async_trait]
    impl AttestationProofProvider for StubProofProvider {
        async fn generate_proof(
            &self,
            _attestation_bytes: &[u8],
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            Ok(AttestationProof {
                output: Bytes::from_static(b"stub-output"),
                proof_bytes: Bytes::from_static(b"stub-proof"),
            })
        }
    }

    /// Mock proof provider that always fails, simulating Boundless errors.
    #[derive(Debug)]
    struct FailingProofProvider;

    #[async_trait]
    impl AttestationProofProvider for FailingProofProvider {
        async fn generate_proof(
            &self,
            _attestation_bytes: &[u8],
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            Err(base_proof_tee_nitro_attestation_prover::ProverError::Boundless(
                "simulated proof failure".into(),
            ))
        }
    }

    /// Mock signer client that returns pre-configured public keys and attestations
    /// per endpoint.
    ///
    /// If an endpoint is not in the `keys` map, the call returns an error
    /// (simulating an unreachable instance).
    #[derive(Debug)]
    struct MockSignerClient {
        /// Maps endpoint URL → list of uncompressed public key bytes (one per enclave).
        keys: HashMap<Url, Vec<Vec<u8>>>,
        /// Maps endpoint URL → list of attestation blobs (one per enclave).
        /// Falls back to `b"mock-attestation"` if not configured.
        attestations: HashMap<Url, Vec<Vec<u8>>>,
    }

    impl MockSignerClient {
        /// Creates a mock with the given host:port-to-private-key mappings.
        /// Each endpoint gets a single enclave key wrapped in a Vec.
        /// The public key is derived automatically from each private key.
        /// An `http://` scheme is prepended to each host:port string.
        fn from_keys(entries: &[(&str, &[u8; 32])]) -> Self {
            let keys = entries
                .iter()
                .map(|(ep, pk)| {
                    let url = Url::parse(&format!("http://{ep}")).unwrap();
                    (url, vec![public_key_from_private(pk)])
                })
                .collect();
            Self { keys, attestations: HashMap::new() }
        }

        /// Creates a mock that returns multiple public keys for a single endpoint,
        /// simulating a multi-enclave instance.
        fn multi_enclave(host_port: &str, private_keys: &[&[u8; 32]]) -> Self {
            let url = Url::parse(&format!("http://{host_port}")).unwrap();
            let pubs = private_keys.iter().map(|pk| public_key_from_private(pk)).collect();
            Self { keys: HashMap::from([(url, pubs)]), attestations: HashMap::new() }
        }

        /// Configures attestation blobs for a given endpoint.
        fn with_attestations(mut self, host_port: &str, attestations: Vec<Vec<u8>>) -> Self {
            let url = Url::parse(&format!("http://{host_port}")).unwrap();
            self.attestations.insert(url, attestations);
            self
        }
    }

    #[async_trait]
    impl SignerClient for MockSignerClient {
        async fn signer_public_key(&self, endpoint: &Url) -> Result<Vec<Vec<u8>>> {
            self.keys.get(endpoint).cloned().ok_or_else(|| RegistrarError::ProverClient {
                instance: endpoint.to_string(),
                source: "unreachable".into(),
            })
        }

        async fn signer_attestation(
            &self,
            endpoint: &Url,
            _user_data: Option<Vec<u8>>,
            _nonce: Option<Vec<u8>>,
        ) -> Result<Vec<Vec<u8>>> {
            if let Some(atts) = self.attestations.get(endpoint) {
                return Ok(atts.clone());
            }
            // Default: one dummy attestation per key at this endpoint.
            let count = self.keys.get(endpoint).map_or(1, |k| k.len());
            Ok(vec![b"mock-attestation".to_vec(); count])
        }
    }

    /// Mock registry that returns a configured set of registered signers.
    ///
    /// By default, `is_registered` checks membership in the `signers` list
    /// (matching real contract behavior). When `all_registered` is set, it
    /// returns `true` unconditionally — useful for `try_register` tests that
    /// need to short-circuit the registration path.
    #[derive(Debug)]
    struct MockRegistry {
        signers: Vec<Address>,
        /// When `true`, `is_registered` returns `true` for all queries,
        /// regardless of `signers` membership. Used by tests that need the
        /// "already registered" path in `try_register`.
        all_registered: bool,
    }

    impl MockRegistry {
        fn with_signers(signers: Vec<Address>) -> Self {
            Self { signers, all_registered: false }
        }

        fn all_registered(signers: Vec<Address>) -> Self {
            Self { signers, all_registered: true }
        }
    }

    #[async_trait]
    impl RegistryClient for MockRegistry {
        async fn is_registered(&self, signer: Address) -> Result<bool> {
            if self.all_registered {
                return Ok(true);
            }
            Ok(self.signers.contains(&signer))
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
            unimplemented!("not used in tests")
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    /// Stub signer client that is unused by `deregister_orphans` tests.
    #[derive(Debug)]
    struct StubSignerClient;

    #[async_trait]
    impl SignerClient for StubSignerClient {
        async fn signer_public_key(&self, _endpoint: &Url) -> Result<Vec<Vec<u8>>> {
            unimplemented!("not used in deregister_orphans tests")
        }

        async fn signer_attestation(
            &self,
            _endpoint: &Url,
            _user_data: Option<Vec<u8>>,
            _nonce: Option<Vec<u8>>,
        ) -> Result<Vec<Vec<u8>>> {
            unimplemented!("not used in deregister_orphans tests")
        }
    }

    // ── Driver constructors ─────────────────────────────────────────────

    fn default_config(cancel: CancellationToken) -> DriverConfig {
        DriverConfig {
            registry_address: TEST_REGISTRY_ADDRESS,
            poll_interval: Duration::from_secs(1),
            cancel,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            max_tx_retries: DEFAULT_MAX_TX_RETRIES,
            tx_retry_delay: Duration::from_secs(DEFAULT_TX_RETRY_DELAY_SECS),
            unhealthy_registration_window: Duration::from_secs(
                DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS,
            ),
        }
    }

    /// Builds a driver for `deregister_orphans` tests (no signer client needed).
    fn driver_with_shared_tx(
        registered_signers: Vec<Address>,
        tx: SharedTxManager,
    ) -> RegistrationDriver<
        MockDiscovery,
        StubProofProvider,
        MockRegistry,
        SharedTxManager,
        StubSignerClient,
    > {
        let registry = MockRegistry::with_signers(registered_signers);
        RegistrationDriver::new(
            MockDiscovery { instances: vec![] },
            StubProofProvider,
            registry,
            tx,
            StubSignerClient,
            default_config(CancellationToken::new()),
        )
    }

    /// Builds a fully-configured driver for `step()` / `process_instance()` tests.
    fn step_driver(
        instances: Vec<ProverInstance>,
        signer_client: MockSignerClient,
        registry: MockRegistry,
        tx: SharedTxManager,
        cancel: CancellationToken,
    ) -> RegistrationDriver<
        MockDiscovery,
        StubProofProvider,
        MockRegistry,
        SharedTxManager,
        MockSignerClient,
    > {
        RegistrationDriver::new(
            MockDiscovery { instances },
            StubProofProvider,
            registry,
            tx,
            signer_client,
            default_config(cancel),
        )
    }

    // ── Configurable mock types for retry tests ────────────────────────

    /// Maximum number of tx submission retries used by `default_config`.
    const MAX_TX_RETRIES: u32 = DEFAULT_MAX_TX_RETRIES;

    /// Proof provider that counts `generate_proof` invocations.
    ///
    /// Returns the same stub proof as [`StubProofProvider`] but tracks
    /// how many times it was called, allowing tests to assert that the
    /// expensive proof generation is not repeated across retries.
    #[derive(Debug)]
    struct CountingProofProvider {
        call_count: AtomicU32,
    }

    impl CountingProofProvider {
        fn new() -> Self {
            Self { call_count: AtomicU32::new(0) }
        }

        fn call_count(&self) -> u32 {
            self.call_count.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl AttestationProofProvider for CountingProofProvider {
        async fn generate_proof(
            &self,
            _attestation_bytes: &[u8],
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            Ok(AttestationProof {
                output: Bytes::from_static(b"stub-output"),
                proof_bytes: Bytes::from_static(b"stub-proof"),
            })
        }
    }

    /// Mock tx manager that returns a configurable sequence of results.
    ///
    /// Each call to `send()` pops the next result from `results`. When
    /// the queue is exhausted, returns a successful receipt.
    #[derive(Debug, Clone)]
    struct FailingTxManager {
        /// FIFO queue of results to return; `None` means success.
        results: Arc<Mutex<VecDeque<Option<TxManagerError>>>>,
        /// Records all submitted calldata for assertion.
        sent: Arc<Mutex<Vec<Bytes>>>,
    }

    impl FailingTxManager {
        /// Creates a manager that returns the given errors in order,
        /// then succeeds on subsequent calls.
        fn with_errors(errors: Vec<TxManagerError>) -> Self {
            let results = errors.into_iter().map(Some).collect();
            Self { results: Arc::new(Mutex::new(results)), sent: Arc::new(Mutex::new(vec![])) }
        }

        /// Returns the number of `send()` calls made.
        fn send_count(&self) -> usize {
            self.sent.lock().unwrap().len()
        }

        /// Returns all submitted calldata for equality assertions.
        fn sent_calldata(&self) -> Vec<Bytes> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl TxManager for FailingTxManager {
        async fn send(&self, candidate: TxCandidate) -> base_tx_manager::SendResponse {
            self.sent.lock().unwrap().push(candidate.tx_data);
            let next = self.results.lock().unwrap().pop_front();
            match next {
                Some(Some(e)) => Err(e),
                _ => Ok(stub_receipt()),
            }
        }

        async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
            panic!("FailingTxManager::send_async is not implemented; retry tests only use send()")
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    /// Mock registry with dynamic `is_registered` responses.
    ///
    /// The first N calls to `is_registered` return values from `responses`;
    /// subsequent calls return `default_registered`.
    #[derive(Debug)]
    struct DynamicRegistry {
        /// On-chain signers for `get_registered_signers`.
        signers: Vec<Address>,
        /// FIFO queue of `is_registered` return values.
        responses: Mutex<VecDeque<bool>>,
        /// Value returned after `responses` is exhausted.
        default_registered: bool,
    }

    impl DynamicRegistry {
        /// Registry where `is_registered` always returns `false`.
        fn never_registered(signers: Vec<Address>) -> Self {
            Self { signers, responses: Mutex::new(VecDeque::new()), default_registered: false }
        }

        /// Registry where the first call returns `false` (initial check),
        /// then subsequent calls return `true` (signer appeared on-chain).
        fn registered_after_first_check(signers: Vec<Address>) -> Self {
            Self {
                signers,
                responses: Mutex::new(VecDeque::from([false])),
                default_registered: true,
            }
        }
    }

    #[async_trait]
    impl RegistryClient for DynamicRegistry {
        async fn is_registered(&self, _signer: Address) -> Result<bool> {
            let next = self.responses.lock().unwrap().pop_front();
            Ok(next.unwrap_or(self.default_registered))
        }

        async fn get_registered_signers(&self) -> Result<Vec<Address>> {
            Ok(self.signers.clone())
        }
    }

    /// Builds a driver for tx retry tests with configurable proof provider,
    /// tx manager, and registry.
    fn retry_driver<P: AttestationProofProvider>(
        signer_client: MockSignerClient,
        registry: DynamicRegistry,
        tx: FailingTxManager,
        proof_provider: P,
        cancel: CancellationToken,
    ) -> RegistrationDriver<MockDiscovery, P, DynamicRegistry, FailingTxManager, MockSignerClient>
    {
        RegistrationDriver::new(
            MockDiscovery { instances: vec![] },
            proof_provider,
            registry,
            tx,
            signer_client,
            default_config(cancel),
        )
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
    #[case::no_orphans(vec![ORPHAN_A, ORPHAN_B], vec![ORPHAN_A, ORPHAN_B], 0)]
    #[case::one_orphan(vec![ORPHAN_A, ORPHAN_B], vec![ORPHAN_A], 1)]
    #[case::all_orphans(vec![ORPHAN_A, ORPHAN_B], vec![], 2)]
    #[tokio::test]
    async fn deregister_orphans_tx_count(
        #[case] registered: Vec<Address>,
        #[case] active: Vec<Address>,
        #[case] expected_txs: usize,
    ) {
        let active: HashSet<Address> = active.into_iter().collect();

        let tx = SharedTxManager::new();
        let driver = driver_with_shared_tx(registered.clone(), tx.clone());

        driver.deregister_orphans(&active, &registered).await.unwrap();

        assert_eq!(tx.sent_calldata().len(), expected_txs);
    }

    #[tokio::test]
    async fn deregister_orphans_calldata_targets_orphan() {
        let registered = vec![ORPHAN_A, ORPHAN_B];
        let tx = SharedTxManager::new();
        let driver = driver_with_shared_tx(registered.clone(), tx.clone());

        driver.deregister_orphans(&HashSet::from([ORPHAN_A]), &registered).await.unwrap();

        let sent = tx.sent_calldata();
        let expected = ITEEProverRegistry::deregisterSignerCall { signer: ORPHAN_B }.abi_encode();
        assert_eq!(sent[0], Bytes::from(expected));
    }

    #[tokio::test]
    async fn deregister_orphans_respects_cancellation() {
        let tx = SharedTxManager::new();
        let cancel = CancellationToken::new();
        let registry = MockRegistry::with_signers(vec![ORPHAN_A]);
        let driver = RegistrationDriver::new(
            MockDiscovery { instances: vec![] },
            StubProofProvider,
            registry,
            tx.clone(),
            StubSignerClient,
            default_config(cancel.clone()),
        );

        let registered = vec![ORPHAN_A];
        cancel.cancel();
        driver.deregister_orphans(&HashSet::new(), &registered).await.unwrap();

        assert!(tx.sent_calldata().is_empty(), "no txs should be sent after cancellation");
    }

    /// Mock registry that simulates a corrupted `EnumerableSetLib.AddressSet`.
    ///
    /// `get_registered_signers()` returns `all_values` (including ghost entries),
    /// but `is_registered()` only returns `true` for addresses in
    /// `truly_registered`. This models the Solady v0.0.245 bug where
    /// `values()` contains stale addresses whose `isRegisteredSigner`
    /// mapping is `false`.
    #[derive(Debug)]
    struct GhostRegistry {
        /// Addresses returned by `getRegisteredSigners()` (includes ghosts).
        all_values: Vec<Address>,
        /// Addresses for which `isRegisteredSigner` is `true`.
        truly_registered: HashSet<Address>,
    }

    impl GhostRegistry {
        /// Creates a registry where `ghosts` appear in `values()` but have
        /// `isRegisteredSigner == false`, and `real` signers appear in both.
        fn new(real: Vec<Address>, ghosts: Vec<Address>) -> Self {
            let truly_registered: HashSet<Address> = real.iter().copied().collect();
            let mut all_values = real;
            all_values.extend(ghosts);
            Self { all_values, truly_registered }
        }
    }

    #[async_trait]
    impl RegistryClient for GhostRegistry {
        async fn is_registered(&self, signer: Address) -> Result<bool> {
            Ok(self.truly_registered.contains(&signer))
        }

        async fn get_registered_signers(&self) -> Result<Vec<Address>> {
            Ok(self.all_values.clone())
        }
    }

    #[tokio::test]
    async fn deregister_orphans_skips_ghost_entries() {
        // Simulates the Solady v0.0.245 EnumerableSetLib bug: ORPHAN_A is a
        // ghost entry that appears in getRegisteredSigners() but has
        // isRegisteredSigner == false. ORPHAN_B is a real orphan.
        let ghost_registry = GhostRegistry::new(vec![ORPHAN_B], vec![ORPHAN_A]);

        let tx = SharedTxManager::new();
        let driver = RegistrationDriver::new(
            MockDiscovery { instances: vec![] },
            StubProofProvider,
            ghost_registry,
            tx.clone(),
            StubSignerClient,
            default_config(CancellationToken::new()),
        );

        // Both ORPHAN_A and ORPHAN_B are "registered" (in values()),
        // neither is in active_signers.
        let registered = vec![ORPHAN_A, ORPHAN_B];
        driver.deregister_orphans(&HashSet::new(), &registered).await.unwrap();

        let sent = tx.sent_calldata();
        // Only ORPHAN_B should be deregistered; ORPHAN_A is a ghost.
        assert_eq!(sent.len(), 1, "ghost entry should be skipped");
        let expected = ITEEProverRegistry::deregisterSignerCall { signer: ORPHAN_B }.abi_encode();
        assert_eq!(sent[0], Bytes::from(expected));
    }

    #[tokio::test]
    async fn deregister_orphans_skips_all_ghosts_sends_nothing() {
        // All orphan candidates are ghost entries — no tx should be sent.
        let ghost_registry = GhostRegistry::new(vec![], vec![ORPHAN_A, ORPHAN_B, ORPHAN_C]);

        let tx = SharedTxManager::new();
        let driver = RegistrationDriver::new(
            MockDiscovery { instances: vec![] },
            StubProofProvider,
            ghost_registry,
            tx.clone(),
            StubSignerClient,
            default_config(CancellationToken::new()),
        );

        let registered = vec![ORPHAN_A, ORPHAN_B, ORPHAN_C];
        driver.deregister_orphans(&HashSet::new(), &registered).await.unwrap();

        assert!(tx.sent_calldata().is_empty(), "all ghosts should be skipped, no txs sent");
    }

    // ── process_instance tests ──────────────────────────────────────────

    #[rstest]
    #[case::healthy_unregistered(InstanceHealthStatus::Healthy, false, 1)]
    #[case::initial_unregistered(InstanceHealthStatus::Initial, false, 1)]
    #[case::draining(InstanceHealthStatus::Draining, false, 0)]
    #[case::unhealthy(InstanceHealthStatus::Unhealthy, false, 0)]
    #[case::already_registered(InstanceHealthStatus::Healthy, true, 0)]
    #[tokio::test]
    async fn process_instance_returns_address_and_correct_tx_count(
        #[case] status: InstanceHealthStatus,
        #[case] all_registered: bool,
        #[case] expected_txs: usize,
    ) {
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let tx = SharedTxManager::new();
        let registry = if all_registered {
            MockRegistry::all_registered(vec![])
        } else {
            MockRegistry::with_signers(vec![])
        };
        let driver =
            step_driver(vec![], signer_client, registry, tx.clone(), CancellationToken::new());

        let inst = instance(EP1, status);
        let addrs = driver.process_instance(&inst).await.unwrap();

        assert_eq!(addrs, vec![HARDHAT_ACCOUNT]);
        assert_eq!(tx.sent_calldata().len(), expected_txs);
    }

    // ── Unhealthy registration window tests ────────────────────────────

    #[tokio::test]
    async fn process_instance_unhealthy_recently_launched_attempts_registration() {
        // An Unhealthy instance launched 10 minutes ago (within the default
        // 60-minute window) should be registered.
        let launch_time = Some(SystemTime::now() - Duration::from_secs(600));
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let tx = SharedTxManager::new();
        let driver = step_driver(
            vec![],
            signer_client,
            MockRegistry::with_signers(vec![]),
            tx.clone(),
            CancellationToken::new(),
        );

        let inst = instance_with_launch_time(EP1, InstanceHealthStatus::Unhealthy, launch_time);
        let addrs = driver.process_instance(&inst).await.unwrap();

        assert_eq!(addrs, vec![HARDHAT_ACCOUNT]);
        assert_eq!(tx.sent_calldata().len(), 1, "recently-launched unhealthy should register");
    }

    #[tokio::test]
    async fn process_instance_unhealthy_old_launch_skips_registration() {
        // An Unhealthy instance launched 2 hours ago (outside the 60-minute
        // window) should NOT be registered.
        let launch_time = Some(SystemTime::now() - Duration::from_secs(7200));
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let tx = SharedTxManager::new();
        let driver = step_driver(
            vec![],
            signer_client,
            MockRegistry::with_signers(vec![]),
            tx.clone(),
            CancellationToken::new(),
        );

        let inst = instance_with_launch_time(EP1, InstanceHealthStatus::Unhealthy, launch_time);
        let addrs = driver.process_instance(&inst).await.unwrap();

        assert_eq!(addrs, vec![HARDHAT_ACCOUNT]);
        assert!(tx.sent_calldata().is_empty(), "old unhealthy should not register");
    }

    #[tokio::test]
    async fn process_instance_unhealthy_no_launch_time_skips_registration() {
        // An Unhealthy instance with no launch_time should NOT be registered
        // (we can't determine age, so we default to the safe path).
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let tx = SharedTxManager::new();
        let driver = step_driver(
            vec![],
            signer_client,
            MockRegistry::with_signers(vec![]),
            tx.clone(),
            CancellationToken::new(),
        );

        let inst = instance(EP1, InstanceHealthStatus::Unhealthy);
        let addrs = driver.process_instance(&inst).await.unwrap();

        assert_eq!(addrs, vec![HARDHAT_ACCOUNT]);
        assert!(tx.sent_calldata().is_empty(), "unhealthy with no launch_time should not register");
    }

    #[tokio::test]
    async fn process_instance_draining_recently_launched_still_skips_registration() {
        // A Draining instance launched 10 minutes ago should NOT be registered.
        // The grace period only applies to Unhealthy, never Draining.
        let launch_time = Some(SystemTime::now() - Duration::from_secs(600));
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let tx = SharedTxManager::new();
        let driver = step_driver(
            vec![],
            signer_client,
            MockRegistry::with_signers(vec![]),
            tx.clone(),
            CancellationToken::new(),
        );

        let inst = instance_with_launch_time(EP1, InstanceHealthStatus::Draining, launch_time);
        let addrs = driver.process_instance(&inst).await.unwrap();

        assert_eq!(addrs, vec![HARDHAT_ACCOUNT]);
        assert!(tx.sent_calldata().is_empty(), "draining should never register even if recent");
    }

    #[tokio::test]
    async fn process_instance_unhealthy_window_zero_disables_feature() {
        // Setting unhealthy_registration_window to zero disables the feature
        // entirely — even a freshly-launched Unhealthy instance is skipped.
        let launch_time = Some(SystemTime::now() - Duration::from_secs(10));
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let tx = SharedTxManager::new();

        let cancel = CancellationToken::new();
        let mut config = default_config(cancel);
        config.unhealthy_registration_window = Duration::ZERO;

        let driver = RegistrationDriver::new(
            MockDiscovery { instances: vec![] },
            StubProofProvider,
            MockRegistry::with_signers(vec![]),
            tx.clone(),
            signer_client,
            config,
        );

        let inst = instance_with_launch_time(EP1, InstanceHealthStatus::Unhealthy, launch_time);
        let addrs = driver.process_instance(&inst).await.unwrap();

        assert_eq!(addrs, vec![HARDHAT_ACCOUNT]);
        assert!(tx.sent_calldata().is_empty(), "window=0 should disable unhealthy registration");
    }

    #[tokio::test]
    async fn step_unhealthy_recently_launched_registers_and_contributes_to_active_set() {
        // A recently-launched Unhealthy instance should be registered AND
        // contribute its signer to active_signers (preventing deregistration).
        let addr = ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();
        let launch_time = Some(SystemTime::now() - Duration::from_secs(300));

        let instances =
            vec![instance_with_launch_time(EP1, InstanceHealthStatus::Unhealthy, launch_time)];
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);

        let tx = SharedTxManager::new();
        let driver = step_driver(
            instances,
            signer_client,
            // addr is already on-chain; without active_signers it would be deregistered.
            MockRegistry::with_signers(vec![addr]),
            tx.clone(),
            CancellationToken::new(),
        );

        driver.step().await.unwrap();

        let sent = tx.sent_calldata();
        // The signer is already on-chain (MockRegistry has it), so try_register
        // sees is_registered=true and skips registration. But no deregistration
        // should happen because the signer is in active_signers.
        assert!(
            sent.is_empty(),
            "already-registered signer should not be re-registered or deregistered"
        );
    }

    // ── step() tests ────────────────────────────────────────────────────

    /// When discovery returns zero instances the active set is empty, so
    /// every on-chain signer is an orphan and must be deregistered.
    #[rstest]
    #[case::single_orphan(vec![ORPHAN_A])]
    #[case::multiple_orphans(vec![ORPHAN_A, ORPHAN_B, ORPHAN_C])]
    #[tokio::test]
    async fn step_zero_instances_deregisters_all_onchain_signers(#[case] orphans: Vec<Address>) {
        let expected_count = orphans.len();
        let tx = SharedTxManager::new();
        let driver = step_driver(
            vec![], // no discovered instances
            MockSignerClient::from_keys(&[]),
            MockRegistry::with_signers(orphans.clone()),
            tx.clone(),
            CancellationToken::new(),
        );

        driver.step().await.unwrap();

        let sent = tx.sent_calldata();
        assert_eq!(sent.len(), expected_count, "all on-chain signers should be deregistered");

        // Verify each deregistration targets the correct signer.
        for orphan in orphans {
            let expected = ITEEProverRegistry::deregisterSignerCall { signer: orphan }.abi_encode();
            assert!(
                sent.iter().any(|s| s[..] == expected[..]),
                "expected deregistration of {orphan}"
            );
        }
    }

    #[tokio::test]
    async fn step_majority_unreachable_skips_orphan_deregistration() {
        // 3 instances discovered, but only 1 is reachable via MockSignerClient.
        // active_signers.len() (1) * 2 <= instances.len() (3) → skip deregistration.
        let instances = vec![
            instance(EP1, InstanceHealthStatus::Healthy),
            instance(EP2, InstanceHealthStatus::Healthy),
            instance(EP3, InstanceHealthStatus::Healthy),
        ];

        // Only EP1 has a key; the other two will fail signer_public_key.
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let tx = SharedTxManager::new();
        let driver = step_driver(
            instances,
            signer_client,
            MockRegistry::all_registered(vec![ORPHAN_B]),
            tx.clone(),
            CancellationToken::new(),
        );

        driver.step().await.unwrap();

        // 1 registration tx for the reachable instance (already registered → 0),
        // but no deregistration tx because majority guard fires.
        let sent = tx.sent_calldata();
        assert!(sent.is_empty(), "expected no txs (majority guard), got {}", sent.len(),);
    }

    #[tokio::test]
    async fn step_cancellation_before_loop_skips_orphan_cleanup() {
        let instances = vec![
            instance(EP1, InstanceHealthStatus::Healthy),
            instance(EP2, InstanceHealthStatus::Healthy),
        ];

        let signer_client =
            MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0), (EP2, &HARDHAT_KEY_1)]);

        let cancel = CancellationToken::new();
        let tx = SharedTxManager::new();

        // All signers already registered so we only care about deregistration.
        let driver = step_driver(
            instances,
            signer_client,
            MockRegistry::all_registered(vec![ORPHAN_C]),
            tx.clone(),
            cancel.clone(),
        );

        // Cancel before running step — cancellation is checked when
        // collecting results, so no deregistration occurs.
        cancel.cancel();
        driver.step().await.unwrap();

        // Cancellation should prevent orphan deregistration entirely.
        assert!(tx.sent_calldata().is_empty(), "no txs should be sent after cancellation",);
    }

    #[tokio::test]
    async fn step_draining_instance_contributes_to_active_set() {
        // A draining instance should contribute its address to active_signers
        // so it isn't deregistered as an orphan, but should not be registered.
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);

        let instances = vec![instance(EP1, InstanceHealthStatus::Draining)];

        let tx = SharedTxManager::new();
        let driver = step_driver(
            instances,
            signer_client,
            // The derived address for HARDHAT_KEY_0 is already on-chain,
            // so it should NOT be deregistered.
            MockRegistry::with_signers(vec![HARDHAT_ACCOUNT]),
            tx.clone(),
            CancellationToken::new(),
        );

        driver.step().await.unwrap();

        // No registration (draining) and no deregistration (signer is active).
        assert!(tx.sent_calldata().is_empty());
    }

    // ── Reachability guard boundary tests ────────────────────────────────
    //
    // The majority guard uses instance counts (not signer counts):
    //
    //     if !instances.is_empty() && reachable_instances * 2 <= instances.len()
    //
    // These tests verify the exact boundary and surrounding values:
    //   - 1/4 reachable → 1*2 <= 4 → true  → deregistration skipped
    //   - 2/4 reachable → 2*2 <= 4 → true  → deregistration skipped
    //   - 3/4 reachable → 3*2 <= 4 → false → deregistration proceeds
    //   - 4/4 reachable → 4*2 <= 4 → false → deregistration proceeds

    /// All 4 endpoints and corresponding private keys, indexed for
    /// dynamic slicing in the parametrized guard test.
    const ALL_ENDPOINTS: [&str; 4] = [EP1, EP2, EP3, EP4];
    const ALL_KEYS: [&[u8; 32]; 4] =
        [&HARDHAT_KEY_0, &HARDHAT_KEY_1, &HARDHAT_KEY_2, &HARDHAT_KEY_3];

    #[rstest]
    #[case::one_of_four(1, true)]
    #[case::two_of_four(2, true)]
    #[case::three_of_four(3, false)]
    #[case::four_of_four(4, false)]
    #[tokio::test]
    async fn step_reachability_guard_boundary(
        #[case] reachable_count: usize,
        #[case] should_skip_deregistration: bool,
    ) {
        // All 4 instances are discovered; only `reachable_count` have keys
        // in the MockSignerClient (the rest will fail signer_public_key).
        let instances: Vec<_> =
            ALL_ENDPOINTS.iter().map(|ep| instance(ep, InstanceHealthStatus::Healthy)).collect();

        let keys: Vec<(&str, &[u8; 32])> = ALL_ENDPOINTS[..reachable_count]
            .iter()
            .zip(&ALL_KEYS[..reachable_count])
            .map(|(ep, key)| (*ep, *key))
            .collect();
        let signer_client = MockSignerClient::from_keys(&keys);

        let tx = SharedTxManager::new();
        let driver = step_driver(
            instances,
            signer_client,
            // All reachable signers already registered, so no registration txs.
            // The orphan is on-chain — deregistered only if guard passes.
            MockRegistry::all_registered(vec![ORPHAN_D]),
            tx.clone(),
            CancellationToken::new(),
        );

        driver.step().await.unwrap();

        if should_skip_deregistration {
            assert!(
                tx.sent_calldata().is_empty(),
                "{reachable_count}/4 reachable: majority guard should skip deregistration"
            );
        } else {
            let sent = tx.sent_calldata();
            assert_eq!(sent.len(), 1, "{reachable_count}/4 reachable: should deregister orphan");
            let expected =
                ITEEProverRegistry::deregisterSignerCall { signer: ORPHAN_D }.abi_encode();
            assert_eq!(sent[0], Bytes::from(expected));
        }
    }

    #[tokio::test]
    async fn step_unreachable_instance_does_not_block_reachable_registration() {
        // An unreachable instance should not prevent other instances from
        // being processed and registered in the same cycle.
        let instances = vec![
            instance(EP4, InstanceHealthStatus::Healthy),
            instance(EP1, InstanceHealthStatus::Healthy),
            instance(EP2, InstanceHealthStatus::Healthy),
            instance(EP3, InstanceHealthStatus::Healthy),
        ];

        // EP4 has no keys → signer_public_key will error.
        let signer_client = MockSignerClient::from_keys(&[
            (EP1, &HARDHAT_KEY_0),
            (EP2, &HARDHAT_KEY_1),
            (EP3, &HARDHAT_KEY_2),
        ]);

        let tx = SharedTxManager::new();
        let driver = step_driver(
            instances,
            signer_client,
            // No signers registered yet → all three reachable signers
            // should be registered.
            MockRegistry::with_signers(vec![]),
            tx.clone(),
            CancellationToken::new(),
        );

        driver.step().await.unwrap();

        // 3 registration txs for the reachable instances, despite the
        // unreachable one failing. No deregistration (no on-chain signers).
        assert_eq!(
            tx.sent_calldata().len(),
            3,
            "all 3 reachable instances should be registered despite 1 unreachable"
        );
    }

    #[tokio::test]
    async fn step_registration_failure_keeps_signer_in_active_set() {
        // A signer whose registration tx fails should remain in active_signers,
        // preventing it from being deregistered as an orphan. This protects
        // against the case where a signer is already on-chain from a previous
        // cycle but the current registration attempt fails (e.g. insufficient
        // funds).
        let signer_addr =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();

        let instances = vec![instance(EP1, InstanceHealthStatus::Healthy)];
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);

        // is_registered returns false (first call in try_register), then
        // false again (post-error check). The signer IS in the on-chain
        // set for get_registered_signers — so without active_signers
        // protection it would be deregistered as an orphan.
        let registry = DynamicRegistry::never_registered(vec![signer_addr]);

        // First send (registration) fails; subsequent sends (deregistration)
        // would succeed — but we expect no deregistration to happen.
        let tx = FailingTxManager::with_errors(vec![
            TxManagerError::InsufficientFunds,
            TxManagerError::InsufficientFunds,
            TxManagerError::InsufficientFunds,
            TxManagerError::InsufficientFunds,
        ]);

        let driver = RegistrationDriver::new(
            MockDiscovery { instances },
            StubProofProvider,
            registry,
            tx.clone(),
            signer_client,
            default_config(CancellationToken::new()),
        );

        driver.step().await.unwrap();

        // Registration was attempted (1 send for the non-retryable error),
        // but no deregistration tx should have been sent because the signer
        // remains in active_signers.
        let sent = tx.sent_calldata();
        assert_eq!(sent.len(), 1, "only the failed registration attempt should be sent");
        // Verify the single tx was a registration, not a deregistration.
        let register_selector = ITEEProverRegistry::registerSignerCall::SELECTOR;
        assert_eq!(
            &sent[0][..4],
            register_selector,
            "the only tx should be the registration attempt"
        );
    }

    /// Signer client wrapper that cancels a token after returning keys.
    ///
    /// Delegates to an inner [`MockSignerClient`] for actual key/attestation
    /// data, but cancels the given [`CancellationToken`] after the first
    /// successful `signer_public_key` call. This simulates cancellation
    /// occurring mid-cycle (after instance processing begins but before
    /// orphan deregistration).
    #[derive(Debug)]
    struct CancellingSignerClient {
        inner: MockSignerClient,
        cancel: CancellationToken,
    }

    #[async_trait]
    impl SignerClient for CancellingSignerClient {
        async fn signer_public_key(&self, endpoint: &Url) -> Result<Vec<Vec<u8>>> {
            let result = self.inner.signer_public_key(endpoint).await;
            if result.is_ok() {
                self.cancel.cancel();
            }
            result
        }

        async fn signer_attestation(
            &self,
            endpoint: &Url,
            user_data: Option<Vec<u8>>,
            nonce: Option<Vec<u8>>,
        ) -> Result<Vec<Vec<u8>>> {
            self.inner.signer_attestation(endpoint, user_data, nonce).await
        }
    }

    #[tokio::test]
    async fn step_cancellation_mid_cycle_skips_orphan_deregistration() {
        // Cancellation during instance processing should skip orphan
        // deregistration. CancellingSignerClient cancels the token as a
        // side-effect of signer_public_key, simulating a shutdown signal
        // arriving while the registrar is processing instances.
        let instances = vec![instance(EP1, InstanceHealthStatus::Healthy)];

        let cancel = CancellationToken::new();
        let tx = SharedTxManager::new();

        let signer_client = CancellingSignerClient {
            inner: MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]),
            cancel: cancel.clone(),
        };

        let driver = RegistrationDriver::new(
            MockDiscovery { instances },
            StubProofProvider,
            MockRegistry::all_registered(vec![ORPHAN_E]),
            tx.clone(),
            signer_client,
            default_config(cancel),
        );

        driver.step().await.unwrap();

        // The instance was processed (all_registered → no registration tx),
        // but orphan deregistration was skipped because the token was
        // cancelled during instance processing.
        assert!(
            tx.sent_calldata().is_empty(),
            "cancellation mid-cycle should prevent orphan deregistration"
        );
    }

    #[tokio::test]
    async fn step_healthy_instances_register_and_deregister_orphans() {
        let addr1 = ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();
        let addr2 = ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_1)).unwrap();
        let orphan =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_2)).unwrap();

        let instances = vec![
            instance(EP1, InstanceHealthStatus::Healthy),
            instance(EP2, InstanceHealthStatus::Healthy),
        ];

        let signer_client =
            MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0), (EP2, &HARDHAT_KEY_1)]);

        let tx = SharedTxManager::new();
        let driver = step_driver(
            instances,
            signer_client,
            // addr1 and addr2 are not yet registered; orphan is on-chain.
            MockRegistry::with_signers(vec![orphan]),
            tx.clone(),
            CancellationToken::new(),
        );

        driver.step().await.unwrap();

        let sent = tx.sent_calldata();
        // 2 registration txs (addr1, addr2) + 1 deregistration tx (orphan).
        assert_eq!(sent.len(), 3, "expected 2 registrations + 1 deregistration");

        // Verify registration calldata uses registerSigner selector.
        let register_selector = ITEEProverRegistry::registerSignerCall::SELECTOR;
        let registration_count =
            sent.iter().filter(|s| s.len() >= 4 && s[..4] == register_selector).count();
        assert_eq!(registration_count, 2, "expected 2 registration txs");

        // Verify the deregistration calldata targets the orphan.
        let deregister_expected =
            ITEEProverRegistry::deregisterSignerCall { signer: orphan }.abi_encode();
        assert!(
            sent.iter().any(|s| s[..] == deregister_expected[..]),
            "expected deregistration of orphan {orphan}, sent: {addr1}, {addr2}",
        );
    }

    // ── Multi-enclave process_instance tests ────────────────────────────

    #[tokio::test]
    async fn process_instance_multi_enclave_returns_all_addresses() {
        let signer_client = MockSignerClient::multi_enclave(EP1, &[&HARDHAT_KEY_0, &HARDHAT_KEY_1]);
        let tx = SharedTxManager::new();
        let driver = step_driver(
            vec![],
            signer_client,
            MockRegistry::with_signers(vec![]),
            tx.clone(),
            CancellationToken::new(),
        );

        let inst = instance(EP1, InstanceHealthStatus::Healthy);
        let addrs = driver.process_instance(&inst).await.unwrap();

        let expected_addr_0 =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();
        let expected_addr_1 =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_1)).unwrap();

        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0], expected_addr_0);
        assert_eq!(addrs[1], expected_addr_1);
        // Two registration transactions (one per enclave).
        assert_eq!(tx.sent_calldata().len(), 2);
    }

    #[tokio::test]
    async fn process_instance_multi_enclave_draining_skips_registration() {
        let signer_client = MockSignerClient::multi_enclave(EP1, &[&HARDHAT_KEY_0, &HARDHAT_KEY_1]);
        let tx = SharedTxManager::new();
        let driver = step_driver(
            vec![],
            signer_client,
            MockRegistry::with_signers(vec![]),
            tx.clone(),
            CancellationToken::new(),
        );

        let inst = instance(EP1, InstanceHealthStatus::Draining);
        let addrs = driver.process_instance(&inst).await.unwrap();

        assert_eq!(addrs.len(), 2, "both addresses should be returned");
        assert!(tx.sent_calldata().is_empty(), "no registration txs for draining instance");
    }

    #[tokio::test]
    async fn step_multi_enclave_draining_protects_all_signers_from_deregistration() {
        // A draining multi-enclave instance should contribute ALL of its
        // signer addresses to active_signers, preventing orphan
        // deregistration for each of them — even though registration is
        // skipped.
        let addr0 = ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();
        let addr1 = ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_1)).unwrap();

        let instances = vec![instance(EP1, InstanceHealthStatus::Draining)];
        let signer_client = MockSignerClient::multi_enclave(EP1, &[&HARDHAT_KEY_0, &HARDHAT_KEY_1]);

        let tx = SharedTxManager::new();
        let driver = step_driver(
            instances,
            signer_client,
            // Both signers are on-chain — without active_signers protection
            // they would be deregistered as orphans.
            MockRegistry::with_signers(vec![addr0, addr1]),
            tx.clone(),
            CancellationToken::new(),
        );

        driver.step().await.unwrap();

        // No registration (draining) and no deregistration (both signers
        // are in active_signers).
        assert!(
            tx.sent_calldata().is_empty(),
            "draining multi-enclave instance should protect all signers from deregistration"
        );
    }

    #[tokio::test]
    async fn step_unhealthy_instance_is_reachable_but_not_registered() {
        // An unhealthy instance (failing ALB health checks) that is still
        // reachable by the registrar (responds to JSON-RPC) should:
        //   1. NOT be registered (should_register returns false for Unhealthy)
        //   2. Count as reachable (increments reachable_instances)
        //   3. Contribute its signers to active_signers (preventing deregistration)
        //
        // This is important because "unhealthy" in ALB terms does not mean
        // the registrar can't connect — the instance may be failing
        // application-level health checks while still responding to RPC.
        let addr_unhealthy =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();

        let instances = vec![
            instance(EP1, InstanceHealthStatus::Unhealthy),
            instance(EP2, InstanceHealthStatus::Healthy),
        ];

        // Both instances are reachable via MockSignerClient.
        let signer_client =
            MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0), (EP2, &HARDHAT_KEY_1)]);

        let tx = SharedTxManager::new();
        let driver = step_driver(
            instances,
            signer_client,
            // The unhealthy signer is on-chain. Without active_signers
            // protection it would be deregistered.
            MockRegistry::with_signers(vec![addr_unhealthy]),
            tx.clone(),
            CancellationToken::new(),
        );

        driver.step().await.unwrap();

        let sent = tx.sent_calldata();

        // 1 registration tx for the healthy instance (unregistered).
        // 0 registration txs for the unhealthy instance (should_register = false).
        // 0 deregistration txs (unhealthy signer is in active_signers).
        assert_eq!(sent.len(), 1, "only the healthy instance should be registered");
        let register_selector = ITEEProverRegistry::registerSignerCall::SELECTOR;
        assert_eq!(&sent[0][..4], register_selector, "the only tx should be a registration");
    }

    #[tokio::test]
    async fn step_proof_generation_failure_keeps_signer_in_active_set() {
        // When proof generation fails, the signer address should still be
        // in active_signers (because process_instance returns the address
        // before try_register is called). This prevents the signer from
        // being deregistered as an orphan due to a transient Boundless outage.
        let signer_addr =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();

        let instances = vec![instance(EP1, InstanceHealthStatus::Healthy)];
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);

        let tx = SharedTxManager::new();
        let driver = RegistrationDriver::new(
            MockDiscovery { instances },
            FailingProofProvider,
            MockRegistry::with_signers(vec![signer_addr]),
            tx.clone(),
            signer_client,
            default_config(CancellationToken::new()),
        );

        driver.step().await.unwrap();

        // No registration tx (proof gen failed), and no deregistration tx
        // (signer is in active_signers despite the proof failure).
        assert!(
            tx.sent_calldata().is_empty(),
            "proof failure should not cause deregistration of the signer"
        );
    }

    #[tokio::test]
    async fn step_mixed_healthy_and_draining_instances() {
        // A cycle with both healthy (should register) and draining
        // (should not register, but contributes to active_signers)
        // instances. Verifies the two statuses compose correctly:
        // the healthy instance is registered, the draining instance
        // protects its signer from deregistration.
        let addr_draining =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();

        let instances = vec![
            instance(EP1, InstanceHealthStatus::Draining),
            instance(EP2, InstanceHealthStatus::Healthy),
        ];

        let signer_client =
            MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0), (EP2, &HARDHAT_KEY_1)]);

        let tx = SharedTxManager::new();
        let driver = step_driver(
            instances,
            signer_client,
            // The draining signer is on-chain. The healthy signer is not.
            MockRegistry::with_signers(vec![addr_draining]),
            tx.clone(),
            CancellationToken::new(),
        );

        driver.step().await.unwrap();

        let sent = tx.sent_calldata();
        // 1 registration tx for the healthy instance.
        // 0 deregistration txs (draining signer is in active_signers).
        assert_eq!(sent.len(), 1, "only the healthy instance should be registered");
        let register_selector = ITEEProverRegistry::registerSignerCall::SELECTOR;
        assert_eq!(&sent[0][..4], register_selector, "the only tx should be a registration");
    }

    // ── Attestation count mismatch test ───────────────────────────────

    #[tokio::test]
    async fn process_instance_fails_on_attestation_count_mismatch() {
        // Return 2 public keys but only 1 attestation → mismatch should error.
        let signer_client = MockSignerClient::multi_enclave(EP1, &[&HARDHAT_KEY_0, &HARDHAT_KEY_1]);
        // Default mock returns 2 attestations (one per key), so override
        // to return only 1 attestation.
        let signer_client = signer_client.with_attestations(EP1, vec![b"single-att".to_vec()]);
        let tx = SharedTxManager::new();
        let driver = step_driver(
            vec![],
            signer_client,
            MockRegistry::with_signers(vec![]),
            tx.clone(),
            CancellationToken::new(),
        );

        let inst = instance(EP1, InstanceHealthStatus::Healthy);
        // Attestations are fetched once for all enclaves before registration.
        // A count mismatch (fewer attestations than keys) fails the entire
        // instance — no enclaves are registered.
        let result = driver.process_instance(&inst).await;

        assert!(result.is_err(), "should fail when attestation count < key count");
    }

    // ── Cancellation responsiveness test ──────────────────────────────────

    /// Signer client that blocks `signer_public_key` on a `Notify` gate.
    /// Allows tests to control when each instance's RPC completes, so
    /// cancellation responsiveness can be verified.
    #[derive(Debug)]
    struct BlockingSignerClient {
        inner: MockSignerClient,
        gate: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl SignerClient for BlockingSignerClient {
        async fn signer_public_key(&self, endpoint: &Url) -> Result<Vec<Vec<u8>>> {
            self.gate.notified().await;
            self.inner.signer_public_key(endpoint).await
        }

        async fn signer_attestation(
            &self,
            endpoint: &Url,
            user_data: Option<Vec<u8>>,
            nonce: Option<Vec<u8>>,
        ) -> Result<Vec<Vec<u8>>> {
            self.inner.signer_attestation(endpoint, user_data, nonce).await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn step_cancellation_breaks_immediately_without_waiting_for_blocked_futures() {
        // Verify that `tokio::select!` with `cancel.cancelled()` causes
        // `step()` to return as soon as cancellation fires, even when all
        // instance futures are blocked on long-running operations. Without
        // this, the old `while let Some(..) = futs.next().await` loop
        // would hang until at least one future completed (~20 min for
        // proof generation in production).
        //
        // The test uses `start_paused = true` so time only advances via
        // explicit `tokio::time::sleep`. We schedule a cancellation after
        // 1s, then assert `step()` completes well before the 20-minute
        // proof generation timeout — proving the `select!` broke the loop
        // immediately without waiting for the blocked futures.
        let instances = vec![
            instance(EP1, InstanceHealthStatus::Healthy),
            instance(EP2, InstanceHealthStatus::Healthy),
        ];

        let gate = Arc::new(tokio::sync::Notify::new());
        let signer_client = BlockingSignerClient {
            inner: MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0), (EP2, &HARDHAT_KEY_1)]),
            gate: Arc::clone(&gate),
        };

        let cancel = CancellationToken::new();
        let tx = SharedTxManager::new();
        let driver = RegistrationDriver::new(
            MockDiscovery { instances },
            StubProofProvider,
            MockRegistry::all_registered(vec![ORPHAN_E]),
            tx.clone(),
            signer_client,
            default_config(cancel.clone()),
        );

        // Schedule cancellation after 1 second (paused time).
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            cancel_clone.cancel();
        });

        // step() should return quickly after the cancellation fires.
        // With paused time, this verifies the select! breaks the loop
        // rather than waiting for the BlockingSignerClient gate.
        let result = tokio::time::timeout(Duration::from_secs(10), driver.step()).await;

        let step_result =
            result.expect("step should complete within timeout, not hang on blocked futures");
        assert!(step_result.is_ok(), "step should return Ok after cancellation");

        // No transactions should have been sent — neither registration
        // (futures never completed) nor deregistration (cancelled).
        assert!(
            tx.sent_calldata().is_empty(),
            "no transactions should be sent when all futures are blocked and cancellation fires"
        );
    }

    // ── Concurrency limit test ──────────────────────────────────────────

    /// Signer client that tracks the peak number of concurrent
    /// `signer_public_key` calls. Used to verify `max_concurrency`.
    #[derive(Debug)]
    struct ConcurrencyTrackingSignerClient {
        inner: MockSignerClient,
        in_flight: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    impl ConcurrencyTrackingSignerClient {
        fn new(inner: MockSignerClient) -> (Self, Arc<AtomicUsize>) {
            let peak = Arc::new(AtomicUsize::new(0));
            let client =
                Self { inner, in_flight: Arc::new(AtomicUsize::new(0)), peak: Arc::clone(&peak) };
            (client, peak)
        }
    }

    #[async_trait]
    impl SignerClient for ConcurrencyTrackingSignerClient {
        async fn signer_public_key(&self, endpoint: &Url) -> Result<Vec<Vec<u8>>> {
            let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(current, Ordering::SeqCst);

            // Yield to give other futures a chance to enter concurrently.
            tokio::task::yield_now().await;

            let result = self.inner.signer_public_key(endpoint).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            result
        }

        async fn signer_attestation(
            &self,
            endpoint: &Url,
            user_data: Option<Vec<u8>>,
            nonce: Option<Vec<u8>>,
        ) -> Result<Vec<Vec<u8>>> {
            self.inner.signer_attestation(endpoint, user_data, nonce).await
        }
    }

    #[rstest]
    #[case::serial(1)]
    #[case::limited(2)]
    #[tokio::test]
    async fn step_respects_max_concurrency(#[case] max_concurrency: usize) {
        // Process 4 instances with a limited max_concurrency and verify
        // the peak concurrent in-flight count never exceeds the limit.
        let instances: Vec<_> =
            ALL_ENDPOINTS.iter().map(|ep| instance(ep, InstanceHealthStatus::Healthy)).collect();

        let keys: Vec<(&str, &[u8; 32])> =
            ALL_ENDPOINTS.iter().copied().zip(ALL_KEYS.iter().copied()).collect();
        let inner = MockSignerClient::from_keys(&keys);
        let (signer_client, peak) = ConcurrencyTrackingSignerClient::new(inner);

        let cancel = CancellationToken::new();
        let tx = SharedTxManager::new();
        let mut config = default_config(cancel);
        config.max_concurrency = max_concurrency;

        let driver = RegistrationDriver::new(
            MockDiscovery { instances },
            StubProofProvider,
            MockRegistry::with_signers(vec![]),
            tx.clone(),
            signer_client,
            config,
        );

        driver.step().await.unwrap();

        let observed_peak = peak.load(Ordering::SeqCst);
        assert!(
            observed_peak <= max_concurrency,
            "peak concurrency {observed_peak} exceeded max_concurrency {max_concurrency}"
        );
        // Verify instances were actually processed (4 registration txs).
        assert_eq!(tx.sent_calldata().len(), 4, "all 4 instances should be registered");
    }

    // ── tx retry tests (Fix C) ──────────────────────────────────────────
    //
    // These tests verify the retry loop in `try_register`. Key
    // invariants:
    // - The expensive proof is generated exactly once and reused across
    //   retries (identical calldata in every `send()` call).
    // - Non-retryable errors abort immediately.
    // - `is_registered` is checked after each failure to catch false
    //   negatives.
    // - Cancellation is respected both at the top of the loop and during
    //   the retry delay.

    /// Asserts that all calldata entries submitted to the tx manager are
    /// identical, confirming the same proof is reused across retries.
    fn assert_all_calldata_identical(sent: &[Bytes]) {
        if sent.len() < 2 {
            return;
        }
        for (i, entry) in sent.iter().enumerate().skip(1) {
            assert_eq!(
                &sent[0], entry,
                "calldata mismatch: sent[0] != sent[{i}] — proof was regenerated"
            );
        }
    }

    /// Transient errors followed by success: the retry loop should retry
    /// and eventually succeed. Proof is generated once, same calldata
    /// across all attempts.
    #[tokio::test(start_paused = true)]
    async fn try_register_retries_transient_error_then_succeeds() {
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let tx = FailingTxManager::with_errors(vec![
            TxManagerError::Rpc("transient 1".into()),
            TxManagerError::Rpc("transient 2".into()),
        ]);
        let proof_provider = CountingProofProvider::new();
        let registry = DynamicRegistry::never_registered(vec![]);
        let driver = retry_driver(
            signer_client,
            registry,
            tx.clone(),
            proof_provider,
            CancellationToken::new(),
        );

        let inst = instance(EP1, InstanceHealthStatus::Healthy);
        let result = driver.process_instance(&inst).await;

        assert!(result.is_ok(), "should succeed after retries: {result:?}");
        // 2 failed attempts + 1 success = 3 total sends.
        assert_eq!(tx.send_count(), 3);
        assert_all_calldata_identical(&tx.sent_calldata());
        assert_eq!(driver.proof_provider.call_count(), 1, "proof should be generated once");
    }

    /// Transient error but on-chain check shows signer is already
    /// registered: should return Ok without retrying.
    #[tokio::test(start_paused = true)]
    async fn try_register_already_registered_after_error_returns_ok() {
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let tx = FailingTxManager::with_errors(vec![TxManagerError::Rpc("nonce race".into())]);
        // First `is_registered` call (before proof gen) returns false.
        // Second call (after tx error) returns true (tx was mined despite error).
        let registry = DynamicRegistry::registered_after_first_check(vec![]);
        let driver = retry_driver(
            signer_client,
            registry,
            tx.clone(),
            StubProofProvider,
            CancellationToken::new(),
        );

        let inst = instance(EP1, InstanceHealthStatus::Healthy);
        let result = driver.process_instance(&inst).await;

        assert!(result.is_ok(), "should succeed: signer registered on-chain: {result:?}");
        // Only 1 send attempt — the is_registered check short-circuits retry.
        assert_eq!(tx.send_count(), 1);
    }

    /// Non-retryable errors abort immediately without retry.
    /// Each variant is a distinct `TxManagerError` that `is_retryable()` returns
    /// `false` for — the retry loop must recognise them and bail after one send.
    #[rstest]
    #[case::execution_reverted(TxManagerError::ExecutionReverted {
        reason: Some("bad proof".into()),
        data: None,
    })]
    #[case::insufficient_funds(TxManagerError::InsufficientFunds)]
    #[case::fee_limit_exceeded(TxManagerError::FeeLimitExceeded { fee: 500, ceiling: 100 })]
    #[tokio::test(start_paused = true)]
    async fn try_register_non_retryable_error_aborts_immediately(#[case] error: TxManagerError) {
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let tx = FailingTxManager::with_errors(vec![error]);
        let registry = DynamicRegistry::never_registered(vec![]);
        let driver = retry_driver(
            signer_client,
            registry,
            tx.clone(),
            StubProofProvider,
            CancellationToken::new(),
        );

        let inst = instance(EP1, InstanceHealthStatus::Healthy);
        let result = driver.process_instance(&inst).await;

        // process_instance logs errors but doesn't propagate them, so it returns Ok.
        // However, the tx manager should only have been called once (no retry).
        assert!(result.is_ok());
        assert_eq!(tx.send_count(), 1, "should not retry after non-retryable error");
    }

    /// Transient errors exhaust all retries: should fail after
    /// `MAX_TX_RETRIES` + 1 attempts. Same calldata in every attempt.
    #[tokio::test(start_paused = true)]
    async fn try_register_exhausts_retries_then_fails() {
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        // Return more errors than MAX_TX_RETRIES allows.
        let errors: Vec<TxManagerError> = (0..=MAX_TX_RETRIES)
            .map(|_| TxManagerError::Rpc("persistent failure".into()))
            .collect();
        let tx = FailingTxManager::with_errors(errors);
        let proof_provider = CountingProofProvider::new();
        let registry = DynamicRegistry::never_registered(vec![]);
        let driver = retry_driver(
            signer_client,
            registry,
            tx.clone(),
            proof_provider,
            CancellationToken::new(),
        );

        let inst = instance(EP1, InstanceHealthStatus::Healthy);
        let result = driver.process_instance(&inst).await;

        // process_instance catches the error — verify via send count.
        assert!(result.is_ok());
        // 1 initial + MAX_TX_RETRIES retries = MAX_TX_RETRIES + 1 total.
        assert_eq!(
            tx.send_count(),
            (MAX_TX_RETRIES + 1) as usize,
            "should attempt exactly MAX_TX_RETRIES + 1 sends",
        );
        assert_all_calldata_identical(&tx.sent_calldata());
        assert_eq!(driver.proof_provider.call_count(), 1, "proof should be generated once");
    }

    /// Cancellation during the retry sleep aborts the retry loop without
    /// sending another transaction.
    ///
    /// Uses `start_paused = true` so time advances only when polled.
    /// The cancel token fires 1 second into the 5-second retry delay,
    /// then we advance time past the full delay to prove no second send
    /// occurs.
    #[tokio::test(start_paused = true)]
    async fn try_register_cancellation_during_retry_sleep_aborts() {
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        // Return enough transient errors for multiple retries — but
        // cancellation should prevent all but the first.
        let tx = FailingTxManager::with_errors(vec![
            TxManagerError::Rpc("fail 1".into()),
            TxManagerError::Rpc("fail 2".into()),
            TxManagerError::Rpc("fail 3".into()),
        ]);
        let registry = DynamicRegistry::never_registered(vec![]);
        let cancel = CancellationToken::new();
        let driver =
            retry_driver(signer_client, registry, tx.clone(), StubProofProvider, cancel.clone());

        let inst = instance(EP1, InstanceHealthStatus::Healthy);

        // Spawn a task that cancels after 1 second (during the 5s delay).
        let cancel_handle = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            cancel_handle.cancel();
        });

        let result = driver.process_instance(&inst).await;

        assert!(result.is_ok());
        // Only 1 send: the tokio::select! in the retry delay catches
        // the cancellation before the sleep completes.
        assert_eq!(tx.send_count(), 1, "should abort during retry sleep");
    }

    /// Cancellation before the retry loop starts: no tx is sent at all.
    #[tokio::test(start_paused = true)]
    async fn try_register_cancellation_before_loop_sends_nothing() {
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let tx = FailingTxManager::with_errors(vec![]);
        let registry = DynamicRegistry::never_registered(vec![]);
        let cancel = CancellationToken::new();
        cancel.cancel(); // Cancel before entering try_register.
        let driver = retry_driver(signer_client, registry, tx.clone(), StubProofProvider, cancel);

        let inst = instance(EP1, InstanceHealthStatus::Healthy);
        let result = driver.process_instance(&inst).await;

        assert!(result.is_ok());
        assert_eq!(tx.send_count(), 0, "should not send any tx after pre-cancellation");
    }

    /// Mixed errors: transient → `ExecutionReverted`. The retry loop should
    /// process the first error (retryable), then abort on the second
    /// (non-retryable) without further retries.
    #[tokio::test(start_paused = true)]
    async fn try_register_transient_then_execution_reverted() {
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let tx = FailingTxManager::with_errors(vec![
            TxManagerError::Rpc("transient".into()),
            TxManagerError::ExecutionReverted { reason: None, data: None },
        ]);
        let registry = DynamicRegistry::never_registered(vec![]);
        let driver = retry_driver(
            signer_client,
            registry,
            tx.clone(),
            StubProofProvider,
            CancellationToken::new(),
        );

        let inst = instance(EP1, InstanceHealthStatus::Healthy);
        let result = driver.process_instance(&inst).await;

        assert!(result.is_ok());
        // 2 sends: first retryable, second fatal.
        assert_eq!(tx.send_count(), 2);
        assert_all_calldata_identical(&tx.sent_calldata());
    }

    /// Immediate success on first attempt: no retries needed.
    #[tokio::test(start_paused = true)]
    async fn try_register_immediate_success() {
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let tx = FailingTxManager::with_errors(vec![]); // no errors — immediate success
        let proof_provider = CountingProofProvider::new();
        let registry = DynamicRegistry::never_registered(vec![]);
        let driver = retry_driver(
            signer_client,
            registry,
            tx.clone(),
            proof_provider,
            CancellationToken::new(),
        );

        let inst = instance(EP1, InstanceHealthStatus::Healthy);
        let result = driver.process_instance(&inst).await;

        assert!(result.is_ok());
        assert_eq!(tx.send_count(), 1, "should succeed on first attempt");
        assert_eq!(driver.proof_provider.call_count(), 1, "proof should be generated once");
    }
}
