#![doc = include_str!("../README.md")]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod proof;
pub use proof::{ProofBundle, ProofClaim, ProofEvidence, ProofRequest, ProofResult};

mod proposal;
pub use proposal::{Proposal, SIGNATURE_LENGTH};

mod prover;
pub use prover::ProverBackend;

#[cfg(any(feature = "rpc-server", feature = "rpc-client"))]
mod rpc;
#[cfg(feature = "rpc-server")]
pub use rpc::ProverApiServer;
#[cfg(feature = "rpc-client")]
pub use rpc::ProverApiClient;
