use alloc::vec::Vec;
use core::ops::RangeBounds;

use alloy_eips::BlockHashOrNumber;
use alloy_primitives::{BlockHash, BlockNumber};
use reth_primitives_traits::SealedHeader;
use reth_storage_errors::provider::ProviderResult;

/// A helper type alias to access [`Headeralloy_consensus::Header`].
pub type ProviderHeader = alloy_consensus::Header;

/// Client trait for fetching `Header` related data.
#[auto_impl::auto_impl(&, Arc)]
pub trait HeaderProvider: Send {
    /// Check if block is known
    fn is_known(&self, block_hash: BlockHash) -> ProviderResult<bool> {
        self.header(block_hash).map(|header| header.is_some())
    }

    /// Get header by block hash
    fn header(&self, block_hash: BlockHash) -> ProviderResult<Option<alloy_consensus::Header>>;

    /// Retrieves the header sealed by the given block hash.
    fn sealed_header_by_hash(&self, block_hash: BlockHash) -> ProviderResult<Option<SealedHeader>> {
        Ok(self.header(block_hash)?.map(|header| SealedHeader::new(header, block_hash)))
    }

    /// Get header by block number
    fn header_by_number(&self, num: u64) -> ProviderResult<Option<alloy_consensus::Header>>;

    /// Get header by block number or hash
    fn header_by_hash_or_number(
        &self,
        hash_or_num: BlockHashOrNumber,
    ) -> ProviderResult<Option<alloy_consensus::Header>> {
        match hash_or_num {
            BlockHashOrNumber::Hash(hash) => self.header(hash),
            BlockHashOrNumber::Number(num) => self.header_by_number(num),
        }
    }

    /// Get headers in range of block numbers
    fn headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<alloy_consensus::Header>>;

    /// Get a single sealed header by block number.
    fn sealed_header(&self, number: BlockNumber) -> ProviderResult<Option<SealedHeader>>;

    /// Get headers in range of block numbers.
    fn sealed_headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<SealedHeader>> {
        self.sealed_headers_while(range, |_| true)
    }

    /// Get sealed headers while `predicate` returns `true` or the range is exhausted.
    fn sealed_headers_while(
        &self,
        range: impl RangeBounds<BlockNumber>,
        predicate: impl FnMut(&SealedHeader) -> bool,
    ) -> ProviderResult<Vec<SealedHeader>>;
}
