use base_prover_service_db::{CancelProofRequestOutcome, canonical_session_id};
use base_prover_service_protocol::{CancelProofRequest, PROOF_REQUEST_NOT_FOUND_MESSAGE};
use jsonrpsee::core::RpcResult;
use tracing::info;

use crate::{
    metrics,
    server::{
        ProverServiceServer, failed_precondition, internal, invalid_argument, not_found,
        record_rpc_result,
    },
};

impl ProverServiceServer {
    /// Cancels a queued or running Cluster or Network proof request.
    pub async fn cancel_proof_request_impl(&self, request: CancelProofRequest) -> RpcResult<()> {
        let start = std::time::Instant::now();
        let result = self.cancel_proof_request_inner(request).await;
        record_rpc_result("CancelProofRequest", start, &result);

        result
    }

    async fn cancel_proof_request_inner(&self, request: CancelProofRequest) -> RpcResult<()> {
        let session_id = canonical_session_id(&request.session_id)
            .map_err(|e| invalid_argument(format!("{e}")))?;
        match self.repo.cancel_proof_request_by_session_id(&session_id).await {
            Ok(CancelProofRequestOutcome::Cancelled(job)) => {
                metrics::record_terminal_proof_job(metrics::PROOF_STATUS_FAILED, &job);
                info!(session_id = %session_id, "Cancelled proof request");
                Ok(())
            }
            Ok(CancelProofRequestOutcome::AlreadyCancelled) => Ok(()),
            Ok(CancelProofRequestOutcome::NotFound) => {
                Err(not_found(PROOF_REQUEST_NOT_FOUND_MESSAGE))
            }
            Ok(CancelProofRequestOutcome::UnsupportedBackend) => {
                Err(failed_precondition("cancellation is not available for this proof backend"))
            }
            Ok(CancelProofRequestOutcome::AlreadyTerminal(status)) => {
                Err(failed_precondition(format!(
                    "session_id {session_id}: proof request status is {}, expected a queued or running request",
                    status.as_str()
                )))
            }
            Err(e) => Err(internal(format!("Database error: {e}"))),
        }
    }
}
