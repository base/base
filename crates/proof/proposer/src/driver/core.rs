//! Core driver types for the proposer.
//!
//! Contains configuration and on-chain state recovery types shared by
//! the [`super::ProvingPipeline`].

use std::time::Duration;

use alloy_primitives::{Address, B256, U256};

/// Driver configuration.
#[derive(Debug, Clone)]
pub struct DriverConfig {
    /// Polling interval for new blocks.
    pub poll_interval: Duration,
    /// Number of L2 blocks between proposals (read from `AggregateVerifier` at startup).
    pub block_interval: u64,
    /// Number of L2 blocks between intermediate output root checkpoints.
    pub intermediate_block_interval: u64,
    /// ETH bond required to create a dispute game.
    pub init_bond: U256,
    /// Game type ID for `AggregateVerifier` dispute games.
    pub game_type: u32,
    /// If true, use `safe_l2` (derived from L1 but L1 not yet finalized).
    /// If false (default), use `finalized_l2` (derived from finalized L1).
    pub allow_non_finalized: bool,
    /// Address of the proposer that submits proof transactions on-chain.
    /// Included in the proof journal so the enclave signs over the correct `msg.sender`.
    pub proposer_address: Address,
    /// Keccak256 hash of the expected enclave PCR0 measurement.
    /// Passed to the prover in each proof request so multi-enclave provers
    /// can select the correct enclave.
    pub tee_image_hash: B256,
    /// Address of the `AnchorStateRegistry` contract on L1.
    /// Used as the "no parent" sentinel when creating the first game from anchor state.
    pub anchor_state_registry_address: Address,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(12),
            block_interval: 512,
            intermediate_block_interval: 512,
            init_bond: U256::ZERO,
            game_type: 0,
            allow_non_finalized: false,
            proposer_address: Address::ZERO,
            tee_image_hash: B256::ZERO,
            anchor_state_registry_address: Address::ZERO,
        }
    }
}

/// On-chain state recovered by the pipeline.
///
/// This is either a game found in the `DisputeGameFactory` or the
/// anchor root from the `AnchorStateRegistry` when no games exist.
#[derive(Debug, Clone, Copy)]
pub struct RecoveredState {
    /// Proxy address of the parent game, or the `AnchorStateRegistry` address
    /// when creating the first game from anchor state (no parent game exists).
    pub parent_address: Address,
    /// Output root claimed by the game or anchor state.
    pub output_root: B256,
    /// L2 block number of the claim.
    pub l2_block_number: u64,
}
