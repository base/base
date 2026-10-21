//! Traits for working with protocol types.

use alloc::boxed::Box;
use core::fmt::Display;

use alloy_primitives::B256;
use async_trait::async_trait;
use base_common_consensus::BaseBlock;

use crate::L2BlockInfo;

/// Describes the functionality of a data source that fetches safe blocks.
#[async_trait]
pub trait BatchValidationProvider {
    /// The error type for the [`BatchValidationProvider`].
    type Error: Display;

    /// Returns the canonical [`L2BlockInfo`] at the given block number.
    ///
    /// The returned block must reflect the canonical chain at this height. Prefer
    /// [`Self::l2_block_info_by_hash`] when the caller already has the block hash.
    ///
    /// Errors if the block does not exist.
    async fn l2_block_info_by_number(&mut self, number: u64) -> Result<L2BlockInfo, Self::Error>;

    /// Returns the [`L2BlockInfo`] with the given block hash.
    ///
    /// Errors if the block does not exist.
    async fn l2_block_info_by_hash(&mut self, hash: B256) -> Result<L2BlockInfo, Self::Error>;

    /// Returns the [`BaseBlock`] for a given number.
    ///
    /// Errors if no block is available for the given block number.
    async fn block_by_number(&mut self, number: u64) -> Result<BaseBlock, Self::Error>;
}
