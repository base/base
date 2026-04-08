#![doc = include_str!("../README.md")]

mod error;
pub use error::{AttestationError, CryptoError, NitroError, NsmError, ProposalError, Result};

mod oracle;
pub use oracle::Oracle;

mod transport;
pub use transport::{Frame, TransportError, TransportResult};

mod types;
pub use types::{
    ECDSA_SIGNATURE_LENGTH, PROOF_JOURNAL_BASE_LENGTH, ProofJournal, Proposal, TeeProofResult,
};

mod attestation;
pub use attestation::{
    AttestationDocument, AwsCaRoot, DEFAULT_CA_ROOTS, DEFAULT_CA_ROOTS_SHA256, VerificationResult,
    get_default_ca_root, verify_attestation,
};

mod crypto;
pub use crypto::{Ecdsa, Signing};

mod nsm;
pub use nsm::{NsmRng, NsmSession};

mod protocol;
pub use protocol::{EnclaveRequest, EnclaveResponse};

mod server;
pub use server::Server;

mod runtime;
#[cfg(target_os = "linux")]
pub use runtime::NitroEnclave;
pub use runtime::VSOCK_PORT;
