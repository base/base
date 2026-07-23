use base_prover_service_db::{DeleteProofRequestOutcome, canonical_session_id};
use base_prover_service_protocol::{
    DeleteProofRequest, DeleteProofsByTeeSignerRequest, DeleteProofsByTeeSignerResponse,
};
use jsonrpsee::core::RpcResult;
use tracing::info;

use crate::server::{
    ProverServiceServer, failed_precondition, internal, invalid_argument, record_rpc_result,
};

impl ProverServiceServer {
    /// Deletes a completed proof request so the same session id can be retried.
    pub async fn delete_proof_request_impl(&self, request: DeleteProofRequest) -> RpcResult<()> {
        let start = std::time::Instant::now();
        let result = self.delete_proof_request_inner(request).await;
        record_rpc_result("DeleteProofRequest", start, &result);

        result
    }

    async fn delete_proof_request_inner(&self, request: DeleteProofRequest) -> RpcResult<()> {
        let session_id = canonical_session_id(&request.session_id)
            .map_err(|e| invalid_argument(format!("{e}")))?;
        match self.repo.delete_proof_request_by_session_id(&session_id).await {
            Ok(DeleteProofRequestOutcome::Deleted) => {
                info!(session_id = %session_id, "Deleted terminal proof request");
                Ok(())
            }
            Ok(DeleteProofRequestOutcome::NotFound) => Ok(()),
            Ok(DeleteProofRequestOutcome::NotCompleted(status)) => {
                Err(failed_precondition(format!(
                    "session_id {session_id}: proof request status is {}, expected SUCCEEDED or FAILED",
                    status.as_str()
                )))
            }
            Err(e) => Err(internal(format!("Database error: {e}"))),
        }
    }

    /// Deletes completed TEE proof requests produced by one signer.
    pub async fn delete_proofs_by_tee_signer_impl(
        &self,
        request: DeleteProofsByTeeSignerRequest,
    ) -> RpcResult<DeleteProofsByTeeSignerResponse> {
        let start = std::time::Instant::now();
        let result = self.delete_proofs_by_tee_signer_inner(request).await;
        record_rpc_result("DeleteProofsByTeeSigner", start, &result);
        result
    }

    async fn delete_proofs_by_tee_signer_inner(
        &self,
        request: DeleteProofsByTeeSignerRequest,
    ) -> RpcResult<DeleteProofsByTeeSignerResponse> {
        if request.tee_signer.is_zero() {
            return Err(invalid_argument("tee_signer must not be zero"));
        }
        let tee_signer = format!("{:#x}", request.tee_signer);
        let deleted_count = self
            .repo
            .delete_proof_requests_by_tee_signer(&tee_signer)
            .await
            .map_err(|error| internal(format!("Database error: {error}")))?;
        info!(tee_signer = %request.tee_signer, deleted_count, "Deleted TEE proofs by signer");
        Ok(DeleteProofsByTeeSignerResponse { deleted_count })
    }
}
