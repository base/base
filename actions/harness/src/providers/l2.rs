use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use alloy_primitives::{B256, map::HashMap};
use async_trait::async_trait;
use base_common_consensus::BaseBlock;
use base_common_genesis::{RollupConfig, SystemConfig};
use base_consensus_derive::{L2ChainProvider, PipelineError, PipelineErrorKind};
use base_protocol::{BatchValidationProvider, L2BlockInfo};

/// Error type for [`ActionL2ChainProvider`].
#[derive(Debug, thiserror::Error)]
pub enum L2ProviderError {
    /// L2 block not found.
    #[error("L2 block not found: {0}")]
    BlockNotFound(u64),
    /// System config not found.
    #[error("system config not found for L2 block {0}")]
    SystemConfigNotFound(B256),
}

impl From<L2ProviderError> for PipelineErrorKind {
    fn from(e: L2ProviderError) -> Self {
        PipelineError::Provider(e.to_string()).temp()
    }
}

/// In-memory L2 chain provider for action tests.
///
/// Implements [`L2ChainProvider`] and [`BatchValidationProvider`] using
/// blocks keyed by number and system configs keyed by L2 block hash. Tests pre-populate it via
/// [`insert_block`] and
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
    blocks: Arc<Mutex<HashMap<u64, L2BlockInfo>>>,
    /// L2 blocks by block hash, used to walk a particular fork's ancestry.
    blocks_by_hash: Arc<Mutex<HashMap<B256, L2BlockInfo>>>,
    /// Base blocks (headers + txs) by block number, needed for batch validation.
    base_blocks: Arc<Mutex<HashMap<u64, BaseBlock>>>,
    /// System configs by L2 block hash.
    system_configs: Arc<Mutex<HashMap<B256, SystemConfig>>>,
}

impl ActionL2ChainProvider {
    /// Create an [`ActionL2ChainProvider`] pre-populated with the L2 genesis block.
    ///
    /// The genesis [`L2BlockInfo`] is derived from the rollup config's genesis
    /// fields, and the genesis [`SystemConfig`] is taken from
    /// `rollup_config.genesis.system_config`.
    pub fn from_genesis(rollup_config: &RollupConfig) -> Self {
        let provider = Self::default();

        let genesis_l2 = L2BlockInfo::from_l2_genesis(&rollup_config.genesis);

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
        provider.insert_system_config(rollup_config.genesis.l2.hash, genesis_config);
        provider
    }

    /// Insert a known L2 block into the provider.
    pub fn insert_block(&self, block: L2BlockInfo) {
        self.blocks.lock().expect("L2 blocks lock poisoned").insert(block.block_info.number, block);
        self.blocks_by_hash
            .lock()
            .expect("L2 blocks by hash lock poisoned")
            .insert(block.block_info.hash, block);
    }

    /// Insert a known L2 block with transactions into the provider.
    pub fn insert_base_block(&self, number: u64, block: BaseBlock) {
        self.base_blocks.lock().expect("L2 base blocks lock poisoned").insert(number, block);
    }

    /// Insert a system config for the given L2 block hash.
    pub fn insert_system_config(&self, hash: B256, config: SystemConfig) {
        self.system_configs.lock().expect("L2 system configs lock poisoned").insert(hash, config);
    }
}

#[async_trait]
impl BatchValidationProvider for ActionL2ChainProvider {
    type Error = L2ProviderError;

    async fn l2_block_info_by_number(
        &mut self,
        number: u64,
    ) -> Result<L2BlockInfo, L2ProviderError> {
        self.blocks
            .lock()
            .expect("L2 blocks lock poisoned")
            .get(&number)
            .copied()
            .ok_or(L2ProviderError::BlockNotFound(number))
    }

    async fn block_by_number(&mut self, number: u64) -> Result<BaseBlock, L2ProviderError> {
        self.base_blocks
            .lock()
            .expect("L2 base blocks lock poisoned")
            .get(&number)
            .cloned()
            .ok_or(L2ProviderError::BlockNotFound(number))
    }
}

#[async_trait]
impl L2ChainProvider for ActionL2ChainProvider {
    type Error = L2ProviderError;

    async fn system_config_by_l2_hash(
        &mut self,
        hash: B256,
        _rollup_config: Arc<RollupConfig>,
    ) -> Result<SystemConfig, L2ProviderError> {
        let mut current_hash = hash;
        let mut visited = HashSet::new();

        loop {
            if let Some(config) = self
                .system_configs
                .lock()
                .expect("L2 system configs lock poisoned")
                .get(&current_hash)
                .copied()
            {
                return Ok(config);
            }

            if !visited.insert(current_hash) {
                return Err(L2ProviderError::SystemConfigNotFound(hash));
            }

            current_hash = self
                .blocks_by_hash
                .lock()
                .expect("L2 blocks by hash lock poisoned")
                .get(&current_hash)
                .ok_or(L2ProviderError::SystemConfigNotFound(hash))?
                .block_info
                .parent_hash;
        }
    }
}

#[cfg(test)]
mod tests {
    use base_protocol::BlockInfo;

    use super::*;

    #[tokio::test]
    async fn system_config_lookup_walks_the_requested_fork_by_hash() {
        let provider = ActionL2ChainProvider::default();
        let genesis_hash = B256::left_padding_from(&[1]);
        let child_hash = B256::left_padding_from(&[2]);
        provider.insert_system_config(
            genesis_hash,
            SystemConfig { gas_limit: 123, ..Default::default() },
        );
        provider.insert_block(L2BlockInfo {
            block_info: BlockInfo {
                hash: child_hash,
                parent_hash: genesis_hash,
                number: 1,
                ..Default::default()
            },
            ..Default::default()
        });

        let mut provider = provider;
        let config = provider
            .system_config_by_l2_hash(child_hash, Arc::new(RollupConfig::default()))
            .await
            .unwrap();
        assert_eq!(config.gas_limit, 123);
    }

    #[tokio::test]
    async fn system_config_lookup_rejects_cyclic_test_ancestry() {
        let provider = ActionL2ChainProvider::default();
        let hash = B256::left_padding_from(&[1]);
        provider.insert_block(L2BlockInfo {
            block_info: BlockInfo { hash, parent_hash: hash, number: 1, ..Default::default() },
            ..Default::default()
        });

        let mut provider = provider;
        assert!(matches!(
            provider.system_config_by_l2_hash(hash, Arc::new(RollupConfig::default())).await,
            Err(L2ProviderError::SystemConfigNotFound(actual)) if actual == hash
        ));
    }
}
