//! Signer lifecycle orchestration for the registrar.
//!
//! Coordinates signer-level registration tasks and orphaned signer cleanup
//! after the driver has resolved discovered prover instances.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
    time::Duration,
};

use alloy_primitives::Address;
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
    /// Cancellation token for graceful shutdown.
    pub cancel: CancellationToken,
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
    /// Originating instance ID, recorded only for logging.
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

    /// Returns the transaction manager used by signer lifecycle operations.
    pub const fn tx_manager(&self) -> &T {
        &self.tx_manager
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

    /// Returns whether no pending proof tasks remain.
    #[cfg(test)]
    pub fn is_pending_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Returns the pending proof-task entries keyed by signer.
    #[cfg(test)]
    pub const fn pending(&self) -> &HashMap<Address, PendingRegistration> {
        &self.pending
    }

    /// Returns mutable access to the pending proof-task entries.
    #[cfg(test)]
    pub const fn pending_mut(&mut self) -> &mut HashMap<Address, PendingRegistration> {
        &mut self.pending
    }

    /// Returns whether the underlying join set is empty.
    #[cfg(test)]
    pub fn is_join_set_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Returns mutable access to the underlying join set.
    #[cfg(test)]
    pub const fn tasks_mut(&mut self) -> &mut JoinSet<ProofTaskOutcome> {
        &mut self.tasks
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
    /// Reconciles in-flight registration tasks against fetched prover signers.
    pub fn reconcile_proof_tasks(
        self: &Arc<Self>,
        resolution: &DiscoveryResolution,
        proof_tasks: &mut ProofTaskSet,
    ) {
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
            let signer_cancel = self.config.cancel.child_token();
            let manager = Arc::clone(self);
            let instance_owned = entry.instance.clone();
            let instance_id = instance_owned.instance_id.clone();
            let attestation = entry.attestation.clone();
            let task_cancel = signer_cancel.clone();
            let signer = entry.signer;
            let enclave_index = entry.enclave_index;

            let handle = proof_tasks.tasks.spawn(async move {
                let result = manager
                    .run_proof_task(instance_owned, signer, enclave_index, attestation, task_cancel)
                    .await;
                ProofTaskOutcome { signer, result }
            });
            proof_tasks.pending.insert(
                signer,
                PendingRegistration {
                    instance_id,
                    task_id: handle.id(),
                    cancel: signer_cancel,
                    cancelled_by_reconcile: false,
                },
            );
            RegistrarMetrics::proof_tasks_spawned().increment(1);
        }
    }

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
    pub async fn run_orphan_dereg(&self, protected_signers: &HashSet<Address>) -> Result<()> {
        let deregistration_manager = DeregistrationManager::new(
            self.config.registry_address,
            &self.registry,
            &self.tx_manager,
        );

        deregistration_manager.run_orphan_dereg(protected_signers, &self.config.cancel).await
    }
}
