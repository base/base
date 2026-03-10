//! Attestation verification for AWS Nitro Enclaves.
//!
//! This module provides:
//! - [`AwsCaRoot`]: AWS CA root certificate handling
//! - [`verify_attestation`]: Attestation document verification

mod ca_roots;
mod verify;

pub use ca_roots::{AwsCaRoot, DEFAULT_CA_ROOTS, DEFAULT_CA_ROOTS_SHA256, get_default_ca_root};
pub use verify::{AttestationDocument, VerificationResult, verify_attestation};
