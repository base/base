#![doc = include_str!("../README.md")]

/// L2 block range calculation and splitting.
pub mod block_range;
/// L1/L2 RPC data fetcher.
pub mod fetcher;
/// Host trait and helpers for witness capture.
pub mod host;
mod l2_output;
pub mod rpc_types;
pub use l2_output::L2Output;
/// Witness generation traits and collectors.
pub mod witness_generation;
