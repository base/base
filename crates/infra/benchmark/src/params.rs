//! Devnet rollup parameters and hardcoded keys for local benchmarking.

use alloy_primitives::{address, b256, Address, B256, U256};

/// Maximum allowed sequencer clock drift in seconds.
pub const MAX_SEQUENCER_DRIFT: u64 = 20;
/// Sequencer window size in blocks.
pub const SEQ_WINDOW_SIZE: u64 = 24;
/// Channel timeout in blocks.
pub const CHANNEL_TIMEOUT: u64 = 120;
/// L1 chain ID used for the devnet.
pub const L1_CHAIN_ID: u64 = 1;
/// Batch inbox address on L1.
pub const BATCH_INBOX_ADDRESS: Address = address!("0000000000000000000000000000000000000001");
/// EIP-1559 elasticity multiplier (Holocene).
pub const EIP1559_ELASTICITY: u64 = 50;
/// EIP-1559 base fee denominator (Holocene).
pub const EIP1559_DENOMINATOR: u64 = 1;

/// Address that receives transaction fees.
pub const SUGGESTED_FEE_RECIPIENT: Address =
    address!("4200000000000000000000000000000000000011");

/// Default block gas limit (30M).
pub const DEFAULT_GAS_LIMIT: u64 = 30_000_000;
/// Gas limit used during account-setup phase (1B).
pub const SETUP_GAS_LIMIT: u64 = 1_000_000_000;

/// Hardhat account #0 private key (batcher).
pub const BATCHER_KEY: B256 =
    b256!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

/// Hardhat account #1 private key (prefund).
pub const PREFUND_KEY: B256 =
    b256!("59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d");

/// Compute the total prefund amount (1M ETH in wei).
pub fn prefund_amount() -> U256 {
    U256::from(1_000_000u64) * U256::from(10u64).pow(U256::from(18u64))
}
