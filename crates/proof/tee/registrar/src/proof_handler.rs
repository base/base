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

/// Runtime parameters for proof-result handling.
#[derive(Debug, Clone, Copy)]
pub struct ProofHandlerConfig {
    /// `TEEProverRegistry` contract address on L1.
    pub registry_address: Address,
    /// Maximum number of transaction submission retries for transient errors.
    pub max_tx_retries: u32,
    /// Delay between transaction submission retries.
    pub tx_retry_delay: Duration,
}

/// Component responsible for turning attestation proof results into durable
/// onchain signer registrations.
pub struct ProofHandler<'a, P: ?Sized, R: ?Sized, T: ?Sized> {
    /// Proof provider used to poll or generate the attestation proof result.
    proof_provider: &'a P,
    /// Registry client used for side-effect-free registration state checks.
    registry: &'a R,
    /// Transaction manager used to deliver `registerSigner`.
    tx_manager: &'a T,
    /// Semaphore bounding concurrent proof work.
    proof_semaphore: &'a Semaphore,
    /// Process-local signer set used to deduplicate concurrent attempts.
    in_flight_registrations: &'a Arc<Mutex<HashSet<Address>>>,
    /// Runtime settings for registration transaction handling.
    config: ProofHandlerConfig,
}

impl<P: ?Sized, R: ?Sized, T: ?Sized> fmt::Debug for ProofHandler<'_, P, R, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProofHandler")
            .field("registry_address", &self.config.registry_address)
            .field("max_tx_retries", &self.config.max_tx_retries)
            .field("tx_retry_delay", &self.config.tx_retry_delay)
            .finish_non_exhaustive()
    }
}

impl<'a, P: ?Sized, R: ?Sized, T: ?Sized> ProofHandler<'a, P, R, T> {
    /// Creates a proof handler from the registration pipeline dependencies.
    pub const fn new(
        proof_provider: &'a P,
        registry: &'a R,
        tx_manager: &'a T,
        proof_semaphore: &'a Semaphore,
        in_flight_registrations: &'a Arc<Mutex<HashSet<Address>>>,
        config: ProofHandlerConfig,
    ) -> Self {
        Self {
            proof_provider,
            registry,
            tx_manager,
            proof_semaphore,
            in_flight_registrations,
            config,
        }
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
        // Recover from poisoning so guard cleanup still runs.
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
        let Some(_in_flight) = self
            .prepare_registration_attempt(instance, signer_address, enclave_index, signer_cancel)
            .await?
        else {
            return Ok(());
        };

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

        if signer_cancel.is_cancelled() {
            debug!("shutdown requested, skipping transaction submission");
            return Ok(());
        }

        let candidate = self.registration_tx_candidate(instance, signer_address, proof);
        self.submit_registration_candidate(signer_address, candidate, signer_cancel).await
    }

    /// Reserves this signer and confirms it still needs registration.
    pub async fn prepare_registration_attempt(
        &self,
        instance: &ProverInstance,
        signer_address: Address,
        enclave_index: usize,
        signer_cancel: &CancellationToken,
    ) -> Result<Option<InFlightRegistrationGuard>> {
        // Avoid taking locks or making registry RPCs after cancellation.
        if signer_cancel.is_cancelled() {
            debug!(signer = %signer_address, "task cancelled before registry probe");
            return Ok(None);
        }

        // Reserve this signer in the in-flight set before the `is_registered`
        // precheck. If another concurrent task already owns it, short-circuit
        // so we do not race past the precheck, regenerate the proof, and
        // submit a duplicate registration transaction.
        let Some(in_flight) =
            InFlightRegistrationGuard::try_acquire(self.in_flight_registrations, signer_address)
        else {
            debug!(
                signer = %signer_address,
                enclave_index,
                instance = %instance.instance_id,
                "registration already in flight for this signer, skipping duplicate",
            );
            return Ok(None);
        };

        // Safe to cancel because this is a side-effect-free registry read.
        let already_registered = tokio::select! {
            biased;
            () = signer_cancel.cancelled() => {
                debug!(
                    signer = %signer_address,
                    "cancelled while probing registry pre-proof-gen"
                );
                return Ok(None);
            }
            res = self.registry.is_registered(signer_address) => res?,
        };
        if already_registered {
            debug!(signer = %signer_address, "already registered, skipping");
            return Ok(None);
        }

        Ok(Some(in_flight))
    }

    /// Generates a registration proof for a signer, returning `None` on cooperative shutdown.
    pub async fn generate_registration_proof(
        &self,
        instance: &ProverInstance,
        signer_address: Address,
        enclave_index: usize,
        attestation_bytes: &[u8],
        signer_cancel: &CancellationToken,
    ) -> Result<Option<base_proof_tee_nitro_attestation_prover::AttestationProof>> {
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

    /// Builds the `registerSigner` transaction candidate for a generated proof.
    pub fn registration_tx_candidate(
        &self,
        instance: &ProverInstance,
        signer_address: Address,
        proof: base_proof_tee_nitro_attestation_prover::AttestationProof,
    ) -> TxCandidate {
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

        candidate
    }

    /// Submits a registration transaction and retries transient delivery failures.
    pub async fn submit_registration_candidate(
        &self,
        signer_address: Address,
        candidate: TxCandidate,
        signer_cancel: &CancellationToken,
    ) -> Result<()> {
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

            match self.tx_manager.send(candidate.clone()).await {
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
                        res = self.registry.is_registered(signer_address) => res,
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
                            self.proof_provider.block_recovery_for_signer(signer_address);
                        }
                        return Err(RegistrarError::from(e));
                    }

                    tx_retries += 1;
                    if tx_retries > self.config.max_tx_retries {
                        return Err(RegistrarError::from(e));
                    }

                    warn!(
                        error = %e,
                        signer = %signer_address,
                        retry = tx_retries,
                        max_retries = self.config.max_tx_retries,
                        "tx submission failed, retrying with same proof"
                    );

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
                        () = tokio::time::sleep(self.config.tx_retry_delay) => {}
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

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashSet, VecDeque},
        sync::{
            Arc, Mutex,
            atomic::{AtomicU32, Ordering},
        },
        time::Duration,
    };

    use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
    use alloy_primitives::{B256, Bloom};
    use alloy_rpc_types_eth::TransactionReceipt;
    use async_trait::async_trait;
    use base_proof_tee_nitro_attestation_prover::{AttestationProof, AttestationProofProvider};
    use base_tx_manager::{SendHandle, TxManagerError};
    use rstest::rstest;
    use tokio::sync::Semaphore;
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
            instance_id: "i-proof-handler".to_string(),
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

    struct HandlerHarness<P, R, T> {
        proof_provider: P,
        registry: R,
        tx_manager: T,
        proof_semaphore: Semaphore,
        in_flight_registrations: Arc<Mutex<HashSet<Address>>>,
    }

    impl<P, R, T> HandlerHarness<P, R, T> {
        fn new(proof_provider: P, registry: R, tx_manager: T) -> Self {
            Self {
                proof_provider,
                registry,
                tx_manager,
                proof_semaphore: Semaphore::new(4),
                in_flight_registrations: Arc::new(Mutex::new(HashSet::new())),
            }
        }

        fn handler(&self) -> ProofHandler<'_, P, R, T> {
            ProofHandler::new(
                &self.proof_provider,
                &self.registry,
                &self.tx_manager,
                &self.proof_semaphore,
                &self.in_flight_registrations,
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
            _cancel: &CancellationToken,
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
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

    #[derive(Debug)]
    struct YieldingProofProvider {
        call_count: Arc<AtomicU32>,
    }

    impl YieldingProofProvider {
        fn new() -> Self {
            Self { call_count: Arc::new(AtomicU32::new(0)) }
        }

        fn call_count(&self) -> u32 {
            self.call_count.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl AttestationProofProvider for YieldingProofProvider {
        async fn generate_proof(
            &self,
            _attestation_bytes: &[u8],
            _cancel: &CancellationToken,
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            Ok(AttestationProof {
                output: Bytes::from_static(b"stub-output"),
                proof_bytes: Bytes::from_static(b"stub-proof"),
            })
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
            panic!("FailingTxManager::send_async is not implemented; tests only use send()")
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    #[derive(Debug)]
    struct DynamicRegistry {
        signers: Vec<Address>,
        responses: Mutex<VecDeque<bool>>,
        default_registered: bool,
    }

    impl DynamicRegistry {
        fn never_registered(signers: Vec<Address>) -> Self {
            Self { signers, responses: Mutex::new(VecDeque::new()), default_registered: false }
        }

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

    fn assert_all_calldata_identical(sent: &[Bytes]) {
        if sent.len() < 2 {
            return;
        }
        for (i, entry) in sent.iter().enumerate().skip(1) {
            assert_eq!(&sent[0], entry, "calldata mismatch: sent[0] != sent[{i}]");
        }
    }

    async fn register<P, R, T>(
        harness: &HandlerHarness<P, R, T>,
        cancel: &CancellationToken,
    ) -> Result<()>
    where
        P: AttestationProofProvider,
        R: RegistryClient,
        T: TxManager,
    {
        harness.handler().register_signer(&instance(), TEST_SIGNER, 0, ATTESTATION, cancel).await
    }

    #[tokio::test(start_paused = true)]
    async fn register_signer_already_registered_after_error_returns_ok() {
        let tx = FailingTxManager::with_errors(vec![TxManagerError::Rpc("nonce race".into())]);
        let harness = HandlerHarness::new(
            StubProofProvider,
            DynamicRegistry::registered_after_first_check(vec![]),
            tx.clone(),
        );

        let result = register(&harness, &CancellationToken::new()).await;

        assert!(result.is_ok(), "should succeed when signer is registered onchain: {result:?}");
        assert_eq!(tx.send_count(), 1);
    }

    #[rstest]
    #[case::execution_reverted(TxManagerError::ExecutionReverted {
        reason: Some("bad proof".into()),
        data: None,
    })]
    #[case::insufficient_funds(TxManagerError::InsufficientFunds)]
    #[case::fee_limit_exceeded(TxManagerError::FeeLimitExceeded { fee: 500, ceiling: 100 })]
    #[tokio::test(start_paused = true)]
    async fn register_signer_non_retryable_error_aborts_immediately(#[case] error: TxManagerError) {
        let tx = FailingTxManager::with_errors(vec![error]);
        let harness = HandlerHarness::new(
            StubProofProvider,
            DynamicRegistry::never_registered(vec![]),
            tx.clone(),
        );

        let result = register(&harness, &CancellationToken::new()).await;

        assert!(result.is_err(), "non-retryable tx errors should propagate");
        assert_eq!(tx.send_count(), 1, "should not retry after non-retryable error");
    }

    #[rstest]
    #[case::immediate_success(vec![], true, 1)]
    #[case::transient_retry_success(
        vec![
            TxManagerError::Rpc("transient 1".into()),
            TxManagerError::Rpc("transient 2".into()),
        ],
        true,
        3,
    )]
    #[case::retry_exhaustion(
        (0..=MAX_TX_RETRIES)
            .map(|_| TxManagerError::Rpc("persistent failure".into()))
            .collect(),
        false,
        (MAX_TX_RETRIES + 1) as usize,
    )]
    #[case::transient_then_execution_reverted(
        vec![
            TxManagerError::Rpc("transient".into()),
            TxManagerError::ExecutionReverted { reason: None, data: None },
        ],
        false,
        2,
    )]
    #[tokio::test(start_paused = true)]
    async fn register_signer_tx_outcome(
        #[case] errors: Vec<TxManagerError>,
        #[case] should_succeed: bool,
        #[case] expected_sends: usize,
    ) {
        let tx = FailingTxManager::with_errors(errors);
        let harness = HandlerHarness::new(
            CountingProofProvider::new(),
            DynamicRegistry::never_registered(vec![]),
            tx.clone(),
        );

        let result = register(&harness, &CancellationToken::new()).await;

        assert_eq!(result.is_ok(), should_succeed, "unexpected registration result: {result:?}",);
        assert_eq!(tx.send_count(), expected_sends);
        assert_all_calldata_identical(&tx.sent_calldata());
        assert_eq!(harness.proof_provider.call_count(), 1, "proof should be generated once");
    }

    #[tokio::test(start_paused = true)]
    async fn register_signer_cancellation_during_retry_sleep_aborts() {
        let tx = FailingTxManager::with_errors(vec![
            TxManagerError::Rpc("fail 1".into()),
            TxManagerError::Rpc("fail 2".into()),
            TxManagerError::Rpc("fail 3".into()),
        ]);
        let harness = HandlerHarness::new(
            StubProofProvider,
            DynamicRegistry::never_registered(vec![]),
            tx.clone(),
        );
        let cancel = CancellationToken::new();
        let cancel_handle = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            cancel_handle.cancel();
        });

        let result = register(&harness, &cancel).await;

        assert!(result.is_ok(), "cancel-induced exit should be Ok(()): {result:?}");
        assert_eq!(tx.send_count(), 1, "should abort during retry sleep");
    }

    #[tokio::test(start_paused = true)]
    async fn register_signer_cancellation_before_loop_sends_nothing() {
        let tx = FailingTxManager::with_errors(vec![]);
        let harness = HandlerHarness::new(
            StubProofProvider,
            DynamicRegistry::never_registered(vec![]),
            tx.clone(),
        );
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = register(&harness, &cancel).await;

        assert!(result.is_ok(), "pre-cancel should be a cooperative success: {result:?}");
        assert_eq!(tx.send_count(), 0, "should not send any tx after pre-cancellation");
    }

    #[tokio::test]
    async fn register_signer_provider_err_after_cancel_returns_ok() {
        let tx = FailingTxManager::with_errors(vec![]);
        let harness = HandlerHarness::new(
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
        let harness = Arc::new(HandlerHarness::new(
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
                .handler()
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

    #[tokio::test]
    async fn register_signer_concurrent_same_signer_dedups() {
        let tx = FailingTxManager::with_errors(vec![]);
        let harness = HandlerHarness::new(
            YieldingProofProvider::new(),
            DynamicRegistry::never_registered(vec![]),
            tx.clone(),
        );
        let cancel = CancellationToken::new();
        let handler = harness.handler();
        let inst = instance();

        let (r1, r2) = tokio::join!(
            handler.register_signer(&inst, TEST_SIGNER, 0, ATTESTATION, &cancel),
            handler.register_signer(&inst, TEST_SIGNER, 0, ATTESTATION, &cancel),
        );

        assert!(r1.is_ok(), "first concurrent registration failed: {r1:?}");
        assert!(r2.is_ok(), "second concurrent registration failed: {r2:?}");
        assert_eq!(tx.send_count(), 1, "same signer must dedup to a single tx");
        assert_eq!(harness.proof_provider.call_count(), 1, "proof should be generated once");
    }

    #[rstest]
    #[case::completion(vec![], false)]
    #[case::failure(vec![TxManagerError::InsufficientFunds], true)]
    #[tokio::test]
    async fn register_signer_in_flight_slot_released_after_attempt(
        #[case] errors: Vec<TxManagerError>,
        #[case] first_attempt_should_fail: bool,
    ) {
        let tx = FailingTxManager::with_errors(errors);
        let harness = HandlerHarness::new(
            StubProofProvider,
            DynamicRegistry::never_registered(vec![]),
            tx.clone(),
        );
        let cancel = CancellationToken::new();

        let first_result = register(&harness, &cancel).await;
        assert_eq!(
            first_result.is_err(),
            first_attempt_should_fail,
            "unexpected first registration result: {first_result:?}",
        );
        register(&harness, &cancel).await.unwrap();

        assert_eq!(tx.send_count(), 2, "registration must release in-flight slot");
    }
}
