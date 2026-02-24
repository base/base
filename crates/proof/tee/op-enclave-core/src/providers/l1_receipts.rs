//! L1 receipts fetcher implementation.
//!
//! This module provides a receipts fetcher for L1 blocks, used by the
//! derivation pipeline to access L1 receipts for deposit transaction parsing.

use alloy_consensus::Header;
use alloy_primitives::B256;
use base_alloy_consensus::OpReceiptEnvelope;

use super::{block_info::BlockInfoWrapper, trie::compute_receipt_root};
use crate::error::ProviderError;

/// A fetcher for L1 block information and receipts.
///
/// This struct holds a single block's header and receipts, and provides
/// methods to access them by block hash. It matches Go's `l1ReceiptsFetcher`.
#[derive(Debug, Clone)]
pub struct L1ReceiptsFetcher {
    /// The block hash.
    hash: B256,
    /// The block header.
    header: Header,
    /// The block receipts.
    receipts: Vec<OpReceiptEnvelope>,
}

impl L1ReceiptsFetcher {
    /// Creates a new `L1ReceiptsFetcher` with the given hash, header, and receipts.
    #[must_use]
    pub const fn new(hash: B256, header: Header, receipts: Vec<OpReceiptEnvelope>) -> Self {
        Self { hash, header, receipts }
    }

    /// Returns block info for the given hash.
    ///
    /// # Errors
    ///
    /// Returns `ProviderError::BlockNotFound` if the hash doesn't match.
    pub fn info_by_hash(&self, hash: B256) -> Result<BlockInfoWrapper, ProviderError> {
        if self.hash != hash {
            return Err(ProviderError::BlockNotFound(hash));
        }
        Ok(BlockInfoWrapper::new(self.hash, self.header.clone()))
    }

    /// Returns block info and receipts for the given hash.
    ///
    /// # Errors
    ///
    /// Returns `ProviderError::BlockNotFound` if the hash doesn't match.
    pub fn fetch_receipts(
        &self,
        block_hash: B256,
    ) -> Result<(BlockInfoWrapper, &[OpReceiptEnvelope]), ProviderError> {
        let info = self.info_by_hash(block_hash)?;
        Ok((info, &self.receipts))
    }

    /// Verifies that the receipts match the header's receipt root.
    ///
    /// # Errors
    ///
    /// Returns `ProviderError::InvalidReceiptRoot` if the computed root
    /// doesn't match the header's receipt root.
    pub fn verify_receipts(&self) -> Result<(), ProviderError> {
        let computed = compute_receipt_root(&self.receipts);
        let expected = self.header.receipts_root;

        if computed != expected {
            return Err(ProviderError::InvalidReceiptRoot { expected, computed });
        }
        Ok(())
    }

    /// Returns the block hash.
    #[must_use]
    pub const fn hash(&self) -> B256 {
        self.hash
    }

    /// Returns a reference to the header.
    #[must_use]
    pub const fn header(&self) -> &Header {
        &self.header
    }

    /// Returns a reference to the receipts.
    #[must_use]
    pub fn receipts(&self) -> &[OpReceiptEnvelope] {
        &self.receipts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::test_utils::test_header;

    #[test]
    fn test_info_by_hash_found() {
        let hash = B256::repeat_byte(0xAA);
        let header = test_header(12345, 1_700_000_000);
        let fetcher = L1ReceiptsFetcher::new(hash, header.clone(), vec![]);

        let info = fetcher.info_by_hash(hash).unwrap();
        assert_eq!(info.hash(), hash);
        assert_eq!(info.number_u64(), header.number);
    }

    #[test]
    fn test_info_by_hash_not_found() {
        let hash = B256::repeat_byte(0xAA);
        let wrong_hash = B256::repeat_byte(0xBB);
        let header = test_header(12345, 1_700_000_000);
        let fetcher = L1ReceiptsFetcher::new(hash, header, vec![]);

        let result = fetcher.info_by_hash(wrong_hash);
        assert!(matches!(result, Err(ProviderError::BlockNotFound(h)) if h == wrong_hash));
    }

    #[test]
    fn test_fetch_receipts() {
        let hash = B256::repeat_byte(0xAA);
        let header = test_header(12345, 1_700_000_000);
        let fetcher = L1ReceiptsFetcher::new(hash, header, vec![]);

        let (info, receipts) = fetcher.fetch_receipts(hash).unwrap();
        assert_eq!(info.hash(), hash);
        assert!(receipts.is_empty());
    }

    #[test]
    fn test_verify_receipts_empty() {
        use alloy_trie::EMPTY_ROOT_HASH;

        let hash = B256::repeat_byte(0xAA);
        let mut header = test_header(12345, 1_700_000_000);
        header.receipts_root = EMPTY_ROOT_HASH;

        let fetcher = L1ReceiptsFetcher::new(hash, header, vec![]);
        assert!(fetcher.verify_receipts().is_ok());
    }

    #[test]
    fn test_verify_receipts_mismatch() {
        let hash = B256::repeat_byte(0xAA);
        let header = test_header(12345, 1_700_000_000); // receipts_root is 0x05050505...
        let fetcher = L1ReceiptsFetcher::new(hash, header, vec![]);

        let result = fetcher.verify_receipts();
        assert!(matches!(result, Err(ProviderError::InvalidReceiptRoot { .. })));
    }
}
