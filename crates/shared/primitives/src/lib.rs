//! Shared primitives and test utilities for node-reth crates.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

#[cfg(any(test, feature = "test-utils"))]
pub use test_utils::*;
