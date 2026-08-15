#![doc = include_str!("../README.md")]

mod config;
pub use config::DatabaseConfig;

mod conversions;
pub use conversions::ConversionError;

mod models;
pub use models::{
    ApiProofType, ClaimAuth, ClaimProofJob, CompleteClaimedProofJob, CompleteProofResult,
    CreateProofRequest, CreateProofRequestError, CreateProofRequestOutcome,
    CreateProofRequestValidationError, CreateProofSession, DeleteProofRequestOutcome,
    DerivedProofRequestFields, FailExpiredProofJobs, HeartbeatOutcome, HeartbeatProofJob,
    JobLockState, ProofJob, ProofJobStatus, ProofRequest, ProofRequestListItem, ProofRequestPage,
    ProofSession, ProofStatus, ProofType, RecordSessionOutcome, RetryOutcome, SessionStatus,
    SessionType, SubmitProofOutcome, TeeKind, UpdateProofSession, UpdateReceipt,
    WorkerSessionUpsert, ZkVmKind, canonical_session_id,
};

mod repo;
pub use repo::ProofRequestRepo;
