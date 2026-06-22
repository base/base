//! Signer lifecycle orchestration for the registrar.
//!
//! Coordinates signer-level registration tasks and orphaned signer cleanup
//! after the driver has resolved discovered prover instances.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolCall;
use base_proof_contracts::{ITEEProverRegistry, TEEProverRegistryClient};
use base_proof_tee_nitro_attestation_prover::AttestationProofProvider;
use base_tx_manager::{TxCandidate, TxManager, TxManagerError};
use tokio::{
    sync::Semaphore,
    task::{self, JoinError, JoinSet},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{DiscoveryResolution, RegistrarError, RegistrarMetrics, Result};

/// Default maximum number of transaction submission retries for transient
/// errors before giving up.
pub const DEFAULT_MAX_TX_RETRIES: u32 = 3;

/// Default initial delay between transaction submission retries in seconds.
pub const DEFAULT_TX_RETRY_DELAY_SECS: u64 = 5;

/// Maximum exponential backoff delay between transaction submission retries.
const MAX_TX_RETRY_BACKOFF_DELAY: Duration = Duration::from_secs(60);

/// State for a proof-generation task currently in-flight.
///
/// One entry per signer address. The pending map is keyed by [`Address`] so
/// each signer has at most one active registration task.
#[derive(Debug)]
pub struct PendingRegistration {
    /// Originating instance ID, used to preserve tasks when the source
    /// instance is unresolved and to attribute log lines.
    pub instance_id: String,
    /// `JoinSet` task id for this proof task.
    pub task_id: task::Id,
    /// Cooperative cancel handle for this single task.
    pub cancel: CancellationToken,
}

/// Coordinates signer registration and orphan signer deregistration.
#[derive(Debug)]
pub struct SignerManager<P, R, T> {
    proof_provider: P,
    registry: R,
    tx_manager: T,
    proof_semaphore: Semaphore,
    registry_address: Address,
    max_tx_retries: u32,
    tx_retry_delay: Duration,
}

impl<P, R, T> SignerManager<P, R, T> {
    /// Creates a signer manager from the signer lifecycle dependencies.
    pub fn new(
        proof_provider: P,
        registry: R,
        tx_manager: T,
        registry_address: Address,
        max_concurrency: usize,
        max_tx_retries: u32,
        tx_retry_delay: Duration,
    ) -> Self {
        let proof_semaphore = Semaphore::new(max_concurrency.max(1));
        Self {
            proof_provider,
            registry,
            tx_manager,
            proof_semaphore,
            registry_address,
            max_tx_retries,
            tx_retry_delay,
        }
    }
}

/// Driver-owned set of in-flight proof-generation tasks.
#[derive(Debug, Default)]
pub struct ProofTaskSet {
    tasks: JoinSet<(Address, Result<()>)>,
    /// Pending registration tasks keyed by signer address.
    pub pending: HashMap<Address, PendingRegistration>,
}

impl ProofTaskSet {
    /// Drains every task that has already finished from `tasks`.
    pub fn reap_finished_tasks(&mut self) {
        while let Some(joined) = self.tasks.try_join_next_with_id() {
            self.apply_join_outcome(joined);
        }
    }

    /// Consumes one `JoinSet` outcome and updates `pending` plus metrics.
    fn apply_join_outcome(
        &mut self,
        joined: std::result::Result<(task::Id, (Address, Result<()>)), JoinError>,
    ) {
        RegistrarMetrics::proof_tasks_completed().increment(1);
        match joined {
            Ok((id, (signer, result))) => {
                let removed = self.pending.remove(&signer);
                let instance_id =
                    removed.as_ref().map_or("missing", |entry| entry.instance_id.as_str());

                match result {
                    Ok(()) => {
                        debug!(
                            task_id = ?id,
                            signer = %signer,
                            instance = %instance_id,
                            pending_entry_found = removed.is_some(),
                            "proof task completed",
                        );
                    }
                    Err(e) => {
                        warn!(
                            task_id = ?id,
                            error = %e,
                            signer = %signer,
                            instance = %instance_id,
                            pending_entry_found = removed.is_some(),
                            "proof task failed"
                        );
                        RegistrarMetrics::processing_errors_total().increment(1);
                    }
                }
            }
            Err(join_err) => {
                let id = join_err.id();
                let removed = self.pending.extract_if(|_, p| p.task_id == id).next();
                let signer = removed.as_ref().map(|(signer, _)| *signer);
                warn!(
                    task_id = ?id,
                    error = %join_err,
                    signer = ?signer,
                    instance = ?removed.as_ref().map(|(_, t)| t.instance_id.as_str()),
                    pending_entry_found = removed.is_some(),
                    "proof task join error (panic or abort)"
                );
                RegistrarMetrics::processing_errors_total().increment(1);
            }
        }
    }

    /// Cancels every pending task cooperatively and awaits natural completion.
    pub async fn drain_proof_tasks(&mut self) {
        for task in self.pending.values_mut() {
            if !task.cancel.is_cancelled() {
                RegistrarMetrics::proof_tasks_cancelled().increment(1);
                task.cancel.cancel();
            }
        }
        while let Some(joined) = self.tasks.join_next_with_id().await {
            self.apply_join_outcome(joined);
        }
        RegistrarMetrics::proof_tasks_pending().set(0.0);
    }
}

impl<P, R, T> SignerManager<P, R, T>
where
    P: AttestationProofProvider,
    R: TEEProverRegistryClient,
    T: TxManager,
{
    /// Attempts to register a signer onchain if it is not already registered.
    pub async fn register_signer(
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

        let Some(permit) = signer_cancel.run_until_cancelled(self.proof_semaphore.acquire()).await
        else {
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
            return Ok(());
        };
        let proof = match proof_result {
            Ok(proof) => proof,
            Err(_) if signer_cancel.is_cancelled() => {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
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

                // Do not wrap send in run_until_cancelled: dropping it after nonce
                // acquisition can leave a nonce gap.
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
                            if matches!(e, TxManagerError::ExecutionReverted { .. }) {
                                warn!(
                                    signer = %signer_address,
                                    "execution reverted, blocking proof recovery for signer"
                                );
                                self.proof_provider.block_recovery_for_signer(signer_address);
                            }
                            return Err(RegistrarError::from(e));
                        }

                        if retry == self.max_tx_retries {
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

    /// Queries onchain signers and deregisters orphans.
    pub async fn run_orphan_dereg(
        &self,
        protected_signers: &HashSet<Address>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let Some(registered_signers) =
            cancel.run_until_cancelled(self.registry.get_registered_signers()).await.transpose()?
        else {
            debug!("cancelled before loading registered signers for orphan dereg");
            return Ok(());
        };

        for signer in
            registered_signers.into_iter().filter(|addr| !protected_signers.contains(addr))
        {
            if cancel.is_cancelled() {
                debug!(signer = %signer, "shutdown requested, skipping orphan deregistration");
                break;
            }

            let candidate = TxCandidate {
                tx_data: Bytes::from(
                    ITEEProverRegistry::deregisterSignerCall { signer }.abi_encode(),
                ),
                to: Some(self.registry_address),
                ..Default::default()
            };
            info!(
                signer = %signer,
                registry = %self.registry_address,
                tx = ?candidate,
                "sending deregistration tx candidate",
            );

            match self.tx_manager.send(candidate).await {
                Ok(receipt) if !receipt.inner.status() => {
                    warn!(
                        signer = %signer,
                        tx_hash = %receipt.transaction_hash,
                        "deregistration transaction reverted onchain",
                    );
                    RegistrarMetrics::processing_errors_total().increment(1);
                }
                Ok(receipt) => {
                    info!(
                        signer = %signer,
                        tx_hash = %receipt.transaction_hash,
                        "signer deregistered"
                    );
                    RegistrarMetrics::deregistrations_total().increment(1);
                }
                Err(e) => {
                    warn!(error = %e, signer = %signer, "failed to deregister signer");
                    RegistrarMetrics::processing_errors_total().increment(1);
                }
            }
        }

        Ok(())
    }
}

impl<P, R, T> SignerManager<P, R, T>
where
    P: AttestationProofProvider + 'static,
    R: TEEProverRegistryClient + 'static,
    T: TxManager + 'static,
{
    /// Reconciles in-flight registration tasks against fetched prover signers.
    ///
    /// New proof-task child tokens are derived from `cancel`, which is owned
    /// by the driver so every signer lifecycle operation shares one shutdown
    /// source of truth.
    pub fn reconcile_proof_tasks(
        self: &Arc<Self>,
        resolution: &DiscoveryResolution,
        proof_tasks: &mut ProofTaskSet,
        cancel: &CancellationToken,
    ) {
        if cancel.is_cancelled() {
            return;
        }

        for (signer, task) in &mut proof_tasks.pending {
            if task.cancel.is_cancelled()
                || resolution.registerable.iter().any(|entry| entry.signer == *signer)
                || resolution.unresolved_instance_ids.contains(&task.instance_id)
            {
                continue;
            }

            info!(
                signer = %signer,
                instance = %task.instance_id,
                "cancelling proof task: signer no longer registerable"
            );
            task.cancel.cancel();
            RegistrarMetrics::proof_tasks_cancelled().increment(1);
        }

        for entry in &resolution.registerable {
            if proof_tasks.pending.contains_key(&entry.signer) {
                continue;
            }
            let signer_cancel = cancel.child_token();
            let manager = Arc::clone(self);
            let instance_id = entry.instance.instance_id.clone();
            let task_instance_id = instance_id.clone();
            let attestation = entry.attestation.clone();
            let task_cancel = signer_cancel.clone();
            let signer = entry.signer;

            let handle = proof_tasks.tasks.spawn(async move {
                let result = manager
                    .register_signer(&task_instance_id, signer, &attestation, &task_cancel)
                    .await;
                (signer, result)
            });
            proof_tasks.pending.insert(
                signer,
                PendingRegistration { instance_id, task_id: handle.id(), cancel: signer_cancel },
            );
            RegistrarMetrics::proof_tasks_spawned().increment(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashSet, VecDeque},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use alloy_primitives::{Address, B256};
    use async_trait::async_trait;
    use base_proof_tee_nitro_attestation_prover::AttestationProof;
    use base_tx_manager::{SendHandle, TxManagerError};
    use tokio::sync::Notify;

    use super::*;
    use crate::{
        DEFAULT_MAX_CONCURRENCY, DEFAULT_MAX_TX_RETRIES, DEFAULT_TX_RETRY_DELAY_SECS,
        RegisterableSigner, RegistrarError,
        test_utils::{
            EP1, EP2, HARDHAT_KEY_0, HARDHAT_KEY_1, TEST_REGISTRY_ADDRESS, healthy_prover_instance,
            signer_from_private_key, stub_receipt_with_status,
        },
    };

    const TEST_PENDING_INSTANCE_ID: &str = "i-pending-test";
    const GATED_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
    const ATTESTATION: &[u8] = b"stub-attestation";
    const SIGNER_A: Address = Address::new([0xAA; 20]);
    const SIGNER_B: Address = Address::new([0xBB; 20]);

    type TestSignerManager =
        Arc<SignerManager<RecordingProofProvider, MockRegistry, RecordingTxManager>>;
    type ProofRecords = Arc<Mutex<Vec<(Address, Vec<u8>)>>>;

    #[derive(Debug, Default)]
    enum RegistryMode {
        #[default]
        Static,
        RegisteredAfterFirstProbe(AtomicBool),
        StallIsRegistered,
        StallGetRegisteredSigners,
    }

    #[derive(Debug, Default)]
    struct MockRegistry {
        signers: Vec<Address>,
        mode: RegistryMode,
        get_registered_signers_started: Notify,
    }

    impl MockRegistry {
        fn with_registered_signers(signers: Vec<Address>) -> Self {
            Self { signers, ..Self::default() }
        }
    }

    #[async_trait]
    impl TEEProverRegistryClient for MockRegistry {
        async fn is_valid_signer(
            &self,
            signer: Address,
        ) -> std::result::Result<bool, base_proof_contracts::ContractError> {
            self.is_registered_signer(signer).await
        }

        async fn is_registered_signer(
            &self,
            signer: Address,
        ) -> std::result::Result<bool, base_proof_contracts::ContractError> {
            match &self.mode {
                RegistryMode::Static | RegistryMode::StallGetRegisteredSigners => {
                    Ok(self.signers.contains(&signer))
                }
                RegistryMode::RegisteredAfterFirstProbe(registered) => {
                    Ok(registered.swap(true, Ordering::SeqCst))
                }
                RegistryMode::StallIsRegistered => std::future::pending().await,
            }
        }

        async fn get_registered_signers(
            &self,
        ) -> std::result::Result<Vec<Address>, base_proof_contracts::ContractError> {
            self.get_registered_signers_started.notify_waiters();
            if matches!(&self.mode, RegistryMode::StallGetRegisteredSigners) {
                std::future::pending::<()>().await;
            }
            Ok(self.signers.clone())
        }
    }

    #[derive(Debug, Default)]
    struct RecordingTxManager {
        results: Mutex<VecDeque<base_tx_manager::SendResponse>>,
        sent: Mutex<Vec<(Option<Address>, Bytes)>>,
        stall_sends: bool,
        send_started: Notify,
    }

    impl RecordingTxManager {
        fn stalling() -> Self {
            Self { stall_sends: true, ..Self::default() }
        }

        fn with_errors(errors: Vec<TxManagerError>) -> Self {
            Self::with_results(errors.into_iter().map(Err).collect())
        }

        fn with_results(results: Vec<base_tx_manager::SendResponse>) -> Self {
            Self { results: Mutex::new(results.into()), ..Self::default() }
        }

        fn send_count(&self) -> usize {
            self.sent.lock().unwrap().len()
        }

        fn take_sent(&self) -> Vec<(Option<Address>, Bytes)> {
            std::mem::take(&mut *self.sent.lock().unwrap())
        }
    }

    impl TxManager for RecordingTxManager {
        async fn send(&self, candidate: TxCandidate) -> base_tx_manager::SendResponse {
            self.sent.lock().unwrap().push((candidate.to, candidate.tx_data));
            self.send_started.notify_one();
            if self.stall_sends {
                return std::future::pending::<base_tx_manager::SendResponse>().await;
            }
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(stub_receipt_with_status(true)))
        }

        async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
            unreachable!("signer manager orphan dereg tests use synchronous send")
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    #[derive(Debug, Clone, Default)]
    struct RecordingProofProvider {
        cancel_then_error: bool,
        blocked_signers: Arc<Mutex<Vec<Address>>>,
        records: ProofRecords,
    }

    #[async_trait]
    impl AttestationProofProvider for RecordingProofProvider {
        async fn generate_proof(
            &self,
            _attestation_bytes: &[u8],
            _cancel: &CancellationToken,
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            unreachable!("signer manager tests call generate_proof_for_signer")
        }

        async fn generate_proof_for_signer(
            &self,
            attestation_bytes: &[u8],
            signer_address: Address,
            cancel: &CancellationToken,
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            self.records.lock().unwrap().push((signer_address, attestation_bytes.to_vec()));
            if self.cancel_then_error {
                cancel.cancel();
                return Err(base_proof_tee_nitro_attestation_prover::ProverError::Boundless(
                    "simulated cancel race".into(),
                ));
            }
            Ok(AttestationProof {
                output: Bytes::from_static(b"stub-output"),
                proof_bytes: Bytes::from_static(b"stub-proof"),
            })
        }

        fn block_recovery_for_signer(&self, signer: Address) {
            self.blocked_signers.lock().unwrap().push(signer);
        }
    }

    fn manager_with<T>(
        proof_provider: RecordingProofProvider,
        registry: MockRegistry,
        tx_manager: T,
    ) -> SignerManager<RecordingProofProvider, MockRegistry, T> {
        SignerManager::new(
            proof_provider,
            registry,
            tx_manager,
            TEST_REGISTRY_ADDRESS,
            DEFAULT_MAX_CONCURRENCY,
            DEFAULT_MAX_TX_RETRIES,
            Duration::from_secs(DEFAULT_TX_RETRY_DELAY_SECS),
        )
    }

    fn manager(proof_provider: RecordingProofProvider) -> TestSignerManager {
        Arc::new(manager_with(
            proof_provider,
            MockRegistry::default(),
            RecordingTxManager::default(),
        ))
    }

    async fn register(
        manager: &SignerManager<RecordingProofProvider, MockRegistry, RecordingTxManager>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        manager.register_signer(TEST_PENDING_INSTANCE_ID, SIGNER_A, ATTESTATION, cancel).await
    }

    fn reconcile(
        manager: &TestSignerManager,
        resolution: &DiscoveryResolution,
        proof_tasks: &mut ProofTaskSet,
    ) {
        let cancel = CancellationToken::new();
        manager.reconcile_proof_tasks(resolution, proof_tasks, &cancel);
    }

    fn resolution_from_registerable(kept: &[(&str, &[u8; 32])]) -> DiscoveryResolution {
        let registerable = kept
            .iter()
            .map(|(ep, key)| RegisterableSigner {
                instance: healthy_prover_instance(ep),
                signer: signer_from_private_key(key),
                attestation: b"gated-attestation".to_vec(),
            })
            .collect();
        DiscoveryResolution {
            registerable,
            active_signers: HashSet::new(),
            unresolved_instance_ids: HashSet::new(),
        }
    }

    fn spawn_pending(
        proof_tasks: &mut ProofTaskSet,
        signer: Address,
        instance_id: &str,
    ) -> (tokio::task::Id, CancellationToken) {
        let cancel = CancellationToken::new();
        let handle = proof_tasks.tasks.spawn(std::future::pending::<(Address, Result<()>)>());
        let task_id = handle.id();
        proof_tasks.pending.insert(
            signer,
            PendingRegistration {
                instance_id: instance_id.to_string(),
                task_id,
                cancel: cancel.clone(),
            },
        );
        (task_id, cancel)
    }

    async fn drain_test_tasks(proof_tasks: &mut ProofTaskSet) {
        let tasks = &mut proof_tasks.tasks;
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        proof_tasks.pending.clear();
    }

    #[tokio::test]
    async fn register_signer_provider_err_after_cancel_returns_ok() {
        let manager = manager_with(
            RecordingProofProvider { cancel_then_error: true, ..Default::default() },
            MockRegistry::default(),
            RecordingTxManager::default(),
        );
        let cancel = CancellationToken::new();

        let result = register(&manager, &cancel).await;

        assert!(result.is_ok(), "provider Err after cancel must be mapped to Ok(()): {result:?}",);
        assert_eq!(
            manager.tx_manager.send_count(),
            0,
            "cancelled task must not submit a transaction"
        );
    }

    #[tokio::test]
    async fn register_signer_aborts_promptly_when_cancel_fires_during_registry_stall() {
        let manager = Arc::new(manager_with(
            RecordingProofProvider::default(),
            MockRegistry { mode: RegistryMode::StallIsRegistered, ..Default::default() },
            RecordingTxManager::default(),
        ));
        let signer_cancel = CancellationToken::new();
        let task_manager = Arc::clone(&manager);
        let task_cancel = signer_cancel.clone();
        let handle = tokio::spawn(async move { register(&task_manager, &task_cancel).await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        signer_cancel.cancel();

        let result = tokio::time::timeout(GATED_WAIT_TIMEOUT, handle)
            .await
            .expect("register_signer must not hang past the timeout")
            .expect("spawned task must not panic");

        assert!(result.is_ok(), "cancel-induced exit must be Ok(()): {result:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn register_signer_already_registered_after_error_returns_ok() {
        let manager = manager_with(
            RecordingProofProvider::default(),
            MockRegistry {
                mode: RegistryMode::RegisteredAfterFirstProbe(AtomicBool::new(false)),
                ..Default::default()
            },
            RecordingTxManager::with_errors(vec![TxManagerError::Rpc("nonce race".into())]),
        );

        let result = register(&manager, &CancellationToken::new()).await;

        assert!(result.is_ok(), "should succeed when signer is registered onchain: {result:?}");
        assert_eq!(manager.tx_manager.send_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn register_signer_tx_outcome() {
        for (errors, should_succeed, expected_sends) in [
            (vec![], true, 1),
            (
                vec![
                    TxManagerError::Rpc("transient 1".into()),
                    TxManagerError::Rpc("transient 2".into()),
                ],
                true,
                3,
            ),
            (
                (0..=DEFAULT_MAX_TX_RETRIES)
                    .map(|_| TxManagerError::Rpc("persistent failure".into()))
                    .collect(),
                false,
                (DEFAULT_MAX_TX_RETRIES + 1) as usize,
            ),
            (
                vec![TxManagerError::ExecutionReverted {
                    reason: Some("bad proof".into()),
                    data: None,
                }],
                false,
                1,
            ),
        ] {
            let manager = manager_with(
                RecordingProofProvider::default(),
                MockRegistry::default(),
                RecordingTxManager::with_errors(errors),
            );

            let result = register(&manager, &CancellationToken::new()).await;

            assert_eq!(
                result.is_ok(),
                should_succeed,
                "unexpected registration result: {result:?}",
            );
            assert_eq!(manager.tx_manager.send_count(), expected_sends);
            let sent: Vec<_> =
                manager.tx_manager.take_sent().into_iter().map(|(_, data)| data).collect();
            assert!(sent.windows(2).all(|w| w[0] == w[1]), "calldata mismatch: {sent:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn register_signer_backs_off_between_tx_retries() {
        for (case, max_retries, retry_delay, errors, expected_sends, expected_elapsed) in [
            ("exponential", 3, Duration::from_secs(1), 3, 4, Duration::from_secs(7)),
            ("capped", 4, Duration::from_secs(30), 4, 5, Duration::from_secs(210)),
        ] {
            let manager = SignerManager::new(
                RecordingProofProvider::default(),
                MockRegistry::default(),
                RecordingTxManager::with_errors(
                    (1..=errors)
                        .map(|retry| TxManagerError::Rpc(format!("transient {retry}")))
                        .collect(),
                ),
                TEST_REGISTRY_ADDRESS,
                DEFAULT_MAX_CONCURRENCY,
                max_retries,
                retry_delay,
            );
            let start = tokio::time::Instant::now();

            let result = register(&manager, &CancellationToken::new()).await;

            assert!(
                result.is_ok(),
                "{case}: retryable errors should eventually succeed: {result:?}"
            );
            assert_eq!(manager.tx_manager.send_count(), expected_sends, "{case}");
            assert_eq!(start.elapsed(), expected_elapsed, "{case}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn register_signer_reverted_receipt_blocks_recovery() {
        let proof_provider = RecordingProofProvider::default();
        let manager = manager_with(
            proof_provider.clone(),
            MockRegistry::default(),
            RecordingTxManager::with_results(vec![Ok(stub_receipt_with_status(false))]),
        );

        let result = register(&manager, &CancellationToken::new()).await;

        assert!(
            matches!(result, Err(RegistrarError::ReceiptReverted { tx_hash }) if tx_hash == B256::ZERO),
            "reverted receipt should fail with ReceiptReverted: {result:?}"
        );
        assert_eq!(manager.tx_manager.send_count(), 1, "should submit exactly one tx");
        assert_eq!(*proof_provider.blocked_signers.lock().unwrap(), vec![SIGNER_A]);
    }

    #[tokio::test(start_paused = true)]
    async fn register_signer_cancellation_during_retry_sleep_aborts() {
        let manager = manager_with(
            RecordingProofProvider::default(),
            MockRegistry::default(),
            RecordingTxManager::with_errors(vec![
                TxManagerError::Rpc("fail 1".into()),
                TxManagerError::Rpc("fail 2".into()),
                TxManagerError::Rpc("fail 3".into()),
            ]),
        );
        let cancel = CancellationToken::new();
        let cancel_handle = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            cancel_handle.cancel();
        });

        let result = register(&manager, &cancel).await;

        assert!(result.is_ok(), "cancel-induced exit should be Ok(()): {result:?}");
        assert_eq!(manager.tx_manager.send_count(), 1, "should abort during retry sleep");
    }

    #[tokio::test(start_paused = true)]
    async fn register_signer_cancellation_before_tx_sends_nothing() {
        let manager = manager_with(
            RecordingProofProvider::default(),
            MockRegistry::default(),
            RecordingTxManager::default(),
        );
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = register(&manager, &cancel).await;

        assert!(result.is_ok(), "pre-cancel should be a cooperative success: {result:?}");
        assert_eq!(
            manager.tx_manager.send_count(),
            0,
            "should not send any tx after pre-cancellation"
        );
    }

    #[tokio::test]
    async fn register_signer_releases_proof_permit_before_tx_submission_finishes() {
        let proof_provider = RecordingProofProvider::default();
        let manager = Arc::new(SignerManager::new(
            proof_provider.clone(),
            MockRegistry::default(),
            RecordingTxManager::stalling(),
            TEST_REGISTRY_ADDRESS,
            1,
            DEFAULT_MAX_TX_RETRIES,
            Duration::from_secs(DEFAULT_TX_RETRY_DELAY_SECS),
        ));
        let cancel = CancellationToken::new();

        let first_manager = Arc::clone(&manager);
        let first_cancel = cancel.clone();
        let first = tokio::spawn(async move {
            first_manager
                .register_signer(TEST_PENDING_INSTANCE_ID, SIGNER_A, ATTESTATION, &first_cancel)
                .await
        });
        let second_manager = Arc::clone(&manager);
        let second_cancel = cancel.clone();
        let second = tokio::spawn(async move {
            second_manager
                .register_signer("i-pending-test-2", SIGNER_B, ATTESTATION, &second_cancel)
                .await
        });

        tokio::time::timeout(GATED_WAIT_TIMEOUT, manager.tx_manager.send_started.notified())
            .await
            .expect("first registration never reached tx submission");
        let records = tokio::time::timeout(GATED_WAIT_TIMEOUT, async {
            loop {
                let records = proof_provider.records.lock().unwrap().clone();
                if records.len() >= 2 {
                    break records;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("second proof generation was blocked behind stalled tx submission");

        assert!(records.iter().any(|(signer, _)| *signer == SIGNER_A));
        assert!(records.iter().any(|(signer, _)| *signer == SIGNER_B));
        first.abort();
        second.abort();
        let _ = first.await;
        let _ = second.await;
    }

    #[tokio::test]
    async fn reconcile_proof_tasks_cancel_and_spawn_passes() {
        for (pre_existing, kept, expected_new_spawns, expected_cancels) in [
            (&[][..], &[(EP1, &HARDHAT_KEY_0)][..], 1, 0),
            (&[(EP1, &HARDHAT_KEY_0)][..], &[(EP1, &HARDHAT_KEY_0)][..], 0, 0),
            (&[(EP1, &HARDHAT_KEY_0)][..], &[][..], 0, 1),
        ] {
            let manager = manager(RecordingProofProvider::default());
            let mut proof_tasks = ProofTaskSet::default();
            let mut seeded_cancels = Vec::new();

            for (_, key) in pre_existing {
                let signer = signer_from_private_key(key);
                let (_, task_cancel) =
                    spawn_pending(&mut proof_tasks, signer, TEST_PENDING_INSTANCE_ID);
                seeded_cancels.push(task_cancel);
            }

            let resolution = resolution_from_registerable(kept);
            let pre_task_count = proof_tasks.tasks.len();
            let expected_pending = pre_existing.len() + expected_new_spawns;

            reconcile(&manager, &resolution, &mut proof_tasks);

            let cancelled = seeded_cancels.iter().filter(|c| c.is_cancelled()).count();
            let new_spawns = proof_tasks.tasks.len().saturating_sub(pre_task_count);

            assert_eq!(new_spawns, expected_new_spawns, "spawn-pass count");
            assert_eq!(cancelled, expected_cancels, "cancel-pass count");
            assert_eq!(proof_tasks.pending.len(), expected_pending, "pending task count");

            drain_test_tasks(&mut proof_tasks).await;
        }
    }

    #[tokio::test]
    async fn reconcile_proof_tasks_waits_for_reap_before_respawn() {
        let manager = manager(RecordingProofProvider::default());
        let mut proof_tasks = ProofTaskSet::default();
        let signer = signer_from_private_key(&HARDHAT_KEY_0);

        let (stale_task_id, stale_cancel) =
            spawn_pending(&mut proof_tasks, signer, TEST_PENDING_INSTANCE_ID);

        reconcile(&manager, &resolution_from_registerable(&[]), &mut proof_tasks);
        assert!(stale_cancel.is_cancelled(), "stale task must be cancelled by reconcile");

        reconcile(
            &manager,
            &resolution_from_registerable(&[(EP1, &HARDHAT_KEY_0)]),
            &mut proof_tasks,
        );

        assert_eq!(proof_tasks.pending.len(), 1, "still exactly one entry per signer");
        let entry = proof_tasks.pending.get(&signer).expect("pending entry keyed by signer");
        assert_eq!(entry.task_id, stale_task_id, "respawn waits for reap");

        drain_test_tasks(&mut proof_tasks).await;
    }

    #[tokio::test]
    async fn reconcile_proof_tasks_preserves_task_when_instance_fails_to_resolve() {
        let manager = manager(RecordingProofProvider::default());
        let mut proof_tasks = ProofTaskSet::default();
        let signer = signer_from_private_key(&HARDHAT_KEY_0);

        let (_, task_cancel) = spawn_pending(&mut proof_tasks, signer, TEST_PENDING_INSTANCE_ID);

        let resolution = DiscoveryResolution {
            registerable: Vec::new(),
            active_signers: HashSet::new(),
            unresolved_instance_ids: HashSet::from([TEST_PENDING_INSTANCE_ID.to_string()]),
        };

        reconcile(&manager, &resolution, &mut proof_tasks);

        assert!(
            !task_cancel.is_cancelled(),
            "task tied to an unresolved instance must be preserved",
        );
        assert_eq!(proof_tasks.pending.len(), 1, "no spurious spawn or eviction this cycle");

        let resolution_conclusive = DiscoveryResolution {
            registerable: Vec::new(),
            active_signers: HashSet::new(),
            unresolved_instance_ids: HashSet::new(),
        };
        reconcile(&manager, &resolution_conclusive, &mut proof_tasks);
        assert!(task_cancel.is_cancelled(), "conclusive absence must cancel the task");

        drain_test_tasks(&mut proof_tasks).await;
    }

    #[tokio::test]
    async fn reconcile_proof_tasks_dedupes_signer_across_registerable_entries() {
        let manager = manager(RecordingProofProvider::default());
        let signer = signer_from_private_key(&HARDHAT_KEY_0);
        let resolution = DiscoveryResolution {
            registerable: vec![
                RegisterableSigner {
                    instance: healthy_prover_instance(EP1),
                    signer,
                    attestation: b"attestation-from-instance-a".to_vec(),
                },
                RegisterableSigner {
                    instance: healthy_prover_instance(EP2),
                    signer,
                    attestation: b"attestation-from-instance-b".to_vec(),
                },
            ],
            active_signers: HashSet::new(),
            unresolved_instance_ids: HashSet::new(),
        };
        let mut proof_tasks = ProofTaskSet::default();

        reconcile(&manager, &resolution, &mut proof_tasks);

        assert_eq!(proof_tasks.pending.len(), 1, "exactly one task should spawn");
        let (&only_signer, _entry) = proof_tasks.pending.iter().next().unwrap();
        assert_eq!(only_signer, signer, "the task is keyed by the deduplicated signer");

        drain_test_tasks(&mut proof_tasks).await;
    }

    #[tokio::test]
    async fn reconcile_proof_tasks_pairs_attestation_with_signer() {
        let signer_a = signer_from_private_key(&HARDHAT_KEY_0);
        let signer_b = signer_from_private_key(&HARDHAT_KEY_1);
        assert_ne!(signer_a, signer_b, "test setup: distinct signer addresses");

        let att_a: Vec<u8> = b"attestation-aligned-to-A".to_vec();
        let att_b: Vec<u8> = b"attestation-aligned-to-B".to_vec();
        let proof_provider = RecordingProofProvider::default();
        let manager = manager(proof_provider.clone());
        let resolution = DiscoveryResolution {
            registerable: vec![
                RegisterableSigner {
                    instance: healthy_prover_instance(EP1),
                    signer: signer_a,
                    attestation: att_a.clone(),
                },
                RegisterableSigner {
                    instance: healthy_prover_instance(EP2),
                    signer: signer_b,
                    attestation: att_b.clone(),
                },
            ],
            active_signers: HashSet::new(),
            unresolved_instance_ids: HashSet::new(),
        };
        let mut proof_tasks = ProofTaskSet::default();

        reconcile(&manager, &resolution, &mut proof_tasks);

        let records = tokio::time::timeout(GATED_WAIT_TIMEOUT, async {
            loop {
                let records = proof_provider.records.lock().unwrap().clone();
                if records.len() >= 2 {
                    break records;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("timed out waiting for proof records");
        drain_test_tasks(&mut proof_tasks).await;

        assert!(records.contains(&(signer_a, att_a)), "signer A got A attestation");
        assert!(records.contains(&(signer_b, att_b)), "signer B got B attestation");
    }

    #[tokio::test]
    async fn reap_finished_tasks_drains_completed_and_evicts_pending() {
        let mut proof_tasks = ProofTaskSet::default();
        let signer = signer_from_private_key(&HARDHAT_KEY_0);

        let cancel = CancellationToken::new();
        let handle = proof_tasks.tasks.spawn(async move { (signer, Ok(())) });
        proof_tasks.pending.insert(
            signer,
            PendingRegistration {
                instance_id: TEST_PENDING_INSTANCE_ID.to_string(),
                task_id: handle.id(),
                cancel,
            },
        );

        tokio::task::yield_now().await;
        proof_tasks.reap_finished_tasks();

        assert!(proof_tasks.pending.is_empty(), "completed task must be evicted");
        assert!(proof_tasks.tasks.is_empty(), "JoinSet must drain to empty");
    }

    #[tokio::test]
    async fn reap_finished_tasks_drops_pending_entry_when_task_panics() {
        let mut proof_tasks = ProofTaskSet::default();
        let signer = signer_from_private_key(&HARDHAT_KEY_0);

        let cancel = CancellationToken::new();
        let handle = proof_tasks.tasks.spawn(async {
            panic!("synthetic proof-task panic for apply_join_outcome test");
        });
        proof_tasks.pending.insert(
            signer,
            PendingRegistration {
                instance_id: TEST_PENDING_INSTANCE_ID.to_string(),
                task_id: handle.id(),
                cancel,
            },
        );

        tokio::task::yield_now().await;
        proof_tasks.reap_finished_tasks();

        assert!(proof_tasks.pending.is_empty(), "panicked task must be evicted");
        assert!(proof_tasks.tasks.is_empty(), "JoinSet must drain to empty");
    }

    #[tokio::test]
    async fn deregister_orphans_submits_expected_signers() {
        for (registered_signers, protected_signers, expected_signers) in [
            (&[SIGNER_A, SIGNER_B][..], &[SIGNER_A, SIGNER_B][..], &[][..]),
            (&[SIGNER_A, SIGNER_B][..], &[SIGNER_A][..], &[SIGNER_B][..]),
            (&[SIGNER_A, SIGNER_B][..], &[][..], &[SIGNER_A, SIGNER_B][..]),
        ] {
            let manager = manager_with(
                RecordingProofProvider::default(),
                MockRegistry::with_registered_signers(registered_signers.to_vec()),
                RecordingTxManager::default(),
            );
            let protected_signers: HashSet<Address> = protected_signers.iter().copied().collect();

            manager.run_orphan_dereg(&protected_signers, &CancellationToken::new()).await.unwrap();

            let sent = manager.tx_manager.take_sent();
            assert_eq!(sent.len(), expected_signers.len());
            for ((to, tx_data), signer) in sent.iter().zip(expected_signers) {
                assert_eq!(*to, Some(TEST_REGISTRY_ADDRESS));
                let expected = Bytes::from(
                    ITEEProverRegistry::deregisterSignerCall { signer: *signer }.abi_encode(),
                );
                assert_eq!(tx_data, &expected);
            }
        }
    }

    #[tokio::test]
    async fn deregister_orphans_respects_cancellation() {
        let manager = manager_with(
            RecordingProofProvider::default(),
            MockRegistry::with_registered_signers(vec![SIGNER_A]),
            RecordingTxManager::default(),
        );
        let cancel = CancellationToken::new();
        cancel.cancel();

        let protected_signers = HashSet::new();

        manager.run_orphan_dereg(&protected_signers, &cancel).await.unwrap();

        assert!(manager.tx_manager.take_sent().is_empty());
    }

    #[tokio::test]
    async fn run_orphan_dereg_respects_cancellation_while_loading_signers() {
        let manager = manager_with(
            RecordingProofProvider::default(),
            MockRegistry { mode: RegistryMode::StallGetRegisteredSigners, ..Default::default() },
            RecordingTxManager::default(),
        );
        let cancel = CancellationToken::new();
        let protected_signers = HashSet::new();
        let run = manager.run_orphan_dereg(&protected_signers, &cancel);
        tokio::pin!(run);

        let notified = manager.registry.get_registered_signers_started.notified();
        tokio::pin!(notified);
        tokio::select! {
            () = &mut notified => {}
            result = &mut run => panic!("run_orphan_dereg completed before cancellation: {result:?}"),
        }

        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(1), run)
            .await
            .expect("run_orphan_dereg should stop promptly after cancellation")
            .unwrap();
        assert!(manager.tx_manager.take_sent().is_empty());
    }
}
