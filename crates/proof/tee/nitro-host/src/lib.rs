#![doc = include_str!("../README.md")]

mod error;
pub use error::NitroHostError;

mod config;
pub use config::{ConfigError, NitroWorkerConfig};

mod backend;
pub use backend::NitroBackend;

mod registration;
pub use registration::{RegistrationChecker, RegistrationError, ValidSigner};

mod proof_submitter;
pub use proof_submitter::{ProofSubmitterRequest, ProofSubmitterRequestError};

mod proof_generator;
pub use proof_generator::{
    DEFAULT_PROOF_GENERATOR_HEARTBEAT_INTERVAL,
    DEFAULT_PROOF_GENERATOR_HEARTBEAT_LOCK_DURATION_SECONDS,
    DEFAULT_PROOF_GENERATOR_MAX_CONSECUTIVE_HEARTBEAT_FAILURES,
    MIN_PROOF_GENERATOR_HEARTBEAT_INTERVAL, ProofGenerator, ProofGeneratorError,
    ProofGeneratorHeartbeatConfig, ProofGeneratorRequest,
};

mod host;
pub use host::NitroHost;

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
