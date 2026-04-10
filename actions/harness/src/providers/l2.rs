use std::sync::Arc;

use alloy_eips::BlockNumHash;
use alloy_primitives::map::HashMap;
use async_trait::async_trait;
use base_alloy_consensus::BaseBlock;
use base_consensus_derive::{L2ChainProvider, PipelineError, PipelineErrorKind};
use base_consensus_genesis::{RollupConfig, SystemConfig};
use base_protocol::{BatchValidationProvider, BlockInfo, L2BlockInfo};

/// Error type for [`ActionL2ChainProvider`].
#[derive(Debug, thiserror::Error)]
pub enum L2ProviderError {
    /// L2 block not found.
    #[error("L2 block not found: {0}")]
    BlockNotFound(u64),
    /// System config not found.
    #[error("system config not found for L2 block {0}")]
    SystemConfigNotFound(u64),
}

impl From<L2ProviderError> for PipelineErrorKind {
    fn from(e: L2ProviderError) -> Self {
        PipelineError::Provider(e.to_string()).temp()
    }
}

/// In-memory L2 chain provider for action tests.
///
/// Implements [`L2ChainProvider`] and [`BatchValidationProvider`] using
/// maps keyed by block number. Tests pre-populate it via [`insert_block`] and
/// [`insert_system_config`].
///
/// The genesis L2 block and its system config must be inserted before the
/// pipeline is stepped for the first time; [`ActionL2ChainProvider::from_genesis`]
/// handles this automatically.
///
/// [`insert_block`]: ActionL2ChainProvider::insert_block
/// [`insert_system_config`]: ActionL2ChainProvider::insert_system_config
#[derive(Debug, Clone, Default)]
pub struct ActionL2ChainProvider {
    /// L2 blocks by block number.
    blocks: HashMap<u64, L2BlockInfo>,
    /// Op blocks (headers + txs) by block number, needed for batch validation.
    op_blocks: HashMap<u64, BaseBlock>,
    /// System configs by L2 block number.
    system_configs: HashMap<u64, SystemConfig>,
}

impl ActionL2ChainProvider {
    /// Create an [`ActionL2ChainProvider`] pre-populated with the L2 genesis block.
    ///
    /// The genesis [`L2BlockInfo`] is derived from the rollup config's genesis
    /// fields, and the genesis [`SystemConfig`] is taken from
    /// `rollup_config.genesis.system_config`.
    pub fn from_genesis(rollup_config: &RollupConfig) -> Self {
        let mut provider = Self::default();

        let genesis_l2 = L2BlockInfo {
            block_info: BlockInfo {
                hash: rollup_config.genesis.l2.hash,
                number: rollup_config.genesis.l2.number,
                parent_hash: Default::default(),
                timestamp: rollup_config.genesis.l2_time,
            },
            l1_origin: BlockNumHash {
                hash: rollup_config.genesis.l1.hash,
                number: rollup_config.genesis.l1.number,
            },
            seq_num: 0,
        };

        // Use the rollup config's genesis system config, falling back to a harness
        // default with a non-zero gas_limit. `SystemConfig::default()` has gas_limit=0
        // (derived Default), which causes the production payload builder to reject all
        // transactions. Tests that use `RollupConfig::default()` (no explicit system
        // config) need a workable gas_limit to build blocks.
        let genesis_config = rollup_config
            .genesis
            .system_config
            .unwrap_or_else(|| SystemConfig { gas_limit: 30_000_000, ..Default::default() });

        provider.insert_block(genesis_l2);
        provider.insert_system_config(rollup_config.genesis.l2.number, genesis_config);
        provider
    }

    /// Insert a known L2 block into the provider.
    pub fn insert_block(&mut self, block: L2BlockInfo) {
        self.blocks.insert(block.block_info.number, block);
    }

    /// Insert a known L2 op-block (with transactions) into the provider.
    pub fn insert_op_block(&mut self, number: u64, block: BaseBlock) {
        self.op_blocks.insert(number, block);
    }

    /// Insert a system config for the given L2 block number.
    pub fn insert_system_config(&mut self, number: u64, config: SystemConfig) {
        self.system_configs.insert(number, config);
    }
}

#[async_trait]
impl BatchValidationProvider for ActionL2ChainProvider {
    type Error = L2ProviderError;

    async fn l2_block_info_by_number(
        &mut self,
        number: u64,
    ) -> Result<L2BlockInfo, L2ProviderError> {
        self.blocks.get(&number).copied().ok_or(L2ProviderError::BlockNotFound(number))
    }

    async fn block_by_number(&mut self, number: u64) -> Result<BaseBlock, L2ProviderError> {
        self.op_blocks.get(&number).cloned().ok_or(L2ProviderError::BlockNotFound(number))
    }
}

#[async_trait]
impl L2ChainProvider for ActionL2ChainProvider {
    type Error = L2ProviderError;

    async fn system_config_by_number(
        &mut self,
        number: u64,
        _rollup_config: Arc<RollupConfig>,
    ) -> Result<SystemConfig, L2ProviderError> {
        // Walk back from `number` to find the nearest config at or before this block.
        let config = (0..=number).rev().find_map(|n| self.system_configs.get(&n).copied());
        config.ok_or(L2ProviderError::SystemConfigNotFound(number))
    }
}
