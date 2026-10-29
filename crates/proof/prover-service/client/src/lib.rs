#![doc = include_str!("../README.md")]

mod config;
pub use config::{
    ProverServiceClientBuildError, ProverServiceClientConfig, ProverServiceClientConfigError,
};

mod error;
pub use error::ProverServiceClientError;

mod requester;
pub use requester::{ProofRequesterClient, ProofRequesterProvider};

mod worker;
#[cfg(any(test, feature = "test-utils"))]
pub use worker::MockProverWorkerProvider;
pub use worker::{ProverWorkerClient, ProverWorkerProvider};
