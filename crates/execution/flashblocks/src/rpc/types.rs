//! Subscription types for the `eth_` `PubSub` RPC extension

use alloy_rpc_types_eth::{Log, pubsub::SubscriptionKind};
use base_alloy_rpc_types::Transaction;
use derive_more::From;
use serde::{Deserialize, Serialize};

/// A full transaction object with its associated logs.
///
/// This is returned by `newFlashblockTransactions` subscription when `full = true`,
/// providing both the transaction details and logs emitted by its execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionWithLogs {
    /// The full transaction object.
    #[serde(flatten)]
    pub transaction: Transaction,
    /// Logs emitted by this transaction.
    pub logs: Vec<Log>,
}

/// Extended subscription kind that includes both standard Ethereum subscription types
/// and flashblocks-specific types.
///
/// This enum encapsulates the standard [`SubscriptionKind`] from alloy and adds flashblocks
/// support, allowing `eth_subscribe` to handle both standard subscriptions (newHeads, logs, etc.)
/// and custom flashblocks subscriptions.
///
/// By encapsulating [`SubscriptionKind`] rather than redefining its variants, we automatically
/// inherit support for any new variants added upstream, or get a compile error if the signature
/// changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, From)]
#[serde(untagged)]
pub enum ExtendedSubscriptionKind {
    /// Standard Ethereum subscription types (newHeads, logs, newPendingTransactions, syncing).
    ///
    /// These are proxied to reth's underlying `EthPubSub` implementation.
    #[from]
    Standard(SubscriptionKind),
    /// Base-specific subscription types for flashblocks.
    #[from]
    Base(BaseSubscriptionKind),
}

/// Base-specific subscription types for flashblocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BaseSubscriptionKind {
    /// New flashblocks subscription.
    ///
    /// Fires a notification each time a new flashblock is processed, providing the current
    /// pending block state. Each flashblock represents an incremental update to the pending
    /// block, so multiple notifications may be emitted for the same block height as new
    /// flashblocks arrive.
    NewFlashblocks,
    /// Pending logs subscription.
    ///
    /// Returns logs from flashblocks pending state that match the given filter criteria.
    /// Unlike standard `logs` subscription which only includes logs from confirmed blocks,
    /// this includes logs from the current pending flashblock state.
    PendingLogs,
    /// New flashblock transactions subscription.
    ///
    /// Returns transactions from flashblocks as they are sequenced, providing higher inclusion
    /// confidence than standard `newPendingTransactions` which returns mempool transactions.
    /// Flashblock transactions have been included by the sequencer and are effectively preconfirmed.
    ///
    /// Accepts an optional boolean parameter:
    /// - `true`: Returns full transaction objects with their associated logs (as
    ///   [`TransactionWithLogs`])
    /// - `false` (default): Returns only transaction hashes
    NewFlashblockTransactions,
}

impl ExtendedSubscriptionKind {
    /// Returns the standard subscription kind if this is a standard subscription type.
    pub const fn as_standard(&self) -> Option<SubscriptionKind> {
        match self {
            Self::Standard(kind) => Some(*kind),
            Self::Base(_) => None,
        }
    }

    /// Returns true if this is a flashblocks-specific subscription.
    pub const fn is_flashblocks(&self) -> bool {
        matches!(self, Self::Base(_))
    }
}
