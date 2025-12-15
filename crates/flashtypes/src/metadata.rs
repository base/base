//! Contains the [`Metadata`] type used in Flashblocks.

use alloy_primitives::{Address, B256, U256, map::foldhash::HashMap};
use reth_optimism_primitives::OpReceipt;
use serde::{Deserialize, Serialize};

/// Metadata associated with a flashblock.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Default)]
pub struct Metadata {
    /// Transaction receipts indexed by hash.
    pub receipts: HashMap<B256, OpReceipt>,
    /// Updated account balances.
    pub new_account_balances: HashMap<Address, U256>,
    /// Block number this flashblock belongs to.
    pub block_number: u64,
}
