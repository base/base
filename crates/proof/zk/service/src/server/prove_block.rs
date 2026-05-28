use base_zk_client::{ProveBlockRequest, ProveBlockResponse};
use base_zk_db::{
    CreateProofRequest, CreateProofRequestError, CreateProofRequestOutcome, ProofType,
};
use tonic::{Request, Response, Status};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{metrics, server::ProverServiceServer};

impl ProverServiceServer {
    /// Enqueues a new proof request and returns the generated `session_id=<uuid>`.
    pub async fn prove_block_impl(
        &self,
        request: Request<ProveBlockRequest>,
    ) -> Result<Response<ProveBlockResponse>, Status> {
        let start = std::time::Instant::now();
        let result = self.prove_block_inner(request).await;

        // Emit unified request metrics at handler boundary
        let (success, status_code) = match &result {
            Ok(_) => (true, "OK"),
            Err(s) => (false, metrics::grpc_status_code_str(s.code())),
        };
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        metrics::inc_requests("ProveBlock", success, status_code);
        metrics::record_response_latency("ProveBlock", success, elapsed_ms);

        result
    }

    async fn prove_block_inner(
        &self,
        request: Request<ProveBlockRequest>,
    ) -> Result<Response<ProveBlockResponse>, Status> {
        let prove_block_request = request.into_inner();

        info!(
            start_block_number = prove_block_request.start_block_number,
            num_blocks_to_prove = prove_block_request.number_of_blocks_to_prove,
            proof_type = prove_block_request.proof_type,
            prover_address = ?prove_block_request.prover_address,
            l1_head = ?prove_block_request.l1_head,
            "Attempting to prove base block(s)",
        );

        let proof_type = ProofType::try_from(prove_block_request.proof_type)
            .map_err(|e| Status::invalid_argument(format!("Invalid proof_type: {e}")))?;

        // Validate prover_address for SNARK_GROTH16 proofs
        if proof_type == ProofType::OpSuccinctSp1ClusterSnarkGroth16 {
            let addr_str = prove_block_request.prover_address.as_deref().ok_or_else(|| {
                Status::invalid_argument("prover_address is required for SNARK_GROTH16 proof type")
            })?;
            addr_str.parse::<alloy_primitives::Address>().map_err(|e| {
                Status::invalid_argument(format!(
                    "Invalid prover_address: must be a valid Ethereum address: {e}"
                ))
            })?;
        }

        // Validate l1_head hex format if provided
        if let Some(ref l1_head_str) = prove_block_request.l1_head {
            l1_head_str.parse::<alloy_primitives::B256>().map_err(|e| {
                Status::invalid_argument(format!(
                    "Invalid l1_head: must be a hex-encoded 32-byte hash (0x-prefixed): {e}"
                ))
            })?;
        }

        if let Some(interval) = prove_block_request.intermediate_root_interval {
            // Reject `intermediate_root_interval == 0`
            if interval == 0 {
                return Err(Status::invalid_argument(
                    "Invalid intermediate_root_interval: must be greater than 0",
                ));
            }
            // Reject misaligned ranges: `number_of_blocks_to_prove` must end on an
            // intermediate-root boundary
            if !prove_block_request.number_of_blocks_to_prove.is_multiple_of(interval) {
                return Err(Status::invalid_argument(format!(
                    "Invalid number_of_blocks_to_prove ({}): must be a multiple of intermediate_root_interval ({})",
                    prove_block_request.number_of_blocks_to_prove, interval,
                )));
            }
        }

        let session_id = match prove_block_request.session_id {
            Some(ref id_str) => {
                let parsed = Uuid::parse_str(id_str)
                    .map_err(|e| Status::invalid_argument(format!("Invalid session_id: {e}")))?;
                Some(parsed)
            }
            None => None,
        };

        let db_request = CreateProofRequest {
            start_block_number: prove_block_request.start_block_number,
            number_of_blocks_to_prove: prove_block_request.number_of_blocks_to_prove,
            sequence_window: prove_block_request.sequence_window,
            proof_type,
            session_id,
            prover_address: prove_block_request.prover_address,
            l1_head: prove_block_request.l1_head,
            intermediate_root_interval: prove_block_request.intermediate_root_interval,
        };

        let outcome =
            self.repo.create_with_outbox(db_request, self.max_proof_retries).await.map_err(|e| match e {
            CreateProofRequestError::IdCollision { id, field } => {
                warn!(
                    proof_request_id = %id,
                    mismatched_field = field,
                    "rejected ProveBlock: session_id already bound to a different request"
                );
                Status::failed_precondition(format!(
                    "session_id {id} already exists with a different {field}"
                ))
            }
            CreateProofRequestError::SessionRowMissingAfterConflict { id } => {
                warn!(
                    proof_request_id = %id,
                    "rejected ProveBlock: session_id row missing after insert conflict"
                );
                Status::unavailable(format!(
                    "session_id {id} is temporarily unavailable after conflict; retry prove_block"
                ))
            }
            CreateProofRequestError::Sqlx(e) => Status::internal(format!("Database error: {e}")),
        })?;

        let proof_request_id = outcome.id();
        match outcome {
            CreateProofRequestOutcome::RetryExhausted(id) => {
                warn!(
                    proof_request_id = %id,
                    max_proof_retries = self.max_proof_retries,
                    "rejected ProveBlock: proof request retry budget exhausted for this session_id",
                );
                return Err(Status::resource_exhausted(format!(
                    "session_id {id}: proof request retry budget exhausted; use get_proof for the stored terminal failure",
                )));
            }
            CreateProofRequestOutcome::Created(id) => {
                info!(
                    proof_request_id = %id,
                    "Created proof request and outbox entry"
                );
            }
            CreateProofRequestOutcome::Requeued(id) => {
                info!(
                    proof_request_id = %id,
                    "Requeued previously failed proof request"
                );
            }
            CreateProofRequestOutcome::Replayed(id) => {
                info!(
                    proof_request_id = %id,
                    "Idempotent replay of in-flight or succeeded proof request"
                );
            }
        }

        let response = ProveBlockResponse { session_id: proof_request_id.to_string() };

        Ok(Response::new(response))
    }
}

#[cfg(test)]
mod tests {
    use base_zk_db::ProofType;

    use crate::metrics;

    #[test]
    fn test_proof_type_label_compressed() {
        assert_eq!(
            metrics::proof_type_label(ProofType::OpSuccinctSp1ClusterCompressed),
            "compressed"
        );
    }

    #[test]
    fn test_proof_type_label_snark_groth16() {
        assert_eq!(
            metrics::proof_type_label(ProofType::OpSuccinctSp1ClusterSnarkGroth16),
            "snark_groth16"
        );
    }

    #[test]
    fn test_grpc_status_code_str() {
        assert_eq!(metrics::grpc_status_code_str(tonic::Code::Ok), "OK");
        assert_eq!(metrics::grpc_status_code_str(tonic::Code::InvalidArgument), "INVALID_ARGUMENT");
        assert_eq!(metrics::grpc_status_code_str(tonic::Code::Internal), "INTERNAL");
        assert_eq!(metrics::grpc_status_code_str(tonic::Code::NotFound), "NOT_FOUND");
        assert_eq!(
            metrics::grpc_status_code_str(tonic::Code::ResourceExhausted),
            "RESOURCE_EXHAUSTED"
        );
        assert_eq!(metrics::grpc_status_code_str(tonic::Code::Unavailable), "UNAVAILABLE");
    }
}
