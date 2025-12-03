//! Transaction tracing utilities for Base Reth.
//!
//! This crate provides an ExEx for tracking transaction lifecycle events
//! from mempool insertion to block inclusion.

/// Transaction tracing execution extension.
pub mod tracing;
mod types;

pub use tracing::transaction_tracing_exex;
