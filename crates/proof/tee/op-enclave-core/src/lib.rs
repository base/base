//! Core types for op-enclave.
//!
//! This crate provides Rust equivalents of the Go types used in op-enclave,
//! with serialization that matches the Go `encoding/json` output exactly.

pub mod serde_utils;
pub mod types;

pub use types::account::AccountResult;
pub use types::proposal::Proposal;

// Re-export commonly used types from alloy
pub use alloy_consensus::Header;
pub use alloy_primitives::{Address, B256, Bytes, U256};
pub use op_alloy_consensus::OpReceiptEnvelope;
