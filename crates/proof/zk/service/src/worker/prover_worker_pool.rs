use std::{fmt, sync::Arc};

use async_trait::async_trait;
use base_zk_client::ProveBlockRequest;
use base_zk_db::{ProofRequestRepo, ProofType};
use base_zk_outbox::{OutboxTask, TaskQueue};
use serde::Deserialize;
use tokio::{sync::Mutex, task::JoinHandle};
use tracing::{error, info};

use crate::{
    backends::{BackendRegistry, BackendType},
    worker::prover_worker::ProverWorker,
};

/// Intermediate struct for deserializing from database JSON.
/// The database stores `proof_type` as a string, but proto expects an integer.
#[derive(Deserialize)]
struct ProveBlockRequestParams {
    start_block_number: u64,
    number_of_blocks_to_prove: u64,
    sequence_window: Option<u64>,
    proof_type: String,
    prover_address: Option<String>,
}

impl ProveBlockRequestParams {
    /// Convert into proto `ProveBlockRequest` and the parsed [`ProofType`], consuming `self`.
    fn into_proto(self) -> anyhow::Result<(ProveBlockRequest, ProofType)> {
        let proof_type =
            ProofType::try_from(self.proof_type.as_str()).map_err(|e| anyhow::anyhow!(e))?;

        let request = ProveBlockRequest {
            start_block_number: self.start_block_number,
            number_of_blocks_to_prove: self.number_of_blocks_to_prove,
            sequence_window: self.sequence_window,
            proof_type: proof_type.proto_i32(),
            session_id: None,
            prover_address: self.prover_address,
        };

        Ok((request, proof_type))
    }
}

/// Pool that creates `ProverWorker` instances and implements `TaskQueue`.
///
/// Tracks spawned worker `JoinHandle`s for graceful shutdown support.
#[derive(Clone)]
pub struct ProverWorkerPool {
    repo: ProofRequestRepo,
    backend_registry: Arc<BackendRegistry>,
    handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl fmt::Debug for ProverWorkerPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProverWorkerPool")
            .field("handles_count", &"<locked>")
            .finish_non_exhaustive()
    }
}

impl ProverWorkerPool {
    /// Creates a new worker pool.
    pub fn new(repo: ProofRequestRepo, backend_registry: Arc<BackendRegistry>) -> Self {
        Self { repo, backend_registry, handles: Arc::new(Mutex::new(Vec::new())) }
    }

    /// Waits for all spawned workers to complete.
    ///
    /// This can be used during graceful shutdown to drain in-flight proving tasks.
    pub async fn shutdown(&self) {
        let handles: Vec<_> = {
            let mut guard = self.handles.lock().await;
            std::mem::take(&mut *guard)
        };
        for handle in handles {
            let _ = handle.await;
        }
    }
}

#[async_trait]
impl TaskQueue for ProverWorkerPool {
    async fn submit(&self, task: OutboxTask) -> anyhow::Result<()> {
        let proof_request_id = task.proof_request_id;
        let sequence_id = task.sequence_id;

        // Deserialize params from JSON (string proof_type) to intermediate struct
        let params_intermediate: ProveBlockRequestParams = serde_json::from_value(task.params)
            .map_err(|e| {
                error!(
                    proof_request_id = %proof_request_id,
                    error = %e,
                    "Failed to deserialize ProveBlockRequestParams"
                );
                anyhow::anyhow!("Failed to deserialize ProveBlockRequestParams: {e}")
            })?;

        // Convert to proto (integer proof_type) and extract the parsed ProofType
        let (params, proof_type) = params_intermediate.into_proto().map_err(|e| {
            error!(
                proof_request_id = %proof_request_id,
                error = %e,
                "Failed to convert to ProveBlockRequest"
            );
            e
        })?;
        let backend_type: BackendType = proof_type.into();

        // Get backend from registry
        let backend = self.backend_registry.get(backend_type).ok_or_else(|| {
            let error_msg = format!("Backend not found: {backend_type:?}");
            error!(
                proof_request_id = %proof_request_id,
                backend_type = ?backend_type,
                "Backend not found"
            );
            anyhow::anyhow!(error_msg)
        })?;

        info!(
            proof_request_id = %proof_request_id,
            backend = %backend.name(),
            "ProverWorkerPool: creating and spawning worker"
        );

        // Clone dependencies for the worker
        let repo = self.repo.clone();

        // Create a new ProverWorker
        let worker = ProverWorker::new(repo, backend, proof_request_id, sequence_id, params);

        // Spawn the worker task
        let handle = tokio::spawn(async move {
            let result = worker.run().await;

            // Log the result (actual task completion is tracked in database)
            match result {
                Ok(()) => {
                    info!(
                        proof_request_id = %proof_request_id,
                        "Worker completed successfully"
                    );
                }
                Err(e) => {
                    error!(
                        proof_request_id = %proof_request_id,
                        error = %e,
                        "Worker failed"
                    );
                }
            }
        });

        let mut guard = self.handles.lock().await;
        guard.retain(|h| !h.is_finished());
        guard.push(handle);

        // Return immediately - task has been successfully submitted to the worker
        Ok(())
    }
}
