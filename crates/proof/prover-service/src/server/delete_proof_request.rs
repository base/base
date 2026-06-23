use base_prover_service_db::{DeleteProofRequestOutcome, canonical_session_id};
use base_prover_service_protocol::{DeleteProofRequest, DeleteProofResponse};
use jsonrpsee::core::RpcResult;
use tracing::info;

use crate::server::{
    ProverServiceServer, failed_precondition, internal, invalid_argument, not_found,
    record_rpc_result,
};

impl ProverServiceServer {
    /// Deletes a completed proof request so the same session id can be retried.
    pub async fn delete_proof_request_impl(
        &self,
        request: DeleteProofRequest,
    ) -> RpcResult<DeleteProofResponse> {
        let start = std::time::Instant::now();
        let result = self.delete_proof_request_inner(request).await;
        record_rpc_result("DeleteProofRequest", start, &result);

        result
    }

    async fn delete_proof_request_inner(
        &self,
        request: DeleteProofRequest,
    ) -> RpcResult<DeleteProofResponse> {
        let session_id = canonical_session_id(&request.session_id)
            .map_err(|e| invalid_argument(format!("{e}")))?;
        match self.repo.delete_proof_request_by_session_id(&session_id).await {
            Ok(DeleteProofRequestOutcome::Deleted) => {
                info!(session_id = %session_id, "Deleted terminal proof request");
                Ok(DeleteProofResponse { deleted: true })
            }
            Ok(DeleteProofRequestOutcome::NotFound) => {
                Err(not_found(format!("session_id {session_id}: proof request not found")))
            }
            Ok(DeleteProofRequestOutcome::NotCompleted(status)) => {
                Err(failed_precondition(format!(
                    "session_id {session_id}: proof request status is {}, expected SUCCEEDED or FAILED",
                    status.as_str()
                )))
            }
            Err(e) => Err(internal(format!("Database error: {e}"))),
        }
    }
}
