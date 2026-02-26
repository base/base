//! L1/L2 data providers for base-consensus-derive integration.
//!
//! This module provides implementations of data providers used by the
//! derivation pipeline to access L1 and L2 block data.
//!
//! # Design Notes
//!
//! These providers are **synchronous** by design. Unlike async trait-based providers
//! in base-consensus-derive, our providers hold pre-loaded block data for stateless execution
//! within an enclave. The data is passed in at construction time, and accessor methods
//! simply return references to the cached data.
//!
//! ## Receipt Verification
//!
//! The [`L1ReceiptsFetcher`] provides a `verify_receipts()` method that must be called
//! explicitly by the caller to verify that receipts match the header's receipt root.
//! This separation allows the caller to control when verification happens, which is
//! useful when receipts are provided by an untrusted source.
//!
//! ## base-consensus-derive Integration
//!
//! These structs are designed to be easily wrapped with base-consensus-derive traits (e.g.,
//! `ChainProvider`, `L2ChainProvider`) in a future milestone. The trait integration
//! with `FetchingAttributesBuilder` will be added in Milestone 5.

mod block_info;
mod l1_receipts;
mod l2_system_config;
#[cfg(test)]
mod test_utils;
mod trie;

pub use block_info::BlockInfoWrapper;
pub use l1_receipts::L1ReceiptsFetcher;
pub use l2_system_config::L2SystemConfigFetcher;
pub use trie::{compute_l1_receipt_root, compute_receipt_root, compute_tx_root};
