#![doc = include_str!("../README.md")]
#![warn(missing_docs)]
#![warn(unreachable_pub)]
#![deny(unused_must_use)]
#![deny(rust_2018_idioms)]

mod client;
mod client_error;

// Re-export core types
pub use base_enclave::*;
pub use base_proof_primitives::{
    ProofBundle, ProofClaim, ProofEvidence, ProofRequest, ProofResult,
};
pub use client::EnclaveClient;
pub use client_error::ClientError;
