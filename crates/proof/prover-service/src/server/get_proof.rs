use crate::{
    GetProofRequest, GetProofResponse, ProofResult, ProofStatus as ProtoProofStatus,
    SnarkGroth16ProofResult, ZkProofResult, ZkVm, proof_result,
};
use base_prover_service_db::{
    ProofRequest, ProofStatus, ProofType as DbProofType, SessionStatus as DbSessionStatus,
};
use tonic::{Request, Response, Status};
use tracing::{Instrument, info};
use uuid::Uuid;

use crate::{
    backends::{OP_SUCCINCT_DRY_RUN_METADATA_KEY, OP_SUCCINCT_EXECUTION_STATS_METADATA_KEY},
    metrics,
    server::ProverServiceServer,
};

fn is_dry_run_metadata(metadata: &serde_json::Value) -> bool {
    metadata
        .get(OP_SUCCINCT_DRY_RUN_METADATA_KEY)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        && metadata.get(OP_SUCCINCT_EXECUTION_STATS_METADATA_KEY).is_some()
}

fn proof_result_for_request(proof_req: &ProofRequest) -> Result<ProofResult, Status> {
    match proof_req.proof_type {
        DbProofType::OpSuccinctSp1ClusterCompressed => {
            let proof = proof_req
                .stark_receipt
                .clone()
                .ok_or_else(|| Status::not_found("compressed proof receipt not available"))?;
            Ok(ProofResult {
                result: Some(proof_result::Result::Compressed(ZkProofResult {
                    zk_vm: ZkVm::Sp1.into(),
                    proof,
                })),
            })
        }
        DbProofType::OpSuccinctSp1ClusterSnarkGroth16 => {
            let proof = proof_req
                .snark_receipt
                .clone()
                .ok_or_else(|| Status::not_found("SNARK Groth16 proof receipt not available"))?;
            Ok(ProofResult {
                result: Some(proof_result::Result::SnarkGroth16(SnarkGroth16ProofResult {
                    proof: Some(ZkProofResult { zk_vm: ZkVm::Sp1.into(), proof }),
                })),
            })
        }
    }
}

impl ProverServiceServer {
    /// Returns current proof status and proof bytes for `session_id=<uuid>`.
    pub async fn get_proof_impl(
        &self,
        request: Request<GetProofRequest>,
    ) -> std::result::Result<Response<GetProofResponse>, Status> {
        let start = std::time::Instant::now();
        let result = self.get_proof_inner(request).await;

        // Emit unified request metrics at handler boundary
        let (success, status_code) = match &result {
            Ok(_) => (true, "OK"),
            Err(s) => (false, metrics::grpc_status_code_str(s.code())),
        };
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        metrics::inc_requests("GetProof", success, status_code);
        metrics::record_response_latency("GetProof", success, elapsed_ms);

        result
    }

    async fn request_is_dry_run(&self, proof_request_id: Uuid) -> Result<bool, Status> {
        let sessions = self
            .repo
            .get_sessions_for_request(proof_request_id)
            .await
            .map_err(|e| Status::internal(format!("Database error: {e}")))?;

        Ok(sessions
            .iter()
            .filter(|session| session.status == DbSessionStatus::Completed)
            .filter_map(|session| session.metadata.as_ref())
            .any(is_dry_run_metadata))
    }

    async fn succeeded_result(
        &self,
        proof_req: &ProofRequest,
    ) -> Result<Option<ProofResult>, Status> {
        if proof_req.stark_receipt.is_none()
            && proof_req.snark_receipt.is_none()
            && self.request_is_dry_run(proof_req.id).await?
        {
            return Ok(Some(ProofResult {
                result: Some(proof_result::Result::Compressed(ZkProofResult {
                    zk_vm: ZkVm::Sp1.into(),
                    proof: Vec::new(),
                })),
            }));
        }

        Ok(Some(proof_result_for_request(proof_req)?))
    }

    async fn get_proof_inner(
        &self,
        request: Request<GetProofRequest>,
    ) -> std::result::Result<Response<GetProofResponse>, Status> {
        let get_proof_request = request.into_inner();

        // Parse UUID from request
        let proof_request_id = Uuid::parse_str(&get_proof_request.session_id)
            .map_err(|_| Status::invalid_argument("Invalid UUID"))?;

        info!(proof_request_id = %proof_request_id, "Getting proof status");

        // Get from database
        let proof_req = self
            .repo
            .get(proof_request_id)
            .await
            .map_err(|e| Status::internal(format!("Database error: {e}")))?
            .ok_or_else(|| Status::not_found("Proof request not found"))?;

        // Map database status to proto status
        let (proto_status, result, error_message) = match proof_req.status {
            ProofStatus::Created | ProofStatus::Pending => (ProtoProofStatus::Queued, None, None),
            ProofStatus::Running => {
                // Sync sessions and update proof status, with a tracing span so all
                // nested log lines carry proof_request_id.
                let sync_span = tracing::info_span!(
                    "sync_proof_status",
                    proof_request_id = %proof_request_id,
                );
                self.manager
                    .sync_and_update_proof_status(&proof_req)
                    .instrument(sync_span)
                    .await
                    .map_err(|e| Status::internal(format!("Failed to sync proof status: {e}")))?;

                // Re-query proof request to get updated status
                let updated_proof_req = self
                    .repo
                    .get(proof_request_id)
                    .await
                    .map_err(|e| Status::internal(format!("Database error: {e}")))?
                    .ok_or_else(|| Status::not_found("Proof request not found"))?;

                match updated_proof_req.status {
                    ProofStatus::Succeeded => (
                        ProtoProofStatus::Succeeded,
                        self.succeeded_result(&updated_proof_req).await?,
                        None,
                    ),
                    ProofStatus::Failed => {
                        (ProtoProofStatus::Failed, None, updated_proof_req.error_message)
                    }
                    _ => (ProtoProofStatus::Running, None, None),
                }
            }
            ProofStatus::Succeeded => {
                (ProtoProofStatus::Succeeded, self.succeeded_result(&proof_req).await?, None)
            }
            ProofStatus::Failed => (ProtoProofStatus::Failed, None, proof_req.error_message),
        };

        let response = GetProofResponse { status: proto_status.into(), error_message, result };

        Ok(Response::new(response))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use base_prover_service_db::{ProofRequest, ProofType};
    use chrono::Utc;

    use super::*;
    use crate::OpSuccinctStoredExecutionStats;

    fn metadata_with_execution_stats(stats: serde_json::Value) -> serde_json::Value {
        let mut metadata = serde_json::Map::new();
        metadata
            .insert(OP_SUCCINCT_DRY_RUN_METADATA_KEY.to_string(), serde_json::Value::Bool(true));
        metadata.insert(OP_SUCCINCT_EXECUTION_STATS_METADATA_KEY.to_string(), stats);
        serde_json::Value::Object(metadata)
    }

    fn make_proof_request(
        proof_type: ProofType,
        stark_receipt: Option<Vec<u8>>,
        snark_receipt: Option<Vec<u8>>,
    ) -> ProofRequest {
        let now = Utc::now();
        ProofRequest {
            id: Uuid::new_v4(),
            start_block_number: 1,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type,
            stark_receipt,
            snark_receipt,
            status: ProofStatus::Succeeded,
            error_message: None,
            prover_address: None,
            l1_head: None,
            intermediate_root_interval: None,
            created_at: now,
            updated_at: now,
            completed_at: Some(now),
            retry_count: 0,
        }
    }

    #[test]
    fn dry_run_metadata_requires_marker_and_stats() {
        let stored_stats = OpSuccinctStoredExecutionStats {
            total_instruction_cycles: 100,
            total_sp1_gas: 200,
            cycle_tracker: HashMap::from([("range".to_string(), 42)]),
            witness_generation_ms: 12.5,
            execution_ms: 34.5,
        };
        let metadata =
            metadata_with_execution_stats(serde_json::to_value(stored_stats).expect("serialize"));

        assert!(is_dry_run_metadata(&metadata));
        assert!(!is_dry_run_metadata(&serde_json::json!({ "dry_run": true })));
    }

    #[test]
    fn proof_result_for_compressed_returns_stark_bytes() {
        let stark_bytes = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let req = make_proof_request(
            ProofType::OpSuccinctSp1ClusterCompressed,
            Some(stark_bytes.clone()),
            None,
        );

        let result = proof_result_for_request(&req).unwrap();
        assert_eq!(
            result.result,
            Some(proof_result::Result::Compressed(ZkProofResult {
                zk_vm: ZkVm::Sp1.into(),
                proof: stark_bytes,
            }))
        );
    }

    #[test]
    fn proof_result_for_snark_returns_snark_bytes() {
        let snark_bytes = vec![0xCA, 0xFE];
        let req = make_proof_request(
            ProofType::OpSuccinctSp1ClusterSnarkGroth16,
            None,
            Some(snark_bytes.clone()),
        );

        let result = proof_result_for_request(&req).unwrap();
        assert_eq!(
            result.result,
            Some(proof_result::Result::SnarkGroth16(SnarkGroth16ProofResult {
                proof: Some(ZkProofResult { zk_vm: ZkVm::Sp1.into(), proof: snark_bytes }),
            }))
        );
    }
}
