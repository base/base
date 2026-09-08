use core::mem;

use alloy_eips::{BlockNumHash, eip1898::BlockWithParent};
use alloy_primitives::{BlockHash, keccak256};
use alloy_rlp::{Decodable, Encodable};
pub use base_common_consensus::Header;
use base_common_consensus::{BlockHeader, Sealed};
use bytes::BufMut;
use derive_more::{AsRef, Deref};

#[cfg(any(test, feature = "test-utils"))]
use crate::test_utils::TestHeader;
use crate::{InMemorySize, sync::OnceLock};

/// Seals the header with the block hash.
///
/// This type uses lazy sealing to avoid hashing the header until it is needed:
///
/// [`SealedHeader::new_unhashed`] creates a sealed header without hashing the header.
/// [`SealedHeader::new`] creates a sealed header with the corresponding block hash.
/// [`SealedHeader::hash`] computes the hash if it has not been computed yet.
#[derive(Debug, Clone, AsRef, Deref)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "reth-codec", base_common_consensus::add_arbitrary_tests(rlp))]
pub struct SealedHeader {
    /// Block hash
    #[cfg_attr(feature = "serde", serde(skip))]
    hash: OnceLock<BlockHash>,
    /// Locked Header fields.
    #[as_ref]
    #[deref]
    header: Header,
}

impl SealedHeader {
    /// Creates the sealed header without hashing the header.
    #[inline]
    pub fn new_unhashed(header: Header) -> Self {
        Self { header, hash: Default::default() }
    }

    /// Creates the sealed header with the corresponding block hash.
    #[inline]
    pub fn new(header: Header, hash: BlockHash) -> Self {
        Self { header, hash: hash.into() }
    }

    /// Returns the sealed Header fields.
    #[inline]
    pub const fn header(&self) -> &Header {
        &self.header
    }

    /// Clone the header.
    pub fn clone_header(&self) -> Header {
        self.header.clone()
    }

    /// Consumes the type and returns the wrapped header.
    #[inline]
    pub fn into_header(self) -> Header {
        self.header
    }

    /// Consumes the type and returns the wrapped header.
    #[inline]
    pub fn unseal(self) -> Header {
        self.header
    }
}

impl SealedHeader {
    /// Hashes the header and creates a sealed header.
    pub fn seal_slow(header: Header) -> Self {
        let hash = header.hash_slow();
        Self::new(header, hash)
    }

    /// Returns the block hash.
    ///
    /// Note: if the hash has not been computed yet, this will compute the hash:
    /// [`alloy_primitives::Sealable::hash_slow`].
    pub fn hash_ref(&self) -> &BlockHash {
        self.hash.get_or_init(|| self.header.hash_slow())
    }

    /// Returns a copy of the block hash.
    pub fn hash(&self) -> BlockHash {
        *self.hash_ref()
    }

    /// This is the inverse of [`Self::seal_slow`] which returns the raw header and hash.
    pub fn split(self) -> (Header, BlockHash) {
        let hash = self.hash();
        (self.header, hash)
    }

    /// Returns references to both the header and hash without taking ownership.
    pub fn split_ref(&self) -> (&Header, &BlockHash) {
        (self.header(), self.hash_ref())
    }
}

impl SealedHeader {
    /// Return the number hash tuple.
    pub fn num_hash(&self) -> BlockNumHash {
        BlockNumHash::new(self.number(), self.hash())
    }

    /// Return a [`BlockWithParent`] for this header.
    pub fn block_with_parent(&self) -> BlockWithParent {
        BlockWithParent { parent: self.parent_hash(), block: self.num_hash() }
    }

    /// Returns this header's block hash when the header commits to a block access list.
    ///
    /// The returned hash identifies the block access list data for this block. Returns `None` for
    /// headers without a `block_access_list_hash`.
    pub fn block_hash_if_block_access_list(&self) -> Option<BlockHash> {
        self.block_access_list_hash().is_some().then(|| self.hash())
    }
}

impl Eq for SealedHeader {}

impl PartialEq for SealedHeader {
    fn eq(&self, other: &Self) -> bool {
        self.hash() == other.hash()
    }
}

impl core::hash::Hash for SealedHeader {
    fn hash<Ha: core::hash::Hasher>(&self, state: &mut Ha) {
        self.hash().hash(state)
    }
}

impl InMemorySize for SealedHeader {
    /// Calculates a heuristic for the in-memory size of the [`SealedHeader`].
    #[inline]
    fn size(&self) -> usize {
        self.header.size() + mem::size_of::<BlockHash>()
    }
}

impl Default for SealedHeader {
    fn default() -> Self {
        Self::seal_slow(Header::default())
    }
}

impl Encodable for SealedHeader {
    fn encode(&self, out: &mut dyn BufMut) {
        self.header.encode(out);
    }
}

impl Decodable for SealedHeader {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let b = &mut &**buf;
        let started_len = buf.len();

        // decode the header from temp buffer
        let header = Header::decode(b)?;

        // hash the consumed bytes, the rlp encoded header
        let consumed = started_len - b.len();
        let hash = keccak256(&buf[..consumed]);

        // update original buffer
        *buf = *b;

        Ok(Self::new(header, hash))
    }
}

impl From<SealedHeader> for Sealed<Header> {
    fn from(value: SealedHeader) -> Self {
        let (header, hash) = value.split();
        Self::new_unchecked(header, hash)
    }
}

#[cfg(any(test, feature = "arbitrary"))]
impl<'a> arbitrary::Arbitrary<'a> for SealedHeader {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let header = Header::arbitrary(u)?;

        Ok(Self::seal_slow(header))
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl SealedHeader {
    /// Updates the block header.
    #[inline]
    pub fn set_header(&mut self, header: Header) {
        self.header = header
    }

    /// Updates the block hash.
    #[inline]
    pub fn set_hash(&mut self, hash: BlockHash) {
        self.hash = hash.into()
    }

    /// Returns a mutable reference to the header.
    #[inline]
    pub const fn header_mut(&mut self) -> &mut Header {
        &mut self.header
    }

    /// Updates the parent block hash.
    #[inline]
    pub fn set_parent_hash(&mut self, hash: BlockHash) {
        self.header.set_parent_hash(hash);
    }

    /// Updates the block number.
    #[inline]
    pub fn set_block_number(&mut self, number: alloy_primitives::BlockNumber) {
        self.header.set_block_number(number);
    }

    /// Updates the block timestamp.
    #[inline]
    pub fn set_timestamp(&mut self, timestamp: u64) {
        self.header.set_timestamp(timestamp);
    }

    /// Updates the block state root.
    #[inline]
    pub fn set_state_root(&mut self, state_root: alloy_primitives::B256) {
        self.header.set_state_root(state_root);
    }

    /// Updates the block difficulty.
    #[inline]
    pub fn set_difficulty(&mut self, difficulty: alloy_primitives::U256) {
        self.header.set_difficulty(difficulty);
    }
}

#[cfg(feature = "rpc-compat")]
mod rpc_compat {
    use super::*;

    impl SealedHeader {
        /// Converts this header into `alloy_rpc_types_eth::Header`.
        ///
        /// Note: This does not set the total difficulty or size of the block.
        #[inline]
        pub fn into_rpc_header(self) -> alloy_rpc_types_eth::Header<Header> {
            alloy_rpc_types_eth::Header::from_sealed(self.into())
        }

        /// Converts an `alloy_rpc_types_eth::Header` into a `SealedHeader`.
        #[inline]
        pub fn from_rpc_header(header: alloy_rpc_types_eth::Header<Header>) -> Self {
            Self::new(header.inner, header.hash)
        }
    }

    impl From<alloy_rpc_types_eth::Header<Header>> for SealedHeader {
        #[inline]
        fn from(value: alloy_rpc_types_eth::Header<Header>) -> Self {
            Self::from_rpc_header(value)
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::*;

    #[test]
    fn block_hash_if_block_access_list_returns_hash_when_header_has_bal_hash() {
        let hash = B256::with_last_byte(1);
        let header = Header { block_access_list_hash: Some(B256::ZERO), ..Default::default() };
        let sealed = SealedHeader::new(header, hash);

        assert_eq!(sealed.block_hash_if_block_access_list(), Some(hash));
    }

    #[test]
    fn block_hash_if_block_access_list_returns_none_without_bal_hash() {
        let hash = B256::with_last_byte(1);
        let sealed = SealedHeader::new(Header::default(), hash);

        assert_eq!(sealed.block_hash_if_block_access_list(), None);
    }
}
