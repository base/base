//! Contains the [`Metadata`] type used in Flashblocks.

use std::collections::HashMap;

use alloy_primitives::{Address, B256, U256};
use serde::{Deserialize, Serialize};

/// Metadata associated with a flashblock.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Default)]
pub struct Metadata {
    /// Block number this flashblock belongs to.
    pub block_number: u64,
    /// New account balances from this flashblock.
    #[serde(default)]
    pub new_account_balances: HashMap<Address, U256>,
    /// Receipts indexed by transaction hash.
    #[serde(default)]
    pub receipts: HashMap<B256, serde_json::Value>,
}
