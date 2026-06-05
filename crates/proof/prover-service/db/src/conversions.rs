//! Conversions between database models and protocol types.
//!
//! Centralizing translation here keeps endpoints from rebuilding protocol
//! responses through ad hoc field access.

use base_prover_service_protocol::{
    ProofJob as ProtocolProofJob, ProofJobStatus as ProtocolProofJobStatus,
    ProofResult as ProtocolProofResult, ProofStatus as ProtocolProofStatus,
    ProofSummary as ProtocolProofSummary, ProofType as ProtocolProofType, SnarkGroth16ProofResult,
    TeeKind as ProtocolTeeKind, ZkProofResult, ZkVm as ProtocolZkVm,
};

use crate::{
    ApiProofType, ProofJob, ProofJobStatus, ProofRequest, ProofRequestListItem, ProofStatus,
    ProofType, TeeKind, ZkVmKind,
};

/// Errors raised while converting stored database state into protocol types.
#[derive(Debug, thiserror::Error)]
pub enum ConversionError {
    /// The stored `request_payload` could not be deserialized into a protocol request.
    #[error("stored request payload for session_id {session_id} is invalid: {source}")]
    InvalidRequestPayload {
        /// Public session identifier of the affected proof request.
        session_id: String,
        /// Underlying deserialization error.
        #[source]
        source: serde_json::Error,
    },
    /// The stored `result_payload` could not be deserialized into a protocol result.
    #[error("stored result payload for session_id {session_id} is invalid: {source}")]
    InvalidResultPayload {
        /// Public session identifier of the affected proof request.
        session_id: String,
        /// Underlying deserialization error.
        #[source]
        source: serde_json::Error,
    },
}

impl From<ProtocolProofType> for ApiProofType {
    fn from(proof_type: ProtocolProofType) -> Self {
        match proof_type {
            ProtocolProofType::Compressed => Self::Compressed,
            ProtocolProofType::SnarkGroth16 => Self::SnarkGroth16,
            ProtocolProofType::Tee => Self::Tee,
        }
    }
}

impl From<ApiProofType> for ProtocolProofType {
    fn from(proof_type: ApiProofType) -> Self {
        match proof_type {
            ApiProofType::Compressed => Self::Compressed,
            ApiProofType::SnarkGroth16 => Self::SnarkGroth16,
            ApiProofType::Tee => Self::Tee,
        }
    }
}

impl From<ProtocolZkVm> for ZkVmKind {
    fn from(zk_vm: ProtocolZkVm) -> Self {
        match zk_vm {
            ProtocolZkVm::Sp1 => Self::Sp1,
        }
    }
}

impl From<ZkVmKind> for ProtocolZkVm {
    fn from(zk_vm: ZkVmKind) -> Self {
        match zk_vm {
            ZkVmKind::Sp1 => Self::Sp1,
        }
    }
}

impl From<ProtocolTeeKind> for TeeKind {
    fn from(tee_kind: ProtocolTeeKind) -> Self {
        match tee_kind {
            ProtocolTeeKind::AwsNitro => Self::AwsNitro,
        }
    }
}

impl From<TeeKind> for ProtocolTeeKind {
    fn from(tee_kind: TeeKind) -> Self {
        match tee_kind {
            TeeKind::AwsNitro => Self::AwsNitro,
        }
    }
}

impl From<ProofJobStatus> for ProtocolProofJobStatus {
    fn from(status: ProofJobStatus) -> Self {
        match status {
            ProofJobStatus::Pending => Self::Pending,
            ProofJobStatus::Claimed => Self::Claimed,
            ProofJobStatus::Succeeded => Self::Succeeded,
            ProofJobStatus::Failed => Self::Failed,
        }
    }
}

impl From<ProtocolProofJobStatus> for ProofJobStatus {
    fn from(status: ProtocolProofJobStatus) -> Self {
        match status {
            ProtocolProofJobStatus::Pending => Self::Pending,
            ProtocolProofJobStatus::Claimed => Self::Claimed,
            ProtocolProofJobStatus::Succeeded => Self::Succeeded,
            ProtocolProofJobStatus::Failed => Self::Failed,
        }
    }
}

impl From<ProofStatus> for ProtocolProofStatus {
    fn from(status: ProofStatus) -> Self {
        match status {
            ProofStatus::Created | ProofStatus::Pending => Self::Queued,
            ProofStatus::Running => Self::Running,
            ProofStatus::Succeeded => Self::Succeeded,
            ProofStatus::Failed => Self::Failed,
        }
    }
}

impl ProofStatus {
    /// Database statuses satisfying a protocol filter (`Queued` covers both
    /// `CREATED` and `PENDING`).
    pub fn matching_filter(filter: ProtocolProofStatus) -> Vec<Self> {
        match filter {
            ProtocolProofStatus::Queued => vec![Self::Created, Self::Pending],
            ProtocolProofStatus::Running => vec![Self::Running],
            ProtocolProofStatus::Succeeded => vec![Self::Succeeded],
            ProtocolProofStatus::Failed => vec![Self::Failed],
        }
    }
}

impl From<ProofRequestListItem> for ProtocolProofSummary {
    fn from(item: ProofRequestListItem) -> Self {
        Self {
            session_id: item.session_id,
            proof_type: item.api_proof_type.into(),
            status: item.status.into(),
            created_at: item.created_at,
            updated_at: item.updated_at,
            completed_at: item.completed_at,
            error_message: item.error_message,
            tee_kind: item.tee_kind.map(Into::into),
            zk_vm: item.zk_vm.map(Into::into),
        }
    }
}

impl TryFrom<ProofJob> for ProtocolProofJob {
    type Error = ConversionError;

    fn try_from(job: ProofJob) -> Result<Self, Self::Error> {
        let session_id = job.session_id;
        let request = serde_json::from_value(job.request_payload).map_err(|source| {
            ConversionError::InvalidRequestPayload { session_id: session_id.clone(), source }
        })?;

        Ok(Self {
            session_id,
            status: job.job_status.into(),
            request,
            attempt: u32::try_from(job.attempt).unwrap_or(0),
            lock_id: job.lock_id.map(|id| id.to_string()),
            worker_id: job.worker_id,
            lock_expires_at: job.lock_expires_at,
            created_at: job.created_at,
            updated_at: job.updated_at,
            completed_at: job.completed_at,
            error_message: job.error_message,
        })
    }
}

impl ProofRequest {
    /// Build the protocol result from stored state: prefers `result_payload`,
    /// else reconstructs from the legacy STARK/SNARK receipts. `None` means no
    /// result data is available.
    pub fn stored_proof_result(self) -> Result<Option<ProtocolProofResult>, ConversionError> {
        if let Some(result_payload) = self.result_payload {
            return serde_json::from_value(result_payload).map(Some).map_err(|source| {
                ConversionError::InvalidResultPayload { session_id: self.session_id, source }
            });
        }

        Ok(match self.proof_type {
            Some(ProofType::OpSuccinctSp1ClusterCompressed) => self.stark_receipt.map(|proof| {
                ProtocolProofResult::Compressed(ZkProofResult {
                    zk_vm: ProtocolZkVm::Sp1,
                    proof: proof.into(),
                })
            }),
            Some(ProofType::OpSuccinctSp1ClusterSnarkGroth16) => self.snark_receipt.map(|proof| {
                ProtocolProofResult::SnarkGroth16(SnarkGroth16ProofResult {
                    proof: ZkProofResult { zk_vm: ProtocolZkVm::Sp1, proof: proof.into() },
                })
            }),
            None => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use base_prover_service_protocol::{ProofRequest as ProtocolProofRequest, ProofRequestKind};
    use chrono::Utc;
    use uuid::Uuid;

    use super::*;

    fn compressed_payload(session_id: &str) -> serde_json::Value {
        serde_json::to_value(ProtocolProofRequest {
            session_id: Some(session_id.to_owned()),
            request: ProofRequestKind::Compressed(base_prover_service_protocol::ZkProofRequest {
                start_block_number: 10,
                number_of_blocks_to_prove: 2,
                sequence_window: None,
                l1_head: None,
                intermediate_root_interval: None,
                zk_vm: ProtocolZkVm::Sp1,
            }),
        })
        .expect("protocol request should serialize")
    }

    fn proof_job(request_payload: serde_json::Value) -> ProofJob {
        let now = Utc::now();
        ProofJob {
            id: Uuid::new_v4(),
            session_id: "session-1".to_owned(),
            request_payload,
            api_proof_type: ApiProofType::Compressed,
            zk_vm: Some(ZkVmKind::Sp1),
            tee_kind: None,
            job_status: ProofJobStatus::Claimed,
            attempt: 2,
            worker_id: Some("worker-1".to_owned()),
            lock_id: Some(Uuid::new_v4()),
            lock_expires_at: Some(now),
            claimed_at: Some(now),
            last_heartbeat_at: Some(now),
            error_message: None,
            created_at: now,
            updated_at: now,
            completed_at: None,
        }
    }

    fn proof_request(proof_type: Option<ProofType>) -> ProofRequest {
        let now = Utc::now();
        let id = Uuid::new_v4();
        ProofRequest {
            id,
            session_id: id.to_string(),
            request_payload: serde_json::json!({}),
            api_proof_type: ApiProofType::Compressed,
            zk_vm: Some(ZkVmKind::Sp1),
            tee_kind: None,
            start_block_number: 1,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            proof_type,
            stark_receipt: None,
            snark_receipt: None,
            result_payload: None,
            submitted_by_worker_id: None,
            submitted_lock_id: None,
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
    fn proof_type_round_trips() {
        for proof_type in
            [ProtocolProofType::Compressed, ProtocolProofType::SnarkGroth16, ProtocolProofType::Tee]
        {
            assert_eq!(ProtocolProofType::from(ApiProofType::from(proof_type)), proof_type);
        }
    }

    #[test]
    fn zk_vm_and_tee_kind_round_trip() {
        assert_eq!(ProtocolZkVm::from(ZkVmKind::from(ProtocolZkVm::Sp1)), ProtocolZkVm::Sp1);
        assert_eq!(
            ProtocolTeeKind::from(TeeKind::from(ProtocolTeeKind::AwsNitro)),
            ProtocolTeeKind::AwsNitro
        );
    }

    #[test]
    fn job_status_round_trips() {
        for status in [
            ProtocolProofJobStatus::Pending,
            ProtocolProofJobStatus::Claimed,
            ProtocolProofJobStatus::Succeeded,
            ProtocolProofJobStatus::Failed,
        ] {
            assert_eq!(ProtocolProofJobStatus::from(ProofJobStatus::from(status)), status);
        }
    }

    #[test]
    fn requester_status_collapses_pre_execution_states() {
        assert_eq!(ProtocolProofStatus::from(ProofStatus::Created), ProtocolProofStatus::Queued);
        assert_eq!(ProtocolProofStatus::from(ProofStatus::Pending), ProtocolProofStatus::Queued);
        assert_eq!(ProtocolProofStatus::from(ProofStatus::Running), ProtocolProofStatus::Running);
        assert_eq!(
            ProtocolProofStatus::from(ProofStatus::Succeeded),
            ProtocolProofStatus::Succeeded
        );
        assert_eq!(ProtocolProofStatus::from(ProofStatus::Failed), ProtocolProofStatus::Failed);
    }

    #[test]
    fn status_filter_expands_queued() {
        assert_eq!(
            ProofStatus::matching_filter(ProtocolProofStatus::Queued),
            vec![ProofStatus::Created, ProofStatus::Pending]
        );
        assert_eq!(
            ProofStatus::matching_filter(ProtocolProofStatus::Running),
            vec![ProofStatus::Running]
        );
        assert_eq!(
            ProofStatus::matching_filter(ProtocolProofStatus::Succeeded),
            vec![ProofStatus::Succeeded]
        );
        assert_eq!(
            ProofStatus::matching_filter(ProtocolProofStatus::Failed),
            vec![ProofStatus::Failed]
        );
    }

    #[test]
    fn proof_job_converts_claim_fields() {
        let job = proof_job(compressed_payload("session-1"));
        let lock_id = job.lock_id;
        let protocol = ProtocolProofJob::try_from(job).expect("job should convert");

        assert_eq!(protocol.session_id, "session-1");
        assert_eq!(protocol.status, ProtocolProofJobStatus::Claimed);
        assert_eq!(protocol.attempt, 2);
        assert_eq!(protocol.worker_id.as_deref(), Some("worker-1"));
        assert_eq!(protocol.lock_id, lock_id.map(|id| id.to_string()));
        assert!(matches!(protocol.request.request, ProofRequestKind::Compressed(_)));
    }

    #[test]
    fn proof_job_rejects_invalid_payload() {
        let job = proof_job(serde_json::json!({ "not": "a-request" }));
        let err = ProtocolProofJob::try_from(job).expect_err("invalid payload should fail");

        assert!(matches!(err, ConversionError::InvalidRequestPayload { .. }));
    }

    #[test]
    fn stored_proof_result_prefers_result_payload() {
        let expected = ProtocolProofResult::Compressed(ZkProofResult {
            zk_vm: ProtocolZkVm::Sp1,
            proof: vec![0xAA, 0xBB].into(),
        });
        let mut req = proof_request(Some(ProofType::OpSuccinctSp1ClusterCompressed));
        req.stark_receipt = Some(vec![0xDE, 0xAD]);
        req.result_payload =
            Some(serde_json::to_value(&expected).expect("result should serialize"));

        assert_eq!(req.stored_proof_result().unwrap(), Some(expected));
    }

    #[test]
    fn stored_proof_result_falls_back_to_compressed_receipt() {
        let mut req = proof_request(Some(ProofType::OpSuccinctSp1ClusterCompressed));
        req.stark_receipt = Some(vec![1, 2, 3]);

        assert_eq!(
            req.stored_proof_result().unwrap(),
            Some(ProtocolProofResult::Compressed(ZkProofResult {
                zk_vm: ProtocolZkVm::Sp1,
                proof: vec![1, 2, 3].into(),
            }))
        );
    }

    #[test]
    fn stored_proof_result_falls_back_to_snark_receipt() {
        let mut req = proof_request(Some(ProofType::OpSuccinctSp1ClusterSnarkGroth16));
        req.snark_receipt = Some(vec![4, 5, 6]);

        assert_eq!(
            req.stored_proof_result().unwrap(),
            Some(ProtocolProofResult::SnarkGroth16(SnarkGroth16ProofResult {
                proof: ZkProofResult { zk_vm: ProtocolZkVm::Sp1, proof: vec![4, 5, 6].into() },
            }))
        );
    }

    #[test]
    fn stored_proof_result_deserializes_tee_payload() {
        let tee_payload = serde_json::json!({
            "proof_type": "tee",
            "payload": {
                "aggregate_proposal": {
                    "output_root": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "signature": "0x",
                    "l1_origin_hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "l1_origin_number": 0,
                    "l2_block_number": 0,
                    "prev_output_root": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "config_hash": "0x0000000000000000000000000000000000000000000000000000000000000000"
                },
                "proposals": [],
                "tee_kind": "aws_nitro"
            }
        });
        let mut req = proof_request(None);
        req.result_payload = Some(tee_payload);

        assert!(matches!(req.stored_proof_result().unwrap(), Some(ProtocolProofResult::Tee(_))));
    }

    #[test]
    fn stored_proof_result_is_none_without_data() {
        let req = proof_request(None);
        assert_eq!(req.stored_proof_result().unwrap(), None);
    }
}
