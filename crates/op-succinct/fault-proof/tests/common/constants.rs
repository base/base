//! Shared constants for E2E tests.

use alloy_primitives::{address, Address, B256, U256};

// Anvil predefined accounts
pub const ANVIL_ACCOUNT_0: Address = address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
pub const ANVIL_ACCOUNT_1: Address = address!("0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

// Account private keys
pub const ANVIL_ACCOUNT_0_PRIVATE_KEY: &str =
    "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
pub const ANVIL_ACCOUNT_1_PRIVATE_KEY: &str =
    "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

// Account roles
pub const PROPOSER_ADDRESS: Address = ANVIL_ACCOUNT_0;
pub const PROPOSER_PRIVATE_KEY: &str = ANVIL_ACCOUNT_0_PRIVATE_KEY;

pub const CHALLENGER_ADDRESS: Address = ANVIL_ACCOUNT_1;
pub const CHALLENGER_PRIVATE_KEY: &str = ANVIL_ACCOUNT_1_PRIVATE_KEY;

// Default deployer is the same as proposer
pub const DEPLOYER_PRIVATE_KEY: &str = ANVIL_ACCOUNT_0_PRIVATE_KEY;

// Test configuration constants
pub const TEST_GAME_TYPE: u32 = 42; // Must match OP_SUCCINCT_FAULT_DISPUTE_GAME_TYPE in contracts
pub const INIT_BOND: U256 = U256::from_limbs([10_000_000_000_000_000, 0, 0, 0]); // 0.01 ETH
pub const CHALLENGER_BOND: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]); // 1 ETH

// Time constants
pub const DISPUTE_GAME_FINALITY_DELAY_SECONDS: u64 = 60 * 60 * 24 * 7; // 7 days
pub const MAX_CHALLENGE_DURATION: u64 = 60 * 60; // 1 hour
pub const MAX_PROVE_DURATION: u64 = 60 * 60 * 12; // 12 hours
pub const FALLBACK_TIMEOUT: U256 = U256::from_limbs([1209600, 0, 0, 0]); // 2 weeks

// Configuration hashes for OPSuccinctFaultDisputeGame
pub const ROLLUP_CONFIG_HASH: B256 = B256::ZERO; // Mock value for testing
pub const AGGREGATION_VKEY: B256 = B256::ZERO; // Mock value for testing
pub const RANGE_VKEY_COMMITMENT: B256 = B256::ZERO; // Mock value for testing

// Test configuration for L2 block offset
// This offset is subtracted from finalized L2 block to get the starting anchor block
pub const L2_BLOCK_OFFSET_FROM_FINALIZED: u64 = 100;

// Test configuration for mock permissioned games
pub const MOCK_PERMISSIONED_GAMES_TO_SEED: usize = 3;
pub const MOCK_PERMISSIONED_GAME_TYPE: u32 = 1;
