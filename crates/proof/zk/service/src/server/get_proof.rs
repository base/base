use base_zk_client::{GetProofRequest, GetProofResponse, ProofJobStatus, ReceiptType};
use base_zk_db::ProofStatus;
use sp1_sdk::SP1ProofWithPublicValues;
use tonic::{Request, Response, Status};
use tracing::{Instrument, info};
use uuid::Uuid;

use crate::{metrics, server::ProverServiceServer};

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
        let (proto_status, receipt_bytes, error_message) = match proof_req.status {
            ProofStatus::Created => (ProofJobStatus::Created, vec![], None),
            ProofStatus::Pending => (ProofJobStatus::Pending, vec![], None),
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
                        let receipt =
                            get_receipt_by_type(&updated_proof_req, requested_receipt_type)?;
                        (ProofJobStatus::Succeeded, receipt, None)
                    }
                    ProofStatus::Failed => {
                        (ProofJobStatus::Failed, vec![], updated_proof_req.error_message)
                    }
                    _ => {
                        // Still RUNNING or PENDING
                        (ProofJobStatus::Running, vec![], None)
                    }
                }
            }
            ProofStatus::Succeeded => {
                let receipt_buf = get_receipt_by_type(&proof_req, requested_receipt_type)?;
                (ProofJobStatus::Succeeded, receipt_buf, None)
            }
            ProofStatus::Failed => (ProofJobStatus::Failed, vec![], proof_req.error_message),
        };

        let response =
            GetProofResponse { status: proto_status.into(), receipt: receipt_bytes, error_message };

        Ok(Response::new(response))
    }
}

#[cfg(test)]
mod tests {
    use base_zk_db::{ProofRequest, ProofType};
    use chrono::Utc;

    use super::*;

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
            created_at: now,
            updated_at: now,
            completed_at: Some(now),
            retry_count: 0,
        }
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
