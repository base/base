//! Block RPC types.

#![allow(unknown_lints, non_local_definitions)]

use crate::op::transaction::Transaction;
use alloy::rpc::types::eth::{BlockTransactionHashes, BlockTransactionHashesMut, Header, Rich, Withdrawal};
use alloy_primitives::{
B256, U256,
};
use serde::{
    Deserialize, Serialize,
};

/// Block representation
#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Block {
    /// Header of the block.
    #[serde(flatten)]
    pub header: Header,
    /// Uncles' hashes.
    #[serde(default)]
    pub uncles: Vec<B256>,
    /// Block Transactions. In the case of an uncle block, this field is not included in RPC
    /// responses, and when deserialized, it will be set to [BlockTransactions::Uncle].
    #[serde(
        default = "BlockTransactions::uncle",
        skip_serializing_if = "BlockTransactions::is_uncle"
    )]
    pub transactions: BlockTransactions,
    /// Integer the size of this block in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<U256>,
    /// Withdrawals in the block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdrawals: Option<Vec<Withdrawal>>,
}

impl Block {
    /// Converts a block with Tx hashes into a full block.
    pub fn into_full_block(self, txs: Vec<Transaction>) -> Self {
        Self { transactions: BlockTransactions::Full(txs), ..self }
    }
}

/// Block Transactions depending on the boolean attribute of `eth_getBlockBy*`,
/// or if used by `eth_getUncle*`
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BlockTransactions<T = Transaction> {
    /// Only hashes
    Hashes(Vec<B256>),
    /// Full transactions
    Full(Vec<T>),
    /// Special case for uncle response.
    Uncle,
}

impl Default for BlockTransactions {
    fn default() -> Self {
        BlockTransactions::Hashes(Vec::default())
    }
}

impl BlockTransactions {
    /// Converts `self` into `Hashes`.
    #[inline]
    pub fn convert_to_hashes(&mut self) {
        if !self.is_hashes() {
            *self = Self::Hashes(self.hashes().copied().collect());
        }
    }

    /// Converts `self` into `Hashes`.
    #[inline]
    pub fn into_hashes(mut self) -> Self {
        self.convert_to_hashes();
        self
    }

    /// Check if the enum variant is used for hashes.
    #[inline]
    pub const fn is_hashes(&self) -> bool {
        matches!(self, Self::Hashes(_))
    }

    /// Returns true if the enum variant is used for full transactions.
    #[inline]
    pub const fn is_full(&self) -> bool {
        matches!(self, Self::Full(_))
    }

    /// Returns true if the enum variant is used for an uncle response.
    #[inline]
    pub const fn is_uncle(&self) -> bool {
        matches!(self, Self::Uncle)
    }

    /// Returns an iterator over the transaction hashes.
    #[deprecated = "use `hashes` instead"]
    #[inline]
    pub fn iter(&self) -> BlockTransactionHashes<'_> {
        self.hashes()
    }

    /// Returns an iterator over references to the transaction hashes.
    #[inline]
    pub fn hashes(&self) -> BlockTransactionHashes<'_> {
        BlockTransactionHashes::new(self) // TODO: The `new` method is not public, so cannot be used here. Make a PR to alloy to make this public
    }

    /// Returns an iterator over mutable references to the transaction hashes.
    #[inline]
    pub fn hashes_mut(&mut self) -> BlockTransactionHashesMut<'_> {
        BlockTransactionHashesMut::new(self) // TODO: The `new` method is not public, so cannot be used here. Make a PR to alloy to make this public
    }

    /// Returns an instance of BlockTransactions with the Uncle special case.
    #[inline]
    pub const fn uncle() -> Self {
        Self::Uncle
    }

    /// Returns the number of transactions.
    #[inline]
    pub fn len(&self) -> usize {
        self.hashes().len()
    }

    /// Whether the block has no transactions.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}


/// A Block representation that allows to include additional fields
pub type RichBlock = Rich<Block>;

impl From<Block> for RichBlock {
    fn from(block: Block) -> Self {
        Rich { inner: block, extra_info: Default::default() }
    }
}