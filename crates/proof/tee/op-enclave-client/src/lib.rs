//! op-enclave client implementation.
//!
//! This crate provides a client for communicating with the enclave RPC server.
//!
//! # Example
//!
//! ```ignore
//! use op_enclave_client::EnclaveClient;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let client = EnclaveClient::new("http://127.0.0.1:1234")?;
//!
//!     let public_key = client.signer_public_key().await?;
//!     println!("Signer public key: {:?}", public_key);
//!
//!     Ok(())
//! }
//! ```

#![warn(missing_docs)]
#![warn(unreachable_pub)]
#![deny(unused_must_use)]
#![deny(rust_2018_idioms)]

mod client;
mod client_error;

pub use client::EnclaveClient;
pub use client_error::ClientError;

// Re-export core types
pub use op_enclave_core::*;
