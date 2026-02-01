//! System/E2E test infrastructure for Base node.
//!
//! This crate provides a programmatic way to spin up complete L1+L2 devnet stacks
//! using testcontainers for container orchestration. It enables writing system tests
//! that span multiple components: builder, consensus, and client.
//!
//! # Overview
//!
//! The infrastructure is organized into layers:
//!
//! - **Config**: Genesis generation, accounts, JWT secrets
//! - **L1**: Reth execution layer + Lighthouse consensus layer containers
//! - **Deployer**: op-deployer for L2 contract deployment
//! - **L2**: Builder, client, op-node, and batcher containers
//! - **Devnet**: Top-level orchestration composing all layers
//!
//! # Example
//!
//! ```rust,ignore
//! use system_tests::{Devnet, DevnetBuilder};
//!
//! #[tokio::test]
//! #[ignore = "requires Docker"]
//! async fn test_full_devnet() -> eyre::Result<()> {
//!     let devnet = DevnetBuilder::new()
//!         .with_l1_chain_id(1337)
//!         .with_l2_chain_id(84538453)
//!         .build()
//!         .await?;
//!     
//!     // Use devnet.l1_provider(), devnet.l2_provider(), etc.
//!     
//!     Ok(()) // Devnet cleans up on drop
//! }
//! ```

#![warn(missing_docs)]
#![warn(unused_crate_dependencies)]

pub mod config;

/// Generates a unique container name with the given prefix.
pub fn unique_name(prefix: &str) -> String {
    format!("{}-{}", prefix, nanoid::nanoid!(8))
}
/// Container orchestration and management.
pub mod containers;
pub mod deployer;
/// Docker images used for testing.
pub mod images;
pub mod l1;
pub mod l2;
/// Network management for test containers.
pub mod network;
/// Test setup and environment generation.
pub mod setup;
/// High-level devnet orchestration and smoke tests.
pub mod smoke;

pub use setup::{L1GenesisOutput, L2DeploymentOutput, SetupContainer};
pub use smoke::{Devnet, DevnetBuilder};
