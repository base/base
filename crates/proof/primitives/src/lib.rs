#![doc = include_str!("../README.md")]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod proof;
pub use proof::{
    MAX_PROPOSALS, ProofBundle, ProofClaim, ProofClaimError, ProofEvidence, ProofRequest,
    ProofResult,
};

mod proposal;
pub use proposal::{ECDSA_SIGNATURE_LENGTH, Proposal};

mod prover;
pub use prover::ProverBackend;
