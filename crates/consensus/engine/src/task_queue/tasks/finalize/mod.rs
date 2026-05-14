//! Error types for finalizing an L2 block.

mod error;
pub use error::FinalizeTaskError;

#[cfg(test)]
mod direct_test;
