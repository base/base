#![doc = include_str!("../README.md")]

mod error;
pub use error::NitroHostError;

mod backend;
pub use backend::NitroBackend;

mod convert;
pub use convert::Convert;

mod registration;
pub use registration::{RegistrationChecker, RegistrationError};

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
