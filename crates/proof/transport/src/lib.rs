#![doc = include_str!("../README.md")]

mod errors;
pub use errors::TransportError;

mod transport;
pub use transport::{TransportResult, WitnessTransport};

mod native;
pub use native::{NativeBackend, NativeTransport};

#[cfg(test)]
mod tests;
