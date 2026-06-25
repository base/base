use base_zk_client::{
    ExecutionStats, GetProofRequest, GetProofResponse, ProofJobStatus, ReceiptType,
};
use base_zk_db::ProofStatus;
use sp1_sdk::SP1ProofWithPublicValues;
use tonic::{Request, Response, Status};
use tracing::{Instrument, info};
use uuid::Uuid;

use crate::{
    backends::{
        OP_SUCCINCT_DRY_RUN_METADATA_KEY, OP_SUCCINCT_EXECUTION_STATS_METADATA_KEY,
        OpSuccinctStoredExecutionStats,
    },
    metrics,
    server::ProverServiceServer,
};

/// Helper function to get the appropriate receipt based on requested type.
fn get_receipt_by_type(
    proof_req: &base_zk_db::ProofRequest,
    requested_type: ReceiptType,
) -> Result<Vec<u8>, Status> {
    match requested_type {
        ReceiptType::Unspecified | ReceiptType::Stark => proof_req
            .stark_receipt
            .clone()
            .ok_or_else(|| Status::not_found("STARK receipt not available")),
        ReceiptType::Snark => proof_req
            .snark_receipt
            .clone()
            .ok_or_else(|| Status::not_found("SNARK receipt not available")),
        ReceiptType::OnChainSnark => {
            let snark_bytes = proof_req
                .snark_receipt
                .as_ref()
                .ok_or_else(|| Status::not_found("SNARK receipt not available"))?;

            let (proof_with_pv, _): (SP1ProofWithPublicValues, _) =
                bincode::serde::decode_from_slice(snark_bytes, bincode::config::standard())
                    .map_err(|e| {
                        Status::internal(format!("Failed to deserialize SNARK proof: {e}"))
                    })?;

            Ok(proof_with_pv.bytes())
        }
    }
}

fn execution_stats_from_metadata(metadata: &serde_json::Value) -> Option<ExecutionStats> {
    if !metadata.get(OP_SUCCINCT_DRY_RUN_METADATA_KEY)?.as_bool()? {
        return None;
    }

    let stats = metadata.get(OP_SUCCINCT_EXECUTION_STATS_METADATA_KEY)?;
    let stored = serde_json::from_value::<OpSuccinctStoredExecutionStats>(stats.clone()).ok()?;
    Some(stored.into())
}

impl ProverServiceServer {
    /// Returns current proof status and receipt bytes for `session_id=<uuid>`.
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

    async fn execution_stats_for_request(
        &self,
        proof_request_id: Uuid,
    ) -> Result<Option<ExecutionStats>, Status> {
        let sessions = self
            .repo
            .get_sessions_for_request(proof_request_id)
            .await
            .map_err(|e| Status::internal(format!("Database error: {e}")))?;

        Ok(sessions
            .iter()
            .filter_map(|session| session.metadata.as_ref())
            .find_map(execution_stats_from_metadata))
    }

    async fn succeeded_payload(
        &self,
        proof_req: &base_zk_db::ProofRequest,
        requested_receipt_type: ReceiptType,
    ) -> Result<(Vec<u8>, Option<ExecutionStats>), Status> {
        if proof_req.stark_receipt.is_none() && proof_req.snark_receipt.is_none() {
            // Receipt absence is only a fast path before touching session metadata; the dry-run
            // marker remains the authoritative check inside `execution_stats_from_metadata`.
            let execution_stats = self.execution_stats_for_request(proof_req.id).await?;
            if execution_stats.is_some() {
                return Ok((vec![], execution_stats));
            }
        }

        let receipt = get_receipt_by_type(proof_req, requested_receipt_type)?;
        Ok((receipt, None))
    }

    async fn get_proof_inner(
        &self,
        request: Request<GetProofRequest>,
    ) -> std::result::Result<Response<GetProofResponse>, Status> {
        let get_proof_request = request.into_inner();

        // Parse UUID from request
        let proof_request_id = Uuid::parse_str(&get_proof_request.session_id)
            .map_err(|_| Status::invalid_argument("Invalid UUID"))?;

        // Determine requested receipt type (default to STARK)
        let requested_receipt_type = get_proof_request
            .receipt_type
            .and_then(|t| ReceiptType::try_from(t).ok())
            .unwrap_or(ReceiptType::Stark);

        info!(
            proof_request_id = %proof_request_id,
            receipt_type = ?requested_receipt_type,
            "Getting proof status"
        );

        // Get from database
        let proof_req = self
            .repo
            .get(proof_request_id)
            .await
            .map_err(|e| Status::internal(format!("Database error: {e}")))?
            .ok_or_else(|| Status::not_found("Proof request not found"))?;

        // Map database status to proto status
        let (proto_status, receipt_bytes, error_message, execution_stats) = match proof_req.status {
            ProofStatus::Created => (ProofJobStatus::Created, vec![], None, None),
            ProofStatus::Pending => (ProofJobStatus::Pending, vec![], None, None),
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

                // Map updated status to response
                match updated_proof_req.status {
                    ProofStatus::Succeeded => {
                        let (receipt, execution_stats) = self
                            .succeeded_payload(&updated_proof_req, requested_receipt_type)
                            .await?;
                        (ProofJobStatus::Succeeded, receipt, None, execution_stats)
                    }
                    ProofStatus::Failed => {
                        (ProofJobStatus::Failed, vec![], updated_proof_req.error_message, None)
                    }
                    _ => {
                        // Still RUNNING or PENDING
                        (ProofJobStatus::Running, vec![], None, None)
                    }
                }
            }
            ProofStatus::Succeeded => {
                let (receipt, execution_stats) =
                    self.succeeded_payload(&proof_req, requested_receipt_type).await?;
                (ProofJobStatus::Succeeded, receipt, None, execution_stats)
            }
            ProofStatus::Failed => (ProofJobStatus::Failed, vec![], proof_req.error_message, None),
        };

        let response = GetProofResponse {
            status: proto_status.into(),
            receipt: receipt_bytes,
            error_message,
            execution_stats,
        };

        Ok(Response::new(response))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use base_zk_db::{ProofRequest, ProofType};
    use chrono::Utc;

    use super::*;

    fn metadata_with_execution_stats(stats: serde_json::Value) -> serde_json::Value {
        let mut metadata = serde_json::Map::new();
        metadata
            .insert(OP_SUCCINCT_DRY_RUN_METADATA_KEY.to_string(), serde_json::Value::Bool(true));
        metadata.insert(OP_SUCCINCT_EXECUTION_STATS_METADATA_KEY.to_string(), stats);
        serde_json::Value::Object(metadata)
    }

    fn load_snark_fixture() -> Vec<u8> {
        let path =
            format!("{}/tests/fixtures/sample_snark_receipt.bin", env!("CARGO_MANIFEST_DIR"));
        std::fs::read(&path).unwrap_or_else(|e| panic!("Failed to read fixture {path}: {e}"))
    }

    fn make_proof_request(
        stark_receipt: Option<Vec<u8>>,
        snark_receipt: Option<Vec<u8>>,
    ) -> ProofRequest {
        let now = Utc::now();
        ProofRequest {
            id: Uuid::new_v4(),
            start_block_number: 1,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type: ProofType::OpSuccinctSp1ClusterSnarkGroth16,
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
    fn test_execution_stats_from_metadata_deserializes_stored_stats() {
        let stored_stats = OpSuccinctStoredExecutionStats {
            total_instruction_cycles: 100,
            total_sp1_gas: 200,
            cycle_tracker: HashMap::from([("range".to_string(), 42)]),
            witness_generation_ms: 12.5,
            execution_ms: 34.5,
        };
        let metadata =
            metadata_with_execution_stats(serde_json::to_value(stored_stats).expect("serialize"));

        let stats = execution_stats_from_metadata(&metadata).expect("execution stats");

        assert_eq!(stats.total_instruction_cycles, 100);
        assert_eq!(stats.total_sp1_gas, 200);
        assert_eq!(stats.cycle_tracker.get("range"), Some(&42));
        assert_eq!(stats.witness_generation_ms, 12.5);
        assert_eq!(stats.execution_ms, 34.5);
    }

    #[test]
    fn test_execution_stats_from_metadata_rejects_invalid_schema() {
        let metadata = metadata_with_execution_stats(serde_json::json!({
            "total_instruction_cycles": "100",
            "total_sp1_gas": 200,
            "cycle_tracker": {},
            "witness_generation_ms": 12.5,
            "execution_ms": 34.5,
        }));

        assert!(execution_stats_from_metadata(&metadata).is_none());
    }

    #[test]
    fn test_execution_stats_from_metadata_requires_dry_run_marker() {
        let stored_stats = OpSuccinctStoredExecutionStats {
            total_instruction_cycles: 100,
            total_sp1_gas: 200,
            cycle_tracker: HashMap::new(),
            witness_generation_ms: 12.5,
            execution_ms: 34.5,
        };
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            OP_SUCCINCT_EXECUTION_STATS_METADATA_KEY.to_string(),
            serde_json::to_value(stored_stats).expect("serialize"),
        );

        assert!(execution_stats_from_metadata(&serde_json::Value::Object(metadata)).is_none());
    }

    #[test]
    fn test_get_receipt_stark_returns_stark_bytes() {
        let stark_bytes = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let req = make_proof_request(Some(stark_bytes.clone()), None);

        let result = get_receipt_by_type(&req, ReceiptType::Stark).unwrap();
        assert_eq!(result, stark_bytes);

        let result = get_receipt_by_type(&req, ReceiptType::Unspecified).unwrap();
        assert_eq!(result, stark_bytes);
    }

    #[test]
    fn test_get_receipt_stark_missing_returns_not_found() {
        let req = make_proof_request(None, None);
        let err = get_receipt_by_type(&req, ReceiptType::Stark).unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert!(err.message().contains("STARK"));
    }

    #[test]
    fn test_get_receipt_snark_returns_raw_snark_bytes() {
        let snark_bytes = load_snark_fixture();
        let req = make_proof_request(None, Some(snark_bytes.clone()));
        let result = get_receipt_by_type(&req, ReceiptType::Snark).unwrap();
        assert_eq!(result, snark_bytes);
    }

    #[test]
    fn test_get_receipt_snark_missing_returns_not_found() {
        let req = make_proof_request(None, None);
        let err = get_receipt_by_type(&req, ReceiptType::Snark).unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert!(err.message().contains("SNARK"));
    }

    #[test]
    fn test_get_receipt_on_chain_snark_returns_onchain_bytes() {
        let snark_bytes = load_snark_fixture();
        let req = make_proof_request(None, Some(snark_bytes.clone()));
        let result = get_receipt_by_type(&req, ReceiptType::OnChainSnark).unwrap();

        assert_ne!(result, snark_bytes);
        assert!(
            result.len() < snark_bytes.len(),
            "on-chain bytes ({}) should be smaller than bincode SNARK ({})",
            result.len(),
            snark_bytes.len()
        );

        assert!(result.len() >= 4, "on-chain bytes must be at least 4 bytes");
        assert_eq!(&result[..4], [0x0e, 0x78, 0xf4, 0xdb], "Groth16 verifier selector");

        let (proof_with_pv, _): (SP1ProofWithPublicValues, _) =
            bincode::serde::decode_from_slice(&snark_bytes, bincode::config::standard())
                .expect("fixture should deserialize");
        assert_eq!(result, proof_with_pv.bytes());
    }

    #[test]
    fn test_get_receipt_on_chain_snark_missing_returns_not_found() {
        let req = make_proof_request(None, None);
        let err = get_receipt_by_type(&req, ReceiptType::OnChainSnark).unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert!(err.message().contains("SNARK"));
    }
}
