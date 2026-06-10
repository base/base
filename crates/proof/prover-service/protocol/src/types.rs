//! JSON-RPC request and response types for the shared prover service protocol.

use alloy_primitives::{Address, B256, Bytes};
use base_proof_primitives::{ProofRequest as PrimitiveProofRequest, Proposal};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// JSON-RPC error message returned when a proof request session cannot be found.
pub const PROOF_REQUEST_NOT_FOUND_MESSAGE: &str = "Proof request not found";

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
pub struct ProveBlockRangeRequest {
    /// Proof request payload.
    pub proof: ProofRequest,
}

/// Response returned after a prove-block-range request is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProveBlockRangeResponse {
    /// Accepted client-supplied session identifier.
    pub session_id: String,
}

/// Submitted proof request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofRequest {
    /// Client-provided idempotency key.
    pub session_id: String,
    /// Proof request details.
    pub request: ProofRequestKind,
}

/// Concrete proof request variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "proof_type", content = "payload", rename_all = "snake_case")]
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
pub struct SnarkGroth16ProofRequest {
    /// Underlying ZK proof request.
    pub proof: ZkProofRequest,
    /// On-chain prover address.
    pub prover_address: Address,
}

/// Trusted execution environment proof request parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeeProofRequest {
    /// Underlying TEE proof request.
    pub proof: PrimitiveProofRequest,
    /// Trusted execution environment implementation.
    pub tee_kind: TeeKind,
}

/// Proof result payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "proof_type", content = "payload", rename_all = "snake_case")]
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
pub struct ZkProofResult {
    /// ZK virtual machine implementation that produced the proof.
    pub zk_vm: ZkVm,
    /// Serialized proof bytes.
    pub proof: Bytes,
}

/// Groth16 SNARK proof result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnarkGroth16ProofResult {
    /// Wrapped ZK proof result.
    pub proof: ZkProofResult,
}

/// TEE proof result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeeProofResult {
    /// Aggregate proposal covering the entire proven block range.
    pub aggregate_proposal: Proposal,
    /// Individual signed proposals.
    pub proposals: Vec<Proposal>,
    /// Trusted execution environment implementation that produced the proof.
    pub tee_kind: TeeKind,
}

/// Request to fetch proof status and result data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetProofRequest {
    /// Proof session identifier.
    pub session_id: String,
}

/// Proof status and result data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
pub struct ListProofsResponse {
    /// Proof summaries.
    pub proofs: Vec<ProofSummary>,
    /// Total matching proof count.
    pub total_count: u64,
}

/// Worker-owned proof job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
pub struct GetNextProofResponse {
    /// Claimed proof job, if one was available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<ProofJob>,
}

/// Request to extend a proof job lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
pub struct HeartbeatResponse {
    /// Updated proof job.
    pub job: ProofJob,
}

/// Request to submit a proof result for a proof job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
pub struct WorkerSubmitProofResponse {
    /// Completed proof job.
    pub job: ProofJob,
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, address};
    use serde_json::json;

    use super::*;

    #[test]
    fn proof_request_serializes_as_json_rpc_payload() {
        let request = ProveBlockRangeRequest {
            proof: ProofRequest {
                session_id: "proof-session".to_owned(),
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
                    "session_id": "proof-session",
                    "request": {
                        "proof_type": "compressed",
                        "payload": {
                            "start_block_number": 10,
                            "number_of_blocks_to_prove": 20,
                            "l1_head": format!("{:#x}", B256::repeat_byte(0xab)),
                            "intermediate_root_interval": 128,
                            "zk_vm": "sp1"
                        }
                    }
                }
            })
        );
    }

    #[test]
    fn proof_request_requires_session_id() {
        let result = serde_json::from_value::<ProofRequest>(json!({
            "request": {
                "proof_type": "compressed",
                "payload": {
                    "start_block_number": 10,
                    "number_of_blocks_to_prove": 20,
                    "zk_vm": "sp1"
                }
            }
        }));

        assert!(result.is_err());
    }

    #[test]
    fn tee_request_uses_hex_encoded_fixed_values() {
        let request = TeeProofRequest {
            proof: PrimitiveProofRequest {
                l1_head: B256::repeat_byte(1),
                agreed_l2_head_hash: B256::repeat_byte(2),
                agreed_l2_output_root: B256::repeat_byte(3),
                claimed_l2_output_root: B256::repeat_byte(4),
                claimed_l2_block_number: 5,
                proposer: address!("0000000000000000000000000000000000000006"),
                intermediate_block_interval: 7,
                l1_head_number: 8,
                image_hash: B256::repeat_byte(9),
            },
            tee_kind: TeeKind::AwsNitro,
        };

        let value = serde_json::to_value(request).expect("tee request should serialize");

        assert_eq!(
            value,
            json!({
                "proof": {
                    "l1_head": format!("{:#x}", B256::repeat_byte(1)),
                    "agreed_l2_head_hash": format!("{:#x}", B256::repeat_byte(2)),
                    "agreed_l2_output_root": format!("{:#x}", B256::repeat_byte(3)),
                    "claimed_l2_output_root": format!("{:#x}", B256::repeat_byte(4)),
                    "claimed_l2_block_number": 5,
                    "proposer": "0x0000000000000000000000000000000000000006",
                    "intermediate_block_interval": 7,
                    "l1_head_number": 8,
                    "image_hash": format!("{:#x}", B256::repeat_byte(9)),
                },
                "tee_kind": "aws_nitro",
            })
        );
    }

    #[test]
    fn tee_request_requires_tee_kind() {
        let result = serde_json::from_value::<TeeProofRequest>(json!({
            "proof": {
                "l1_head": format!("{:#x}", B256::repeat_byte(1)),
                "agreed_l2_head_hash": format!("{:#x}", B256::repeat_byte(2)),
                "agreed_l2_output_root": format!("{:#x}", B256::repeat_byte(3)),
                "claimed_l2_output_root": format!("{:#x}", B256::repeat_byte(4)),
                "claimed_l2_block_number": 5,
                "proposer": "0x0000000000000000000000000000000000000006",
                "intermediate_block_interval": 7,
                "l1_head_number": 8,
                "image_hash": format!("{:#x}", B256::repeat_byte(9)),
            }
        }));

        assert!(result.is_err());
    }

    #[test]
    fn tee_request_rejects_flat_shape() {
        let result = serde_json::from_value::<TeeProofRequest>(json!({
            "l1_head": format!("{:#x}", B256::repeat_byte(1)),
            "agreed_l2_head_hash": format!("{:#x}", B256::repeat_byte(2)),
            "agreed_l2_output_root": format!("{:#x}", B256::repeat_byte(3)),
            "claimed_l2_output_root": format!("{:#x}", B256::repeat_byte(4)),
            "claimed_l2_block_number": 5,
            "proposer": "0x0000000000000000000000000000000000000006",
            "intermediate_block_interval": 7,
            "l1_head_number": 8,
            "image_hash": format!("{:#x}", B256::repeat_byte(9)),
            "tee_kind": "aws_nitro"
        }));

        assert!(result.is_err());
    }

    #[test]
    fn tee_result_uses_flat_proposal_shape() {
        let aggregate_proposal = tee_proposal(1, 20);
        let proposal = tee_proposal(2, 19);
        let result = ProofResult::Tee(TeeProofResult {
            aggregate_proposal: aggregate_proposal.clone(),
            proposals: vec![proposal.clone()],
            tee_kind: TeeKind::AwsNitro,
        });

        let value = serde_json::to_value(result).expect("tee result should serialize");

        assert_eq!(value["proof_type"], json!("tee"));
        assert_eq!(value["payload"]["tee_kind"], json!("aws_nitro"));
        assert_eq!(
            value["payload"]["aggregate_proposal"],
            json!({
                "output_root": format!("{:#x}", aggregate_proposal.output_root),
                "signature": "0x010101",
                "l1_origin_hash": format!("{:#x}", aggregate_proposal.l1_origin_hash),
                "l1_origin_number": aggregate_proposal.l1_origin_number,
                "l2_block_number": aggregate_proposal.l2_block_number,
                "prev_output_root": format!("{:#x}", aggregate_proposal.prev_output_root),
                "config_hash": format!("{:#x}", aggregate_proposal.config_hash),
            })
        );
        assert_eq!(
            value["payload"]["proposals"][0],
            json!({
                "output_root": format!("{:#x}", proposal.output_root),
                "signature": "0x020202",
                "l1_origin_hash": format!("{:#x}", proposal.l1_origin_hash),
                "l1_origin_number": proposal.l1_origin_number,
                "l2_block_number": proposal.l2_block_number,
                "prev_output_root": format!("{:#x}", proposal.prev_output_root),
                "config_hash": format!("{:#x}", proposal.config_hash),
            })
        );
    }

    #[test]
    fn tee_result_rejects_nested_proof_shape() {
        let aggregate_proposal = tee_proposal(1, 20);
        let result = serde_json::from_value::<ProofResult>(json!({
            "proof_type": "tee",
            "payload": {
                "proof": {
                    "aggregate_proposal": {
                        "output_root": format!("{:#x}", aggregate_proposal.output_root),
                        "signature": "0x010101",
                        "l1_origin_hash": format!("{:#x}", aggregate_proposal.l1_origin_hash),
                        "l1_origin_number": aggregate_proposal.l1_origin_number,
                        "l2_block_number": aggregate_proposal.l2_block_number,
                        "prev_output_root": format!("{:#x}", aggregate_proposal.prev_output_root),
                        "config_hash": format!("{:#x}", aggregate_proposal.config_hash),
                    },
                    "proposals": []
                },
                "tee_kind": "aws_nitro"
            }
        }));

        assert!(result.is_err());
    }

    #[test]
    fn tee_result_requires_aggregate_proposal() {
        let proposal = tee_proposal(2, 19);
        let result = serde_json::from_value::<ProofResult>(json!({
            "proof_type": "tee",
            "payload": {
                "proposals": [{
                    "output_root": format!("{:#x}", proposal.output_root),
                    "signature": "0x020202",
                    "l1_origin_hash": format!("{:#x}", proposal.l1_origin_hash),
                    "l1_origin_number": proposal.l1_origin_number,
                    "l2_block_number": proposal.l2_block_number,
                    "prev_output_root": format!("{:#x}", proposal.prev_output_root),
                    "config_hash": format!("{:#x}", proposal.config_hash),
                }],
                "tee_kind": "aws_nitro"
            }
        }));

        assert!(result.is_err());
    }

    #[test]
    fn tee_proof_payload_uses_hex_encoded_fixed_values() {
        let request = PrimitiveProofRequest {
            l1_head: B256::repeat_byte(1),
            agreed_l2_head_hash: B256::repeat_byte(2),
            agreed_l2_output_root: B256::repeat_byte(3),
            claimed_l2_output_root: B256::repeat_byte(4),
            claimed_l2_block_number: 5,
            proposer: address!("0000000000000000000000000000000000000006"),
            intermediate_block_interval: 7,
            l1_head_number: 8,
            image_hash: B256::repeat_byte(9),
        };

        let value = serde_json::to_value(request).expect("tee request payload should serialize");

        assert_eq!(
            value,
            json!({
                "l1_head": format!("{:#x}", B256::repeat_byte(1)),
                "agreed_l2_head_hash": format!("{:#x}", B256::repeat_byte(2)),
                "agreed_l2_output_root": format!("{:#x}", B256::repeat_byte(3)),
                "claimed_l2_output_root": format!("{:#x}", B256::repeat_byte(4)),
                "claimed_l2_block_number": 5,
                "proposer": "0x0000000000000000000000000000000000000006",
                "intermediate_block_interval": 7,
                "l1_head_number": 8,
                "image_hash": format!("{:#x}", B256::repeat_byte(9)),
            })
        );
    }

    #[test]
    fn tee_proposal_uses_hex_encoded_fixed_values() {
        let aggregate_proposal = tee_proposal(1, 20);
        let proposal = tee_proposal(2, 19);
        let result = TeeProofResult {
            aggregate_proposal: aggregate_proposal.clone(),
            proposals: vec![proposal.clone()],
            tee_kind: TeeKind::AwsNitro,
        };

        let value = serde_json::to_value(result).expect("tee result payload should serialize");

        assert_eq!(
            value["aggregate_proposal"],
            json!({
                "output_root": format!("{:#x}", aggregate_proposal.output_root),
                "signature": "0x010101",
                "l1_origin_hash": format!("{:#x}", aggregate_proposal.l1_origin_hash),
                "l1_origin_number": aggregate_proposal.l1_origin_number,
                "l2_block_number": aggregate_proposal.l2_block_number,
                "prev_output_root": format!("{:#x}", aggregate_proposal.prev_output_root),
                "config_hash": format!("{:#x}", aggregate_proposal.config_hash),
            })
        );
        assert_eq!(
            value["proposals"][0],
            json!({
                "output_root": format!("{:#x}", proposal.output_root),
                "signature": "0x020202",
                "l1_origin_hash": format!("{:#x}", proposal.l1_origin_hash),
                "l1_origin_number": proposal.l1_origin_number,
                "l2_block_number": proposal.l2_block_number,
                "prev_output_root": format!("{:#x}", proposal.prev_output_root),
                "config_hash": format!("{:#x}", proposal.config_hash),
            })
        );
    }

    #[test]
    fn omitted_optional_fields_deserialize_to_none() {
        let request: ZkProofRequest = serde_json::from_value(json!({
            "start_block_number": 10,
            "number_of_blocks_to_prove": 20,
            "zk_vm": "sp1"
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
            "start_block_number": 10,
            "number_of_blocks_to_prove": 20,
            "l1_head": "0xabc",
            "zk_vm": "sp1"
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
                "worker_id": "worker-1",
                "proof_type": "compressed",
                "tee_kinds": [],
                "zk_vms": ["sp1"],
                "lock_duration_seconds": 30
            })
        );
    }

    #[test]
    fn get_next_proof_request_defaults_omitted_capability_lists() {
        let request: GetNextProofRequest = serde_json::from_value(json!({
            "worker_id": "worker-1",
            "proof_type": "compressed",
            "lock_duration_seconds": 30
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
        assert_eq!(heartbeat_value["lock_id"], json!("lock-1"));
        assert!(
            serde_json::from_value::<HeartbeatRequest>(json!({
                "session_id": "session-1",
                "worker_id": "worker-1",
                "lock_duration_seconds": 30,
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
        assert_eq!(submit_value["lock_id"], json!("lock-1"));
        assert!(
            serde_json::from_value::<WorkerSubmitProofRequest>(json!({
                "session_id": "session-1",
                "worker_id": "worker-1",
                "result": {
                    "proof_type": "compressed",
                    "payload": {
                        "zk_vm": "sp1",
                        "proof": "0x010203"
                    }
                }
            }))
            .is_err()
        );
    }

    fn tee_proposal(byte: u8, l2_block_number: u64) -> Proposal {
        Proposal {
            output_root: B256::repeat_byte(byte),
            signature: Bytes::from(vec![byte; 3]),
            l1_origin_hash: B256::repeat_byte(byte + 1),
            l1_origin_number: u64::from(byte) + 100,
            l2_block_number,
            prev_output_root: B256::repeat_byte(byte + 2),
            config_hash: B256::repeat_byte(byte + 3),
        }
    }
}
