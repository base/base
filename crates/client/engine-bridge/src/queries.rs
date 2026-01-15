//! Block query methods.
//!
//! This module contains the block info query method implementations
//! for [`InProcessEngineDriver`].

use alloy_primitives::B256;
use base_engine_driver::DirectEngineError;
use kona_protocol::{BlockInfo as KonaBlockInfo, L2BlockInfo};
use reth_provider::BlockNumReader;
use reth_storage_api::{BlockHashReader, HeaderProvider};
use tracing::debug;

use crate::InProcessEngineDriver;

impl<P> InProcessEngineDriver<P>
where
    P: BlockNumReader + HeaderProvider + BlockHashReader + Send + Sync + 'static,
{
    /// Queries L2 block info by block number.
    pub(crate) async fn handle_l2_block_info_by_number(
        &self,
        number: u64,
    ) -> Result<Option<L2BlockInfo>, DirectEngineError> {
        debug!(%number, "Querying L2 block info by number");

        let hash = self
            .provider
            .block_hash(number)
            .map_err(|e| DirectEngineError::Internal(e.to_string()))?;

        // For now, return a basic L2BlockInfo
        // Full implementation would extract L1 origin info from the block
        Ok(hash.map(|hash| L2BlockInfo {
            block_info: KonaBlockInfo { number, hash, ..Default::default() },
            ..Default::default()
        }))
    }

    /// Queries the latest L2 block info.
    pub(crate) async fn handle_l2_block_info_latest(
        &self,
    ) -> Result<Option<L2BlockInfo>, DirectEngineError> {
        debug!("Querying latest L2 block info");

        let latest = self
            .provider
            .last_block_number()
            .map_err(|e| DirectEngineError::Internal(e.to_string()))?;

        self.handle_l2_block_info_by_number(latest).await
    }

    /// Queries L2 block info by block hash.
    pub(crate) async fn handle_l2_block_info_by_hash(
        &self,
        hash: B256,
    ) -> Result<Option<L2BlockInfo>, DirectEngineError> {
        debug!(?hash, "Querying L2 block info by hash");

        let number = self
            .provider
            .block_number(hash)
            .map_err(|e| DirectEngineError::Internal(e.to_string()))?;

        Ok(number.map(|num| L2BlockInfo {
            block_info: KonaBlockInfo { number: num, hash, ..Default::default() },
            ..Default::default()
        }))
    }

    /// Gets the L2 safe head.
    pub(crate) async fn handle_l2_safe_head(
        &self,
    ) -> Result<Option<L2BlockInfo>, DirectEngineError> {
        // For now, return the latest block as the safe head
        // Full implementation would track safe head separately
        self.handle_l2_block_info_latest().await
    }

    /// Gets the L2 finalized head.
    pub(crate) async fn handle_l2_finalized_head(
        &self,
    ) -> Result<Option<L2BlockInfo>, DirectEngineError> {
        // For now, return the latest block as finalized
        // Full implementation would track finalized head separately
        self.handle_l2_block_info_latest().await
    }
}
