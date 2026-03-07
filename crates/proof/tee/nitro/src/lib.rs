#![doc = include_str!("../README.md")]

mod error;
pub use error::{AttestationError, CryptoError, NitroError, NsmError, ProposalError};

mod oracle;
pub use oracle::Oracle;

mod enclave;
// Only available on Linux (requires vsock + NSM hardware).
#[cfg(target_os = "linux")]
pub use enclave::run;
pub use enclave::{
    AttestationDocument, AwsCaRoot, DEFAULT_CA_ROOTS, DEFAULT_CA_ROOTS_SHA256, Ecdsa,
    EnclaveConfig, NsmRng, NsmSession, SIGNATURE_LENGTH, SIGNING_DATA_BASE_LENGTH, Server, Signing,
    VerificationResult, VerifyOptions, extract_public_key, get_default_ca_root, verify_attestation,
    verify_attestation_with_options, verify_attestation_with_pcr0,
    verify_attestation_with_pcr0_and_options,
};
