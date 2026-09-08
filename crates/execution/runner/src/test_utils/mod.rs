//! Test utilities for integration testing.
//!
//! This module provides testing infrastructure including:
//! - [`TestHarness`] and [`TestHarnessBuilder`] - Unified test harness for node and engine.
//! - [`LocalNode`] and [`LocalNodeProvider`] - Local node setup.
//! - [`EngineApi`] - Direct execution commands.
//! - Test constants and fixtures.

mod constants;
pub use constants::{
    BLOCK_BUILD_DELAY_MS, BLOCK_TIME_SECONDS, GAS_LIMIT, L1_BLOCK_INFO_DEPOSIT_TX,
    L1_BLOCK_INFO_DEPOSIT_TX_HASH, NODE_STARTUP_DELAY_MS, TEST_ACCOUNT_BALANCE_ETH,
};

mod engine;
pub use engine::EngineApi;

mod fixtures;
pub use fixtures::{create_provider_factory, load_chain_spec};

mod harness;
pub use harness::{PreparedBlock, TestHarness, TestHarnessBuilder};

mod node;
pub use node::{LocalNode, LocalNodeProvider};

mod tracing;
pub use tracing::init_silenced_tracing;
