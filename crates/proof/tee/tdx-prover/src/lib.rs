#![doc = include_str!("../README.md")]

pub use base_proof_tee_tdx_verifier::{TdxSignerAttestation, TdxSignerAttestationDecodeError};

mod backend;
pub use backend::TdxBackend;

mod error;
pub use error::{Result, TdxProverError};

#[cfg(any(test, feature = "test-utils"))]
mod image;
#[cfg(any(test, feature = "test-utils"))]
pub use image::TdxMeasurements;

mod oracle;
pub use oracle::Oracle;

mod proof_generator;
pub use proof_generator::{
    DEFAULT_TDX_WORKER_ID, ProofGenerator, ProofGeneratorBoxError, ProofGeneratorError,
    ProofGeneratorRequest, TdxProofSubmitterRequest,
};

mod server;
pub use server::{TDX_ATTESTATION_KIND, TdxEnclaveService, TdxProverServer, TdxSignerRpc};
