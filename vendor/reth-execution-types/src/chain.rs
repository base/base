//! Contains [Chain], a chain of blocks and their final state.

use alloc::{borrow::Cow, collections::BTreeMap, sync::Arc, vec::Vec};
use core::{fmt, ops::RangeInclusive};

use alloy_eips::{BlockNumHash, eip1898::ForkBlock};
use alloy_primitives::{Address, BlockHash, BlockNumber, Log, TxHash, map::HashSet};
use base_common_types_chain::{
    BaseReceipt, BaseTxEnvelope, BlockHeader, TxReceipt, transaction::Recovered,
};
use reth_primitives_traits::{
    BlockBody, IndexedTx, RecoveredBlock, SealedHeader, transaction::signed::SignedTransaction,
};
use base_execution_state_types::LazyTrieData;

use crate::ExecutionOutcome;

/// A chain of blocks and their final state.
///
/// The chain contains the state of accounts after execution of its blocks,
/// changesets for those blocks (and their transactions), as well as the blocks themselves.
///
/// Used inside the `BlockchainTree`.
///
/// # Warning
///
/// A chain of blocks should not be empty.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Chain {
    /// All blocks in this chain.
    blocks: BTreeMap<BlockNumber, Arc<RecoveredBlock>>,
    /// The outcome of block execution for this chain.
    ///
    /// This field contains the state of all accounts after the execution of all blocks in this
    /// chain, ranging from the [`Chain::first`] block to the [`Chain::tip`] block, inclusive.
    ///
    /// Additionally, it includes the individual state changes that led to the current state.
    execution_outcome: ExecutionOutcome,
    /// Lazy trie data for each block in the chain, keyed by block number.
    ///
    /// Contains handles to lazily-initialized sorted trie updates and hashed state.
    trie_data: BTreeMap<BlockNumber, LazyTrieData>,
}

type ChainTxReceiptMeta<'a> =
    (&'a Arc<RecoveredBlock>, IndexedTx<'a>, &'a BaseReceipt, &'a [BaseReceipt]);

impl Default for Chain {
    fn default() -> Self {
        Self {
            blocks: Default::default(),
            execution_outcome: Default::default(),
            trie_data: Default::default(),
        }
    }
}

impl Chain {
    /// Create new Chain from blocks and state.
    ///
    /// # Warning
    ///
    /// A chain of blocks should not be empty.
    pub fn new(
        blocks: impl IntoIterator<Item: Into<Arc<RecoveredBlock>>>,
        execution_outcome: ExecutionOutcome,
        trie_data: BTreeMap<BlockNumber, LazyTrieData>,
    ) -> Self {
        let blocks = blocks
            .into_iter()
            .map(|b| {
                let block = b.into();
                (block.header().number(), block)
            })
            .collect::<BTreeMap<_, _>>();
        debug_assert!(!blocks.is_empty(), "Chain should have at least one block");

        Self { blocks, execution_outcome, trie_data }
    }

    /// Create new Chain from a single block and its state.
    pub fn from_block(
        block: impl Into<Arc<RecoveredBlock>>,
        execution_outcome: ExecutionOutcome,
        trie_data: LazyTrieData,
    ) -> Self {
        let block = block.into();
        let block_number = block.header().number();
        Self::new([block], execution_outcome, BTreeMap::from([(block_number, trie_data)]))
    }

    /// Get the blocks in this chain.
    pub const fn blocks(&self) -> &BTreeMap<BlockNumber, Arc<RecoveredBlock>> {
        &self.blocks
    }

    /// Consumes the type and only returns the blocks in this chain.
    pub fn into_blocks(self) -> BTreeMap<BlockNumber, Arc<RecoveredBlock>> {
        self.blocks
    }

    /// Returns an iterator over all headers in the block with increasing block numbers.
    pub fn headers(&self) -> impl Iterator<Item = SealedHeader> + '_ {
        self.blocks.values().map(|block| block.clone_sealed_header())
    }

    /// Get all trie data for this chain.
    pub const fn trie_data(&self) -> &BTreeMap<BlockNumber, LazyTrieData> {
        &self.trie_data
    }

    /// Get trie data for a specific block number.
    pub fn trie_data_at(&self, block_number: BlockNumber) -> Option<&LazyTrieData> {
        self.trie_data.get(&block_number)
    }

    /// Remove all trie data for this chain.
    pub fn clear_trie_data(&mut self) {
        self.trie_data.clear();
    }

    /// Get execution outcome of this chain
    pub const fn execution_outcome(&self) -> &ExecutionOutcome {
        &self.execution_outcome
    }

    /// Get mutable execution outcome of this chain
    pub const fn execution_outcome_mut(&mut self) -> &mut ExecutionOutcome {
        &mut self.execution_outcome
    }

    /// Return true if chain is empty and has no blocks.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Return block number of the block hash.
    pub fn block_number(&self, block_hash: BlockHash) -> Option<BlockNumber> {
        self.blocks.iter().find_map(|(num, block)| (block.hash() == block_hash).then_some(*num))
    }

    /// Returns the block with matching hash.
    pub fn recovered_block(&self, block_hash: BlockHash) -> Option<&RecoveredBlock> {
        self.blocks
            .iter()
            .find_map(|(_num, block)| (block.hash() == block_hash).then_some(block.as_ref()))
    }

    /// Return execution outcome at the `block_number` or None if block is not known
    pub fn execution_outcome_at_block(
        &self,
        block_number: BlockNumber,
    ) -> Option<ExecutionOutcome> {
        if self.tip().number() == block_number {
            return Some(self.execution_outcome.clone());
        }

        if self.blocks.contains_key(&block_number) {
            let mut execution_outcome = self.execution_outcome.clone();
            execution_outcome.revert_to(block_number);
            return Some(execution_outcome);
        }
        None
    }

    /// Destructure the chain into its inner components:
    /// 1. The blocks contained in the chain.
    /// 2. The execution outcome representing the final state.
    /// 3. The trie data map.
    #[expect(clippy::type_complexity)]
    pub fn into_inner(
        self,
    ) -> (ChainBlocks<'static>, ExecutionOutcome, BTreeMap<BlockNumber, LazyTrieData>) {
        (ChainBlocks { blocks: Cow::Owned(self.blocks) }, self.execution_outcome, self.trie_data)
    }

    /// Destructure the chain into its inner components:
    /// 1. A reference to the blocks contained in the chain.
    /// 2. A reference to the execution outcome representing the final state.
    pub const fn inner(&self) -> (ChainBlocks<'_>, &ExecutionOutcome) {
        (ChainBlocks { blocks: Cow::Borrowed(&self.blocks) }, &self.execution_outcome)
    }

    /// Returns an iterator over all the receipts of the blocks in the chain.
    pub fn block_receipts_iter(&self) -> impl Iterator<Item = &Vec<BaseReceipt>> + '_ {
        self.execution_outcome.receipts().iter()
    }

    /// Returns an iterator over all receipts in the chain.
    pub fn receipts_iter(&self) -> impl Iterator<Item = &BaseReceipt> + '_ {
        self.block_receipts_iter().flatten()
    }

    /// Returns an iterator over all logs in the chain.
    pub fn logs_iter(&self) -> impl Iterator<Item = &Log> + '_ {
        self.receipts_iter().flat_map(|receipt| receipt.logs())
    }

    /// Returns an iterator over all blocks in the chain with increasing block number.
    pub fn blocks_iter(&self) -> impl Iterator<Item = &Arc<RecoveredBlock>> + '_ {
        self.blocks().values()
    }

    /// Returns an iterator over all transactions in the chain.
    pub fn transactions_iter(&self) -> impl Iterator<Item = &BaseTxEnvelope> + '_ {
        self.blocks_iter().flat_map(|block| block.body().transactions())
    }

    /// Returns an iterator over all transaction hashes in the chain.
    pub fn transaction_hashes(&self) -> impl Iterator<Item = &TxHash> + '_ {
        self.transactions_iter().map(|tx| tx.hash())
    }

    /// Returns an iterator over all [`Recovered`] transaction references in the chain.
    pub fn transactions_recovered_iter(
        &self,
    ) -> impl Iterator<Item = Recovered<&BaseTxEnvelope>> + '_ {
        self.blocks_iter().flat_map(|block| block.transactions_recovered())
    }

    /// Returns an iterator over all blocks and their receipts in the chain.
    pub fn blocks_and_receipts(
        &self,
    ) -> impl Iterator<Item = (&Arc<RecoveredBlock>, &Vec<BaseReceipt>)> + '_ {
        self.blocks_iter().zip(self.block_receipts_iter())
    }

    /// Finds a transaction by hash and returns it along with its corresponding receipt data.
    ///
    /// Returns `None` if the transaction is not found in this chain.
    pub fn find_transaction_and_receipt_by_hash(
        &self,
        tx_hash: TxHash,
    ) -> Option<ChainTxReceiptMeta<'_>> {
        for (block, receipts) in self.blocks_and_receipts() {
            let Some(indexed_tx) = block.find_indexed(tx_hash) else {
                continue;
            };
            let receipt = receipts.get(indexed_tx.index())?;
            return Some((block, indexed_tx, receipt, receipts.as_slice()));
        }

        None
    }

    /// Get the block at which this chain forked.
    pub fn fork_block(&self) -> ForkBlock {
        let first = self.first();
        ForkBlock {
            number: first.header().number().saturating_sub(1),
            hash: first.header().parent_hash(),
        }
    }

    /// Get the first block in this chain.
    ///
    /// # Panics
    ///
    /// If chain doesn't have any blocks.
    #[track_caller]
    pub fn first(&self) -> &RecoveredBlock {
        self.blocks.first_key_value().expect("Chain should have at least one block").1
    }

    /// Get the tip of the chain.
    ///
    /// # Panics
    ///
    /// If chain doesn't have any blocks.
    #[track_caller]
    pub fn tip(&self) -> &RecoveredBlock {
        self.blocks.last_key_value().expect("Chain should have at least one block").1
    }

    /// Returns length of the chain.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Returns the range of block numbers in the chain.
    ///
    /// # Panics
    ///
    /// If chain doesn't have any blocks.
    pub fn range(&self) -> RangeInclusive<BlockNumber> {
        self.first().header().number()..=self.tip().header().number()
    }

    /// Get all receipts for the given block.
    pub fn receipts_by_block_hash(&self, block_hash: BlockHash) -> Option<Vec<&BaseReceipt>> {
        let num = self.block_number(block_hash)?;
        Some(self.execution_outcome.receipts_by_block(num).iter().collect())
    }

    /// Get all receipts with attachment.
    ///
    /// Attachment includes block number, block hash, transaction hash and transaction index.
    pub fn receipts_with_attachment(&self) -> Vec<BlockReceipts<BaseReceipt>> {
        let mut receipt_attach = Vec::with_capacity(self.blocks().len());

        self.blocks_and_receipts().for_each(|(block, receipts)| {
            let block_num_hash = BlockNumHash::new(block.number(), block.hash());

            let tx_receipts = block
                .body()
                .transactions
                .iter()
                .zip(receipts)
                .map(|(tx, receipt)| (*tx.hash(), receipt.clone()))
                .collect();

            receipt_attach.push(BlockReceipts {
                block: block_num_hash,
                tx_receipts,
                timestamp: block.timestamp(),
            });
        });

        receipt_attach
    }

    /// Append a single block with state to the chain.
    /// This method assumes that blocks attachment to the chain has already been validated.
    pub fn append_block(
        &mut self,
        block: impl Into<Arc<RecoveredBlock>>,
        execution_outcome: ExecutionOutcome,
        trie_data: LazyTrieData,
    ) {
        let block = block.into();
        let block_number = block.header().number();
        self.blocks.insert(block_number, block);
        self.execution_outcome.extend(execution_outcome);
        self.trie_data.insert(block_number, trie_data);
    }

    /// Merge two chains by appending the given chain into the current one.
    ///
    /// The state of accounts for this chain is set to the state of the newest chain.
    ///
    /// Returns the passed `other` chain in [`Result::Err`] variant if the chains could not be
    /// connected.
    pub fn append_chain(&mut self, other: Self) -> Result<(), Self> {
        let chain_tip = self.tip();
        let other_fork_block = other.fork_block();
        if chain_tip.hash() != other_fork_block.hash {
            return Err(other);
        }

        // Insert blocks from other chain
        self.blocks.extend(other.blocks);
        self.execution_outcome.extend(other.execution_outcome);
        self.trie_data.extend(other.trie_data);

        Ok(())
    }
}

/// Wrapper type for `blocks` display in `Chain`
#[derive(Debug)]
pub struct DisplayBlocksChain<'a>(pub &'a BTreeMap<BlockNumber, Arc<RecoveredBlock>>);

impl fmt::Display for DisplayBlocksChain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_list();
        let mut values = self.0.values().map(|block| block.num_hash());
        if values.len() <= 3 {
            list.entries(values);
        } else {
            list.entry(&values.next().unwrap());
            list.entry(&format_args!("..."));
            list.entry(&values.next_back().unwrap());
        }
        list.finish()
    }
}

/// All blocks in the chain
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChainBlocks<'a> {
    blocks: Cow<'a, BTreeMap<BlockNumber, Arc<RecoveredBlock>>>,
}

impl ChainBlocks<'_> {
    /// Creates a consuming iterator over all blocks in the chain with increasing block number.
    ///
    /// Note: this always yields at least one block.
    #[inline]
    pub fn into_blocks(self) -> impl Iterator<Item = Arc<RecoveredBlock>> {
        self.blocks.into_owned().into_values()
    }

    /// Creates an iterator over all blocks in the chain with increasing block number.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = (&BlockNumber, &RecoveredBlock)> {
        self.blocks.iter().map(|(number, block)| (number, block.as_ref()))
    }

    /// Get the tip of the chain.
    ///
    /// # Note
    ///
    /// Chains always have at least one block.
    #[inline]
    pub fn tip(&self) -> &RecoveredBlock {
        self.blocks.last_key_value().expect("Chain should have at least one block").1.as_ref()
    }

    /// Get the _first_ block of the chain.
    ///
    /// # Note
    ///
    /// Chains always have at least one block.
    #[inline]
    pub fn first(&self) -> &RecoveredBlock {
        self.blocks.first_key_value().expect("Chain should have at least one block").1.as_ref()
    }

    /// Returns an iterator over all transactions in the chain.
    #[inline]
    pub fn transactions(
        &self,
    ) -> impl Iterator<Item = &<base_common_types_chain::BaseBlockBody as BlockBody>::Transaction> + '_
    {
        self.blocks.values().flat_map(|block| block.body().transactions_iter())
    }

    /// Returns an iterator over all transactions and their senders.
    #[inline]
    pub fn transactions_with_sender(
        &self,
    ) -> impl Iterator<
        Item = (&Address, &<base_common_types_chain::BaseBlockBody as BlockBody>::Transaction),
    > + '_ {
        self.blocks.values().flat_map(|block| block.transactions_with_sender())
    }

    /// Returns an iterator over all [`Recovered`] in the blocks
    ///
    /// Note: This clones the transactions since it is assumed this is part of a shared [Chain].
    #[inline]
    pub fn transactions_ecrecovered(
        &self,
    ) -> impl Iterator<
        Item = Recovered<<base_common_types_chain::BaseBlockBody as BlockBody>::Transaction>,
    > + '_ {
        self.transactions_with_sender().map(|(signer, tx)| tx.clone().with_signer(*signer))
    }

    /// Returns an iterator over all transaction hashes in the block
    #[inline]
    pub fn transaction_hashes(&self) -> impl Iterator<Item = TxHash> + '_ {
        self.blocks
            .values()
            .flat_map(|block| block.body().transactions_iter().map(|tx| tx.tx_hash()))
    }

    /// Returns all transaction hashes in a pre-allocated vector.
    #[inline]
    pub fn transaction_hashes_vec(&self) -> Vec<TxHash> {
        let capacity = self.blocks.values().map(|block| block.body().transactions.len()).sum();

        let mut hashes = Vec::with_capacity(capacity);
        hashes.extend(self.transaction_hashes());
        hashes
    }

    /// Returns all transaction hashes in a pre-allocated set.
    #[inline]
    pub fn transaction_hashes_set(&self) -> HashSet<TxHash> {
        let capacity = self.blocks.values().map(|block| block.body().transactions.len()).sum();

        let mut hashes = HashSet::with_capacity_and_hasher(capacity, Default::default());
        hashes.extend(self.transaction_hashes());
        hashes
    }
}

impl IntoIterator for ChainBlocks<'_> {
    type Item = (BlockNumber, Arc<RecoveredBlock>);
    type IntoIter = alloc::collections::btree_map::IntoIter<BlockNumber, Arc<RecoveredBlock>>;

    fn into_iter(self) -> Self::IntoIter {
        self.blocks.into_owned().into_iter()
    }
}

/// Used to hold receipts and their attachment.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct BlockReceipts<T = base_common_types_chain::EthereumReceipt> {
    /// Block identifier
    pub block: BlockNumHash,
    /// Transaction identifier and receipt.
    pub tx_receipts: Vec<(TxHash, T)>,
    /// Block timestamp
    pub timestamp: u64,
}

/// Bincode-compatible [`Chain`] serde implementation.
#[cfg(feature = "serde-bincode-compat")]
pub(super) mod serde_bincode_compat {
    use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

    use alloy_primitives::{Address, BlockNumber, Bytes};
    use alloy_rlp::Decodable;
    use base_common_types_chain::BaseBlock;
    use reth_primitives_traits::SealedBlock;
    use base_execution_state_types::ComputedTrieData;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_with::{DeserializeAs, SerializeAs};

    use crate::serde_bincode_compat;

    /// Bincode-compatible [`super::Chain`] serde implementation.
    ///
    /// Intended to use with the [`serde_with::serde_as`] macro in the following way:
    /// ```rust
    /// use reth_execution_types::{serde_bincode_compat, Chain};
    /// use serde::{Deserialize, Serialize};
    /// use serde_with::serde_as;
    ///
    /// #[serde_as]
    /// #[derive(Serialize, Deserialize)]
    /// struct Data {
    ///     #[serde_as(as = "serde_bincode_compat::Chain")]
    ///     chain: Chain,
    /// }
    /// ```
    #[derive(Debug, Serialize, Deserialize)]
    #[serde(bound = "")]
    pub struct Chain<'a> {
        blocks: BTreeMap<BlockNumber, RecoveredBlockRepr>,
        execution_outcome: serde_bincode_compat::ExecutionOutcome<'a>,
        #[serde(default)]
        trie_updates: BTreeMap<
            BlockNumber,
            base_execution_state_types::serde_bincode_compat::updates::TrieUpdatesSorted<'a>,
        >,
        #[serde(default)]
        hashed_state: BTreeMap<
            BlockNumber,
            base_execution_state_types::serde_bincode_compat::hashed_state::HashedPostStateSorted<'a>,
        >,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct RecoveredBlockRepr {
        rlp: Bytes,
        senders: Vec<Address>,
    }

    impl<'a> From<&'a super::Chain> for Chain<'a> {
        fn from(value: &'a super::Chain) -> Self {
            Self {
                blocks: value
                    .blocks
                    .iter()
                    .map(|(num, recovered)| {
                        let senders = recovered.senders().to_vec();
                        let rlp = Bytes::from(alloy_rlp::encode(recovered.sealed_block()));
                        (*num, RecoveredBlockRepr { rlp, senders })
                    })
                    .collect(),
                execution_outcome: (&value.execution_outcome).into(),
                trie_updates: value
                    .trie_data
                    .iter()
                    .map(|(k, v)| (*k, v.get().sorted.trie_updates.as_ref().into()))
                    .collect(),
                hashed_state: value
                    .trie_data
                    .iter()
                    .map(|(k, v)| (*k, v.get().sorted.hashed_state.as_ref().into()))
                    .collect(),
            }
        }
    }

    impl<'a> From<Chain<'a>> for super::Chain {
        fn from(value: Chain<'a>) -> Self {
            use reth_primitives_traits::RecoveredBlock;
            use base_execution_state_types::LazyTrieData;

            let hashed_state_map: BTreeMap<_, _> =
                value.hashed_state.into_iter().map(|(k, v)| (k, Arc::new(v.into()))).collect();

            let trie_data: BTreeMap<BlockNumber, LazyTrieData> = value
                .trie_updates
                .into_iter()
                .map(|(k, v)| {
                    let hashed_state = hashed_state_map.get(&k).cloned().unwrap_or_default();
                    (
                        k,
                        LazyTrieData::ready(ComputedTrieData::new(
                            hashed_state,
                            Arc::new(v.into()),
                        )),
                    )
                })
                .collect();

            let blocks = value
                .blocks
                .into_iter()
                .map(|(num, repr)| {
                    let block = BaseBlock::decode(&mut repr.rlp.as_ref())
                        .expect("invalid RLP for block in serde_bincode_compat");
                    let sealed = SealedBlock::new_unhashed(block);
                    (num, Arc::new(RecoveredBlock::new_sealed(sealed, repr.senders)))
                })
                .collect();

            Self { blocks, execution_outcome: value.execution_outcome.into(), trie_data }
        }
    }

    impl SerializeAs<super::Chain> for Chain<'_> {
        fn serialize_as<S>(source: &super::Chain, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            Chain::from(source).serialize(serializer)
        }
    }

    impl<'de> DeserializeAs<'de, super::Chain> for Chain<'de> {
        fn deserialize_as<D>(deserializer: D) -> Result<super::Chain, D::Error>
        where
            D: Deserializer<'de>,
        {
            Chain::deserialize(deserializer).map(Into::into)
        }
    }

    #[cfg(test)]
    mod tests {
        use alloc::collections::BTreeMap;

        use alloy_primitives::Address;
        use arbitrary::Arbitrary;
        use base_common_types_chain::BaseBlock;
        use rand::Rng;
        use reth_primitives_traits::RecoveredBlock;
        use serde::{Deserialize, Serialize};
        use serde_with::serde_as;

        use super::super::{Chain, serde_bincode_compat};

        #[test]
        fn test_chain_bincode_roundtrip() {
            #[serde_as]
            #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
            struct Data {
                #[serde_as(as = "serde_bincode_compat::Chain")]
                chain: Chain,
            }

            let mut bytes = [0u8; 1024];
            rand::rng().fill(bytes.as_mut_slice());
            let block = BaseBlock::arbitrary(&mut arbitrary::Unstructured::new(&bytes)).unwrap();
            // Serialization preserves supplied senders; arbitrary Base transactions need not
            // carry recoverable signatures (for example, account-abstraction transactions).
            let senders = vec![Address::ZERO; block.body.transactions.len()];
            let data = Data {
                chain: Chain::new(
                    vec![RecoveredBlock::new_unhashed(block, senders)],
                    Default::default(),
                    BTreeMap::new(),
                ),
            };

            let encoded = bincode_1_3_3::serialize(&data).unwrap();
            let decoded: Data = bincode_1_3_3::deserialize(&encoded).unwrap();
            assert_eq!(decoded, data);
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, map::HashMap};
    use base_common_types_chain::BaseReceipt;
    use base_execution_state_memory::BundleState;
    use base_execution_state_memory::AccountInfo;

    use super::*;

    #[test]
    fn chain_append() {
        let block: RecoveredBlock = Default::default();
        let block1_hash = B256::new([0x01; 32]);
        let block2_hash = B256::new([0x02; 32]);
        let block3_hash = B256::new([0x03; 32]);
        let block4_hash = B256::new([0x04; 32]);

        let mut block1 = block.clone();
        let mut block2 = block.clone();
        let mut block3 = block.clone();
        let mut block4 = block;

        block1.set_hash(block1_hash);
        block2.set_hash(block2_hash);
        block3.set_hash(block3_hash);
        block4.set_hash(block4_hash);

        block3.set_parent_hash(block2_hash);

        let mut chain1: Chain = Chain {
            blocks: BTreeMap::from([(1, Arc::new(block1)), (2, Arc::new(block2))]),
            ..Default::default()
        };

        let chain2 = Chain {
            blocks: BTreeMap::from([(3, Arc::new(block3)), (4, Arc::new(block4))]),
            ..Default::default()
        };

        assert!(chain1.append_chain(chain2.clone()).is_ok());

        // chain1 got changed so this will fail
        assert!(chain1.append_chain(chain2).is_err());
    }

    #[test]
    fn test_number_split() {
        let execution_outcome1: ExecutionOutcome = ExecutionOutcome::new(
            BundleState::new(
                vec![(
                    Address::new([2; 20]),
                    None,
                    Some(AccountInfo::default()),
                    HashMap::default(),
                )],
                vec![vec![(Address::new([2; 20]), None, vec![])]],
                vec![],
            ),
            vec![vec![]],
            1,
            vec![],
        );

        let execution_outcome2 = ExecutionOutcome::new(
            BundleState::new(
                vec![(
                    Address::new([3; 20]),
                    None,
                    Some(AccountInfo::default()),
                    HashMap::default(),
                )],
                vec![vec![(Address::new([3; 20]), None, vec![])]],
                vec![],
            ),
            vec![vec![]],
            2,
            vec![],
        );

        let mut block1: RecoveredBlock = Default::default();
        let block1_hash = B256::new([15; 32]);
        block1.set_block_number(1);
        block1.set_hash(block1_hash);
        block1.push_sender(Address::new([4; 20]));

        let mut block2: RecoveredBlock = Default::default();
        let block2_hash = B256::new([16; 32]);
        block2.set_block_number(2);
        block2.set_hash(block2_hash);
        block2.push_sender(Address::new([4; 20]));

        let mut block_state_extended = execution_outcome1;
        block_state_extended.extend(execution_outcome2);

        let chain: Chain =
            Chain::new(vec![block1.clone(), block2.clone()], block_state_extended, BTreeMap::new());

        // return tip state
        assert_eq!(
            chain.execution_outcome_at_block(block2.number),
            Some(chain.execution_outcome.clone())
        );
        // state at unknown block
        assert_eq!(chain.execution_outcome_at_block(100), None);
    }

    #[test]
    fn receipts_by_block_hash() {
        // Create a default RecoveredBlock object
        let block: RecoveredBlock = Default::default();

        // Define block hashes for block1 and block2
        let block1_hash = B256::new([0x01; 32]);
        let block2_hash = B256::new([0x02; 32]);

        // Clone the default block into block1 and block2
        let mut block1 = block.clone();
        let mut block2 = block;

        // Set the hashes of block1 and block2
        block1.set_hash(block1_hash);
        block2.set_hash(block2_hash);

        // Create a random receipt object, receipt1
        let receipt1 = BaseReceipt::Legacy(base_common_types_chain::Receipt {
            cumulative_gas_used: 46913,
            logs: vec![],
            status: true.into(),
        });

        // Create another random receipt object, receipt2
        let receipt2 = BaseReceipt::Legacy(base_common_types_chain::Receipt {
            cumulative_gas_used: 1325345,
            logs: vec![],
            status: true.into(),
        });

        // Create a Receipts object with a vector of receipt vectors
        let receipts = vec![vec![receipt1.clone()], vec![receipt2]];

        // Create an ExecutionOutcome object with the created bundle, receipts, an empty requests
        // vector, and first_block set to 10
        let execution_outcome = ExecutionOutcome {
            bundle: Default::default(),
            receipts,
            requests: vec![],
            first_block: 10,
        };

        // Create a Chain object with a BTreeMap of blocks mapped to their block numbers,
        // including block1_hash and block2_hash, and the execution_outcome
        let chain: Chain = Chain {
            blocks: BTreeMap::from([(10, Arc::new(block1)), (11, Arc::new(block2))]),
            execution_outcome: execution_outcome.clone(),
            ..Default::default()
        };

        // Assert that the proper receipt vector is returned for block1_hash
        assert_eq!(chain.receipts_by_block_hash(block1_hash), Some(vec![&receipt1]));

        // Create an ExecutionOutcome object with a single receipt vector containing receipt1
        let execution_outcome1 = ExecutionOutcome {
            bundle: Default::default(),
            receipts: vec![vec![receipt1]],
            requests: vec![],
            first_block: 10,
        };

        // Assert that the execution outcome at the first block contains only the first receipt
        assert_eq!(chain.execution_outcome_at_block(10), Some(execution_outcome1));

        // Assert that the execution outcome at the tip block contains the whole execution outcome
        assert_eq!(chain.execution_outcome_at_block(11), Some(execution_outcome));
    }
}
