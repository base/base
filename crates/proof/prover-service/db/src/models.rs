use std::convert::TryFrom;

use base_prover_service_protocol::{
    ProofRequest as ProtocolProofRequest, ProofRequestKind as ProtocolProofRequestKind,
    ProofResult as ProtocolProofResult, TeeKind as ProtocolTeeKind, ZkVm as ProtocolZkVm,
};
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

/// Worker-owned job lifecycle status, distinct from requester [`ProofStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "VARCHAR", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProofJobStatus {
    /// Job is claimable and not currently owned by any worker.
    Pending,
    /// Job is currently claimed by a worker under an unexpired lock.
    Claimed,
    /// Job completed successfully through the worker API.
    Succeeded,
    /// Job failed terminally.
    Failed,
}

impl ProofJobStatus {
    /// Convert enum to static string representation.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Claimed => "CLAIMED",
            Self::Succeeded => "SUCCEEDED",
            Self::Failed => "FAILED",
        }
    }
}

impl std::fmt::Display for ProofJobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl TryFrom<&str> for ProofJobStatus {
    type Error = String;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "PENDING" => Ok(Self::Pending),
            "CLAIMED" => Ok(Self::Claimed),
            "SUCCEEDED" => Ok(Self::Succeeded),
            "FAILED" => Ok(Self::Failed),
            other => Err(format!("Unknown proof job status: {other}")),
        }
    }
}

/// Status of an individual proof session (STARK or SNARK)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "VARCHAR", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SessionStatus {
    /// Reservation placeholder before the backend job has been submitted. The row holds a
    /// synthetic `backend_session_id` so the partial unique index serializes concurrent
    /// reservations; sync loops skip it because they only poll RUNNING rows.
    Submitting,
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
            Self::Submitting => "SUBMITTING",
            Self::Running => "RUNNING",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
        }
    }

    /// Whether this status represents a terminal backend session.
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
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
            "SUBMITTING" => Ok(Self::Submitting),
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

/// Outcome of attempting to retry or fail a stuck proof request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOutcome {
    /// Request was reset to CREATED with incremented `retry_count`.
    Retried,
    /// Request was permanently marked FAILED (max retries exceeded).
    PermanentlyFailed,
    /// Request was no longer in PENDING state (already claimed or transitioned).
    Skipped,
}

/// Outcome of creating or replaying a proof request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateProofRequestOutcome {
    /// A new proof request row was inserted.
    Created(Uuid),
    /// An existing terminal `FAILED` row was reset to `CREATED` and a fresh
    /// worker job was made claimable again.
    Requeued(Uuid),
    /// An existing non-terminal or `SUCCEEDED` row was returned unchanged for
    /// idempotent replay.
    Replayed(Uuid),
    /// An existing terminal `FAILED` row is at the retry cap; no requeue.
    RetryExhausted(Uuid),
}

impl CreateProofRequestOutcome {
    /// Returns the proof request UUID regardless of outcome variant.
    pub const fn id(&self) -> Uuid {
        match self {
            Self::Created(id)
            | Self::Requeued(id)
            | Self::Replayed(id)
            | Self::RetryExhausted(id) => *id,
        }
    }
}

/// Errors returned while creating proof requests.
#[derive(Debug, thiserror::Error)]
pub enum CreateProofRequestError {
    /// Request fields are not a supported protocol/backend combination.
    #[error(transparent)]
    Validation(#[from] CreateProofRequestValidationError),
    /// Persisted row disagrees with the new request for this `session_id`.
    #[error(
        "session_id {id} already exists with a different {field} \
         (existing request parameters do not match the new request)"
    )]
    IdCollision {
        /// Conflicting proof request UUID.
        id: Uuid,
        /// Name of the first mismatched field. Stable across runs.
        field: &'static str,
    },
    /// Conflicting row disappeared after insert conflict; safe to retry.
    #[error("session_id {id}: proof request row missing after insert conflict; retry prove_block")]
    SessionRowMissingAfterConflict {
        /// Proof request UUID that was expected to exist.
        id: Uuid,
    },
    /// Underlying database error.
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

/// Validation errors for protocol-facing proof request creation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CreateProofRequestValidationError {
    /// A caller supplied an empty protocol session identifier.
    #[error("session_id must not be empty")]
    EmptySessionId,
    /// The explicit session id and the session id embedded in the payload disagree.
    #[error("session_id disagrees with request_payload.session_id")]
    SessionIdMismatch,
    /// A request field disagrees with the canonical value derived from `request_payload`.
    #[error("field {field} disagrees with request_payload")]
    FieldMismatch {
        /// Name of the mismatched field.
        field: &'static str,
    },
    /// A numeric request field cannot fit in the database representation.
    #[error("field {field} exceeds database range")]
    ValueOutOfRange {
        /// Name of the out-of-range field.
        field: &'static str,
    },
    /// The protocol request payload could not be serialized for storage.
    #[error("failed to serialize request_payload")]
    RequestPayloadSerialization,
    /// A backend proof type is required for the requested protocol proof type.
    #[error("missing backend proof_type for {api_proof_type}")]
    MissingBackendProofType {
        /// Protocol proof type that needs a backend proof type.
        api_proof_type: ApiProofType,
    },
    /// A backend proof type was provided for a request that must not have one.
    #[error("backend proof_type is not supported for {api_proof_type}")]
    UnexpectedBackendProofType {
        /// Protocol proof type that must not carry a backend proof type.
        api_proof_type: ApiProofType,
    },
    /// The backend proof type does not match the protocol proof type.
    #[error("backend proof_type {proof_type} is invalid for {api_proof_type}")]
    BackendProofTypeMismatch {
        /// Protocol proof type requested by the API.
        api_proof_type: ApiProofType,
        /// Backend proof type supplied for the request.
        proof_type: ProofType,
    },
}

/// Type of proof that determines success criteria
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "VARCHAR")]
pub enum ProofType {
    /// Compressed proof generated via the Succinct SP1 cluster.
    #[sqlx(rename = "op_succinct_sp1_cluster_compressed")]
    OpSuccinctSp1ClusterCompressed,
    /// SNARK Groth16 proof generated via the Succinct SP1 cluster.
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

/// Protocol-level proof type requested by API callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "VARCHAR")]
pub enum ApiProofType {
    /// Compressed ZK proof.
    #[sqlx(rename = "compressed")]
    Compressed,
    /// Groth16 SNARK proof.
    #[sqlx(rename = "snark_groth16")]
    SnarkGroth16,
    /// Trusted execution environment proof.
    #[sqlx(rename = "tee")]
    Tee,
}

impl ApiProofType {
    /// Convert enum to static string representation.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Compressed => "compressed",
            Self::SnarkGroth16 => "snark_groth16",
            Self::Tee => "tee",
        }
    }
}

impl std::fmt::Display for ApiProofType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl TryFrom<&str> for ApiProofType {
    type Error = String;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "compressed" => Ok(Self::Compressed),
            "snark_groth16" => Ok(Self::SnarkGroth16),
            "tee" => Ok(Self::Tee),
            other => Err(format!("Unknown API proof type: {other}")),
        }
    }
}

/// Protocol-level ZK virtual machine discriminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "VARCHAR")]
pub enum ZkVmKind {
    /// Succinct SP1.
    #[sqlx(rename = "sp1")]
    Sp1,
}

impl ZkVmKind {
    /// Convert enum to static string representation.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Sp1 => "sp1",
        }
    }
}

impl std::fmt::Display for ZkVmKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl TryFrom<&str> for ZkVmKind {
    type Error = String;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "sp1" => Ok(Self::Sp1),
            other => Err(format!("Unknown ZK VM: {other}")),
        }
    }
}

/// Protocol-level TEE implementation discriminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "VARCHAR")]
pub enum TeeKind {
    /// AWS Nitro Enclaves.
    #[sqlx(rename = "aws_nitro")]
    AwsNitro,
}

impl TeeKind {
    /// Convert enum to static string representation.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::AwsNitro => "aws_nitro",
        }
    }
}

impl std::fmt::Display for TeeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl TryFrom<&str> for TeeKind {
    type Error = String;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "aws_nitro" => Ok(Self::AwsNitro),
            other => Err(format!("Unknown TEE kind: {other}")),
        }
    }
}

/// A proof request record in the database
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofRequest {
    /// Unique identifier.
    pub id: Uuid,
    /// Public protocol session identifier.
    pub session_id: String,
    /// Original protocol request payload serialized as JSON.
    pub request_payload: serde_json::Value,
    /// Protocol-level proof type requested by API callers.
    pub api_proof_type: ApiProofType,
    /// Protocol-level ZK VM discriminator for ZK proofs.
    pub zk_vm: Option<ZkVmKind>,
    /// Protocol-level TEE discriminator for TEE proofs.
    pub tee_kind: Option<TeeKind>,
    /// Starting L2 block number.
    pub start_block_number: i64,
    /// Number of consecutive blocks to prove.
    pub number_of_blocks_to_prove: i64,
    /// Optional sequence window for the proof range.
    pub sequence_window: Option<i64>,
    /// Backend-specific proof type for ZK requests.
    pub proof_type: Option<ProofType>,
    /// Raw STARK receipt bytes, if available.
    pub stark_receipt: Option<Vec<u8>>,
    /// Raw SNARK receipt bytes, if available.
    pub snark_receipt: Option<Vec<u8>>,
    /// Protocol-level proof result payload, if available.
    pub result_payload: Option<serde_json::Value>,
    /// Worker id that submitted the result, if completed through the worker API.
    pub submitted_by_worker_id: Option<String>,
    /// Worker lock token that submitted the result, if completed through the worker API.
    pub submitted_lock_id: Option<String>,
    /// Current proof status.
    pub status: ProofStatus,
    /// Error message if the proof failed.
    pub error_message: Option<String>,
    /// Ethereum address of the on-chain prover.
    pub prover_address: Option<String>,
    /// Explicit L1 head hash used for witness generation.
    pub l1_head: Option<String>,
    /// Intermediate root interval requested for ZK proof generation.
    pub intermediate_root_interval: Option<i64>,
    /// Timestamp when the request was created.
    pub created_at: DateTime<Utc>,
    /// Timestamp of the last status update.
    pub updated_at: DateTime<Utc>,
    /// Timestamp when the proof completed (success or failure).
    pub completed_at: Option<DateTime<Utc>>,
    /// Number of times this request has been retried after getting stuck.
    pub retry_count: i32,
}

/// Receipt-free proof request row used by list endpoints.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ProofRequestListItem {
    /// Unique identifier.
    pub id: Uuid,
    /// Public protocol session identifier.
    pub session_id: String,
    /// Protocol-level proof type requested by API callers.
    pub api_proof_type: ApiProofType,
    /// Protocol-level ZK VM discriminator for ZK proofs.
    pub zk_vm: Option<ZkVmKind>,
    /// Protocol-level TEE discriminator for TEE proofs.
    pub tee_kind: Option<TeeKind>,
    /// Starting L2 block number.
    pub start_block_number: i64,
    /// Number of consecutive blocks to prove.
    pub number_of_blocks_to_prove: i64,
    /// Backend-specific proof type for ZK requests.
    pub proof_type: Option<ProofType>,
    /// Current proof status.
    pub status: ProofStatus,
    /// Error message if the proof failed.
    pub error_message: Option<String>,
    /// Timestamp when the request was created.
    pub created_at: DateTime<Utc>,
    /// Timestamp of the last status update.
    pub updated_at: DateTime<Utc>,
    /// Timestamp when the proof completed (success or failure).
    pub completed_at: Option<DateTime<Utc>>,
}

/// Worker-visible proof job, combining requester request data with the
/// worker-owned claim/lock state needed to build a protocol `ProofJob`.
#[derive(Debug, Clone)]
pub struct ProofJob {
    /// Internal proof request identifier.
    pub id: Uuid,
    /// Public protocol session identifier.
    pub session_id: String,
    /// Original protocol request payload serialized as JSON.
    pub request_payload: serde_json::Value,
    /// Protocol-level proof type requested by API callers.
    pub api_proof_type: ApiProofType,
    /// Protocol-level ZK VM discriminator for ZK proofs.
    pub zk_vm: Option<ZkVmKind>,
    /// Protocol-level TEE discriminator for TEE proofs.
    pub tee_kind: Option<TeeKind>,
    /// Worker-owned job lifecycle status.
    pub job_status: ProofJobStatus,
    /// Number of times the job has been claimed.
    pub attempt: i32,
    /// Worker that currently holds (or last held) the claim.
    pub worker_id: Option<String>,
    /// Active fencing token for the claim.
    pub lock_id: Option<Uuid>,
    /// Time when the current claim expires.
    pub lock_expires_at: Option<DateTime<Utc>>,
    /// Time when the current claim was acquired.
    pub claimed_at: Option<DateTime<Utc>>,
    /// Time of the most recent worker heartbeat.
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    /// Error message when the job failed.
    pub error_message: Option<String>,
    /// Stored protocol result payload once the job has completed.
    pub result_payload: Option<serde_json::Value>,
    /// Timestamp when the job was created.
    pub created_at: DateTime<Utc>,
    /// Timestamp of the last update.
    pub updated_at: DateTime<Utc>,
    /// Timestamp when the job completed.
    pub completed_at: Option<DateTime<Utc>>,
}

impl ProofJob {
    /// Reject a submitted result whose variant or capability discriminator
    /// (`zk_vm`/`tee_kind`) does not match this claimed job, returning the
    /// mismatch reason. Guards against a worker storing the wrong proof type.
    pub fn validate_submitted_result(&self, result: &ProtocolProofResult) -> Result<(), String> {
        match result {
            ProtocolProofResult::Compressed(zk) => {
                self.check_api_proof_type(ApiProofType::Compressed)?;
                self.check_zk_vm(ZkVmKind::from(zk.zk_vm))
            }
            ProtocolProofResult::SnarkGroth16(snark) => {
                self.check_api_proof_type(ApiProofType::SnarkGroth16)?;
                self.check_zk_vm(ZkVmKind::from(snark.proof.zk_vm))
            }
            ProtocolProofResult::Tee(tee) => {
                self.check_api_proof_type(ApiProofType::Tee)?;
                self.check_tee_kind(TeeKind::from(tee.tee_kind))
            }
        }
    }

    fn check_api_proof_type(&self, expected: ApiProofType) -> Result<(), String> {
        if self.api_proof_type == expected {
            Ok(())
        } else {
            Err(format!(
                "submitted {expected} result but claimed job proof type is {}",
                self.api_proof_type
            ))
        }
    }

    fn check_zk_vm(&self, submitted: ZkVmKind) -> Result<(), String> {
        match self.zk_vm {
            Some(expected) if expected == submitted => Ok(()),
            Some(expected) => {
                Err(format!("submitted zk_vm {submitted} but claimed job zk_vm is {expected}"))
            }
            None => Err(format!("submitted zk_vm {submitted} but claimed job has no zk_vm")),
        }
    }

    fn check_tee_kind(&self, submitted: TeeKind) -> Result<(), String> {
        match self.tee_kind {
            Some(expected) if expected == submitted => Ok(()),
            Some(expected) => Err(format!(
                "submitted tee_kind {submitted} but claimed job tee_kind is {expected}"
            )),
            None => Err(format!("submitted tee_kind {submitted} but claimed job has no tee_kind")),
        }
    }
}

/// Offset pagination parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofRequestPage {
    limit: i64,
    offset: i64,
}

impl ProofRequestPage {
    /// Create pagination parameters from API-level unsigned values.
    pub fn try_new(limit: u64, offset: u64) -> Result<Self, String> {
        if limit == 0 {
            return Err("limit must be greater than zero".to_owned());
        }

        let limit =
            i64::try_from(limit).map_err(|_| "limit exceeds maximum supported value".to_owned())?;
        let offset = i64::try_from(offset)
            .map_err(|_| "offset exceeds maximum supported value".to_owned())?;

        Ok(Self { limit, offset })
    }

    /// Maximum number of rows to return.
    pub const fn limit(&self) -> i64 {
        self.limit
    }

    /// Number of rows to skip.
    pub const fn offset(&self) -> i64 {
        self.offset
    }
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

/// Parameters for creating a new proof request.
#[derive(Debug, Clone)]
pub struct CreateProofRequest {
    /// Public protocol session identifier.
    pub session_id: String,
    /// Original protocol request payload.
    pub request_payload: ProtocolProofRequest,
    /// Protocol-level proof type requested by API callers.
    pub api_proof_type: ApiProofType,
    /// Protocol-level ZK VM discriminator for ZK proofs.
    pub zk_vm: Option<ZkVmKind>,
    /// Protocol-level TEE discriminator for TEE proofs.
    pub tee_kind: Option<TeeKind>,
    /// Backend-specific proof type for current OP Succinct backends.
    pub proof_type: Option<ProofType>,
    /// Starting L2 block number.
    pub start_block_number: u64,
    /// Number of consecutive blocks to prove.
    pub number_of_blocks_to_prove: u64,
    /// Optional sequence window.
    pub sequence_window: Option<u64>,
    /// Ethereum address of the on-chain prover (required for SNARK Groth16 proofs).
    pub prover_address: Option<String>,
    /// Explicit L1 head hash for witness generation.
    pub l1_head: Option<String>,
    /// Intermediate root interval for ZK proof generation.
    pub intermediate_root_interval: Option<u64>,
}

impl CreateProofRequest {
    /// Build a canonical create request from the protocol payload.
    pub fn new(
        request_payload: ProtocolProofRequest,
    ) -> Result<Self, CreateProofRequestValidationError> {
        let fields = DerivedProofRequestFields::from_protocol(&request_payload)?;

        Ok(Self {
            session_id: request_payload.session_id.clone(),
            request_payload,
            api_proof_type: fields.api_proof_type,
            zk_vm: fields.zk_vm,
            tee_kind: fields.tee_kind,
            proof_type: fields.proof_type,
            start_block_number: fields.start_block_number,
            number_of_blocks_to_prove: fields.number_of_blocks_to_prove,
            sequence_window: fields.sequence_window,
            prover_address: fields.prover_address,
            l1_head: fields.l1_head,
            intermediate_root_interval: fields.intermediate_root_interval,
        })
    }

    /// Validate that explicit DB fields match the protocol payload and supported backends.
    pub fn validate(&self) -> Result<(), CreateProofRequestValidationError> {
        let expected = DerivedProofRequestFields::from_protocol(&self.request_payload)?;
        let session_id = canonical_session_id(&self.session_id)?;
        let payload_session_id = canonical_session_id(&self.request_payload.session_id)?;

        if session_id != payload_session_id {
            return Err(CreateProofRequestValidationError::SessionIdMismatch);
        }
        if self.api_proof_type != expected.api_proof_type {
            return Err(CreateProofRequestValidationError::FieldMismatch {
                field: "api_proof_type",
            });
        }
        if self.zk_vm != expected.zk_vm {
            return Err(CreateProofRequestValidationError::FieldMismatch { field: "zk_vm" });
        }
        if self.tee_kind != expected.tee_kind {
            return Err(CreateProofRequestValidationError::FieldMismatch { field: "tee_kind" });
        }
        if self.proof_type != expected.proof_type {
            return Err(CreateProofRequestValidationError::FieldMismatch { field: "proof_type" });
        }
        if self.start_block_number != expected.start_block_number {
            return Err(CreateProofRequestValidationError::FieldMismatch {
                field: "start_block_number",
            });
        }
        if self.number_of_blocks_to_prove != expected.number_of_blocks_to_prove {
            return Err(CreateProofRequestValidationError::FieldMismatch {
                field: "number_of_blocks_to_prove",
            });
        }
        if self.sequence_window != expected.sequence_window {
            return Err(CreateProofRequestValidationError::FieldMismatch {
                field: "sequence_window",
            });
        }
        if self.prover_address != expected.prover_address {
            return Err(CreateProofRequestValidationError::FieldMismatch {
                field: "prover_address",
            });
        }
        if self.l1_head != expected.l1_head {
            return Err(CreateProofRequestValidationError::FieldMismatch { field: "l1_head" });
        }
        if self.intermediate_root_interval != expected.intermediate_root_interval {
            return Err(CreateProofRequestValidationError::FieldMismatch {
                field: "intermediate_root_interval",
            });
        }

        Ok(())
    }
}

/// Protocol fields derived from a create request payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedProofRequestFields {
    /// Protocol-level proof type requested by API callers.
    pub api_proof_type: ApiProofType,
    /// Protocol-level ZK VM discriminator for ZK proofs.
    pub zk_vm: Option<ZkVmKind>,
    /// Protocol-level TEE discriminator for TEE proofs.
    pub tee_kind: Option<TeeKind>,
    /// Backend-specific proof type for current OP Succinct backends.
    pub proof_type: Option<ProofType>,
    /// Starting L2 block number.
    pub start_block_number: u64,
    /// Number of consecutive blocks to prove.
    pub number_of_blocks_to_prove: u64,
    /// Optional sequence window.
    pub sequence_window: Option<u64>,
    /// Ethereum address of the on-chain prover.
    pub prover_address: Option<String>,
    /// Explicit L1 head hash.
    pub l1_head: Option<String>,
    /// Intermediate root interval.
    pub intermediate_root_interval: Option<u64>,
}

impl DerivedProofRequestFields {
    /// Derive database fields from a protocol proof request.
    pub fn from_protocol(
        request: &ProtocolProofRequest,
    ) -> Result<Self, CreateProofRequestValidationError> {
        match &request.request {
            ProtocolProofRequestKind::Compressed(proof) => Ok(Self {
                api_proof_type: ApiProofType::Compressed,
                zk_vm: Some(protocol_zk_vm(proof.zk_vm)),
                tee_kind: None,
                proof_type: Some(ProofType::OpSuccinctSp1ClusterCompressed),
                start_block_number: proof.start_block_number,
                number_of_blocks_to_prove: proof.number_of_blocks_to_prove,
                sequence_window: proof.sequence_window,
                prover_address: None,
                l1_head: proof.l1_head.map(|hash| format!("{hash:#x}")),
                intermediate_root_interval: proof.intermediate_root_interval,
            }),
            ProtocolProofRequestKind::SnarkGroth16(request) => Ok(Self {
                api_proof_type: ApiProofType::SnarkGroth16,
                zk_vm: Some(protocol_zk_vm(request.proof.zk_vm)),
                tee_kind: None,
                proof_type: Some(ProofType::OpSuccinctSp1ClusterSnarkGroth16),
                start_block_number: request.proof.start_block_number,
                number_of_blocks_to_prove: request.proof.number_of_blocks_to_prove,
                sequence_window: request.proof.sequence_window,
                prover_address: Some(format!("{:#x}", request.prover_address)),
                l1_head: request.proof.l1_head.map(|hash| format!("{hash:#x}")),
                intermediate_root_interval: request.proof.intermediate_root_interval,
            }),
            ProtocolProofRequestKind::Tee(request) => Ok(Self {
                api_proof_type: ApiProofType::Tee,
                zk_vm: None,
                tee_kind: Some(protocol_tee_kind(request.tee_kind)),
                proof_type: None,
                start_block_number: request.proof.claimed_l2_block_number,
                number_of_blocks_to_prove: 1,
                sequence_window: None,
                prover_address: None,
                l1_head: Some(format!("{:#x}", request.proof.l1_head)),
                intermediate_root_interval: (request.proof.intermediate_block_interval > 0)
                    .then_some(request.proof.intermediate_block_interval),
            }),
        }
    }
}

const fn protocol_zk_vm(zk_vm: ProtocolZkVm) -> ZkVmKind {
    match zk_vm {
        ProtocolZkVm::Sp1 => ZkVmKind::Sp1,
    }
}

const fn protocol_tee_kind(tee_kind: ProtocolTeeKind) -> TeeKind {
    match tee_kind {
        ProtocolTeeKind::AwsNitro => TeeKind::AwsNitro,
    }
}

/// Canonicalize a public proof request session id.
pub fn canonical_session_id(session_id: &str) -> Result<String, CreateProofRequestValidationError> {
    if session_id.is_empty() {
        return Err(CreateProofRequestValidationError::EmptySessionId);
    }

    Ok(Uuid::parse_str(session_id)
        .map(|uuid| uuid.to_string())
        .unwrap_or_else(|_| session_id.to_owned()))
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

/// Parameters for completing a proof request with a protocol-native result payload.
#[derive(Debug, Clone)]
pub struct CompleteProofResult {
    /// Proof request identifier.
    pub id: Uuid,
    /// Protocol result to store in `result_payload`.
    pub result: ProtocolProofResult,
    /// Worker id that submitted the proof, if completed through the worker API.
    pub submitted_by_worker_id: Option<String>,
    /// Worker lock token that submitted the proof, if completed through the worker API.
    pub submitted_lock_id: Option<String>,
    /// Error message to store with the completion. Usually `None`.
    pub error_message: Option<String>,
}

/// Parameters for claiming the next available worker proof job.
#[derive(Debug, Clone)]
pub struct ClaimProofJob {
    /// Worker identifier acquiring the claim.
    pub worker_id: String,
    /// Protocol proof type this worker can execute.
    pub api_proof_type: ApiProofType,
    /// TEE implementations this worker can execute (matched for TEE proofs).
    pub tee_kinds: Vec<TeeKind>,
    /// ZK virtual machines this worker can execute (matched for ZK proofs).
    pub zk_vms: Vec<ZkVmKind>,
    /// Lock duration in seconds. Callers must resolve the server default first.
    pub lock_duration_seconds: u32,
    /// Reclaim budget for expired claims.
    pub max_attempts: u32,
}

/// Parameters for extending the currently owned worker proof job lock.
#[derive(Debug, Clone)]
pub struct HeartbeatProofJob {
    /// Public proof session identifier.
    pub session_id: String,
    /// Current worker fencing token.
    pub lock_id: Uuid,
    /// Worker identifier that owns the claim.
    pub worker_id: String,
    /// Lock duration in seconds. Callers must resolve the server default first.
    pub lock_duration_seconds: u32,
}

/// Outcome of attempting to heartbeat a worker proof job.
#[derive(Debug, Clone)]
pub enum HeartbeatOutcome {
    /// Heartbeat succeeded.
    Updated(ProofJob),
    /// No proof job exists for the supplied `session_id`.
    NotFound,
    /// The job exists but is not currently claimed.
    NotClaimed(ProofJob),
    /// The supplied worker or lock no longer owns the job.
    StaleLock(ProofJob),
    /// The lock matched the job but had expired.
    Expired(ProofJob),
    /// The job is already terminal.
    Terminal(ProofJob),
    /// The update was denied, but the diagnostic read did not identify a stable reason.
    Unknown(ProofJob),
}

/// Parameters for completing a claimed worker proof job.
#[derive(Debug, Clone)]
pub struct CompleteClaimedProofJob {
    /// Public proof session identifier.
    pub session_id: String,
    /// Current worker fencing token.
    pub lock_id: Uuid,
    /// Worker identifier that owns the claim.
    pub worker_id: String,
    /// Protocol result to store in `result_payload`.
    pub result: ProtocolProofResult,
}

/// Outcome of attempting to complete a worker proof job.
#[derive(Debug, Clone)]
pub enum SubmitProofOutcome {
    /// Submit succeeded.
    Completed(ProofJob),
    /// The submitted result does not match the claimed job's proof type or
    /// capability discriminator. The stored job is left unchanged.
    ResultMismatch {
        /// The claimed job whose proof type was violated.
        job: ProofJob,
        /// Human-readable description of the mismatch.
        reason: String,
    },
    /// An idempotent retry from the owning worker/lock submitted a result that
    /// differs from the one already stored. The stored result is kept.
    ResultConflict {
        /// The already-completed job whose stored result was retained.
        job: ProofJob,
    },
    /// No proof job exists for the supplied `session_id`.
    NotFound,
    /// The job exists but is not currently claimed.
    NotClaimed(ProofJob),
    /// The supplied worker or lock no longer owns the job.
    StaleLock(ProofJob),
    /// The lock matched the job but had expired.
    Expired(ProofJob),
    /// The job is already terminal.
    Terminal(ProofJob),
    /// The update was denied, but the diagnostic read did not identify a stable reason.
    Unknown(ProofJob),
}

/// A proof job's current lock columns, as read under a row lock, against which a
/// worker write is authorized.
///
/// Grouping the job-side values keeps them from being confused with the
/// worker-supplied fencing token at `ClaimAuth::classify` call sites.
#[derive(Debug, Clone, Copy)]
pub struct JobLockState<'a> {
    /// Current lifecycle status of the job.
    pub status: ProofJobStatus,
    /// Active lock identifier, set while the job is claimed.
    pub lock_id: Option<Uuid>,
    /// Worker that currently holds the lock, if any.
    pub worker_id: Option<&'a str>,
    /// When the active lock expires, if any.
    pub lock_expires_at: Option<DateTime<Utc>>,
}

/// Parameters for an external worker recording a backend session id against a
/// claimed job, so a restart or reclaim resumes the in-flight backend job.
#[derive(Debug, Clone)]
pub struct WorkerSessionUpsert {
    /// Public proof session identifier.
    pub session_id: String,
    /// Current worker fencing token.
    pub lock_id: Uuid,
    /// Worker identifier that owns the claim.
    pub worker_id: String,
    /// Backend session type being recorded.
    pub session_type: SessionType,
    /// Backend-issued session identifier to persist.
    pub backend_session_id: String,
    /// Backend session lifecycle state to persist.
    pub status: SessionStatus,
    /// Failure reason to persist, typically set alongside a `Failed` status.
    pub error_message: Option<String>,
}

/// Outcome of attempting to record a backend session for a worker job.
#[derive(Debug, Clone)]
pub enum RecordSessionOutcome {
    /// The session was inserted or updated and the active row is returned.
    Recorded(ProofSession),
    /// No proof job exists for the supplied `session_id`.
    NotFound,
    /// The job exists but is not currently claimed.
    NotClaimed,
    /// The supplied `worker_id` or `lock_id` no longer owns the job.
    StaleLock,
    /// The supplied lock matched the job, but it had already expired.
    Expired,
    /// The job is already terminal.
    Terminal,
    /// The requested session status is terminal and must be coordinated with job completion.
    TerminalSessionStatus,
}

/// Result of checking a worker's fencing token against a claimed job's lock.
///
/// Shared by `heartbeat_proof_job` and `record_worker_proof_session` so both
/// authorize worker writes against identical lock rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimAuth {
    /// The requesting worker holds the active lock; the write may proceed.
    Authorized,
    /// The job has already reached a terminal state.
    Terminal,
    /// The job is not currently claimed.
    NotClaimed,
    /// The lock is held by another worker or has been rotated.
    StaleLock,
    /// The lock has expired.
    Expired,
}

impl ClaimAuth {
    /// Classify a worker claim from a job's current lock columns (`job`) against
    /// the fencing token (`req_lock_id`, `req_worker_id`) the worker presented.
    ///
    /// `now` is supplied by the caller so the expiry check stays pure and
    /// deterministically testable.
    pub fn classify(
        job: JobLockState<'_>,
        req_lock_id: Uuid,
        req_worker_id: &str,
        now: DateTime<Utc>,
    ) -> Self {
        if matches!(job.status, ProofJobStatus::Succeeded | ProofJobStatus::Failed) {
            return Self::Terminal;
        }
        if job.status != ProofJobStatus::Claimed {
            return Self::NotClaimed;
        }
        if job.lock_id != Some(req_lock_id) || job.worker_id != Some(req_worker_id) {
            return Self::StaleLock;
        }
        if job.lock_expires_at.is_none_or(|expires_at| expires_at <= now) {
            return Self::Expired;
        }
        Self::Authorized
    }
}

/// Parameters for terminally failing expired worker jobs that exhausted attempts.
#[derive(Debug, Clone)]
pub struct FailExpiredProofJobs<'a> {
    /// Jobs with `attempt >= max_attempts` are failed once their lock has expired.
    pub max_attempts: u32,
    /// Maximum number of expired jobs to fail in this batch.
    pub batch_size: u32,
    /// Error message stored on newly failed jobs.
    pub error_message: &'a str,
}

#[cfg(test)]
mod tests {
    use base_prover_service_protocol::{
        SnarkGroth16ProofResult, ZkProofRequest, ZkProofResult, ZkVm,
    };

    use super::*;

    fn proof_job_with(
        api_proof_type: ApiProofType,
        zk_vm: Option<ZkVmKind>,
        tee_kind: Option<TeeKind>,
    ) -> ProofJob {
        let now = Utc::now();
        ProofJob {
            id: Uuid::new_v4(),
            session_id: "session-1".to_owned(),
            request_payload: serde_json::Value::Null,
            api_proof_type,
            zk_vm,
            tee_kind,
            job_status: ProofJobStatus::Claimed,
            attempt: 1,
            worker_id: Some("worker-1".to_owned()),
            lock_id: Some(Uuid::new_v4()),
            lock_expires_at: Some(now),
            claimed_at: Some(now),
            last_heartbeat_at: Some(now),
            error_message: None,
            result_payload: None,
            created_at: now,
            updated_at: now,
            completed_at: None,
        }
    }

    fn compressed_protocol_request(session_id: impl Into<String>) -> ProtocolProofRequest {
        ProtocolProofRequest {
            session_id: session_id.into(),
            request: ProtocolProofRequestKind::Compressed(ZkProofRequest {
                start_block_number: 100,
                number_of_blocks_to_prove: 5,
                sequence_window: Some(50),
                l1_head: None,
                intermediate_root_interval: None,
                zk_vm: ZkVm::Sp1,
            }),
        }
    }

    #[test]
    fn test_proof_type_try_from_proto() {
        assert_eq!(ProofType::try_from(3).unwrap(), ProofType::OpSuccinctSp1ClusterCompressed);
        assert_eq!(ProofType::try_from(4).unwrap(), ProofType::OpSuccinctSp1ClusterSnarkGroth16);

        assert!(ProofType::try_from(0).is_err());
        assert!(ProofType::try_from(1).is_err());
        assert!(ProofType::try_from(2).is_err());
        assert!(ProofType::try_from(5).is_err());
    }

    #[test]
    fn validate_rejects_empty_session_ids() {
        let mut req = CreateProofRequest::new(compressed_protocol_request("session-1")).unwrap();
        req.session_id = String::new();

        assert_eq!(req.validate(), Err(CreateProofRequestValidationError::EmptySessionId));

        let mut req = CreateProofRequest::new(compressed_protocol_request("session-1")).unwrap();
        req.request_payload.session_id = String::new();

        assert_eq!(req.validate(), Err(CreateProofRequestValidationError::EmptySessionId));
    }

    #[test]
    fn validate_compares_canonical_session_ids_when_both_are_present() {
        let id = Uuid::new_v4();
        let mut req = CreateProofRequest::new(compressed_protocol_request(id.to_string())).unwrap();
        req.session_id = id.to_string().to_uppercase();

        assert_eq!(req.validate(), Ok(()));

        req.session_id = "other-session".to_owned();

        assert_eq!(req.validate(), Err(CreateProofRequestValidationError::SessionIdMismatch));
    }

    #[test]
    fn validate_submitted_result_accepts_matching_compressed() {
        let job = proof_job_with(ApiProofType::Compressed, Some(ZkVmKind::Sp1), None);
        let result = ProtocolProofResult::Compressed(ZkProofResult {
            zk_vm: ZkVm::Sp1,
            proof: vec![0x01].into(),
        });

        assert_eq!(job.validate_submitted_result(&result), Ok(()));
    }

    #[test]
    fn validate_submitted_result_rejects_wrong_variant() {
        let job = proof_job_with(ApiProofType::Tee, None, Some(TeeKind::AwsNitro));
        let result = ProtocolProofResult::Compressed(ZkProofResult {
            zk_vm: ZkVm::Sp1,
            proof: vec![0x01].into(),
        });

        assert!(job.validate_submitted_result(&result).is_err());
    }

    #[test]
    fn validate_submitted_result_rejects_snark_for_compressed_job() {
        let job = proof_job_with(ApiProofType::Compressed, Some(ZkVmKind::Sp1), None);
        let result = ProtocolProofResult::SnarkGroth16(SnarkGroth16ProofResult {
            proof: ZkProofResult { zk_vm: ZkVm::Sp1, proof: vec![0x01].into() },
        });

        assert!(job.validate_submitted_result(&result).is_err());
    }

    #[test]
    fn validate_submitted_result_rejects_missing_zk_vm_capability() {
        let job = proof_job_with(ApiProofType::Compressed, None, None);
        let result = ProtocolProofResult::Compressed(ZkProofResult {
            zk_vm: ZkVm::Sp1,
            proof: vec![0x01].into(),
        });

        assert!(job.validate_submitted_result(&result).is_err());
    }

    #[test]
    fn claim_auth_classify_covers_every_branch() {
        let now = Utc::now();
        let lock = Uuid::new_v4();
        let worker = "worker-1";
        let unexpired = Some(now + chrono::Duration::seconds(30));

        let claimed = |lock_id, worker_id, lock_expires_at| JobLockState {
            status: ProofJobStatus::Claimed,
            lock_id,
            worker_id,
            lock_expires_at,
        };

        let authorized =
            ClaimAuth::classify(claimed(Some(lock), Some(worker), unexpired), lock, worker, now);
        assert_eq!(authorized, ClaimAuth::Authorized);

        for status in [ProofJobStatus::Succeeded, ProofJobStatus::Failed] {
            let job = JobLockState {
                status,
                lock_id: Some(lock),
                worker_id: Some(worker),
                lock_expires_at: unexpired,
            };
            assert_eq!(ClaimAuth::classify(job, lock, worker, now), ClaimAuth::Terminal);
        }

        let pending = JobLockState {
            status: ProofJobStatus::Pending,
            lock_id: Some(lock),
            worker_id: Some(worker),
            lock_expires_at: unexpired,
        };
        assert_eq!(ClaimAuth::classify(pending, lock, worker, now), ClaimAuth::NotClaimed);

        let wrong_lock = ClaimAuth::classify(
            claimed(Some(Uuid::new_v4()), Some(worker), unexpired),
            lock,
            worker,
            now,
        );
        assert_eq!(wrong_lock, ClaimAuth::StaleLock);

        let wrong_worker = ClaimAuth::classify(
            claimed(Some(lock), Some("worker-2"), unexpired),
            lock,
            worker,
            now,
        );
        assert_eq!(wrong_worker, ClaimAuth::StaleLock);

        // Expiry is evaluated against the supplied `now`, so the boundary is
        // deterministic: an `expires_at` equal to `now` counts as expired.
        let expired =
            ClaimAuth::classify(claimed(Some(lock), Some(worker), Some(now)), lock, worker, now);
        assert_eq!(expired, ClaimAuth::Expired);

        let missing_expiry =
            ClaimAuth::classify(claimed(Some(lock), Some(worker), None), lock, worker, now);
        assert_eq!(missing_expiry, ClaimAuth::Expired);
    }
}
