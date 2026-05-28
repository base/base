//! JSON-RPC request and response types for the shared prover service.

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
    /// Proof job is queued.
    Queued,
    /// Proof job is currently leased by a worker.
    Leased,
    /// Proof job lease expired before completion.
    LeaseExpired,
    /// Proof job completed successfully.
    Succeeded,
    /// Proof job failed.
    Failed,
}

/// Request to submit a proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitProofRequest {
    /// Proof request payload.
    pub proof: ProofRequest,
}

/// Response returned after a proof request is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitProofResponse {
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
    /// Current lease identifier, if the job is leased.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<String>,
    /// Worker identifier, if the job is leased.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    /// Timestamp when the lease expires.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_expires_at: Option<DateTime<Utc>>,
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

/// Request to fetch a proof job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetProofJobRequest {
    /// Proof session identifier.
    pub session_id: String,
}

/// Proof job response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetProofJobResponse {
    /// Matching proof job, if it exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<ProofJob>,
}

/// Request to claim a proof job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimProofJobRequest {
    /// Proof type the worker can process.
    pub proof_type: ProofType,
    /// Worker identifier.
    pub worker_id: String,
    /// Requested lease duration in seconds. Zero uses the server default.
    pub lease_duration_seconds: u32,
    /// Optional TEE implementation restrictions.
    #[serde(default)]
    pub tee_kinds: Vec<TeeKind>,
    /// Optional ZK virtual machine restrictions.
    #[serde(default)]
    pub zk_vms: Vec<ZkVm>,
}

/// Response returned when a worker attempts to claim a proof job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimProofJobResponse {
    /// Whether a job was claimed.
    pub claimed: bool,
    /// Claimed proof job, if one was available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<ProofJob>,
}

/// Request to extend a proof job lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatProofJobRequest {
    /// Proof session identifier.
    pub session_id: String,
    /// Lease identifier.
    pub lease_id: String,
    /// Worker identifier.
    pub worker_id: String,
    /// Requested lease duration in seconds. Zero uses the server default.
    pub lease_duration_seconds: u32,
}

/// Response returned after a proof job heartbeat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatProofJobResponse {
    /// Whether the heartbeat was accepted.
    pub accepted: bool,
    /// Updated proof job, if accepted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<ProofJob>,
}

/// Request to complete a proof job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteProofJobRequest {
    /// Proof session identifier.
    pub session_id: String,
    /// Lease identifier.
    pub lease_id: String,
    /// Worker identifier.
    pub worker_id: String,
    /// Proof result.
    pub result: ProofResult,
}

/// Response returned after a proof job is completed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteProofJobResponse {
    /// Completed proof job.
    pub job: ProofJob,
}

/// Request to fail a proof job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailProofJobRequest {
    /// Proof session identifier.
    pub session_id: String,
    /// Lease identifier.
    pub lease_id: String,
    /// Worker identifier.
    pub worker_id: String,
    /// Failure reason.
    pub error_message: String,
    /// Whether the job should be retried.
    pub retryable: bool,
}

/// Response returned after a proof job is failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailProofJobResponse {
    /// Updated proof job.
    pub job: ProofJob,
    /// Whether the server will retry the job.
    pub will_retry: bool,
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use serde_json::json;

    use super::*;

    #[test]
    fn proof_request_serializes_as_json_rpc_payload() {
        let request = SubmitProofRequest {
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
    fn claim_request_defaults_omitted_filters_to_empty_lists() {
        let request: ClaimProofJobRequest = serde_json::from_value(json!({
            "proofType": "compressed",
            "workerId": "worker-1",
            "leaseDurationSeconds": 30
        }))
        .expect("claim request should accept omitted optional filters");

        assert_eq!(
            request,
            ClaimProofJobRequest {
                proof_type: ProofType::Compressed,
                worker_id: "worker-1".to_owned(),
                lease_duration_seconds: 30,
                tee_kinds: Vec::new(),
                zk_vms: Vec::new(),
            }
        );
    }
}
