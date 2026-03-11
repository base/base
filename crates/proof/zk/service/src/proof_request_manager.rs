use std::sync::Arc;

use anyhow::Result;
use base_zk_db::{ProofRequest, ProofRequestRepo, ProofStatus, ProofType, UpdateReceipt};

use crate::backends::{BackendRegistry, BackendType, ProvingBackend};

/// Coordinates proof request status transitions by delegating backend-specific logic.
#[derive(Debug, Clone)]
pub struct ProofRequestManager {
    repo: ProofRequestRepo,
    backend_registry: Arc<BackendRegistry>,
}

impl ProofRequestManager {
    /// Creates a new proof request manager (`repo=<db_repo>`, `backend_registry=<registry>`).
    pub const fn new(repo: ProofRequestRepo, backend_registry: Arc<BackendRegistry>) -> Self {
        Self { repo, backend_registry }
    }

    /// Sync proof request status by delegating to backend
    pub async fn sync_and_update_proof_status(&self, proof_request: &ProofRequest) -> Result<()> {
        // 1. Get backend for this proof type
        let backend = self.get_backend_for_proof_type(proof_request.proof_type)?;

        // 2. Let backend drive the proof request (sync sessions, create new sessions, determine
        //    status)
        let result = backend.process_proof_request(proof_request, &self.repo).await?;

        // 3. Update proof request status based on backend's result
        match result.status {
            ProofStatus::Succeeded => {
                // Re-query to get updated receipts (backend updated them during processing)
                let updated_proof_request =
                    self.repo.get(proof_request.id).await?.ok_or_else(|| {
                        anyhow::anyhow!("Proof request not found after processing")
                    })?;

                // Mark as succeeded with fresh receipts
                let update = UpdateReceipt {
                    id: proof_request.id,
                    stark_receipt: updated_proof_request.stark_receipt,
                    snark_receipt: updated_proof_request.snark_receipt,
                    status: ProofStatus::Succeeded,
                    error_message: None,
                };
                self.repo.update_receipt_if_non_terminal(update).await?;
            }
            ProofStatus::Failed => {
                // Mark as failed
                self.repo
                    .update_status(proof_request.id, ProofStatus::Failed, result.error_message)
                    .await?;
            }
            ProofStatus::Running | ProofStatus::Pending | ProofStatus::Created => {
                // Still in progress, sessions were updated but proof_request stays RUNNING
            }
        }

        Ok(())
    }

    fn get_backend_for_proof_type(&self, proof_type: ProofType) -> Result<Arc<dyn ProvingBackend>> {
        let backend_type: BackendType = proof_type.into();

        self.backend_registry
            .get(backend_type)
            .ok_or_else(|| anyhow::anyhow!("Backend not found for proof type: {proof_type:?}"))
    }
}
