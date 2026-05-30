//! JSON-RPC request and response types for the shared prover service protocol.

use alloy_primitives::{Address, B256, Bytes};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Type of proof requested from the prover service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofType {
    /// Compressed ZK proof.
    Compressed,
    /// Groth16 SNARK proof.
    SnarkGroth16,
    /// Trusted execution environment proof.
    Tee,
}

/// Trusted execution environment implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeeKind {
    /// AWS Nitro Enclaves.
    AwsNitro,
}

/// ZK virtual machine implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZkVm {
    /// Succinct SP1.
    Sp1,
}

/// Status of a submitted proof request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofStatus {
    /// Proof request is queued.
    Queued,
    /// Proof request is running.
    Running,
    /// Proof request completed successfully.
    Succeeded,
    /// Proof request failed.
    Failed,
}

/// Status of a worker-owned proof job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofJobStatus {
    /// Proof job is pending and not currently claimed by a worker.
    Pending,
    /// Proof job is currently claimed by a worker.
    Claimed,
    /// Proof job completed successfully.
    Succeeded,
    /// Proof job failed.
    Failed,
}

/// Request to prove a block range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProveBlockRangeRequest {
    /// Proof request payload.
    pub proof: ProofRequest,
}

/// Response returned after a prove-block-range request is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProveBlockRangeResponse {
    /// Server-assigned or client-supplied session identifier.
    pub session_id: String,
}

/// Submitted proof request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofRequest {
    /// Optional client-provided idempotency key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Proof request details.
    pub request: ProofRequestKind,
}

/// Concrete proof request variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "proofType", content = "payload", rename_all = "snake_case")]
pub enum ProofRequestKind {
    /// Request a compressed ZK proof.
    Compressed(ZkProofRequest),
    /// Request a Groth16 SNARK proof.
    SnarkGroth16(SnarkGroth16ProofRequest),
    /// Request a TEE proof.
    Tee(TeeProofRequest),
}

/// ZK proof request parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ZkProofRequest {
    /// First L2 block number to prove.
    pub start_block_number: u64,
    /// Number of consecutive L2 blocks to prove.
    pub number_of_blocks_to_prove: u64,
    /// Optional sequencing window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence_window: Option<u64>,
    /// Optional L1 head hash used for witness generation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l1_head: Option<B256>,
    /// Optional intermediate output root interval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intermediate_root_interval: Option<u64>,
    /// ZK virtual machine implementation to use.
    pub zk_vm: ZkVm,
}

/// Groth16 SNARK proof request parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnarkGroth16ProofRequest {
    /// Underlying ZK proof request.
    pub proof: ZkProofRequest,
    /// On-chain prover address.
    pub prover_address: Address,
}

/// Trusted execution environment proof request parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeeProofRequest {
    /// L1 head hash.
    pub l1_head: B256,
    /// Agreed L2 head block hash.
    pub agreed_l2_head_hash: B256,
    /// Agreed L2 output root.
    pub agreed_l2_output_root: B256,
    /// Claimed L2 output root.
    pub claimed_l2_output_root: B256,
    /// Claimed L2 block number.
    pub claimed_l2_block_number: u64,
    /// Proposal submitter address.
    pub proposer: Address,
    /// Intermediate block interval.
    pub intermediate_block_interval: u64,
    /// L1 head block number.
    pub l1_head_number: u64,
    /// TEE image hash.
    pub image_hash: B256,
    /// Trusted execution environment implementation.
    pub tee_kind: TeeKind,
}

/// Proof result payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "proofType", content = "payload", rename_all = "snake_case")]
pub enum ProofResult {
    /// Compressed ZK proof result.
    Compressed(ZkProofResult),
    /// Groth16 SNARK proof result.
    SnarkGroth16(SnarkGroth16ProofResult),
    /// TEE proof result.
    Tee(TeeProofResult),
}

/// ZK proof result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ZkProofResult {
    /// ZK virtual machine implementation that produced the proof.
    pub zk_vm: ZkVm,
    /// Serialized proof bytes.
    pub proof: Bytes,
}

/// Groth16 SNARK proof result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnarkGroth16ProofResult {
    /// Wrapped ZK proof result.
    pub proof: ZkProofResult,
}

/// TEE proof result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeeProofResult {
    /// Aggregate proposal, if one was produced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregate_proposal: Option<TeeProposal>,
    /// Individual signed proposals.
    pub proposals: Vec<TeeProposal>,
    /// Trusted execution environment implementation that produced the proof.
    pub tee_kind: TeeKind,
}

/// Signed TEE proposal data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeeProposal {
    /// Output root.
    pub output_root: B256,
    /// Serialized signature bytes.
    pub signature: Bytes,
    /// L1 origin block hash.
    pub l1_origin_hash: B256,
    /// L1 origin block number.
    pub l1_origin_number: u64,
    /// L2 block number.
    pub l2_block_number: u64,
    /// Previous output root.
    pub prev_output_root: B256,
    /// Rollup config hash.
    pub config_hash: B256,
}

/// Request to fetch proof status and result data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetProofRequest {
    /// Proof session identifier.
    pub session_id: String,
}

/// Proof status and result data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetProofResponse {
    /// Current proof status.
    pub status: ProofStatus,
    /// Error message when the proof failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Proof result, present only after the proof succeeds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ProofResult>,
}

/// Request to list submitted proofs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListProofsRequest {
    /// Number of rows to skip.
    pub offset: u64,
    /// Maximum rows to return. Zero uses the server default.
    pub limit: u32,
    /// Optional status filter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_filter: Option<ProofStatus>,
}

/// Summary of a submitted proof request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofSummary {
    /// Proof session identifier.
    pub session_id: String,
    /// Proof type.
    pub proof_type: ProofType,
    /// Current proof status.
    pub status: ProofStatus,
    /// Timestamp when the proof request was created.
    pub created_at: DateTime<Utc>,
    /// Timestamp when the proof request was last updated.
    pub updated_at: DateTime<Utc>,
    /// Timestamp when the proof request completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    /// Error message when the proof failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// TEE implementation for TEE proofs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tee_kind: Option<TeeKind>,
    /// ZK virtual machine for ZK proofs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zk_vm: Option<ZkVm>,
}

/// Response containing proof summaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListProofsResponse {
    /// Proof summaries.
    pub proofs: Vec<ProofSummary>,
    /// Total matching proof count.
    pub total_count: u64,
}

/// Worker-owned proof job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofJob {
    /// Proof session identifier.
    pub session_id: String,
    /// Current proof job status.
    pub status: ProofJobStatus,
    /// Submitted proof request.
    pub request: ProofRequest,
    /// Current attempt number.
    pub attempt: u32,
    /// Server-issued lock identifier, if the job is claimed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_id: Option<String>,
    /// Worker identifier, if the job is claimed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    /// Timestamp when the worker claim expires.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_expires_at: Option<DateTime<Utc>>,
    /// Timestamp when the job was created.
    pub created_at: DateTime<Utc>,
    /// Timestamp when the job was last updated.
    pub updated_at: DateTime<Utc>,
    /// Timestamp when the job completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    /// Error message when the job failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// Request to claim the next available proof job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetNextProofRequest {
    /// Worker identifier.
    pub worker_id: String,
    /// Proof type this worker can execute.
    pub proof_type: ProofType,
    /// TEE implementations this worker can execute for TEE proofs.
    #[serde(default)]
    pub tee_kinds: Vec<TeeKind>,
    /// ZK virtual machines this worker can execute for ZK proofs.
    #[serde(default)]
    pub zk_vms: Vec<ZkVm>,
    /// Requested lock duration in seconds. Zero uses the server default.
    pub lock_duration_seconds: u32,
}

/// Response returned when a worker claims the next proof job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetNextProofResponse {
    /// Claimed proof job, if one was available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<ProofJob>,
}

/// Request to extend a proof job lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatRequest {
    /// Proof session identifier.
    pub session_id: String,
    /// Server-issued lock identifier for this worker claim.
    pub lock_id: String,
    /// Worker identifier.
    pub worker_id: String,
    /// Requested lock duration in seconds. Zero uses the server default.
    pub lock_duration_seconds: u32,
}

/// Response returned after a proof job heartbeat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatResponse {
    /// Updated proof job.
    pub job: ProofJob,
}

/// Request to submit a proof result for a proof job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerSubmitProofRequest {
    /// Proof session identifier.
    pub session_id: String,
    /// Server-issued lock identifier for this worker claim.
    pub lock_id: String,
    /// Worker identifier.
    pub worker_id: String,
    /// Proof result.
    pub result: ProofResult,
}

/// Response returned after a worker proof submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerSubmitProofResponse {
    /// Completed proof job.
    pub job: ProofJob,
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use serde_json::json;

    use super::*;

    #[test]
    fn proof_request_serializes_as_json_rpc_payload() {
        let request = ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id: Some("proof-session".to_owned()),
                request: ProofRequestKind::Compressed(ZkProofRequest {
                    start_block_number: 10,
                    number_of_blocks_to_prove: 20,
                    sequence_window: None,
                    l1_head: Some(B256::repeat_byte(0xab)),
                    intermediate_root_interval: Some(128),
                    zk_vm: ZkVm::Sp1,
                }),
            },
        };

        let value = serde_json::to_value(request).expect("proof request should serialize");

        assert_eq!(
            value,
            json!({
                "proof": {
                    "sessionId": "proof-session",
                    "request": {
                        "proofType": "compressed",
                        "payload": {
                            "startBlockNumber": 10,
                            "numberOfBlocksToProve": 20,
                            "l1Head": format!("{:#x}", B256::repeat_byte(0xab)),
                            "intermediateRootInterval": 128,
                            "zkVm": "sp1"
                        }
                    }
                }
            })
        );
    }

    #[test]
    fn tee_request_uses_hex_encoded_fixed_values() {
        let request = TeeProofRequest {
            l1_head: B256::repeat_byte(1),
            agreed_l2_head_hash: B256::repeat_byte(2),
            agreed_l2_output_root: B256::repeat_byte(3),
            claimed_l2_output_root: B256::repeat_byte(4),
            claimed_l2_block_number: 5,
            proposer: address!("0000000000000000000000000000000000000006"),
            intermediate_block_interval: 7,
            l1_head_number: 8,
            image_hash: B256::repeat_byte(9),
            tee_kind: TeeKind::AwsNitro,
        };

        let value = serde_json::to_value(request).expect("tee request should serialize");

        assert_eq!(value["l1Head"], json!(format!("{:#x}", B256::repeat_byte(1))));
        assert_eq!(value["proposer"], json!("0x0000000000000000000000000000000000000006"));
        assert_eq!(value["teeKind"], json!("aws_nitro"));
    }

    #[test]
    fn omitted_optional_fields_deserialize_to_none() {
        let request: ZkProofRequest = serde_json::from_value(json!({
            "startBlockNumber": 10,
            "numberOfBlocksToProve": 20,
            "zkVm": "sp1"
        }))
        .expect("zk request should accept omitted optional fields");

        assert_eq!(
            request,
            ZkProofRequest {
                start_block_number: 10,
                number_of_blocks_to_prove: 20,
                sequence_window: None,
                l1_head: None,
                intermediate_root_interval: None,
                zk_vm: ZkVm::Sp1,
            }
        );
    }

    #[test]
    fn zk_l1_head_rejects_malformed_hash() {
        let result = serde_json::from_value::<ZkProofRequest>(json!({
            "startBlockNumber": 10,
            "numberOfBlocksToProve": 20,
            "l1Head": "0xabc",
            "zkVm": "sp1"
        }));

        assert!(result.is_err());
    }

    #[test]
    fn get_next_proof_request_serializes_as_worker_claim() {
        let request = GetNextProofRequest {
            worker_id: "worker-1".to_owned(),
            proof_type: ProofType::Compressed,
            tee_kinds: Vec::new(),
            zk_vms: vec![ZkVm::Sp1],
            lock_duration_seconds: 30,
        };

        let value = serde_json::to_value(request).expect("get next proof request should serialize");

        assert_eq!(
            value,
            json!({
                "workerId": "worker-1",
                "proofType": "compressed",
                "teeKinds": [],
                "zkVms": ["sp1"],
                "lockDurationSeconds": 30
            })
        );
    }

    #[test]
    fn get_next_proof_request_defaults_omitted_capability_lists() {
        let request: GetNextProofRequest = serde_json::from_value(json!({
            "workerId": "worker-1",
            "proofType": "compressed",
            "lockDurationSeconds": 30
        }))
        .expect("get next proof request should accept omitted capability lists");

        assert_eq!(request.tee_kinds, Vec::new());
        assert_eq!(request.zk_vms, Vec::new());
    }

    #[test]
    fn worker_ownership_requests_require_lock_fencing_token() {
        let heartbeat = HeartbeatRequest {
            session_id: "session-1".to_owned(),
            lock_id: "lock-1".to_owned(),
            worker_id: "worker-1".to_owned(),
            lock_duration_seconds: 30,
        };

        let heartbeat_value = serde_json::to_value(heartbeat).expect("heartbeat should serialize");
        assert_eq!(heartbeat_value["lockId"], json!("lock-1"));
        assert!(
            serde_json::from_value::<HeartbeatRequest>(json!({
                "sessionId": "session-1",
                "workerId": "worker-1",
                "lockDurationSeconds": 30,
            }))
            .is_err()
        );

        let submit = WorkerSubmitProofRequest {
            session_id: "session-1".to_owned(),
            lock_id: "lock-1".to_owned(),
            worker_id: "worker-1".to_owned(),
            result: ProofResult::Compressed(ZkProofResult {
                zk_vm: ZkVm::Sp1,
                proof: vec![1, 2, 3].into(),
            }),
        };

        let submit_value = serde_json::to_value(submit).expect("submit should serialize");
        assert_eq!(submit_value["lockId"], json!("lock-1"));
        assert!(
            serde_json::from_value::<WorkerSubmitProofRequest>(json!({
                "sessionId": "session-1",
                "workerId": "worker-1",
                "result": {
                    "proofType": "compressed",
                    "payload": {
                        "zkVm": "sp1",
                        "proof": "0x010203"
                    }
                }
            }))
            .is_err()
        );
    }
}
