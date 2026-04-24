//! Host-side utilities for OP Succinct proof generation.

/// L2 block range calculation and splitting.
pub mod block_range;
mod constants;
mod contract;
/// L1/L2 RPC data fetcher.
pub mod fetcher;
/// OP Succinct host trait and helpers.
pub mod host;
mod proof;
pub mod rpc_types;
/// Execution statistics collection and formatting.
pub mod stats;
pub use constants::*;
pub use contract::*;
pub use proof::*;
/// Logging setup.
pub mod logger;
/// Prometheus metrics initialization.
pub mod metrics;
/// SP1 network proof client.
pub mod network;
pub mod proof_cache;
pub mod witness_cache;
/// Witness generation traits and collectors.
pub mod witness_generation;
pub use logger::setup_logger;
