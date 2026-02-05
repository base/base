//! Attestation verification for AWS Nitro Enclaves.
//!
//! This module provides:
//! - [`AwsCaRoot`]: AWS CA root certificate handling
//! - [`verify_attestation`]: Attestation document verification
//! - [`verify_attestation_with_pcr0`]: Verification with PCR0 check

mod ca_roots;
mod verify;

pub use ca_roots::{AwsCaRoot, DEFAULT_CA_ROOTS, DEFAULT_CA_ROOTS_SHA256, get_default_ca_root};
pub use verify::{
    AttestationDocument, VerificationResult, VerifyOptions, extract_public_key, verify_attestation,
    verify_attestation_with_options, verify_attestation_with_pcr0,
    verify_attestation_with_pcr0_and_options,
};
