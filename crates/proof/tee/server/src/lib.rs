#![doc = include_str!("../README.md")]
#![warn(missing_docs)]
#![warn(unreachable_pub)]
#![deny(unused_must_use)]
#![deny(rust_2018_idioms)]

mod enclave;
pub use enclave::run as run_enclave;

mod proxy;
pub use proxy::run as run_proxy;

pub mod attestation;
pub mod crypto;
pub mod error;
pub mod nsm;
pub mod rpc;
mod server;
pub mod transport;
pub use attestation::{
    AttestationDocument, VerificationResult, verify_attestation, verify_attestation_with_pcr0,
};
pub use base_enclave::*;
pub use crypto::{
    SIGNATURE_LENGTH, SIGNING_DATA_BASE_LENGTH, build_signing_data, decrypt_pkcs1v15,
    encrypt_pkcs1v15, generate_rsa_key, generate_signer, pkix_to_public_key, private_key_bytes,
    public_key_bytes, public_key_to_pkix, sign_proposal_data_sync, signer_from_bytes,
    signer_from_hex, verify_proposal_signature,
};
pub use error::{AttestationError, CryptoError, NsmError, ProposalError, Result, ServerError};
pub use nsm::{NsmRng, NsmSession};
// Re-export server
pub use server::Server;
