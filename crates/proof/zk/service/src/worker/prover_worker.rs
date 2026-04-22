use std::{fmt, sync::Arc};

use base_zk_client::ProveBlockRequest;
use base_zk_db::{CreateProofSession, ProofRequestRepo, ProofStatus, ProofType, SessionType};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::{backends::ProvingBackend, metrics};

/// Individual worker that processes a single proving task
pub struct ProverWorker {
    repo: ProofRequestRepo,
    backend: Arc<dyn ProvingBackend>,
    proof_request_id: Uuid,
    params: ProveBlockRequest,
}

impl fmt::Debug for ProverWorker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProverWorker")
            .field("proof_request_id", &self.proof_request_id)
            .field("backend", &self.backend.name())
            .finish_non_exhaustive()
    }
}

impl ProverWorker {
    /// Creates a worker bound to one proof request (`proof_request_id=<uuid>`).
    pub fn new(
        repo: ProofRequestRepo,
        backend: Arc<dyn ProvingBackend>,
        proof_request_id: Uuid,
        params: ProveBlockRequest,
    ) -> Self {
        Self { repo, backend, proof_request_id, params }
    }

    /// Run the proving task
    pub async fn run(self) -> anyhow::Result<()> {
        info!(
            proof_request_id = %self.proof_request_id,
            "Attempting to claim proving task"
        );

        // Atomically claim the task (CREATED -> PENDING)
        // This ensures idempotency - only one worker will successfully claim the task
        let claimed = self
            .repo
            .atomic_claim_task(self.proof_request_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to claim task: {e}"))?;

        if !claimed {
            info!(
                proof_request_id = %self.proof_request_id,
                "Task already claimed or processed, skipping"
            );
            return Ok(());
        }

        info!(
            proof_request_id = %self.proof_request_id,
            backend = %self.backend.name(),
            "Successfully claimed task, starting proving"
        );

        debug!(
            proof_request_id = %self.proof_request_id,
            backend = %self.backend.name(),
            "Calling backend to prove block"
        );

        let pt_label = ProofType::try_from(self.params.proof_type)
            .map_or("unknown", metrics::proof_type_label);

        // Call backend prove (backend handles temp dir creation and config)
        let result = self.backend.prove(&self.params).await;

        match result {
            Ok(prove_result) => {
                // Record witness generation duration on success
                if let Some(wg_ms) = prove_result.witness_gen_duration_ms {
                    metrics::record_witness_generation_duration(pt_label, true, wg_ms);
                }

                // Success path
                if let Some(session_id) = prove_result.session_id {
                    info!(
                        proof_request_id = %self.proof_request_id,
                        session_id = %session_id,
                        backend = %self.backend.name(),
                        "Got backend session ID for STARK proof"
                    );

                    // Atomically create proof session and update proof request to RUNNING
                    let session = CreateProofSession {
                        proof_request_id: self.proof_request_id,
                        session_type: SessionType::Stark,
                        backend_session_id: session_id.clone(),
                        metadata: prove_result.metadata,
                    };

                    if let Err(e) = self
                        .repo
                        .create_session_and_update_status(session, ProofStatus::Running)
                        .await
                    {
                        error!(
                            proof_request_id = %self.proof_request_id,
                            backend_session_id = %session_id,
                            backend = %self.backend.name(),
                            error = %e,
                            "Failed to persist session after successful prove — backend session may be orphaned"
                        );
                        return Err(anyhow::anyhow!(
                            "Failed to persist session {session_id} for request {}: {e}",
                            self.proof_request_id
                        ));
                    }

                    info!(
                        proof_request_id = %self.proof_request_id,
                        "Atomically created STARK proof session and updated status to RUNNING"
                    );
                } else {
                    info!(
                        proof_request_id = %self.proof_request_id,
                        "Proof completed without session ID (local proving)"
                    );
                }

                Ok(())
            }
            Err(e) => {
                // Failure path
                let error_msg = format!("Backend error: {e}");
                warn!(
                    proof_request_id = %self.proof_request_id,
                    backend = %self.backend.name(),
                    error = %error_msg,
                    "Backend proving failed"
                );

                // Update database status to failed
                self.repo
                    .update_status(
                        self.proof_request_id,
                        ProofStatus::Failed,
                        Some(error_msg.clone()),
                    )
                    .await?;

                // Emit proof_requests_completed for early failures (PENDING → FAILED).
                // These are never seen by the StatusPoller (which only queries RUNNING),
                // so we emit directly here.
                metrics::inc_proof_requests_completed("failed", pt_label);

                info!(
                    proof_request_id = %self.proof_request_id,
                    "Updated proof request as FAILED"
                );

                Err(anyhow::anyhow!(error_msg))
            }
        }
    }
}
