use std::convert::TryFrom;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

/// Status of a proof request
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "VARCHAR", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProofStatus {
    /// Proof request has been created but not yet queued.
    Created,
    /// Proof request is queued and awaiting processing.
    Pending,
    /// Proof is actively being generated.
    Running,
    /// Proof generation completed successfully.
    Succeeded,
    /// Proof generation failed.
    Failed,
}

impl ProofStatus {
    /// Convert enum to static string representation
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "CREATED",
            Self::Pending => "PENDING",
            Self::Running => "RUNNING",
            Self::Succeeded => "SUCCEEDED",
            Self::Failed => "FAILED",
        }
    }
}

impl std::fmt::Display for ProofStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl TryFrom<&str> for ProofStatus {
    type Error = String;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "CREATED" => Ok(Self::Created),
            "PENDING" => Ok(Self::Pending),
            "RUNNING" => Ok(Self::Running),
            "SUCCEEDED" => Ok(Self::Succeeded),
            "FAILED" => Ok(Self::Failed),
            other => Err(format!("Unknown proof status: {other}")),
        }
    }
}

/// Status of an individual proof session (STARK or SNARK)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "VARCHAR", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SessionStatus {
    /// Backend session is actively running.
    Running,
    /// Backend session completed successfully.
    Completed,
    /// Backend session failed.
    Failed,
}

impl SessionStatus {
    /// Convert enum to static string representation
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "RUNNING",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
        }
    }
}

impl std::fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl TryFrom<&str> for SessionStatus {
    type Error = String;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "RUNNING" => Ok(Self::Running),
            "COMPLETED" => Ok(Self::Completed),
            "FAILED" => Ok(Self::Failed),
            other => Err(format!("Unknown session status: {other}")),
        }
    }
}

/// Type of proof session
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "VARCHAR", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SessionType {
    /// STARK proof session.
    Stark,
    /// SNARK proof session.
    Snark,
}

impl SessionType {
    /// Convert enum to static string representation
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Stark => "STARK",
            Self::Snark => "SNARK",
        }
    }
}

impl std::fmt::Display for SessionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl TryFrom<&str> for SessionType {
    type Error = String;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "STARK" => Ok(Self::Stark),
            "SNARK" => Ok(Self::Snark),
            other => Err(format!("Unknown session type: {other}")),
        }
    }
}

/// Type of proof that determines success criteria
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "VARCHAR")]
pub enum ProofType {
    /// Compressed proof generated via OP-Succinct SP1 cluster.
    #[sqlx(rename = "op_succinct_sp1_cluster_compressed")]
    OpSuccinctSp1ClusterCompressed,
    /// SNARK Groth16 proof generated via OP-Succinct SP1 cluster.
    #[sqlx(rename = "op_succinct_sp1_cluster_snark_groth16")]
    OpSuccinctSp1ClusterSnarkGroth16,
}

impl ProofType {
    /// Proto discriminant for `PROOF_TYPE_COMPRESSED`.
    pub const PROTO_COMPRESSED: i32 = 3;
    /// Proto discriminant for `PROOF_TYPE_SNARK_GROTH16`.
    pub const PROTO_SNARK_GROTH16: i32 = 4;

    /// Returns the proto wire value for this proof type.
    pub const fn proto_i32(&self) -> i32 {
        match self {
            Self::OpSuccinctSp1ClusterCompressed => Self::PROTO_COMPRESSED,
            Self::OpSuccinctSp1ClusterSnarkGroth16 => Self::PROTO_SNARK_GROTH16,
        }
    }

    /// Convert enum to static string representation
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::OpSuccinctSp1ClusterCompressed => "op_succinct_sp1_cluster_compressed",
            Self::OpSuccinctSp1ClusterSnarkGroth16 => "op_succinct_sp1_cluster_snark_groth16",
        }
    }
}

impl std::fmt::Display for ProofType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl TryFrom<&str> for ProofType {
    type Error = String;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "op_succinct_sp1_cluster_compressed" => Ok(Self::OpSuccinctSp1ClusterCompressed),
            "op_succinct_sp1_cluster_snark_groth16" => Ok(Self::OpSuccinctSp1ClusterSnarkGroth16),
            other => Err(format!("Unknown proof type: {other}")),
        }
    }
}

/// Convert from proto proof type integer to `ProofType`
impl TryFrom<i32> for ProofType {
    type Error = String;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            Self::PROTO_COMPRESSED => Ok(Self::OpSuccinctSp1ClusterCompressed),
            Self::PROTO_SNARK_GROTH16 => Ok(Self::OpSuccinctSp1ClusterSnarkGroth16),
            _ => Err(format!("Unknown proof type: {value}")),
        }
    }
}

/// A proof request record in the database
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ProofRequest {
    /// Unique identifier.
    pub id: Uuid,
    /// Starting L2 block number.
    pub start_block_number: i64,
    /// Number of consecutive blocks to prove.
    pub number_of_blocks_to_prove: i64,
    /// Optional sequence window for the proof range.
    pub sequence_window: Option<i64>,
    /// Type of proof to generate.
    pub proof_type: ProofType,
    /// Raw STARK receipt bytes, if available.
    pub stark_receipt: Option<Vec<u8>>,
    /// Raw SNARK receipt bytes, if available.
    pub snark_receipt: Option<Vec<u8>>,
    /// Current proof status.
    pub status: ProofStatus,
    /// Error message if the proof failed.
    pub error_message: Option<String>,
    /// Ethereum address of the on-chain prover.
    pub prover_address: Option<String>,
    /// Explicit L1 head hash used for witness generation.
    pub l1_head: Option<String>,
    /// Timestamp when the request was created.
    pub created_at: DateTime<Utc>,
    /// Timestamp of the last status update.
    pub updated_at: DateTime<Utc>,
    /// Timestamp when the proof completed (success or failure).
    pub completed_at: Option<DateTime<Utc>>,
}

/// A proof session record tracking a specific backend job (STARK or SNARK)
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ProofSession {
    /// Auto-incrementing session identifier.
    pub id: i64,
    /// Parent proof request identifier.
    pub proof_request_id: Uuid,
    /// Whether this session produces a STARK or SNARK proof.
    pub session_type: SessionType,
    /// Backend-assigned session identifier.
    pub backend_session_id: String,
    /// Current session status.
    pub status: SessionStatus,
    /// Error message if the session failed.
    pub error_message: Option<String>,
    /// Backend-specific metadata (JSON).
    pub metadata: Option<serde_json::Value>,
    /// Timestamp when the session was created.
    pub created_at: DateTime<Utc>,
    /// Timestamp when the session completed.
    pub completed_at: Option<DateTime<Utc>>,
}

/// Parameters for creating a new proof request
#[derive(Debug, Clone)]
pub struct CreateProofRequest {
    /// Starting L2 block number.
    pub start_block_number: u64,
    /// Number of consecutive blocks to prove.
    pub number_of_blocks_to_prove: u64,
    /// Optional sequence window.
    pub sequence_window: Option<u64>,
    /// Type of proof to generate.
    pub proof_type: ProofType,
    /// Client-provided UUID for idempotent requests.
    pub session_id: Option<Uuid>,
    /// Ethereum address of the on-chain prover (required for SNARK Groth16 proofs).
    pub prover_address: Option<String>,
    /// Explicit L1 head hash for witness generation.
    pub l1_head: Option<String>,
}

/// Parameters for creating a new proof session
#[derive(Debug, Clone)]
pub struct CreateProofSession {
    /// Parent proof request identifier.
    pub proof_request_id: Uuid,
    /// Whether this is a STARK or SNARK session.
    pub session_type: SessionType,
    /// Backend-assigned session identifier.
    pub backend_session_id: String,
    /// Backend-specific metadata (JSON).
    pub metadata: Option<serde_json::Value>,
}

/// Parameters for updating a proof session status
#[derive(Debug, Clone)]
pub struct UpdateProofSession {
    /// Backend-assigned session identifier to look up.
    pub backend_session_id: String,
    /// New session status.
    pub status: SessionStatus,
    /// Error message, if the session failed.
    pub error_message: Option<String>,
    /// Updated backend metadata (JSON).
    pub metadata: Option<serde_json::Value>,
}

/// Parameters for updating a proof request with receipt
#[derive(Debug, Clone)]
pub struct UpdateReceipt {
    /// Proof request identifier.
    pub id: Uuid,
    /// Raw STARK receipt bytes.
    pub stark_receipt: Option<Vec<u8>>,
    /// Raw SNARK receipt bytes.
    pub snark_receipt: Option<Vec<u8>>,
    /// New proof status.
    pub status: ProofStatus,
    /// Error message, if the proof failed.
    pub error_message: Option<String>,
}

/// Outbox entry for reliable task processing
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct OutboxEntry {
    /// Auto-incrementing sequence identifier (FIFO ordering).
    pub sequence_id: i64,
    /// Associated proof request identifier.
    pub proof_request_id: Uuid,
    /// Serialized proof request parameters (JSON).
    pub request_params: serde_json::Value,
    /// Whether this entry has been processed.
    pub processed: bool,
    /// Timestamp when the entry was processed.
    pub processed_at: Option<DateTime<Utc>>,
    /// Number of times processing has been retried.
    pub retry_count: i32,
    /// Error from the most recent processing attempt.
    pub last_error: Option<String>,
    /// Timestamp when the entry was created.
    pub created_at: DateTime<Utc>,
}

/// Parameters for creating an outbox entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateOutboxEntry {
    /// Associated proof request identifier.
    pub proof_request_id: Uuid,
    /// Serialized proof request parameters (JSON).
    pub request_params: serde_json::Value,
}

/// Parameters for marking an outbox entry as processed
#[derive(Debug, Clone)]
pub struct MarkOutboxProcessed {
    /// Sequence identifier of the outbox entry to mark.
    pub sequence_id: i64,
}

/// Parameters for recording a processing error
#[derive(Debug, Clone)]
pub struct MarkOutboxError {
    /// Sequence identifier of the outbox entry.
    pub sequence_id: i64,
    /// Error message from the failed processing attempt.
    pub error_message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proof_type_try_from_proto() {
        assert_eq!(ProofType::try_from(3).unwrap(), ProofType::OpSuccinctSp1ClusterCompressed);
        assert_eq!(ProofType::try_from(4).unwrap(), ProofType::OpSuccinctSp1ClusterSnarkGroth16);

        assert!(ProofType::try_from(0).is_err());
        assert!(ProofType::try_from(1).is_err());
        assert!(ProofType::try_from(2).is_err());
        assert!(ProofType::try_from(5).is_err());
    }
}
