#![doc = include_str!("../README.md")]

mod error;
pub use error::NitroHostError;

mod config;
pub use config::{ConfigError, NitroWorkerConfig};

mod job_discovery;
pub use job_discovery::{
    DEFAULT_JOB_DISCOVERY_LOCK_DURATION_SECONDS, DEFAULT_JOB_DISCOVERY_MAX_CONCURRENT_JOBS,
    DEFAULT_JOB_DISCOVERY_POLL_INTERVAL, DEFAULT_JOB_DISCOVERY_WORKER_ID, JobDiscovery,
    JobDiscoveryConfig, JobDiscoveryPollOutcome, JobDiscoveryTask, MIN_JOB_DISCOVERY_POLL_INTERVAL,
};

mod backend;
pub use backend::NitroBackend;

mod registration;
pub use registration::{RegistrationChecker, RegistrationError, ValidSigner};

mod proof_submitter;
pub use proof_submitter::{
    DEFAULT_PROOF_SUBMITTER_INITIAL_BACKOFF, DEFAULT_PROOF_SUBMITTER_MAX_BACKOFF,
    MIN_PROOF_SUBMITTER_BACKOFF, ProofSubmitter, ProofSubmitterBackoffConfig, ProofSubmitterError,
    ProofSubmitterRequest,
};

mod proof_generator;
pub use proof_generator::{
    DEFAULT_PROOF_GENERATOR_HEARTBEAT_INTERVAL,
    DEFAULT_PROOF_GENERATOR_HEARTBEAT_LOCK_DURATION_SECONDS,
    DEFAULT_PROOF_GENERATOR_MAX_CONSECUTIVE_HEARTBEAT_FAILURES,
    MIN_PROOF_GENERATOR_HEARTBEAT_INTERVAL, ProofGenerator, ProofGeneratorError,
    ProofGeneratorHeartbeatConfig, ProofGeneratorRequest, ProofGeneratorTask,
};

mod pool;
pub use pool::{
    MAX_CONCURRENT_PROOF_REQUESTS_PER_ENCLAVE, NitroEnclavePool, NitroEnclavePoolError,
};

mod health;
pub use health::{RegistrationHealthConfig, RegistrationHealthzRpc};

mod server;
pub use server::NitroProverServer;

mod transport;
pub use transport::NitroTransport;

#[cfg(target_os = "linux")]
mod vsock;
#[cfg(target_os = "linux")]
pub use vsock::VsockTransport;

#[cfg(test)]
pub mod test_utils;
