#![doc = include_str!("../README.md")]

mod config;
pub use config::DatabaseConfig;

mod models;
pub use models::{
    ApiProofType, ClaimProofJob, CompleteClaimedProofJob, CompleteProofResult, CreateProofRequest,
    CreateProofRequestError, CreateProofRequestOutcome, CreateProofRequestValidationError,
    CreateProofSession, DerivedProofRequestFields, FailExpiredProofJobs, HeartbeatOutcome,
    HeartbeatProofJob, ProofJob, ProofJobStatus, ProofRequest, ProofRequestListItem,
    ProofRequestPage, ProofSession, ProofStatus, ProofType, RetryOutcome, SessionStatus,
    SessionType, SubmitProofOutcome, TeeKind, UpdateProofSession, UpdateReceipt, ZkVmKind,
    canonical_session_id,
};

mod repo;
pub use repo::ProofRequestRepo;
