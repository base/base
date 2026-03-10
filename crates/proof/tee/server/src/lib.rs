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
pub use attestation::{AttestationDocument, VerificationResult, verify_attestation};

pub mod crypto;
pub use crypto::{
    SIGNATURE_LENGTH, SIGNING_DATA_BASE_LENGTH, build_signing_data, generate_signer,
    public_key_bytes, sign_proposal_data_sync, signer_from_hex, verify_proposal_signature,
};

pub mod error;
pub use error::{AttestationError, CryptoError, NsmError, ProposalError, Result, ServerError};

pub mod nsm;
pub use nsm::{NsmRng, NsmSession};

pub mod rpc;

mod server;
pub use server::Server;

pub mod transport;

pub use base_enclave::*;
