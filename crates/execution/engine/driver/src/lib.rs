#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

/// Support for backfill sync mode.
pub mod backfill;
/// The type that drives the chain forward.
pub mod chain;
/// Support for downloading blocks on demand for live sync.
pub mod download;
/// Engine Api chain handler support.
pub mod engine;
/// Engine orchestrator launch helper.
pub mod launch;
/// Metrics support.
pub mod metrics;
/// The background writer service, coordinating write operations on static files and the database.
pub mod persistence;
/// Support for interacting with the blockchain tree.
pub mod tree;
pub use tree::{BlockValidationMetrics, EngineMetrics, ReorgMetrics, TreeMetrics};

/// Test utilities.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

#[cfg(test)]
pub use tree::payload_processor::bal::execute::L1InfoDeposit;

mod miner;
pub use miner::{LocalMiner, MiningMode};
