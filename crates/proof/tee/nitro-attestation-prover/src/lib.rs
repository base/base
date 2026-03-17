#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod error;
pub use error::{ProverError, Result};

mod types;
pub use types::{AttestationProof, AttestationProofProvider};

mod direct;
pub use direct::DirectProver;

mod boundless;
pub use boundless::BoundlessProver;
