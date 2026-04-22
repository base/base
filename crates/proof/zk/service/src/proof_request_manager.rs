use std::sync::Arc;

use anyhow::Result;
use base_zk_db::{ProofRequest, ProofRequestRepo, ProofStatus, ProofType, UpdateReceipt};
use tracing::warn;

use crate::{
    backends::{BackendRegistry, BackendType, ProvingBackend},
    metrics,
};

/// Coordinates proof request status transitions by delegating backend-specific logic.
///
/// When a proof request transitions from RUNNING to a terminal state
/// (SUCCEEDED or FAILED), this emits `proof_requests_completed` and
/// `proof_request_duration_ms` metrics.
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

    /// Sync proof request status by delegating to backend.
    ///
    /// Uses `_if_non_terminal` DB methods for terminal transitions so that only
    /// the first caller to transition the request actually succeeds. This
    /// prevents double-counting metrics when `StatusPoller` and `GetProof` RPC
    /// race on the same proof request.
    pub async fn sync_and_update_proof_status(&self, proof_request: &ProofRequest) -> Result<()> {
        let prev_status = proof_request.status;
        let proof_type_label = metrics::proof_type_label(proof_request.proof_type);

        // 1. Get backend for this proof type
        let backend = self.get_backend_for_proof_type(proof_request.proof_type)?;

        // 2. Let backend drive the proof request (sync sessions, create new sessions, determine
        //    status)
        let result = backend.process_proof_request(proof_request, &self.repo).await?;

        // 3. Update proof request status based on backend's result.
        //    Both terminal paths use _if_non_terminal variants so that only the
        //    first caller to transition the request actually succeeds.
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
                let was_updated = self.repo.update_receipt_if_non_terminal(update).await?;
                if !was_updated {
                    // Another caller already transitioned this request; skip metrics.
                    return Ok(());
                }
            }
            ProofStatus::Failed => {
                // Mark as failed (only if not already terminal)
                let was_updated = self
                    .repo
                    .update_status_if_non_terminal(
                        proof_request.id,
                        ProofStatus::Failed,
                        result.error_message,
                    )
                    .await?;
                if !was_updated {
                    // Another caller already transitioned this request; skip metrics.
                    return Ok(());
                }
            }
            ProofStatus::Running | ProofStatus::Pending | ProofStatus::Created => {
                // Still in progress, sessions were updated but proof_request stays RUNNING
            }
        }

        // 4. Emit terminal metrics if status transitioned from RUNNING
        if prev_status == ProofStatus::Running
            && matches!(result.status, ProofStatus::Succeeded | ProofStatus::Failed)
        {
            let status_label = match result.status {
                ProofStatus::Succeeded => "succeeded",
                ProofStatus::Failed => "failed",
                _ => unreachable!(),
            };

            metrics::inc_proof_requests_completed(status_label, proof_type_label);

            // Record wall-clock duration using DB timestamps
            if let Ok(Some(updated)) = self.repo.get(proof_request.id).await {
                if let Some(completed_at) = updated.completed_at {
                    let duration_ms =
                        (completed_at - updated.created_at).num_milliseconds().max(0) as f64;
                    metrics::record_proof_request_duration(
                        proof_type_label,
                        status_label,
                        duration_ms,
                    );
                } else {
                    warn!(
                        proof_request_id = %proof_request.id,
                        "Terminal status without completed_at timestamp"
                    );
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{
        BackendType, ProofProcessingResult, ProveResult, ProvingBackend, SessionStatus,
    };

    struct StubBackend {
        result: ProofProcessingResult,
    }

    #[async_trait::async_trait]
    impl ProvingBackend for StubBackend {
        fn backend_type(&self) -> BackendType {
            BackendType::OpSuccinct
        }
        async fn prove(
            &self,
            _request: &base_zk_client::ProveBlockRequest,
        ) -> anyhow::Result<ProveResult> {
            unimplemented!()
        }
        async fn process_proof_request(
            &self,
            _proof_request: &ProofRequest,
            _repo: &ProofRequestRepo,
        ) -> anyhow::Result<ProofProcessingResult> {
            Ok(self.result.clone())
        }
        async fn get_session_status(
            &self,
            _session: &base_zk_db::ProofSession,
        ) -> anyhow::Result<SessionStatus> {
            unimplemented!()
        }
        fn name(&self) -> &'static str {
            "stub"
        }
    }

    #[test]
    fn test_empty_registry_returns_no_backend() {
        let registry = BackendRegistry::new();
        let backend_type: BackendType = ProofType::OpSuccinctSp1ClusterCompressed.into();
        assert!(registry.get(backend_type).is_none());
    }

    #[test]
    fn test_registry_with_stub_returns_backend() {
        let mut registry = BackendRegistry::new();
        registry.register(Arc::new(StubBackend {
            result: ProofProcessingResult { status: ProofStatus::Running, error_message: None },
        }));

        let backend_type: BackendType = ProofType::OpSuccinctSp1ClusterCompressed.into();
        let backend = registry.get(backend_type);
        assert!(backend.is_some());
        assert_eq!(backend.unwrap().name(), "stub");
    }
}
