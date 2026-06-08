//! Proof result handling and onchain signer registration.
//!
//! Polls or waits for attestation proof results, checks whether the signer is
//! already registered onchain, and submits `TEEProverRegistry.registerSigner`
//! transactions through the transaction manager with delivery retries.

use std::{
    collections::HashSet,
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolCall;
use base_proof_contracts::ITEEProverRegistry;
use base_proof_tee_nitro_attestation_prover::AttestationProofProvider;
use base_tx_manager::{TxCandidate, TxManager, TxManagerError};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{ProverInstance, RegistrarError, RegistrarMetrics, RegistryClient, Result};

/// Component responsible for turning attestation proof results into durable
/// onchain signer registrations.
pub struct ProofHandler<'a, P: ?Sized, R: ?Sized, T: ?Sized> {
    /// Proof provider used to poll or generate the attestation proof result.
    pub proof_provider: &'a P,
    /// Registry client used for side-effect-free registration state checks.
    pub registry: &'a R,
    /// Transaction manager used to deliver `registerSigner`.
    pub tx_manager: &'a T,
    /// Semaphore bounding concurrent proof work.
    pub proof_semaphore: &'a Semaphore,
    /// Process-local signer set used to deduplicate concurrent attempts.
    pub in_flight_registrations: &'a Arc<Mutex<HashSet<Address>>>,
    /// `TEEProverRegistry` contract address on L1.
    pub registry_address: Address,
    /// Maximum number of transaction submission retries for transient errors.
    pub max_tx_retries: u32,
    /// Delay between transaction submission retries.
    pub tx_retry_delay: Duration,
}

impl<P: ?Sized, R: ?Sized, T: ?Sized> fmt::Debug for ProofHandler<'_, P, R, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProofHandler")
            .field("registry_address", &self.registry_address)
            .field("max_tx_retries", &self.max_tx_retries)
            .field("tx_retry_delay", &self.tx_retry_delay)
            .finish_non_exhaustive()
    }
}

/// RAII guard that removes a signer address from the in-flight set when
/// dropped.
///
/// Ensures cleanup on every exit path from [`ProofHandler::register_signer`]:
/// success, error, retry exhaustion, cancellation drop, and panic.
#[doc(hidden)]
#[derive(Debug)]
pub struct InFlightRegistrationGuard {
    in_flight: Arc<Mutex<HashSet<Address>>>,
    signer: Address,
}

impl InFlightRegistrationGuard {
    /// Reserves `signer` in `in_flight` until the returned guard is dropped.
    pub fn try_acquire(in_flight: &Arc<Mutex<HashSet<Address>>>, signer: Address) -> Option<Self> {
        let mut set = in_flight.lock().unwrap_or_else(|e| e.into_inner());
        set.insert(signer).then(|| Self { in_flight: Arc::clone(in_flight), signer })
    }
}

impl Drop for InFlightRegistrationGuard {
    fn drop(&mut self) {
        // The critical section is a single `HashSet::remove` and cannot
        // panic under normal conditions, so poisoning is effectively
        // impossible. If it ever occurs, the set contents are still
        // valid and cleanup must proceed.
        let mut set = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        set.remove(&self.signer);
    }
}

impl<'a, P, R, T> ProofHandler<'a, P, R, T>
where
    P: AttestationProofProvider + ?Sized,
    R: RegistryClient + ?Sized,
    T: TxManager + ?Sized,
{
    /// Attempts to register a signer onchain if it is not already registered.
    ///
    /// This is the expensive path: checks onchain status, polls or generates a
    /// proof from the pre-fetched attestation, and submits a registration
    /// transaction with delivery retries.
    ///
    /// Registration is PCR0-agnostic: all legitimate enclaves are registered
    /// regardless of their PCR0 measurement. This enables pre-registration of
    /// new-PCR0 enclaves before a hardfork, eliminating the proof-generation
    /// delay when the onchain `TEE_IMAGE_HASH` rotates. The onchain
    /// `TEEVerifier` gates proof acceptance on `TEE_IMAGE_HASH` at submission
    /// time, so pre-registered enclaves cannot produce accepted proposals
    /// until the hardfork activates.
    pub async fn register_signer(
        &self,
        instance: &ProverInstance,
        signer_address: Address,
        enclave_index: usize,
        attestation_bytes: &[u8],
        signer_cancel: &CancellationToken,
    ) -> Result<()> {
        let Self {
            proof_provider,
            registry,
            tx_manager,
            proof_semaphore,
            in_flight_registrations,
            registry_address,
            max_tx_retries,
            tx_retry_delay,
        } = *self;

        // Check cancellation BEFORE any other work: a task that was
        // already cancelled should not acquire the in-flight mutex or do
        // registry RPC work. This bounds shutdown latency by the longest
        // in-flight operation rather than by an additional registry round-trip
        // per pending task.
        if signer_cancel.is_cancelled() {
            debug!(signer = %signer_address, "task cancelled before registry probe");
            return Ok(());
        }

        // Reserve this signer in the in-flight set before the `is_registered`
        // precheck. If another concurrent task already owns it, short-circuit
        // so we do not race past the precheck, regenerate the proof, and
        // submit a duplicate registration transaction.
        let Some(_in_flight) =
            InFlightRegistrationGuard::try_acquire(in_flight_registrations, signer_address)
        else {
            debug!(
                signer = %signer_address,
                enclave_index,
                instance = %instance.instance_id,
                "registration already in flight for this signer, skipping duplicate",
            );
            return Ok(());
        };

        // Cancel-aware: `is_registered` is a side-effect-free read, so
        // dropping it on cancel is safe.
        let already_registered = tokio::select! {
            biased;
            () = signer_cancel.cancelled() => {
                debug!(
                    signer = %signer_address,
                    "cancelled while probing registry pre-proof-gen"
                );
                return Ok(());
            }
            res = registry.is_registered(signer_address) => res?,
        };
        if already_registered {
            debug!(signer = %signer_address, "already registered, skipping");
            return Ok(());
        }

        // Check cancellation before the most expensive operation. Proof
        // generation and proof-result polling can take minutes via Boundless.
        if signer_cancel.is_cancelled() {
            debug!("shutdown requested, skipping proof generation");
            return Ok(());
        }

        info!(
            signer = %signer_address,
            enclave_index,
            instance = %instance.instance_id,
            "generating proof for unregistered signer"
        );

        // Acquire a proof-concurrency permit. Bounds simultaneous
        // Boundless/Direct proof generations across all spawned tasks.
        let _permit = tokio::select! {
            biased;
            () = signer_cancel.cancelled() => {
                debug!(
                    signer = %signer_address,
                    instance = %instance.instance_id,
                    "task cancelled before acquiring proof permit"
                );
                return Ok(());
            }
            permit = proof_semaphore.acquire() => {
                match permit {
                    Ok(p) => p,
                    Err(_) => {
                        warn!(
                            signer = %signer_address,
                            instance = %instance.instance_id,
                            "proof semaphore closed unexpectedly, exiting task"
                        );
                        return Ok(());
                    }
                }
            }
        };

        // Cooperative cancel-safety around the long-running proof. Dropping the
        // provider future on cancel may abandon work the impl had already
        // started; for Boundless, any submitted offchain request is recoverable
        // via deterministic request-id derivation on the next call.
        let proof = tokio::select! {
            biased;
            () = signer_cancel.cancelled() => {
                debug!(
                    signer = %signer_address,
                    instance = %instance.instance_id,
                    "task cancelled during proof generation"
                );
                return Ok(());
            }
            res = proof_provider.generate_proof_for_signer(
                attestation_bytes,
                signer_address,
                signer_cancel,
            ) => {
                match res {
                    Ok(p) => p,
                    Err(_) if signer_cancel.is_cancelled() => {
                        debug!(
                            signer = %signer_address,
                            instance = %instance.instance_id,
                            "task cancelled during proof generation (provider returned Err after cancel)",
                        );
                        return Ok(());
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        };

        // Check cancellation before submitting the transaction to avoid starting
        // new onchain work if shutdown is in progress.
        if signer_cancel.is_cancelled() {
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
            instance = %instance.instance_id,
            registry = %registry_address,
            calldata_len = calldata.len(),
            "Registering signer"
        );

        let candidate =
            TxCandidate { tx_data: calldata, to: Some(registry_address), ..Default::default() };

        info!(
            tx = ?candidate,
            "Sending tx candidate",
        );

        // Retry tx submission on transient errors to avoid discarding an
        // expensive proof on a nonce race or brief network blip.
        let mut tx_retries = 0;

        let receipt = loop {
            // Check cancellation at the top of each iteration to avoid starting
            // new onchain work after shutdown is requested.
            //
            // IMPORTANT: we never wrap `tx_manager.send()` itself in a
            // `select!` against `signer_cancel`; dropping `send()` after nonce
            // acquisition but before broadcast leaves a nonce gap.
            if signer_cancel.is_cancelled() {
                debug!("shutdown requested, aborting tx submission");
                return Ok(());
            }

            match tx_manager.send(candidate.clone()).await {
                Ok(receipt) => break receipt,
                Err(e) => {
                    // The signer may already be registered despite the error
                    // (for example, the tx was mined but the tx manager reported
                    // a nonce race during fee bumping). Check onchain state.
                    let post_err_check = tokio::select! {
                        biased;
                        () = signer_cancel.cancelled() => {
                            debug!(
                                signer = %signer_address,
                                "cancelled while verifying post-tx-error registration state"
                            );
                            return Ok(());
                        }
                        res = registry.is_registered(signer_address) => res,
                    };
                    match post_err_check {
                        Ok(true) => {
                            info!(
                                signer = %signer_address,
                                error = %e,
                                "tx error but signer is registered onchain, treating as success"
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

                    // Non-retryable errors cannot be resolved by retrying with
                    // the same calldata.
                    if !e.is_retryable() {
                        // If the contract reverted execution, the proof itself
                        // is likely invalid. Block recovery for this signer so
                        // the next cycle generates a fresh proof instead of
                        // re-recovering the same one.
                        if matches!(e, TxManagerError::ExecutionReverted { .. }) {
                            warn!(
                                signer = %signer_address,
                                "execution reverted, blocking proof recovery for signer"
                            );
                            proof_provider.block_recovery_for_signer(signer_address);
                        }
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

                    // Cancellation-aware delay: abort immediately if shutdown
                    // is requested during the retry wait.
                    tokio::select! {
                        biased;
                        () = signer_cancel.cancelled() => {
                            debug!(
                                error = %e,
                                signer = %signer_address,
                                "shutdown requested during retry delay; abandoning task"
                            );
                            return Ok(());
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
                format!("registration transaction {} reverted", receipt.transaction_hash).into(),
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
}
