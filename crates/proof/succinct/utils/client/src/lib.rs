//! Succinct zkVM client utilities for OP Stack proof generation.

pub mod boot;

mod oracle;
pub use oracle::BlobStore;

/// FPVM-accelerated precompile providers.
pub mod precompiles;

/// Shared types for range and aggregation programs.
pub mod types;

extern crate alloc;

/// High-level client helpers for derivation and execution.
pub mod client;

/// Witness data, preimage storage, and block execution.
pub mod witness;
