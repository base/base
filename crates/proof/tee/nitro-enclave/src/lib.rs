#![doc = include_str!("../README.md")]

mod error;
pub use error::{CryptoError, NitroError, NsmError, ProposalError, Result};

mod oracle;
pub use oracle::Oracle;

mod transport;
pub use transport::{Frame, TransportError, TransportResult};

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
