//! Signer lifecycle orchestration for the registrar.
//!
//! Coordinates signer-level registration tasks and orphaned signer cleanup
//! after the driver has resolved discovered prover instances.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::Address;
use base_proof_tee_nitro_attestation_prover::AttestationProofProvider;
use base_tx_manager::TxManager;
use tokio::{
    sync::Semaphore,
    task::{self, JoinSet},
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

/// Coordinates signer registration and orphan signer deregistration.
pub struct SignerManager<P, R, T> {
    proof_provider: P,
    registry: R,
    tx_manager: T,
    proof_semaphore: Arc<Semaphore>,
    in_flight_registrations: Arc<Mutex<HashSet<Address>>>,
    signer_history: Arc<Mutex<HashMap<Address, String>>>,
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
        let proof_semaphore = Arc::new(Semaphore::new(config.max_concurrency.max(1)));
        Self {
            proof_provider,
            registry,
            tx_manager,
            proof_semaphore,
            in_flight_registrations: Arc::new(Mutex::new(HashSet::new())),
            signer_history: Arc::new(Mutex::new(HashMap::new())),
            config,
        }
    }

    /// Returns the transaction manager used by signer lifecycle operations.
    pub const fn tx_manager(&self) -> &T {
        &self.tx_manager
    }

    /// Records last-known instance attribution for signer addresses.
    pub fn record_signers(&self, addresses: &[Address], instance_id: &str) {
        let mut history = self.signer_history.lock().unwrap_or_else(|e| e.into_inner());
        for addr in addresses {
            history.insert(*addr, instance_id.to_string());
        }
    }

    /// Builds the protected-signer set for orphan deregistration.
    ///
    /// Includes both fetched prover signers and pending proof tasks so a signer
    /// that registers mid-pass is not immediately deregistered.
    pub fn protected_signers(
        resolution: &DiscoveryResolution,
        pending: &HashMap<Address, PendingRegistration>,
    ) -> HashSet<Address> {
        let mut protected = resolution.active_signers.clone();
        protected.extend(pending.keys().copied());
        protected
    }

    /// Drains every task that has already finished from `tasks`.
    pub fn reap_finished_tasks(
        tasks: &mut JoinSet<Result<Address>>,
        pending: &mut HashMap<Address, PendingRegistration>,
    ) {
        while let Some(joined) = tasks.try_join_next_with_id() {
            Self::apply_join_outcome(Some(joined), pending);
        }
    }

    /// Removes the `pending` entry for `signer` only when its task id matches.
    pub fn remove_if_task_matches(
        pending: &mut HashMap<Address, PendingRegistration>,
        signer: Address,
        id: task::Id,
    ) -> Option<PendingRegistration> {
        match pending.get(&signer) {
            Some(entry) if entry.task_id == id => pending.remove(&signer),
            _ => None,
        }
    }

    /// Removes the `pending` entry spawned with `task_id`.
    pub fn remove_by_task_id(
        pending: &mut HashMap<Address, PendingRegistration>,
        task_id: task::Id,
    ) -> Option<(Address, PendingRegistration)> {
        let signer =
            pending.iter().find_map(|(addr, p)| (p.task_id == task_id).then_some(*addr))?;
        let registration = pending.remove(&signer)?;
        Some((signer, registration))
    }

    /// Consumes one `JoinSet` outcome and updates `pending` plus metrics.
    pub fn apply_join_outcome(
        joined: Option<std::result::Result<(task::Id, Result<Address>), tokio::task::JoinError>>,
        pending: &mut HashMap<Address, PendingRegistration>,
    ) {
        let Some(result) = joined else { return };
        RegistrarMetrics::proof_tasks_completed().increment(1);
        match result {
            Ok((id, Ok(signer))) => {
                let removed = Self::remove_if_task_matches(pending, signer, id);
                debug!(
                    task_id = ?id,
                    signer = %signer,
                    instance = ?removed.as_ref().map(|t| t.instance_id.as_str()),
                    superseded = removed.is_none(),
                    "proof task completed",
                );
            }
            Ok((id, Err(e))) => {
                let removed = Self::remove_by_task_id(pending, id);
                let signer = removed.as_ref().map(|(signer, _)| *signer);
                warn!(
                    task_id = ?id,
                    error = %e,
                    signer = ?signer,
                    instance = ?removed.as_ref().map(|(_, t)| t.instance_id.as_str()),
                    pending_entry_found = removed.is_some(),
                    "proof task failed"
                );
                RegistrarMetrics::processing_errors_total().increment(1);
            }
            Err(join_err) => {
                let id = join_err.id();
                let removed = Self::remove_by_task_id(pending, id);
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
    pub async fn drain_proof_tasks(
        tasks: &mut JoinSet<Result<Address>>,
        pending: &mut HashMap<Address, PendingRegistration>,
    ) {
        for task in pending.values() {
            if !task.cancelled_by_reconcile {
                task.cancel.cancel();
                RegistrarMetrics::proof_tasks_cancelled().increment(1);
            }
        }
        while let Some(joined) = tasks.join_next_with_id().await {
            Self::apply_join_outcome(Some(joined), pending);
        }
        RegistrarMetrics::proof_tasks_pending().set(0.0);
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
        tasks: &mut JoinSet<Result<Address>>,
        pending: &mut HashMap<Address, PendingRegistration>,
    ) {
        let wanted: HashSet<Address> = resolution.registerable.iter().map(|e| e.signer).collect();

        for (signer, task) in pending.iter_mut() {
            if wanted.contains(signer) || task.cancel.is_cancelled() {
                continue;
            }
            if resolution.unresolved_instance_ids.contains(&task.instance_id) {
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

        let mut in_flight: HashSet<Address> = pending
            .iter()
            .filter(|(_, t)| !t.cancel.is_cancelled())
            .map(|(addr, _)| *addr)
            .collect();

        for entry in &resolution.registerable {
            if !in_flight.insert(entry.signer) {
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

            let handle = tasks.spawn(async move {
                manager
                    .run_proof_task(instance_owned, signer, enclave_index, attestation, task_cancel)
                    .await
            });
            pending.insert(
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
    ) -> Result<Address> {
        let registration_manager = RegistrationManager::new(
            &self.proof_provider,
            &self.registry,
            &self.tx_manager,
            &self.proof_semaphore,
            &self.in_flight_registrations,
            ProofHandlerConfig {
                registry_address: self.config.registry_address,
                max_tx_retries: self.config.max_tx_retries,
                tx_retry_delay: self.config.tx_retry_delay,
            },
        );
        registration_manager
            .register_signer(&instance, signer, enclave_index, &attestation_bytes, &signer_cancel)
            .await?;
        Ok(signer)
    }

    /// Queries onchain signers and deregisters orphans.
    pub async fn run_orphan_dereg(&self, protected_signers: &HashSet<Address>) -> Result<()> {
        let deregistration_manager = DeregistrationManager::new(
            self.config.registry_address,
            &self.registry,
            &self.tx_manager,
            &self.signer_history,
        );

        deregistration_manager.run_orphan_dereg(protected_signers, &self.config.cancel).await
    }
}
