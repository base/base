#![doc = include_str!("../README.md")]

mod config;
pub use config::DatabaseConfig;

mod models;
pub use models::{
    ApiProofType, CreateOutboxEntry, CreateProofRequest, CreateProofRequestError,
    CreateProofRequestOutcome, CreateProofSession, MarkOutboxError, MarkOutboxProcessed,
    OutboxEntry, ProofRequest, ProofRequestListItem, ProofRequestPage, ProofSession, ProofStatus,
    ProofType, RetryOutcome, SessionStatus, SessionType, TeeKind, UpdateProofSession,
    UpdateReceipt, ZkVmKind,
};

mod repo;
pub use repo::ProofRequestRepo;
