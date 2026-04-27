//! Storage traits and types for the audit archiver.
//!
//! Defines vendor-agnostic storage interfaces that the audit application interacts with.
//! Concrete implementations (e.g. S3) live in submodules and implement these traits.

mod s3;
use alloy_primitives::TxHash;
use anyhow::Result;
use async_trait::async_trait;
use base_bundles::{AcceptedBundle, RejectedTransaction};
pub use s3::S3EventReaderWriter;
use serde::{Deserialize, Serialize};

use crate::{
    reader::Event,
    types::{BundleEvent, DropReason},
};

/// Trait for writing bundle events to storage.
#[async_trait]
pub trait EventWriter: Send + Sync {
    /// Archives a bundle event to persistent storage.
    async fn archive_event(&self, event: Event) -> Result<()>;
}

/// Trait for reading bundle event history from storage.
#[async_trait]
pub trait BundleEventReader: Send + Sync {
    /// Gets the bundle history by its key suffix (`bundle_hash` or `bundle_id`).
    async fn get_bundle_history(&self, bundle_key: &str) -> Result<Option<BundleHistory>>;

    /// Gets transaction metadata for a given transaction hash.
    async fn get_transaction_metadata(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<TransactionMetadata>>;
}

/// Trait for persisting and retrieving rejected transactions.
#[async_trait]
pub trait RejectedTxStore: Send + Sync {
    /// Stores a rejected transaction.
    async fn store_rejected_transaction(&self, rejected_tx: &RejectedTransaction) -> Result<()>;

    /// Retrieves a rejected transaction by block number and transaction hash.
    async fn get_rejected_transaction(
        &self,
        block_number: u64,
        tx_hash: TxHash,
    ) -> Result<Option<RejectedTransaction>>;
}

/// Metadata for a transaction, tracking which bundles it belongs to.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TransactionMetadata {
    /// Bundle identifiers that contain this transaction.
    ///
    /// Stored as strings for backwards compatibility -- old S3 objects contain
    /// UUIDs, new objects contain `B256` hex hashes.
    pub bundle_ids: Vec<String>,
}

/// History event for a bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum BundleHistoryEvent {
    /// Bundle was received.
    Received {
        /// Event key.
        key: String,
        /// Event timestamp.
        timestamp: i64,
        /// The accepted bundle.
        bundle: Box<AcceptedBundle>,
    },
    /// Bundle was cancelled.
    Cancelled {
        /// Event key.
        key: String,
        /// Event timestamp.
        timestamp: i64,
    },
    /// Bundle was included by a builder.
    BuilderIncluded {
        /// Event key.
        key: String,
        /// Event timestamp.
        timestamp: i64,
        /// Builder identifier.
        builder: String,
        /// Block number.
        block_number: u64,
        /// Flashblock index.
        flashblock_index: u64,
    },
    /// Bundle was included in a block.
    BlockIncluded {
        /// Event key.
        key: String,
        /// Event timestamp.
        timestamp: i64,
        /// Block number.
        block_number: u64,
        /// Block hash.
        block_hash: TxHash,
    },
    /// Bundle was dropped.
    Dropped {
        /// Event key.
        key: String,
        /// Event timestamp.
        timestamp: i64,
        /// Drop reason.
        reason: DropReason,
    },
}

impl BundleHistoryEvent {
    /// Returns the event key.
    pub fn key(&self) -> &str {
        match self {
            Self::Received { key, .. }
            | Self::Cancelled { key, .. }
            | Self::BuilderIncluded { key, .. }
            | Self::BlockIncluded { key, .. }
            | Self::Dropped { key, .. } => key,
        }
    }
}

/// History of events for a bundle.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BundleHistory {
    /// List of history events.
    pub history: Vec<BundleHistoryEvent>,
}

/// Converts an [`Event`] into a [`BundleHistoryEvent`] for storage.
pub(crate) fn to_history_event(event: &Event) -> BundleHistoryEvent {
    match &event.event {
        BundleEvent::Received { bundle, .. } => BundleHistoryEvent::Received {
            key: event.key.clone(),
            timestamp: event.timestamp,
            bundle: bundle.clone(),
        },
        BundleEvent::Cancelled { .. } => {
            BundleHistoryEvent::Cancelled { key: event.key.clone(), timestamp: event.timestamp }
        }
        BundleEvent::BuilderIncluded { builder, block_number, flashblock_index, .. } => {
            BundleHistoryEvent::BuilderIncluded {
                key: event.key.clone(),
                timestamp: event.timestamp,
                builder: builder.clone(),
                block_number: *block_number,
                flashblock_index: *flashblock_index,
            }
        }
        BundleEvent::BlockIncluded { block_number, block_hash, .. } => {
            BundleHistoryEvent::BlockIncluded {
                key: event.key.clone(),
                timestamp: event.timestamp,
                block_number: *block_number,
                block_hash: *block_hash,
            }
        }
        BundleEvent::Dropped { reason, .. } => BundleHistoryEvent::Dropped {
            key: event.key.clone(),
            timestamp: event.timestamp,
            reason: reason.clone(),
        },
    }
}

/// Applies a bundle key to transaction metadata, returning `None` if already present (no-op).
pub(crate) fn update_transaction_metadata_transform(
    transaction_metadata: TransactionMetadata,
    bundle_key: String,
) -> Option<TransactionMetadata> {
    let mut bundle_ids = transaction_metadata.bundle_ids;

    if bundle_ids.contains(&bundle_key) {
        return None;
    }

    bundle_ids.push(bundle_key);
    Some(TransactionMetadata { bundle_ids })
}

#[cfg(test)]
mod tests {
    use alloy_primitives::TxHash;
    use base_bundles::{BundleExtensions, test_utils::create_bundle_from_txn_data};
    use uuid::Uuid;

    use super::*;
    use crate::{
        reader::Event,
        types::{BundleEvent, DropReason},
    };

    fn create_test_event(key: &str, timestamp: i64, bundle_event: BundleEvent) -> Event {
        Event { key: key.to_string(), timestamp, event: bundle_event }
    }

    #[test]
    fn test_to_history_event_received() {
        let bundle = create_bundle_from_txn_data();
        let bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());
        let bundle_event = BundleEvent::Received { bundle_id, bundle: Box::new(bundle.clone()) };
        let event = create_test_event("test-key", 1234567890, bundle_event);

        let history_event = to_history_event(&event);

        match &history_event {
            BundleHistoryEvent::Received { key, timestamp, bundle: b } => {
                assert_eq!(key, "test-key");
                assert_eq!(*timestamp, 1234567890);
                assert_eq!(b.block_number, bundle.block_number);
            }
            _ => panic!("expected Received event"),
        }
    }

    #[test]
    fn test_to_history_event_all_types() {
        let bundle = create_bundle_from_txn_data();
        let bundle_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, bundle.bundle_hash().as_slice());

        let cases: Vec<(&str, BundleEvent)> = vec![
            ("received", BundleEvent::Received { bundle_id, bundle: Box::new(bundle) }),
            ("cancelled", BundleEvent::Cancelled { bundle_id }),
            (
                "builder-included",
                BundleEvent::BuilderIncluded {
                    bundle_id,
                    builder: "test-builder".to_string(),
                    block_number: 12345,
                    flashblock_index: 1,
                },
            ),
            (
                "block-included",
                BundleEvent::BlockIncluded {
                    bundle_id,
                    block_number: 12345,
                    block_hash: TxHash::from([1u8; 32]),
                },
            ),
            ("dropped", BundleEvent::Dropped { bundle_id, reason: DropReason::TimedOut }),
        ];

        for (name, bundle_event) in cases {
            let event = create_test_event(&format!("key-{name}"), 1234567890, bundle_event);
            let history_event = to_history_event(&event);
            assert_eq!(history_event.key(), format!("key-{name}"), "key mismatch for {name}");
        }
    }

    #[test]
    fn test_update_transaction_metadata_transform_adds_new_bundle() {
        let metadata = TransactionMetadata { bundle_ids: vec![] };
        let bundle = create_bundle_from_txn_data();
        let key = format!("{}", bundle.bundle_hash());

        let result = update_transaction_metadata_transform(metadata, key.clone());

        assert!(result.is_some());
        let metadata = result.unwrap();
        assert_eq!(metadata.bundle_ids.len(), 1);
        assert_eq!(metadata.bundle_ids[0], key);
    }

    #[test]
    fn test_update_transaction_metadata_transform_skips_existing_bundle() {
        let bundle = create_bundle_from_txn_data();
        let key = format!("{}", bundle.bundle_hash());
        let metadata = TransactionMetadata { bundle_ids: vec![key.clone()] };

        let result = update_transaction_metadata_transform(metadata, key);

        assert!(result.is_none());
    }

    #[test]
    fn test_update_transaction_metadata_transform_adds_to_existing_bundles() {
        let existing = "0xaaaa".to_string();
        let new = "0xbbbb".to_string();

        let metadata = TransactionMetadata { bundle_ids: vec![existing.clone()] };

        let result = update_transaction_metadata_transform(metadata, new.clone());

        assert!(result.is_some());
        let metadata = result.unwrap();
        assert_eq!(metadata.bundle_ids.len(), 2);
        assert!(metadata.bundle_ids.contains(&existing));
        assert!(metadata.bundle_ids.contains(&new));
    }
}
