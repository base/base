#![doc = include_str!("../README.md")]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod proof;
pub use proof::{ProofBundle, ProofRequest, ProofResult};

mod proposal;
pub use proposal::{ECDSA_SIGNATURE_LENGTH, PROOF_JOURNAL_BASE_LENGTH, ProofJournal, Proposal};

mod proof_encoder;
pub use proof_encoder::{CryptoError, PROOF_TYPE_TEE, ProofEncoder};

mod prover;
pub use prover::ProverBackend;

#[cfg(any(feature = "rpc-server", feature = "rpc-client"))]
mod rpc;
#[cfg(feature = "rpc-client")]
pub use rpc::{EnclaveApiClient, ProverApiClient};
#[cfg(feature = "rpc-server")]
pub use rpc::{EnclaveApiServer, ProverApiServer};

#[cfg(feature = "rpc-client")]
mod prover_client;
#[cfg(feature = "rpc-client")]
pub use prover_client::ProverClient;
