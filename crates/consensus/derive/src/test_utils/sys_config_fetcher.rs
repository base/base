//! Implements a mock [`L2ChainProvider`] and [`BatchValidationProvider`] for testing.

use alloc::{boxed::Box, string::ToString, sync::Arc};

use alloy_primitives::{B256, map::HashMap};
use async_trait::async_trait;
use base_common_consensus::BaseBlock;
use base_common_genesis::{RollupConfig, SystemConfig};
use base_protocol::{BatchValidationProvider, L2BlockInfo};
use thiserror::Error;

use crate::{
    errors::{PipelineError, PipelineErrorKind},
    traits::L2ChainProvider,
};

/// A mock implementation of the [`L2ChainProvider`] and [`BatchValidationProvider`] for testing.
#[derive(Debug, Default)]
pub struct TestSystemConfigL2Fetcher {
    /// A map from [u64] block number to a [`SystemConfig`].
    pub system_configs: HashMap<u64, SystemConfig>,
    /// A map from L2 block hash to a [`SystemConfig`].
    pub system_configs_by_hash: HashMap<B256, SystemConfig>,
}

impl TestSystemConfigL2Fetcher {
    /// Inserts a new system config into the mock fetcher with the given block number.
    pub fn insert(&mut self, number: u64, config: SystemConfig) {
        self.system_configs.insert(number, config);
    }

    /// Inserts a new system config into the mock fetcher with the given L2 block hash.
    pub fn insert_by_hash(&mut self, hash: B256, config: SystemConfig) {
        self.system_configs_by_hash.insert(hash, config);
    }

    /// Clears all system configs from the mock fetcher.
    pub fn clear(&mut self) {
        self.system_configs.clear();
        self.system_configs_by_hash.clear();
    }
}

/// An error returned by the [`TestSystemConfigL2Fetcher`].
#[derive(Error, Debug, PartialEq, Eq)]
pub enum TestSystemConfigL2FetcherError {
    /// The system config was not found.
    #[error("system config not found: {0}")]
    NotFound(u64),
    /// The system config was not found by hash.
    #[error("system config not found: {0}")]
    HashNotFound(B256),
}

impl From<TestSystemConfigL2FetcherError> for PipelineErrorKind {
    fn from(val: TestSystemConfigL2FetcherError) -> Self {
        PipelineError::Provider(val.to_string()).temp()
    }
}

#[async_trait]
impl BatchValidationProvider for TestSystemConfigL2Fetcher {
    type Error = TestSystemConfigL2FetcherError;

    async fn block_by_number(&mut self, _: u64) -> Result<BaseBlock, Self::Error> {
        unimplemented!()
    }

    async fn l2_block_info_by_number(&mut self, _: u64) -> Result<L2BlockInfo, Self::Error> {
        unimplemented!()
    }
}

#[async_trait]
impl L2ChainProvider for TestSystemConfigL2Fetcher {
    type Error = TestSystemConfigL2FetcherError;

    async fn system_config_by_number(
        &mut self,
        number: u64,
        _: Arc<RollupConfig>,
    ) -> Result<SystemConfig, <Self as L2ChainProvider>::Error> {
        self.system_configs
            .get(&number)
            .copied()
            .ok_or_else(|| TestSystemConfigL2FetcherError::NotFound(number))
    }

    async fn l2_block_info_by_hash(
        &mut self,
        _: B256,
    ) -> Result<L2BlockInfo, <Self as L2ChainProvider>::Error> {
        unimplemented!()
    }

    async fn system_config_by_hash(
        &mut self,
        hash: B256,
        _: Arc<RollupConfig>,
    ) -> Result<SystemConfig, <Self as L2ChainProvider>::Error> {
        self.system_configs_by_hash
            .get(&hash)
            .copied()
            .ok_or_else(|| TestSystemConfigL2FetcherError::HashNotFound(hash))
    }
}
