//! Flashblocks RPC extension for Base Reth.
//!
//! This crate provides RPC APIs for accessing flashblock state,
//! including pending transactions, blocks, and receipts before
//! they are finalized on-chain.

mod metrics;
mod pending_blocks;
/// RPC trait definitions and implementations for flashblocks.
pub mod rpc;
/// Flashblocks state management.
pub mod state;
/// WebSocket subscription handling for flashblocks.
pub mod subscription;

pub use pending_blocks::PendingBlocks;
