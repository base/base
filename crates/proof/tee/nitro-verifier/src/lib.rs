#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod attestation;
pub use attestation::{AttestationDocument, AttestationReport, CoseSign1};

mod error;
pub use error::{Result, VerifierError};

mod types;
pub use types::{
    BatchVerifierJournal, Bytes48, Pcr, VerificationResult, VerifierInput, VerifierJournal,
    ZkCoProcessorConfig, ZkCoProcessorType,
};
