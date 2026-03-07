#![doc = include_str!("../README.md")]
#![warn(missing_docs)]
#![warn(unreachable_pub)]
#![deny(unused_must_use)]
#![deny(rust_2018_idioms)]

mod client;
pub use client::{EnclaveClient, EnclaveProvider};

mod client_error;
pub use client_error::ClientError;

pub use base_enclave::{AggregateRequest, ExecuteStatelessRequest, Proposal};
pub use base_proof_primitives::{
    ProofBundle, ProofClaim, ProofEvidence, ProofRequest, ProofResult,
};
