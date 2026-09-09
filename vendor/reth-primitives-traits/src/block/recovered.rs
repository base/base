//! Recovered Block variant.

use alloc::vec::Vec;

use alloy_eips::{BlockNumHash, Encodable2718, eip1898::BlockWithParent};
use alloy_primitives::{
    Address, B64, B256, BlockHash, BlockNumber, Bloom, Bytes, Sealed, TxHash, U256,
};
use base_common_types_chain::{
    BaseBlock, BaseBlockBody, BaseTxEnvelope, BlockHeader, Header,
    transaction::{Recovered, TransactionMeta},
};
use derive_more::Deref;

use crate::{
    Block, BlockBody, InMemorySize, SealedHeader,
    block::{SealedBlock, error::SealedBlockRecoveryError},
    transaction::signed::{RecoveryError, SignedTransaction},
};

/// A block with senders recovered from the block's transactions.
///
/// This type represents a [`SealedBlock`] where all transaction senders have been
/// recovered and verified. Recovery is an expensive operation that extracts the
/// sender address from each transaction's signature.
///
/// # Construction
///
/// - [`RecoveredBlock::new`] / [`RecoveredBlock::new_unhashed`] - Create with pre-recovered senders
///   (unchecked)
/// - [`RecoveredBlock::try_new`] / [`RecoveredBlock::try_new_unhashed`] - Create with validation
/// - [`RecoveredBlock::try_recover`] - Recover from a block
/// - [`RecoveredBlock::try_recover_sealed`] - Recover from a sealed block
///
/// # Performance
///
/// Sender recovery is computationally expensive. Cache recovered blocks when possible
/// to avoid repeated recovery operations.
///
/// ## Sealing
///
/// This type uses lazy sealing to avoid hashing the header until it is needed:
///
/// [`RecoveredBlock::new_unhashed`] creates a recovered block without hashing the header.
/// [`RecoveredBlock::new`] creates a recovered block with the corresponding block hash.
///
/// ## Recovery
///
/// Sender recovery is fallible and can fail if any of the transactions fail to recover the sender.
/// A [`SealedBlock`] can be upgraded to a [`RecoveredBlock`] using the
/// [`RecoveredBlock::try_recover`] or [`SealedBlock::try_recover`] method.
#[derive(Debug, Clone, Deref)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RecoveredBlock {
    /// Block
    #[deref]
    block: SealedBlock,
    /// List of senders that match the transactions in the block
    senders: Vec<Address>,
}

impl RecoveredBlock {
    /// Creates a new recovered block instance with the given senders as provided and the block
    /// hash.
    ///
    /// Note: This expects that the given senders match the transactions in the block.
    #[inline]
    pub fn new(block: BaseBlock, senders: Vec<Address>, hash: BlockHash) -> Self {
        Self { block: SealedBlock::new_unchecked(block, hash), senders }
    }

    /// Creates a new recovered block instance with the given senders as provided.
    ///
    /// Note: This expects that the given senders match the transactions in the block.
    #[inline]
    pub fn new_unhashed(block: BaseBlock, senders: Vec<Address>) -> Self {
        Self { block: SealedBlock::new_unhashed(block), senders }
    }

    /// Returns the recovered senders.
    #[inline]
    pub fn senders(&self) -> &[Address] {
        &self.senders
    }

    /// Returns an iterator over the recovered senders.
    #[inline]
    pub fn senders_iter(&self) -> impl Iterator<Item = &Address> {
        self.senders.iter()
    }

    /// Consumes the type and returns the inner block.
    #[inline]
    pub fn into_block(self) -> BaseBlock {
        self.block.into_block()
    }

    /// Returns a reference to the sealed block.
    #[inline]
    pub const fn sealed_block(&self) -> &SealedBlock {
        &self.block
    }

    /// Creates a new recovered block instance with the given [`SealedBlock`] and senders as
    /// provided
    #[inline]
    pub const fn new_sealed(block: SealedBlock, senders: Vec<Address>) -> Self {
        Self { block, senders }
    }

    /// A safer variant of [`Self::new`] that checks if the number of senders is equal to
    /// the number of transactions in the block and recovers the senders from the transactions, if
    /// not using [`SignedTransaction::recover_signer`](crate::transaction::signed::SignedTransaction)
    /// to recover the senders.
    pub fn try_new(
        block: BaseBlock,
        senders: Vec<Address>,
        hash: BlockHash,
    ) -> Result<Self, SealedBlockRecoveryError> {
        let senders = if block.body().transaction_count() == senders.len() {
            senders
        } else {
            let Ok(senders) = block.body().try_recover_signers() else {
                return Err(SealedBlockRecoveryError::new(SealedBlock::new_unchecked(block, hash)));
            };
            senders
        };
        Ok(Self::new(block, senders, hash))
    }

    /// A safer variant of [`Self::new`] that checks if the number of senders is equal to
    /// the number of transactions in the block and recovers the senders from the transactions, if
    /// not using [`SignedTransaction::recover_signer_unchecked`](crate::transaction::signed::SignedTransaction)
    /// to recover the senders.
    pub fn try_new_unchecked(
        block: BaseBlock,
        senders: Vec<Address>,
        hash: BlockHash,
    ) -> Result<Self, SealedBlockRecoveryError> {
        let senders = if block.body().transaction_count() == senders.len() {
            senders
        } else {
            let Ok(senders) = block.body().try_recover_signers_unchecked() else {
                return Err(SealedBlockRecoveryError::new(SealedBlock::new_unchecked(block, hash)));
            };
            senders
        };
        Ok(Self::new(block, senders, hash))
    }

    /// A safer variant of [`Self::new_unhashed`] that checks if the number of senders is equal to
    /// the number of transactions in the block and recovers the senders from the transactions, if
    /// not using [`SignedTransaction::recover_signer`](crate::transaction::signed::SignedTransaction)
    /// to recover the senders.
    pub fn try_new_unhashed(
        block: BaseBlock,
        senders: Vec<Address>,
    ) -> Result<Self, RecoveryError> {
        let senders = if block.body().transaction_count() == senders.len() {
            senders
        } else {
            block.body().try_recover_signers()?
        };
        Ok(Self::new_unhashed(block, senders))
    }

    /// A safer variant of [`Self::new_unhashed`] that checks if the number of senders is equal to
    /// the number of transactions in the block and recovers the senders from the transactions, if
    /// not using [`SignedTransaction::recover_signer_unchecked`](crate::transaction::signed::SignedTransaction)
    /// to recover the senders.
    pub fn try_new_unhashed_unchecked(
        block: BaseBlock,
        senders: Vec<Address>,
    ) -> Result<Self, RecoveryError> {
        let senders = if block.body().transaction_count() == senders.len() {
            senders
        } else {
            block.body().try_recover_signers_unchecked()?
        };
        Ok(Self::new_unhashed(block, senders))
    }

    /// Recovers the senders from the transactions in the block using
    /// [`SignedTransaction::recover_signer`](crate::transaction::signed::SignedTransaction).
    ///
    /// Returns an error if any of the transactions fail to recover the sender.
    pub fn try_recover(block: BaseBlock) -> Result<Self, RecoveryError> {
        let senders = block.body().try_recover_signers()?;
        Ok(Self::new_unhashed(block, senders))
    }

    /// Recovers the senders from the transactions in the block using
    /// [`SignedTransaction::recover_signer_unchecked`](crate::transaction::signed::SignedTransaction).
    ///
    /// Returns an error if any of the transactions fail to recover the sender.
    pub fn try_recover_unchecked(block: BaseBlock) -> Result<Self, RecoveryError> {
        let senders = block.body().try_recover_signers_unchecked()?;
        Ok(Self::new_unhashed(block, senders))
    }

    /// Recovers the senders from the transactions in the block using
    /// [`SignedTransaction::recover_signer`](crate::transaction::signed::SignedTransaction).
    ///
    /// Returns an error if any of the transactions fail to recover the sender.
    pub fn try_recover_sealed(block: SealedBlock) -> Result<Self, SealedBlockRecoveryError> {
        let Ok(senders) = block.body().try_recover_signers() else {
            return Err(SealedBlockRecoveryError::new(block));
        };
        let (block, hash) = block.split();
        Ok(Self::new(block, senders, hash))
    }

    /// Recovers the senders from the transactions in the sealed block using
    /// [`SignedTransaction::recover_signer_unchecked`](crate::transaction::signed::SignedTransaction).
    ///
    /// Returns an error if any of the transactions fail to recover the sender.
    pub fn try_recover_sealed_unchecked(
        block: SealedBlock,
    ) -> Result<Self, SealedBlockRecoveryError> {
        let Ok(senders) = block.body().try_recover_signers_unchecked() else {
            return Err(SealedBlockRecoveryError::new(block));
        };
        let (block, hash) = block.split();
        Ok(Self::new(block, senders, hash))
    }

    /// A safer variant of [`Self::new_sealed`] that checks if the number of senders is equal to
    /// the number of transactions in the block and recovers the senders from the transactions, if
    /// not using [`SignedTransaction::recover_signer`](crate::transaction::signed::SignedTransaction)
    /// to recover the senders.
    ///
    /// Returns an error if any of the transactions fail to recover the sender.
    pub fn try_recover_sealed_with_senders(
        block: SealedBlock,
        senders: Vec<Address>,
    ) -> Result<Self, SealedBlockRecoveryError> {
        let (block, hash) = block.split();
        Self::try_new(block, senders, hash)
    }

    /// A safer variant of [`Self::new_sealed`] that checks if the number of senders is equal to
    /// the number of transactions in the block and recovers the senders from the transactions, if
    /// not using [`SignedTransaction::recover_signer_unchecked`](crate::transaction::signed::SignedTransaction)
    /// to recover the senders.
    pub fn try_recover_sealed_with_senders_unchecked(
        block: SealedBlock,
        senders: Vec<Address>,
    ) -> Result<Self, SealedBlockRecoveryError> {
        let (block, hash) = block.split();
        Self::try_new_unchecked(block, senders, hash)
    }

    /// Returns the block hash.
    pub fn hash_ref(&self) -> &BlockHash {
        self.block.hash_ref()
    }

    /// Returns a copy of the block hash.
    pub fn hash(&self) -> BlockHash {
        *self.hash_ref()
    }

    /// Return the number hash tuple.
    pub fn num_hash(&self) -> BlockNumHash {
        BlockNumHash::new(self.header().number(), self.hash())
    }

    /// Return a [`BlockWithParent`] for this header.
    pub fn block_with_parent(&self) -> BlockWithParent {
        BlockWithParent { parent: self.header().parent_hash(), block: self.num_hash() }
    }

    /// Clone the header.
    pub fn clone_header(&self) -> Header {
        self.header().clone()
    }

    /// Clones the internal header and returns a [`SealedHeader`] sealed with the hash.
    pub fn clone_sealed_header(&self) -> SealedHeader {
        SealedHeader::new(self.clone_header(), self.hash())
    }

    /// Clones the wrapped block and returns the [`SealedBlock`] sealed with the hash.
    pub fn clone_sealed_block(&self) -> SealedBlock {
        self.block.clone()
    }

    /// Consumes the block and returns the block's header.
    #[inline]
    pub fn into_header(self) -> Header {
        self.block.into_header()
    }

    /// Consumes the block and returns the block's body.
    #[inline]
    pub fn into_body(self) -> BaseBlockBody {
        self.block.into_body()
    }

    /// Consumes the block and returns the [`SealedBlock`] and drops the recovered senders.
    #[inline]
    pub fn into_sealed_block(self) -> SealedBlock {
        self.block
    }

    /// Consumes the type and returns its components.
    #[inline]
    pub fn split_sealed(self) -> (SealedBlock, Vec<Address>) {
        (self.block, self.senders)
    }

    /// Consumes the type and returns its components.
    #[doc(alias = "into_components")]
    #[inline]
    pub fn split(self) -> (BaseBlock, Vec<Address>) {
        (self.block.into_block(), self.senders)
    }

    /// Returns the `Recovered<&T>` transaction at the given index.
    #[inline]
    pub fn recovered_transaction(&self, idx: usize) -> Option<Recovered<&BaseTxEnvelope>> {
        let sender = self.senders.get(idx).copied()?;
        self.block.body().transactions.get(idx).map(|tx| Recovered::new_unchecked(tx, sender))
    }

    /// Finds a transaction by hash and returns it with its index and block context.
    pub fn find_indexed(&self, tx_hash: TxHash) -> Option<IndexedTx<'_>> {
        self.body()
            .transactions_iter()
            .enumerate()
            .find(|(_, tx)| tx.trie_hash() == tx_hash)
            .map(|(index, tx)| IndexedTx { block: self, tx, index })
    }

    /// Returns an iterator over all transactions and their sender.
    #[inline]
    pub fn transactions_with_sender(
        &self,
    ) -> impl Iterator<Item = (&Address, &BaseTxEnvelope)> + '_ {
        self.senders.iter().zip(self.block.body().transactions())
    }

    /// Returns an iterator over cloned `Recovered<Transaction>`
    #[inline]
    pub fn clone_transactions_recovered(
        &self,
    ) -> impl Iterator<Item = Recovered<BaseTxEnvelope>> + '_ {
        self.transactions_with_sender()
            .map(|(sender, tx)| Recovered::new_unchecked(tx.clone(), *sender))
    }

    /// Returns an iterator over `Recovered<&Transaction>`
    #[inline]
    pub fn transactions_recovered(
        &self,
    ) -> impl Iterator<Item = Recovered<&'_ BaseTxEnvelope>> + '_ {
        self.transactions_with_sender().map(|(sender, tx)| Recovered::new_unchecked(tx, *sender))
    }

    /// Consumes the type and returns an iterator over all [`Recovered`] transactions in the block.
    #[inline]
    pub fn into_transactions_recovered(self) -> impl Iterator<Item = Recovered<BaseTxEnvelope>> {
        self.block
            .split()
            .0
            .into_body()
            .into_transactions()
            .into_iter()
            .zip(self.senders)
            .map(|(tx, sender)| tx.with_signer(sender))
    }

    /// Consumes the block and returns the transactions of the block.
    #[inline]
    pub fn into_transactions(self) -> Vec<BaseTxEnvelope> {
        self.block.split().0.into_body().into_transactions()
    }
}

impl BlockHeader for RecoveredBlock {
    #[inline]
    fn parent_hash(&self) -> B256 {
        self.header().parent_hash()
    }

    #[inline]
    fn ommers_hash(&self) -> B256 {
        self.header().ommers_hash()
    }

    #[inline]
    fn beneficiary(&self) -> Address {
        self.header().beneficiary()
    }

    #[inline]
    fn state_root(&self) -> B256 {
        self.header().state_root()
    }

    #[inline]
    fn transactions_root(&self) -> B256 {
        self.header().transactions_root()
    }

    #[inline]
    fn receipts_root(&self) -> B256 {
        self.header().receipts_root()
    }

    #[inline]
    fn withdrawals_root(&self) -> Option<B256> {
        self.header().withdrawals_root()
    }

    #[inline]
    fn logs_bloom(&self) -> Bloom {
        self.header().logs_bloom()
    }

    #[inline]
    fn difficulty(&self) -> U256 {
        self.header().difficulty()
    }

    #[inline]
    fn number(&self) -> BlockNumber {
        self.header().number()
    }

    #[inline]
    fn gas_limit(&self) -> u64 {
        self.header().gas_limit()
    }

    #[inline]
    fn gas_used(&self) -> u64 {
        self.header().gas_used()
    }

    #[inline]
    fn timestamp(&self) -> u64 {
        self.header().timestamp()
    }

    #[inline]
    fn mix_hash(&self) -> Option<B256> {
        self.header().mix_hash()
    }

    #[inline]
    fn nonce(&self) -> Option<B64> {
        self.header().nonce()
    }

    #[inline]
    fn base_fee_per_gas(&self) -> Option<u64> {
        self.header().base_fee_per_gas()
    }

    #[inline]
    fn blob_gas_used(&self) -> Option<u64> {
        self.header().blob_gas_used()
    }

    #[inline]
    fn excess_blob_gas(&self) -> Option<u64> {
        self.header().excess_blob_gas()
    }

    #[inline]
    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.header().parent_beacon_block_root()
    }

    #[inline]
    fn requests_hash(&self) -> Option<B256> {
        self.header().requests_hash()
    }

    #[inline]
    fn block_access_list_hash(&self) -> Option<B256> {
        self.header().block_access_list_hash()
    }

    #[inline]
    fn slot_number(&self) -> Option<u64> {
        self.header().slot_number()
    }

    #[inline]
    fn extra_data(&self) -> &Bytes {
        self.header().extra_data()
    }
}

impl Eq for RecoveredBlock {}

impl PartialEq for RecoveredBlock {
    fn eq(&self, other: &Self) -> bool {
        self.block.eq(&other.block) && self.senders.eq(&other.senders)
    }
}

impl Default for RecoveredBlock {
    #[inline]
    fn default() -> Self {
        Self::new_unhashed(BaseBlock::default(), Default::default())
    }
}

impl InMemorySize for RecoveredBlock {
    #[inline]
    fn size(&self) -> usize {
        self.block.size() + self.senders.capacity() * core::mem::size_of::<Address>()
    }
}

impl From<RecoveredBlock> for Sealed<BaseBlock> {
    #[inline]
    fn from(value: RecoveredBlock) -> Self {
        value.block.into()
    }
}

/// Converts a block with recovered transactions into a [`RecoveredBlock`].
///
/// This implementation takes an `base_common_types_chain::Block` where transactions are of type
/// `Recovered<T>` (transactions with their recovered senders) and converts it into a
/// [`RecoveredBlock`] which stores transactions and senders separately for efficiency.
impl From<base_common_types_chain::Block<Recovered<BaseTxEnvelope>>> for RecoveredBlock {
    fn from(block: base_common_types_chain::Block<Recovered<BaseTxEnvelope>>) -> Self {
        let header = block.header;

        // Split the recovered transactions into transactions and senders
        let (transactions, senders): (Vec<BaseTxEnvelope>, Vec<Address>) = block
            .body
            .transactions
            .into_iter()
            .map(|recovered| {
                let (tx, sender) = recovered.into_parts();
                (tx, sender)
            })
            .unzip();

        // Reconstruct the block with regular transactions
        let body = base_common_types_chain::BlockBody {
            transactions,
            ommers: block.body.ommers,
            withdrawals: block.body.withdrawals,
        };

        let block = base_common_types_chain::Block::new(header, body);

        Self::new_unhashed(block, senders)
    }
}

#[cfg(any(test, feature = "arbitrary"))]
impl<'a> arbitrary::Arbitrary<'a> for RecoveredBlock {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let block = BaseBlock::arbitrary(u)?;
        Ok(Self::try_recover(block).unwrap())
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl RecoveredBlock {
    /// Returns a mutable reference to the recovered senders.
    #[inline]
    pub const fn senders_mut(&mut self) -> &mut Vec<Address> {
        &mut self.senders
    }

    /// Appends the sender to the list of senders.
    #[inline]
    pub fn push_sender(&mut self, sender: Address) {
        self.senders.push(sender);
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl core::ops::DerefMut for RecoveredBlock {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.block
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl RecoveredBlock {
    /// Updates the block header.
    #[inline]
    pub fn set_header(&mut self, header: Header) {
        *self.header_mut() = header
    }

    /// Updates the block hash.
    #[inline]
    pub fn set_hash(&mut self, hash: BlockHash) {
        self.block.set_hash(hash)
    }

    /// Returns a mutable reference to the header.
    #[inline]
    pub const fn header_mut(&mut self) -> &mut Header {
        self.block.header_mut()
    }

    /// Returns a mutable reference to the body.
    #[inline]
    pub const fn block_mut(&mut self) -> &mut BaseBlockBody {
        self.block.body_mut()
    }

    /// Updates the parent block hash.
    #[inline]
    pub fn set_parent_hash(&mut self, hash: BlockHash) {
        self.block.set_parent_hash(hash);
    }

    /// Updates the block number.
    #[inline]
    pub fn set_block_number(&mut self, number: alloy_primitives::BlockNumber) {
        self.block.set_block_number(number);
    }

    /// Updates the block timestamp.
    #[inline]
    pub fn set_timestamp(&mut self, timestamp: u64) {
        self.block.set_timestamp(timestamp);
    }

    /// Updates the block state root.
    #[inline]
    pub fn set_state_root(&mut self, state_root: alloy_primitives::B256) {
        self.block.set_state_root(state_root);
    }

    /// Updates the block difficulty.
    #[inline]
    pub fn set_difficulty(&mut self, difficulty: alloy_primitives::U256) {
        self.block.set_difficulty(difficulty);
    }
}

/// Transaction with its index and block reference for efficient metadata access.
#[derive(Debug)]
pub struct IndexedTx<'a> {
    /// Recovered block containing the transaction
    block: &'a RecoveredBlock,
    /// Transaction matching the hash
    tx: &'a BaseTxEnvelope,
    /// Index of the transaction in the block
    index: usize,
}

impl<'a> IndexedTx<'a> {
    /// Returns the transaction.
    #[inline]
    pub const fn tx(&self) -> &BaseTxEnvelope {
        self.tx
    }

    /// Returns the recovered transaction with the sender.
    #[inline]
    pub fn recovered_tx(&self) -> Recovered<&BaseTxEnvelope> {
        let sender = self.block.senders[self.index];
        Recovered::new_unchecked(self.tx, sender)
    }

    /// Returns the transaction hash.
    #[inline]
    pub fn tx_hash(&self) -> TxHash {
        self.tx.trie_hash()
    }

    /// Returns the block hash.
    pub fn block_hash(&self) -> B256 {
        self.block.hash()
    }

    /// Returns the index of the transaction in the block.
    #[inline]
    pub const fn index(&self) -> usize {
        self.index
    }

    /// Builds a [`TransactionMeta`] for the indexed transaction.
    pub fn meta(&self) -> TransactionMeta {
        TransactionMeta {
            tx_hash: self.tx.trie_hash(),
            index: self.index as u64,
            block_hash: self.block.hash(),
            block_number: self.block.number(),
            base_fee: self.block.base_fee_per_gas(),
            timestamp: self.block.timestamp(),
            excess_blob_gas: self.block.excess_blob_gas(),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Signature, TxKind, bytes};
    use base_common_types_chain::{BaseTxEnvelope, Header, TxLegacy};

    use super::*;

    #[test]
    fn test_from_block_with_recovered_transactions() {
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 21_000_000_000,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: bytes!(),
        };

        let signature = Signature::new(U256::from(1), U256::from(2), false);
        let sender = Address::from([0x01; 20]);

        let signed_tx = BaseTxEnvelope::Legacy(base_common_types_chain::Signed::new_unchecked(
            tx,
            signature,
            B256::ZERO,
        ));

        let recovered_tx = Recovered::new_unchecked(signed_tx, sender);

        let header = Header::default();
        let body = base_common_types_chain::BlockBody {
            transactions: vec![recovered_tx],
            ommers: vec![],
            withdrawals: None,
        };
        let block_with_recovered = base_common_types_chain::Block::new(header, body);

        let recovered_block: RecoveredBlock = block_with_recovered.into();

        assert_eq!(recovered_block.senders().len(), 1);
        assert_eq!(recovered_block.senders()[0], sender);
        assert_eq!(recovered_block.body().transactions().count(), 1);
    }
}
