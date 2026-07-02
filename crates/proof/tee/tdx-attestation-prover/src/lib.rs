#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[cfg(test)]
pub mod test_utils;

mod direct;
pub use direct::{
    DIRECT_DEV_PROOF_BYTES, DirectProver, NativeTdxJournalVerifier, TdxJournalVerifier,
};

mod error;
pub use error::{ProverError, Result};

mod input;
pub use input::{
    TdxAttestationProverInput, TdxCertificateInput, TdxCollateralInput, TdxRevocationEvidenceInput,
    TdxSignedCollateralInput, TdxVerifierInputAbi, tdx_tcb_status_from_u8,
};

#[cfg(feature = "prove")]
mod boundless;
#[cfg(feature = "prove")]
pub use boundless::BoundlessProver;
