#![doc = include_str!("../README.md")]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod proof;
pub use proof::{ProofBundle, ProofClaim, ProofEvidence, ProofRequest, ProofResult};

mod proposal;
pub use proposal::Proposal;

mod prover;
pub use prover::ProverBackend;
