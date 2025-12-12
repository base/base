//! Types for the transaction status rpc

use alloy_rpc_types_eth::pubsub::SubscriptionKind;
use serde::{Deserialize, Serialize};

/// The status of a transaction.
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub enum Status {
    /// Transaction is not known to the node.
    Unknown,
    /// Transaction is known to the node (in mempool or confirmed).
    Known,
}

/// Response containing the status of a transaction.
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct TransactionStatusResponse {
    /// The status of the queried transaction.
    pub status: Status,
}

/// Extended subscription kind that includes both standard Ethereum subscription types
/// and flashblocks-specific types.
///
/// This enum wraps the standard `SubscriptionKind` from alloy and adds flashblocks support,
/// allowing `eth_subscribe` to handle both standard subscriptions (newHeads, logs, etc.)
/// and custom flashblocks subscriptions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExtendedSubscriptionKind {
    /// New block headers subscription (standard).
    NewHeads,
    /// Logs subscription (standard).
    Logs,
    /// New pending transactions subscription (standard).
    NewPendingTransactions,
    /// Node syncing status subscription (standard).
    Syncing,
    /// New flashblocks subscription (Base-specific).
    ///
    /// Fires a notification each time a new flashblock is processed, providing the current
    /// pending block state. Each flashblock represents an incremental update to the pending
    /// block, so multiple notifications may be emitted for the same block height as new
    /// flashblocks arrive.
    NewFlashblocks,
    /// Pending logs subscription (Base-specific).
    ///
    /// Returns logs from flashblocks pending state that match the given filter criteria.
    /// Unlike standard `logs` subscription which only includes logs from confirmed blocks,
    /// this includes logs from the current pending flashblock state.
    PendingLogs,
}

impl ExtendedSubscriptionKind {
    /// Returns the standard subscription kind if this is a standard subscription type.
    pub const fn as_standard(&self) -> Option<SubscriptionKind> {
        match self {
            Self::NewHeads => Some(SubscriptionKind::NewHeads),
            Self::Logs => Some(SubscriptionKind::Logs),
            Self::NewPendingTransactions => Some(SubscriptionKind::NewPendingTransactions),
            Self::Syncing => Some(SubscriptionKind::Syncing),
            Self::NewFlashblocks | Self::PendingLogs => None,
        }
    }

    /// Returns true if this is a flashblocks-specific subscription.
    pub const fn is_flashblocks(&self) -> bool {
        matches!(self, Self::NewFlashblocks | Self::PendingLogs)
    }
}
