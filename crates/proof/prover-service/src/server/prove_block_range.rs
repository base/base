use base_prover_service_db::{
    ApiProofType, CreateProofRequest, CreateProofRequestError, CreateProofRequestOutcome,
    canonical_session_id,
};
use base_prover_service_protocol::{
    ProofRequest, ProofRequestIdCollisionMessage, ProofRequestKind, ProveBlockRangeRequest,
    ProveBlockRangeResponse,
};
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
        validate_protocol_version(&proof_request)?;
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

        validate_intermediate_root_interval(
            db_request.api_proof_type,
            db_request.number_of_blocks_to_prove,
            db_request.intermediate_root_interval,
        )?;

        let outcome = self
            .repo
            .create_for_worker_queue(db_request, self.config.max_proof_retries)
            .await
            .map_err(|e| match e {
                CreateProofRequestError::IdCollision { id, field } => {
                    warn!(
                        proof_request_id = %id,
                        mismatched_field = field,
                        "rejected ProveBlockRange: session_id already bound to a different request"
                    );
                    failed_precondition(ProofRequestIdCollisionMessage::for_field(id, field))
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
                    max_proof_retries = self.config.max_proof_retries,
                    "rejected ProveBlockRange: proof request retry budget exhausted for this session_id",
                );
                return Err(resource_exhausted(format!(
                    "session_id {session_id}: proof request retry budget exhausted; use get_proof for the stored terminal result",
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
                    "Idempotent replay of non-failed proof request"
                );
            }
        }

        Ok(ProveBlockRangeResponse { session_id })
    }
}

fn parse_session_id(session_id: &str) -> RpcResult<String> {
    canonical_session_id(session_id).map_err(|e| invalid_argument(format!("{e}")))
}

/// Rejects schedule-pinned requests mislabeled as legacy jobs.
fn validate_protocol_version(request: &ProofRequest) -> RpcResult<()> {
    let protocol_version = request.protocol_version;
    let schedule_l2_block_number = match &request.request {
        ProofRequestKind::Compressed(request) => request.schedule_l2_block_number,
        ProofRequestKind::SnarkPlonk(request) => request.proof.schedule_l2_block_number,
        ProofRequestKind::Tee(request) => request.proof.schedule_l2_block_number,
    };
    if schedule_l2_block_number.is_some() && protocol_version == 0 {
        return Err(invalid_argument(
            "schedule_l2_block_number requires a non-zero protocol_version",
        ));
    }

    Ok(())
}

fn validate_intermediate_root_interval(
    api_proof_type: ApiProofType,
    number_of_blocks_to_prove: u64,
    intermediate_root_interval: Option<u64>,
) -> RpcResult<()> {
    match api_proof_type {
        ApiProofType::Tee => return Ok(()),
        ApiProofType::Compressed | ApiProofType::SnarkPlonk => {}
    }

    if let Some(interval) = intermediate_root_interval {
        if interval == 0 {
            return Err(invalid_argument(
                "Invalid intermediate_root_interval: must be greater than 0",
            ));
        }
        if !number_of_blocks_to_prove.is_multiple_of(interval) {
            return Err(invalid_argument(format!(
                "Invalid number_of_blocks_to_prove ({number_of_blocks_to_prove}): must be a multiple of intermediate_root_interval ({interval})",
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use base_prover_service_db::{ApiProofType, ProofType};
    use base_prover_service_protocol::{
        ProofRequest, ProofRequestKind, SnarkPlonkProofRequest, TeeKind, TeeProofRequest,
        ZkBackend, ZkProofRequest, ZkVm,
    };
    use uuid::Uuid;

    use super::{parse_session_id, validate_intermediate_root_interval, validate_protocol_version};
    use crate::metrics;

    #[test]
    fn test_proof_type_label_compressed() {
        assert_eq!(
            metrics::proof_type_label(ProofType::OpSuccinctSp1ClusterCompressed),
            "compressed"
        );
    }

    #[test]
    fn test_proof_type_label_snark_plonk() {
        assert_eq!(
            metrics::proof_type_label(ProofType::OpSuccinctSp1ClusterSnarkPlonk),
            "snark_plonk"
        );
    }

    #[test]
    fn test_api_proof_type_label_compressed() {
        assert_eq!(metrics::api_proof_type_label(ApiProofType::Compressed), "compressed");
    }

    #[test]
    fn test_api_proof_type_label_snark_plonk() {
        assert_eq!(metrics::api_proof_type_label(ApiProofType::SnarkPlonk), "snark_plonk");
    }

    #[test]
    fn test_api_proof_type_label_tee() {
        assert_eq!(metrics::api_proof_type_label(ApiProofType::Tee), "tee");
    }

    #[test]
    fn validate_protocol_version_accepts_arbitrary_versions() {
        for version in [0, 1, 7, u32::MAX] {
            assert!(validate_protocol_version(&proof_request(version, None)).is_ok());
        }
    }

    #[test]
    fn schedule_pinning_requires_current_protocol_version() {
        let mut tee_request =
            TeeProofRequest { proof: Default::default(), tee_kind: TeeKind::AwsNitro };
        tee_request.proof.schedule_l2_block_number = Some(42);
        let requests = [
            ProofRequestKind::Compressed(zk_request(Some(42))),
            ProofRequestKind::SnarkPlonk(SnarkPlonkProofRequest {
                proof: zk_request(Some(42)),
                prover_address: Default::default(),
            }),
            ProofRequestKind::Tee(tee_request),
        ];

        for request in requests {
            let mut proof_request =
                ProofRequest { session_id: "session".to_owned(), protocol_version: 0, request };
            let err = validate_protocol_version(&proof_request)
                .expect_err("legacy protocol must reject schedule pinning");
            assert!(err.message().contains("schedule_l2_block_number requires"));

            proof_request.protocol_version = 7;
            assert!(validate_protocol_version(&proof_request).is_ok());
        }
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

    #[test]
    fn zkp_request_rejects_non_multiple_intermediate_root_interval() {
        let result = validate_intermediate_root_interval(ApiProofType::Compressed, 1, Some(30));

        assert!(result.is_err());
    }

    #[test]
    fn tee_request_accepts_intermediate_block_interval() {
        let result = validate_intermediate_root_interval(ApiProofType::Tee, 1, Some(30));

        assert!(result.is_ok());
    }

    fn proof_request(protocol_version: u32, schedule_l2_block_number: Option<u64>) -> ProofRequest {
        ProofRequest {
            session_id: "session".to_owned(),
            protocol_version,
            request: ProofRequestKind::Compressed(zk_request(schedule_l2_block_number)),
        }
    }

    fn zk_request(schedule_l2_block_number: Option<u64>) -> ZkProofRequest {
        ZkProofRequest {
            start_block_number: 1,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            l1_head: None,
            intermediate_root_interval: None,
            schedule_l2_block_number,
            zk_vm: ZkVm::Sp1,
            zk_backend: ZkBackend::Cluster,
        }
    }
}
