//! Query methods for L2 chain state.

use alloy_eips::BlockNumHash;
use alloy_primitives::B256;
use kona_protocol::{BlockInfo, L2BlockInfo};
use reth_primitives_traits::{AlloyBlockHeader as _, SealedHeader};
use reth_provider::{BlockNumReader, ProviderError};
use reth_storage_api::{BlockHashReader, HeaderProvider};
use tracing::trace;

use crate::{EngineError, InProcessEngineClient};

impl<P> InProcessEngineClient<P>
where
    P: BlockNumReader + BlockHashReader + HeaderProvider,
{
    /// Returns the L2 block info for a given block number (synchronous).
    ///
    /// This is the underlying synchronous implementation. For async usage via
    /// [`DirectEngineApi`][crate::DirectEngineApi], use the trait method instead.
    pub fn l2_block_info_by_number_sync(&self, number: u64) -> Result<L2BlockInfo, EngineError> {
        trace!(number, "Querying L2 block info by number");

        let header = self
            .provider
            .header_by_number(number)
            .map_err(|e: ProviderError| EngineError::Provider(e.to_string()))?
            .ok_or_else(|| EngineError::BlockNotFound(format!("block number {number}")))?;

        let sealed = SealedHeader::seal_slow(header);

        let parent_header = if number > 0 {
            self.provider
                .header_by_number(number - 1)
                .map_err(|e: ProviderError| EngineError::Provider(e.to_string()))?
                .map(SealedHeader::seal_slow)
        } else {
            None
        };

        // Extract L1 origin from the parent block's timestamp (if available).
        // In OP Stack, the L1 origin is encoded in the L1 attributes transaction.
        // For simplicity, we use the parent block info as a placeholder.
        let l1_origin_num = parent_header.as_ref().map(|h| h.number()).unwrap_or(0);
        let l1_origin_hash = parent_header.as_ref().map(|h| h.hash()).unwrap_or_default();

        Ok(L2BlockInfo {
            block_info: BlockInfo {
                hash: sealed.hash(),
                number: sealed.number(),
                parent_hash: sealed.parent_hash(),
                timestamp: sealed.timestamp(),
            },
            l1_origin: BlockNumHash { number: l1_origin_num, hash: l1_origin_hash },
            seq_num: 0, // Sequence number would be extracted from L1 attributes transaction.
        })
    }

    /// Returns the L2 block info for the latest block (synchronous).
    ///
    /// This is the underlying synchronous implementation. For async usage via
    /// [`DirectEngineApi`][crate::DirectEngineApi], use the trait method instead.
    pub fn l2_block_info_latest_sync(&self) -> Result<L2BlockInfo, EngineError> {
        let number =
            self.provider.last_block_number().map_err(|e| EngineError::Provider(e.to_string()))?;
        self.l2_block_info_by_number_sync(number)
    }

    /// Returns the L2 block info for a given block hash (synchronous).
    ///
    /// This is the underlying synchronous implementation. For async usage via
    /// [`DirectEngineApi`][crate::DirectEngineApi], use the trait method instead.
    pub fn l2_block_info_by_hash_sync(&self, hash: B256) -> Result<L2BlockInfo, EngineError> {
        trace!(?hash, "Querying L2 block info by hash");

        let number = self
            .provider
            .block_number(hash)
            .map_err(|e| EngineError::Provider(e.to_string()))?
            .ok_or_else(|| EngineError::BlockNotFound(format!("block hash {hash}")))?;

        self.l2_block_info_by_number_sync(number)
    }

    /// Returns the L2 safe head block info (synchronous).
    ///
    /// This relies on the forkchoice tracker being updated with FCU responses.
    /// This is the underlying synchronous implementation. For async usage via
    /// [`DirectEngineApi`][crate::DirectEngineApi], use the trait method instead.
    pub fn l2_safe_head_sync(&self) -> Result<L2BlockInfo, EngineError> {
        let safe_hash = self.forkchoice.safe_head().ok_or(EngineError::ForkchoiceNotInitialized)?;
        self.l2_block_info_by_hash_sync(safe_hash)
    }

    /// Returns the L2 finalized head block info (synchronous).
    ///
    /// This relies on the forkchoice tracker being updated with FCU responses.
    /// This is the underlying synchronous implementation. For async usage via
    /// [`DirectEngineApi`][crate::DirectEngineApi], use the trait method instead.
    pub fn l2_finalized_head_sync(&self) -> Result<L2BlockInfo, EngineError> {
        let finalized_hash =
            self.forkchoice.finalized_head().ok_or(EngineError::ForkchoiceNotInitialized)?;
        self.l2_block_info_by_hash_sync(finalized_hash)
    }

    /// Returns the L2 unsafe head block info (synchronous).
    ///
    /// This relies on the forkchoice tracker being updated with FCU responses.
    /// This is the underlying synchronous implementation. For async usage via
    /// [`DirectEngineApi`][crate::DirectEngineApi], use the trait method instead.
    pub fn l2_unsafe_head_sync(&self) -> Result<L2BlockInfo, EngineError> {
        let unsafe_hash =
            self.forkchoice.unsafe_head().ok_or(EngineError::ForkchoiceNotInitialized)?;
        self.l2_block_info_by_hash_sync(unsafe_hash)
    }
}
