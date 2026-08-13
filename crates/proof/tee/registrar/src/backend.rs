//! Temporary Boundless registration backend for the Boundless→hinted cutover.
//!
//! [`BoundlessRegistrationBackend`] owns today's Boundless proof → calldata →
//! submit workflow. This module is cutover scaffolding and is removed with
//! Boundless in CHAIN-4833.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolCall;
use base_proof_contracts::{ITEEProverRegistry, TEEProverRegistryClient};
use base_proof_tee_nitro_attestation_prover::AttestationProofProvider;
use base_proof_tee_nitro_verifier::VerifierJournal;
use base_tx_manager::{TxCandidate, TxManager, TxManagerError};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{AttestationNonce, RegistrarError, RegistrarMetrics, RegistrationBackend, Result};

/// Default maximum number of transaction submission retries for transient
/// errors before giving up.
pub const DEFAULT_MAX_TX_RETRIES: u32 = 3;

/// Default initial delay between transaction submission retries in seconds.
pub const DEFAULT_TX_RETRY_DELAY_SECS: u64 = 5;

/// Maximum exponential backoff delay between transaction submission retries.
const MAX_TX_RETRY_BACKOFF_DELAY: Duration = Duration::from_secs(60);

/// Runtime configuration for [`BoundlessRegistrationBackend`].
#[derive(Debug, Clone, Copy)]
pub struct BoundlessRegistrationBackendConfig {
    /// `TEEProverRegistry` contract address.
    pub registry_address: Address,
    /// Maximum concurrent proof-generation tasks.
    pub max_concurrency: usize,
    /// Maximum number of transaction submission retries for transient errors.
    pub max_tx_retries: u32,
    /// Delay between transaction submission retries.
    pub tx_retry_delay: Duration,
    /// Maximum proof attestation age accepted before on-chain submission.
    pub max_attestation_age: Duration,
}

/// Boundless proof registration workflow behind [`RegistrationBackend`].
///
/// Temporary cutover scaffolding; deleted with Boundless in CHAIN-4833.
#[derive(Debug)]
pub struct BoundlessRegistrationBackend<P, R, T> {
    proof_provider: P,
    registry: R,
    /// Transaction manager used for registration submits.
    ///
    /// `pub(crate)` so colocated registrar tests can assert send outcomes without
    /// widening the permanent public API of this temporary cutover type.
    pub(crate) tx_manager: T,
    proof_semaphore: Semaphore,
    registry_address: Address,
    max_tx_retries: u32,
    tx_retry_delay: Duration,
    max_attestation_age: Duration,
}

impl<P, R, T> BoundlessRegistrationBackend<P, R, T> {
    /// Creates a Boundless registration backend from proof and submit deps.
    pub fn new(
        proof_provider: P,
        registry: R,
        tx_manager: T,
        config: BoundlessRegistrationBackendConfig,
    ) -> Self {
        let BoundlessRegistrationBackendConfig {
            registry_address,
            max_concurrency,
            max_tx_retries,
            tx_retry_delay,
            max_attestation_age,
        } = config;
        let proof_semaphore = Semaphore::new(max_concurrency.max(1));
        Self {
            proof_provider,
            registry,
            tx_manager,
            proof_semaphore,
            registry_address,
            max_tx_retries,
            tx_retry_delay,
            max_attestation_age,
        }
    }
}

impl<P, R, T> RegistrationBackend for BoundlessRegistrationBackend<P, R, T>
where
    P: AttestationProofProvider,
    R: TEEProverRegistryClient,
    T: TxManager,
{
    async fn register_signer(
        &self,
        instance_id: &str,
        signer_address: Address,
        attestation_bytes: &[u8],
        signer_cancel: &CancellationToken,
    ) -> Result<()> {
        let Some(already_registered) = signer_cancel
            .run_until_cancelled(self.registry.is_registered_signer(signer_address))
            .await
            .transpose()?
        else {
            return Ok(());
        };
        if already_registered {
            RegistrarMetrics::record_registration_stage(
                RegistrarMetrics::REGISTRATION_STAGE_ALREADY_REGISTERED,
            );
            debug!(
                signer = %signer_address,
                instance = %instance_id,
                "already registered, skipping"
            );
            return Ok(());
        }

        info!(
            signer = %signer_address,
            instance = %instance_id,
            "generating proof for unregistered signer"
        );
        RegistrarMetrics::record_registration_stage(
            RegistrarMetrics::REGISTRATION_STAGE_PROOF_STARTED,
        );

        let Some(permit) = signer_cancel.run_until_cancelled(self.proof_semaphore.acquire()).await
        else {
            RegistrarMetrics::record_registration_stage(
                RegistrarMetrics::REGISTRATION_STAGE_PROOF_CANCELLED,
            );
            return Ok(());
        };
        let proof_permit = match permit {
            Ok(p) => p,
            Err(_) => {
                warn!(
                    signer = %signer_address,
                    instance = %instance_id,
                    "proof semaphore closed unexpectedly, exiting task"
                );
                return Ok(());
            }
        };

        // Dropping the provider future on cancel can abandon offchain work; the
        // Boundless provider recovers via deterministic request IDs on retry.
        let Some(proof_result) = signer_cancel
            .run_until_cancelled(self.proof_provider.generate_proof_for_signer(
                attestation_bytes,
                signer_address,
                signer_cancel,
            ))
            .await
        else {
            RegistrarMetrics::record_registration_stage(
                RegistrarMetrics::REGISTRATION_STAGE_PROOF_CANCELLED,
            );
            return Ok(());
        };
        let proof = match proof_result {
            Ok(proof) => {
                RegistrarMetrics::record_registration_stage(
                    RegistrarMetrics::REGISTRATION_STAGE_PROOF_SUCCEEDED,
                );
                proof
            }
            Err(_) if signer_cancel.is_cancelled() => {
                RegistrarMetrics::record_registration_stage(
                    RegistrarMetrics::REGISTRATION_STAGE_PROOF_CANCELLED,
                );
                return Ok(());
            }
            Err(e) => {
                RegistrarMetrics::record_registration_stage(
                    RegistrarMetrics::REGISTRATION_STAGE_PROOF_FAILED,
                );
                return Err(e.into());
            }
        };
        let journal = match VerifierJournal::decode(&proof.output) {
            Ok(journal) => journal,
            Err(e) => {
                RegistrarMetrics::record_registration_stage(
                    RegistrarMetrics::REGISTRATION_STAGE_PROOF_INVALID,
                );
                return Err(RegistrarError::InvalidProofJournal { reason: e.to_string() });
            }
        };
        let expected_nonce = AttestationNonce::derive(self.registry_address, signer_address);
        if journal.nonce.as_ref() != expected_nonce.as_slice() {
            RegistrarMetrics::record_registration_stage(
                RegistrarMetrics::REGISTRATION_STAGE_PROOF_INVALID,
            );
            self.proof_provider.block_recovery_for_signer(signer_address);
            return Err(RegistrarError::InvalidAttestationProof(format!(
                "nonce mismatch for signer {signer_address}: expected 0x{}, got 0x{}",
                hex::encode(expected_nonce),
                hex::encode(journal.nonce)
            )));
        }
        let attestation_timestamp = journal.timestamp;
        drop(proof_permit);

        let calldata = Bytes::from(
            ITEEProverRegistry::registerSignerCall {
                output: proof.output,
                proofBytes: proof.proof_bytes,
            }
            .abi_encode(),
        );

        let candidate = TxCandidate {
            tx_data: calldata,
            to: Some(self.registry_address),
            ..Default::default()
        };

        info!(
            signer = %signer_address,
            instance = %instance_id,
            registry = %self.registry_address,
            tx = ?candidate,
            "sending registration tx candidate"
        );

        let receipt = 'submit: {
            for retry in 0..=self.max_tx_retries {
                if signer_cancel.is_cancelled() {
                    return Ok(());
                }
                if let Err(e) = self.ensure_attestation_fresh(signer_address, attestation_timestamp)
                {
                    RegistrarMetrics::record_registration_stage(
                        RegistrarMetrics::REGISTRATION_STAGE_PROOF_STALE,
                    );
                    return Err(e);
                }

                // Do not wrap send in run_until_cancelled: dropping it after nonce
                // acquisition can leave a nonce gap.
                RegistrarMetrics::record_registration_stage(
                    RegistrarMetrics::REGISTRATION_STAGE_TX_SUBMITTED,
                );
                match self.tx_manager.send(candidate.clone()).await {
                    Ok(receipt) => break 'submit receipt,
                    Err(e) => {
                        let Some(post_err_check) = signer_cancel
                            .run_until_cancelled(self.registry.is_registered_signer(signer_address))
                            .await
                        else {
                            return Ok(());
                        };
                        match post_err_check {
                            Ok(true) => {
                                info!(
                                    signer = %signer_address,
                                    error = %e,
                                    "tx error but signer is registered onchain, treating as success"
                                );
                                RegistrarMetrics::record_registration_stage(
                                    RegistrarMetrics::REGISTRATION_STAGE_TX_OBSERVED_REGISTERED,
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

                        if !e.is_retryable() {
                            if let TxManagerError::ExecutionReverted { data, reason, .. } = &e {
                                let registry_error = data
                                    .as_ref()
                                    .and_then(|d| d.get(..4))
                                    .and_then(|selector| selector.try_into().ok())
                                    .and_then(
                                        ITEEProverRegistry::ITEEProverRegistryErrors::name_by_selector,
                                    );
                                warn!(
                                    signer = %signer_address,
                                    registry_error = ?registry_error,
                                    reason = ?reason,
                                    "execution reverted, blocking proof recovery for signer"
                                );
                                self.proof_provider.block_recovery_for_signer(signer_address);
                            }
                            RegistrarMetrics::record_registration_stage(
                                RegistrarMetrics::REGISTRATION_STAGE_TX_FAILED,
                            );
                            return Err(RegistrarError::from(e));
                        }

                        if retry == self.max_tx_retries {
                            RegistrarMetrics::record_registration_stage(
                                RegistrarMetrics::REGISTRATION_STAGE_TX_FAILED,
                            );
                            return Err(RegistrarError::from(e));
                        }

                        let retry = retry + 1;
                        let max_retry_delay = MAX_TX_RETRY_BACKOFF_DELAY.max(self.tx_retry_delay);
                        let retry_delay = self
                            .tx_retry_delay
                            .saturating_mul(2_u32.saturating_pow(retry - 1))
                            .min(max_retry_delay);
                        warn!(
                            error = %e,
                            signer = %signer_address,
                            retry,
                            max_retries = self.max_tx_retries,
                            delay = ?retry_delay,
                            "tx submission failed, retrying with same proof"
                        );
                        RegistrarMetrics::record_registration_stage(
                            RegistrarMetrics::REGISTRATION_STAGE_TX_RETRY,
                        );

                        if signer_cancel
                            .run_until_cancelled(tokio::time::sleep(retry_delay))
                            .await
                            .is_none()
                        {
                            return Ok(());
                        }
                    }
                }
            }
            unreachable!("retry loop must return a receipt or error")
        };

        if !receipt.inner.status() {
            RegistrarMetrics::record_registration_stage(
                RegistrarMetrics::REGISTRATION_STAGE_TX_REVERTED,
            );
            warn!(
                signer = %signer_address,
                tx_hash = %receipt.transaction_hash,
                "registration transaction reverted onchain",
            );
            self.proof_provider.block_recovery_for_signer(signer_address);
            return Err(RegistrarError::ReceiptReverted { tx_hash: receipt.transaction_hash });
        }

        info!(
            signer = %signer_address,
            tx_hash = %receipt.transaction_hash,
            "signer registered successfully"
        );
        RegistrarMetrics::record_registration_stage(
            RegistrarMetrics::REGISTRATION_STAGE_TX_SUCCEEDED,
        );
        RegistrarMetrics::registrations_total().increment(1);

        Ok(())
    }
}

impl<P, R, T> BoundlessRegistrationBackend<P, R, T> {
    fn ensure_attestation_fresh(&self, signer: Address, timestamp_ms: u64) -> Result<()> {
        let now_ms =
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
        let age = Duration::from_millis(now_ms.saturating_sub(timestamp_ms));

        if age > self.max_attestation_age {
            warn!(
                signer = %signer,
                age_secs = age.as_secs(),
                max_age_secs = self.max_attestation_age.as_secs(),
                timestamp_ms,
                "pre-submission freshness check failed"
            );
            return Err(RegistrarError::StaleAttestationProof {
                signer,
                age,
                max_age: self.max_attestation_age,
            });
        }

        Ok(())
    }
}

/// No-op [`RegistrationBackend`] for tests that do not exercise registration.
///
/// Hand-rolled rather than `automock` because reconcile/orphan/driver tests only
/// need a successful no-op; mocking call expectations would add noise without
/// coverage value for this temporary seam.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopRegistrationBackend;

impl RegistrationBackend for NoopRegistrationBackend {
    async fn register_signer(
        &self,
        _instance_id: &str,
        _signer_address: Address,
        _attestation_bytes: &[u8],
        _cancel: &CancellationToken,
    ) -> Result<()> {
        Ok(())
    }
}
