#![doc = include_str!("../README.md")]

pub use base_proof_worker::{
    DEFAULT_JOB_DISCOVERY_LOCK_DURATION_SECONDS, DEFAULT_JOB_DISCOVERY_MAX_CONCURRENT_JOBS,
    JobDiscovery, JobDiscoveryConfig, ProofSubmitter, ProofSubmitterError, ZkProofClaimType,
};

mod prover;
pub use prover::{
    UnimplementedZkProver, ZkProofRequestKind, ZkProver, ZkProverError, ZkSessionState,
};

mod session_handle;
pub use session_handle::ProofSessionHandle;

mod proof_submitter;
pub use proof_submitter::ProofSubmitterRequest;

mod proof_generator;
pub use proof_generator::{
    DEFAULT_PROOF_GENERATOR_HEARTBEAT_FAILURE_DRAIN_TIMEOUT,
    DEFAULT_PROOF_GENERATOR_HEARTBEAT_INTERVAL,
    DEFAULT_PROOF_GENERATOR_HEARTBEAT_LOCK_DURATION_SECONDS,
    DEFAULT_PROOF_GENERATOR_MAX_CONSECUTIVE_HEARTBEAT_FAILURES,
    DEFAULT_PROOF_GENERATOR_POLL_INTERVAL, MIN_PROOF_GENERATOR_HEARTBEAT_INTERVAL,
    MIN_PROOF_GENERATOR_POLL_INTERVAL, ProofGenerator, ProofGeneratorError,
    ProofGeneratorHeartbeatConfig, ProofGeneratorRequest,
};
