use crate::op::transaction::request::TransactionRequest;
use alloy::rpc::types::eth::BlockOverrides;
use serde::{Deserialize, Serialize};

/// Bundle of transactions
#[derive(Debug, Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Bundle {
    /// All transactions to execute
    pub transactions: Vec<TransactionRequest>,
    /// Block overrides to apply
    pub block_override: Option<BlockOverrides>,
}
