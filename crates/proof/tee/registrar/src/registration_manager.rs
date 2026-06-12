//! Registration proof request orchestration.
//!
//! Coordinates signer-level registration work before handing completed proof
//! results to [`ProofHandler`]. This keeps the driver focused on discovery and
//! task lifecycle management while centralizing the expensive path:
//! confirm the signer still needs registration, submit the proof request,
//! then invoke the proof handler.

use std::fmt;

use alloy_primitives::Address;
use base_proof_tee_nitro_attestation_prover::{AttestationProof, AttestationProofProvider};
use base_tx_manager::TxManager;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{ProofHandler, ProofHandlerConfig, ProverInstance, RegistryClient, Result};

/// Component responsible for signer-level registration orchestration.
///
/// For each signer it confirms the signer still needs registration, starts the
/// attestation proof request when needed, and passes the completed proof to
/// [`ProofHandler`] for onchain registration. The caller owns task-level
/// deduplication; production calls flow through [`crate::RegistrationDriver`].
pub struct RegistrationManager<'a, P: ?Sized, R: ?Sized, T: ?Sized> {
    /// Proof provider used to submit or recover attestation proof requests.
    proof_provider: &'a P,
    /// Registry client used for side-effect-free registration state checks.
    registry: &'a R,
    /// Transaction manager used by the proof handler.
    tx_manager: &'a T,
    /// Semaphore bounding concurrent proof work.
    proof_semaphore: &'a Semaphore,
    /// Runtime settings for registration transaction handling.
    config: ProofHandlerConfig,
}

impl<P: ?Sized, R: ?Sized, T: ?Sized> fmt::Debug for RegistrationManager<'_, P, R, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistrationManager")
            .field("registry_address", &self.config.registry_address)
            .field("max_tx_retries", &self.config.max_tx_retries)
            .field("tx_retry_delay", &self.config.tx_retry_delay)
            .finish_non_exhaustive()
    }
}

impl<'a, P: ?Sized, R: ?Sized, T: ?Sized> RegistrationManager<'a, P, R, T> {
    /// Creates a registration manager from the registration pipeline dependencies.
    pub const fn new(
        proof_provider: &'a P,
        registry: &'a R,
        tx_manager: &'a T,
        proof_semaphore: &'a Semaphore,
        config: ProofHandlerConfig,
    ) -> Self {
        Self { proof_provider, registry, tx_manager, proof_semaphore, config }
    }
}

impl<'a, P, R, T> RegistrationManager<'a, P, R, T>
where
    P: AttestationProofProvider + ?Sized,
    R: RegistryClient + ?Sized,
    T: TxManager + ?Sized,
{
    /// Attempts to register a signer onchain if it is not already registered.
    ///
    /// This is the expensive path: confirms the signer still needs
    /// registration, generates or recovers the proof, and invokes
    /// [`ProofHandler`] with the completed proof.
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
        if !self
            .signer_needs_registration(instance, signer_address, enclave_index, signer_cancel)
            .await?
        {
            return Ok(());
        }

        let Some(proof) = self
            .generate_registration_proof(
                instance,
                signer_address,
                enclave_index,
                attestation_bytes,
                signer_cancel,
            )
            .await?
        else {
            return Ok(());
        };

        let handler =
            ProofHandler::new(self.proof_provider, self.registry, self.tx_manager, self.config);
        handler.handle_registration_proof(instance, signer_address, proof, signer_cancel).await
    }

    /// Confirms this signer still needs registration.
    pub async fn signer_needs_registration(
        &self,
        instance: &ProverInstance,
        signer_address: Address,
        enclave_index: usize,
        signer_cancel: &CancellationToken,
    ) -> Result<bool> {
        // Avoid taking locks or making registry RPCs after cancellation.
        if signer_cancel.is_cancelled() {
            debug!(signer = %signer_address, "task cancelled before registry probe");
            return Ok(false);
        }

        // Safe to cancel because this is a side-effect-free registry read.
        let already_registered = tokio::select! {
            biased;
            () = signer_cancel.cancelled() => {
                debug!(
                    signer = %signer_address,
                    enclave_index,
                    instance = %instance.instance_id,
                    "cancelled while probing registry pre-proof-gen"
                );
                return Ok(false);
            }
            res = self.registry.is_registered(signer_address) => res?,
        };
        if already_registered {
            debug!(
                signer = %signer_address,
                enclave_index,
                instance = %instance.instance_id,
                "already registered, skipping"
            );
            return Ok(false);
        }

        Ok(true)
    }

    /// Generates or recovers an attestation proof for a signer.
    ///
    /// Returns `None` on cooperative shutdown. The provider may be a direct
    /// prover in tests or local deployments, but production Boundless providers
    /// use this as the request submission/recovery point.
    pub async fn generate_registration_proof(
        &self,
        instance: &ProverInstance,
        signer_address: Address,
        enclave_index: usize,
        attestation_bytes: &[u8],
        signer_cancel: &CancellationToken,
    ) -> Result<Option<AttestationProof>> {
        // Check cancellation before the most expensive operation. Proof
        // generation and proof-result polling can take minutes via Boundless.
        if signer_cancel.is_cancelled() {
            debug!("shutdown requested, skipping proof generation");
            return Ok(None);
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
                return Ok(None);
            }
            permit = self.proof_semaphore.acquire() => {
                match permit {
                    Ok(p) => p,
                    Err(_) => {
                        warn!(
                            signer = %signer_address,
                            instance = %instance.instance_id,
                            "proof semaphore closed unexpectedly, exiting task"
                        );
                        return Ok(None);
                    }
                }
            }
        };

        // Cooperative cancel-safety around the long-running proof. Dropping the
        // provider future on cancel may abandon work the impl had already
        // started; for Boundless, any submitted offchain request is recoverable
        // via deterministic request-id derivation on the next call.
        tokio::select! {
            biased;
            () = signer_cancel.cancelled() => {
                debug!(
                    signer = %signer_address,
                    instance = %instance.instance_id,
                    "task cancelled during proof generation"
                );
                Ok(None)
            }
            res = self.proof_provider.generate_proof_for_signer(
                attestation_bytes,
                signer_address,
                signer_cancel,
            ) => {
                match res {
                    Ok(proof) => Ok(Some(proof)),
                    Err(_) if signer_cancel.is_cancelled() => {
                        debug!(
                            signer = %signer_address,
                            instance = %instance.instance_id,
                            "task cancelled during proof generation (provider returned Err after cancel)",
                        );
                        Ok(None)
                    }
                    Err(e) => Err(e.into()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
    use alloy_primitives::{B256, Bloom, Bytes};
    use alloy_rpc_types_eth::TransactionReceipt;
    use async_trait::async_trait;
    use base_proof_tee_nitro_attestation_prover::{AttestationProof, AttestationProofProvider};
    use base_tx_manager::{SendHandle, TxCandidate, TxManager, TxManagerError};
    use tokio::sync::Semaphore;
    use tokio_util::sync::CancellationToken;
    use url::Url;

    use super::*;
    use crate::{InstanceHealthStatus, RegistryClient};

    const TEST_REGISTRY_ADDRESS: Address = Address::repeat_byte(0x01);
    const TEST_SIGNER: Address = Address::repeat_byte(0x02);
    const MAX_TX_RETRIES: u32 = 3;
    const TX_RETRY_DELAY: Duration = Duration::from_secs(5);
    const CANCEL_ABORT_BUDGET: Duration = Duration::from_secs(1);
    const PRE_CANCEL_WARMUP: Duration = Duration::from_millis(50);
    const WAIT_TIMEOUT: Duration = Duration::from_secs(5);
    const ATTESTATION: &[u8] = b"stub-attestation";

    fn instance() -> ProverInstance {
        ProverInstance {
            instance_id: "i-registration-manager".to_string(),
            endpoint: Url::parse("http://10.0.0.1:8000").unwrap(),
            health_status: InstanceHealthStatus::Healthy,
            launch_time: None,
        }
    }

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

    struct ManagerHarness<P, R, T> {
        proof_provider: P,
        registry: R,
        tx_manager: T,
        proof_semaphore: Semaphore,
    }

    impl<P, R, T> ManagerHarness<P, R, T> {
        fn new(proof_provider: P, registry: R, tx_manager: T) -> Self {
            Self { proof_provider, registry, tx_manager, proof_semaphore: Semaphore::new(4) }
        }

        fn manager(&self) -> RegistrationManager<'_, P, R, T> {
            RegistrationManager::new(
                &self.proof_provider,
                &self.registry,
                &self.tx_manager,
                &self.proof_semaphore,
                ProofHandlerConfig {
                    registry_address: TEST_REGISTRY_ADDRESS,
                    max_tx_retries: MAX_TX_RETRIES,
                    tx_retry_delay: TX_RETRY_DELAY,
                },
            )
        }
    }

    #[derive(Debug)]
    struct StubProofProvider;

    #[async_trait]
    impl AttestationProofProvider for StubProofProvider {
        async fn generate_proof(
            &self,
            _attestation_bytes: &[u8],
            _cancel: &CancellationToken,
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            Ok(AttestationProof {
                output: Bytes::from_static(b"stub-output"),
                proof_bytes: Bytes::from_static(b"stub-proof"),
            })
        }
    }

    #[derive(Debug)]
    struct CancelThenErrorProofProvider;

    #[async_trait]
    impl AttestationProofProvider for CancelThenErrorProofProvider {
        async fn generate_proof(
            &self,
            _attestation_bytes: &[u8],
            cancel: &CancellationToken,
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            cancel.cancel();
            Err(base_proof_tee_nitro_attestation_prover::ProverError::Boundless(
                "simulated cancel race".into(),
            ))
        }
    }

    #[derive(Debug, Clone)]
    struct FailingTxManager {
        results: Arc<Mutex<VecDeque<Option<TxManagerError>>>>,
        sent: Arc<Mutex<Vec<Bytes>>>,
    }

    impl FailingTxManager {
        fn with_errors(errors: Vec<TxManagerError>) -> Self {
            let results = errors.into_iter().map(Some).collect();
            Self { results: Arc::new(Mutex::new(results)), sent: Arc::new(Mutex::new(vec![])) }
        }

        fn send_count(&self) -> usize {
            self.sent.lock().unwrap().len()
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
            panic!("FailingTxManager::send_async is not implemented; tests only use send()")
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    #[derive(Debug)]
    struct DynamicRegistry {
        signers: Vec<Address>,
        default_registered: bool,
    }

    impl DynamicRegistry {
        fn never_registered(signers: Vec<Address>) -> Self {
            Self { signers, default_registered: false }
        }
    }

    #[async_trait]
    impl RegistryClient for DynamicRegistry {
        async fn is_registered(&self, _signer: Address) -> Result<bool> {
            Ok(self.default_registered)
        }

        async fn get_registered_signers(&self) -> Result<Vec<Address>> {
            Ok(self.signers.clone())
        }
    }

    #[derive(Debug)]
    struct StallingRegistry;

    #[async_trait]
    impl RegistryClient for StallingRegistry {
        async fn is_registered(&self, _signer: Address) -> Result<bool> {
            std::future::pending::<()>().await;
            Ok(false)
        }

        async fn get_registered_signers(&self) -> Result<Vec<Address>> {
            Ok(vec![])
        }
    }

    async fn register<P, R, T>(
        harness: &ManagerHarness<P, R, T>,
        cancel: &CancellationToken,
    ) -> Result<()>
    where
        P: AttestationProofProvider,
        R: RegistryClient,
        T: TxManager,
    {
        harness.manager().register_signer(&instance(), TEST_SIGNER, 0, ATTESTATION, cancel).await
    }

    #[tokio::test]
    async fn register_signer_provider_err_after_cancel_returns_ok() {
        let tx = FailingTxManager::with_errors(vec![]);
        let harness = ManagerHarness::new(
            CancelThenErrorProofProvider,
            DynamicRegistry::never_registered(vec![]),
            tx.clone(),
        );
        let cancel = CancellationToken::new();

        let result = register(&harness, &cancel).await;

        assert!(result.is_ok(), "provider Err after cancel must be mapped to Ok(()): {result:?}",);
        assert_eq!(tx.send_count(), 0, "cancelled task must not submit a transaction");
    }

    #[tokio::test]
    async fn register_signer_aborts_promptly_when_cancel_fires_during_registry_stall() {
        let harness = Arc::new(ManagerHarness::new(
            StubProofProvider,
            StallingRegistry,
            FailingTxManager::with_errors(vec![]),
        ));
        let signer_cancel = CancellationToken::new();
        let task_harness = Arc::clone(&harness);
        let task_cancel = signer_cancel.clone();
        let inst = instance();
        let handle = tokio::spawn(async move {
            let start = tokio::time::Instant::now();
            let res = task_harness
                .manager()
                .register_signer(&inst, TEST_SIGNER, 0, ATTESTATION, &task_cancel)
                .await;
            (res, start.elapsed())
        });

        tokio::time::sleep(PRE_CANCEL_WARMUP).await;
        signer_cancel.cancel();

        let (result, elapsed) = tokio::time::timeout(WAIT_TIMEOUT, handle)
            .await
            .expect("register_signer must not hang past the timeout")
            .expect("spawned task must not panic");

        assert!(result.is_ok(), "cancel-induced exit must be Ok(()): {result:?}");
        assert!(
            elapsed < CANCEL_ABORT_BUDGET,
            "cancel must abort the registry stall within {CANCEL_ABORT_BUDGET:?} (took {elapsed:?})",
        );
    }
}
