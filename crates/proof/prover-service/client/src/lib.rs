#![doc = include_str!("../README.md")]

mod config;
pub use config::{
    ProverServiceClientBuildError, ProverServiceClientConfig, ProverServiceClientConfigError,
};

mod error;
pub use error::ProverServiceClientError;

mod requester;
pub use requester::{ProofRequesterClient, ProofRequesterProvider};

mod retry;
pub use retry::{
    DEFAULT_PROOF_REQUESTER_INITIAL_DELAY, DEFAULT_PROOF_REQUESTER_MAX_ATTEMPTS,
    DEFAULT_PROOF_REQUESTER_MAX_DELAY, MIN_PROOF_REQUESTER_RETRY_DELAY, ProofRequesterRetryConfig,
};

mod worker;
pub use worker::{ProverWorkerClient, ProverWorkerProvider};
