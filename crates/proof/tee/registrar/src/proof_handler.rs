//! Proof result handling and onchain signer registration.
//!
//! Converts completed attestation proof results into
//! `TEEProverRegistry.registerSigner` transactions and submits them through
//! the transaction manager with delivery retries.

use std::{fmt, time::Duration};

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolCall;
use base_proof_contracts::ITEEProverRegistry;
use base_proof_tee_nitro_attestation_prover::{AttestationProof, AttestationProofProvider};
use base_tx_manager::{TxCandidate, TxManager, TxManagerError};
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
    /// Proof provider used for proof recovery blocking after reverted transactions.
    proof_provider: &'a P,
    /// Registry client used for side-effect-free registration state checks.
    registry: &'a R,
    /// Transaction manager used to deliver `registerSigner`.
    tx_manager: &'a T,
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
        config: ProofHandlerConfig,
    ) -> Self {
        Self { proof_provider, registry, tx_manager, config }
    }
}

impl<'a, P, R, T> ProofHandler<'a, P, R, T>
where
    P: AttestationProofProvider + ?Sized,
    R: RegistryClient + ?Sized,
    T: TxManager + ?Sized,
{
    /// Handles a completed registration proof by submitting its onchain transaction.
    pub async fn handle_registration_proof(
        &self,
        instance: &ProverInstance,
        signer_address: Address,
        proof: AttestationProof,
        signer_cancel: &CancellationToken,
    ) -> Result<()> {
        if signer_cancel.is_cancelled() {
            debug!("shutdown requested, skipping transaction submission");
            return Ok(());
        }

        let candidate = self.registration_tx_candidate(instance, signer_address, proof);
        self.submit_registration_candidate(signer_address, candidate, signer_cancel).await
    }

    /// Builds the `registerSigner` transaction candidate for a generated proof.
    pub fn registration_tx_candidate(
        &self,
        instance: &ProverInstance,
        signer_address: Address,
        proof: AttestationProof,
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
            self.proof_provider.block_recovery_for_signer(signer_address);
            return Err(RegistrarError::ReceiptReverted { tx_hash: receipt.transaction_hash });
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
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use alloy_rpc_types_eth::TransactionReceipt;
    use async_trait::async_trait;
    use base_proof_tee_nitro_attestation_prover::{AttestationProof, AttestationProofProvider};
    use base_tx_manager::{SendHandle, TxManagerError};
    use rstest::rstest;
    use url::Url;

    use super::*;
    use crate::{
        InstanceHealthStatus, RegistryClient,
        test_utils::{stub_receipt, stub_receipt_with_status},
    };

    const TEST_REGISTRY_ADDRESS: Address = Address::repeat_byte(0x01);
    const TEST_SIGNER: Address = Address::repeat_byte(0x02);
    const MAX_TX_RETRIES: u32 = 3;
    const TX_RETRY_DELAY: Duration = Duration::from_secs(5);

    fn instance() -> ProverInstance {
        ProverInstance {
            instance_id: "i-proof-handler".to_string(),
            endpoint: Url::parse("http://10.0.0.1:8000").unwrap(),
            health_status: InstanceHealthStatus::Healthy,
            launch_time: None,
        }
    }

    struct HandlerHarness<P, R, T> {
        proof_provider: P,
        registry: R,
        tx_manager: T,
    }

    impl<P, R, T> HandlerHarness<P, R, T> {
        fn new(proof_provider: P, registry: R, tx_manager: T) -> Self {
            Self { proof_provider, registry, tx_manager }
        }

        fn handler(&self) -> ProofHandler<'_, P, R, T> {
            ProofHandler::new(
                &self.proof_provider,
                &self.registry,
                &self.tx_manager,
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

    #[derive(Debug, Default)]
    struct TrackingProofProvider {
        blocked_signers: Arc<Mutex<Vec<Address>>>,
    }

    impl TrackingProofProvider {
        fn blocked_signers(&self) -> Vec<Address> {
            self.blocked_signers.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AttestationProofProvider for TrackingProofProvider {
        async fn generate_proof(
            &self,
            _attestation_bytes: &[u8],
            _cancel: &CancellationToken,
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            Ok(stub_proof())
        }

        fn block_recovery_for_signer(&self, signer: Address) {
            self.blocked_signers.lock().unwrap().push(signer);
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

    #[derive(Debug, Clone)]
    struct ReceiptTxManager {
        receipt: TransactionReceipt,
        sent: Arc<Mutex<Vec<Bytes>>>,
    }

    impl ReceiptTxManager {
        fn new(receipt: TransactionReceipt) -> Self {
            Self { receipt, sent: Arc::new(Mutex::new(vec![])) }
        }

        fn send_count(&self) -> usize {
            self.sent.lock().unwrap().len()
        }
    }

    impl TxManager for ReceiptTxManager {
        async fn send(&self, candidate: TxCandidate) -> base_tx_manager::SendResponse {
            self.sent.lock().unwrap().push(candidate.tx_data);
            Ok(self.receipt.clone())
        }

        async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
            panic!("ReceiptTxManager::send_async is not implemented; tests only use send()")
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

        fn registered(signers: Vec<Address>) -> Self {
            Self { signers, default_registered: true }
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

    fn assert_all_calldata_identical(sent: &[Bytes]) {
        if sent.len() < 2 {
            return;
        }
        for (i, entry) in sent.iter().enumerate().skip(1) {
            assert_eq!(&sent[0], entry, "calldata mismatch: sent[0] != sent[{i}]");
        }
    }

    fn stub_proof() -> AttestationProof {
        AttestationProof {
            output: Bytes::from_static(b"stub-output"),
            proof_bytes: Bytes::from_static(b"stub-proof"),
        }
    }

    async fn handle_proof<P, R, T>(
        harness: &HandlerHarness<P, R, T>,
        cancel: &CancellationToken,
    ) -> Result<()>
    where
        P: AttestationProofProvider,
        R: RegistryClient,
        T: TxManager,
    {
        harness
            .handler()
            .handle_registration_proof(&instance(), TEST_SIGNER, stub_proof(), cancel)
            .await
    }

    #[tokio::test(start_paused = true)]
    async fn handle_registration_proof_already_registered_after_error_returns_ok() {
        let tx = FailingTxManager::with_errors(vec![TxManagerError::Rpc("nonce race".into())]);
        let harness =
            HandlerHarness::new(StubProofProvider, DynamicRegistry::registered(vec![]), tx.clone());

        let result = handle_proof(&harness, &CancellationToken::new()).await;

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
    async fn handle_registration_proof_non_retryable_error_aborts_immediately(
        #[case] error: TxManagerError,
    ) {
        let tx = FailingTxManager::with_errors(vec![error]);
        let harness = HandlerHarness::new(
            StubProofProvider,
            DynamicRegistry::never_registered(vec![]),
            tx.clone(),
        );

        let result = handle_proof(&harness, &CancellationToken::new()).await;

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
    async fn handle_registration_proof_tx_outcome(
        #[case] errors: Vec<TxManagerError>,
        #[case] should_succeed: bool,
        #[case] expected_sends: usize,
    ) {
        let tx = FailingTxManager::with_errors(errors);
        let harness = HandlerHarness::new(
            StubProofProvider,
            DynamicRegistry::never_registered(vec![]),
            tx.clone(),
        );

        let result = handle_proof(&harness, &CancellationToken::new()).await;

        assert_eq!(result.is_ok(), should_succeed, "unexpected registration result: {result:?}",);
        assert_eq!(tx.send_count(), expected_sends);
        assert_all_calldata_identical(&tx.sent_calldata());
    }

    #[tokio::test(start_paused = true)]
    async fn handle_registration_proof_reverted_receipt_blocks_recovery() {
        let tx = ReceiptTxManager::new(stub_receipt_with_status(false));
        let harness = HandlerHarness::new(
            TrackingProofProvider::default(),
            DynamicRegistry::never_registered(vec![]),
            tx.clone(),
        );

        let result = handle_proof(&harness, &CancellationToken::new()).await;

        assert!(
            matches!(result, Err(RegistrarError::ReceiptReverted { tx_hash }) if tx_hash == tx.receipt.transaction_hash),
            "reverted receipt should fail with ReceiptReverted: {result:?}"
        );
        assert_eq!(tx.send_count(), 1, "should submit exactly one tx");
        assert_eq!(harness.proof_provider.blocked_signers(), vec![TEST_SIGNER]);
    }

    #[tokio::test(start_paused = true)]
    async fn handle_registration_proof_cancellation_during_retry_sleep_aborts() {
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

        let result = handle_proof(&harness, &cancel).await;

        assert!(result.is_ok(), "cancel-induced exit should be Ok(()): {result:?}");
        assert_eq!(tx.send_count(), 1, "should abort during retry sleep");
    }

    #[tokio::test(start_paused = true)]
    async fn handle_registration_proof_cancellation_before_loop_sends_nothing() {
        let tx = FailingTxManager::with_errors(vec![]);
        let harness = HandlerHarness::new(
            StubProofProvider,
            DynamicRegistry::never_registered(vec![]),
            tx.clone(),
        );
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = handle_proof(&harness, &cancel).await;

        assert!(result.is_ok(), "pre-cancel should be a cooperative success: {result:?}");
        assert_eq!(tx.send_count(), 0, "should not send any tx after pre-cancellation");
    }
}
