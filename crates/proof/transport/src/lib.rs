#![doc = include_str!("../README.md")]

mod errors;
pub use errors::TransportError;

mod transport;
pub use transport::{ProofTransport, TransportResult};

#[cfg(feature = "vsock")]
mod vsock;
#[cfg(feature = "vsock")]
pub use self::vsock::VsockTransport;

/// Test utilities for in-process proof transport.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
#[cfg(any(test, feature = "test-utils"))]
pub use test_utils::NativeTransport;

#[cfg(test)]
mod tests;
