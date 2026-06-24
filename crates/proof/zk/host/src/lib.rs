#![doc = include_str!("../README.md")]

pub use base_prover_service_protocol::ZkVm;

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

mod host;
pub use host::{ZkHost, ZkHostConfig};
