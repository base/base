#![doc = include_str!("../README.md")]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod proof;
pub use proof::{ProofBundle, ProofRequest, ProofResult};

mod proposal;
pub use proposal::{ECDSA_SIGNATURE_LENGTH, Proposal};

mod prover;
pub use prover::ProverBackend;

#[cfg(any(feature = "rpc-server", feature = "rpc-client"))]
mod rpc;
#[cfg(feature = "rpc-client")]
pub use rpc::{EnclaveApiClient, ProverApiClient};
#[cfg(feature = "rpc-server")]
pub use rpc::{EnclaveApiServer, ProverApiServer};
