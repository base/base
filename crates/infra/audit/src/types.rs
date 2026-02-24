use alloy_consensus::transaction::{SignerRecoverable, Transaction as ConsensusTransaction};
use alloy_primitives::{Address, TxHash, U256};
use base_bundles::AcceptedBundle;
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

    /// Generates a unique event key for this event.
    pub fn generate_event_key(&self) -> String {
        match self {
            Self::BlockIncluded { bundle_id, block_hash, .. } => {
                format!("{bundle_id}-{block_hash}")
            }
            _ => {
                format!(
                    "{}-{}",
                    self.bundle_id(),
                    Uuid::new_v5(&Uuid::NAMESPACE_OID, self.bundle_id().as_bytes())
                )
            }
        }
    }
}
