//! Cryptographic operations for the enclave server.
//!
//! This module provides:
//! - [`ecdsa`]: ECDSA secp256k1 operations using alloy-signer-local
//! - [`signing`]: Proposal signing and verification

pub mod ecdsa;
pub mod signing;

pub use ecdsa::{generate_signer, public_key_bytes, signer_from_hex};
pub use signing::{
    SIGNATURE_LENGTH, SIGNING_DATA_BASE_LENGTH, build_signing_data, sign_proposal_data_sync,
    verify_proposal_signature,
};
