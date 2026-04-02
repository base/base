#![doc = include_str!("../README.md")]

mod error;
pub use error::NitroHostError;

mod backend;
pub use backend::NitroBackend;

mod convert;
pub use convert::Convert;

mod health;
pub use health::RegistrationHealthConfig;

mod server;
pub use server::NitroProverServer;

mod transport;
pub use base_proof_tee_nitro_enclave::{Server as EnclaveServer, VSOCK_PORT};
pub use transport::NitroTransport;

#[cfg(target_os = "linux")]
mod vsock;
#[cfg(target_os = "linux")]
pub use vsock::VsockTransport;
