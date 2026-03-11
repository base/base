#![doc = include_str!("../README.md")]
#![warn(missing_docs)]
#![warn(unreachable_pub)]
#![deny(unused_must_use)]
#![deny(rust_2018_idioms)]

mod client;
pub use client::EnclaveClient;

mod client_error;
pub use client_error::ClientError;
