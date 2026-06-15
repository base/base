//! Signer lifecycle orchestration for the registrar.
//!
//! Coordinates signer-level registration tasks and orphaned signer cleanup
//! after the driver has resolved discovered prover instances.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    future::Future,
    sync::Arc,
    time::Duration,
};

use alloy_primitives::Address;
use async_trait::async_trait;
use base_proof_tee_nitro_attestation_prover::AttestationProofProvider;
use base_tx_manager::TxManager;
use tokio::{
    sync::Semaphore,
    task::{self, JoinError, JoinSet},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    DeregistrationManager, DiscoveryResolution, ProofHandlerConfig, ProverInstance,
    RegistrarMetrics, RegistrationManager, RegistryClient, Result,
};

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

/// Terminal outcome returned by a spawned signer proof task.
#[derive(Debug)]
pub struct ProofTaskOutcome {
    /// Signer address the task attempted to register.
    pub signer: Address,
    /// Result of the signer registration attempt.
    pub result: Result<()>,
}

/// Coordinates signer registration and orphan signer deregistration.
pub struct SignerManager<P, R, T> {
    proof_provider: P,
    registry: R,
    tx_manager: T,
    proof_semaphore: Semaphore,
    config: SignerManagerConfig,
}

impl<P, R, T> fmt::Debug for SignerManager<P, R, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignerManager").field("config", &self.config).finish_non_exhaustive()
    }
}

impl<P, R, T> SignerManager<P, R, T> {
    /// Creates a signer manager from the signer lifecycle dependencies.
    pub fn new(proof_provider: P, registry: R, tx_manager: T, config: SignerManagerConfig) -> Self {
        let proof_semaphore = Semaphore::new(config.max_concurrency.max(1));
        Self { proof_provider, registry, tx_manager, proof_semaphore, config }
    }
}

/// Driver-facing signer lifecycle boundary.
///
/// The production implementation is [`SignerManager`], but the driver only
/// needs to reconcile proof tasks and run orphan cleanup. Keeping this as a
/// trait lets driver tests mock signer lifecycle behavior without standing up
/// proof tasks or registry state.
#[async_trait]
pub trait SignerLifecycle: Send + Sync + fmt::Debug {
    /// Reconciles in-flight registration tasks against fetched prover signers.
    ///
    /// New proof-task child tokens are derived from `cancel`, which is owned
    /// by the driver so every signer lifecycle operation shares one shutdown
    /// source of truth.
    fn reconcile_proof_tasks(
        &self,
        resolution: &DiscoveryResolution,
        proof_tasks: &mut ProofTaskSet,
        cancel: &CancellationToken,
    );

    /// Queries onchain signers and deregisters orphans.
    ///
    /// The caller-provided cancellation token controls every registry read and
    /// transaction submission attempted by the deregistration pass.
    async fn run_orphan_dereg(
        &self,
        protected_signers: &HashSet<Address>,
        cancel: &CancellationToken,
    ) -> Result<()>;
}

#[async_trait]
impl<P, R, T> SignerLifecycle for Arc<SignerManager<P, R, T>>
where
    P: AttestationProofProvider + 'static,
    R: RegistryClient + 'static,
    T: TxManager + 'static,
{
    fn reconcile_proof_tasks(
        &self,
        resolution: &DiscoveryResolution,
        proof_tasks: &mut ProofTaskSet,
        cancel: &CancellationToken,
    ) {
        if cancel.is_cancelled() {
            return;
        }

        let wanted: HashSet<Address> = resolution.registerable.iter().map(|e| e.signer).collect();
        let mut live_signers = HashSet::new();

        for (signer, task) in &mut proof_tasks.pending {
            if task.cancel.is_cancelled() {
                continue;
            }

            if wanted.contains(signer) {
                live_signers.insert(*signer);
                continue;
            }

            if resolution.unresolved_instance_ids.contains(&task.instance_id) {
                live_signers.insert(*signer);
                debug!(
                    signer = %signer,
                    instance = %task.instance_id,
                    "preserving proof task: source instance failed to resolve this cycle (inconclusive)"
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
            let manager = Self::clone(self);
            let instance_owned = entry.instance.clone();
            let instance_id = instance_owned.instance_id.clone();
            let attestation = entry.attestation.clone();
            let task_cancel = signer_cancel.clone();
            let signer = entry.signer;
            let enclave_index = entry.enclave_index;

            proof_tasks.spawn_task(signer, instance_id, signer_cancel, async move {
                let result = manager
                    .run_proof_task(instance_owned, signer, enclave_index, attestation, task_cancel)
                    .await;
                ProofTaskOutcome { signer, result }
            });
        }
    }

    async fn run_orphan_dereg(
        &self,
        protected_signers: &HashSet<Address>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.as_ref().run_orphan_dereg(protected_signers, cancel).await
    }
}

/// Driver-owned set of in-flight proof-generation tasks.
pub struct ProofTaskSet {
    tasks: JoinSet<ProofTaskOutcome>,
    pending: HashMap<Address, PendingRegistration>,
}

impl fmt::Debug for ProofTaskSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProofTaskSet")
            .field("tasks", &self.tasks.len())
            .field("pending", &self.pending.len())
            .finish()
    }
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

    /// Builds the protected-signer set for orphan deregistration.
    ///
    /// Includes both fetched prover signers and pending proof tasks so a signer
    /// that registers mid-pass is not immediately deregistered.
    pub fn protected_signers(&self, resolution: &DiscoveryResolution) -> HashSet<Address> {
        let mut protected = resolution.active_signers.clone();
        protected.extend(self.pending.keys().copied());
        protected
    }

    /// Spawns a proof task and tracks its pending registration state.
    pub fn spawn_task<F>(
        &mut self,
        signer: Address,
        instance_id: String,
        cancel: CancellationToken,
        task: F,
    ) where
        F: Future<Output = ProofTaskOutcome> + Send + 'static,
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
        joined: std::result::Result<(task::Id, ProofTaskOutcome), JoinError>,
    ) {
        RegistrarMetrics::proof_tasks_completed().increment(1);
        match joined {
            Ok((id, outcome)) => {
                let removed = match self.pending.get(&outcome.signer) {
                    Some(entry) if entry.task_id == id => self.pending.remove(&outcome.signer),
                    _ => None,
                };
                let instance_id =
                    removed.as_ref().map_or("superseded", |entry| entry.instance_id.as_str());

                match outcome.result {
                    Ok(()) => {
                        debug!(
                            task_id = ?id,
                            signer = %outcome.signer,
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
                            signer = %outcome.signer,
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
        for task in self.pending.values() {
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

impl Default for ProofTaskSet {
    fn default() -> Self {
        Self::new()
    }
}

impl<P, R, T> SignerManager<P, R, T>
where
    P: AttestationProofProvider + 'static,
    R: RegistryClient + 'static,
    T: TxManager + 'static,
{
    /// Runs a signer registration through [`RegistrationManager`].
    pub async fn run_proof_task(
        self: Arc<Self>,
        instance: ProverInstance,
        signer: Address,
        enclave_index: usize,
        attestation_bytes: Vec<u8>,
        signer_cancel: CancellationToken,
    ) -> Result<()> {
        let registration_manager = RegistrationManager::new(
            &self.proof_provider,
            &self.registry,
            &self.tx_manager,
            &self.proof_semaphore,
            ProofHandlerConfig {
                registry_address: self.config.registry_address,
                max_tx_retries: self.config.max_tx_retries,
                tx_retry_delay: self.config.tx_retry_delay,
            },
        );
        registration_manager
            .register_signer(&instance, signer, enclave_index, &attestation_bytes, &signer_cancel)
            .await
    }

    /// Queries onchain signers and deregisters orphans.
    pub async fn run_orphan_dereg(
        &self,
        protected_signers: &HashSet<Address>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let deregistration_manager = DeregistrationManager::new(
            self.config.registry_address,
            &self.registry,
            &self.tx_manager,
        );

        deregistration_manager.run_orphan_dereg(protected_signers, cancel).await
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
    use rstest::rstest;

    use super::*;
    use crate::{
        DEFAULT_MAX_CONCURRENCY, DEFAULT_MAX_TX_RETRIES, DEFAULT_TX_RETRY_DELAY_SECS,
        RegisterableSigner, RegistrarError,
        test_utils::{
            EP1, EP2, HARDHAT_KEY_0, HARDHAT_KEY_1, NoopTxManager, TEST_REGISTRY_ADDRESS,
            healthy_prover_instance, signer_from_private_key,
        },
    };

    const TEST_PENDING_INSTANCE_ID: &str = "i-pending-test";
    const GATED_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
    const REAP_POLL_INTERVAL: Duration = Duration::from_millis(1);

    type TestSignerManager =
        Arc<SignerManager<RecordingProofProvider, MockRegistry, NoopTxManager>>;

    #[derive(Debug)]
    struct MockRegistry;

    #[async_trait]
    impl RegistryClient for MockRegistry {
        async fn is_registered(&self, _signer: Address) -> Result<bool> {
            Ok(false)
        }

        async fn get_registered_signers(&self) -> Result<Vec<Address>> {
            unreachable!("signer manager unit tests do not run orphan deregistration")
        }
    }

    #[derive(Debug, Clone, Default)]
    struct RecordingProofProvider {
        recorded: Arc<Mutex<HashMap<Address, Vec<u8>>>>,
    }

    impl RecordingProofProvider {
        fn snapshot(&self) -> HashMap<Address, Vec<u8>> {
            self.recorded.lock().unwrap().clone()
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
            self.recorded.lock().unwrap().insert(signer_address, attestation_bytes.to_vec());
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
        Arc::new(SignerManager::new(proof_provider, MockRegistry, NoopTxManager, config()))
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
        let total = kept.len();
        DiscoveryResolution {
            registerable,
            active_signers,
            reachable_count: total,
            total_count: total,
            ok_to_dereg: true,
            unresolved_instance_ids: HashSet::new(),
        }
    }

    fn spawn_pending_success_task(
        proof_tasks: &mut ProofTaskSet,
        signer: Address,
        instance_id: &str,
        cancelled_by_reconcile: bool,
    ) -> (tokio::task::Id, CancellationToken) {
        let cancel = CancellationToken::new();
        let cancel_inner = cancel.clone();
        proof_tasks.spawn_task(signer, instance_id.to_string(), cancel.clone(), async move {
            cancel_inner.cancelled().await;
            ProofTaskOutcome { signer, result: Ok(()) }
        });
        let entry = proof_tasks.pending.get_mut(&signer).expect("spawned task is pending");
        entry.cancelled_by_reconcile = cancelled_by_reconcile;
        let task_id = entry.task_id;
        (task_id, cancel)
    }

    async fn wait_for(label: &str, predicate: impl Fn() -> bool) {
        let started = std::time::Instant::now();
        while !predicate() {
            if started.elapsed() > GATED_WAIT_TIMEOUT {
                panic!("timed out waiting for: {label}");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
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
            let (_, task_cancel) = spawn_pending_success_task(
                &mut proof_tasks,
                signer,
                TEST_PENDING_INSTANCE_ID,
                false,
            );
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
            spawn_pending_success_task(&mut proof_tasks, signer, TEST_PENDING_INSTANCE_ID, false);

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
            spawn_pending_success_task(&mut proof_tasks, signer, TEST_PENDING_INSTANCE_ID, false);

        let resolution = DiscoveryResolution {
            registerable: Vec::new(),
            active_signers: HashSet::new(),
            reachable_count: 0,
            total_count: 1,
            ok_to_dereg: false,
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
            reachable_count: 1,
            total_count: 1,
            ok_to_dereg: true,
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
            reachable_count: 2,
            total_count: 2,
            ok_to_dereg: false,
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
        let proof_provider = RecordingProofProvider::default();
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
            reachable_count: 2,
            total_count: 2,
            ok_to_dereg: false,
            unresolved_instance_ids: HashSet::new(),
        };
        let mut proof_tasks = ProofTaskSet::new();

        reconcile(&manager, &resolution, &mut proof_tasks);

        wait_for("both signers recorded their attestations", || {
            proof_provider.snapshot().len() == 2
        })
        .await;
        drain_test_tasks(&mut proof_tasks).await;

        let snap = proof_provider.snapshot();
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
            async move { ProofTaskOutcome { signer, result: Ok(()) } },
        );

        reap_until_pending_empty(&mut proof_tasks).await;

        assert!(proof_tasks.pending.is_empty(), "completed task must be evicted");
        assert!(proof_tasks.tasks.is_empty(), "JoinSet must drain to empty");
    }

    #[tokio::test]
    async fn reap_finished_tasks_leaves_in_flight_alone() {
        let mut proof_tasks = ProofTaskSet::new();
        let signer = signer_from_private_key(&HARDHAT_KEY_0);

        spawn_pending_success_task(&mut proof_tasks, signer, TEST_PENDING_INSTANCE_ID, false);

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
            ProofTaskOutcome {
                signer,
                result: Err(RegistrarError::Config("synthetic stale proof failure".to_string())),
            }
        });
        let stale_task_id = stale_handle.id();

        let (fresh_task_id, _) =
            spawn_pending_success_task(&mut proof_tasks, signer, TEST_PENDING_INSTANCE_ID, false);
        assert_ne!(stale_task_id, fresh_task_id, "test setup: distinct task ids");

        let started = std::time::Instant::now();
        loop {
            if let Some(joined) = proof_tasks.tasks.try_join_next_with_id() {
                proof_tasks.apply_join_outcome(joined);
                break;
            }
            if started.elapsed() > GATED_WAIT_TIMEOUT {
                panic!("stale task never resolved");
            }
            tokio::time::sleep(REAP_POLL_INTERVAL).await;
        }

        assert_eq!(proof_tasks.pending_len(), 1, "fresh entry must survive stale task");
        let entry = proof_tasks.pending.get(&signer).expect("fresh entry still keyed by signer");
        assert_eq!(entry.task_id, fresh_task_id, "fresh task_id preserved");
        assert!(!entry.cancel.is_cancelled(), "fresh cancel handle untouched");

        drain_test_tasks(&mut proof_tasks).await;
    }

    #[tokio::test]
    async fn protected_signers_unions_active_and_pending_signers() {
        let active_signer = signer_from_private_key(&HARDHAT_KEY_0);
        let pending_signer = signer_from_private_key(&HARDHAT_KEY_1);
        assert_ne!(active_signer, pending_signer, "test setup: distinct signers");

        let mut proof_tasks = ProofTaskSet::new();
        spawn_pending_success_task(
            &mut proof_tasks,
            pending_signer,
            TEST_PENDING_INSTANCE_ID,
            false,
        );

        let resolution = DiscoveryResolution {
            registerable: Vec::new(),
            active_signers: HashSet::from([active_signer]),
            reachable_count: 1,
            total_count: 1,
            ok_to_dereg: true,
            unresolved_instance_ids: HashSet::from([TEST_PENDING_INSTANCE_ID.to_string()]),
        };
        let protected = proof_tasks.protected_signers(&resolution);
        assert_eq!(protected, HashSet::from([active_signer, pending_signer]));

        drain_test_tasks(&mut proof_tasks).await;

        let protected_after_drain = proof_tasks.protected_signers(&resolution);
        assert_eq!(protected_after_drain, HashSet::from([active_signer]));
    }

    #[cfg(feature = "metrics")]
    mod drain_metric_tests {
        use metrics_exporter_prometheus::PrometheusBuilder;

        use super::*;
        use crate::test_utils::HARDHAT_KEY_2;

        #[test]
        fn drain_counts_only_tasks_not_already_cancelled_by_reconcile() {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let recorder = PrometheusBuilder::new().build_recorder();
            let handle = recorder.handle();

            metrics::with_local_recorder(&recorder, || {
                rt.block_on(async {
                    let mut proof_tasks = ProofTaskSet::new();
                    let seed: &[(&[u8; 32], bool)] =
                        &[(&HARDHAT_KEY_0, true), (&HARDHAT_KEY_1, false), (&HARDHAT_KEY_2, false)];

                    for (key, flagged) in seed {
                        let signer = signer_from_private_key(key);
                        let (_, cancel) = spawn_pending_success_task(
                            &mut proof_tasks,
                            signer,
                            TEST_PENDING_INSTANCE_ID,
                            *flagged,
                        );
                        if *flagged {
                            cancel.cancel();
                        }
                    }

                    proof_tasks.drain_proof_tasks().await;
                });
            });

            let rendered = handle.render();
            assert!(
                rendered.contains("base_registrar_proof_tasks_cancelled 2"),
                "drain must count only the unflagged tasks once each. Got:\n{rendered}",
            );
        }
    }
}
