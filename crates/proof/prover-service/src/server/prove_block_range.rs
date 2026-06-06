use base_prover_service_db::{
    CreateProofRequest, CreateProofRequestError, CreateProofRequestOutcome, canonical_session_id,
};
use base_prover_service_protocol::{ProveBlockRangeRequest, ProveBlockRangeResponse};
use jsonrpsee::core::RpcResult;
use tracing::{info, warn};

use crate::server::{
    ProverServiceServer, failed_precondition, internal, invalid_argument, record_rpc_result,
    resource_exhausted, unavailable,
};

impl ProverServiceServer {
    /// Enqueues a new proof request and returns the accepted session ID.
    pub async fn prove_block_range_impl(
        &self,
        request: ProveBlockRangeRequest,
    ) -> RpcResult<ProveBlockRangeResponse> {
        let start = std::time::Instant::now();
        let result = self.prove_block_range_inner(request).await;
        record_rpc_result("ProveBlockRange", start, &result);

        result
    }

    async fn prove_block_range_inner(
        &self,
        request: ProveBlockRangeRequest,
    ) -> RpcResult<ProveBlockRangeResponse> {
        let mut proof_request = request.proof;
        let session_id = parse_session_id(&proof_request.session_id)?;
        proof_request.session_id = session_id.clone();

        let db_request =
            CreateProofRequest::new(proof_request).map_err(|e| invalid_argument(format!("{e}")))?;

        info!(
            start_block_number = db_request.start_block_number,
            num_blocks_to_prove = db_request.number_of_blocks_to_prove,
            proof_type = ?db_request.proof_type,
            prover_address = ?db_request.prover_address.as_deref(),
            l1_head = ?db_request.l1_head.as_deref(),
            "Attempting to prove base block(s)",
        );

        if let Some(interval) = db_request.intermediate_root_interval {
            if interval == 0 {
                return Err(invalid_argument(
                    "Invalid intermediate_root_interval: must be greater than 0",
                ));
            }
            if !db_request.number_of_blocks_to_prove.is_multiple_of(interval) {
                return Err(invalid_argument(format!(
                    "Invalid number_of_blocks_to_prove ({}): must be a multiple of intermediate_root_interval ({})",
                    db_request.number_of_blocks_to_prove, interval,
                )));
            }
        }

        let outcome = self
            .repo
            .create_for_worker_queue(db_request, self.max_proof_retries)
            .await
            .map_err(|e| match e {
                CreateProofRequestError::IdCollision { id, field } => {
                    warn!(
                        proof_request_id = %id,
                        mismatched_field = field,
                        "rejected ProveBlockRange: session_id already bound to a different request"
                    );
                    failed_precondition(format!(
                        "session_id {id} already exists with a different {field}"
                    ))
                }
                CreateProofRequestError::SessionRowMissingAfterConflict { id } => {
                    warn!(
                        proof_request_id = %id,
                        "rejected ProveBlockRange: session_id row missing after insert conflict"
                    );
                    unavailable(format!(
                        "session_id {id} is temporarily unavailable after conflict; retry prove_block_range"
                    ))
                }
                CreateProofRequestError::Validation(e) => invalid_argument(format!("{e}")),
                CreateProofRequestError::Sqlx(e) => internal(format!("Database error: {e}")),
            })?;

        match outcome {
            CreateProofRequestOutcome::RetryExhausted(id) => {
                warn!(
                    proof_request_id = %id,
                    session_id = %session_id,
                    max_proof_retries = self.max_proof_retries,
                    "rejected ProveBlockRange: proof request retry budget exhausted for this session_id",
                );
                return Err(resource_exhausted(format!(
                    "session_id {session_id}: proof request retry budget exhausted; use get_proof for the stored terminal failure",
                )));
            }
            CreateProofRequestOutcome::Created(id) => {
                info!(
                    proof_request_id = %id,
                    "Created proof request for worker queue"
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

        Ok(ProveBlockRangeResponse { session_id })
    }
}

fn parse_session_id(session_id: &str) -> RpcResult<String> {
    canonical_session_id(session_id).map_err(|e| invalid_argument(format!("{e}")))
}

#[cfg(test)]
mod tests {
    use base_prover_service_db::{ApiProofType, ProofType};
    use uuid::Uuid;

    use super::parse_session_id;
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
    fn test_api_proof_type_label_compressed() {
        assert_eq!(metrics::api_proof_type_label(ApiProofType::Compressed), "compressed");
    }

    #[test]
    fn test_api_proof_type_label_snark_groth16() {
        assert_eq!(metrics::api_proof_type_label(ApiProofType::SnarkGroth16), "snark_groth16");
    }

    #[test]
    fn test_api_proof_type_label_tee() {
        assert_eq!(metrics::api_proof_type_label(ApiProofType::Tee), "tee");
    }

    #[test]
    fn parse_session_id_accepts_uppercase_uuid() {
        let id = Uuid::new_v4();
        let parsed = parse_session_id(&id.to_string().to_uppercase()).unwrap();

        assert_eq!(parsed, id.to_string());
    }

    #[test]
    fn parse_session_id_accepts_opaque_values() {
        let session_id = "tee/aws_nitro/claimed-root";
        let parsed = parse_session_id(session_id).unwrap();

        assert_eq!(parsed, session_id);
    }
}
