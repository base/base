use base_prover_service_db::{
    ProofRequest, ProofStatus as DbProofStatus, ProofType as DbProofType,
    SessionStatus as DbSessionStatus, canonical_session_id,
};
use base_prover_service_protocol::{
    GetProofRequest, GetProofResponse, ProofResult, ProofStatus, SnarkGroth16ProofResult,
    ZkProofResult, ZkVm,
};
use jsonrpsee::core::RpcResult;
use tracing::{Instrument, info};
use uuid::Uuid;

use crate::{
    backends::{OP_SUCCINCT_DRY_RUN_METADATA_KEY, OP_SUCCINCT_EXECUTION_STATS_METADATA_KEY},
    server::{ProverServiceServer, internal, invalid_argument, not_found, record_rpc_result},
};

fn is_dry_run_metadata(metadata: &serde_json::Value) -> bool {
    metadata
        .get(OP_SUCCINCT_DRY_RUN_METADATA_KEY)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        && metadata.get(OP_SUCCINCT_EXECUTION_STATS_METADATA_KEY).is_some()
}

fn proof_result_for_request(proof_req: &ProofRequest) -> RpcResult<ProofResult> {
    match proof_req.proof_type {
        Some(DbProofType::OpSuccinctSp1ClusterCompressed) => {
            let proof = proof_req
                .stark_receipt
                .clone()
                .ok_or_else(|| not_found("compressed proof receipt not available"))?;
            Ok(ProofResult::Compressed(ZkProofResult { zk_vm: ZkVm::Sp1, proof: proof.into() }))
        }
        Some(DbProofType::OpSuccinctSp1ClusterSnarkGroth16) => {
            let proof = proof_req
                .snark_receipt
                .clone()
                .ok_or_else(|| not_found("SNARK Groth16 proof receipt not available"))?;
            Ok(ProofResult::SnarkGroth16(SnarkGroth16ProofResult {
                proof: ZkProofResult { zk_vm: ZkVm::Sp1, proof: proof.into() },
            }))
        }
        None => Err(not_found("proof result not available for request without backend proof_type")),
    }
}

impl ProverServiceServer {
    /// Returns current proof status and proof bytes for the public `session_id`.
    pub async fn get_proof_impl(&self, request: GetProofRequest) -> RpcResult<GetProofResponse> {
        let start = std::time::Instant::now();
        let result = self.get_proof_inner(request).await;
        record_rpc_result("GetProof", start, &result);

        result
    }

    async fn request_is_dry_run(&self, proof_request_id: Uuid) -> RpcResult<bool> {
        let sessions = self
            .repo
            .get_sessions_for_request(proof_request_id)
            .await
            .map_err(|e| internal(format!("Database error: {e}")))?;

        Ok(sessions
            .iter()
            .filter(|session| session.status == DbSessionStatus::Completed)
            .filter_map(|session| session.metadata.as_ref())
            .any(is_dry_run_metadata))
    }

    async fn succeeded_result(&self, proof_req: &ProofRequest) -> RpcResult<Option<ProofResult>> {
        if proof_req.stark_receipt.is_none()
            && proof_req.snark_receipt.is_none()
            && self.request_is_dry_run(proof_req.id).await?
        {
            return Ok(Some(ProofResult::Compressed(ZkProofResult {
                zk_vm: ZkVm::Sp1,
                proof: Vec::new().into(),
            })));
        }

        Ok(Some(proof_result_for_request(proof_req)?))
    }

    async fn get_proof_inner(&self, request: GetProofRequest) -> RpcResult<GetProofResponse> {
        let session_id = canonical_session_id(&request.session_id)
            .map_err(|e| invalid_argument(format!("{e}")))?;
        let proof_req = self
            .repo
            .get_by_session_id(&session_id)
            .await
            .map_err(|e| internal(format!("Database error: {e}")))?
            .ok_or_else(|| not_found("Proof request not found"))?;
        let proof_request_id = proof_req.id;

        info!(
            proof_request_id = %proof_request_id,
            session_id = %proof_req.session_id,
            "Getting proof status"
        );

        let (status, result, error_message) = match proof_req.status {
            DbProofStatus::Created | DbProofStatus::Pending => (ProofStatus::Queued, None, None),
            DbProofStatus::Running => {
                let sync_span = tracing::info_span!(
                    "sync_proof_status",
                    proof_request_id = %proof_request_id,
                );
                self.manager
                    .sync_and_update_proof_status(&proof_req)
                    .instrument(sync_span)
                    .await
                    .map_err(|e| internal(format!("Failed to sync proof status: {e}")))?;

                let updated_proof_req = self
                    .repo
                    .get(proof_request_id)
                    .await
                    .map_err(|e| internal(format!("Database error: {e}")))?
                    .ok_or_else(|| not_found("Proof request not found"))?;

                match updated_proof_req.status {
                    DbProofStatus::Succeeded => (
                        ProofStatus::Succeeded,
                        self.succeeded_result(&updated_proof_req).await?,
                        None,
                    ),
                    DbProofStatus::Failed => {
                        (ProofStatus::Failed, None, updated_proof_req.error_message)
                    }
                    _ => (ProofStatus::Running, None, None),
                }
            }
            DbProofStatus::Succeeded => {
                (ProofStatus::Succeeded, self.succeeded_result(&proof_req).await?, None)
            }
            DbProofStatus::Failed => (ProofStatus::Failed, None, proof_req.error_message),
        };

        Ok(GetProofResponse { status, error_message, result })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use base_prover_service_db::{ApiProofType, ProofRequest, ProofType, ZkVmKind};
    use chrono::Utc;
    use uuid::Uuid;

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
        let id = Uuid::new_v4();
        ProofRequest {
            id,
            session_id: id.to_string(),
            request_payload: serde_json::json!({}),
            api_proof_type: match proof_type {
                ProofType::OpSuccinctSp1ClusterCompressed => ApiProofType::Compressed,
                ProofType::OpSuccinctSp1ClusterSnarkGroth16 => ApiProofType::SnarkGroth16,
            },
            zk_vm: Some(ZkVmKind::Sp1),
            tee_kind: None,
            start_block_number: 1,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: Some(proof_type),
            stark_receipt,
            snark_receipt,
            status: DbProofStatus::Succeeded,
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
            result,
            ProofResult::Compressed(ZkProofResult { zk_vm: ZkVm::Sp1, proof: stark_bytes.into() })
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
            result,
            ProofResult::SnarkGroth16(SnarkGroth16ProofResult {
                proof: ZkProofResult { zk_vm: ZkVm::Sp1, proof: snark_bytes.into() },
            })
        );
    }

    #[test]
    fn canonical_session_id_lowercases_uuid_values() {
        let id = Uuid::new_v4();

        assert_eq!(canonical_session_id(&id.to_string().to_uppercase()).unwrap(), id.to_string());
    }

    #[test]
    fn canonical_session_id_preserves_opaque_values() {
        let session_id = "Mock-SESSION-ABC123";

        assert_eq!(canonical_session_id(session_id).unwrap(), session_id);
    }
}
