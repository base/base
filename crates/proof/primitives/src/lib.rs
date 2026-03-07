#![doc = include_str!("../README.md")]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod proof;
pub use proof::{
    ProofBundle, ProofClaim, ProofClaimError, ProofEvidence, ProofRequest, ProofResult,
    MAX_PROPOSALS,
};

mod proposal;
pub use proposal::{Proposal, ECDSA_SIGNATURE_LENGTH};

mod prover;
pub use prover::ProverBackend;
