//! Sealed block types

use alloc::vec::Vec;
use core::ops::Deref;

use alloy_eips::{BlockNumHash, eip1898::BlockWithParent};
use alloy_primitives::{Address, B256, BlockHash, Sealed};
use alloy_rlp::{Decodable, Encodable};
use base_common_consensus::{BaseBlock, BaseBlockBody, BlockHeader as _, Header};
use bytes::BufMut;

use crate::{
    Block, BlockBody, GotExpected, InMemorySize, SealedHeader,
    block::{RecoveredBlock, error::BlockRecoveryError},
    transaction::signed::RecoveryError,
};

/// Sealed full block composed of the block's header and body.
///
/// This type uses lazy sealing to avoid hashing the header until it is needed, see also
/// [`SealedHeader`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SealedBlock {
    /// Sealed Header.
    header: SealedHeader,
    /// the block's body.
    body: BaseBlockBody,
}

impl SealedBlock {
    /// Hashes the header and creates a sealed block.
    ///
    /// This calculates the header hash. To create a [`SealedBlock`] without calculating the hash
    /// upfront see [`SealedBlock::new_unhashed`]
    pub fn seal_slow(block: BaseBlock) -> Self {
        let hash = block.header().hash_slow();
        Self::new_unchecked(block, hash)
    }

    /// Create a new sealed block instance using the block.
    ///
    /// Caution: This assumes the given hash is the block's hash.
    #[inline]
    pub fn new_unchecked(block: BaseBlock, hash: BlockHash) -> Self {
        let (header, body) = block.split();
        Self { header: SealedHeader::new(header, hash), body }
    }

    /// Creates a `SealedBlock` from the block without the available hash
    #[inline]
    pub fn new_unhashed(block: BaseBlock) -> Self {
        let (header, body) = block.split();
        Self { header: SealedHeader::new_unhashed(header), body }
    }

    /// Creates the [`SealedBlock`] from the block's parts by hashing the header.
    ///
    ///
    /// This calculates the header hash. To create a [`SealedBlock`] from its parts without
    /// calculating the hash upfront see [`SealedBlock::from_parts_unhashed`]
    pub fn seal_parts(header: Header, body: BaseBlockBody) -> Self {
        Self::seal_slow(BaseBlock::new(header, body))
    }

    /// Creates the [`SealedBlock`] from the block's parts without calculating the hash upfront.
    #[inline]
    pub fn from_parts_unhashed(header: Header, body: BaseBlockBody) -> Self {
        Self::new_unhashed(BaseBlock::new(header, body))
    }

    /// Creates the [`SealedBlock`] from the block's parts.
    #[inline]
    pub fn from_parts_unchecked(header: Header, body: BaseBlockBody, hash: BlockHash) -> Self {
        Self::new_unchecked(BaseBlock::new(header, body), hash)
    }

    /// Creates the [`SealedBlock`] from the [`SealedHeader`] and the body.
    #[inline]
    pub fn from_sealed_parts(header: SealedHeader, body: BaseBlockBody) -> Self {
        let (header, hash) = header.split();
        Self::from_parts_unchecked(header, body, hash)
    }

    /// Decodes the block from RLP and seals it.
    pub fn decode_sealed(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        BaseBlock::decode_sealed(buf).map(Into::into)
    }

    /// Returns a reference to the block hash.
    #[inline]
    pub fn hash_ref(&self) -> &BlockHash {
        self.header.hash_ref()
    }

    /// Returns the block hash.
    #[inline]
    pub fn hash(&self) -> B256 {
        self.header.hash()
    }

    /// Consumes the type and returns its components.
    #[doc(alias = "into_components")]
    #[inline]
    pub fn split(self) -> (BaseBlock, BlockHash) {
        let (header, hash) = self.header.split();
        (BaseBlock::new(header, self.body), hash)
    }

    /// Consumes the type and returns the block.
    #[inline]
    pub fn into_block(self) -> BaseBlock {
        self.unseal()
    }

    /// Consumes the type and returns the block.
    #[inline]
    pub fn unseal(self) -> BaseBlock {
        let header = self.header.unseal();
        BaseBlock::new(header, self.body)
    }

    /// Clones the wrapped block.
    pub fn clone_block(&self) -> BaseBlock {
        BaseBlock::new(self.header.clone_header(), self.body.clone())
    }

    /// Converts this block into a [`RecoveredBlock`] with the given senders
    ///
    /// Note: This method assumes the senders are correct and does not validate them.
    #[inline]
    pub const fn with_senders(self, senders: Vec<Address>) -> RecoveredBlock {
        RecoveredBlock::new_sealed(self, senders)
    }

    /// Converts this block into a [`RecoveredBlock`] with the given senders if the number of
    /// senders is equal to the number of transactions in the block and recovers the senders from
    /// the transactions, if
    /// not using [`SignedTransaction::recover_signer`](crate::transaction::signed::SignedTransaction)
    /// to recover the senders.
    ///
    /// Returns an error if any of the transactions fail to recover the sender.
    pub fn try_with_senders(
        self,
        senders: Vec<Address>,
    ) -> Result<RecoveredBlock, BlockRecoveryError<Self>> {
        RecoveredBlock::try_recover_sealed_with_senders(self, senders)
    }

    /// Converts this block into a [`RecoveredBlock`] with the given senders if the number of
    /// senders is equal to the number of transactions in the block and recovers the senders from
    /// the transactions, if
    /// not using [`SignedTransaction::recover_signer_unchecked`](crate::transaction::signed::SignedTransaction)
    /// to recover the senders.
    ///
    /// Returns an error if any of the transactions fail to recover the sender.
    pub fn try_with_senders_unchecked(
        self,
        senders: Vec<Address>,
    ) -> Result<RecoveredBlock, BlockRecoveryError<Self>> {
        RecoveredBlock::try_recover_sealed_with_senders_unchecked(self, senders)
    }

    /// Recovers the senders from the transactions in the block using
    /// [`SignedTransaction::recover_signer`](crate::transaction::signed::SignedTransaction).
    ///
    /// Returns an error if any of the transactions fail to recover the sender.
    pub fn try_recover(self) -> Result<RecoveredBlock, BlockRecoveryError<Self>> {
        RecoveredBlock::try_recover_sealed(self)
    }

    /// Recovers the senders from the transactions in the block using
    /// [`SignedTransaction::recover_signer_unchecked`](crate::transaction::signed::SignedTransaction).
    ///
    /// Returns an error if any of the transactions fail to recover the sender.
    pub fn try_recover_unchecked(self) -> Result<RecoveredBlock, BlockRecoveryError<Self>> {
        RecoveredBlock::try_recover_sealed_unchecked(self)
    }

    /// Returns reference to block header.
    #[inline]
    pub const fn header(&self) -> &Header {
        self.header.header()
    }

    /// Returns reference to block body.
    #[inline]
    pub const fn body(&self) -> &BaseBlockBody {
        &self.body
    }

    /// Returns the length of the block.
    pub fn rlp_length(&self) -> usize {
        BaseBlock::rlp_length(self.header(), self.body())
    }

    /// Recovers all senders from the transactions in the block.
    ///
    /// Returns an error if any of the transactions fail to recover the sender.
    pub fn senders(&self) -> Result<Vec<Address>, RecoveryError> {
        self.body().recover_signers()
    }

    /// Return the number hash tuple.
    #[inline]
    pub fn num_hash(&self) -> BlockNumHash {
        BlockNumHash::new(self.number(), self.hash())
    }

    /// Return a [`BlockWithParent`] for this header.
    #[inline]
    pub fn block_with_parent(&self) -> BlockWithParent {
        BlockWithParent { parent: self.parent_hash(), block: self.num_hash() }
    }

    /// Returns the Sealed header.
    #[inline]
    pub const fn sealed_header(&self) -> &SealedHeader {
        &self.header
    }

    /// Clones the wrapped header and returns a [`SealedHeader`] sealed with the hash.
    pub fn clone_sealed_header(&self) -> SealedHeader {
        self.header.clone()
    }

    /// Consumes the block and returns the sealed header.
    #[inline]
    pub fn into_sealed_header(self) -> SealedHeader {
        self.header
    }

    /// Consumes the block and returns the header.
    #[inline]
    pub fn into_header(self) -> Header {
        self.header.unseal()
    }

    /// Consumes the block and returns the body.
    #[inline]
    pub fn into_body(self) -> BaseBlockBody {
        self.body
    }

    /// Splits the block into body and header into separate components
    #[inline]
    pub fn split_header_body(self) -> (Header, BaseBlockBody) {
        let header = self.header.unseal();
        (header, self.body)
    }

    /// Splits the block into body and header into separate components.
    pub fn split_sealed_header_body(self) -> (SealedHeader, BaseBlockBody) {
        (self.header, self.body)
    }

    /// Returns an iterator over all blob versioned hashes from the block body.
    #[inline]
    pub fn blob_versioned_hashes_iter(&self) -> impl Iterator<Item = &B256> + '_ {
        self.body().blob_versioned_hashes_iter()
    }

    /// Returns the number of transactions in the block.
    #[inline]
    pub fn transaction_count(&self) -> usize {
        self.body().transaction_count()
    }

    /// Ensures that the transaction root in the block header is valid.
    ///
    /// The transaction root is the Keccak 256-bit hash of the root node of the trie structure
    /// populated with each transaction in the transactions list portion of the block.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if the calculated transaction root matches the one stored in the header,
    /// indicating that the transactions in the block are correctly represented in the trie.
    ///
    /// Returns `Err(error)` if the transaction root validation fails, providing a `GotExpected`
    /// error containing the calculated and expected roots.
    pub fn ensure_transaction_root_valid(&self) -> Result<(), GotExpected<B256>> {
        let calculated_root = self.body().calculate_tx_root();

        if self.header().transactions_root() != calculated_root {
            return Err(GotExpected {
                got: calculated_root,
                expected: self.header().transactions_root(),
            });
        }

        Ok(())
    }
}

impl From<BaseBlock> for SealedBlock {
    #[inline]
    fn from(block: BaseBlock) -> Self {
        Self::seal_slow(block)
    }
}

impl Default for SealedBlock {
    #[inline]
    fn default() -> Self {
        Self::seal_slow(Default::default())
    }
}

impl InMemorySize for SealedBlock {
    #[inline]
    fn size(&self) -> usize {
        self.body.size() + self.header.size()
    }
}

impl Deref for SealedBlock {
    type Target = Header;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.header()
    }
}

impl Encodable for SealedBlock {
    fn encode(&self, out: &mut dyn BufMut) {
        BaseBlock::rlp_encode(self.header(), self.body(), out);
    }

    fn length(&self) -> usize {
        self.rlp_length()
    }
}

impl Decodable for SealedBlock {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        BaseBlock::decode_sealed(buf).map(Into::into)
    }
}

impl From<SealedBlock> for Sealed<BaseBlock> {
    #[inline]
    fn from(value: SealedBlock) -> Self {
        let (block, hash) = value.split();
        Self::new_unchecked(block, hash)
    }
}

impl From<Sealed<BaseBlock>> for SealedBlock {
    #[inline]
    fn from(value: Sealed<BaseBlock>) -> Self {
        let (block, hash) = value.into_parts();
        Self::new_unchecked(block, hash)
    }
}

#[cfg(any(test, feature = "arbitrary"))]
impl<'a> arbitrary::Arbitrary<'a> for SealedBlock {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let block = BaseBlock::arbitrary(u)?;
        Ok(Self::seal_slow(block))
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl SealedBlock {
    /// Returns a mutable reference to the header.
    #[inline]
    pub const fn header_mut(&mut self) -> &mut Header {
        self.header.header_mut()
    }

    /// Updates the block hash.
    #[inline]
    pub fn set_hash(&mut self, hash: BlockHash) {
        self.header.set_hash(hash)
    }

    /// Returns a mutable reference to the body.
    #[inline]
    pub const fn body_mut(&mut self) -> &mut BaseBlockBody {
        &mut self.body
    }

    /// Updates the parent block hash.
    #[inline]
    pub fn set_parent_hash(&mut self, hash: BlockHash) {
        self.header.set_parent_hash(hash)
    }

    /// Updates the block number.
    #[inline]
    pub fn set_block_number(&mut self, number: alloy_primitives::BlockNumber) {
        self.header.set_block_number(number)
    }

    /// Updates the block timestamp.
    #[inline]
    pub fn set_timestamp(&mut self, timestamp: u64) {
        self.header.set_timestamp(timestamp)
    }

    /// Updates the block state root.
    #[inline]
    pub fn set_state_root(&mut self, state_root: alloy_primitives::B256) {
        self.header.set_state_root(state_root)
    }

    /// Updates the block difficulty.
    #[inline]
    pub fn set_difficulty(&mut self, difficulty: alloy_primitives::U256) {
        self.header.set_difficulty(difficulty)
    }
}

/// A [`SealedBlock`] paired with associated data.
///
/// This is useful for workflows that need to carry block-adjacent metadata alongside a sealed
/// block without defining a new wrapper type for each metadata payload.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(bound(
        serialize = "SealedBlock: serde::Serialize, T: serde::Serialize",
        deserialize = "SealedBlock: serde::Deserialize<'de>, T: serde::Deserialize<'de>"
    ))
)]
pub struct SealedBlockWith<T> {
    /// The sealed block.
    block: SealedBlock,
    /// Associated data for the sealed block.
    data: T,
}

impl<T> SealedBlockWith<T> {
    /// Creates a new sealed block with associated data.
    #[inline]
    pub const fn new(block: SealedBlock, data: T) -> Self {
        Self { block, data }
    }

    /// Returns the sealed block.
    #[inline]
    pub const fn block(&self) -> &SealedBlock {
        &self.block
    }

    /// Returns the associated data.
    #[inline]
    pub const fn data(&self) -> &T {
        &self.data
    }

    /// Consumes the type and returns its components.
    #[doc(alias = "into_parts")]
    #[inline]
    pub fn split(self) -> (SealedBlock, T) {
        (self.block, self.data)
    }
}

impl<T> SealedBlockWith<Option<T>> {
    /// Creates a sealed block without associated data.
    #[inline]
    pub const fn from_block(block: SealedBlock) -> Self {
        Self::new(block, None)
    }
}

impl<T> From<(SealedBlock, T)> for SealedBlockWith<T> {
    #[inline]
    fn from((block, data): (SealedBlock, T)) -> Self {
        Self::new(block, data)
    }
}

impl<T> From<SealedBlock> for SealedBlockWith<Option<T>> {
    #[inline]
    fn from(block: SealedBlock) -> Self {
        Self::from_block(block)
    }
}

impl<T: InMemorySize> InMemorySize for SealedBlockWith<T> {
    #[inline]
    fn size(&self) -> usize {
        self.block.size() + self.data.size()
    }
}

impl<T> Deref for SealedBlockWith<T> {
    type Target = SealedBlock;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.block()
    }
}

#[cfg(test)]
mod tests {
    use alloy_rlp::{Decodable, Encodable};
    use base_common_consensus::{BaseTxEnvelope, Header};

    use super::*;

    fn sample_alloy_block() -> base_common_consensus::Block<BaseTxEnvelope> {
        let header = Header {
            number: 42,
            gas_limit: 30_000_000,
            gas_used: 21_000,
            timestamp: 1_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };

        let tx = base_common_consensus::TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 21_000_000_000,
            gas_limit: 21_000,
            to: alloy_primitives::TxKind::Call(Address::ZERO),
            value: alloy_primitives::U256::from(100),
            input: alloy_primitives::Bytes::default(),
        };

        let tx_signed = BaseTxEnvelope::Legacy(base_common_consensus::Signed::new_unchecked(
            tx,
            alloy_primitives::Signature::test_signature(),
            B256::ZERO,
        ));

        let body = base_common_consensus::BlockBody {
            transactions: vec![tx_signed],
            ommers: vec![],
            withdrawals: Some(Default::default()),
        };

        base_common_consensus::Block::new(header, body)
    }

    #[test]
    fn test_sealed_block_rlp_roundtrip() {
        // Create a sample block using base_common_consensus::Block
        let header = Header {
            number: 42,
            gas_limit: 30_000_000,
            gas_used: 21_000,
            timestamp: 1_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };

        // Create a simple transaction
        let tx = base_common_consensus::TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 21_000_000_000,
            gas_limit: 21_000,
            to: alloy_primitives::TxKind::Call(Address::ZERO),
            value: alloy_primitives::U256::from(100),
            input: alloy_primitives::Bytes::default(),
        };

        let tx_signed = BaseTxEnvelope::Legacy(base_common_consensus::Signed::new_unchecked(
            tx,
            alloy_primitives::Signature::test_signature(),
            B256::ZERO,
        ));

        // Create block body with the transaction
        let body = base_common_consensus::BlockBody {
            transactions: vec![tx_signed],
            ommers: vec![],
            withdrawals: Some(Default::default()),
        };

        // Create the block
        let block = base_common_consensus::Block::new(header, body);

        // Create a sealed block
        let sealed_block = SealedBlock::seal_slow(block);

        // Encode the sealed block
        let mut encoded = Vec::new();
        sealed_block.encode(&mut encoded);

        // Decode the sealed block
        let decoded =
            SealedBlock::decode(&mut encoded.as_slice()).expect("Failed to decode sealed block");

        // Verify the roundtrip
        assert_eq!(sealed_block.hash(), decoded.hash());
        assert_eq!(sealed_block.header().number, decoded.header().number);
        assert_eq!(sealed_block.header().state_root, decoded.header().state_root);
        assert_eq!(sealed_block.body().transactions.len(), decoded.body().transactions.len());
    }

    #[test]
    fn test_alloy_block_sealed_encoding_matches_regular_block() {
        let block = sample_alloy_block();

        let mut block_encoded = Vec::new();
        block.encode(&mut block_encoded);

        let mut borrowed_encoded = Vec::new();
        <base_common_consensus::Block<BaseTxEnvelope> as Block>::rlp_encode(
            &block.header,
            &block.body,
            &mut borrowed_encoded,
        );

        let sealed_block = SealedBlock::seal_slow(block.clone());
        let mut sealed_encoded = Vec::new();
        sealed_block.encode(&mut sealed_encoded);

        assert_eq!(borrowed_encoded, block_encoded);
        assert_eq!(sealed_encoded, block_encoded);
        assert_eq!(sealed_block.length(), block.length());
    }

    #[test]
    fn test_decode_sealed_produces_correct_hash() {
        // Create a sample block using base_common_consensus::Block
        let header = Header {
            number: 42,
            gas_limit: 30_000_000,
            gas_used: 21_000,
            timestamp: 1_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };

        // Create a simple transaction
        let tx = base_common_consensus::TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 21_000_000_000,
            gas_limit: 21_000,
            to: alloy_primitives::TxKind::Call(Address::ZERO),
            value: alloy_primitives::U256::from(100),
            input: alloy_primitives::Bytes::default(),
        };

        let tx_signed = BaseTxEnvelope::Legacy(base_common_consensus::Signed::new_unchecked(
            tx,
            alloy_primitives::Signature::test_signature(),
            B256::ZERO,
        ));

        // Create block body with the transaction
        let body = base_common_consensus::BlockBody {
            transactions: vec![tx_signed],
            ommers: vec![],
            withdrawals: Some(Default::default()),
        };

        // Create the block
        let block = base_common_consensus::Block::new(header, body);
        let expected_hash = block.header.hash_slow();

        // Encode the block
        let mut encoded = Vec::new();
        block.encode(&mut encoded);

        // Decode using decode_sealed - this should compute hash from raw RLP
        let decoded = SealedBlock::decode_sealed(&mut encoded.as_slice())
            .expect("Failed to decode sealed block");

        // Verify the hash matches
        assert_eq!(decoded.hash(), expected_hash);
        assert_eq!(decoded.header().number, 42);
        assert_eq!(decoded.body().transactions.len(), 1);
    }

    #[test]
    fn test_sealed_block_from_sealed() {
        let header = Header::default();
        let body = base_common_consensus::BlockBody::<BaseTxEnvelope>::default();
        let block = base_common_consensus::Block::new(header, body);
        let hash = block.header.hash_slow();

        // Create Sealed<Block>
        let sealed: Sealed<base_common_consensus::Block<BaseTxEnvelope>> =
            Sealed::new_unchecked(block.clone(), hash);

        // Convert to SealedBlock
        let sealed_block: SealedBlock = SealedBlock::from(sealed);

        assert_eq!(sealed_block.hash(), hash);
        assert_eq!(sealed_block.header().number, block.header.number);
    }

    #[test]
    fn test_sealed_block_with_data() {
        let block = base_common_consensus::Block::<BaseTxEnvelope>::default();
        let sealed_block = SealedBlock::seal_slow(block);

        let with_data = SealedBlockWith::new(sealed_block.clone(), Some(42u64));

        assert_eq!(&*with_data, &sealed_block);
        assert_eq!(with_data.hash(), sealed_block.hash());
        assert_eq!(with_data.block(), &sealed_block);
        assert_eq!(with_data.data(), &Some(42));

        let (block, data) = with_data.split();
        assert_eq!(block, sealed_block);
        assert_eq!(data, Some(42));
    }

    #[test]
    fn test_sealed_block_with_from_block() {
        let block = base_common_consensus::Block::<BaseTxEnvelope>::default();
        let sealed_block = SealedBlock::seal_slow(block);

        let with_data = SealedBlockWith::<Option<u64>>::from_block(sealed_block.clone());
        assert_eq!(with_data.block(), &sealed_block);
        assert_eq!(with_data.data(), &None);

        let from_block: SealedBlockWith<Option<u64>> = sealed_block.into();
        assert_eq!(from_block.data(), &None);
    }
}
