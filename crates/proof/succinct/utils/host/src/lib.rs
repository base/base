#![doc = include_str!("../README.md")]

mod constants;
mod contract;
mod proof;
/// Execution statistics collection and formatting.
pub mod stats;
mod stdin;
pub use constants::*;
pub use contract::*;
pub use proof::*;
pub use stdin::get_sp1_stdin;
/// Logging setup.
pub mod logger;
/// Prometheus metrics initialization.
pub mod metrics;
/// SP1 network proof client.
pub mod network;
pub mod proof_cache;
pub mod witness_cache;
pub use logger::setup_logger;
