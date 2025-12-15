//! Shared constants used across integration tests.
//!
//! This module centralizes configuration values and magic constants to avoid
//! duplication and make them easy to discover.

use alloy_primitives::{B256, Bytes, b256, bytes};
// Re-export NamedChain for convenient access to chain IDs.
pub use reth::chainspec::NamedChain;

// =============================================================================
// Chain Configuration
// =============================================================================

/// Chain ID for the Base Sepolia environment spun up by the harness.
///
/// This is equivalent to `NamedChain::BaseSepolia as u64`.
pub const BASE_CHAIN_ID: u64 = NamedChain::BaseSepolia as u64;

// =============================================================================
// Block Building
// =============================================================================

/// Time between blocks in seconds.
pub const BLOCK_TIME_SECONDS: u64 = 2;

/// Gas limit for blocks built by the harness.
pub const GAS_LIMIT: u64 = 200_000_000;

// =============================================================================
// Timing / Delays
// =============================================================================

/// Delay after node startup before the harness is ready.
pub const NODE_STARTUP_DELAY_MS: u64 = 500;

/// Delay between requesting and fetching a payload during block building.
pub const BLOCK_BUILD_DELAY_MS: u64 = 100;

// =============================================================================
// Engine API
// =============================================================================

/// Default JWT secret for Engine API authentication in tests.
///
/// This is an all-zeros secret used only for local testing.
pub const DEFAULT_JWT_SECRET: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000000";

// =============================================================================
// L1 Block Info (OP Stack)
// =============================================================================

/// Pre-captured L1 block info deposit transaction required by OP Stack.
///
/// Every OP Stack block must start with an L1 block info deposit. This is a
/// sample transaction suitable for Base Sepolia tests.
pub const L1_BLOCK_INFO_DEPOSIT_TX: Bytes = bytes!(
    "0x7ef90104a06c0c775b6b492bab9d7e81abdf27f77cafb698551226455a82f559e0f93fea3794deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8b0098999be000008dd00101c1200000000000000020000000068869d6300000000015f277f000000000000000000000000000000000000000000000000000000000d42ac290000000000000000000000000000000000000000000000000000000000000001abf52777e63959936b1bf633a2a643f0da38d63deffe49452fed1bf8a44975d50000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c9000000000000000000000000"
);

/// Hash of the L1 block info deposit transaction.
pub const L1_BLOCK_INFO_DEPOSIT_TX_HASH: B256 =
    b256!("0xba56c8b0deb460ff070f8fca8e2ee01e51a3db27841cc862fdd94cc1a47662b6");
