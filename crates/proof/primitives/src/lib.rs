#![doc = include_str!("../README.md")]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod witness;
pub use witness::WitnessBundle;

mod proof;
pub use proof::{ProofClaim, ProofEvidence, ProofRequest, ProofResult};

mod prover;
pub use prover::ProverBackend;
