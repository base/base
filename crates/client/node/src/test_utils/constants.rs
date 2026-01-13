//! Shared constants used across integration tests.

use alloy_primitives::{B256, Bytes, b256, bytes};
pub use reth_chainspec::NamedChain;

// Block Building

/// Block time in seconds for test node configuration.
pub const BLOCK_TIME_SECONDS: u64 = 2;
/// Gas limit for test blocks.
pub const GAS_LIMIT: u64 = 200_000_000;

// Test Account Balances

/// Balance in ETH for test accounts in fixtures.
pub const TEST_ACCOUNT_BALANCE_ETH: u64 = 100;

// Timing / Delays

/// Delay in milliseconds to wait for node startup.
pub const NODE_STARTUP_DELAY_MS: u64 = 500;
/// Delay in milliseconds to wait for block building.
pub const BLOCK_BUILD_DELAY_MS: u64 = 100;

// Engine API

/// All-zeros secret for local testing only.
pub const DEFAULT_JWT_SECRET: B256 = B256::ZERO;

// L1 Block Info (OP Stack)

/// Sample L1 block info deposit transaction for Base Sepolia tests.
pub const L1_BLOCK_INFO_DEPOSIT_TX: Bytes = bytes!(
    "0x7ef90104a06c0c775b6b492bab9d7e81abdf27f77cafb698551226455a82f559e0f93fea3794deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8b0098999be000008dd00101c1200000000000000020000000068869d6300000000015f277f000000000000000000000000000000000000000000000000000000000d42ac290000000000000000000000000000000000000000000000000000000000000001abf52777e63959936b1bf633a2a643f0da38d63deffe49452fed1bf8a44975d50000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c9000000000000000000000000"
);

/// Hash of the sample L1 block info deposit transaction.
pub const L1_BLOCK_INFO_DEPOSIT_TX_HASH: B256 =
    b256!("0xba56c8b0deb460ff070f8fca8e2ee01e51a3db27841cc862fdd94cc1a47662b6");
