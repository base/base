//! Signer lifecycle orchestration for the registrar.
//!
//! Coordinates signer-level registration tasks and orphaned signer cleanup
//! after the driver has resolved discovered prover instances.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::Arc,
    time::Duration,
};

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolCall;
use base_proof_contracts::ITEEProverRegistry;
use base_proof_tee_nitro_attestation_prover::AttestationProofProvider;
use base_tx_manager::{TxCandidate, TxManager};
use tokio::{
    sync::Semaphore,
    task::{self, JoinError, JoinSet},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    DiscoveryResolution, ProofHandlerConfig, RegistrarMetrics, RegistrationManager, RegistryClient,
    Result,
};

/// Default maximum number of transaction submission retries for transient
/// errors before giving up.
pub const DEFAULT_MAX_TX_RETRIES: u32 = 3;

/// Default delay between transaction submission retries.
pub const DEFAULT_TX_RETRY_DELAY_SECS: u64 = 5;

/// Runtime settings for signer lifecycle management.
#[derive(Debug, Clone)]
pub struct SignerManagerConfig {
    /// `TEEProverRegistry` contract address on L1.
    pub registry_address: Address,
    /// Maximum number of signer registration proof tasks to run concurrently.
    pub max_concurrency: usize,
    /// Maximum number of transaction submission retries for transient errors.
    pub max_tx_retries: u32,
    /// Delay between transaction submission retries.
    pub tx_retry_delay: Duration,
}

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
    /// Whether this task was already cancelled by the reconcile pass.
    pub cancelled_by_reconcile: bool,
}

/// Coordinates signer registration and orphan signer deregistration.
#[derive(Debug)]
pub struct SignerManager<P, R, T> {
    proof_provider: P,
    registry: R,
    tx_manager: T,
    proof_semaphore: Semaphore,
    config: SignerManagerConfig,
}

impl<P, R, T> SignerManager<P, R, T> {
    /// Creates a signer manager from the signer lifecycle dependencies.
    pub fn new(proof_provider: P, registry: R, tx_manager: T, config: SignerManagerConfig) -> Self {
        let proof_semaphore = Semaphore::new(config.max_concurrency.max(1));
        Self { proof_provider, registry, tx_manager, proof_semaphore, config }
    }
}

/// Driver-owned set of in-flight proof-generation tasks.
#[derive(Debug, Default)]
pub struct ProofTaskSet {
    tasks: JoinSet<(Address, Result<()>)>,
    pending: HashMap<Address, PendingRegistration>,
}

impl ProofTaskSet {
    /// Creates an empty proof task set.
    pub fn new() -> Self {
        Self { tasks: JoinSet::new(), pending: HashMap::new() }
    }

    /// Returns the number of signers with a pending proof task.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Returns true when a signer has a pending proof task.
    pub fn has_pending_signer(&self, signer: &Address) -> bool {
        self.pending.contains_key(signer)
    }

    /// Spawns a proof task and tracks its pending registration state.
    pub fn spawn_task<F>(
        &mut self,
        signer: Address,
        instance_id: String,
        cancel: CancellationToken,
        task: F,
    ) where
        F: Future<Output = (Address, Result<()>)> + Send + 'static,
    {
        let handle = self.tasks.spawn(task);
        self.pending.insert(
            signer,
            PendingRegistration {
                instance_id,
                task_id: handle.id(),
                cancel,
                cancelled_by_reconcile: false,
            },
        );
        RegistrarMetrics::proof_tasks_spawned().increment(1);
    }

    /// Drains every task that has already finished from `tasks`.
    pub fn reap_finished_tasks(&mut self) {
        while let Some(joined) = self.tasks.try_join_next_with_id() {
            self.apply_join_outcome(joined);
        }
    }

    /// Consumes one `JoinSet` outcome and updates `pending` plus metrics.
    pub fn apply_join_outcome(
        &mut self,
        joined: std::result::Result<(task::Id, (Address, Result<()>)), JoinError>,
    ) {
        RegistrarMetrics::proof_tasks_completed().increment(1);
        match joined {
            Ok((id, (signer, result))) => {
                let removed = match self.pending.get(&signer) {
                    Some(entry) if entry.task_id == id => self.pending.remove(&signer),
                    _ => None,
                };
                let instance_id =
                    removed.as_ref().map_or("superseded", |entry| entry.instance_id.as_str());

                match result {
                    Ok(()) => {
                        debug!(
                            task_id = ?id,
                            signer = %signer,
                            instance = %instance_id,
                            pending_entry_found = removed.is_some(),
                            superseded = removed.is_none(),
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
                            superseded = removed.is_none(),
                            "proof task failed"
                        );
                        RegistrarMetrics::processing_errors_total().increment(1);
                    }
                }
            }
            Err(join_err) => {
                let id = join_err.id();
                let removed = self
                    .pending
                    .iter()
                    .find_map(|(addr, p)| (p.task_id == id).then_some(*addr))
                    .and_then(|signer| self.pending.remove(&signer).map(|entry| (signer, entry)));
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
            if !task.cancelled_by_reconcile {
                task.cancel.cancel();
                RegistrarMetrics::proof_tasks_cancelled().increment(1);
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
    R: RegistryClient,
    T: TxManager,
{
    /// Queries onchain signers and deregisters orphans.
    pub async fn run_orphan_dereg(
        &self,
        protected_signer: impl Fn(&Address) -> bool,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let registered_signers = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                debug!("cancelled before loading registered signers for orphan dereg");
                return Ok(());
            }
            res = self.registry.get_registered_signers() => res?,
        };

        let mut deregistered = 0usize;
        for signer in registered_signers.into_iter().filter(|addr| !protected_signer(addr)) {
            if cancel.is_cancelled() {
                debug!("shutdown requested, stopping orphan deregistration");
                break;
            }

            if self.deregister_orphan(signer, cancel).await {
                RegistrarMetrics::deregistrations_total().increment(1);
                deregistered += 1;
            }
        }

        if deregistered > 0 {
            info!(count = deregistered, "orphan signers deregistered");
        }

        Ok(())
    }

    /// Deregisters one orphan signer if it is still registered onchain.
    ///
    /// Checks `isRegisteredSigner` before submitting a transaction so stale
    /// `getRegisteredSigners()` entries do not loop forever.
    pub async fn deregister_orphan(&self, signer: Address, cancel: &CancellationToken) -> bool {
        let registered = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                debug!("shutdown requested while verifying orphan signer");
                return false;
            }
            res = self.registry.is_registered(signer) => res,
        };

        match registered {
            Ok(false) => {
                warn!(
                    signer = %signer,
                    "signer appears in getRegisteredSigners but isRegisteredSigner is false, \
                     skipping (possible EnumerableSet ghost entry)"
                );
                false
            }
            Err(e) => {
                warn!(
                    error = %e,
                    signer = %signer,
                    "failed to verify signer registration status, skipping deregistration"
                );
                false
            }
            Ok(true) if cancel.is_cancelled() => {
                debug!(signer = %signer, "shutdown requested before submitting orphan deregistration");
                false
            }
            Ok(true) => {
                let candidate = TxCandidate {
                    tx_data: Bytes::from(
                        ITEEProverRegistry::deregisterSignerCall { signer }.abi_encode(),
                    ),
                    to: Some(self.config.registry_address),
                    ..Default::default()
                };
                info!(
                    signer = %signer,
                    registry = %self.config.registry_address,
                    tx = ?candidate,
                    "sending deregistration tx candidate",
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
        }
    }
}

impl<P, R, T> SignerManager<P, R, T>
where
    P: AttestationProofProvider + 'static,
    R: RegistryClient + 'static,
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

        let mut live_signers = HashSet::new();

        for (signer, task) in &mut proof_tasks.pending {
            if task.cancel.is_cancelled() {
                continue;
            }

            if resolution.registerable.iter().any(|entry| entry.signer == *signer) {
                live_signers.insert(*signer);
                continue;
            }

            if resolution.unresolved_instance_ids.contains(&task.instance_id) {
                live_signers.insert(*signer);
                debug!(
                    signer = %signer,
                    instance = %task.instance_id,
                    "preserving proof task: source instance failed to resolve this cycle"
                );
            } else {
                info!(
                    signer = %signer,
                    instance = %task.instance_id,
                    "cancelling proof task: signer no longer registerable"
                );
                task.cancel.cancel();
                task.cancelled_by_reconcile = true;
                RegistrarMetrics::proof_tasks_cancelled().increment(1);
            }
        }

        for entry in &resolution.registerable {
            if !live_signers.insert(entry.signer) {
                continue;
            }
            let signer_cancel = cancel.child_token();
            let manager = Arc::clone(self);
            let instance_owned = entry.instance.clone();
            let instance_id = instance_owned.instance_id.clone();
            let attestation = entry.attestation.clone();
            let task_cancel = signer_cancel.clone();
            let signer = entry.signer;
            let enclave_index = entry.enclave_index;

            proof_tasks.spawn_task(signer, instance_id, signer_cancel, async move {
                let registration_manager = RegistrationManager::new(
                    &manager.proof_provider,
                    &manager.registry,
                    &manager.tx_manager,
                    &manager.proof_semaphore,
                    ProofHandlerConfig {
                        registry_address: manager.config.registry_address,
                        max_tx_retries: manager.config.max_tx_retries,
                        tx_retry_delay: manager.config.tx_retry_delay,
                    },
                );
                let result = registration_manager
                    .register_signer(
                        &instance_owned,
                        signer,
                        enclave_index,
                        &attestation,
                        &task_cancel,
                    )
                    .await;
                (signer, result)
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        sync::{Arc, Mutex},
        time::Duration,
    };

    use alloy_primitives::Address;
    use async_trait::async_trait;
    use base_proof_tee_nitro_attestation_prover::AttestationProof;
    use base_tx_manager::SendHandle;
    use rstest::rstest;
    use tokio::sync::{Notify, mpsc};

    use super::*;
    use crate::{
        DEFAULT_MAX_CONCURRENCY, DEFAULT_MAX_TX_RETRIES, DEFAULT_TX_RETRY_DELAY_SECS,
        RegisterableSigner, RegistrarError,
        test_utils::{
            EP1, EP2, HARDHAT_KEY_0, HARDHAT_KEY_1, NoopTxManager, TEST_REGISTRY_ADDRESS,
            healthy_prover_instance, signer_from_private_key, stub_receipt,
        },
    };

    const TEST_PENDING_INSTANCE_ID: &str = "i-pending-test";
    const GATED_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
    const REAP_POLL_INTERVAL: Duration = Duration::from_millis(1);
    const SIGNER_A: Address = Address::new([0xAA; 20]);
    const SIGNER_B: Address = Address::new([0xBB; 20]);
    const SIGNER_C: Address = Address::new([0xCC; 20]);

    type TestSignerManager =
        Arc<SignerManager<RecordingProofProvider, MockRegistry, NoopTxManager>>;

    #[derive(Debug, Default)]
    struct MockRegistry {
        signers: Vec<Address>,
        true_signers: HashSet<Address>,
    }

    impl MockRegistry {
        fn with_enumerated_and_true_signers(
            enumerated_signers: Vec<Address>,
            true_signers: Vec<Address>,
        ) -> Self {
            Self { signers: enumerated_signers, true_signers: true_signers.into_iter().collect() }
        }
    }

    #[async_trait]
    impl RegistryClient for MockRegistry {
        async fn is_registered(&self, signer: Address) -> Result<bool> {
            Ok(self.true_signers.contains(&signer))
        }

        async fn get_registered_signers(&self) -> Result<Vec<Address>> {
            Ok(self.signers.clone())
        }
    }

    #[derive(Debug, Default)]
    struct RecordingTxManager {
        sent: Mutex<Vec<(Option<Address>, Bytes)>>,
    }

    impl RecordingTxManager {
        fn take_sent(&self) -> Vec<(Option<Address>, Bytes)> {
            std::mem::take(&mut *self.sent.lock().unwrap())
        }
    }

    impl TxManager for RecordingTxManager {
        async fn send(&self, candidate: TxCandidate) -> base_tx_manager::SendResponse {
            self.sent.lock().unwrap().push((candidate.to, candidate.tx_data));
            Ok(stub_receipt())
        }

        async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
            unreachable!("signer manager orphan dereg tests use synchronous send")
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    #[derive(Debug, Default)]
    struct StallingRegistry {
        get_registered_signers_started: Notify,
    }

    #[async_trait]
    impl RegistryClient for StallingRegistry {
        async fn is_registered(&self, _signer: Address) -> Result<bool> {
            Ok(false)
        }

        async fn get_registered_signers(&self) -> Result<Vec<Address>> {
            self.get_registered_signers_started.notify_waiters();
            std::future::pending::<()>().await;
            Ok(vec![])
        }
    }

    #[derive(Debug)]
    struct CancellingRegistry {
        cancel: CancellationToken,
    }

    #[async_trait]
    impl RegistryClient for CancellingRegistry {
        async fn is_registered(&self, _signer: Address) -> Result<bool> {
            self.cancel.cancel();
            Ok(true)
        }

        async fn get_registered_signers(&self) -> Result<Vec<Address>> {
            Ok(vec![SIGNER_A])
        }
    }

    #[derive(Debug, Clone)]
    struct RecordingProofProvider {
        recorded: mpsc::UnboundedSender<(Address, Vec<u8>)>,
    }

    impl Default for RecordingProofProvider {
        fn default() -> Self {
            let (recorded, _) = mpsc::unbounded_channel();
            Self { recorded }
        }
    }

    #[async_trait]
    impl AttestationProofProvider for RecordingProofProvider {
        async fn generate_proof(
            &self,
            _attestation_bytes: &[u8],
            _cancel: &CancellationToken,
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            unreachable!("signer manager should route proof generation through signer-aware API")
        }

        async fn generate_proof_for_signer(
            &self,
            attestation_bytes: &[u8],
            signer_address: Address,
            _cancel: &CancellationToken,
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            let _ = self.recorded.send((signer_address, attestation_bytes.to_vec()));
            Err(base_proof_tee_nitro_attestation_prover::ProverError::Boundless(
                "RecordingProofProvider exits after capturing attestation".into(),
            ))
        }
    }

    fn config() -> SignerManagerConfig {
        SignerManagerConfig {
            registry_address: TEST_REGISTRY_ADDRESS,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            max_tx_retries: DEFAULT_MAX_TX_RETRIES,
            tx_retry_delay: Duration::from_secs(DEFAULT_TX_RETRY_DELAY_SECS),
        }
    }

    fn manager(proof_provider: RecordingProofProvider) -> TestSignerManager {
        Arc::new(SignerManager::new(
            proof_provider,
            MockRegistry::default(),
            NoopTxManager,
            config(),
        ))
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
        let mut registerable = Vec::new();
        let mut active_signers = HashSet::new();
        for (ep, key) in kept {
            let inst = healthy_prover_instance(ep);
            let addr = signer_from_private_key(key);
            active_signers.insert(addr);
            registerable.push(RegisterableSigner {
                instance: inst,
                signer: addr,
                attestation: b"gated-attestation".to_vec(),
                enclave_index: 0,
            });
        }
        DiscoveryResolution {
            registerable,
            active_signers,
            unresolved_instance_ids: HashSet::new(),
        }
    }

    fn spawn_pending_success_task(
        proof_tasks: &mut ProofTaskSet,
        signer: Address,
        instance_id: &str,
    ) -> (tokio::task::Id, CancellationToken) {
        let cancel = CancellationToken::new();
        let cancel_inner = cancel.clone();
        proof_tasks.spawn_task(signer, instance_id.to_string(), cancel.clone(), async move {
            cancel_inner.cancelled().await;
            (signer, Ok(()))
        });
        let task_id = proof_tasks.pending.get(&signer).expect("spawned task is pending").task_id;
        (task_id, cancel)
    }

    async fn drain_test_tasks(proof_tasks: &mut ProofTaskSet) {
        for task in proof_tasks.pending.values() {
            task.cancel.cancel();
        }
        let tasks = &mut proof_tasks.tasks;
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        proof_tasks.pending.clear();
    }

    async fn reap_until_pending_empty(proof_tasks: &mut ProofTaskSet) {
        let started = std::time::Instant::now();
        while !proof_tasks.pending.is_empty() {
            if started.elapsed() > GATED_WAIT_TIMEOUT {
                panic!("timed out reaping {} pending task(s)", proof_tasks.pending_len());
            }
            proof_tasks.reap_finished_tasks();
            tokio::time::sleep(REAP_POLL_INTERVAL).await;
        }
    }

    #[rstest]
    #[case::no_pending_spawns_all(&[], &[(EP1, &HARDHAT_KEY_0)], 1, 0)]
    #[case::pending_for_kept_spawns_nothing(&[(EP1, &HARDHAT_KEY_0)], &[(EP1, &HARDHAT_KEY_0)], 0, 0)]
    #[case::pending_for_dropped_cancels_one(&[(EP1, &HARDHAT_KEY_0)], &[], 0, 1)]
    #[tokio::test]
    async fn reconcile_proof_tasks_cancel_and_spawn_passes(
        #[case] pre_existing: &[(&'static str, &'static [u8; 32])],
        #[case] kept: &[(&'static str, &'static [u8; 32])],
        #[case] expected_new_spawns: usize,
        #[case] expected_cancels: usize,
    ) {
        let manager = manager(RecordingProofProvider::default());
        let mut proof_tasks = ProofTaskSet::new();
        let mut seeded_cancels = Vec::new();

        for (_, key) in pre_existing {
            let signer = signer_from_private_key(key);
            let (_, task_cancel) =
                spawn_pending_success_task(&mut proof_tasks, signer, TEST_PENDING_INSTANCE_ID);
            seeded_cancels.push(task_cancel);
        }

        let resolution = resolution_from_registerable(kept);
        let pre_task_count = proof_tasks.tasks.len();
        let expected_pending = pre_existing.len() + expected_new_spawns;
        let pre_cancelled = seeded_cancels.iter().filter(|c| c.is_cancelled()).count();

        reconcile(&manager, &resolution, &mut proof_tasks);

        let post_cancelled = seeded_cancels.iter().filter(|c| c.is_cancelled()).count();
        let new_spawns = proof_tasks.tasks.len().saturating_sub(pre_task_count);

        assert_eq!(new_spawns, expected_new_spawns, "spawn-pass count");
        assert_eq!(post_cancelled - pre_cancelled, expected_cancels, "cancel-pass count");
        assert_eq!(proof_tasks.pending_len(), expected_pending, "pending task count");

        drain_test_tasks(&mut proof_tasks).await;
    }

    #[tokio::test]
    async fn reconcile_proof_tasks_respawns_after_vanish_and_reappear() {
        let manager = manager(RecordingProofProvider::default());
        let mut proof_tasks = ProofTaskSet::new();
        let signer = signer_from_private_key(&HARDHAT_KEY_0);

        let (stale_task_id, stale_cancel) =
            spawn_pending_success_task(&mut proof_tasks, signer, TEST_PENDING_INSTANCE_ID);

        reconcile(&manager, &resolution_from_registerable(&[]), &mut proof_tasks);
        assert!(stale_cancel.is_cancelled(), "stale task must be cancelled by reconcile");
        assert_eq!(
            proof_tasks.pending.get(&signer).map(|p| p.task_id),
            Some(stale_task_id),
            "cancelled entry still keyed by signer until reaped",
        );

        reconcile(
            &manager,
            &resolution_from_registerable(&[(EP1, &HARDHAT_KEY_0)]),
            &mut proof_tasks,
        );

        assert_eq!(proof_tasks.pending_len(), 1, "still exactly one entry per signer");
        let fresh = proof_tasks.pending.get(&signer).expect("fresh entry keyed by signer");
        assert_ne!(fresh.task_id, stale_task_id, "fresh task_id replaces the stale one");
        assert!(!fresh.cancel.is_cancelled(), "fresh task carries a live cancel token");

        drain_test_tasks(&mut proof_tasks).await;
    }

    #[tokio::test]
    async fn reconcile_proof_tasks_preserves_task_when_instance_fails_to_resolve() {
        let manager = manager(RecordingProofProvider::default());
        let mut proof_tasks = ProofTaskSet::new();
        let signer = signer_from_private_key(&HARDHAT_KEY_0);

        let (_, task_cancel) =
            spawn_pending_success_task(&mut proof_tasks, signer, TEST_PENDING_INSTANCE_ID);

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
        assert_eq!(proof_tasks.pending_len(), 1, "no spurious spawn or eviction this cycle");

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
                    enclave_index: 0,
                },
                RegisterableSigner {
                    instance: healthy_prover_instance(EP2),
                    signer,
                    attestation: b"attestation-from-instance-b".to_vec(),
                    enclave_index: 0,
                },
            ],
            active_signers: HashSet::from([signer]),
            unresolved_instance_ids: HashSet::new(),
        };
        let mut proof_tasks = ProofTaskSet::new();

        reconcile(&manager, &resolution, &mut proof_tasks);

        assert_eq!(proof_tasks.pending_len(), 1, "exactly one task should spawn");
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
        let (recorded, mut records) = mpsc::unbounded_channel();
        let proof_provider = RecordingProofProvider { recorded };
        let manager = manager(proof_provider.clone());
        let resolution = DiscoveryResolution {
            registerable: vec![
                RegisterableSigner {
                    instance: healthy_prover_instance(EP1),
                    signer: signer_a,
                    attestation: att_a.clone(),
                    enclave_index: 0,
                },
                RegisterableSigner {
                    instance: healthy_prover_instance(EP2),
                    signer: signer_b,
                    attestation: att_b.clone(),
                    enclave_index: 0,
                },
            ],
            active_signers: HashSet::from([signer_a, signer_b]),
            unresolved_instance_ids: HashSet::new(),
        };
        let mut proof_tasks = ProofTaskSet::new();

        reconcile(&manager, &resolution, &mut proof_tasks);

        let mut snap = HashMap::new();
        for _ in 0..2 {
            let (signer, attestation) = tokio::time::timeout(GATED_WAIT_TIMEOUT, records.recv())
                .await
                .expect("timed out waiting for proof record")
                .expect("proof record channel closed");
            snap.insert(signer, attestation);
        }
        drain_test_tasks(&mut proof_tasks).await;

        assert_eq!(snap.get(&signer_a), Some(&att_a), "signer A got A attestation");
        assert_eq!(snap.get(&signer_b), Some(&att_b), "signer B got B attestation");
    }

    #[tokio::test]
    async fn reap_finished_tasks_drains_completed_and_evicts_pending() {
        let mut proof_tasks = ProofTaskSet::new();
        let signer = signer_from_private_key(&HARDHAT_KEY_0);

        proof_tasks.spawn_task(
            signer,
            TEST_PENDING_INSTANCE_ID.to_string(),
            CancellationToken::new(),
            async move { (signer, Ok(())) },
        );

        reap_until_pending_empty(&mut proof_tasks).await;

        assert!(proof_tasks.pending.is_empty(), "completed task must be evicted");
        assert!(proof_tasks.tasks.is_empty(), "JoinSet must drain to empty");
    }

    #[tokio::test]
    async fn reap_finished_tasks_leaves_in_flight_alone() {
        let mut proof_tasks = ProofTaskSet::new();
        let signer = signer_from_private_key(&HARDHAT_KEY_0);

        spawn_pending_success_task(&mut proof_tasks, signer, TEST_PENDING_INSTANCE_ID);

        proof_tasks.reap_finished_tasks();

        assert_eq!(proof_tasks.pending_len(), 1, "live task must remain pending");

        drain_test_tasks(&mut proof_tasks).await;
    }

    #[tokio::test]
    async fn apply_join_outcome_drops_pending_entry_when_task_panics() {
        let mut proof_tasks = ProofTaskSet::new();
        let signer = signer_from_private_key(&HARDHAT_KEY_0);

        proof_tasks.spawn_task(
            signer,
            TEST_PENDING_INSTANCE_ID.to_string(),
            CancellationToken::new(),
            async {
                panic!("synthetic proof-task panic for apply_join_outcome test");
            },
        );

        reap_until_pending_empty(&mut proof_tasks).await;

        assert!(proof_tasks.pending.is_empty(), "panicked task must be evicted");
        assert!(proof_tasks.tasks.is_empty(), "JoinSet must drain to empty");
    }

    #[tokio::test]
    async fn apply_join_outcome_preserves_fresh_entry_when_stale_task_fails_for_same_signer() {
        let mut proof_tasks = ProofTaskSet::new();
        let signer = signer_from_private_key(&HARDHAT_KEY_0);

        let stale_handle = proof_tasks.tasks.spawn(async move {
            (signer, Err(RegistrarError::Config("synthetic stale proof failure".to_string())))
        });
        let stale_task_id = stale_handle.id();

        let (fresh_task_id, _) =
            spawn_pending_success_task(&mut proof_tasks, signer, TEST_PENDING_INSTANCE_ID);
        assert_ne!(stale_task_id, fresh_task_id, "test setup: distinct task ids");

        let joined =
            tokio::time::timeout(GATED_WAIT_TIMEOUT, proof_tasks.tasks.join_next_with_id())
                .await
                .expect("stale task never resolved")
                .expect("JoinSet should contain stale task");
        proof_tasks.apply_join_outcome(joined);

        assert_eq!(proof_tasks.pending_len(), 1, "fresh entry must survive stale task");
        let entry = proof_tasks.pending.get(&signer).expect("fresh entry still keyed by signer");
        assert_eq!(entry.task_id, fresh_task_id, "fresh task_id preserved");
        assert!(!entry.cancel.is_cancelled(), "fresh cancel handle untouched");

        drain_test_tasks(&mut proof_tasks).await;
    }

    #[rstest]
    #[case::no_orphans(vec![SIGNER_A, SIGNER_B], vec![SIGNER_A, SIGNER_B], vec![SIGNER_A, SIGNER_B], vec![])]
    #[case::one_orphan(vec![SIGNER_A, SIGNER_B], vec![SIGNER_A, SIGNER_B], vec![SIGNER_A], vec![SIGNER_B])]
    #[case::all_orphans(vec![SIGNER_A, SIGNER_B], vec![SIGNER_A, SIGNER_B], vec![], vec![SIGNER_A, SIGNER_B])]
    #[case::one_true_signer(vec![SIGNER_A, SIGNER_B], vec![SIGNER_B], vec![], vec![SIGNER_B])]
    #[case::all_ghosts(vec![SIGNER_A, SIGNER_B, SIGNER_C], vec![], vec![], vec![])]
    #[tokio::test]
    async fn deregister_orphans_submits_expected_signers(
        #[case] enumerated_signers: Vec<Address>,
        #[case] true_signers: Vec<Address>,
        #[case] protected_signers: Vec<Address>,
        #[case] expected_signers: Vec<Address>,
    ) {
        let manager = SignerManager::new(
            RecordingProofProvider::default(),
            MockRegistry::with_enumerated_and_true_signers(enumerated_signers, true_signers),
            RecordingTxManager::default(),
            config(),
        );
        let protected_signers: HashSet<Address> = protected_signers.into_iter().collect();

        manager
            .run_orphan_dereg(
                |signer| protected_signers.contains(signer),
                &CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_sent_signers(manager.tx_manager.take_sent(), expected_signers);
    }

    #[tokio::test]
    async fn deregister_orphans_respects_cancellation() {
        let manager = SignerManager::new(
            RecordingProofProvider::default(),
            MockRegistry::with_enumerated_and_true_signers(vec![SIGNER_A], vec![SIGNER_A]),
            RecordingTxManager::default(),
            config(),
        );
        let cancel = CancellationToken::new();
        cancel.cancel();

        manager.run_orphan_dereg(|_| false, &cancel).await.unwrap();

        assert!(manager.tx_manager.take_sent().is_empty());
    }

    #[tokio::test]
    async fn run_orphan_dereg_respects_cancellation_while_loading_signers() {
        let manager = SignerManager::new(
            RecordingProofProvider::default(),
            StallingRegistry::default(),
            RecordingTxManager::default(),
            config(),
        );
        let cancel = CancellationToken::new();
        let run = manager.run_orphan_dereg(|_| false, &cancel);
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

    #[tokio::test]
    async fn run_orphan_dereg_respects_cancellation_after_verifying_signer() {
        let cancel = CancellationToken::new();
        let manager = SignerManager::new(
            RecordingProofProvider::default(),
            CancellingRegistry { cancel: cancel.clone() },
            RecordingTxManager::default(),
            config(),
        );

        manager.run_orphan_dereg(|_| false, &cancel).await.unwrap();

        assert!(manager.tx_manager.take_sent().is_empty());
    }

    fn assert_sent_signers(sent: Vec<(Option<Address>, Bytes)>, expected_signers: Vec<Address>) {
        assert_eq!(sent.len(), expected_signers.len());
        for ((to, tx_data), signer) in sent.into_iter().zip(expected_signers) {
            assert_eq!(to, Some(TEST_REGISTRY_ADDRESS));
            assert_eq!(
                tx_data,
                Bytes::from(ITEEProverRegistry::deregisterSignerCall { signer }.abi_encode())
            );
        }
    }

    #[cfg(feature = "metrics")]
    mod drain_metric_tests {
        use metrics_exporter_prometheus::PrometheusBuilder;

        use super::*;
        use crate::test_utils::HARDHAT_KEY_2;

        #[test]
        fn drain_counts_shutdown_cancelled_tasks_once() {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let recorder = PrometheusBuilder::new().build_recorder();
            let handle = recorder.handle();

            metrics::with_local_recorder(&recorder, || {
                rt.block_on(async {
                    let mut proof_tasks = ProofTaskSet::new();
                    let seed: &[(&[u8; 32], bool, bool)] = &[
                        (&HARDHAT_KEY_0, true, true),
                        (&HARDHAT_KEY_1, true, false),
                        (&HARDHAT_KEY_2, false, false),
                    ];

                    for (key, cancelled_before_drain, cancelled_by_reconcile) in seed {
                        let signer = signer_from_private_key(key);
                        let (_, cancel) = spawn_pending_success_task(
                            &mut proof_tasks,
                            signer,
                            TEST_PENDING_INSTANCE_ID,
                        );
                        if *cancelled_before_drain {
                            cancel.cancel();
                        }
                        proof_tasks.pending.get_mut(&signer).unwrap().cancelled_by_reconcile =
                            *cancelled_by_reconcile;
                    }

                    proof_tasks.drain_proof_tasks().await;
                });
            });

            let rendered = handle.render();
            assert!(
                rendered.contains("base_registrar_proof_tasks_cancelled 2"),
                "drain must count shutdown and live task cancellations once each. Got:\n{rendered}",
            );
        }
    }
}
