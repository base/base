#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod attestation;
pub use attestation::{TdxSignerAttestation, TdxSignerAttestationDecodeError};

mod error;
pub use error::{Result, TdxVerifierError};

mod input;
pub use input::{TdxVerifierInput, TdxVerifierInputAbi};

mod types;
pub use types::{TDXVerificationResult, TDXVerifierJournal};

mod verify;
pub use verify::TdxVerifier;
