use std::sync::Arc;

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256};
use async_trait::async_trait;
use base_common_consensus::BaseBlock;
use base_common_genesis::{RollupConfig, SystemConfig};
use base_consensus_derive::{L2ChainProvider, PipelineError, PipelineErrorKind, ResetError};
use base_protocol::{BatchValidationProvider, L2BlockInfo, to_system_config};
use reth_db::DatabaseEnv;
use reth_provider::{BlockReaderIdExt, StateProviderFactory, providers::BlockchainProvider};
use reth_storage_api::errors::ProviderError;
use reth_trie_common::HashedStorage;

/// Direct access to the execution node's canonical and in-memory L2 state.
#[derive(Debug, Clone)]
pub struct LocalL2Provider {
    /// The execution provider, including unpersisted canonical blocks.
    pub provider: BlockchainProvider<DatabaseEnv>,
    /// Configuration used to interpret L1 information in L2 blocks.
    pub rollup_config: Arc<RollupConfig>,
}

/// An error reading local execution state.
#[derive(Debug, thiserror::Error)]
pub enum LocalL2Error {
    /// The storage provider failed.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// The blocking read task failed.
    #[error(transparent)]
    ReadTask(#[from] tokio::task::JoinError),
    /// A required canonical block is missing.
    #[error("L2 block {0} not found")]
    BlockNotFound(u64),
    /// The block's L1 information is malformed.
    #[error(transparent)]
    BlockInfo(#[from] base_protocol::FromBlockError),
    /// System configuration cannot be recovered from the block.
    #[error("cannot recover system configuration from L2 block {0}")]
    SystemConfig(u64),
}

impl LocalL2Provider {
    /// Runs a storage read off the async executor, without a local RPC round trip.
    pub async fn read<T, F>(&self, read: F) -> Result<T, LocalL2Error>
    where
        T: Send + 'static,
        F: FnOnce(BlockchainProvider<DatabaseEnv>) -> Result<T, ProviderError> + Send + 'static,
    {
        let provider = self.provider.clone();
        Ok(tokio::task::spawn_blocking(move || read(provider)).await??)
    }

    /// Reads a native block using the execution provider's block-tag semantics.
    pub async fn block(&self, id: BlockId) -> Result<Option<BaseBlock>, LocalL2Error> {
        self.read(move |provider| provider.block_by_id(id)).await
    }

    /// Reads L1 origin information from a local L2 block.
    pub async fn block_info(&self, id: BlockId) -> Result<Option<L2BlockInfo>, LocalL2Error> {
        self.block(id)
            .await?
            .map(|block| {
                L2BlockInfo::from_block_and_genesis(&block, &self.rollup_config.genesis)
                    .map_err(LocalL2Error::BlockInfo)
            })
            .transpose()
    }

    /// Reads an account's storage root at the requested block.
    pub async fn storage_root(&self, id: BlockId, address: Address) -> Result<B256, LocalL2Error> {
        self.read(move |provider| {
            provider.state_by_block_id(id)?.storage_root(address, HashedStorage::default())
        })
        .await
    }
}

impl From<LocalL2Error> for PipelineErrorKind {
    fn from(error: LocalL2Error) -> Self {
        match error {
            LocalL2Error::BlockNotFound(number) => ResetError::BlockNotFound(number.into()).reset(),
            error => PipelineError::Provider(error.to_string()).temp(),
        }
    }
}

#[async_trait]
impl BatchValidationProvider for LocalL2Provider {
    type Error = LocalL2Error;

    async fn l2_block_info_by_number(&mut self, number: u64) -> Result<L2BlockInfo, Self::Error> {
        self.block_info(number.into()).await?.ok_or(LocalL2Error::BlockNotFound(number))
    }

    async fn block_by_number(&mut self, number: u64) -> Result<BaseBlock, Self::Error> {
        self.block(number.into()).await?.ok_or(LocalL2Error::BlockNotFound(number))
    }
}

#[async_trait]
impl L2ChainProvider for LocalL2Provider {
    type Error = LocalL2Error;

    async fn system_config_by_number(
        &mut self,
        number: u64,
        rollup_config: Arc<RollupConfig>,
    ) -> Result<SystemConfig, LocalL2Error> {
        let block = self.block_by_number(number).await?;
        to_system_config(&block, &rollup_config).map_err(|_| LocalL2Error::SystemConfig(number))
    }
}
