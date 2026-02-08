//! Proposer library for TEE-based output proposal generation for OP Stack chains.
//!
//! This crate provides the core functionality for the proposer, including:
//! - Contract bindings for on-chain verification
//! - RPC clients for L1, L2, and rollup nodes
//! - Enclave client for TEE proof generation
//! - Driver loop for coordinating proposal generation
//! - Metrics collection and exposition

#![warn(missing_docs)]
#![warn(unreachable_pub)]
#![deny(unused_must_use)]
#![deny(rust_2018_idioms)]

pub mod cli;
pub mod constants;
pub mod contracts;
pub mod driver;
pub mod enclave;
pub mod error;
pub mod metrics;
pub mod rpc;

pub use cli::Cli;
pub use constants::*;
pub use error::*;
