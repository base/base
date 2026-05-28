use crate::{
    ProofRequest, SubmitProofRequest, SubmitProofResponse, ZkProofRequest, ZkVm, proof_request,
};
use base_prover_service_db::{
    CreateProofRequest, CreateProofRequestError, CreateProofRequestOutcome, ProofType,
};
use tonic::{Request, Response, Status};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{metrics, server::ProverServiceServer};

impl ProverServiceServer {
    /// Enqueues a new proof request and returns the generated `session_id=<uuid>`.
    pub async fn submit_proof_impl(
        &self,
        request: Request<SubmitProofRequest>,
    ) -> Result<Response<SubmitProofResponse>, Status> {
        let start = std::time::Instant::now();
        let result = self.submit_proof_inner(request).await;

        // Emit unified request metrics at handler boundary
        let (success, status_code) = match &result {
            Ok(_) => (true, "OK"),
            Err(s) => (false, metrics::grpc_status_code_str(s.code())),
        };
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        metrics::inc_requests("SubmitProof", success, status_code);
        metrics::record_response_latency("SubmitProof", success, elapsed_ms);

        result
    }

    async fn submit_proof_inner(
        &self,
        request: Request<SubmitProofRequest>,
    ) -> Result<Response<SubmitProofResponse>, Status> {
        let submit_request = request.into_inner();
        let proof_request = submit_request
            .proof
            .ok_or_else(|| Status::invalid_argument("proof request is required"))?;
        let session_id = parse_session_id(proof_request.session_id.as_deref())?;
        let (zk_request, proof_type, prover_address) = parse_zk_request(proof_request)?;

        info!(
            start_block_number = zk_request.start_block_number,
            num_blocks_to_prove = zk_request.number_of_blocks_to_prove,
            proof_type = %proof_type,
            prover_address = ?prover_address,
            l1_head = ?zk_request.l1_head,
            "Attempting to prove base block(s)",
        );

        // Validate prover_address for SNARK_GROTH16 proofs
        if proof_type == ProofType::OpSuccinctSp1ClusterSnarkGroth16 {
            let addr_str = prover_address.as_deref().ok_or_else(|| {
                Status::invalid_argument("prover_address is required for SNARK_GROTH16 proof type")
            })?;
            addr_str.parse::<alloy_primitives::Address>().map_err(|e| {
                Status::invalid_argument(format!(
                    "Invalid prover_address: must be a valid Ethereum address: {e}"
                ))
            })?;
        }

        // Validate l1_head hex format if provided
        if let Some(ref l1_head_str) = zk_request.l1_head {
            l1_head_str.parse::<alloy_primitives::B256>().map_err(|e| {
                Status::invalid_argument(format!(
                    "Invalid l1_head: must be a hex-encoded 32-byte hash (0x-prefixed): {e}"
                ))
            })?;
        }

        if let Some(interval) = zk_request.intermediate_root_interval {
            // Reject `intermediate_root_interval == 0`
            if interval == 0 {
                return Err(Status::invalid_argument(
                    "Invalid intermediate_root_interval: must be greater than 0",
                ));
            }
            // Reject misaligned ranges: `number_of_blocks_to_prove` must end on an
            // intermediate-root boundary
            if !zk_request.number_of_blocks_to_prove.is_multiple_of(interval) {
                return Err(Status::invalid_argument(format!(
                    "Invalid number_of_blocks_to_prove ({}): must be a multiple of intermediate_root_interval ({})",
                    zk_request.number_of_blocks_to_prove, interval,
                )));
            }
        }

        let db_request = CreateProofRequest {
            start_block_number: zk_request.start_block_number,
            number_of_blocks_to_prove: zk_request.number_of_blocks_to_prove,
            sequence_window: zk_request.sequence_window,
            proof_type,
            session_id,
            prover_address,
            l1_head: zk_request.l1_head,
            intermediate_root_interval: zk_request.intermediate_root_interval,
        };

        let outcome =
            self.repo.create_with_outbox(db_request, self.max_proof_retries).await.map_err(|e| match e {
            CreateProofRequestError::IdCollision { id, field } => {
                warn!(
                    proof_request_id = %id,
                    mismatched_field = field,
                    "rejected SubmitProof: session_id already bound to a different request"
                );
                Status::failed_precondition(format!(
                    "session_id {id} already exists with a different {field}"
                ))
            }
            CreateProofRequestError::SessionRowMissingAfterConflict { id } => {
                warn!(
                    proof_request_id = %id,
                    "rejected SubmitProof: session_id row missing after insert conflict"
                );
                Status::unavailable(format!(
                    "session_id {id} is temporarily unavailable after conflict; retry submit_proof"
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
                    "rejected SubmitProof: proof request retry budget exhausted for this session_id",
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

        let response = SubmitProofResponse { session_id: proof_request_id.to_string() };

        Ok(Response::new(response))
    }
}

fn parse_session_id(session_id: Option<&str>) -> Result<Option<Uuid>, Status> {
    session_id
        .map(|id| {
            Uuid::parse_str(id)
                .map_err(|e| Status::invalid_argument(format!("Invalid session_id: {e}")))
        })
        .transpose()
}

fn parse_zk_request(
    proof_request: ProofRequest,
) -> Result<(ZkProofRequest, ProofType, Option<String>), Status> {
    let request = proof_request
        .request
        .ok_or_else(|| Status::invalid_argument("proof request body is required"))?;

    match request {
        proof_request::Request::Compressed(request) => {
            validate_zk_vm(request.zk_vm)?;
            Ok((request, ProofType::OpSuccinctSp1ClusterCompressed, None))
        }
        proof_request::Request::SnarkGroth16(request) => {
            let proof = request
                .proof
                .ok_or_else(|| Status::invalid_argument("snark_groth16.proof is required"))?;
            validate_zk_vm(proof.zk_vm)?;
            let prover_address =
                if request.prover_address.is_empty() { None } else { Some(request.prover_address) };
            Ok((proof, ProofType::OpSuccinctSp1ClusterSnarkGroth16, prover_address))
        }
        proof_request::Request::Tee(_) => {
            Err(Status::unimplemented("TEE proof requests are not supported by this ZK service"))
        }
    }
}

fn validate_zk_vm(zk_vm: i32) -> Result<(), Status> {
    let zk_vm = ZkVm::try_from(zk_vm)
        .map_err(|_| Status::invalid_argument(format!("invalid zk_vm value: {zk_vm}")))?;
    match zk_vm {
        ZkVm::Unspecified | ZkVm::Sp1 => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use base_prover_service_db::ProofType;

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
