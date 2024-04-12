//! Optimism types for pub-sub

use crate::op::transaction::Transaction;
use alloy::rpc::types::eth::{pubsub::PubSubSyncStatus, Log, RichHeader};
use alloy_primitives::B256;
use serde::{Deserialize, Serialize, Serializer};

/// Subscription result.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum SubscriptionResult {
    /// New block header.
    Header(Box<RichHeader>),
    /// Log
    Log(Box<Log>),
    /// Transaction hash
    TransactionHash(B256),
    /// Full Transaction
    FullTransaction(Box<Transaction>),
    /// SyncStatus
    SyncState(PubSubSyncStatus),
}

impl Serialize for SubscriptionResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match *self {
            SubscriptionResult::Header(ref header) => header.serialize(serializer),
            SubscriptionResult::Log(ref log) => log.serialize(serializer),
            SubscriptionResult::TransactionHash(ref hash) => hash.serialize(serializer),
            SubscriptionResult::FullTransaction(ref tx) => tx.serialize(serializer),
            SubscriptionResult::SyncState(ref sync) => sync.serialize(serializer),
        }
    }
}