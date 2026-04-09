use alloy_consensus::transaction::{SignerRecoverable, Transaction as ConsensusTransaction};
use alloy_primitives::{Address, TxHash, U256};
use base_bundles::{AcceptedBundle, BundleExtensions};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for a transaction.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TransactionId {
    /// The sender address.
    pub sender: Address,
    /// The transaction nonce.
    pub nonce: U256,
    /// The transaction hash.
    pub hash: TxHash,
}

/// Unique identifier for a bundle.
pub type BundleId = Uuid;

/// Reason a bundle was dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DropReason {
    /// Bundle timed out.
    TimedOut,
    /// Bundle transaction reverted.
    Reverted,
}

/// A transaction with its data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    /// Transaction identifier.
    pub id: TransactionId,
    /// Raw transaction data.
    pub data: Bytes,
}

/// Bundle lifecycle event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum BundleEvent {
    /// Bundle was received.
    Received {
        /// Bundle identifier.
        bundle_id: BundleId,
        /// The accepted bundle.
        bundle: Box<AcceptedBundle>,
    },
    /// Bundle was cancelled.
    Cancelled {
        /// Bundle identifier.
        bundle_id: BundleId,
    },
    /// Bundle was included by a builder.
    BuilderIncluded {
        /// Bundle identifier.
        bundle_id: BundleId,
        /// Builder identifier.
        builder: String,
        /// Block number.
        block_number: u64,
        /// Flashblock index.
        flashblock_index: u64,
    },
    /// Bundle was included in a block.
    BlockIncluded {
        /// Bundle identifier.
        bundle_id: BundleId,
        /// Block number.
        block_number: u64,
        /// Block hash.
        block_hash: TxHash,
    },
    /// Bundle was dropped.
    Dropped {
        /// Bundle identifier.
        bundle_id: BundleId,
        /// Drop reason.
        reason: DropReason,
    },
}

impl BundleEvent {
    /// Returns the bundle ID for this event.
    pub const fn bundle_id(&self) -> BundleId {
        match self {
            Self::Received { bundle_id, .. }
            | Self::Cancelled { bundle_id, .. }
            | Self::BuilderIncluded { bundle_id, .. }
            | Self::BlockIncluded { bundle_id, .. }
            | Self::Dropped { bundle_id, .. } => *bundle_id,
        }
    }

    /// Returns transaction IDs from this event (only for Received events).
    pub fn transaction_ids(&self) -> Vec<TransactionId> {
        match self {
            Self::Received { bundle, .. } => bundle
                .txs
                .iter()
                .filter_map(|envelope| {
                    envelope.recover_signer().ok().map(|sender| TransactionId {
                        sender,
                        nonce: U256::from(envelope.nonce()),
                        hash: *envelope.hash(),
                    })
                })
                .collect(),
            Self::Cancelled { .. }
            | Self::BuilderIncluded { .. }
            | Self::BlockIncluded { .. }
            | Self::Dropped { .. } => vec![],
        }
    }

    /// Returns the `bundle_hash` for events that carry bundle data.
    pub fn bundle_hash(&self) -> Option<alloy_primitives::B256> {
        match self {
            Self::Received { bundle, .. } => Some(bundle.bundle_hash()),
            _ => None,
        }
    }

    /// Generates the event key used as both the Kafka message key and S3 object name.
    ///
    /// For `Received` events, derived from `bundle_hash` so that the same
    /// bundle on different ingress pods produces the same key.
    pub fn generate_event_key(&self) -> String {
        match self {
            Self::Received { bundle, .. } => {
                let hash = bundle.bundle_hash();
                format!("received-{hash}")
            }
            Self::BlockIncluded { bundle_id, block_hash, .. } => {
                format!("block-included-{bundle_id}-{block_hash}")
            }
            _ => {
                let id = self.bundle_id();
                let event_type = match self {
                    Self::Cancelled { .. } => "cancelled",
                    Self::BuilderIncluded { .. } => "builder-included",
                    Self::Dropped { .. } => "dropped",
                    _ => unreachable!(),
                };
                format!("{event_type}-{id}")
            }
        }
    }

    /// Returns the full S3 key for this event: `bundles/{prefix}/{event_key}`.
    pub fn s3_event_key(&self) -> String {
        let prefix = match self {
            Self::Received { bundle, .. } => format!("{}", bundle.bundle_hash()),
            _ => format!("{}", self.bundle_id()),
        };
        format!("bundles/{prefix}/{}", self.generate_event_key())
    }
}
