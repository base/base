//! Block RPC types.

#![allow(unknown_lints, non_local_definitions)]

use crate::op::transaction::Transaction;
use alloy::rpc::types::eth::{
    BlockTransactions, Header, Rich, Withdrawal
};
use alloy_primitives::{B256, U256};
use serde::{Deserialize, Serialize};

/// Block representation
#[derive(Default, Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Block{
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
    pub transactions: BlockTransactions<Transaction>,
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

/// A Block representation that allows to include additional fields
pub type RichBlock = Rich<Block>;

impl From<Block> for RichBlock {
    fn from(block: Block) -> Self {
        Rich { inner: block, extra_info: Default::default() }
    }
}
