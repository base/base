use alloy_consensus::transaction::{SignerRecoverable, Transaction as ConsensusTransaction};
use alloy_primitives::{Address, B256, TxHash, U256};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tips_core::AcceptedBundle;
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

/// Hash of a user operation.
pub type UserOpHash = B256;

/// Reason a user operation was dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UserOpDropReason {
    /// User operation was invalid.
    Invalid(String),
    /// User operation expired.
    Expired,
    /// Replaced by a higher fee user operation.
    ReplacedByHigherFee,
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

/// User operation lifecycle event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum UserOpEvent {
    /// User operation was added to the mempool.
    AddedToMempool {
        /// Hash of the user operation.
        user_op_hash: UserOpHash,
        /// Sender address.
        sender: Address,
        /// Entry point address.
        entry_point: Address,
        /// Nonce.
        nonce: U256,
    },
    /// User operation was dropped.
    Dropped {
        /// Hash of the user operation.
        user_op_hash: UserOpHash,
        /// Reason for dropping.
        reason: UserOpDropReason,
    },
    /// User operation was included in a block.
    Included {
        /// Hash of the user operation.
        user_op_hash: UserOpHash,
        /// Block number.
        block_number: u64,
        /// Transaction hash.
        tx_hash: TxHash,
    },
}

impl UserOpEvent {
    /// Returns the user operation hash for this event.
    pub const fn user_op_hash(&self) -> UserOpHash {
        match self {
            Self::AddedToMempool { user_op_hash, .. }
            | Self::Dropped { user_op_hash, .. }
            | Self::Included { user_op_hash, .. } => *user_op_hash,
        }
    }

    /// Generates a unique event key for this event.
    pub fn generate_event_key(&self) -> String {
        match self {
            Self::Included { user_op_hash, tx_hash, .. } => {
                format!("{user_op_hash}-{tx_hash}")
            }
            _ => {
                format!(
                    "{}-{}",
                    self.user_op_hash(),
                    Uuid::new_v5(&Uuid::NAMESPACE_OID, self.user_op_hash().as_slice())
                )
            }
        }
    }
}

#[cfg(test)]
mod user_op_event_tests {
    use alloy_primitives::{address, b256};

    use super::*;

    fn create_test_user_op_hash() -> UserOpHash {
        b256!("1111111111111111111111111111111111111111111111111111111111111111")
    }

    #[test]
    fn test_user_op_event_added_to_mempool_serialization() {
        let event = UserOpEvent::AddedToMempool {
            user_op_hash: create_test_user_op_hash(),
            sender: address!("2222222222222222222222222222222222222222"),
            entry_point: address!("0000000071727De22E5E9d8BAf0edAc6f37da032"),
            nonce: U256::from(1),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"AddedToMempool\""));

        let deserialized: UserOpEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.user_op_hash(), deserialized.user_op_hash());
    }

    #[test]
    fn test_user_op_event_dropped_serialization() {
        let event = UserOpEvent::Dropped {
            user_op_hash: create_test_user_op_hash(),
            reason: UserOpDropReason::Invalid("gas too low".to_string()),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"Dropped\""));
        assert!(json.contains("gas too low"));

        let deserialized: UserOpEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.user_op_hash(), deserialized.user_op_hash());
    }

    #[test]
    fn test_user_op_event_included_serialization() {
        let event = UserOpEvent::Included {
            user_op_hash: create_test_user_op_hash(),
            block_number: 12345,
            tx_hash: b256!("3333333333333333333333333333333333333333333333333333333333333333"),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"Included\""));
        assert!(json.contains("\"block_number\":12345"));

        let deserialized: UserOpEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.user_op_hash(), deserialized.user_op_hash());
    }

    #[test]
    fn test_user_op_hash_accessor() {
        let hash = create_test_user_op_hash();

        let added = UserOpEvent::AddedToMempool {
            user_op_hash: hash,
            sender: address!("2222222222222222222222222222222222222222"),
            entry_point: address!("0000000071727De22E5E9d8BAf0edAc6f37da032"),
            nonce: U256::from(1),
        };
        assert_eq!(added.user_op_hash(), hash);

        let dropped =
            UserOpEvent::Dropped { user_op_hash: hash, reason: UserOpDropReason::Expired };
        assert_eq!(dropped.user_op_hash(), hash);

        let included = UserOpEvent::Included {
            user_op_hash: hash,
            block_number: 100,
            tx_hash: b256!("4444444444444444444444444444444444444444444444444444444444444444"),
        };
        assert_eq!(included.user_op_hash(), hash);
    }

    #[test]
    fn test_generate_event_key_included() {
        let user_op_hash =
            b256!("1111111111111111111111111111111111111111111111111111111111111111");
        let tx_hash = b256!("2222222222222222222222222222222222222222222222222222222222222222");

        let event = UserOpEvent::Included { user_op_hash, block_number: 100, tx_hash };

        let key = event.generate_event_key();
        assert!(key.contains(&format!("{user_op_hash}")));
        assert!(key.contains(&format!("{tx_hash}")));
    }

    #[test]
    fn test_user_op_drop_reason_variants() {
        let invalid = UserOpDropReason::Invalid("test reason".to_string());
        let json = serde_json::to_string(&invalid).unwrap();
        assert!(json.contains("Invalid"));
        assert!(json.contains("test reason"));

        let expired = UserOpDropReason::Expired;
        let json = serde_json::to_string(&expired).unwrap();
        assert!(json.contains("Expired"));

        let replaced = UserOpDropReason::ReplacedByHigherFee;
        let json = serde_json::to_string(&replaced).unwrap();
        assert!(json.contains("ReplacedByHigherFee"));
    }
}
