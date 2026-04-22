use base_zk_client::{ProveBlockRequest, ProveBlockResponse};
use base_zk_db::{CreateProofRequest, ProofType};
use tonic::{Request, Response, Status};
use tracing::info;
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
        };

        let proof_request_id = self
            .repo
            .create_with_outbox(db_request)
            .await
            .map_err(|e| Status::internal(format!("Database error: {e}")))?;

        info!(
            proof_request_id = %proof_request_id,
            "Created proof request and outbox entry"
        );

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
    }
}
