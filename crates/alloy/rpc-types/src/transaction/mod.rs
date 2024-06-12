//! Optimism specific types related to transactions.

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

/// OP Transaction type
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    /// Ethereum Transaction Types
    #[serde(flatten)]
    pub inner: alloy_rpc_types_eth::Transaction,
    /// The ETH value to mint on L2
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mint: Option<u128>,
    /// Hash that uniquely identifies the source of the deposit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_hash: Option<B256>,
    /// Field indicating whether the transaction is a system transaction, and therefore
    /// exempt from the L2 gas limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_system_tx: Option<bool>,
    /// Deposit receipt version for deposit transactions post-canyon
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deposit_receipt_version: Option<u64>,
}
