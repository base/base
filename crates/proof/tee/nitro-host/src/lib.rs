#![doc = include_str!("../README.md")]

mod error;
pub use error::NitroHostError;

mod backend;
pub use backend::NitroBackend;

mod registration;
pub use registration::{RegistrationChecker, RegistrationError, ValidSigner};

mod health;
pub use health::{RegistrationHealthConfig, RegistrationHealthzRpc};

mod server;
pub use server::NitroProverServer;

mod transport;
pub use transport::NitroTransport;

#[cfg(target_os = "linux")]
mod vsock;
#[cfg(target_os = "linux")]
pub use vsock::VsockTransport;

#[cfg(test)]
pub mod test_utils;
