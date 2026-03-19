#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod error;
pub use error::{ProverError, Result};

mod types;
pub use types::{AttestationProof, AttestationProofProvider};

// Prover implementations are behind the `prove` feature to avoid pulling in
// risc0-sys (Metal kernel builds on macOS) and to reduce compile times for
// consumers that only need the trait, types, and error definitions.
#[cfg(feature = "prove")]
mod direct;
#[cfg(feature = "prove")]
pub use direct::DirectProver;

#[cfg(feature = "prove")]
mod boundless;
#[cfg(feature = "prove")]
pub use boundless::BoundlessProver;
