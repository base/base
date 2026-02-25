//! Block info wrapper providing accessor methods.
//!
//! This module provides a wrapper around `alloy_consensus::Header` that
//! implements all the accessor methods from Go's `eth.BlockInfo` interface.

use alloy_consensus::Header;
use alloy_eips::eip4844::calc_blob_gasprice;
use alloy_primitives::{Address, B256};
use alloy_rlp::Encodable;

/// A wrapper around a block header with a cached hash.
///
/// This provides all 18 accessor methods matching Go's `eth.BlockInfo` interface.
#[derive(Debug, Clone)]
pub struct BlockInfoWrapper {
    /// The cached block hash.
    hash: B256,
    /// The block header.
    header: Header,
}

impl BlockInfoWrapper {
    /// Creates a new `BlockInfoWrapper` with the given hash and header.
    #[must_use]
    pub const fn new(hash: B256, header: Header) -> Self {
        Self { hash, header }
    }

    /// Creates a new `BlockInfoWrapper`, computing the hash from the header.
    #[must_use]
    pub fn from_header(header: Header) -> Self {
        let hash = header.hash_slow();
        Self { hash, header }
    }

    /// Returns the block hash.
    #[must_use]
    pub const fn hash(&self) -> B256 {
        self.hash
    }

    /// Returns the parent block hash.
    #[must_use]
    pub const fn parent_hash(&self) -> B256 {
        self.header.parent_hash
    }

    /// Returns the coinbase (beneficiary) address.
    #[must_use]
    pub const fn coinbase(&self) -> Address {
        self.header.beneficiary
    }

    /// Returns the state root.
    #[must_use]
    pub const fn root(&self) -> B256 {
        self.header.state_root
    }

    /// Returns the block number as u64.
    #[must_use]
    pub const fn number_u64(&self) -> u64 {
        self.header.number
    }

    /// Returns the block timestamp.
    #[must_use]
    pub const fn time(&self) -> u64 {
        self.header.timestamp
    }

    /// Returns the mix digest (`prev_randao` after The Merge).
    #[must_use]
    pub const fn mix_digest(&self) -> B256 {
        self.header.mix_hash
    }

    /// Returns the base fee per gas, if present.
    #[must_use]
    pub const fn base_fee(&self) -> Option<u64> {
        self.header.base_fee_per_gas
    }

    /// Returns the blob base fee (EIP-4844), if excess blob gas is present.
    ///
    /// Calculates the blob gas price using the EIP-4844 formula.
    #[must_use]
    pub fn blob_base_fee(&self) -> Option<u128> {
        self.header.excess_blob_gas.map(calc_blob_gasprice)
    }

    /// Returns the excess blob gas, if present.
    #[must_use]
    pub const fn excess_blob_gas(&self) -> Option<u64> {
        self.header.excess_blob_gas
    }

    /// Returns the blob gas used, if present.
    #[must_use]
    pub const fn blob_gas_used(&self) -> Option<u64> {
        self.header.blob_gas_used
    }

    /// Returns the withdrawals root, if present.
    #[must_use]
    pub const fn withdrawals_root(&self) -> Option<B256> {
        self.header.withdrawals_root
    }

    /// Returns the receipts root.
    #[must_use]
    pub const fn receipt_hash(&self) -> B256 {
        self.header.receipts_root
    }

    /// Returns the gas used by the block.
    #[must_use]
    pub const fn gas_used(&self) -> u64 {
        self.header.gas_used
    }

    /// Returns the gas limit of the block.
    #[must_use]
    pub const fn gas_limit(&self) -> u64 {
        self.header.gas_limit
    }

    /// Returns the parent beacon block root, if present.
    #[must_use]
    pub const fn parent_beacon_root(&self) -> Option<B256> {
        self.header.parent_beacon_block_root
    }

    /// Returns the RLP-encoded header.
    #[must_use]
    pub fn header_rlp(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        self.header.encode(&mut buf);
        buf
    }

    /// Returns a reference to the underlying header.
    #[must_use]
    pub const fn header(&self) -> &Header {
        &self.header
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256};

    use super::BlockInfoWrapper;
    use crate::providers::test_utils::test_header;

    #[test]
    fn test_block_info_wrapper_accessors() {
        let header = test_header(12345, 1_700_000_000);
        let wrapper = BlockInfoWrapper::from_header(header);

        assert_eq!(wrapper.parent_hash(), B256::repeat_byte(0x01));
        assert_eq!(wrapper.coinbase(), Address::repeat_byte(0x02));
        assert_eq!(wrapper.root(), B256::repeat_byte(0x03));
        assert_eq!(wrapper.receipt_hash(), B256::repeat_byte(0x05));
        assert_eq!(wrapper.number_u64(), 12345);
        assert_eq!(wrapper.gas_limit(), 30_000_000);
        assert_eq!(wrapper.gas_used(), 21_000);
        assert_eq!(wrapper.time(), 1_700_000_000);
        assert_eq!(wrapper.mix_digest(), B256::repeat_byte(0x06));
        assert_eq!(wrapper.base_fee(), Some(1_000_000_000));
        assert_eq!(wrapper.withdrawals_root(), Some(B256::repeat_byte(0x07)));
        assert_eq!(wrapper.blob_gas_used(), Some(131_072));
        assert_eq!(wrapper.excess_blob_gas(), Some(0));
        assert_eq!(wrapper.parent_beacon_root(), Some(B256::repeat_byte(0x08)));

        // Verify header() returns the original header
        assert_eq!(wrapper.header().number, 12345);

        // Verify header_rlp() returns non-empty bytes
        assert!(!wrapper.header_rlp().is_empty());
    }

    #[test]
    fn test_blob_base_fee_calculation() {
        let mut header = test_header(12345, 1_700_000_000);
        header.excess_blob_gas = Some(0);
        let wrapper = BlockInfoWrapper::from_header(header);

        // At 0 excess blob gas, the blob base fee should be MIN_BLOB_GASPRICE (1)
        assert_eq!(wrapper.blob_base_fee(), Some(1));
    }

    #[test]
    fn test_blob_base_fee_none_when_missing() {
        let mut header = test_header(12345, 1_700_000_000);
        header.excess_blob_gas = None;
        let wrapper = BlockInfoWrapper::from_header(header);

        assert_eq!(wrapper.blob_base_fee(), None);
    }

    #[test]
    fn test_hash_consistency() {
        let header = test_header(12345, 1_700_000_000);
        let wrapper = BlockInfoWrapper::from_header(header.clone());

        // Hash should be consistent with slow hash
        assert_eq!(wrapper.hash(), header.hash_slow());
    }
}
