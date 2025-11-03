//! Common integration test utilities for node-reth crates
//!
//! This crate provides a comprehensive test framework for integration testing.
//!
//! # Quick Start
//!
//! ```no_run
//! use base_reth_test_utils::TestHarness;
//!
//! #[tokio::test]
//! async fn test_example() -> eyre::Result<()> {
//!     let harness = TestHarness::new().await?;
//!
//!     // Send flashblocks for pending state testing
//!     harness.send_flashblock(flashblock).await?;
//!
//!     // Access test accounts
//!     let alice = harness.alice();
//!     let balance = harness.get_balance(alice.address).await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! # Components
//!
//! - **TestHarness** - Unified interface combining node, engine API, and flashblocks
//! - **TestNode** - Node setup with Base Sepolia chainspec and flashblocks integration
//! - **EngineContext** - Engine API integration for canonical block production
//! - **TestAccounts** - Pre-funded test accounts (Alice, Bob, Charlie, Deployer - 10,000 ETH each)

pub mod accounts;
pub mod engine;
pub mod flashblocks;
pub mod harness;
pub mod node;

// Re-export commonly used types
pub use accounts::{TestAccount, TestAccounts};
pub use base_reth_flashblocks_rpc::subscription::{Flashblock, Metadata as FlashblockMetadata};
pub use engine::EngineApi;
pub use flashblocks::FlashblocksContext;
pub use harness::TestHarness;
pub use node::LocalNode;
