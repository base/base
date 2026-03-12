#![doc = include_str!("../README.md")]

mod error;
pub use error::{AttestationError, CryptoError, NitroError, NsmError, ProposalError, Result};

mod oracle;
pub use oracle::Oracle;

mod transport;
pub use transport::{Frame, TransportError, TransportResult};

mod enclave;
#[cfg(target_os = "linux")]
pub use enclave::NitroEnclave;
pub use enclave::{
    AttestationDocument, AwsCaRoot, DEFAULT_CA_ROOTS, DEFAULT_CA_ROOTS_SHA256, Ecdsa,
    EnclaveConfig, EnclaveRequest, EnclaveResponse, NsmRng, NsmSession, Server, Signing,
    VerificationResult, get_default_ca_root, verify_attestation,
};

#[cfg(feature = "host")]
mod host;
#[cfg(all(feature = "host", target_os = "linux"))]
pub use host::VsockTransport;
#[cfg(feature = "host")]
pub use host::{NitroBackend, NitroProverServer, NitroTransport};
