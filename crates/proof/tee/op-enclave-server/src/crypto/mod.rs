//! Cryptographic operations for the enclave server.
//!
//! This module provides:
//! - [`rsa`]: RSA-4096 key generation, PKIX serialization, PKCS1v15 encrypt/decrypt
//! - [`ecdsa`]: ECDSA secp256k1 operations using alloy-signer-local

pub mod ecdsa;
pub mod rsa;

pub use ecdsa::{
    generate_signer, private_key_bytes, public_key_bytes, signer_address, signer_from_bytes,
    signer_from_hex,
};
pub use rsa::{
    RSA_KEY_BITS, decrypt_pkcs1v15, encrypt_pkcs1v15, generate_rsa_key, pkix_to_public_key,
    private_to_public, public_key_to_pkix,
};
