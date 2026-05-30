use alloy_primitives::B256;
use base_prover_service_db::{
    CreateProofRequest, CreateProofRequestError, CreateProofRequestOutcome, ProofType,
};
use base_prover_service_protocol::{
    ProofRequest, ProofRequestKind, ProveBlockRangeRequest, ProveBlockRangeResponse,
    ZkProofRequest, ZkVm,
};
use jsonrpsee::core::RpcResult;
use tracing::{info, warn};
use uuid::Uuid;

use crate::server::{
    ProverServiceServer, failed_precondition, internal, invalid_argument, record_rpc_result,
    resource_exhausted, unavailable,
};

impl ProverServiceServer {
    /// Enqueues a new proof request and returns the generated `session_id=<uuid>`.
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
        let session_id = parse_session_id(request.proof.session_id.as_deref())?;
        let (zk_request, proof_type, prover_address) = parse_zk_request(request.proof)?;
        let l1_head = zk_request.l1_head.map(format_hash);

        info!(
            start_block_number = zk_request.start_block_number,
            num_blocks_to_prove = zk_request.number_of_blocks_to_prove,
            proof_type = %proof_type,
            prover_address = ?prover_address,
            l1_head = ?l1_head,
            "Attempting to prove base block(s)",
        );

        if let Some(interval) = zk_request.intermediate_root_interval {
            if interval == 0 {
                return Err(invalid_argument(
                    "Invalid intermediate_root_interval: must be greater than 0",
                ));
            }
            if !zk_request.number_of_blocks_to_prove.is_multiple_of(interval) {
                return Err(invalid_argument(format!(
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
            l1_head,
            intermediate_root_interval: zk_request.intermediate_root_interval,
        };

        let outcome = self
            .repo
            .create_with_outbox(db_request, self.max_proof_retries)
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
                CreateProofRequestError::Sqlx(e) => internal(format!("Database error: {e}")),
            })?;

        let proof_request_id = outcome.id();
        match outcome {
            CreateProofRequestOutcome::RetryExhausted(id) => {
                warn!(
                    proof_request_id = %id,
                    max_proof_retries = self.max_proof_retries,
                    "rejected ProveBlockRange: proof request retry budget exhausted for this session_id",
                );
                return Err(resource_exhausted(format!(
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

        Ok(ProveBlockRangeResponse { session_id: proof_request_id.to_string() })
    }
}

fn parse_session_id(session_id: Option<&str>) -> RpcResult<Option<Uuid>> {
    session_id
        .map(|id| {
            Uuid::parse_str(id).map_err(|e| invalid_argument(format!("Invalid session_id: {e}")))
        })
        .transpose()
}

fn parse_zk_request(
    proof_request: ProofRequest,
) -> RpcResult<(ZkProofRequest, ProofType, Option<String>)> {
    match proof_request.request {
        ProofRequestKind::Compressed(request) => {
            validate_zk_vm(request.zk_vm)?;
            Ok((request, ProofType::OpSuccinctSp1ClusterCompressed, None))
        }
        ProofRequestKind::SnarkGroth16(request) => {
            validate_zk_vm(request.proof.zk_vm)?;
            let prover_address = Some(format!("{:#x}", request.prover_address));
            Ok((request.proof, ProofType::OpSuccinctSp1ClusterSnarkGroth16, prover_address))
        }
        ProofRequestKind::Tee(_) => {
            Err(invalid_argument("TEE proof requests are not supported by this ZK service"))
        }
    }
}

const fn validate_zk_vm(zk_vm: ZkVm) -> RpcResult<()> {
    match zk_vm {
        ZkVm::Sp1 => Ok(()),
    }
}

fn format_hash(hash: B256) -> String {
    format!("{hash:#x}")
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
}
